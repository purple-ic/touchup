#![cfg(feature = "youtube")]

use std::backtrace::Backtrace;
use std::future::Future;
use std::io::SeekFrom;
use std::pin::Pin;
use std::sync::mpsc;
use std::{io, mem};

use async_trait::async_trait;
use eframe::emath::{Align, Vec2};
use eframe::epaint::text::TextWrapping;
use egui::text::{LayoutJob, LayoutSection};
use egui::{
    include_image, Button, Color32, Context, CursorIcon, FontId, Id, Image, ImageSource, Label,
    Layout, OpenUrl, RichText, Sense, TextEdit, TextFormat, TextStyle, Ui, Widget,
};
use keyring::Entry;
use log::{debug, error};
use puffin::profile_function;
use reqwest::header::HeaderValue;
use reqwest::multipart;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use thiserror::Error;
use tokio::io::AsyncSeekExt;
use tokio::sync::oneshot;
use tokio::task::spawn_blocking;
use yup_oauth2::authenticator::{Authenticator, DefaultHyperClient, HyperClientBuilder};
use yup_oauth2::authenticator_delegate::InstalledFlowDelegate;
use yup_oauth2::storage::{TokenInfo, TokenStorage};
use yup_oauth2::{ConsoleApplicationSecret, InstalledFlowAuthenticator, InstalledFlowReturnMethod};

use crate::task::Task;
use crate::util::{report_err, CheapClone, Updatable};
use crate::{header_map, https_client, spawn_async, AuthArc, MessageManager};

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
}

impl YtScreen {
    pub fn draw(&mut self, ui: &mut Ui, tasks: &mut Vec<Task>) -> bool {
        profile_function!();

        let mut finalize = false;
        let mut cancelled = false;

        // ui.with_layout(Layout::left_to_right(Align::Min), |ui| {
        //     {
        //         let style = ui.style_mut();
        //         style.spacing.item_spacing = Vec2::ZERO;
        //         style.wrap_mode = Some(TextWrapMode::Wrap)
        //     }

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
        ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
            ui.with_layout(Layout::right_to_left(Align::Max), |ui| {
                if Button::image_and_text(include_image!("../embedded/checkmark.svg"), "Upload")
                    .ui(ui)
                    .on_hover_text(
                        "Finish editing the video details and upload the video once it's done",
                    )
                    .clicked()
                {
                    finalize = true;
                }
                if Button::new("Cancel")
                    .ui(ui)
                    .on_hover_text("Cancel the upload\nThis will also cancel the export if it still hasn't completed")
                    .clicked() {
                    cancelled = true;
                    let _ = self.send_final.take().expect("YtScreen::draw should not be called after send_final is taken and becomes None").send(YtInfo::Cancel);
                }
            });
            ui.scope(tos_text);

            TextEdit::multiline(&mut self.description)
                .min_size(ui.available_size())
                .hint_text("Video description")
                .ui(ui);
            ui.add_space(8.);

            let task = tasks.iter_mut().find(|t| t.id == self.task_id);

            if task.is_none() {
                cancelled = true;
            }

            if finalize && !cancelled {
                if let Some(task) = task {
                    task.name.clone_from(&self.title);
                }

                let _ = self.send_final.take().expect("YtScreen::draw should not be called after send_final is taken and becomes None").send(YtInfo::Continue {
                    title: mem::take(&mut self.title),
                    description: mem::take(&mut self.description),
                    visibility,
                });
            } else if cancelled {
                if let Some(task) = task {
                    task.remove_requested = true;
                }
            }
        });
        // whether to go back to the select menu: only if we've finalized or we've cancelled
        finalize | cancelled
    }
}

