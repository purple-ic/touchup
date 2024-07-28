use std::fmt::{Debug, Formatter};
use std::fs;
use std::io::{ErrorKind, Read};
use std::ops::{ControlFlow, Deref, DerefMut};
use std::ops::ControlFlow::{Break, Continue};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Sender, SyncSender, TrySendError};
use std::sync::Mutex;

use egui::{Context, Id, NumExt};
use ffmpeg::{codec, decoder, encoder, format, Frame, Packet, picture, Rational};
use ffmpeg::codec::context::Context as CodecContext;
use ffmpeg::encoder::Encoder;
use ffmpeg::ffi::AV_TIME_BASE;
use ffmpeg::format::context::{Input, Output};
use ffmpeg::format::output;
use ffmpeg::frame::{Audio as AudioFrame, Video as VideoFrame};
use ffmpeg::sys::AV_TIME_BASE_Q;

use crate::{AuthArc, TaskCommand, TaskStage, TaskStatus};
use crate::player::r#impl::sec2ts;
#[cfg(feature = "async")]
use crate::spawn_async;
use crate::util::{precise_seek, RationalExt, rescale};

#[derive(Default)]
pub enum ExportFollowUp {
    #[default]
    Nothing,
    #[cfg(feature = "youtube")]
    Youtube {
        info_recv: tokio::sync::oneshot::Receiver<crate::youtube::YtInfo>,
    },
}

impl Debug for ExportFollowUp {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ExportFollowUp::Nothing => "Nothing",
            #[cfg(feature = "youtube")]
            ExportFollowUp::Youtube { .. } => "Youtube { .. }",
        })
    }
}

impl ExportFollowUp {
    #[cfg(feature = "async")]
    pub fn needs_async_stopper(&self) -> bool {
        !matches!(self, Self::Nothing)
    }

    fn run(
        self,
        ctx: &Context,
        use_trash: bool,
        status: SyncSender<TaskStatus>,
        output_file: PathBuf,
        auth: AuthArc,
        task_cmds: Sender<TaskCommand>,
        task_id: u32,
        after_follow_up: impl FnOnce(&Context) + Send + 'static,
        #[cfg(feature = "async")]
        cancel_recv: Option<tokio::sync::oneshot::Receiver<()>>,
    ) {
        match self {
            ExportFollowUp::Nothing => after_follow_up(ctx),
            #[cfg(feature = "youtube")]
            ExportFollowUp::Youtube { info_recv } => {
                use crate::youtube;
                status.send(TaskStatus {
                    stage: TaskStage::YtAwaitingInfo,
                    progress: f32::NEG_INFINITY,
                }).unwrap();
                let should_delete = ctx
                    .data_mut(|d| d.get_persisted(Id::new("deleteAfterUpload")))
                    .unwrap_or(true);
                let ctx = Context::clone(&ctx);

                spawn_async(async move {
                    let mut file = tokio::fs::File::open(&output_file).await.unwrap();
                    let auth = auth.read();
                    let yt = match &auth.youtube {
                        None => {
                            task_cmds.send(TaskCommand::Cancel { id: task_id }).unwrap();
                            return;
                        }
                        Some(v) => v,
                    };
                    let (title, description, visibility) = match info_recv.await.unwrap() {
                        youtube::YtInfo::Continue {
                            title,
                            description,
                            visibility,
                        } => (title, description, visibility),
                        youtube::YtInfo::Cancel => {
                            task_cmds.send(TaskCommand::Cancel { id: task_id }).unwrap();
                            return;
                        }
                    };
                    status.send(TaskStatus {
                        stage: TaskStage::YtUpload,
                        progress: f32::INFINITY,
                    }).unwrap();

                    let upload_video = youtube::yt_upload(
                        crate::https_client(), &mut file, yt, &title, &description, visibility,
                    );
                    tokio::select! {
                        v = upload_video => {
                            status.send(TaskStatus {
                                stage: TaskStage::YtUpload,
                                progress: 1.,
                            }).unwrap();
                            println!("finished uploading");

                            if should_delete {
                                maybe_trash(use_trash, &output_file)
                            }
                            after_follow_up(&ctx);
                        }
                        _ = cancel_recv.unwrap() => {

                        }
                    }
                });
            }
        }
    }
}

// gts = global timestamp (AV_TIME_BASE timestamp)
fn sec2gts(sec: f32) -> i64 {
    (sec * AV_TIME_BASE as f32) as i64
}

fn gts2sec(gts: i64) -> f32 {
    gts as f32 / AV_TIME_BASE as f32
}

// lts = local timestamp (local to some stream)
fn gts2lts(gts: i64, local_base: Rational) -> i64 {
    rescale(gts, AV_TIME_BASE_Q.into(), local_base)
}

