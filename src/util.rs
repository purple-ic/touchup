use std::{iter, mem, thread};
use std::cmp::Ordering;
use std::ffi::c_int;
use std::sync::{Arc, mpsc, Once};
use std::sync::mpsc::{Receiver, Sender, SyncSender, TryRecvError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use eframe::glow::{INVALID_ENUM, INVALID_FRAMEBUFFER_OPERATION, INVALID_OPERATION, INVALID_VALUE, NO_ERROR, OUT_OF_MEMORY, STACK_OVERFLOW, STACK_UNDERFLOW};
use ffmpeg::ffi::{AV_PKT_FLAG_DISCARD, av_seek_frame, AVSEEK_FLAG_BACKWARD};
use ffmpeg::format::context::Input;
use ffmpeg::frame::Video;
use ffmpeg::packet::Mut;
use ffmpeg::Rational;
use ffmpeg::sys::av_rescale_q;
use ffmpeg_next::codec::decoder::video::Video as VideoDecoder;

pub trait RationalExt {
    fn value_f64(self) -> f64;
    fn value_f32(self) -> f32;
}

impl RationalExt for Rational {
    fn value_f64(self) -> f64 {
        self.numerator() as f64 / self.denominator() as f64
    }

    fn value_f32(self) -> f32 {
        self.numerator() as f32 / self.denominator() as f32
    }
}

pub struct Updatable<T> {
    current: T,
    receive: Receiver<T>,
}

impl<T> Updatable<T> {
    pub fn new_sync(initial: T, bound: usize) -> (SyncSender<T>, Self) {
        let (send, recv) = mpsc::sync_channel(bound);
        (send, Self::wrap(initial, recv))
    }

    pub fn new(initial: T) -> (Sender<T>, Self) {
        let (send, recv) = mpsc::channel();
        (send, Self::wrap(initial, recv))
    }

    pub fn wrap(initial: T, recv: Receiver<T>) -> Self {
        Self {
            current: initial,
            receive: recv,
        }
    }

    pub fn is_closed(&mut self) -> bool {
        match self.receive.try_recv() {
            Ok(value) => {
                self.current = value;
                self.is_closed()
            }
            Err(TryRecvError::Disconnected) => true,
            Err(TryRecvError::Empty) => false
        }
    }

    pub fn get(&mut self) -> &mut T {
        if let Some(new_val) = self.receive.try_iter().last() {
            self.current = new_val;
            &mut self.current
        } else {
            &mut self.current
        }
    }

    /// Returns the last-received value.
    ///
    /// This will always return the same value until [Self::get]
    /// is called.
    ///
    /// This can be useful if you've already called [Self::get]
    /// and don't want to have to keep a mutable reference and
    /// don't care about getting any new updated values.
    pub fn get_now(&self) -> &T {
        &self.current
    }

    pub fn replace_current(&mut self, value: T) -> T {
        mem::replace(&mut self.current, value)
    }
}

pub struct FrameCounter {
    frames: Vec<Instant>,
}

impl FrameCounter {
    pub fn new() -> FrameCounter {
        Self {
            frames: Vec::with_capacity(60)
        }
    }

    /// Counts the current frame and returns the current FPS (frames per second).
    ///
    /// Note that this FPS is not a mean average and is instead the literal
    /// amount of frames that happened in the last second. This means that as
    /// far as the first frame knows, the current FPS is 1 even though in reality
    /// they're just yet to happen.
    ///
    /// This may be confusing to users so this should only be used for developer-facing
    /// frame measuring.
    pub fn frame(&mut self) -> usize {
        let now = Instant::now();

        self.frames.push(now);
        let to_rem = self.frames.iter().take_while(|&&frame_time| (now - frame_time) > Duration::from_secs(1)).count();

        self.frames.splice(..to_rem, iter::empty());
        self.frames.len()
    }
}

pub fn measure_time<R>(func: impl FnOnce() -> R) -> (R, Duration) {
    let start_time = Instant::now();
    let ret = func();
    (ret, start_time.elapsed())
}

#[macro_export]
macro_rules! print_time {
    ($v:expr) => {{
        let (val, time) = $crate::util::measure_time(|| {
            $v
        });
        println!("{} => {:?}", stringify!($v), time);
        val
    }};
}

pub trait BoolExt {
    fn toggle(&mut self);
}

impl BoolExt for bool {
    fn toggle(&mut self) {
        *self = !*self;
    }
}

// since duration's cant be negative, abs(a - b) isn't possible
pub fn time_diff(a: Duration, b: Duration) -> Duration {
    if a > b {
        a - b
    } else {
        b - a
    }
}

pub struct OwnedThreads {
    closer: Arc<Once>,
}

impl Drop for OwnedThreads {
    fn drop(&mut self) {
        self.closer.call_once(|| ());
    }
}

#[must_use]
#[derive(Clone)]
pub struct CloseReceiver {
    closed: Arc<Once>,
}

impl CloseReceiver {
    #[must_use]
    pub fn should_close(&self) -> bool {
        self.closed.is_completed()
    }

    pub fn cheap_clone(&self) -> Self {
        self.clone()
    }
}

pub fn spawn_owned_thread<T: Send + 'static>(name: String, func: impl Send + 'static + FnOnce(CloseReceiver) -> T) -> std::io::Result<(JoinHandle<T>, OwnedThreads)> {
    let closer = Arc::new(Once::new());
    let closed = Arc::clone(&closer);

    thread::Builder::new()
        .name(name)
        .spawn(|| {
            func(CloseReceiver {
                closed
            })
        }).map(|handle| (handle, OwnedThreads { closer }))
}