fn tos_text(ui: &mut Ui) {
    {
        let style = ui.style_mut();
        style.visuals.hyperlink_color = Color32::TRANSPARENT;
    }

    macro_rules! prefix {
                () => {
                    "By clicking 'Upload', you certify that the content you are uploading complies with the YouTube Terms of Service (including the YouTube Community Guidelines) at "
                };
            }

    macro_rules! url {
        () => {
            "https://www.youtube.com/t/terms"
        };
    }

    macro_rules! suffix {
        () => {
            ". Please be sure not to violate others' copyright or privacy rights."
        };
    }
    const PREFIX: usize = prefix!().len();
    const URL: &str = url!();
    const SUFFIX: usize = suffix!().len();
    let font = FontId::proportional(10.);

    let text = LayoutJob {
        text: concat!(prefix!(), url!(), suffix!()).into(),
        sections: vec![
            LayoutSection {
                leading_space: 0.,
                byte_range: 0..PREFIX,
                format: TextFormat::simple(font.cheap_clone(), Color32::GRAY),
            },
            LayoutSection {
                leading_space: 0.,
                byte_range: PREFIX..(PREFIX + URL.len()),
                format: TextFormat::simple(font.cheap_clone(), Color32::LIGHT_GRAY),
            },
            LayoutSection {
                leading_space: 0.,
                byte_range: (PREFIX + URL.len())..(PREFIX + URL.len() + SUFFIX),
                format: TextFormat::simple(font, Color32::GRAY),
            },
        ],
        wrap: TextWrapping::wrap_at_width(ui.available_width()),
        ..Default::default()
    };
    ui.hyperlink_to(text, URL);
    ui.add_space(4.);
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

pub struct YtCtx {
    pub auth: Authenticator<<DefaultHyperClient as HyperClientBuilder>::Connector>,
    pub upload_url: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct YtVInfo {
    upload_url: String,
    outdated: bool,
}

pub const UPLOAD_SCOPE: &str = "https://www.googleapis.com/auth/youtube.upload";

#[derive(Error, Debug)]
pub enum YtAuthErr {
    #[error("TouchUp must be updated to use YouTube-related features.\nThe YouTube API has made breaking changes and the current version of TouchUp can no longer interact with it."
    )]
    Outdated,
    #[error("Could not send or parse an http request: {0}")]
    ReqwestError(#[from] reqwest::Error, Backtrace),
    #[error("Could not authenticate into YouTube: {0}")]
    AuthErr(io::Error),
    #[error("Could not access credentials: {0}")]
    KeyRing(#[from] keyring::Error, Backtrace),
    #[error("JSON error: {0}")]
    JsonErr(#[from] serde_json::Error, Backtrace),
}

pub async fn yt_auth(
    ctx: &Context,
    send_url: mpsc::Sender<String>,
    keep_login: bool,
    msg: MessageManager,
) -> Result<YtCtx, YtAuthErr> {
    let secret: ConsoleApplicationSecret =
        serde_json::from_slice(include_bytes!("../embedded/yt_cred.json"))
            .expect("could not parse YouTube cred json");

    let client = https_client();
    let v_info = client
        .get("https://api.github.com/repos/purple-ic/touchup/contents/data/youtube/vinfo1.json")
        .header("User-Agent", "TouchUp")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("Accept", "application/vnd.github.raw+json")
        .send()
        .await?
        .json::<YtVInfo>()
        .await?;
    debug!("yt version info: {v_info:?}");
    if v_info.outdated {
        match yt_remove_entry().await {
            Ok(()) => {}
            Err(e) => report_err("removing the YouTube token entry", &e),
        }
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
        ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
            assert!(!need_code);

            Box::pin(async move {
                match self.send_url.send(url.to_string()) {
                    Ok(()) => {}
                    Err(mpsc::SendError(_)) => return Err("url send channel is closed".into()),
                }
                self.ctx.output_mut(|o| {
                    o.open_url = Some(OpenUrl::new_tab(url));
                });
                self.ctx.request_repaint();
                Ok(String::new())
            })
        }
    }

    let ctx2 = ctx.clone();
    let mut auth_b = InstalledFlowAuthenticator::builder(
        secret.installed.unwrap(),
        InstalledFlowReturnMethod::HTTPRedirect,
    );
    if keep_login {
        auth_b = auth_b.with_storage(Box::new(YtAuthStorage::new(msg).await?))
    }
    let auth = auth_b
        .flow_delegate(Box::new(CodePresenter {
            ctx: ctx2,
            send_url,
        }))
        .build()
        .await
        .map_err(YtAuthErr::AuthErr)?;

    // try to cache the token for the upload scope
    let _ = auth.token(&[UPLOAD_SCOPE]).await;

    let ctx = ctx.cheap_clone();
    spawn_blocking(move || ctx.data_mut(|d| d.insert_persisted(Id::new("ytLoggedIn"), true)));

    Ok(YtCtx {
        auth,
        upload_url: v_info.upload_url,
    })
}

// this is not the std (blocking) OnceLock because OnceLock's try methods aren't stabilized (https://github.com/rust-lang/rust/issues/109737)
// also, it's usually initialized in async code anyway
static YT_ENTRY: tokio::sync::OnceCell<keyring::Entry> = tokio::sync::OnceCell::const_new();

struct YtAuthStorage {
    msg: MessageManager,
}

impl YtAuthStorage {
    async fn new(msg: MessageManager) -> Result<Self, YtAuthErr> {
        // if the entry can't be initialized then return Err
        get_or_init_entry().await?;
        Ok(YtAuthStorage { msg })
    }
}

async fn get_or_init_entry() -> Result<&'static keyring::Entry, keyring::Error> {
    YT_ENTRY
        .get_or_try_init(|| async {
            spawn_blocking(|| Entry::new("touchup", "main"))
                .await
                .expect("Entry::new should not panic")
        })
        .await
}

fn get_entry() -> &'static keyring::Entry {
    YT_ENTRY
        .get()
        .expect("YT_ENTRY must be initialized by this point")
}

