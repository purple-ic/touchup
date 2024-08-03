use std::mem;
use std::path::{Path, PathBuf};

use eframe::egui::Ui;
use eframe::Frame;
use egui::{Align, Color32, Context, CursorIcon, FontSelection, Id, Label, Layout, OpenUrl, RichText, Sense, TextEdit, TextStyle, TextWrapMode, Widget, WidgetText};
use rfd::{MessageButtons, MessageDialog, MessageLevel};

use SelectScreenOut::*;

use crate::{AuthArc, storage};
use crate::util::report_err;

pub struct SelectScreen {
    // dir_reader: OwnedThreads,
    // dirs: Vec<PathBuf>,
    // dir_receiver: Receiver<PathBuf>,
    out_path: String,
}

pub enum SelectScreenOut {
    Stay,
    Edit(PathBuf),
    #[cfg(feature = "youtube")]
    YtLogin,
}

impl SelectScreen {
    pub fn new(ctx: Context) -> Self {
        Self {
            out_path: ctx
                .data_mut(|d| d.get_persisted(Id::new("outPath")))
                .or_else(|| {
                    dirs::video_dir()
                        .map(|p| p.join("touchup"))
                        .and_then(|p| p.to_str().map(str::to_string))
                })
                .unwrap_or_else(|| String::new()),
        }
    }

