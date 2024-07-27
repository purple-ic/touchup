use std::cmp::min;
use std::ffi::c_int;
use std::mem::size_of;
use std::ops::{ControlFlow, RangeInclusive};
use std::ops::ControlFlow::{Break, Continue};
use std::path::PathBuf;
use std::sync::{Arc, mpsc, Mutex};
use std::sync::mpsc::{Receiver, Sender, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui::Context;
use ffmpeg::{media, Packet, Stream, StreamMut};
use ffmpeg::codec::context::Context as CodecContext;
use ffmpeg::ffi::{av_seek_frame, AVDiscard, AVSEEK_FLAG_BACKWARD};
use ffmpeg::ffi::AVDiscard::AVDISCARD_NONE;
use ffmpeg::format::{Pixel, Sample};
use ffmpeg::format::context::Input;
use ffmpeg::format::sample::Type;
use ffmpeg::frame::{Audio, Video as VideoFrame};
use ffmpeg::frame::{Audio as AudioFrame, Video};
use ffmpeg::media::Type as MediaType;
use ffmpeg::packet::Mut;
use ffmpeg::sys::AV_TIME_BASE;
use ffmpeg::sys::AVDiscard::AVDISCARD_ALL;
use ffmpeg_next::codec::decoder::audio::Audio as AudioDecoder;
use ffmpeg_next::codec::decoder::video::Video as VideoDecoder;
use ffmpeg_next::software::resampling::context::Context as Resampler;
use ffmpeg_next::software::scaling::Context as Scaler;
use rodio::{OutputStream, Sink, Source};

use VideoTime::PausedAt;

use crate::{AuthArc, PlayerTexture, TaskCommand, TaskStatus, TextureArc};
use crate::export::{export, ExportFollowUp};
use crate::player::r#impl::PlayerCommand::UpdatePreview;
use crate::player::r#impl::VideoTime::Anchored;
use crate::util::{
    CloseReceiver, OwnedThreads, precise_seek, RationalExt, spawn_owned_thread, time_diff,
    Updatable,
};

// #[derive(Debug)]
enum PlayerCommand {
    /// Pause if we're currently playing, or unpause if we're currently paused.
    ///
    /// When paused, both the audio and video threads are
    /// blocked until unpaused. (however the video thread
    /// can still process commands such as [enabling and
    /// updating previews](PlayerCommand::EnablePreviewMode)
    /// and [seeking](PlayerCommand::SeekTo).)
    PauseUnpause,
    Skip15s,
    Back15s,
    /// Perform a precise seek to the given position.
    ///
    /// This is a rather expensive operation and it should
    /// only be used when the player explicitly wishes to
    /// move to a different part of the video. A precise seek
    /// may sometimes take 300ms (that is _very_ noticeable and
    /// pretty jarring to see).
    SeekTo(Duration),
    // UpdatePreview(Duration),
    // are we about to have better documentation than libav? i guess thats not a very high bar though
    /// Enables _preview mode_.
    ///
    /// During preview mode, sound playback will be completely disabled while
    /// video playback enters this "preview mode": the current frame will
    /// not update automatically with time, but it _will_ update when the
    /// UI thread requests it updates through the [preview update
    /// request command](PlayerCommand::UpdatePreview).
    ///
    /// When a preview update request is made, the player thread will seek
    /// to the first I-frame _before_ the requested position in the video.
    /// This will give the user the general idea of what's happening at the
    /// given timestamp without having to perform a precise seek (which may
    /// sometimes take up to 300ms!).
    ///
    /// To disable preview mode, one must [seek](PlayerCommand::SeekTo) to the
    /// final position the user wishes to take themselves to.
    EnablePreviewMode,
    UpdatePreview(Duration),
    Export {
        trim: (f32, f32),
        audio_track: usize,
        status: SyncSender<TaskStatus>,
        path: PathBuf,
        id: u32,
        auth: AuthArc,
        follow_up: ExportFollowUp,
        task_cmds: Sender<TaskCommand>,
        #[cfg(feature = "async")]
        cancel_recv: Option<tokio::sync::oneshot::Receiver<()>>,
    },
}

pub struct Player {
    resolution: [usize; 2],
    frame_time: Duration,
    commands: SyncSender<PlayerCommand>,
    // is_paused: bool,
    time: Updatable<VideoTime>,
    duration: Duration,
    #[allow(unused)] // it is actually used for dropping behaviour
    threads: OwnedThreads,
    change_audio_track: Sender<usize>,
    audio_tracks: usize,
    preview_mode: bool,
    change_volume: Sender<f32>,
}

struct PlayerStartupInfo {
    resolution: [usize; 2],
    frame_time: Duration,
    duration: Duration,
    audio_tracks: usize,
}

#[derive(Copy, Clone, Debug)]
pub enum VideoTime {
    Anchored(Instant),
    PausedAt(Duration),
}

impl VideoTime {
    pub fn now(&self) -> Duration {
        match self {
            Anchored(i) => i.elapsed(),
            PausedAt(t) => *t,
        }
    }
}

fn set_stream_discard(stream: &mut StreamMut, discard: AVDiscard) {
    unsafe {
        let stream = &mut *stream.as_mut_ptr();
        stream.discard = discard;
    }
}

fn new_audio_decoder(audio_stream: &Stream) -> AudioDecoder {
    CodecContext::from_parameters(audio_stream.parameters())
        .unwrap_or_else(|err| todo!("could not get decoder: {err}"))
        .decoder()
        .audio()
        .unwrap_or_else(|e| todo!("{e:?}"))
}

impl Player {
    pub fn change_volume(&self, volume: f32) {
        self.change_volume.send(volume).unwrap()
    }

    pub fn begin_export(
        &self,
        trim: (f32, f32),
        audio_track: usize,
        status: SyncSender<TaskStatus>,
        path: PathBuf,
        id: u32,
        auth: AuthArc,
        follow_up: ExportFollowUp,
        task_cmds: Sender<TaskCommand>,
        #[cfg(feature = "async")]
        cancel_recv: Option<tokio::sync::oneshot::Receiver<()>>,
    ) {
        self.commands.send(PlayerCommand::Export {
            trim,
            audio_track,
            status,
            path,
            id,
            auth,
            follow_up,
            task_cmds,
            #[cfg(feature = "async")]
            cancel_recv,
        });
    }

    pub fn preview_mode(&self) -> bool {
        self.preview_mode
    }

    pub fn update_preview(&self, to: Duration) {
        self.commands.try_send(PlayerCommand::UpdatePreview(to));
    }

    pub fn enable_preview_mode(&mut self) {
        if !self.preview_mode {
            self.preview_mode = true;
            let new_time = self.time.get().now();
            self.time.replace_current(PausedAt(new_time));
            self.commands
                .send(PlayerCommand::EnablePreviewMode)
                .unwrap()
        }
    }

    pub fn set_audio_track(&self, idx: usize) {
        self.change_audio_track.send(idx).unwrap()
    }

    pub fn nb_audio_tracks(&self) -> usize {
        self.audio_tracks
    }

    pub fn seek_to(&mut self, time: Duration) {
        self.preview_mode = false;
        self.time.replace_current(PausedAt(time));
        self.commands.send(PlayerCommand::SeekTo(time)).unwrap();
    }

    pub fn duration(&self) -> Duration {
        self.duration
    }

    pub fn is_paused(&mut self) -> bool {
        matches!(self.time.get(), PausedAt(_))
    }

    pub fn toggle_pause(&mut self) {
        // self.is_paused.toggle();
        self.commands.send(PlayerCommand::PauseUnpause);
    }

    pub fn user_toggle_pause(&mut self, trim: &RangeInclusive<f32>) {
        if self.time.get().now().as_secs_f32() >= *trim.end() {
            let time = Duration::from_secs_f32(*trim.start());
            self.time.replace_current(Anchored(Instant::now() - time));
            self.seek_to(time);
            if self.is_paused() {
                self.toggle_pause()
            }
        } else {
            self.toggle_pause();
        }
    }

    pub fn forward(&mut self) {
        self.commands.try_send(PlayerCommand::Skip15s);
    }

    pub fn backward(&mut self) {
        self.commands.try_send(PlayerCommand::Back15s);
    }

    pub fn resolution(&self) -> [usize; 2] {
        self.resolution
    }

    pub fn frame_time(&self) -> Duration {
        self.frame_time
    }

    pub fn time(&mut self) -> VideoTime {
        *self.time.get()
    }

    pub fn new(gui_ctx: Context, input: Input, input_path: PathBuf, starting_volume: f32, texture: TextureArc) -> Self {
        let (info_send, info_recv) = mpsc::channel();
        let (commands_send, commands_recv) = mpsc::sync_channel(20);
        let (time_updater, time) = Updatable::new(PausedAt(Duration::ZERO));
        let (change_audio_track, audio_track_changes) = mpsc::channel();
        let (change_volume, volume) = Updatable::new(starting_volume);

        let thread_owner = spawn_owned_thread("video".into(), |closer| {
            Self::setup(
                gui_ctx,
                input,
                input_path,
                info_send,
                commands_recv,
                time_updater,
                closer,
                audio_track_changes,
                volume,
                texture
            );
        })
            .unwrap()
            .1;
        let PlayerStartupInfo {
            resolution,
            frame_time,
            duration,
            audio_tracks,
        } = info_recv.recv().unwrap();

        Self {
            resolution,
            frame_time,
            commands: commands_send,
            // is_paused: false,
            time,
            duration,
            threads: thread_owner,
            change_audio_track,
            audio_tracks,
            preview_mode: false,
            change_volume,
        }
    }

    fn setup(
        gui_ctx: Context,
        mut input: Input,
        input_path: PathBuf,
        info_send: mpsc::Sender<PlayerStartupInfo>,
        commands_recv: Receiver<PlayerCommand>,
        time_updater: Sender<VideoTime>,
        closer: CloseReceiver,
        audio_track_changes: Receiver<usize>,
        volume: Updatable<f32>,
        texture: TextureArc
    ) {
        let audio_stream_idx = input.streams().best(MediaType::Audio).unwrap().index();
        let mut audio_tracks = 1;

        for mut stream in input
            .streams_mut()
            .filter(|stream| is_audio(&stream) && stream.index() != audio_stream_idx)
        {
            audio_tracks += 1;
            set_stream_discard(&mut stream, AVDISCARD_ALL)
        }

        let video_stream = input.streams().best(MediaType::Video).unwrap();
        let audio_stream = input.stream(audio_stream_idx).unwrap();

        let video_stream_idx = video_stream.index();

        let ts_rate = video_stream.time_base().invert().value_f64();
        let duration = ts2dur(AV_TIME_BASE as f64, input.duration());

        let (video_send, video_recv) = mpsc::sync_channel(5);
        let (audio_send, audio_recv) = mpsc::sync_channel(5);

        let make_video_decoder = || {
            CodecContext::from_parameters(video_stream.parameters())
                .unwrap_or_else(|err| todo!("could not get decoder: {err}"))
                .decoder()
                .video()
                .unwrap_or_else(|e| todo!("{e}"))
        };
        let video_decoder = make_video_decoder();

        let audio_decoder = new_audio_decoder(&audio_stream);

        let resolution = [video_decoder.width(), video_decoder.height()];

        let scaler = video_decoder
            .converter(
                // convert to rgb24 pixel format and keep the same resolution
                Pixel::RGB24,
            )
            .unwrap_or_else(|e| todo!("could not create scaler: {e}"));

        let resampler = audio_decoder.resampler(
            Sample::F32(Type::Packed /* packed = interleaved (rodio only supports interleaved sample formats) */), audio_decoder.channel_layout(), audio_decoder.rate(),
        ).unwrap_or_else(|e| todo!("could not create resampler: {e}"));

        let avg_frame_rate = video_stream.avg_frame_rate();
        let frame_time = Duration::from_secs_f64(avg_frame_rate.invert().value_f64());
        let input_mutex = Arc::new(Mutex::new((input, LastTs { stream: 0, ts: 0 })));

        // set up our audio output
        let (audio_output_stream, audio_output) = OutputStream::try_default().unwrap();

        let (audio_request_flush, audio_flush_requests) = mpsc::channel::<()>();

        let audio_manager = AudioManager {
            stream_idx: audio_stream_idx,
            video_stream_idx,
            input_mutex: Arc::clone(&input_mutex),
            audio_recv,
            flush_requests: audio_flush_requests,
            video_send,
            frame: Audio::empty(),
            frame_resampled: Audio::empty(),
            sample_idx: usize::MAX,
            decoder: audio_decoder,
            resampler,
            closer: closer.cheap_clone(),
            audio_track_changes,
            volume,
        };

        let sink = Sink::try_new(&audio_output).unwrap();
        sink.append(audio_manager);

        let mut video_manager = VideoManager {
            stream_idx: video_stream_idx,
            // audio_stream_idx,
            ts_rate,
            input_mutex,
            video_recv,
            audio_send,
            audio_request_flush,
            closer,
            frame: Video::empty(),
            frame_scaled: Video::empty(),
            decoder: video_decoder,
            scaler,
            gui_ctx,
            time: Anchored(Instant::now()),
            // anchor: Instant::now(),
            texture,
            input_path,

            audio_sink: sink,
            commands_recv,
            time_updater,

            #[cfg(print_fps)]
            frame_counter: FrameCounter::new(),
        };
        video_manager.time_updater.send(video_manager.time).unwrap();

        info_send
            .send(PlayerStartupInfo {
                resolution: resolution.map(|x| x as usize),
                frame_time,
                duration,
                audio_tracks,
            })
            .unwrap();

        video_manager.begin_loop();

        // make sure rust only drops our streams at the end
        //      otherwise rust attempts to make their lifetimes as
        //      short as it determines they have to be, but it doesn't
        //      account for the runtime ramifications of dropping our
        //      audio streams (that disconnects the stream and turns off the sound)
        drop((audio_output_stream, audio_output));
        println!("ending video thread");
    }
}

fn next_packet(
    mut self_stream_check: impl FnMut(&Stream) -> bool,
    mut partner_stream_check: impl FnMut(&Stream) -> bool,
    // self_stream_idx: usize,
    // partner_stream_idx: usize,
    recv: &mut Receiver<Packet>,
    send: &mut mpsc::SyncSender<Packet>,
    input_mutex: &Arc<Mutex<(Input, LastTs)>>,
) -> Option<Packet> {
    let mut fine_ill_do_it_myself = |(input, last_ts): &mut (Input, LastTs)| {
        for (packet_stream, packet) in input.packets() {
            if let Some(ts) = packet.pts() {
                *last_ts = LastTs {
                    stream: packet_stream.index(),
                    ts,
                };
            }
            if self_stream_check(&packet_stream) {
                return Some(packet);
            } else if partner_stream_check(&packet_stream) {
                send.try_send(packet);
                continue;
            } else {
                println!("{}", packet_stream.index());
                continue;
            }
        }
        None
    };

    if let Ok(packet) = recv.try_recv() {
        return Some(packet);
    } else if let Ok(mut input) = input_mutex.try_lock() {
        fine_ill_do_it_myself(&mut input)
    } else {
        let mut input_lock = input_mutex.lock().unwrap();
        if let Ok(packet) = recv.try_recv() {
            Some(packet)
        } else {
            fine_ill_do_it_myself(&mut input_lock)
        }
    }
}

pub fn sec2ts(ts_rate: f64, sec: f64) -> i64 {
    (sec * ts_rate) as i64
}

pub fn ts2sec(ts_rate: f64, ts: i64) -> f64 {
    ts as f64 / ts_rate
}

pub fn ts2dur(ts_rate: f64, ts: i64) -> Duration {
    Duration::from_secs_f64(ts2sec(ts_rate, ts))
}

#[derive(Debug, Copy, Clone)]
struct LastTs {
    stream: usize,
    ts: i64,
}

struct VideoManager {
    stream_idx: usize,
    // audio_stream_idx: usize,
    ts_rate: f64,

    input_mutex: Arc<Mutex<(Input, LastTs)>>,
    video_recv: Receiver<Packet>,
    audio_send: SyncSender<Packet>,
    audio_request_flush: Sender<()>,
    closer: CloseReceiver,

    frame: VideoFrame,
    frame_scaled: VideoFrame,

    decoder: VideoDecoder,
    scaler: Scaler,

    gui_ctx: Context,
    time: VideoTime,
    texture: TextureArc,
    input_path: PathBuf,

    audio_sink: Sink,
    commands_recv: Receiver<PlayerCommand>,
    time_updater: Sender<VideoTime>,

    #[cfg(print_fps)]
    frame_counter: FrameCounter,
}

fn is_audio(stream: &Stream) -> bool {
    stream.parameters().medium() == media::Type::Audio
}

fn is_video(video_idx: usize) -> impl FnMut(&Stream) -> bool {
    move |stream| stream.index() == video_idx
}

impl VideoManager {
    fn next_packet(&mut self) -> Option<Packet> {
        next_packet(
            is_video(self.stream_idx),
            is_audio,
            // self.stream_idx,
            // self.audio_stream_idx,
            &mut self.video_recv,
            &mut self.audio_send,
            &self.input_mutex,
        )
    }

    fn ts_now(&self) -> i64 {
        sec2ts(self.ts_rate, self.time.now().as_secs_f64())
    }

    fn precise_seek(&mut self, pause: bool, target: Duration) {
        self.audio_request_flush.send(()).unwrap();

        let anchor_old = self.time.now();
        self.time = PausedAt(target);
        self.time_updater.send(self.time).unwrap();

        let mut input = self.input_mutex.lock().unwrap();
        // dump the packets we might have received from the audio thread
        //    otherwise we might feed old packets to the video decoder
        //    which causes weird artifacts and sometimes freezes
        //    (since input is locked, the channel won't receive anything new
        //     while we're performing the seek)
        for _ in self.video_recv.try_iter() {}
        let target_ts = sec2ts(self.ts_rate, target.as_secs_f64().floor());

        precise_seek(
            &mut input.0,
            &mut self.decoder,
            &mut self.frame,
            self.stream_idx,
            target_ts,
        );
        if pause {
            self.scaler.run(&self.frame, &mut self.frame_scaled)
                .unwrap();
            let mut tex = self.texture.lock().unwrap();
            update_texture(&self.frame_scaled, &mut tex);
        }

        let new_frame_time = target;
        dbg!(new_frame_time);

        // self.anchor = Instant::now() - new_frame_time;
        self.time = if pause {
            PausedAt(new_frame_time)
        } else {
            Anchored(Instant::now() - new_frame_time)
        };
        self.time_updater.send(self.time);
        self.gui_ctx.request_repaint();
        println!("jumped {:?}", time_diff(new_frame_time, anchor_old));
    }

    fn update_preview(&mut self, time: Duration) {
        let mut input = self.input_mutex.lock().unwrap();
        self.decoder.flush();
        unsafe {
            let result = av_seek_frame(
                input.0.as_mut_ptr(),
                self.stream_idx as c_int,
                sec2ts(self.ts_rate, time.as_secs_f64()),
                AVSEEK_FLAG_BACKWARD,
            );
            if result < 0 {
                panic!(
                    "ffmpeg err when seeking to preview: {}",
                    ffmpeg::Error::from(result)
                )
            }
        }
        for (stream, packet) in input.0.packets() {
            if stream.index() == self.stream_idx {
                // if the pts of the packet matches the pts of the last preview, just skip it to save time
                self.decoder.send_packet(&packet);
                if self.decoder.receive_frame(&mut self.frame).is_ok() {
                    break;
                }
            }
        }
        // we've done everything we need with the input. drop the lock now
        drop(input);
        self.decoder.flush();
        self.scaler
            .run(&self.frame, &mut self.frame_scaled)
            .unwrap();
        let mut tex = self.texture.lock().unwrap();
        update_texture(&self.frame_scaled, &mut *tex);
    }

    fn preview_mode(&mut self, was_paused: bool) {
        self.audio_sink.pause();
        while let Ok(command) = self.commands_recv.recv() {
            // println!("processing command (during preview mode): {command:?}");
            match command {
                PlayerCommand::SeekTo(target) => {
                    self.precise_seek(was_paused, target);
                    if !was_paused {
                        self.audio_sink.play();
                    }
                    break;
                }
                PlayerCommand::UpdatePreview(target) => self.update_preview(target),
                _ => {}
            }
        }
    }

    fn export(
        &self,
        trim: (f32, f32),
        audio_track: usize,
        status: SyncSender<TaskStatus>,
        path: PathBuf,
        id: u32,
        auth: AuthArc,
        follow_up: ExportFollowUp,
        task_cmds: Sender<TaskCommand>,
        #[cfg(feature = "async")]
        cancel_recv: Option<tokio::sync::oneshot::Receiver<()>>,
    ) {
        self.audio_sink.clear();
        let ctx = self.gui_ctx.clone();
        let input_mutex = Arc::clone(&self.input_mutex);
        let stream_idx = self.stream_idx;
        thread::Builder::new()
            .name(format!("taskExport{id}"))
            .spawn(move || {
                let mut input = input_mutex.lock().unwrap();
                let audio_stream_idx = input
                    .0
                    .streams()
                    .filter(is_audio)
                    .nth(audio_track)
                    .unwrap()
                    .index();
                export(
                    ctx,
                    &mut input.0,
                    trim,
                    stream_idx,
                    audio_stream_idx,
                    status,
                    path,
                    auth,
                    follow_up,
                    task_cmds,
                    id,
                    #[cfg(feature = "async")]
                    cancel_recv,
                )
            })
            .unwrap();
    }

    #[must_use]
    fn process_command(&mut self, paused: bool, command: PlayerCommand) -> ControlFlow<()> {
        // println!("processing command: {command:?}");
        match command {
            PlayerCommand::PauseUnpause => {
                debug_assert!(!paused, "we should not be able to receive PauseUnpaused while paused and not in pause mode");

                // pause the audio playback
                self.audio_sink.pause();
                self.time = PausedAt(self.time.now());
                self.time_updater.send(self.time).unwrap();

                // block while we wait for the next command (and we won't stop blocking until the command is to unpause)
                loop {
                    let command = self.commands_recv.recv().unwrap();
                    // println!("processing command (during pause): {command:?}");
                    match command {
                        PlayerCommand::PauseUnpause => {
                            break;
                        }
                        _ => self.process_command(true, command)?,
                    }
                }

                self.time = Anchored(Instant::now() - self.time.now());
                self.time_updater.send(self.time).unwrap();
                self.audio_sink.play();
            }
            PlayerCommand::Skip15s => {
                self.precise_seek(paused, self.time.now() + Duration::from_secs(15))
            }
            PlayerCommand::Back15s => self.precise_seek(
                paused,
                self.time.now().saturating_sub(Duration::from_secs(15)),
            ),
            PlayerCommand::SeekTo(time) => self.precise_seek(paused, time),
            UpdatePreview(_) => {
                // ignore the preview update request since we're not in preview mode
            }
            PlayerCommand::EnablePreviewMode => {
                self.preview_mode(paused);
            }
            PlayerCommand::Export {
                trim,
                audio_track,
                status,
                path,
                id,
                auth,
                follow_up,
                task_cmds,
                #[cfg(feature = "async")]
                cancel_recv
            } => {
                self.export(
                    trim,
                    audio_track,
                    status,
                    path,
                    id,
                    auth,
                    follow_up,
                    task_cmds,
                    #[cfg(feature = "async")]
                    cancel_recv,
                );
                return Break(());
            }
        }
        Continue(())
    }

    #[must_use]
    fn process_commands(&mut self) -> ControlFlow<()> {
        while let Ok(command) = self.commands_recv.try_recv() {
            self.process_command(false, command)?;
        }
        Continue(())
    }

    fn begin_loop(&mut self) {
        loop {
            if self.closer.should_close() {
                let _ = self.process_commands();
                break;
            }

            if let Break(()) = self.process_commands() {
                break;
            }
            self.attempt_refresh();

            let next_frame_sec =
                Duration::from_secs_f64(ts2sec(self.ts_rate, self.frame.pts().unwrap_or(0)));
            let sec_now = self.time.now();

            thread::sleep(min(
                Duration::from_secs_f32(1. / 60.),
                next_frame_sec.saturating_sub(sec_now),
            ));
        }
    }

    fn attempt_refresh(&mut self) {
        let frame_pts = self.frame.pts().unwrap_or(0);
        let ts_now = self.ts_now();

        if ts_now >= frame_pts {
            // update the texture (if the frame isn't empty)
            if self.frame_scaled.planes() != 0 {
                let mut tex = self.texture.lock().unwrap();
                update_texture(&self.frame_scaled, &mut *tex);
            }
            // decode the next frame for later
            loop {
                if self.decoder.receive_frame(&mut self.frame).is_ok() {
                    if self.frame.pts().unwrap() < self.ts_now() {
                        continue;
                    }
                    #[cfg(print_fps)]
                    {
                        println!("{}fps", self.frame_counter.frame());
                    }
                    break;
                } else {
                    let packet = match self.next_packet() {
                        None => {
                            self.frame_scaled = VideoFrame::empty();
                            return;
                        }
                        Some(v) => v,
                    };
                    self.decoder.send_packet(&packet);
                }
            }
            // rescale our new frame
            self.scaler
                .run(&self.frame, &mut self.frame_scaled)
                .unwrap();
        }
    }
}

fn update_texture(frame: &VideoFrame, texture: &mut PlayerTexture) {
    let size = [frame.width() as usize, frame.height() as usize];
    texture.size = size;
    texture.changed = true;
    texture.bytes.clear();

    let data = frame.data(0);
    let total_pixels = size[0] * size[1];
    let total_bytes = total_pixels * 3;
    texture.bytes.reserve(total_bytes);
    if total_bytes != data.len() {
        /*
        when the width of the frame isn't a multiple of 32,
        ffmpeg adds padding to make it so.

        stride (aka linesize) tells us how many bytes are
        stored by ffmped per line,

        we have to go through each line (each Y position) and
        copy to our new buffer only the data that actually matters
        (ignoring the padding bytes added to each line)

        note that if no padding is added, we don't do all of this
        and just copy the buffer as-is
         */
        let line_size = frame.stride(0);
        let real_line_size = size[0] * 3;

        for y in 0..size[1] /* height */ {
            let real_data = &data[(y * line_size)..][..real_line_size];
            texture.bytes.extend_from_slice(real_data);
        }
        // make sure no reallocations were made in debug mode
        debug_assert_eq!(texture.bytes.len(), total_bytes);
        texture.bytes.shrink_to_fit();
    } else {
        texture.bytes.extend_from_slice(data);
    }
}

struct AudioManager {
    stream_idx: usize,
    video_stream_idx: usize,

    input_mutex: Arc<Mutex<(Input, LastTs)>>,
    audio_recv: Receiver<Packet>,
    video_send: SyncSender<Packet>,
    flush_requests: Receiver<()>,
    closer: CloseReceiver,
    audio_track_changes: Receiver<usize>,

    frame: AudioFrame,
    frame_resampled: AudioFrame,
    sample_idx: usize,

    decoder: AudioDecoder,
    resampler: Resampler,

    volume: Updatable<f32>,
}

impl AudioManager {
    fn next_packet(&mut self) -> Option<Packet> {
        next_packet(
            |stream| stream.index() == self.stream_idx,
            is_video(self.video_stream_idx),
            // self.stream_idx,
            // self.video_stream_idx,
            &mut self.audio_recv,
            &mut self.video_send,
            &self.input_mutex,
        )
    }
}

impl Iterator for AudioManager {
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        // this is not strictly required since the audio stream will most likely be
        //  dropped by the video thread, stopping the audio thread. but just in case
        //  we'll end our iterator if we have to close
        if self.closer.should_close() {
            return None;
        }
        if let Ok(()) = self.flush_requests.try_recv() {
            self.decoder.flush();
        }

        // .samples() returns the number of samples *per channel*. so we need to multiply it by the number of channels
        if self.sample_idx >= (self.frame_resampled.samples() * self.channels() as usize) {
            // we're out of samples on our current frame! gotta decode a new frame
            // but before decoding a new frame, check if we have to switch tracks
            if let Some(new_track) = self.audio_track_changes.try_iter().last() {
                let mut input = self.input_mutex.lock().unwrap();
                let new_track_idx = input
                    .0
                    .streams()
                    .filter(is_audio)
                    .nth(new_track)
                    .unwrap()
                    .index();
                let old_track = self.stream_idx;
                if new_track_idx != old_track {
                    self.stream_idx = new_track_idx;

                    set_stream_discard(&mut input.0.stream_mut(old_track).unwrap(), AVDISCARD_ALL);
                    let mut new_track = input.0.stream_mut(new_track_idx).unwrap();
                    set_stream_discard(&mut new_track, AVDISCARD_NONE);

                    // create new decoder and resampler
                    self.decoder = new_audio_decoder(&new_track);
                    self.resampler = self.decoder.resampler(
                        Sample::F32(Type::Packed /* packed = interleaved (rodio only supports interleaved sample formats) */), self.decoder.channel_layout(), self.decoder.rate(),
                    ).unwrap_or_else(|e| todo!("could not create resampler: {e}"))
                }
            }
            loop {
                let packet = match self.next_packet() {
                    None => return Some(0.),
                    Some(v) => v,
                };

                if packet.stream() != self.stream_idx {
                    continue;
                }

                self.decoder.send_packet(&packet);
                if self.decoder.receive_frame(&mut self.frame).is_ok() {
                    break;
                }
            }

            // resample our newly decoded frame
            self.resampler
                .run(&self.frame, &mut self.frame_resampled)
                .unwrap();
            self.sample_idx = 0;
        }
        // .plane::<f32>(0) is nice because it returns the floats instead of bytes, but
        //      we can't use it because the array it returns bases its size off of the
        //      number of samples per channel, which is wrong since our array contains
        //      the samples in *all* channels
        //          that is, until https://github.com/zmwangx/rust-ffmpeg/pull/104 is merged
        let data = &self.frame_resampled.data(0)[(self.sample_idx * size_of::<f32>())..][..4];
        let value = f32::from_ne_bytes(data.try_into().unwrap_or_else(|_| {
            unreachable!(
                "could not convert [u8] sample slice to [u8; 4] array (sample slice is: {data:?})"
            )
        }));
        self.sample_idx += 1;
        Some(value * *self.volume.get())
    }
}

impl Source for AudioManager {
    fn current_frame_len(&self) -> Option<usize> {
        self.frame_resampled.samples().checked_sub(self.sample_idx)
    }

    fn channels(&self) -> u16 {
        self.decoder.channels()
    }

    fn sample_rate(&self) -> u32 {
        self.decoder.rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        None
    }
}