#[async_trait]
impl TokenStorage for YtAuthStorage {
    async fn set(&self, scopes: &[&str], token: TokenInfo) -> anyhow::Result<()> {
        assert_eq!(scopes, &[UPLOAD_SCOPE]);
        spawn_blocking(move || {
            let entry = get_entry();
            let bytes = serde_json::to_vec(&token)?;
            Ok(entry.set_secret(&bytes)?)
        })
        .await
        .expect("closure shouldnt fail")
    }

    async fn get(&self, scopes: &[&str]) -> Option<TokenInfo> {
        assert_eq!(scopes, &[UPLOAD_SCOPE]);
        let msg = self.msg.cheap_clone();
        let result = spawn_blocking(move || {
            let entry = get_entry();
            msg.handle_err::<YtAuthErr, _>("attempting to fetch YouTube credentials", |_| {
                match entry.get_secret() {
                    Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
                    Err(keyring::Error::NoEntry) => Ok(None),
                    Err(other) => {
                        msg.report_err("attempting to fetch YouTube credentials", &other);
                        Err(other.into())
                    }
                }
            })
            .flatten()
        })
        .await
        .expect("inner closure shouldn't panic");
        result
    }
}

pub async fn yt_remove_entry() -> Result<(), keyring::Error> {
    let entry = get_or_init_entry().await?;
    spawn_blocking(|| match entry.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(err) => Err(err),
    })
    .await
    .expect("inner closure should never fail (yt_remove_entry)")?;
    Ok(())
}

pub fn yt_log_out(msg: &MessageManager, yt: &YtCtx) {
    // auth is reference-counted
    let auth = yt.auth.clone();

    // revoke the access token
    spawn_async(async move {
        let t = auth.token(&[UPLOAD_SCOPE]).await;
        let token = match t.as_ref().map(|t| t.token()) {
            Ok(Some(token)) => token,
            Ok(None) => unreachable!("tokens should always be required for the upload scope"),
            Err(_) => {
                // assume the token is already invalid. just cancel
                return;
            }
        };

        https_client()
            .post("https://oauth2.googleapis.com/revoke")
            .query(&[("token", token)])
            .send()
            .await
            .unwrap();
    });

    msg.handle_err_spawn("removing YouTube token entry", yt_remove_entry());
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
            let msg2 = msg.cheap_clone();
            let auth_future = msg.handle_err_async(
                "logging into YouTube",
                yt_auth(&ctx, url_send, keep_login, msg2),
            );
            tokio::select! {
                v = auth_future => {
                    if let Some(v) = v {
                        auth.write().youtube = Some(v);
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
        profile_function!();

        let auth_r = auth.read();
        if auth_r.youtube.is_some() {
            return None;
        } else if self.cancel.is_closed() {
            return None;
        }
        ui.heading("Please log into YouTube");
        let url: &String = self.url.get();
        let url_available = !url.is_empty();
        ui.with_layout(Layout::left_to_right(Align::Min), |ui| {
            if url_available {
                if ui.button("Try again").clicked() {
                    self.pressed_try_again = true;
                    ui.output_mut(|o| o.open_url = Some(OpenUrl::new_tab(url)))
                }
                if self.pressed_try_again
                    && Label::new(
                        RichText::new("Click to copy the login URL").color(Color32::WHITE),
                    )
                    .sense(Sense::hover())
                    .ui(ui)
                    .on_hover_cursor(CursorIcon::PointingHand)
                    .clicked()
                {
                    ui.output_mut(|o| o.copied_text = url.to_string())
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

#[derive(Error, Debug)]
pub enum YtUploadError {
    #[error("IO error: {0}")]
    Io(#[from] tokio::io::Error, Backtrace),
    #[error("HTTP error: {0}")]
    Reqwest(#[from] reqwest::Error, Backtrace),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error, Backtrace),
    #[error("Authentication error: {0}")]
    Auth(#[from] yup_oauth2::Error, Backtrace),
}

pub async fn yt_upload(
    client: &reqwest::Client,
    file_name: String,
    mut file: tokio::fs::File,
    ctx: &YtCtx,
    title: &str,
    description: &str,
    vis: YtVisibility,
) -> Result<(), YtUploadError> {
    // todo: track upload progress
    let file_len = file.seek(SeekFrom::End(0)).await?;

    // these docs are for google drive but the same multipart method applies to the youtube api
    // https://developers.google.com/drive/api/guides/manage-uploads#multipart

    let token = ctx.auth.token(&[UPLOAD_SCOPE]).await?;
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
        let metadata = serde_json::to_vec(&snippet)?;
        file.seek(SeekFrom::Start(0)).await?;

        let f = file.try_clone().await?;
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
                    // todo: don't provide the file name if we don't know it yet (atm we provide "<unknown>" in that case)
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
            .await?;

        let status = out.status();

        if status.is_success() {
            return Ok(());
        } else if (status.is_server_error() || status.is_client_error()) && attempts < 5 {
            debug!("retrying request ({status}). made {attempts} previous attempts");
            attempts += 1;
            file.seek(SeekFrom::Start(0)).await?;
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
