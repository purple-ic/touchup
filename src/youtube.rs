#![cfg(feature = "youtube")]

use std::{fs, io, iter, mem};
use std::borrow::Cow;
use std::error::Error;
use std::fmt::Write;
use std::fs::File;
use std::future::Future;
use std::io::{Cursor, ErrorKind, Seek, SeekFrom};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, mpsc};
use std::thread::Scope;

use eframe::emath::{Align, Vec2};
use egui::{
    Button, Color32, Context, CursorIcon, Id, Image, ImageSource, include_image, Label, Layout,
    OpenUrl, RichText, Sense, TextBuffer, TextEdit, TextStyle, Ui, Widget,
};
use egui::util::IdTypeMap;
use futures::Stream;
use futures_util::StreamExt;
use log::debug;
use reqwest::{Body, multipart};
use reqwest::header::{HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::AsyncSeekExt;
use tokio::sync::oneshot;
use tokio::task::spawn_blocking;
use tokio_util::bytes::{BufMut, BytesMut};
use tokio_util::codec::{BytesCodec, FramedRead};
use tokio_util::io::ReaderStream;
use yup_oauth2::{ConsoleApplicationSecret, InstalledFlowAuthenticator, InstalledFlowReturnMethod};
use yup_oauth2::authenticator::{Authenticator, DefaultHyperClient, HyperClientBuilder};
use yup_oauth2::authenticator_delegate::InstalledFlowDelegate;
use yup_oauth2::hyper_rustls::HttpsConnector;

use crate::{AuthArc, header_map, https_client, infallible_unreachable, MessageManager, spawn_async, storage, Task};
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

pub struct YtCtx {
    pub auth: Authenticator<<DefaultHyperClient as HyperClientBuilder>::Connector>,
    pub upload_url: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct YtVInfo {
    upload_url: String,
    outdated: bool,
}

pub const UPLOAD_SCOPE: &str = "https://www.googleapis.com/auth/youtube.upload";

pub enum YtAuthErr {
    Outdated,
}

pub async fn yt_auth(
    ctx: &Context,
    send_url: mpsc::Sender<String>,
    keep_login: bool,
) -> Result<YtCtx, YtAuthErr> {
    let secret: ConsoleApplicationSecret =
        serde_json::from_slice(include_bytes!("../embedded/yt_cred.json")).unwrap();

    let client = https_client();
    let v_info = client
        .get("https://api.github.com/repos/purple-ic/touchup/contents/data/youtube/vinfo1.json")
        .header("User-Agent", "TouchUp")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("Accept", "application/vnd.github.raw+json")
        .send()
        .await
        .unwrap()
        .json::<YtVInfo>()
        .await
        .unwrap();
    if v_info.outdated {
        spawn_blocking(|| {
            yt_delete_token_file();
        });
        return Err(YtAuthErr::Outdated);
    }

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
    let mut auth = InstalledFlowAuthenticator::builder(
        secret.installed.unwrap(),
        InstalledFlowReturnMethod::HTTPRedirect,
    )
        .maybe_apply(keep_login, |builder| {
            builder.persist_tokens_to_disk(yt_token_file())
        })
        .flow_delegate(Box::new(CodePresenter {
            ctx: ctx2,
            send_url,
        }))
        .build()
        .await
        .unwrap();

    // try to cache the token for the upload scope
    let _ = auth.token(&[UPLOAD_SCOPE]).await;

    ctx.data_mut(|d| d.remove_by_type::<LoginUrl>());
    // todo: allow updating the upload_url with an api
    Ok(YtCtx {
        auth,
        upload_url: v_info.upload_url,
    })
}

pub fn yt_token_file() -> PathBuf {
    let mut p = storage();
    p.push("yt_tokencache.json");
    p
}

// note: for logging out, use yt_log_out
//      yt_log_out also revokes the token
pub fn yt_delete_token_file() {
    match fs::remove_file(yt_token_file()) {
        Ok(_) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => panic!("could not delete youtube token file: {e}"),
    }
}

pub fn yt_log_out(yt: &YtCtx) {
    // auth is reference-counted
    let auth = yt.auth.clone();

    // revoke the access token
    spawn_async(async move {
        let t = auth.token(&[UPLOAD_SCOPE]).await;
        let token = match t.as_ref().map(|t| t.token()) {
            Ok(Some(token)) => token,
            Ok(None) => unreachable!("tokens should always be required for the upload scope"),
            Err(_) => todo!("handle get_token errors"),
        };

        https_client()
            .post("https://oauth2.googleapis.com/revoke")
            .query(&[("token", token)])
            .send()
            .await
            .unwrap();
    });

    yt_delete_token_file();
}

pub struct YtAuthScreen {
    // if an empty string that means we haven't received anything yet
    url: Updatable<String>,
    cancel: oneshot::Sender<()>,
    pressed_try_again: bool,
}

impl YtAuthScreen {
    pub fn new(ctx: Context, auth: AuthArc, msg: MessageManager) -> Self {
        let (url_send, url_recv) = Updatable::new(String::new());
        let (cancel_send, cancel_recv) = oneshot::channel();
        let keep_login = ctx
            .data_mut(|d| d.get_persisted(Id::new("ytKeepLogin")))
            .unwrap_or(true);
        spawn_async(async move {
            let auth_future = yt_auth(&ctx, url_send, keep_login);
            tokio::select! {
                v = auth_future => {
                    match v {
                        Ok(v) => {
                            auth.write().youtube = Some(v);
                        }
                        Err(YtAuthErr::Outdated) => {
                            msg.show_waiting("TouchUp must be updated to use YouTube-related features.\nThe YouTube API has made breaking changes and the current version of TouchUp can no longer interact with it.").await;
                        }
                    }
                }
                // we cancel even if the receiver was dropped
                _ = cancel_recv => {

                }
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
        } else if self.cancel.is_closed() {
            return None;
        }
        ui.heading("Please log into YouTube");
        let url = self.url.get();
        let url_available = !url.is_empty();
        ui.with_layout(Layout::left_to_right(Align::Min), |ui| {
            if url_available {
                if ui.button("Try again").clicked() {
                    self.pressed_try_again = true;
                    ui.output_mut(|o| o.open_url = Some(OpenUrl::new_tab(&url)))
                }
                if self.pressed_try_again {
                    if Label::new(
                        RichText::new("Click to copy the login URL").color(Color32::WHITE),
                    )
                        .sense(Sense::hover())
                        .ui(ui)
                        .on_hover_cursor(CursorIcon::PointingHand)
                        .clicked()
                    {
                        ui.output_mut(|o| o.copied_text = url.to_string())
                    }
                }
            } else {
                ui.spinner();
                ui.label("Setting up...");
            }
        });
        if ui.button("Cancel").clicked() {
            let _ = self.cancel.send(());
            return None;
        }
        Some(self)
    }
}

pub async fn yt_upload(
    client: &reqwest::Client,
    file_name: String,
    mut file: tokio::fs::File,
    ctx: &YtCtx,
    title: &str,
    description: &str,
    vis: YtVisibility,
) {
    // todo: track upload progress
    let file_len = file.seek(SeekFrom::End(0)).await.unwrap();

    // these docs are for google drive but the same multipart method applies to the youtube api
    // https://developers.google.com/drive/api/guides/manage-uploads#multipart

    let body_len = dbg!(file_len) as usize /* the vec will 100% exceed the file_len capacity, but starting at file_len is already good as it'll prevent a LOT of reallocations anyway */;

    let token = ctx.auth.token(&[UPLOAD_SCOPE]).await.unwrap();
    let token = token
        .token()
        .expect("successful token requests with upload scope must always return access tokens");

    let snippet = json!({
        "snippet": {
            "title": title,
            "description": description,
        },
        "status": {
            "privacyStatus": vis.api_str(),
        }
    });
    let mut attempts = 0;

    loop {
        debug!("starting yt upload...");
        let metadata = serde_json::to_vec(&snippet).unwrap();
        file.seek(SeekFrom::Start(0)).await.unwrap();

        let f = file.try_clone().await.unwrap();
        let form = multipart::Form::new()
            .percent_encode_noop()
            .part(
                "metadata",
                multipart::Part::bytes(metadata).headers(header_map! {
                    "content-type": "application/json; charset=UTF-8"
                }),
            )
            .part(
                "video",
                multipart::Part::stream(f)
                    .file_name(file_name.clone())
                    .headers(header_map! {
                        "content-type": "video/*",
                        "content-length": HeaderValue::from(file_len)
                    }),
            );

        // "https://www.googleapis.com/upload/youtube/v3/videos"
        let out = client
            .post(&ctx.upload_url)
            .multipart(form)
            .bearer_auth(token)
            .query(&[("uploadType", "multipart"), ("part", "status,snippet")])
            .send()
            .await
            .unwrap();

        let status = out.status();

        if status.is_success() {
            break;
        } else if (status.is_server_error() || status.is_client_error()) && attempts < 5 {
            debug!("retrying request ({status})");
            attempts += 1;
            file.seek(SeekFrom::Start(0)).await.unwrap();
            continue; // retry for error codes 5xx and 4xx
        } else {
            let body = out.json::<Value>().await.ok();
            if let Some(body) = body {
                panic!("could not upload youtube video: {status} -> {:?}", body)
            } else {
                panic!("could not upload youtube video: {status}");
            }
        }
    }
}