pub fn audio_transcoder(
    input: &mut Input,
    output: &mut Output,
    in_stream_idx: usize,
    (start, end): (f32, f32),
) -> Transcoder<AudioFrame, impl FnMut(&mut AudioFrame)> {
    let global_header = output
        .format()
        .flags()
        .contains(format::Flags::GLOBAL_HEADER);
    let in_stream = input.stream(in_stream_idx).unwrap();
    let time_base = in_stream.time_base();
    let ts_rate = time_base.invert().value_f64();

    let mut decoder = CodecContext::from_parameters(in_stream.parameters())
        .unwrap_or_else(|err| todo!("could not get decoder: {err}"))
        .decoder()
        .audio()
        .unwrap_or_else(|e| todo!("{e}"));
    let codec = encoder::find(decoder.codec().unwrap().id()).unwrap();
    let mut out_stream = output.add_stream(codec).unwrap();
    out_stream.set_parameters(in_stream.parameters());
    out_stream.set_time_base(time_base);
    out_stream.set_avg_frame_rate(in_stream.avg_frame_rate());

    let start_ts = sec2ts(ts_rate, start as f64);
    let end_ts = sec2ts(ts_rate, end as f64);

    unsafe { (&mut *out_stream.as_mut_ptr()).duration = end_ts - start_ts }
    let mut encoder = CodecContext::new_with_codec(codec)
        .encoder()
        .audio()
        .unwrap();
    encoder.set_parameters(out_stream.parameters()).unwrap();
    encoder.set_time_base(time_base);
    encoder.set_rate(decoder.rate() as i32);

    if global_header {
        encoder.set_flags(codec::Flags::GLOBAL_HEADER);
    }

    let encoder = encoder.open_as(codec).unwrap();
    out_stream.set_parameters(&encoder);

    Transcoder {
        start: start_ts,
        end: end_ts,
        decoder: decoder.0,
        encoder: encoder.0.0,
        in_stream_idx,
        out_stream_idx: out_stream.index(),
        frame: AudioFrame::empty(),
        apply_frame_extra: |_| {},
    }
}

pub fn video_transcoder(
    input: &mut Input,
    output: &mut Output,
    in_stream_idx: usize,
    (start, end): (f32, f32),
) -> Transcoder<VideoFrame, impl FnMut(&mut VideoFrame)> {
    let global_header = output
        .format()
        .flags()
        .contains(format::Flags::GLOBAL_HEADER);
    let in_stream = input.stream(in_stream_idx).unwrap();
    let time_base = in_stream.time_base();
    let ts_rate = time_base.invert().value_f64();

    let mut decoder = CodecContext::from_parameters(in_stream.parameters())
        .unwrap_or_else(|err| todo!("could not get decoder: {err}"))
        .decoder()
        .video()
        .unwrap_or_else(|e| todo!("{e}"));

    let codec = encoder::find(decoder.codec().unwrap().id()).unwrap();
    let mut out_stream = output.add_stream(codec).unwrap();
    out_stream.set_parameters(in_stream.parameters());
    out_stream.set_time_base(time_base);
    out_stream.set_avg_frame_rate(in_stream.avg_frame_rate());

    let start_ts = sec2ts(ts_rate, start as f64);
    let end_ts = sec2ts(ts_rate, end as f64);

    unsafe { (&mut *out_stream.as_mut_ptr()).duration = end_ts - start_ts }
    let mut encoder = CodecContext::new_with_codec(codec)
        .encoder()
        .video()
        .unwrap();
    encoder.set_parameters(out_stream.parameters()).unwrap();
    encoder.set_time_base(time_base);
    if global_header {
        encoder.set_flags(codec::Flags::GLOBAL_HEADER);
    }
    let mut encoder = encoder.open_as(codec).unwrap();
    out_stream.set_parameters(&encoder);

    let mut frame = VideoFrame::empty();

    precise_seek(input, &mut decoder, &mut frame, in_stream_idx, start_ts);

    Transcoder {
        start: start_ts,
        end: end_ts,
        decoder: decoder.0,
        encoder: encoder.0.0,
        in_stream_idx,
        out_stream_idx: out_stream.index(),
        frame,
        apply_frame_extra: |frame| {
            // super important! make sure we don't confuse the encoder by keeping the old frame types
            //      the encoder should by itself choose what frame type to use per frame
            frame.set_kind(picture::Type::None);
        },
    }
}

struct Transcoder<Fr: Deref<Target=Frame> + DerefMut, Fn: FnMut(&mut Fr)> {
    start: i64,
    end: i64,
    decoder: decoder::opened::Opened,
    encoder: Encoder,
    in_stream_idx: usize,
    out_stream_idx: usize,
    frame: Fr,
    apply_frame_extra: Fn,
}

impl<Fr: Deref<Target=Frame> + DerefMut, Fn: FnMut(&mut Fr)> Transcoder<Fr, Fn> {
    fn send_packets(&mut self, packet: &mut Packet, output: &mut Output) {
        while self.encoder.receive_packet(packet).is_ok() {
            packet.set_stream(self.out_stream_idx);
            packet.write_interleaved(output).unwrap()
        }
    }

    fn process_frames(&mut self, packet: &mut Packet, output: &mut Output) -> ControlFlow<()> {
        while self.decoder.receive_frame(&mut self.frame).is_ok() {
            let old_pts = self.frame.pts().unwrap_or_else(|| todo!());
            if old_pts >= self.end {
                return Break(());
            }
            let new_pts = old_pts.saturating_sub(self.start).at_least(0);
            self.frame.set_pts(Some(new_pts));
            (self.apply_frame_extra)(&mut self.frame);

            self.encoder.send_frame(&self.frame).unwrap();
            self.send_packets(packet, output);
        }
        Continue(())
    }

