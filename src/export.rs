use std::backtrace::Backtrace;
use std::fmt::{Debug, Formatter};
use std::io::ErrorKind;
use std::ops::ControlFlow::{Break, Continue};
use std::ops::{ControlFlow, Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Sender, SyncSender, TrySendError};
use std::sync::Mutex;
use std::{fs, io};

use egui::{Context, Id, NumExt};
use ffmpeg::codec::context::Context as CodecContext;
use ffmpeg::encoder::Encoder;
use ffmpeg::format::context::{Input, Output};
use ffmpeg::format::output;
use ffmpeg::frame::{Audio as AudioFrame, Video as VideoFrame};
use ffmpeg::{codec, decoder, encoder, format, picture, Frame, Packet};
use log::{debug, info};
use thiserror::Error;

use crate::player::r#impl::sec2ts;
use crate::task::{TaskCommand, TaskStage, TaskStatus};
use crate::util::{lock_ignore_poison, precise_seek, report_err, result2flow, RationalExt};
use crate::{AuthArc, MessageManager};

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
        msg: MessageManager,
        use_trash: bool,
        status: SyncSender<TaskStatus>,
        output_file: PathBuf,
        auth: AuthArc,
        task_cmds: Sender<TaskCommand>,
        task_id: u32,
        after_follow_up: impl Send + 'static + FnOnce(&Context) -> Result<(), ExportError>,
        #[cfg(feature = "async")] cancel_recv: Option<tokio::sync::oneshot::Receiver<()>>,
    ) -> Result<(), ExportError> {
        match self {
            ExportFollowUp::Nothing => after_follow_up(ctx),
            #[cfg(feature = "youtube")]
            ExportFollowUp::Youtube { info_recv } => {
                use crate::youtube;
                status
                    .send(TaskStatus {
                        stage: TaskStage::YtAwaitingInfo,
                        progress: f32::NEG_INFINITY,
                    })
                    .unwrap();
                let should_delete = ctx
                    .data_mut(|d| d.get_persisted(Id::new("deleteAfterUpload")))
                    .unwrap_or(true);
                let ctx = Context::clone(ctx);
                msg.handle_err_spawn("uploading to YouTube", async move {
                    let file = tokio::fs::File::open(&output_file).await.map_err(|err| ExportError::OpenOutputFile { path: output_file.clone() /* technically we can do without cloning but it wouldnt be pretty */, err })?;
                    let auth = auth.read();
                    let yt = match &auth.youtube {
                        None => {
                            task_cmds.send(TaskCommand::Cancel { id: task_id }).expect("task commands should only be closed if the UI thread is done");
                            return Ok(());
                        }
                        Some(v) => v,
                    };
                    let (title, description, visibility) = match info_recv.await.expect("info receiver closed. looks like the user canceled the youtube upload (or the UI thread is closed)") {
                        youtube::YtInfo::Continue {
                            title,
                            description,
                            visibility,
                        } => (title, description, visibility),
                        youtube::YtInfo::Cancel => {
                            task_cmds.send(TaskCommand::Cancel { id: task_id }).expect("task commands should only be closed if the UI thread is done");
                            return Ok(());
                        }
                    };
                    status.send(TaskStatus {
                        stage: TaskStage::YtUpload,
                        progress: f32::INFINITY,
                    }).unwrap();

                    let upload_video = youtube::yt_upload(
                        crate::https_client(), output_file.file_name().map(|str| str.to_string_lossy().into_owned()).unwrap_or_else(|| "<unknown>".into()), file, yt, &title, &description, visibility,
                    );
                    tokio::select! {
                        v = upload_video => {
                            v?;
                            status.send(TaskStatus {
                                stage: TaskStage::YtUpload,
                                progress: 1.,
                            }).unwrap();
                            info!("finished uploading");

                            tokio::task::spawn_blocking(move || {
                                if should_delete {
                                    maybe_trash(use_trash, &output_file)?
                                }
                                after_follow_up(&ctx)
                            }).await.expect("cleanup closure should not panic")
                        }
                        _ = cancel_recv.expect("youtube follow-up should not be passed without also passing an async stopper") => {
                            Ok(())
                        }
                    }
                });

                Ok(())
            }
        }
    }
}

