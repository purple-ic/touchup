#![feature(error_generic_member_access)]

extern crate ffmpeg_next as ffmpeg;

use std::backtrace::{Backtrace, BacktraceStatus};
use std::borrow::Cow;
use std::error::{Error, request_ref};
use std::fmt::{Debug, Display, Formatter};
use std::future::Future;
use std::io::Stdout;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, Barrier, mpsc, Mutex, OnceLock};
use std::sync::mpsc::{SendError, TrySendError};
use std::thread;
use std::time::Duration;

use eframe::{CreationContext, egui, egui_glow, Frame, glow, NativeOptions, storage_dir};
use eframe::egui::{CentralPanel, Widget};
use eframe::egui::load::TextureLoader;
use eframe::egui_glow::{CallbackFn, check_for_gl_error};
use eframe::epaint::PaintCallbackInfo;
use eframe::glow::{HasContext, INVALID_ENUM, PixelUnpackData, TEXTURE_2D};
use egui::{Align, Color32, Context, Id, ImageData, Label, Layout, ProgressBar, Rect, RichText, ScrollArea, TextureId, TopBottomPanel, ViewportBuilder, Visuals, WidgetText};
use egui::epaint::mutex::RwLock;
use egui::epaint::TextureManager;
use egui::panel::TopBottomSide;
use log::{error, info, LevelFilter};
use replace_with::replace_with;
use rfd::{MessageButtons, MessageLevel};
use serde::{Deserialize, Serialize};
use simple_logger::SimpleLogger;

use crate::editor::{Editor, EditorExit};
use crate::select::{SelectScreen, SelectScreenOut};
use crate::util::{CheapClone, plural, Updatable};

mod editor;
pub mod export;
pub mod player;
mod select;
mod util;
mod youtube;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(feature = "reqwest")]
pub fn https_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .use_rustls_tls()
            .build().unwrap()
    })
}

#[derive(Default)]
#[cfg(any(feature = "youtube"))]
pub struct Auth {
    pub ctx: Context,
    #[cfg(feature = "youtube")]
    pub youtube: Option<youtube::YtCtx>,
}

#[cfg(any(feature = "youtube"))]
pub type AuthArc = Arc<RwLock<Auth>>;
#[cfg(not(any(feature = "youtube")))]
pub type AuthArc = ();

fn storage() -> PathBuf {
    dbg!(storage_dir(APP_ID).unwrap())
}

const APP_ID: &'static str = "touch_up";

#[cfg(feature = "async")]
static ASYNC_HANDLE: OnceLock<tokio::runtime::Handle> = OnceLock::new();

#[cfg(feature = "async")]
pub fn spawn_async<F>(task: F) -> tokio::task::JoinHandle<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    let handle = ASYNC_HANDLE.get().unwrap_or_else(|| {
        panic!("attempted to create async task but async hasn't been initialized")
    });
    let guard = handle.enter();
    let handle = tokio::spawn(task);
    drop(guard);
    handle
}

fn main() {
    SimpleLogger::new()
        .with_colors(true)
        .with_level(
            if cfg!(debug_assertions) {
                LevelFilter::Debug
            } else {
                LevelFilter::Warn
            }
        )
        .init()
        .unwrap();
    info!("TouchUp version {VERSION}");

    // todo: remove this line
    storage();

    #[cfg(feature = "async")]
    let barrier = Arc::new(Barrier::new(2));
    #[cfg(feature = "async")]
    let (async_stop, async_should_stop) = tokio::sync::oneshot::channel();

    #[cfg(feature = "async")]
    {
        use tokio::runtime;
        use tokio::runtime::Handle;

        let barrier = Arc::clone(&barrier);

        thread::spawn(move || {
            let runtime = runtime::Builder::new_current_thread()
                .enable_io()
                .enable_time()
                .thread_name("async")
                .build()
                .unwrap();
            let guard = runtime.enter();
            ASYNC_HANDLE.set(Handle::current()).unwrap();
            barrier.wait();
            drop(barrier);

            runtime.block_on(async { async_should_stop.await.unwrap() });
            drop(guard);
            println!("async shutting down")
        });
    }
    ffmpeg::init().expect("could not initialize ffmpeg");
    #[cfg(feature = "async")]
    {
        barrier.wait();
        drop(barrier);
    }

    eframe::run_native(
        "TouchUp",
        NativeOptions {
            viewport: ViewportBuilder::default()
                .with_app_id(APP_ID.to_owned())
                // todo: disable drag & drop when not needed
                .with_drag_and_drop(true),
            ..NativeOptions::default()
        },
        Box::new(|cc| {
            egui_extras::install_image_loaders(&cc.egui_ctx);
            Ok(Box::new(TouchUp::new(cc)))
        }),
    )
        .unwrap();

    #[cfg(feature = "async")]
    let _ = async_stop.send(());
}

