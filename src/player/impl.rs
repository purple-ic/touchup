use std::backtrace::Backtrace;
use std::cmp::min;
use std::convert::identity;
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
use log::{debug, trace};
use rodio::{OutputStream, Sink, Source};
use thiserror::Error;

use VideoTime::PausedAt;

use crate::{AuthArc, MessageManager, PlayerTexture, TextureArc};
use crate::task::{ TaskCommand, TaskStatus};
use crate::export::{export, ExportFollowUp};
use crate::player::r#impl::PlayerCommand::UpdatePreview;
use crate::player::r#impl::VideoTime::Anchored;
use crate::util::{CloseReceiver, OwnedThreads, precise_seek, RationalExt, result2flow, spawn_owned_thread, time_diff, Updatable};
use crate::util::CheapClone;

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
        audio_track: Option<usize>,
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
    change_audio_track: Sender<Option<usize>>,
    audio_tracks: usize,
    preview_mode: bool,
    change_volume: Sender<f32>,
}

#[derive(Error, Debug)]
pub enum PlayerError {
    #[error("Received an error from FFmpeg: {0}")]
    FFmpeg(#[from] ffmpeg_next::Error, Backtrace),
    #[error("The provided video file has no video stream")]
    NoVideoStream,
    #[error("Could not initialize audio output: {0}")]
    // no backtrace since there's only one place this can originate
    RodioStream(#[from] rodio::StreamError),
    #[error("Could not begin playing audio: {0}")]
    // no backtrace since there's only one place this can originate
    RodioPlay(#[from] rodio::PlayError)
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

fn new_audio_decoder(audio_stream: &Stream) -> Result<AudioDecoder, PlayerError> {
    CodecContext::from_parameters(audio_stream.parameters())?
        .decoder()
        .audio()
        .map_err(Into::into)
}

impl Player {
    pub fn is_closed(&self) -> bool {
        let ref_c = self.threads.ref_count();
        debug_assert!(ref_c <= 2, "reference count (={ref_c}) should not be >2");
        ref_c < 2
    }

    pub fn change_volume(&self, volume: f32) {
        let _ = self.change_volume.send(volume);
    }

    pub fn begin_export(
        &self,
        trim: (f32, f32),
        audio_track: Option<usize>,
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
            let _ = self.commands
                .send(PlayerCommand::EnablePreviewMode);
        }
    }

    pub fn set_audio_track(&self, idx: Option<usize>) {
        let _ = self.change_audio_track.send(idx);
    }

    pub fn nb_audio_tracks(&self) -> usize {
        self.audio_tracks
    }

    pub fn seek_to(&mut self, time: Duration) {
        self.preview_mode = false;
        self.time.replace_current(PausedAt(time));
        let _ = self.commands.send(PlayerCommand::SeekTo(time));
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

    pub fn new(gui_ctx: Context, msg: MessageManager, input: Input, input_path: PathBuf, starting_volume: f32, texture: TextureArc) -> Option<Self> {
        let (info_send, info_recv) = mpsc::channel();
        let (commands_send, commands_recv) = mpsc::sync_channel(20);
        let (time_updater, time) = Updatable::new(PausedAt(Duration::ZERO));
        let (change_audio_track, audio_track_changes) = mpsc::channel();
        let (change_volume, volume) = Updatable::new(starting_volume);

        let thread_owner = spawn_owned_thread("video".into(), move |closer| {
            msg.handle_err("playing or setting up video", |msg| {
                Self::setup(
                    gui_ctx,
                    msg.cheap_clone(),
                    input,
                    input_path,
                    info_send,
                    commands_recv,
                    time_updater,
                    closer,
                    audio_track_changes,
                    volume,
                    texture
                )
            });
        })
            .unwrap()
            .1;
        let PlayerStartupInfo {
            resolution,
            frame_time,
            duration,
            audio_tracks,
        } = info_recv.recv().ok()?;

        Some(Self {
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
        })
    }

    fn setup(
        gui_ctx: Context,
        msg: MessageManager,
        mut input: Input,
        input_path: PathBuf,
        info_send: mpsc::Sender<PlayerStartupInfo>,
        commands_recv: Receiver<PlayerCommand>,
        time_updater: Sender<VideoTime>,
        closer: CloseReceiver,
        audio_track_changes: Receiver<Option<usize>>,
        volume: Updatable<f32>,
        texture: TextureArc
    ) -> Result<(), PlayerError> {
        let audio_stream_idx = input.streams().find(is_audio).map(|stream| stream.index());
        let mut audio_tracks = if audio_stream_idx.is_some() {
            1
        } else {
            0
        };

        for mut stream in input
            .streams_mut()
            .filter(|stream| is_audio(&stream) && Some(stream.index()) != audio_stream_idx)
        {
            audio_tracks += 1;
            set_stream_discard(&mut stream, AVDISCARD_ALL)
        }

        let video_stream = input.streams().best(MediaType::Video).ok_or(PlayerError::NoVideoStream)?;
        let audio_stream = audio_stream_idx.map(|idx| input.stream(idx).expect("audio_stream_idx should point to the correct idx"));

        let video_stream_idx = video_stream.index();

        let ts_rate = video_stream.time_base().invert().value_f64();
        let duration = ts2dur(AV_TIME_BASE as f64, input.duration());

        let (video_send, video_recv) = mpsc::sync_channel(5);
        let (audio_send, audio_recv) = mpsc::sync_channel(5);

        let make_video_decoder = || {
            CodecContext::from_parameters(video_stream.parameters())?
                .decoder()
                .video()
                .map_err(PlayerError::from)
        };
        let video_decoder = make_video_decoder()?;

        let audio_decoder = match &audio_stream {
            None => None,
            Some(audio_stream) => {
                Some(new_audio_decoder(audio_stream)?)
            }
        };

        let resolution = [video_decoder.width(), video_decoder.height()];

        let scaler = video_decoder
            .converter(
                // convert to rgb24 pixel format and keep the same resolution
                Pixel::RGB24,
            )?;

        let resampler = match &audio_decoder {
            None => None,
            Some(audio_decoder) => {
                Some(audio_decoder.resampler(
                    Sample::F32(Type::Packed /* packed = interleaved (rodio only supports interleaved sample formats) */), audio_decoder.channel_layout(), audio_decoder.rate(),
                )?)
            }
        };

        let avg_frame_rate = video_stream.avg_frame_rate();
        let frame_time = Duration::from_secs_f64(avg_frame_rate.invert().value_f64());
        let input_mutex = Arc::new(Mutex::new((input, LastTs { stream: 0, ts: 0 })));

        // set up our audio output
        let (audio_output_stream, audio_output) = OutputStream::try_default()?;

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
            msg: msg.cheap_clone()
        };

        let sink = Sink::try_new(&audio_output)?;
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
            msg,
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

        video_manager.begin_loop()?;

        // make sure rust only drops our streams at the end
        //      otherwise rust attempts to make their lifetimes as
        //      short as it determines they have to be, but it doesn't
        //      account for the runtime ramifications of dropping our
        //      audio streams (that disconnects the stream and turns off the sound)
        drop((audio_output_stream, audio_output));
        debug!("ending video thread");
        Ok(())
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
                // println!("{}", packet_stream.index());
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
    msg: MessageManager,
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

    fn precise_seek(&mut self, pause: bool, target: Duration) -> Result<(), PlayerError> {
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
        )?;
        if pause {
            self.scaler.run(&self.frame, &mut self.frame_scaled)?;
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
        debug!("jumped {:?}", time_diff(new_frame_time, anchor_old));

        Ok(())
    }

    fn update_preview(&mut self, time: Duration) -> Result<(), PlayerError> {
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
            .run(&self.frame, &mut self.frame_scaled)?;
        let mut tex = self.texture.lock().unwrap();
        update_texture(&self.frame_scaled, &mut *tex);

        Ok(())
    }

    fn preview_mode(&mut self, was_paused: bool) -> Result<(), PlayerError> {
        self.audio_sink.pause();
        while let Ok(command) = self.commands_recv.recv() {
            // println!("processing command (during preview mode): {command:?}");
            match command {
                PlayerCommand::SeekTo(target) => {
                    self.precise_seek(was_paused, target)?;
                    if !was_paused {
                        self.audio_sink.play();
                    }
                    break;
                }
                PlayerCommand::UpdatePreview(target) => {
                    self.update_preview(target)?;
                },
                _ => {}
            }
        }

        Ok(())
    }

    fn export(
        &self,
        trim: (f32, f32),
        audio_track: Option<usize>,
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
        let msg = self.msg.cheap_clone();
        thread::Builder::new()
            .name(format!("taskExport{id}"))
            .spawn(move || {
                let mut input = input_mutex.lock().unwrap();
                let audio_stream_idx =
                    audio_track.map(|audio_track|
                    input
                        .0
                        .streams()
                        .filter(is_audio)
                        .nth(audio_track)
                        .expect("audio_track should point to an existing audio track but there aren't enough")
                        .index()
                    );
                msg.handle_err("exporting", move |msg| {
                    export(
                        ctx,
                        msg.cheap_clone(),
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
                });
            })
            .unwrap();
    }

    #[must_use]
    fn process_command(&mut self, paused: bool, command: PlayerCommand) -> ControlFlow<Option<PlayerError>> {
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
                result2flow(self.precise_seek(paused, self.time.now() + Duration::from_secs(15)).map_err(Some))?
            }
            PlayerCommand::Back15s => result2flow(self.precise_seek(
                paused,
                self.time.now().saturating_sub(Duration::from_secs(15)),
            ).map_err(Some))?,
            PlayerCommand::SeekTo(time) => result2flow(self.precise_seek(paused, time).map_err(Some))?,
            UpdatePreview(_) => {
                // ignore the preview update request since we're not in preview mode
            }
            PlayerCommand::EnablePreviewMode => {
                result2flow(self.preview_mode(paused).map_err(Some))?;
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
                return Break(None);
            }
        }
        Continue(())
    }

    #[must_use]
    fn process_commands(&mut self) -> ControlFlow<Option<PlayerError>> {
        while let Ok(command) = self.commands_recv.try_recv() {
            self.process_command(false, command)?;
        }
        Continue(())
    }

    fn begin_loop(&mut self) -> Result<(), PlayerError> {
        loop {
            if self.closer.should_close() {
                let _ = self.process_commands();
                return Ok(());
            }

            if let Break(reason) = self.process_commands() {
                return if let Some(err) = reason {
                    Err(err)
                } else {
                    Ok(())
                }
            }
            self.attempt_refresh()?;

            let next_frame_sec =
                Duration::from_secs_f64(ts2sec(self.ts_rate, self.frame.pts().unwrap_or(0)));
            let sec_now = self.time.now();

            thread::sleep(min(
                Duration::from_secs_f32(1. / 60.),
                next_frame_sec.saturating_sub(sec_now),
            ));
        }
    }

    fn attempt_refresh(&mut self) -> Result<(), PlayerError> {
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
                    if self.frame.pts().unwrap_or_else(|| todo!()) < self.ts_now() {
                        continue;
                    }
                    #[cfg(print_fps)]
                    {
                        trace!("{}fps", self.frame_counter.frame());
                    }
                    break;
                } else {
                    let packet = match self.next_packet() {
                        None => {
                            self.frame_scaled = VideoFrame::empty();
                            return Ok(());
                        }
                        Some(v) => v,
                    };
                    self.decoder.send_packet(&packet);
                }
            }
            // rescale our new frame
            self.scaler
                .run(&self.frame, &mut self.frame_scaled)?;
        }

        Ok(())
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
    stream_idx: Option<usize>,
    video_stream_idx: usize,

    input_mutex: Arc<Mutex<(Input, LastTs)>>,
    audio_recv: Receiver<Packet>,
    video_send: SyncSender<Packet>,
    flush_requests: Receiver<()>,
    closer: CloseReceiver,
    audio_track_changes: Receiver<Option<usize>>,

    frame: AudioFrame,
    frame_resampled: AudioFrame,
    sample_idx: usize,

    decoder: Option<AudioDecoder>,
    resampler: Option<Resampler>,

    volume: Updatable<f32>,
    msg: MessageManager
}

impl AudioManager {
    fn next_packet(&mut self) -> Option<Packet> {
        if let Some(stream_idx) = self.stream_idx {
            next_packet(
                |stream| stream.index() == stream_idx,
                is_video(self.video_stream_idx),
                // self.stream_idx,
                // self.video_stream_idx,
                &mut self.audio_recv,
                &mut self.video_send,
                &self.input_mutex,
            )
        } else {
            None
        }
    }