    pub fn eof(&mut self, packet: &mut Packet, output: &mut Output) {
        self.decoder.send_eof().unwrap();
        self.process_frames(packet, output);
        self.encoder.send_eof().unwrap();
        self.send_packets(packet, output)
    }

    pub fn receive_packet(&mut self, output: &mut Output, packet: &mut Packet) -> ControlFlow<()> {
        debug_assert_eq!(packet.stream(), self.in_stream_idx);
        self.decoder.send_packet(packet);
        self.process_frames(packet, output)
    }
}

static TASK_LOCK: Mutex<()> = Mutex::new(());

pub fn export(
    ctx: Context,
    input: &mut Input,
    trim: (f32, f32),
    video_stream_idx: usize,
    audio_stream_idx: Option<usize>,
    status: SyncSender<TaskStatus>,
    input_path: PathBuf,
    auth: AuthArc,
    follow_up: ExportFollowUp,
    task_cmds: Sender<TaskCommand>,
    task_id: u32,
    #[cfg(feature = "async")] cancel_recv: Option<tokio::sync::oneshot::Receiver<()>>,
) {
    let use_trash = ctx
        .data_mut(|d| d.get_persisted(Id::new("useTrash")))
        .unwrap_or(true);

    let file_name = input_path.file_name().unwrap();
    let mut out_path =
        PathBuf::from(ctx.data_mut(|d| d.get_persisted::<String>(Id::new("outPath")).unwrap()));
    fs::create_dir_all(&out_path).unwrap();
    out_path.push(file_name);

    let mut output = output(&out_path).unwrap();

    let video = video_transcoder(input, &mut output, video_stream_idx, trim);

    // all of these are in the video stream's ts units
    let video_start = video.start;
    let video_dur = video.end - video.start;

    let mut video = Some(video);
    let mut audio =
        audio_stream_idx.map(|audio_stream_idx| {
            audio_transcoder(input, &mut output, audio_stream_idx, trim)
        });

    output.write_header().unwrap();

    let mut packet = Packet::empty();
    let is_final_stage = matches!(follow_up, ExportFollowUp::Nothing);
    let task_stage = TaskStage::Export {
        has_follow_up: !is_final_stage,
    };

    let lock = TASK_LOCK.lock().unwrap();

    while (video.is_some() || (audio.is_some() && audio_stream_idx.is_some())) && packet.read(input).is_ok() {
        let packet_pts = packet.pts();

        let stream = packet.stream();
        if stream == video_stream_idx {
            if let Some(transcoder) = &mut video {
                match status.try_send(TaskStatus {
                    stage: task_stage,
                    progress: ((packet_pts.unwrap_or(video_start) - video_start) as f32
                        / video_dur as f32).at_most(0.99),
                }) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => {
                        // the buffer's full? thats fine, just wait for it to empty
                    }
                    Err(TrySendError::Disconnected(_)) => {
                        // the channel is closed! that must mean the task was canceled OR the ui thread has shut down. just cancel
                        // todo: maybe delete the incomplete output file? should it be moved to trash or deleted directly?
                        return;
                    }
                }

                if transcoder
                    .receive_packet(&mut output, &mut packet)
                    .is_break()
                {
                    transcoder.eof(&mut packet, &mut output);
                    video = None;
                }
            }
        } else if Some(stream) == audio_stream_idx {
            if let Some(transcoder) = &mut audio {
                if transcoder
                    .receive_packet(&mut output, &mut packet)
                    .is_break()
                {
                    transcoder.eof(&mut packet, &mut output);
                    audio = None;
                }
            }
        }
    }
    if let Some(transcoder) = &mut video {
        transcoder.eof(&mut packet, &mut output);
    }
    if let Some(transcoder) = &mut audio {
        transcoder.eof(&mut packet, &mut output);
    }
    drop((video, audio));
    output.write_trailer().unwrap();
    drop(lock);

    status.send(TaskStatus {
        stage: task_stage,
        progress: 1.,
    }).unwrap();

    follow_up.run(
        &ctx,
        use_trash,
        status,
        out_path,
        auth,
        task_cmds,
        task_id,
        move |ctx| {
            if ctx
                .data_mut(|d| d.get_persisted(Id::new("deleteAfterExport")))
                .unwrap_or(false)
            {
                maybe_trash(use_trash, &input_path)
            }
        },
        #[cfg(feature = "async")]
        cancel_recv,
    );
}

fn maybe_trash(use_trash: bool, path: impl AsRef<Path>) {
    if use_trash {
        match trash::delete(&path) {
            Ok(_) => {}
            Err(trash::Error::CouldNotAccess { .. }) => {}
            Err(e) => panic!(
                "could not move {p} to the trash bin: {e}",
                p = path.as_ref().display()
            ),
        }
    } else {
        match fs::remove_file(&path) {
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => panic!("could not delete {p}: {e}", p = path.as_ref().display()),
        }
    }
}
