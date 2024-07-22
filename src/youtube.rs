#![cfg(feature = "youtube")]

use std::{fs, mem};
use std::future::Future;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, mpsc};

use eframe::emath::{Align, Vec2};
use egui::{Button, Color32, Context, CursorIcon, Id, Image, ImageSource, include_image, Label, Layout, OpenUrl, RichText, Sense, TextBuffer, TextEdit, TextStyle, Ui, Widget};
use egui::util::IdTypeMap;
use google_youtube3::api::Scope;
use google_youtube3::client::serde_with::serde_derive::{Deserialize, Serialize};
use tokio::sync::oneshot;

use crate::{AuthArc, spawn_async, storage, Task, YtHub};
use crate::util::{AnyExt, Updatable};

pub enum YtInfo {
    Continue {
        title: String,
        description: String,
        visibility: YtVisibility,
    },
    Cancel,
}

pub struct YtScreen {
    pub title: String,
    pub description: String,
    // visibility is handled through global memory

    // if None, the value has already been sent
    pub send_final: Option<tokio::sync::oneshot::Sender<YtInfo>>,
    pub task_id: u32,
    pub next_frame_request_focus: bool,
}

impl YtScreen {
    pub fn draw(&mut self, ui: &mut Ui, tasks: &mut Vec<Task>) -> bool {
        let mut finalize = false;
        let mut cancelled = false;

        let mut visibility: YtVisibility = ui
            .ctx()
            .data_mut(|d| d.get_persisted(Id::new("ytInfoVis")))
            .unwrap_or_default();
        ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
            let height = 20.;
            let text_styles = &mut ui.style_mut().text_styles;
            text_styles.get_mut(&TextStyle::Button).unwrap().size = height;
            text_styles.get_mut(&TextStyle::Body).unwrap().size = height;

            ui.menu_button(visibility.display_str(), |ui| {
                let menu_height = 18.;

                ui.style_mut()
                    .text_styles
                    .get_mut(&TextStyle::Button)
                    .unwrap()
                    .size = menu_height;
                for vis in YtVisibility::ALL {
                    if Button::image_and_text(
                        Image::new(vis.icon()).fit_to_exact_size(Vec2::splat(menu_height)),
                        vis.display_str(),
                    )
                        .selected(*vis == visibility)
                        .ui(ui)
                        .on_hover_text(vis.description())
                        .clicked()
                    {
                        visibility = *vis;
                        ui.ctx()
                            .data_mut(|d| d.insert_persisted(Id::new("ytInfoVis"), visibility));
                        ui.close_menu();
                    }
                }
            })
                .response
                .rect
                .height();
            // todo: maybe show the visibility icon even when not in the dropdown menu?
            // Image::new(visibility.icon())
            //     .fit_to_exact_size(Vec2::splat(real_height))
            //     .ui(ui);

            TextEdit::singleline(&mut self.title)
                .min_size(Vec2::new(ui.available_width(), 0.))
                .hint_text("Video title")
                .ui(ui);
        });
        ui.add_space(8.);
        ui.with_layout(Layout::bottom_up(Align::Max), |ui| {
            ui.with_layout(Layout::right_to_left(Align::Max), |ui| {
                if Button::image_and_text(include_image!("../embedded/checkmark.svg"), "Finish")
                    .ui(ui)
                    .on_hover_text(
                        "Finish editing the video details and upload the video once it's done",
                    )
                    .clicked()
                {
                    finalize = true;
                }
                if Button::new("Cancel").ui(ui).clicked() {
                    cancelled = true;
                    let _ = self.send_final.take().unwrap().send(YtInfo::Cancel);
                }
            });
            ui.add_space(8.);
            TextEdit::multiline(&mut self.description)
                .min_size(ui.available_size())
                .hint_text("Video description")
                .ui(ui);

            let task = tasks.iter_mut().find(|t| t.id == self.task_id);

            if task.is_none() {
                cancelled = true;
            }

            if finalize && !cancelled {
                if let Some(task) = task {
                    task.name.clone_from(&self.title);
                }

                let _ = self.send_final.take().unwrap().send(YtInfo::Continue {
                    title: mem::take(&mut self.title),
                    description: mem::take(&mut self.description),
                    visibility,
                });
            }
        });
        // whether to go back to the select menu: only if we've finalized or we've cancelled
        finalize | cancelled
    }
}

#[derive(Copy, Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum YtVisibility {
    Public,
    #[default]
    Unlisted,
    Private,
}

impl YtVisibility {
    pub const ALL: &'static [YtVisibility] = &[Self::Public, Self::Unlisted, Self::Private];

    pub const fn api_str(self) -> &'static str {
        use YtVisibility::*;
        match self {
            Public => "public",
            Unlisted => "unlisted",
            Private => "private",
        }
    }

    pub const fn display_str(self) -> &'static str {
        use YtVisibility::*;
        match self {
            Public => "Public",
            Unlisted => "Unlisted",
            Private => "Private",
        }
    }

    pub const fn description(self) -> &'static str {
        use YtVisibility::*;
        match self {
            Public => "Available for anyone to see",
            Unlisted => "Only available to those with a link",
            Private => "Only available to the uploader",
        }
    }

    pub fn icon(self) -> ImageSource<'static> {
        match self {
            YtVisibility::Public => include_image!("../embedded/yt_public.svg"),
            YtVisibility::Unlisted => include_image!("../embedded/yt_unlisted.svg"),
            YtVisibility::Private => include_image!("../embedded/yt_private.svg"),
        }
    }
}

