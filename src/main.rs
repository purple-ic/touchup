#![feature(error_generic_member_access)]

extern crate ffmpeg_next as ffmpeg;

use std::error::Error;
use std::fmt::Debug;
use std::future::Future;
use std::path::PathBuf;
use std::sync::mpsc::{SendError, TrySendError};
use std::sync::{mpsc, Arc, Barrier, Mutex, OnceLock};
use std::thread;

use eframe::egui::CentralPanel;
use eframe::{egui, storage_dir, CreationContext, Frame, NativeOptions};
use egui::epaint::mutex::RwLock;
use egui::{
    Color32, Context, KeyboardShortcut, Modifiers, ScrollArea, ViewportBuilder, ViewportId, Visuals,
};
use log::info;
use player::tex::{attempt_tex_update, CurrentTex, PlayerTexture, TextureArc};
use puffin::{profile_function, profile_scope_custom};
use replace_with::replace_with;
use rfd::{MessageButtons, MessageLevel};

use crate::editor::{Editor, EditorExit};
use crate::logging::init_logging;
use crate::select::{SelectScreen, SelectScreenOut};
use crate::task::{draw_tasks, Task, TaskCommand};
use crate::util::{report_err, CheapClone};

mod editor;
pub mod export;
mod logging;
pub mod player;
mod select;
pub mod task;
mod util;
mod youtube;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(feature = "reqwest")]
pub fn https_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .use_rustls_tls()
            .build()
            .unwrap_or_else(|e| todo!("handle reqwest errors (got: {e})"))
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
    storage_dir(APP_ID).unwrap_or_else(|| todo!())
}

const APP_ID: &str = "touch_up";

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
    init_logging();
    info!("TouchUp version {VERSION}");

    #[cfg(feature = "async")]
    let barrier = Arc::new(Barrier::new(2));
    #[cfg(feature = "async")]
    let (async_stop, async_should_stop) = tokio::sync::oneshot::channel();

    #[cfg(feature = "async")]
    {
        use tokio::runtime;
        use tokio::runtime::Handle;

        let barrier = Arc::clone(&barrier);

        thread::Builder::new()
            .name("async".into())
            .spawn(move || {
                let runtime = runtime::Builder::new_current_thread()
                    .enable_io()
                    .enable_time()
                    .build()
                    .unwrap_or_else(|e| todo!("handle async errors (got: {e})\n note: we also have to somehow communicate this to the user correctly"));
                let guard = runtime.enter();
                ASYNC_HANDLE.set(Handle::current()).expect("ASYNC_HANDLE should only be set in the async thread");
                barrier.wait();
                drop(barrier);

                runtime.block_on(async { async_should_stop.await.unwrap() });
                drop(guard);
                info!("async shutting down")
            }).expect("could not spoawn async thread");
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
    .expect("could not start eframe");

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
    messages: mpsc::Receiver<String>,
}

impl TouchUp {
    fn select_screen(ctx: &Context) -> SelectScreen {
        SelectScreen::new(ctx.clone())
    }

