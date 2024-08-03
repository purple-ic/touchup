#![allow(dead_code)]

use std::backtrace::{Backtrace, BacktraceStatus};
use std::cmp::Ordering;
use std::error::{request_ref, Error};
use std::ffi::c_int;
use std::fmt::{Display, Formatter};
use std::ops::ControlFlow;
use std::sync::mpsc::{Receiver, Sender, SyncSender, TryRecvError};
use std::sync::{mpsc, Arc, Mutex, MutexGuard, Once};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use std::{fmt, iter, mem, thread};

use egui::{Context, FontFamily, FontId};
use ffmpeg::ffi::{av_seek_frame, AVSEEK_FLAG_BACKWARD, AV_PKT_FLAG_DISCARD};
use ffmpeg::format::context::Input;
use ffmpeg::frame::Video;
use ffmpeg::packet::Mut;
use ffmpeg::sys::av_rescale_q;
use ffmpeg::Rational;
use ffmpeg_next::codec::decoder::video::Video as VideoDecoder;
use log::error;

#[cfg(feature = "async")]
pub use async_util::*;

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
            Err(TryRecvError::Empty) => false,
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
            frames: Vec::with_capacity(60),
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
        let to_rem = self
            .frames
            .iter()
            .take_while(|&&frame_time| (now - frame_time) > Duration::from_secs(1))
            .count();

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
        let (val, time) = $crate::util::measure_time(|| $v);
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

impl OwnedThreads {
    pub fn ref_count(&self) -> usize {
        // we sub 1 because the count includes the current reference aswell
        Arc::strong_count(&self.closer).saturating_sub(1)
    }
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

pub fn spawn_owned_thread<T: Send + 'static>(
    name: String,
    func: impl Send + 'static + FnOnce(CloseReceiver) -> T,
) -> std::io::Result<(JoinHandle<T>, OwnedThreads)> {
    let closer = Arc::new(Once::new());
    let closed = Arc::clone(&closer);

    thread::Builder::new()
        .name(name)
        .spawn(|| func(CloseReceiver { closed }))
        .map(|handle| (handle, OwnedThreads { closer }))
}

pub trait OptionExt<T> {
    #[allow(clippy::wrong_self_convention)] // we're following is_some_and's convention
    fn is_none_or(self, f: impl FnOnce(T) -> bool) -> bool;
}

impl<T> OptionExt<T> for Option<T> {
    fn is_none_or(self, f: impl FnOnce(T) -> bool) -> bool {
        match self {
            None => true,
            Some(v) => f(v),
        }
    }
}

pub fn ffmpeg_err(result: c_int) -> Result<(), ffmpeg_next::Error> {
    if result < 0 {
        Err(ffmpeg_next::Error::from(result))
    } else {
        Ok(())
    }
}

pub fn precise_seek(
    input: &mut Input,
    decoder: &mut VideoDecoder,
    frame: &mut Video,
    stream_idx: usize,
    target_ts: i64,
) -> Result<(), ffmpeg_next::Error> {
    unsafe {
        let result = av_seek_frame(
            input.as_mut_ptr(),
            // -1,
            stream_idx as c_int,
            target_ts,
            AVSEEK_FLAG_BACKWARD,
        );
        ffmpeg_err(result)?;
    }

    decoder.flush();

    // println!("starting seek");

    loop {
        let (packet_stream, mut packet) = input.packets().next().unwrap_or_else(
            || todo!(), /* not sure what to even do here. we do sometimes hit this part! */
        );
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

        // todo: only allow "not yet available" errors and throw others
        let _ = decoder.send_packet(&packet);
        if is_target_pkt {
            // if this packet is our target, then try to read it and output the frame
            //  if the frame is not yet available, we'll just continue this loop
            if decoder.receive_frame(frame).is_ok() {
                break Ok(());
            }
        } else {
            // if this packet is not our target, then exhaust the decoder's frames
            //  in case it outputted something
            while decoder.receive_frame(frame).is_ok() {}
        }
    }
}