#[derive(Clone)]
struct LoginUrl(Arc<str>);

pub(super) fn is_trying_login(data: &IdTypeMap) -> bool {
    data.get_temp::<LoginUrl>(Id::NULL).is_some()
}

pub(crate) async fn yt_auth(
    ctx: &Context,
    send_url: mpsc::Sender<String>,
    cancel: oneshot::Receiver<()>,
    keep_login: bool,
) -> Result<YtHub, ()> {
    use google_youtube3::oauth2::authenticator_delegate::InstalledFlowDelegate;
    use google_youtube3::oauth2::{
        ConsoleApplicationSecret, InstalledFlowAuthenticator, InstalledFlowReturnMethod,
    };
    use google_youtube3::YouTube;
    use google_youtube3::{hyper, hyper_rustls};

    let secret: ConsoleApplicationSecret =
        serde_json::from_slice(include_bytes!("../embedded/yt_cred.json")).unwrap();

    let http_c = hyper_rustls::HttpsConnectorBuilder::new()
        .with_native_roots()
        .unwrap()
        .https_or_http()
        .enable_http1()
        .build();
    let http = hyper::client::Client::builder().build(http_c);

    struct CodePresenter {
        ctx: Context,
        send_url: mpsc::Sender<String>,
    }

    impl InstalledFlowDelegate for CodePresenter {
        fn present_user_url<'a>(
            &'a self,
            url: &'a str,
            need_code: bool,
        ) -> Pin<Box<dyn Future<Output=Result<String, String>> + Send + 'a>> {
            assert!(!need_code);

            Box::pin(async move {
                let _ = self.send_url.send(url.to_string());
                self.ctx.output_mut(|o| {
                    o.open_url = Some(OpenUrl::new_tab(url));
                });
                self.ctx.request_repaint();
                Ok(String::new())
            })
        }
    }

    let ctx2 = ctx.clone();
    let out = tokio::select! {
        _ = cancel => {
            Err(())
        }
        v = async {
            let mut auth = InstalledFlowAuthenticator::builder(
                secret.installed.unwrap(),
                InstalledFlowReturnMethod::HTTPRedirect,
            )
                .maybe_apply(keep_login, |builder| builder.persist_tokens_to_disk(yt_token_file()))
                .flow_delegate(Box::new(CodePresenter { ctx: ctx2, send_url }))
                .build().await.unwrap();

            // try to cache the token for the upload scope
            let _ = auth.token(&[Scope::Upload]).await;
            Ok(YouTube::new(http, auth))
        } => v
    };
    ctx.data_mut(|d| d.remove_by_type::<LoginUrl>());
    out
}

pub fn yt_token_file() -> PathBuf {
    let mut p = storage();
    p.push("yt_tokencache.json");
    p
}

pub fn delete_token_file() {
    match fs::remove_file(yt_token_file()) {
        Ok(_) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => panic!("could not delete youtube token file: {e}"),
    }
}

pub struct YtAuthScreen {
    // if an empty string that means we haven't received anything yet
    url: Updatable<String>,
    cancel: oneshot::Sender<()>,
    pressed_try_again: bool,
}

impl YtAuthScreen {
    pub fn new(ctx: Context, auth: AuthArc) -> Self {
        let (url_send, url_recv) = Updatable::new(String::new());
        let (cancel_send, cancel_recv) = oneshot::channel();
        let keep_login = ctx
            .data_mut(|d| d.get_persisted(Id::new("ytKeepLogin")))
            .unwrap_or(true);
        spawn_async(async move {
            match yt_auth(&ctx, url_send, cancel_recv, keep_login).await {
                Ok(v) => {
                    auth.write()
                        .youtube = Some(v);
                }
                Err(_) => {}
            }
        });
        YtAuthScreen {
            url: url_recv,
            cancel: cancel_send,
            pressed_try_again: false,
        }
    }

    pub fn draw(mut self, ui: &mut Ui, auth: &AuthArc) -> Option<Self> {
        let auth_r = auth.read();
        if auth_r.youtube.is_some() {
            return None;
        }
        ui.heading("Please log into YouTube");
        let url = self.url.get();
        let url_available = !url.is_empty();
        ui.with_layout(Layout::left_to_right(Align::Min), |ui| {
            if url_available {
                if ui.button("Try again").clicked() {
                    self.pressed_try_again = true;
                    ui.output_mut(|o| {
                        o.open_url = Some(
                            OpenUrl::new_tab(&url)
                        )
                    })
                }
                if self.pressed_try_again {
                    if Label::new(RichText::new("Click to copy the login URL").color(Color32::WHITE))
                        .sense(Sense::hover())
                        .ui(ui)
                        .on_hover_cursor(CursorIcon::PointingHand)
                        .clicked() {
                        ui.output_mut(|o| {
                            o.copied_text = url.to_string()
                        })
                    }
                }
            } else {
                ui.spinner();
                ui.label("Please wait while we set this up...");
            }
        });
        if ui.button("Cancel").clicked() {
            self.cancel.send(()).unwrap();
            return None;
        }
        Some(self)
    }
}