enum Screen {
    Select(SelectScreen),
    Edit(Editor),
    #[cfg(feature = "youtube")]
    YouTube(youtube::YtScreen),
    #[cfg(feature = "youtube")]
    YouTubeLogin(youtube::YtAuthScreen),
}

impl Screen {
    fn id(&self) -> &'static str {
        use Screen::*;
        match self {
            Select(_) => "selectScreen",
            Edit(_) => "editScreen",
            #[cfg(feature = "youtube")]
            YouTube(_) => "youtubeScreen",
            #[cfg(feature = "youtube")]
            YouTubeLogin(_) => "youtubeLoginScreen",
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub enum TaskStage {
    Export {
        has_follow_up: bool,
    },
    #[cfg(feature = "youtube")]
    YtAwaitingInfo,
    #[cfg(feature = "youtube")]
    YtUpload,
}

impl TaskStage {
    pub fn name(&self) -> &'static str {
        match self {
            TaskStage::Export { .. } => "Exporting",
            #[cfg(feature = "youtube")]
            TaskStage::YtAwaitingInfo => "Please fill out the video's details",
            #[cfg(feature = "youtube")]
            TaskStage::YtUpload => "Uploading to YouTube",
        }
    }

    pub fn is_final(&self) -> bool {
        match self {
            TaskStage::Export { has_follow_up } => !has_follow_up,
            #[cfg(feature = "youtube")]
            TaskStage::YtAwaitingInfo => false,
            #[cfg(feature = "youtube")]
            TaskStage::YtUpload => true,
        }
    }

    pub fn custom_color(&self) -> Option<Color32> {
        match self {
            TaskStage::Export { .. } => None,
            #[cfg(feature = "youtube")]
            TaskStage::YtAwaitingInfo | TaskStage::YtUpload => Some(Color32::RED),
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub struct TaskStatus {
    stage: TaskStage,
    // - usually works as a normalized (0 to 1) scale of progress
    // - when is +Infinity, represents a task that is currently being worked on but the progress can't be tracked
    // - when is -Infinity, represents a task that can't be worked on and is waiting for user input
    progress: f32,
}

impl TaskStatus {
    pub fn is_finished(&self) -> bool {
        self.stage.is_final() && self.progress >= 1. && self.progress.is_finite()
    }
}

pub struct Task {
    status: Updatable<TaskStatus>,
    id: u32,
    name: String,
    remove_requested: bool,
    #[cfg(feature = "async")]
    async_stopper: Option<util::AsyncCanceler>,
}

#[derive(Debug, Clone)]
pub enum TaskCommand {
    Rename { id: u32, new_name: String },
    Cancel { id: u32 },
}

pub struct PlayerTexture {
    /// \[width, height] in pixels
    pub size: [usize; 2],
    /// in rgb24 bytes
    pub bytes: Vec<u8>,
    pub changed: bool,
}

struct TouchUp {
    screen: Screen,
    // the tasks should be sorted by their IDs
    //      note that the first task in the vector may not have an ID of 0
    //      and there may also be holes in the task IDs.
    //      -- all of this means we can perform a binary search if necessary
    //      -- though it probably won't be
    tasks: (u32, Vec<Task>),
    task_commands: mpsc::Receiver<TaskCommand>,
    // used for passing the sender to new threads
    task_cmd_sender: mpsc::Sender<TaskCommand>,

    auth: AuthArc,

    current_tex: Option<CurrentTex>,
    texture: TextureArc,

    message_manager: MessageManager,
    messages: mpsc::Receiver<String>
}

struct CurrentTex {
    id: TextureId,
    native: glow::Texture,
    size: [usize; 2],
}

pub type TextureArc = Arc<Mutex<PlayerTexture>>;

fn set_player_texture_settings(gl: &glow::Context) {
    unsafe {
        gl.tex_parameter_i32(
            TEXTURE_2D,
            glow::TEXTURE_MAG_FILTER,
            glow::LINEAR as i32,
        );
        gl.tex_parameter_i32(
            TEXTURE_2D,
            glow::TEXTURE_MIN_FILTER,
            glow::LINEAR as i32,
        );

        gl.tex_parameter_i32(
            TEXTURE_2D,
            glow::TEXTURE_WRAP_S,
            glow::CLAMP_TO_EDGE as i32,
        );
        gl.tex_parameter_i32(
            TEXTURE_2D,
            glow::TEXTURE_WRAP_T,
            glow::CLAMP_TO_EDGE as i32,
        );
    }
    check_for_gl_error!(gl, "trying to set tex params");
}

impl TouchUp {
    fn init_texture(texture: glow::Texture, gl: &glow::Context, size: [usize; 2], data: &[u8]) {
        unsafe {
            gl.get_error();
            gl.bind_texture(TEXTURE_2D, Some(texture));
            set_player_texture_settings(gl);
            // using RGB instead of SRGB makes the images.. way brighter?
            //  kind of interesting that it makes them brighter instead of loosing quality
            //  even though the source format is still the same
            gl.tex_image_2d(TEXTURE_2D, 0, glow::SRGB8 as i32, size[0].try_into().unwrap(), size[1].try_into().unwrap(), 0, glow::RGB, glow::UNSIGNED_BYTE, Some(data));
            gl.bind_texture(TEXTURE_2D, None);
        }
    }

    fn create_texture(size: [usize; 2], data: &[u8], frame: &mut Frame) -> CurrentTex {
        let gl = frame.gl().unwrap();
        let texture = unsafe { gl.create_texture() }.unwrap();
        Self::init_texture(texture, gl, size, data);
        check_for_gl_error!(gl, "trying to initialize texture");
        let texture_id = frame.register_native_glow_texture(texture);
        CurrentTex {
            id: texture_id,
            native: texture,
            size,
        }
    }

    fn react_to_tex_update(&mut self, ctx: &Context, frame: &mut Frame) {
        // todo: this texture system leaves a frame or two where the last-played-frame of the previous video is visible
        if let Ok(mut tex) = self.texture.try_lock() {
            if tex.changed {
                tex.changed = false;

                if let Some(current_tex) = &mut self.current_tex {
                    if current_tex.size != tex.size {
                        println!("texture resizing");

                        current_tex.size = tex.size;
                        let gl = frame.gl().unwrap();
                        // tell gl to reallocate the texture with a new size
                        Self::init_texture(current_tex.native, gl, tex.size, &tex.bytes);
                        check_for_gl_error!(gl, "trying to reinitialize texture for new size");
                    } else {
                        // println!("updating texture");
                        let gl = frame.gl().unwrap();
                        unsafe {
                            gl.get_error();
                            gl.bind_texture(TEXTURE_2D, Some(current_tex.native));
                            set_player_texture_settings(gl);
                            gl.tex_sub_image_2d(
                                TEXTURE_2D,
                                0,
                                0,
                                0,
                                tex.size[0].try_into().unwrap(),
                                tex.size[1].try_into().unwrap(),
                                glow::RGB,
                                glow::UNSIGNED_BYTE,
                                PixelUnpackData::Slice(&tex.bytes),
                            );
                            gl.bind_texture(TEXTURE_2D, None);
                            check_for_gl_error!(&gl, "while attempting to update the player texture")
                        }
                    }
                } else {
                    println!("creating texture");
                    self.current_tex = Some(Self::create_texture(tex.size, &tex.bytes, frame));
                };
            }
        }
    }

    fn select_screen(ctx: &Context) -> SelectScreen {
        SelectScreen::new(ctx.clone())
    }

    pub fn new(cc: &CreationContext) -> Self {
        let (msg_send, msg_recv) = mpsc::sync_channel(3);
        let message_manager = MessageManager { sender: msg_send, ctx: cc.egui_ctx.cheap_clone() };

        #[cfg(any(feature = "youtube"))]
        let auth = AuthArc::new(RwLock::new(Auth {
            ctx: cc.egui_ctx.clone(),
            #[cfg(feature = "youtube")]
            youtube: None,
        }));
        #[cfg(not(any(feature = "youtube")))]
        let auth = ();
        let (task_cmd_sender, task_commands) = mpsc::channel();
        let mut screen = Screen::Select(Self::select_screen(&cc.egui_ctx));
        #[cfg(feature = "youtube")]
        if youtube::yt_token_file().exists() {
            let msg = message_manager.cheap_clone();
            // if the token file exists, attempt to log into youtube right now
            screen = Screen::YouTubeLogin(youtube::YtAuthScreen::new(
                cc.egui_ctx.clone(),
                AuthArc::clone(&auth),
                msg
            ))
        }

        TouchUp {
            screen,
            tasks: (0, vec![]),
            task_commands,
            task_cmd_sender,
            auth,
            current_tex: None,
            texture: Arc::new(Mutex::new(PlayerTexture {
                size: [0, 0],
                bytes: vec![],
                changed: false,
            })),
            message_manager,
            messages: msg_recv,
        }
    }
}

fn draw_tasks(
    ctx: &egui::Context,
    tasks: &mut Vec<Task>,
    task_commands: &mut mpsc::Receiver<TaskCommand>,
) {
    // we also consume the task commands here
    for cmd in task_commands.try_iter() {
        // we prefer just searching for tasks from the start (over binary search)
        //      as commands are almost always sent by the newest
        //      task
        macro_rules! find_task {
            ($id:expr) => {
                tasks.iter_mut().find(move |t| t.id == $id)
            };
        }

        match cmd {
            TaskCommand::Rename { id, new_name } => {
                match find_task!(id) {
                    None => continue,
                    Some(t) => t.name = new_name,
                };
            }
            TaskCommand::Cancel { id } => {
                match find_task!(id) {
                    None => continue,
                    Some(t) => {
                        // this is likely better than Vec::remove since
                        //    once we're done with task commands, we
                        //    immediately call Vec::retain_mut
                        t.remove_requested = true;
                    }
                }
            }
        }
    }

    tasks.retain_mut(|task| {
        let status = task.status.get();
        !status.is_finished() && !task.remove_requested
    });

    if !tasks.is_empty() {
        ctx.request_repaint_after(Duration::from_secs_f32(0.5))
    }

    TopBottomPanel::new(TopBottomSide::Bottom, "taskPanel").show_animated(
        ctx,
        !tasks.is_empty(),
        |ui| {
            if tasks.is_empty() {
                return;
            }
            ui.add_space(5.);

            let task = &tasks[0];
            let is_new_task = ctx.data_mut(|d| {
                let old = d.get_temp_mut_or::<u32>(Id::new("taskLastShown"), task.id);
                let is_new = *old != task.id;
                *old = task.id;
                is_new
            });

            // it's safe to get_now() since the retain_mut closure already called get()
            let status = *task.status.get_now();

            ui.with_layout(Layout::left_to_right(Align::Min), |ui| {
                if status.progress.is_finite() {
                    ui.label(WidgetText::from(status.stage.name()).color(Color32::WHITE));
                }
                ui.label(&*task.name);
            });
            let progress = if status.progress.is_finite() {
                ctx.animate_value_with_time(
                    Id::new("taskProgress"),
                    status.progress,
                    if is_new_task { 0.0 } else { 0.5 },
                )
            } else if status.progress.is_sign_positive() {
                // +Infinity
                1.
            } else {
                // -Infinity
                0.
            };

            let mut progress_bar = ProgressBar::new(progress)
                .desired_width(ui.available_width())
                .desired_height(20.)
                .rounding(5.);
            if status.progress.is_finite() {
                progress_bar = progress_bar.show_percentage()
            } else {
                let mut text = RichText::new(status.stage.name());
                if status.progress == f32::NEG_INFINITY {
                    text = text
                        .color(Color32::from_rgb(222, 53, 53))
                        .strong()
                        .size(16.)
                }
                progress_bar = progress_bar.text(text);
            }
            if status.progress == f32::NEG_INFINITY {
                progress_bar = progress_bar.fill(Color32::TRANSPARENT);
            } else if let Some(color) = status.stage.custom_color() {
                progress_bar = progress_bar.fill(color);
            }
            progress_bar.ui(ui);
            ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                Label::new(format!(
                    "{n} task{s}",
                    n = tasks.len(),
                    s = plural(tasks.len()),
                ))
                    .selectable(false)
                    .ui(ui);

                ui.with_layout(Layout::left_to_right(Align::Min), |ui| {
                    if ui.button("Cancel").clicked() {
                        tasks[0].remove_requested = true;
                    }
                })
            });
        },
    );
}

impl eframe::App for TouchUp {
    fn update(&mut self, ctx: &Context, frame: &mut Frame) {
        self.react_to_tex_update(ctx, frame);
        draw_tasks(ctx, &mut self.tasks.1, &mut self.task_commands);

        CentralPanel::default().show(ctx, |ui| {
            ScrollArea::vertical().show(ui, |ui| {
                ui.push_id(self.screen.id(), |ui| {
                    match &mut self.screen {
                        Screen::Select(select) => match select.draw(ui, frame, &self.auth) {
                            SelectScreenOut::Stay => {}
                            SelectScreenOut::Edit(to_open) => {
                                match Editor::new(ctx, self.message_manager.cheap_clone(), to_open, frame, Arc::clone(&self.texture)) {
                                    None => {}
                                    Some(screen) => {
                                        self.screen = Screen::Edit(screen);
                                    }
                                }
                                ctx.request_repaint();
                            }
                            #[cfg(feature = "youtube")]
                            SelectScreenOut::YtLogin => {
                                self.screen = Screen::YouTubeLogin(youtube::YtAuthScreen::new(
                                    ui.ctx().clone(),
                                    self.auth.clone(),
                                    self.message_manager.cheap_clone()
                                ))
                            }
                        },
                        Screen::Edit(player) => {
                            match player.draw(
                                &mut self.tasks,
                                &self.auth,
                                ui,
                                &self.task_cmd_sender,
                                self.current_tex.as_ref()
                                    .map(|CurrentTex { id, .. }| id),
                            ) {
                                None => {}
                                Some(EditorExit::ToSelectScreen) => {
                                    self.screen = Screen::Select(Self::select_screen(&ctx))
                                }
                                #[cfg(feature = "youtube")]
                                Some(EditorExit::ToYoutubeScreen { init }) => {
                                    self.screen = Screen::YouTube(init);
                                }
                            }
                        }
                        #[cfg(feature = "youtube")]
                        Screen::YouTube(yt) => {
                            if yt.draw(ui, &mut self.tasks.1) {
                                self.screen = Screen::Select(Self::select_screen(&ctx))
                            }
                        }
                        #[cfg(feature = "youtube")]
                        Screen::YouTubeLogin(_) => replace_with(
                            &mut self.screen,
                            || Screen::Select(Self::select_screen(&ctx)),
                            |s| {
                                let login = match s {
                                    Screen::YouTubeLogin(v) => v,
                                    _ => unreachable!(),
                                };
                                login
                                    .draw(ui, &self.auth)
                                    .map(Screen::YouTubeLogin)
                                    .unwrap_or_else(|| Screen::Select(Self::select_screen(&ctx)))
                            },
                        ),
                    };
                });
            });
        });

        for message in self.messages.try_iter() {
            rfd::MessageDialog::new()
                .set_title("TouchUp error")
                .set_level(MessageLevel::Error)
                .set_description(message)
                .set_buttons(MessageButtons::Ok)
                .show();
        }
    }

    fn clear_color(&self, _visuals: &Visuals) -> [f32; 4] {
        Color32::BLUE.to_normalized_gamma_f32()
    }

    fn persist_egui_memory(&self) -> bool {
        true
    }
}

#[derive(Clone)]
pub struct MessageManager {
    sender: mpsc::SyncSender<String>,
    ctx: Context,
}

#[derive(Clone, Debug)]
pub struct MessageBufFull {
    attempted_message: String,
}

impl CheapClone for MessageManager {}

impl MessageManager {
    pub fn show_blocking(&self, message: impl Into<String>) {
        match self.sender.send(message.into()) {
            Ok(()) => {
                self.ctx.request_repaint();
            }
            Err(SendError(_)) => {
                panic!("message sender disconnected. looks like the UI thread is done!")
            }
        }
    }

    pub fn show(&self, message: impl Into<String>) -> Result<(), MessageBufFull> {
        match self.sender.try_send(message.into()) {
            Ok(()) => {
                self.ctx.request_repaint();
                Ok(())
            },
            Err(TrySendError::Full(attempted_message)) => Err(MessageBufFull {
                attempted_message
            }),
            Err(TrySendError::Disconnected(_)) => {
                panic!("message sender disconnected. looks like the UI thread is done!")
            }
        }
    }

    #[cfg(feature = "async")]
    pub async fn show_async(&self, message: impl Into<String>) {
        let self2 = self.cheap_clone();
        let msg = message.into();
        tokio::task::spawn_blocking(move || {
            self2.show_blocking(msg);
        }).await.unwrap();
    }

    fn _handle_err<E: Error>(&self, stage: &str, err: E) {
        let backtrace = request_ref::<Backtrace>(&err)
            .filter(|b| matches!(b.status(), BacktraceStatus::Captured));
        if let Some(backtrace) = backtrace {
            error!("error at stage `{stage}`: {err}\n{}", backtrace)
        } else {
            error!("(no backtrace) error at stage `{stage}`: {err}")
        }
        self.show_blocking(
            format!("An error occurred during {stage}:\n{err}")
        );
    }

    pub fn handle_err<E: Error, R>(&self, stage: &str, func: impl FnOnce() -> Result<R, E>) -> Option<R> {
        match func() {
            Ok(v) => Some(v),
            Err(err) => {
                self._handle_err(stage, err);
                None
            }
        }
    }

    // pub async fn handle_blocking_err_async<E : Error + 'static, R : Send + 'static>(&self, stage: &'static str, func: impl 'static + Send + FnOnce() -> Result<R, E>) -> Option<R> {
    //     let self2 = self.cheap_clone();
    //     spawn_blocking(move || {
    //         self2.handle_err(stage, func)
    //     }).await.expect("_handle_err should not panic")
    // }

    #[cfg(feature = "async")]
    pub async fn handle_err_async<E: Error + Send + 'static, R>(&self, stage: impl AsRef<str> + Send + 'static, future: impl Future<Output=Result<R, E>>) -> Option<R> {
        match future.await {
            Ok(v) => Some(v),
            Err(err) => {
                let self2 = self.cheap_clone();
                tokio::task::spawn_blocking(move || {
                    let stage = stage;
                    self2._handle_err(stage.as_ref(), err);
                }).await.expect("_handle_err should not panic");
                None
            }
        }
        // let value = future.await;
        // let self2 = self.cheap_clone();
        // spawn_blocking(move || {
        //     self2._handle_err(stage, value)
        // }).await.expect("_handle_err should not panic")
    }

    #[cfg(feature = "async")]
    pub fn handle_err_spawn<E: Error + Send + 'static, R: Send + 'static>(&self, stage: impl AsRef<str> + Send + 'static, future: impl Future<Output=Result<R, E>> + Send + 'static) -> tokio::task::JoinHandle<Option<R>> {
        let msg = self.cheap_clone();
        spawn_async(async move {
            msg.handle_err_async(stage, future).await
        })
    }
}