pub fn rescale(ts: i64, from: Rational, to: Rational) -> i64 {
    unsafe { av_rescale_q(ts, from.into(), to.into()) }
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
mod async_util {
    use std::future::Future;

    use tokio::sync::oneshot;

    #[cfg(feature = "async")]
    pub struct AsyncCanceler {
        send: Option<oneshot::Sender<()>>,
    }

    #[cfg(feature = "async")]
    impl AsyncCanceler {
        pub fn new(send_cancel: oneshot::Sender<()>) -> Self {
            Self {
                send: Some(send_cancel),
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

    enum PromiseInner<T> {
        Complete(T),
        Waiting(oneshot::Receiver<T>),
    }

    // todo: maybe we can just replace the inner value with Arc<OnceLock<T>>? or Arc<OnceCell<T>>? idk
    pub struct Promise<T> {
        inner: PromiseInner<T>,
    }

    impl<T> Promise<T> {
        pub fn complete(value: T) -> Self {
            Self {
                inner: PromiseInner::Complete(value),
            }
        }

        pub fn wrap(recv: oneshot::Receiver<T>) -> Self {
            Self {
                inner: PromiseInner::Waiting(recv),
            }
        }

        pub fn spawn(future: impl Future<Output = T> + 'static + Send) -> Self
        where
            T: Send + 'static,
        {
            let (send, recv) = oneshot::channel();
            crate::spawn_async(async move {
                let value = future.await;
                let _ = send.send(value);
            });
            Self::wrap(recv)
        }

        pub fn get(&mut self) -> Option<&mut T> {
            match &mut self.inner {
                PromiseInner::Complete(_) => {}
                PromiseInner::Waiting(recv) => match recv.try_recv() {
                    Ok(v) => {
                        self.inner = PromiseInner::Complete(v);
                    }
                    Err(_) => return None,
                },
            }
            match &mut self.inner {
                PromiseInner::Complete(v) => Some(v),
                PromiseInner::Waiting(_) => unreachable!(),
            }
        }

        pub fn get_now(&self) -> Option<&T> {
            match &self.inner {
                PromiseInner::Complete(v) => Some(v),
                PromiseInner::Waiting(_) => None,
            }
        }

        pub fn take_now(self) -> Result<T, Self> {
            match self.inner {
                PromiseInner::Complete(v) => Ok(v),
                inner @ PromiseInner::Waiting(_) => Err(Self { inner }),
            }
        }

        pub async fn await_value(&mut self) -> &mut T {
            if let PromiseInner::Waiting(w) = &mut self.inner {
                w.await.unwrap_or_else(|_| todo!());
            }
            match &mut self.inner {
                PromiseInner::Complete(v) => v,
                PromiseInner::Waiting(_) => unreachable!(),
            }
        }

        pub async fn take_await_value(self) -> T {
            match self.inner {
                PromiseInner::Complete(v) => v,
                PromiseInner::Waiting(recv) => recv.await.unwrap_or_else(|_| todo!()),
            }
        }
    }
}

#[macro_export]
macro_rules! infallible_unreachable {
    ($inf:expr) => {{
        // make sure $inf is actually Infallible
        let v: ::core::convert::Infallible = $inf;
        unreachable!("infallible cannot exist at runtime")
    }};
}

#[macro_export]
macro_rules! header_map {
    // if $value is a literal, then we expect it to be a string and just create a string header value
    // if $value is an expression and not *just* a literal, then we expect it to return its own HeaderValue
    { @INTERNAL@VALUE@ $value:literal } => {
        ::reqwest::header::HeaderValue::from_static($value)
    };
    { @INTERNAL@VALUE@ $value:expr } => {
        $value
    };
    { $($name:literal: $value:expr), *$(,)? } => {
        ::reqwest::header::HeaderMap::from_iter(
            [
                $((::reqwest::header::HeaderName::from_static($name), $crate::header_map!(@INTERNAL@VALUE@ $value))),*
            ]
        )
    };
}

// CheapClone cannot be impld for Copy types because std may add Copy for the types we manually impl CheapClone for
//  specialization would probably save us here were it stable...
pub trait CheapClone: Clone {
    #[must_use = "cloning does not usually have side effects"]
    fn cheap_clone(&self) -> Self {
        self.clone()
    }
}

macro_rules! cheap_clone_impl {
    ($(
        $(< $($param:ident),* >)?
        $struct:path
    ),* $(,)?) => {
        $(
        impl$(<$($param),*>)? CheapClone for $struct {}
        )*
    };
}

cheap_clone_impl!(
    <T> Arc<T>,
    <T> Sender<T>,
    <T> SyncSender<T>,
    Context,
    FontId,
    FontFamily
);

pub fn result2flow<T, E>(result: Result<T, E>) -> ControlFlow<E, T> {
    match result {
        Ok(v) => ControlFlow::Continue(v),
        Err(e) => ControlFlow::Break(e),
    }
}

pub fn lock_ignore_poison<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(v) => v,
        Err(p) => p.into_inner(),
    }
}

pub fn report_err(action: &str, err: &impl Error) {
    let backtrace =
        request_ref::<Backtrace>(err).filter(|b| matches!(b.status(), BacktraceStatus::Captured));
    if let Some(backtrace) = backtrace {
        error!("error while `{action}`: {err}\n{}", backtrace)
    } else {
        error!("(no backtrace) error while `{action}`: {err}")
    }
}

pub struct FnDisplay<F: Fn(&mut Formatter) -> fmt::Result>(pub F);

impl<F: Fn(&mut Formatter) -> fmt::Result> Display for FnDisplay<F> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        (self.0)(f)
    }
}