    pub fn new(cc: &CreationContext) -> Self {
        let (msg_send, msg_recv) = mpsc::sync_channel(3);
        let message_manager = MessageManager {
            sender: msg_send,
            ctx: cc.egui_ctx.cheap_clone(),
        };

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
                msg,
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

const PROFILER_HOTKEY: KeyboardShortcut = KeyboardShortcut::new(Modifiers::CTRL, egui::Key::P);

impl eframe::App for TouchUp {
    fn update(&mut self, ctx: &Context, frame: &mut Frame) {
        profile_function!();

        puffin::GlobalProfiler::lock().new_frame();

        let p = profile_scope_custom!("touchup_update");

        if ctx.input_mut(|i| i.consume_shortcut(&PROFILER_HOTKEY)) {
            puffin::set_scopes_on(true);
        }

        attempt_tex_update(&mut self.current_tex, &self.texture, frame);
        draw_tasks(ctx, &mut self.tasks.1, &mut self.task_commands);

        CentralPanel::default().show(ctx, |ui| {
            ScrollArea::vertical().show(ui, |ui| {
                ui.push_id(self.screen.id(), |ui| {
                    match &mut self.screen {
                        Screen::Select(select) => match select.draw(ui, frame, &self.auth) {
                            SelectScreenOut::Stay => {}
                            SelectScreenOut::Edit(to_open) => {
                                match Editor::new(
                                    ctx,
                                    self.message_manager.cheap_clone(),
                                    to_open,
                                    frame,
                                    Arc::clone(&self.texture),
                                ) {
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
                                    self.message_manager.cheap_clone(),
                                ))
                            }
                        },
                        Screen::Edit(player) => {
                            match player.draw(
                                &mut self.tasks,
                                &self.auth,
                                ui,
                                &self.task_cmd_sender,
                                self.current_tex.as_ref().map(|CurrentTex { id, .. }| id),
                            ) {
                                None => {}
                                Some(EditorExit::ToSelectScreen) => {
                                    self.screen = Screen::Select(Self::select_screen(ctx))
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
                                self.screen = Screen::Select(Self::select_screen(ctx))
                            }
                        }
                        #[cfg(feature = "youtube")]
                        Screen::YouTubeLogin(_) => replace_with(
                            &mut self.screen,
                            || Screen::Select(Self::select_screen(ctx)),
                            |s| {
                                let login = match s {
                                    Screen::YouTubeLogin(v) => v,
                                    _ => unreachable!(),
                                };
                                login
                                    .draw(ui, &self.auth)
                                    .map(Screen::YouTubeLogin)
                                    .unwrap_or_else(|| Screen::Select(Self::select_screen(ctx)))
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

        drop(p);

        if puffin::are_scopes_on() {
            ctx.show_viewport_immediate(
                ViewportId::from_hash_of("puffin"),
                ViewportBuilder::default().with_title("Profiler (Ctrl+P)"),
                |ctx, _| CentralPanel::default().show(ctx, puffin_egui::profiler_ui),
            );
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

assert_cheap_clone!(Context, mpsc::SyncSender<String>);
impl CheapClone for MessageManager {}

#[derive(Clone, Debug)]
pub struct MessageBufFull {
    #[allow(dead_code)]
    attempted_message: String,
}

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
            }
            Err(TrySendError::Full(attempted_message)) => Err(MessageBufFull { attempted_message }),
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
        })
        .await
        .unwrap();
    }

    fn _handle_err<E: Error>(&self, stage: &str, err: E) {
        report_err(stage, &err);
        self.show_blocking(format!("An error occurred while {stage}:\n{err}"));
    }

    pub fn handle_err<E: Error, R>(
        &self,
        stage: &str,
        func: impl FnOnce(&Self) -> Result<R, E>,
    ) -> Option<R> {
        match func(self) {
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
    pub async fn handle_err_async<E: Error + Send + 'static, R>(
        &self,
        stage: impl AsRef<str> + Send + 'static,
        future: impl Future<Output = Result<R, E>>,
    ) -> Option<R> {
        match future.await {
            Ok(v) => Some(v),
            Err(err) => {
                let self2 = self.cheap_clone();
                tokio::task::spawn_blocking(move || {
                    let stage = stage;
                    self2._handle_err(stage.as_ref(), err);
                })
                .await
                .expect("_handle_err should not panic");
                None
            }
        }
    }

    #[cfg(feature = "async")]
    pub fn handle_err_spawn<E: Error + Send + 'static, R: Send + 'static>(
        &self,
        stage: impl AsRef<str> + Send + 'static,
        future: impl Future<Output = Result<R, E>> + Send + 'static,
    ) -> tokio::task::JoinHandle<Option<R>> {
        let msg = self.cheap_clone();
        spawn_async(async move { msg.handle_err_async(stage, future).await })
    }
}