    fn next_sample(&mut self) -> Result<Option<f32>, PlayerError> {
        // .samples() returns the number of samples *per channel*. so we need to multiply it by the number of channels
        if self.sample_idx >= (self.frame_resampled.samples() * self.channels() as usize) {
            // we're out of samples on our current frame! gotta decode a new frame
            // but before decoding a new frame, check if we have to switch tracks
            if let Some(new_track) = self.audio_track_changes.try_iter().last() {
                let mut input = self.input_mutex.lock().unwrap();
                let new_track_idx = new_track.map(|new_track| input
                    .0
                    .streams()
                    .filter(is_audio)
                    .nth(new_track)
                    .expect("new_track should point to an existing audio track but there aren't enough")
                    .index());
                let old_track = self.stream_idx;
                if new_track_idx != old_track {
                    self.stream_idx = new_track_idx;

                    if let Some(old_track) = old_track {
                        set_stream_discard(&mut input.0.stream_mut(old_track).expect("old_track should point to an existing stream but there aren't enough"), AVDISCARD_ALL);
                    }
                    if let Some(new_track_idx) = new_track_idx {
                        let mut new_track = input.0.stream_mut(new_track_idx).unwrap();
                        set_stream_discard(&mut new_track, AVDISCARD_NONE);

                        // create new decoder and resampler
                        let decoder = new_audio_decoder(&new_track)?;
                        self.resampler = Some(decoder.resampler(
                            Sample::F32(Type::Packed /* packed = interleaved (rodio only supports interleaved sample formats) */), decoder.channel_layout(), decoder.rate(),
                        )?);
                        self.decoder = Some(decoder);
                    } else {
                        self.resampler = None;
                        self.decoder = None;
                    }
                }
            }
            if self.stream_idx.is_none() {
                return Ok(Some(0.));
            }
            loop {
                let packet = match self.next_packet() {
                    None => return Ok(Some(0.)),
                    Some(v) => v,
                };

                if Some(packet.stream()) != self.stream_idx {
                    continue;
                }

                let decoder = self.decoder.as_mut().unwrap();
                decoder.send_packet(&packet);
                if decoder.receive_frame(&mut self.frame).is_ok() {
                    break;
                }
            }

            // resample our newly decoded frame
            self.resampler
                .as_mut()
                .expect("since stream_idx is Some(_), resampler must also be Some(_)")
                .run(&self.frame, &mut self.frame_resampled)
                .unwrap();
            self.sample_idx = 0;
        } else if self.stream_idx.is_none() {
            return Ok(Some(0.))
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
        Ok(Some(value * *self.volume.get()))
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
            if let Some(decoder) = &mut self.decoder {
                decoder.flush();
            }
        }

        let msg = self.msg.cheap_clone();
        msg.handle_err("playing/decoding audio", |_| {
            self.next_sample()
        }).and_then(identity)
    }
}

impl Source for AudioManager {
    fn current_frame_len(&self) -> Option<usize> {
        self.frame_resampled.samples().checked_sub(self.sample_idx)
    }

    fn channels(&self) -> u16 {
        self.decoder.as_ref().map(|decoder| decoder.channels()).unwrap_or(1)
    }

    fn sample_rate(&self) -> u32 {
        self.decoder.as_ref().map(|decoder| decoder.rate()).unwrap_or(48000)
    }

    fn total_duration(&self) -> Option<Duration> {
        None
    }
}