pub trait OptionExt<T> {
    #[allow(clippy::wrong_self_convention)] // we're following is_some_and's convention
    fn is_none_or(self, f: impl FnOnce(T) -> bool) -> bool;
}

impl<T> OptionExt<T> for Option<T> {
    fn is_none_or(self, f: impl FnOnce(T) -> bool) -> bool {
        match self {
            None => true,
            Some(v) => f(v)
        }
    }
}

pub fn precise_seek(input: &mut Input, decoder: &mut VideoDecoder, frame: &mut Video, stream_idx: usize, target_ts: i64) {
    unsafe {
        let result = av_seek_frame(
            input.as_mut_ptr(),
            // -1,
            stream_idx as c_int,
            target_ts,
            AVSEEK_FLAG_BACKWARD,
        );
        if result < 0 {
            panic!("ffmpeg err when seeking: {result}")
        }
    }

    decoder.flush();

    // println!("starting seek");

    loop {
        let (packet_stream, mut packet) = input.packets().next().unwrap_or_else(|| todo!());
        let is_target_pkt = packet.pts().unwrap_or_else(|| todo!()) >= target_ts;

        if packet_stream.index() != stream_idx {
            continue;
        }

        if !is_target_pkt {
            // set AV_PKT_FLAG_DISCARD on the packet so the decoder knows to
            // use it only to construct the next non-I frames but not actually
            // output it (which saves it quite a bit of work)
            unsafe {
                // ffmpeg-next doesn't have proper bindings for the AV_PKT_FLAG_DISCARD packet flag, only the AV_PKT_FLAG_KEY and AV_PKT_FLAG_CORRUPT flags
                // so we have to get the raw AVPacket and change the flags unsafely
                let packet = &mut *packet.as_mut_ptr();
                packet.flags |= AV_PKT_FLAG_DISCARD
            }
        }

        decoder.send_packet(&packet);
        if is_target_pkt {
            // if this packet is our target, then try to read it and output the frame
            //  if the frame is not yet available, we'll just continue this loop
            if decoder.receive_frame(frame).is_ok() {
                break;
            }
        } else {
            // if this packet is not our target, then exhaust the decoder's frames
            //  in case it outputted something
            while decoder.receive_frame(frame).is_ok() {}
        }
    }
}

pub fn rescale(ts: i64, from: Rational, to: Rational) -> i64 {
    unsafe {
        av_rescale_q(ts, from.into(), to.into())
    }
}

pub fn f32_cmp(a: f32, b: f32) -> Ordering {
    if a < b {
        Ordering::Less
    } else if a == b {
        Ordering::Equal
    } else {
        Ordering::Greater
    }
}

pub fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

pub trait AnyExt {
    fn maybe_apply(self, should_apply: bool, func: impl FnOnce(Self) -> Self) -> Self
    where
        Self: Sized,
    {
        if should_apply {
            func(self)
        } else {
            self
        }
    }
}

impl<T> AnyExt for T {}

#[cfg(feature = "async")]
pub struct AsyncCanceler {
    send: Option<tokio::sync::oneshot::Sender<()>>,
}

#[cfg(feature = "async")]
impl AsyncCanceler {
    pub fn new(send_cancel: tokio::sync::oneshot::Sender<()>) -> Self {
        Self {
            send: Some(send_cancel)
        }
    }
}

#[cfg(feature = "async")]
impl Drop for AsyncCanceler {
    fn drop(&mut self) {
        match self.send.take() {
            None => {
                if cfg!(debug_assertions) {
                    panic!("AsyncCanceler has `send = None`. has it been dropped twice?")
                }
            }
            Some(send) => {
                let _ = send.send(());
            }
        }
    }
}