#[derive(Error, Debug)]
pub enum ExportError {
    #[error("Input and output paths cannot be the same:\n\t{}", .0.display())]
    SameOutput(PathBuf),
    #[error("The provided video file has no video stream")]
    NoVideoStream,
    #[error("Received an error from FFmpeg: {0}")]
    FFmpeg(#[from] ffmpeg_next::Error, Backtrace),
    #[error("No encoder found for codec: {}", .id.name())]
    NoEncoder { id: codec::Id },
    #[error("Could not create output directory(ies): {0}")]
    CreateOutDir(io::Error),
    #[error("Could not move {} to the recycling bin: {err}", .path.display())]
    TrashErr { path: PathBuf, err: trash::Error },
    #[error("Could not permanently delete {}: {err}", .path.display())]
    DeleteErr { path: PathBuf, err: io::Error },
    #[error("IO error: {0}")]
    GenericIo(#[from] io::Error, Backtrace),
    #[cfg(feature = "youtube")]
    #[error("Could not open output file {}: {err}", .path.display())]
    OpenOutputFile { path: PathBuf, err: io::Error },
    #[cfg(feature = "youtube")]
    #[error("Could not upload to YouTube:\n{0}")]
    YouTubeUpload(
        #[from]
        #[backtrace]
        crate::youtube::YtUploadError,
    ),
}

fn audio_transcoder(
    input: &mut Input,
    output: &mut Output,
    in_stream_idx: usize,
    (start, end): (f32, f32),
) -> Result<Transcoder<false, AudioFrame, impl FnMut(&mut AudioFrame)>, ExportError> {
    debug!("creating audio transcoder (input stream = index {in_stream_idx})");
    let global_header = output
        .format()
        .flags()
        .contains(format::Flags::GLOBAL_HEADER);
    let in_stream = input
        .stream(in_stream_idx)
        .expect("in_stream_idx should have been valid");
    let time_base = in_stream.time_base();
    let ts_rate = time_base.invert().value_f64();

    let decoder = CodecContext::from_parameters(in_stream.parameters())?
        .decoder()
        .audio()?;
    let codec_id = decoder.codec().expect("decoder should have a codec").id();
    debug!("audio codec: {}", codec_id.name());
    let codec = encoder::find(codec_id).ok_or(ExportError::NoEncoder { id: codec_id })?;
    let mut out_stream = output.add_stream(codec)?;
    out_stream.set_parameters(in_stream.parameters());
    out_stream.set_time_base(time_base);
    out_stream.set_avg_frame_rate(in_stream.avg_frame_rate());

    let start_ts = sec2ts(ts_rate, start as f64);
    let end_ts = sec2ts(ts_rate, end as f64);

    unsafe { (*out_stream.as_mut_ptr()).duration = end_ts - start_ts }
    let mut encoder = CodecContext::new_with_codec(codec).encoder().audio()?;
    encoder.set_parameters(out_stream.parameters())?;
    encoder.set_time_base(time_base);
    encoder.set_rate(decoder.rate() as i32);

    if global_header {
        encoder.set_flags(codec::Flags::GLOBAL_HEADER);
    }

    let encoder = encoder.open_as(codec)?;
    out_stream.set_parameters(&encoder);

    Ok(Transcoder {
        start: start_ts,
        end: end_ts,
        decoder: decoder.0,
        encoder: encoder.0 .0,
        in_stream_idx,
        out_stream_idx: out_stream.index(),
        frame: AudioFrame::empty(),
        apply_frame_extra: |_| {},
    })
}

fn video_transcoder(
    input: &mut Input,
    output: &mut Output,
    in_stream_idx: usize,
    (start, end): (f32, f32),
) -> Result<Transcoder<true, VideoFrame, impl FnMut(&mut VideoFrame)>, ExportError> {
    debug!("creating video transcoder (input stream = index {in_stream_idx})");

    let global_header = output
        .format()
        .flags()
        .contains(format::Flags::GLOBAL_HEADER);
    let in_stream = input
        .stream(in_stream_idx)
        .expect("in_stream_idx should have been valid");
    let time_base = in_stream.time_base();
    let ts_rate = time_base.invert().value_f64();

    let mut decoder = CodecContext::from_parameters(in_stream.parameters())?
        .decoder()
        .video()?;

    let codec_id = decoder.codec().expect("decoder should have a codec").id();
    debug!("video codec: {}", codec_id.name());
    let codec = encoder::find(codec_id).ok_or(ExportError::NoEncoder { id: codec_id })?;
    let mut out_stream = output.add_stream(codec)?;
    out_stream.set_parameters(in_stream.parameters());
    out_stream.set_time_base(time_base);
    out_stream.set_avg_frame_rate(in_stream.avg_frame_rate());

    let start_ts = sec2ts(ts_rate, start as f64);
    let end_ts = sec2ts(ts_rate, end as f64);

    unsafe { (*out_stream.as_mut_ptr()).duration = end_ts - start_ts }
    let mut encoder = CodecContext::new_with_codec(codec).encoder().video()?;
    encoder.set_parameters(out_stream.parameters())?;
    encoder.set_time_base(time_base);
    if global_header {
        encoder.set_flags(codec::Flags::GLOBAL_HEADER);
    }
    let encoder = encoder.open_as(codec)?;
    out_stream.set_parameters(&encoder);

    let mut frame = VideoFrame::empty();

    precise_seek(input, &mut decoder, &mut frame, in_stream_idx, start_ts)?;

    Ok(Transcoder {
        start: start_ts,
        end: end_ts,
        decoder: decoder.0,
        encoder: encoder.0 .0,
        in_stream_idx,
        out_stream_idx: out_stream.index(),
        frame,
        apply_frame_extra: |frame| {
            // super important! make sure we don't confuse the encoder by keeping the old frame types
            //      the encoder should by itself choose what frame type to use per frame
            frame.set_kind(picture::Type::None);
        },
    })
}

struct Transcoder<const IS_VIDEO: bool, Fr: Deref<Target = Frame> + DerefMut, Fn: FnMut(&mut Fr)> {
    start: i64,
    end: i64,
    decoder: decoder::opened::Opened,
    encoder: Encoder,
    in_stream_idx: usize,
    out_stream_idx: usize,
    frame: Fr,
    apply_frame_extra: Fn,
}

impl<const IS_VIDEO: bool, Fr: Deref<Target = Frame> + DerefMut, Fn: FnMut(&mut Fr)>
    Transcoder<IS_VIDEO, Fr, Fn>
{
    fn send_packets(
        &mut self,
        packet: &mut Packet,
        output: &mut Output,
    ) -> Result<(), ffmpeg_next::Error> {
        while self.encoder.receive_packet(packet).is_ok() {
            packet.set_stream(self.out_stream_idx);
            packet.write_interleaved(output)?;
        }
        Ok(())
    }

    fn process_frames(
        &mut self,
        packet: &mut Packet,
        output: &mut Output,
    ) -> ControlFlow<Option<ffmpeg_next::Error>> {
        while self.decoder.receive_frame(&mut self.frame).is_ok() {
            let old_pts = self.frame.pts().unwrap_or_else(|| todo!());
            if old_pts >= self.end {
                return Break(None);
            }
            let new_pts = old_pts.saturating_sub(self.start).at_least(0);
            self.frame.set_pts(Some(new_pts));
            (self.apply_frame_extra)(&mut self.frame);

            result2flow(self.encoder.send_frame(&self.frame).map_err(Some))?;
            result2flow(self.send_packets(packet, output).map_err(Some))?;
        }
        Continue(())
    }

    pub fn eof(
        &mut self,
        packet: &mut Packet,
        output: &mut Output,
    ) -> Result<(), ffmpeg_next::Error> {
        debug!(
            "finishing {} stream",
            if IS_VIDEO { "video" } else { "audio" }
        );
        self.decoder.send_eof()?;
        if let Break(Some(err)) = self.process_frames(packet, output) {
            return Err(err);
        }
        self.encoder.send_eof()?;
        self.send_packets(packet, output)
    }

    pub fn receive_packet(
        &mut self,
        output: &mut Output,
        packet: &mut Packet,
    ) -> ControlFlow<Option<ffmpeg_next::Error>> {
        debug_assert_eq!(packet.stream(), self.in_stream_idx);
        result2flow(self.decoder.send_packet(packet).map_err(Some))?;
        self.process_frames(packet, output)
    }
}

static TASK_LOCK: Mutex<()> = Mutex::new(());

#[must_use]
pub fn export(
    ctx: Context,
    msg: MessageManager,
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
) -> Result<(), ExportError> {
    let use_trash = ctx
        .data_mut(|d| d.get_persisted(Id::new("useTrash")))
        .unwrap_or(true);

    let file_name = input_path
        .file_name()
        .expect("user should not be able to provide input path with no file name");
    let out_path = ctx.data_mut(|d| {
        d.get_persisted::<String>(Id::new("outPath"))
            .expect("outPath should be already set once we get to exporting")
    });
    fs::create_dir_all(&out_path).map_err(ExportError::CreateOutDir)?;
    let mut out_path = fs::canonicalize(&out_path).unwrap_or_else(|e| {
        report_err("while canonicalizing output path", &e);
        PathBuf::from(out_path)
    });

    out_path.push(file_name);

    if input_path == out_path {
        return Err(ExportError::SameOutput(out_path));
    }
    debug!(
        "beginning file export:
            input:  {i}
            output: {o}",
        i = input_path.display(),
        o = out_path.display()
    );

    let mut output = output(&out_path)?;

    let video = video_transcoder(input, &mut output, video_stream_idx, trim)?;

    // all of these are in the video stream's ts units
    let video_start = video.start;
    let video_dur = video.end - video.start;

    let mut video = Some(video);
    let mut audio = match audio_stream_idx {
        None => None,
        Some(audio_stream_idx) => Some(audio_transcoder(
            input,
            &mut output,
            audio_stream_idx,
            trim,
        )?),
    };

    output.write_header()?;

    let mut packet = Packet::empty();
    let is_final_stage = matches!(follow_up, ExportFollowUp::Nothing);
    let task_stage = TaskStage::Export {
        has_follow_up: !is_final_stage,
    };

    let lock = lock_ignore_poison(&TASK_LOCK);

    while (video.is_some() || (audio.is_some() && audio_stream_idx.is_some()))
        && packet.read(input).is_ok()
    {
        let packet_pts = packet.pts();

        let stream = packet.stream();
        if stream == video_stream_idx {
            if let Some(transcoder) = &mut video {
                match status.try_send(TaskStatus {
                    stage: task_stage,
                    progress: ((packet_pts.unwrap_or(video_start) - video_start) as f32
                        / video_dur as f32)
                        .at_most(0.99),
                }) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => {
                        // the buffer's full? thats fine, just wait for it to empty
                    }
                    Err(TrySendError::Disconnected(_)) => {
                        // the channel is closed! that must mean the task was canceled OR the ui thread has shut down. just cancel
                        // todo: maybe delete the incomplete output file? should it be moved to trash or deleted directly?
                        return Ok(());
                    }
                }

                if let Break(r) = transcoder.receive_packet(&mut output, &mut packet) {
                    if let Some(err) = r {
                        return Err(err.into());
                    }

                    transcoder.eof(&mut packet, &mut output)?;
                    video = None;
                }
            }
        } else if Some(stream) == audio_stream_idx {
            if let Some(transcoder) = &mut audio {
                if let Break(r) = transcoder.receive_packet(&mut output, &mut packet) {
                    if let Some(err) = r {
                        return Err(err.into());
                    }

                    transcoder.eof(&mut packet, &mut output)?;
                    audio = None;
                }
            }
        }
    }
    if let Some(transcoder) = &mut video {
        transcoder.eof(&mut packet, &mut output)?;
    }
    if let Some(transcoder) = &mut audio {
        transcoder.eof(&mut packet, &mut output)?;
    }
    drop((video, audio));
    output.write_trailer()?;
    drop(lock);

    status
        .send(TaskStatus {
            stage: task_stage,
            progress: 1.,
        })
        .unwrap();

    debug!("file export finished");

    follow_up.run(
        &ctx,
        msg,
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
                maybe_trash(use_trash, &input_path)?;
            }
            Ok(())
        },
        #[cfg(feature = "async")]
        cancel_recv,
    )
}

fn maybe_trash(use_trash: bool, path: impl AsRef<Path>) -> Result<(), ExportError> {
    if use_trash {
        match trash::delete(&path) {
            Ok(_) => {}
            Err(trash::Error::CouldNotAccess { .. }) => {}
            Err(err) => {
                return Err(ExportError::TrashErr {
                    err,
                    path: path.as_ref().to_path_buf(),
                })
            }
        }
    } else {
        match fs::remove_file(&path) {
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(err) => {
                return Err(ExportError::DeleteErr {
                    err,
                    path: path.as_ref().to_path_buf(),
                })
            }
        }
    }
    Ok(())
}