    #[must_use]
    pub fn draw(&mut self, ui: &mut Ui, frame: &mut Frame, auth: &AuthArc) -> SelectScreenOut {
        let mut try_save_path = |ctx: &Context, frame: &mut Frame, out_path: &mut String| {
            let path = Path::new(out_path);
            match path.try_exists() {
                Ok(_) if !out_path.is_empty() => {
                    ctx.data_mut(|d| d.insert_persisted(Id::new("outPath"), mem::take(out_path)));
                    true
                }
                _ => {
                    MessageDialog::new()
                        .set_title("Invalid output path")
                        .set_level(MessageLevel::Error)
                        .set_description(format!("The given output path is invalid: {path:?}"))
                        .set_buttons(MessageButtons::Ok)
                        .show();
                    false
                }
            }
        };
        let mut yt_login = false;

        ui.style_mut().wrap_mode = Some(TextWrapMode::Wrap);

        let do_fd = ui.scope(|ui| {
            for (_, font) in ui.style_mut().text_styles.iter_mut() {
                font.size *= 1.2;
            }
            ui
                .with_layout(Layout::left_to_right(Align::Min), |ui| {
                    let do_fd = ui.button(RichText::new("Open file...").color(Color32::WHITE)).clicked();
                    ui.label(WidgetText::from("Or drag & drop").text_style(TextStyle::Button).color(Color32::WHITE));
                    do_fd
                })
                .inner
        }).inner;

        ui.add_space(32.);

        ui.label("You can start editing videos by opening a video file.");
        ui.label("If you find any problems, please report them to our issue tracker on Github!");

        if Label::new("TouchUp's source code and issue tracker can be found at https://github.com/purple-ic/touchup")
            .sense(Sense::click())
            .ui(ui)
            .on_hover_cursor(CursorIcon::PointingHand)
            .clicked() {
            ui.output_mut(|o| o.open_url = Some(OpenUrl::new_tab("https://github.com/purple-ic/touchup")))
        }

        ui.add_space(24.);

        ui.heading("Settings");
        ui.add_space(4.);
        ui.with_layout(Layout::top_down(Align::Min), |ui| {
            // returns None if the value didn't change,
            //  or Some with the new value if it did change
            let mut checkbox = |ui: &mut Ui, id: &str, text: &str, description: &str, default: bool| -> Option<bool> {
                let id = Id::new(id);
                let mut checked = ui
                    .ctx()
                    .data_mut(|d| d.get_persisted(id).unwrap_or(default));
                let old = checked;
                ui.checkbox(&mut checked, text)
                    .on_hover_text(description);
                if checked != old {
                    ui.ctx().data_mut(|d| d.insert_persisted(id, checked));
                    Some(checked)
                } else {
                    None
                }
            };
            ui.group(|ui| {
                ui.with_layout(Layout::left_to_right(Align::Min), |ui| {
                    ui.label("Output folder");
                    TextEdit::singleline(&mut self.out_path)
                        .show(ui);
                    if ui.button("📁")
                        .on_hover_text("Browse folder")
                        .clicked() {
                        let f = rfd::FileDialog::new()
                            .set_title("Select output folder")
                            .set_parent(frame)
                            .set_directory(&self.out_path)
                            .pick_folder();
                        if let Some(f) = f {
                            self.out_path = f.display().to_string();
                        }
                    }
                });
                checkbox(ui, "useTrash", "Send deleted videos to the recycling/trash bin", "If enabled, deleted videos will be moved to your recycling bin. Or if disabled, they will be deleted permanently.",
                         true);

                checkbox(ui, "deleteAfterExport", "Delete input videos after export", "If enabled, the original video will be deleted after the edited version is exported.",
                         false);
                if ui.button("Open data folder")
                    .on_hover_text("Open the folder where preferences and authentication are saved.")
                    .clicked() {
                    let storage = storage();

                    if let Err(e) = open::that_detached(storage) {
                        report_err("attempting to open data folder", &e);
                        MessageDialog::new()
                            .set_title("TouchUp error")
                            .set_buttons(MessageButtons::Ok)
                            .set_description(format!("Could not open data folder: \n{e}"))
                            .set_parent(frame)
                            .show();
                    }
                }
            });
            ui.add_space(8.);
            #[cfg(feature = "youtube")]
            ui.group(|ui| {
                use crate::youtube;
                ui.with_layout(Layout::left_to_right(Align::Min), |ui| {
                    let keep_login_delta =
                        checkbox(ui, "ytKeepLogin", "Stay logged in to YouTube",
                                 "Whether to stay logged in to your YouTube account. Disabling will delete the token file and as such you'll have to log back in on the next startup even if you turn it back on immediately after.",
                                 true);
                    // if the user disabled login saving, delete the token cache
                    if let Some(false) = keep_login_delta {
                        youtube::yt_delete_token_file()
                    }
                    let auth_r = auth.read();
                    if auth_r.youtube.is_some() {
                        if ui.button("Log out of YouTube").clicked() {
                            drop(auth_r);
                            let mut auth_w = auth.write();
                            let yt = auth_w.youtube.take().expect("youtube should not have been removed between switching lock from read -> to write");
                            youtube::yt_log_out(&yt);
                        }
                    } else {
                        if ui.button("Log in to YouTube").clicked() {
                            yt_login = true;
                        }
                    }
                });
                checkbox(ui, "deleteAfterUpload", "Delete exported videos after uploading", "If enabled, the edited version of the video will be deleted as soon as it is uploaded. The original unedited version will remain intact unless \"Delete input videos after export\" is enabled.",
                         true);
            });
        });

        // todo: maybe offer a complete list of crate dependencies?
        let credits = WidgetText::from("This software uses libraries from the FFmpeg project under the LGPLv2.1\n\
            ...in addition to crates such as: egui/eframe, ffmpeg-next, rodio, rfd, trash-rs, open-rs, dirs-rs, google-youtube3, serde, serde_json and tokio.");
        let credits = credits.into_galley(ui, None, ui.available_width(), FontSelection::Default);
        let credits_layout = if credits.size().y + 8. > ui.available_height() {
            ui.add_space(8.);
            Layout::top_down(Align::Min)
        } else {
            Layout::bottom_up(Align::Min)
        };
        ui.with_layout(credits_layout, |ui| {
            ui.label(credits);
        });

        if do_fd {
            match rfd::FileDialog::new()
                .set_parent(frame)
                .set_title("Pick a video to edit")
                .pick_file() {
                Some(v) if try_save_path(ui.ctx(), frame, &mut self.out_path) => {
                    Edit(v)
                }
                _ => Stay,
            }
        } else if let Some(v) = ui
            .input_mut(|i| i.raw.dropped_files.pop())
            .and_then(|f| f.path)
        {
            if try_save_path(ui.ctx(), frame, &mut self.out_path) {
                Edit(v)
            } else {
                Stay
            }
        } else if yt_login {
            #[cfg(feature = "youtube")]
            {
                SelectScreenOut::YtLogin
            }
            #[cfg(not(feature = "youtube"))]
            {
                unreachable!()
            }
        } else {
            Stay
        }
        // let mut output = None;

        // for dir in self.dir_receiver.try_iter() {
        //     self.dirs.push(dir)
        // }

        // let font_height = ui.style().text_styles.get(&TextStyle::Body).unwrap().size;
        // TableBuilder::new(ui)
        //     .column(Column::remainder())
        //     .vscroll(true)
        //     .body(|body| {
        //         body.rows(font_height, self.dirs.len(), |mut row| {
        //             let row_index = row.index();
        //             let path = &self.dirs[row_index];
        //
        //             row.col(|ui| {
        //                 if ui.link(path.file_stem().unwrap().to_string_lossy()).clicked() {
        //                     output = Some(row_index);
        //                 }
        //             });
        //         })
        //     });
        //
        // output.map(|idx| {
        //     // swap remove is faster than remove and we don't need to preserve ordering
        //     //      (we actually don't even care about the vec staying alive after ths, but that doesn't matter)
        //     self.dirs.swap_remove(idx)
        // })
    }
}
