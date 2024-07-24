extern crate ffmpeg_next as ffmpeg;

use std::fmt::{Debug, Formatter};
use std::future::Future;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, Barrier, mpsc, OnceLock};
use std::thread;
use std::time::Duration;

use eframe::{CreationContext, egui, Frame, NativeOptions, storage_dir};
use eframe::egui::{CentralPanel, Widget};
use eframe::egui::load::TextureLoader;
use egui::{
    Align, Color32, Context, Id, Label, Layout,
    ProgressBar, RichText, ScrollArea, TopBottomPanel,
    ViewportBuilder, Visuals, WidgetText,
};
use egui::epaint::mutex::RwLock;
use egui::panel::TopBottomSide;
use replace_with::replace_with;
use serde::{Deserialize, Serialize};

use crate::editor::{Editor, EditorExit};
use crate::select::{SelectScreen, SelectScreenOut};
use crate::util::{plural, Updatable};

mod editor;
pub mod export;
pub mod player;
mod select;
mod util;
mod youtube;

#[cfg(feature = "hyper")]
pub type HttpsConnector = hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>;

#[cfg(feature = "hyper")]
pub type HttpsClient = hyper_util::client::legacy::Client<
    HttpsConnector,
    http_body_util::Full<std::io::Cursor<Vec<u8>>>
>;

#[cfg(feature = "hyper")]
pub fn https_client() -> &'static HttpsClient {
    static CLIENT: OnceLock<HttpsClient> = OnceLock::new();

    CLIENT.get_or_init(|| {
        use hyper_util::client::legacy::Client;
        use hyper_util::rt::TokioExecutor;

        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_native_roots()
            .unwrap()
            .https_only()
            .enable_http1()
            .build();

        let client = Client::builder(TokioExecutor::new())
            .http2_only(false)
            .build(https);

        client
    })
}

#[derive(Default)]
#[cfg(any(
    feature = "youtube"
))]
pub struct Auth {
    pub ctx: Context,
    #[cfg(feature = "youtube")]
    pub youtube: Option<youtube::YtAuth>,
}

#[cfg(any(
    feature = "youtube"
))]
pub type AuthArc = Arc<RwLock<Auth>>;
#[cfg(not(any(
    feature = "youtube"
)))]
pub type AuthArc = ();

impl Debug for Auth {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Auth {{ .. }}")
    }
}

fn storage() -> PathBuf {
    dbg!(storage_dir(APP_ID).unwrap())
}

impl Auth {
    pub fn ctx(&self) -> &Context {
        &self.ctx
    }
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
    // todo: remove this line
    storage();

    #[cfg(feature = "hyper")]
    {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }

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
    #[cfg(feature = "async")]
    {
        barrier.wait();
        drop(barrier);
    }

    ffmpeg::init().expect("could not initialize ffmpeg");

    eframe::run_native(
        "TouchUp",
        NativeOptions {
            viewport: ViewportBuilder::default().with_app_id(APP_ID.to_owned())
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

    #[cfg(feature = "youtube")]
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
            YouTubeLogin(_) => "youtubeLoginScreen"
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
}

impl TouchUp {
    fn select_screen(ctx: &Context) -> SelectScreen {
        SelectScreen::new(
            ctx.clone(),
        )
    }

    pub fn new(cc: &CreationContext) -> Self {
        let auth = AuthArc::new(RwLock::new(Auth {
            ctx: cc.egui_ctx.clone(),
            #[cfg(feature = "youtube")]
            youtube: None,
        }));
        let (task_cmd_sender, task_commands) = mpsc::channel();
        let mut screen = Screen::Select(Self::select_screen(&cc.egui_ctx));
        #[cfg(feature = "youtube")]
        if youtube::yt_token_file().exists() {
            // if the token file exists, attempt to log into youtube right now
            screen = Screen::YouTubeLogin(
                youtube::YtAuthScreen::new(cc.egui_ctx.clone(), AuthArc::clone(&auth))
            )
        }

        TouchUp {
            screen,
            tasks: (0, vec![]),
            task_commands,
            task_cmd_sender,
            auth,
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
                    text = text.color(Color32::from_rgb(222, 53, 53))
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
        // let mut task_render_start = ctx.input(|i| i.viewport().inner_rect.map(|r| r.right_top()).unwrap_or_default());

        draw_tasks(ctx, &mut self.tasks.1, &mut self.task_commands);

        CentralPanel::default().show(ctx, |ui| {
            ScrollArea::vertical().show(ui, |ui| {
                ui.push_id(self.screen.id(), |ui| {
                    match &mut self.screen {
                        Screen::Select(select) => {
                            match select.draw(ui, frame, &self.auth) {
                                SelectScreenOut::Stay => {}
                                SelectScreenOut::Edit(to_open) => {
                                    match Editor::new(ctx, to_open, frame) {
                                        None => {}
                                        Some(screen) => {
                                            self.screen = Screen::Edit(screen);
                                        }
                                    }
                                    ctx.request_repaint();
                                }
                                SelectScreenOut::YtLogin => {
                                    self.screen = Screen::YouTubeLogin(youtube::YtAuthScreen::new(ui.ctx().clone(), self.auth.clone()))
                                }
                            }
                        }
                        Screen::Edit(player) => {
                            match player.draw(&mut self.tasks, &self.auth, ui, &self.task_cmd_sender)
                            {
                                None => {}
                                Some(EditorExit::ToSelectScreen) => {
                                    self.screen = Screen::Select(Self::select_screen(&ctx))
                                }
                                Some(EditorExit::ToYoutubeScreen { init }) => {
                                    self.screen = Screen::YouTube(init);
                                }
                            }
                        }
                        #[cfg(feature = "youtube")]
                        Screen::YouTube(yt) => {
                            if yt.draw(ui, &mut self.tasks.1)
                            {
                                self.screen = Screen::Select(Self::select_screen(&ctx))
                            }
                        }
                        #[cfg(feature = "youtube")]
                        Screen::YouTubeLogin(_) => {
                            replace_with(&mut self.screen, || {
                                Screen::Select(Self::select_screen(&ctx))
                            }, |s| {
                                let login = match s {
                                    Screen::YouTubeLogin(v) => v,
                                    _ => unreachable!()
                                };
                                login.draw(ui, &self.auth).map(Screen::YouTubeLogin)
                                    .unwrap_or_else(|| Screen::Select(Self::select_screen(&ctx)))
                            })
                        }
                    };
                });
            });
        });
    }

    fn clear_color(&self, _visuals: &Visuals) -> [f32; 4] {
        Color32::BLUE.to_normalized_gamma_f32()
    }

    fn persist_egui_memory(&self) -> bool {
        true
    }
}