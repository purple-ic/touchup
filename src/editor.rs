use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;
use std::{fs, mem};

use eframe::emath::{Pos2, Rect};
use eframe::epaint::TextureId;
use eframe::Frame;
use egui::{
    include_image, Align, Align2, Button, Color32, Context, CursorIcon, FontId, Image, Layout,
    RichText, Sense, Ui, Vec2, Widget, WidgetText,
};

use crate::editor::EditorExit::ToSelectScreen;
use crate::export::ExportFollowUp;
use crate::player::{write_duration, PlayerUI};
use crate::task::{Task, TaskCommand, TaskStage, TaskStatus};
use crate::util::{report_err, Updatable};
use crate::{AuthArc, MessageManager, TextureArc};

pub struct Editor {
    player: PlayerUI,
    current_audio_track: Option<usize>,
    trim: RangeInclusive<f32>,
    path: PathBuf,
}

pub enum EditorExit {
    ToSelectScreen,
    #[cfg(feature = "youtube")]
    ToYoutubeScreen {
        init: crate::youtube::YtScreen,
    },
}

impl Editor {
    pub fn new(
        ctx: &Context,
        msg: MessageManager,
        path: PathBuf,
        frame: &mut Frame,
        texture: TextureArc,
    ) -> Option<Self> {
        let player = PlayerUI::new(ctx, msg, &path, frame, texture)?;

        Some(Editor {
            trim: (0.)..=player.duration().as_secs_f32(),
            current_audio_track: if player.nb_audio_tracks() > 0 {
                Some(0)
            } else {
                None
            },
            player,
            path,
        })
    }

    #[must_use]
    pub fn draw(
        &mut self,
        (task_id, tasks): &mut (u32, Vec<Task>),
        auth: &AuthArc,
        ui: &mut Ui,
        task_cmds: &Sender<TaskCommand>,
        current_texture: Option<&TextureId>,
    ) -> Option<EditorExit> {
        let mut exit = None;
        if self.player.is_closed() {
            exit = Some(ToSelectScreen)
        }

        if Button::new(RichText::new("   <").size(20.))
            .frame(false)
            .ui(ui)
            .on_hover_cursor(CursorIcon::PointingHand)
            .clicked()
        {
            exit = Some(ToSelectScreen)
        }

        self.player.draw(&self.trim, ui, current_texture);
        ui.add_space(15.);

        let old_audio_track = self.current_audio_track;

        // only show the audio track selector when there is more than one
        let nb_audio_tracks = self.player.nb_audio_tracks();
        if nb_audio_tracks > 1 {
            ui.with_layout(Layout::left_to_right(Align::Min), |ui| {
                ui.label("Audio track");
                ui.columns(self.player.nb_audio_tracks() + 1, |columns| {
                    let mut columns = columns.iter_mut();
                    {
                        let ui = columns.next().unwrap_or_else(|| unreachable!());
                        ui.radio_value(&mut self.current_audio_track, None, "None");
                    }
                    // let width_per_column = total_width / columns.len() as f32;
                    for (i, ui) in columns.enumerate() {
                        ui.with_layout(Layout::top_down(Align::Center), |ui| {
                            // ui.set_max_width(width_per_column);
                            ui.radio_value(
                                &mut self.current_audio_track,
                                Some(i),
                                (i + 1/* make the track numbers one-indexed */).to_string(),
                            );
                        });
                    }
                })
            });
            ui.add_space(15.);
        } else if nb_audio_tracks == 1 {
            let mut enable_audio = self.current_audio_track.is_some();

            ui.with_layout(Layout::top_down(Align::Center), |ui| {
                ui.checkbox(&mut enable_audio, "Enable audio")
            });
            self.current_audio_track = if enable_audio { Some(0) } else { None };
            ui.add_space(15.);
        }
        if old_audio_track != self.current_audio_track {
            self.player.set_audio_track(self.current_audio_track)
        }

        ui.with_layout(Layout::left_to_right(Align::Min), |ui| {
            ui.add_space(64.);

            let old_trim = self.trim.clone();
            let real_pos_now = self.player.real_pos().as_secs_f32();
            let out = ui
                .with_layout(Layout::left_to_right(Align::Min), |ui| {
                    ui.set_max_width(ui.available_width() - 64.);
                    Trimmer {
                        range: &mut self.trim,
                        max: self.player.duration().as_secs_f32(),
                        cursor_pos: real_pos_now,
                    }
                    .show(ui)
                })
                .inner;
            let clamp_real_pos = || {
                Duration::from_secs_f32(real_pos_now.clamp(*self.trim.start(), *self.trim.end()))
            };
            if self.trim != old_trim && !self.trim.contains(&real_pos_now) {
                self.player.update_preview(clamp_real_pos());
                self.player.enable_preview_mode();
            }
            if out.range_changed && self.player.preview_mode() {
                self.player.seek_to(clamp_real_pos())
            }
            match out.cursor {
                CursorUpdate::Dormant => {}
                CursorUpdate::Dragging(pos) => {
                    self.player.enable_preview_mode();
                    self.player.update_preview(Duration::from_secs_f32(pos))
                }
                CursorUpdate::Done(pos) => self.player.seek_to(Duration::from_secs_f32(pos)),
            }
        });
        let mut export = |task_id: &mut u32, path: &mut PathBuf, follow_up: ExportFollowUp| {
            let id = *task_id;
            let (status_sender, status) = Updatable::new_sync(
                TaskStatus {
                    stage: TaskStage::Export {
                        has_follow_up: !matches!(follow_up, ExportFollowUp::Nothing),
                    },
                    progress: 0.0,
                },
                2,
            );
            let path = fs::canonicalize(&path).unwrap_or_else(|e| {
                report_err("canonicalizing input path", &e);
                mem::take(path)
            });
            let name: String = String::from(path.file_stem().unwrap_or_default().to_string_lossy());
            #[cfg(feature = "async")]
            let (cancel_send, cancel_recv) = if follow_up.needs_async_stopper() {
                Some(tokio::sync::oneshot::channel())
            } else {
                None
            }
            .unzip();

            self.player.export(
                (*self.trim.start(), *self.trim.end()),
                self.current_audio_track,
                status_sender,
                path,
                id,
                #[cfg(any(feature = "youtube"))]
                Arc::clone(auth),
                #[cfg(not(any(feature = "youtube")))]
                (),
                follow_up,
                Sender::clone(task_cmds),
                #[cfg(feature = "async")]
                cancel_recv,
            );
            *task_id = task_id.wrapping_add(1);
            tasks.push(Task {
                status,
                id,
                name,
                remove_requested: false,
                #[cfg(feature = "async")]
                async_stopper: cancel_send.map(crate::util::AsyncCanceler::new),
            });
        };
        ui.with_layout(Layout::right_to_left(Align::Max), |ui| {
            if Button::new(WidgetText::from("Export"))
                // .fill(Color32::LIGHT_GREEN)
                .ui(ui)
                .on_hover_text("Save the edited video to file")
                .clicked()
            {
                exit = Some(ToSelectScreen);
                export(task_id, &mut self.path, ExportFollowUp::Nothing)
            }
            #[cfg(feature = "youtube")]
            ui.add_enabled_ui(auth.read().youtube.is_some(), |ui| {
                if Button::image_and_text(
                    Image::new(include_image!("../embedded/yt_logo.svg")),
                    WidgetText::from("Upload").color(Color32::WHITE),
                )
                    .fill(Color32::RED)
                    .ui(ui)
                    .on_hover_text("Upload the edited video to YouTube")
                    .on_disabled_hover_text("You must log in (in the startup screen) to upload to YouTube")
                    .clicked()
                {
                    let (send, recv) = tokio::sync::oneshot::channel();
                    exit = Some(EditorExit::ToYoutubeScreen {
                        init: crate::youtube::YtScreen {
                            title: self.path.file_stem().unwrap_or_else(|| unreachable!("user should not be able to select a path without a file stem")).to_string_lossy().into(),
                            description: "".to_string(),
                            send_final: Some(send),
                            // task_id will be incremented later, in the `export` closure
                            task_id: *task_id,
                        },
                    });

                    export(
                        task_id,
                        &mut self.path,
                        ExportFollowUp::Youtube { info_recv: recv },
                    )
                }
            })
        });
        exit
    }
}

#[derive(Default)]
pub enum CursorUpdate {
    #[default]
    Dormant,
    Dragging(f32),
    Done(f32),
}

#[derive(Default)]
pub struct TrimmerOutput {
    range_changed: bool,
    cursor: CursorUpdate,
}

pub struct Trimmer<'a> {
    range: &'a mut RangeInclusive<f32>,
    max: f32,
    cursor_pos: f32,
}

impl<'a> Trimmer<'a> {
    pub fn show(self, ui: &mut Ui) -> TrimmerOutput {
        let mut out = TrimmerOutput::default();

        let (_id, rect) = ui.allocate_space(Vec2::new(ui.available_width(), 64.));

        let inner_rect = Rect::from_min_max(
            Pos2::new(
                rect.min.x + self.range.start() / self.max * rect.width(),
                rect.min.y,
            ),
            Pos2::new(
                rect.min.x + self.range.end() / self.max * rect.width(),
                rect.max.y,
            ),
        );

        if ui.is_rect_visible(rect) {
            let clip_rect = rect.expand(30.);

            let painter = ui.painter_at(clip_rect);
            painter.rect_filled(rect, 5., Color32::GRAY);

            painter.rect_filled(inner_rect, 5., Color32::LIGHT_BLUE);
            let make_cursor_rect = |cursor_pos: f32| {
                let cursor_x = rect.min.x
                    + cursor_pos.clamp(*self.range.start(), *self.range.end()) / self.max
                        * rect.width();
                Rect::from_min_max(
                    Pos2::new(cursor_x, rect.min.y - 10.),
                    Pos2::new(cursor_x, rect.max.y + 3.),
                )
                .expand2(Vec2::new(2., 0.))
            };
            let mut cursor_rect = make_cursor_rect(self.cursor_pos);
            let cursor_r = ui
                .interact(cursor_rect, ui.next_auto_id(), Sense::drag())
                .on_hover_cursor(CursorIcon::ResizeHorizontal);
            if let Some(pos) = cursor_r.interact_pointer_pos() {
                let new_pos = ((pos.x - rect.min.x) / rect.width() * self.max)
                    .clamp(*self.range.start(), *self.range.end());
                if cursor_r.drag_stopped() {
                    out.cursor = CursorUpdate::Done(new_pos)
                } else {
                    out.cursor = CursorUpdate::Dragging(new_pos)
                }
                cursor_rect = make_cursor_rect(new_pos);
            }
            painter.rect(cursor_rect, 0., Color32::WHITE, (0.2, Color32::BLACK));

            ui.skip_ahead_auto_ids(1);

            let mut add_handle = |x: f32, current: f32, clamp: RangeInclusive<f32>| {
                let r = Rect::from_min_max(
                    Pos2::new(x, inner_rect.min.y),
                    Pos2::new(x, inner_rect.max.y),
                )
                .expand2(Vec2::new(5., 5.));
                painter.rect(
                    r,
                    2.,
                    Color32::from_rgb(132, 179, 194),
                    (1., Color32::BLACK),
                );
                let mut str = String::new();
                write_duration(Duration::from_secs_f32(current), &mut str)
                    .expect("writing duration to String should not fail");
                painter.text(
                    r.center_bottom(),
                    Align2::CENTER_TOP,
                    str,
                    FontId {
                        size: 14.,
                        family: Default::default(),
                    },
                    Color32::WHITE,
                );
                let d = ui
                    .interact(r, ui.next_auto_id(), Sense::drag())
                    .on_hover_cursor(CursorIcon::ResizeHorizontal);

                ui.skip_ahead_auto_ids(1);

                if d.drag_stopped() {
                    out.range_changed = true;
                }
                d.interact_pointer_pos()
                    .map(|pos| {
                        let pos = pos.x.clamp(rect.min.x, rect.max.x) - rect.min.x;
                        let value =
                            (pos / rect.width() * self.max).clamp(*clamp.start(), *clamp.end());
                        value
                    })
                    .unwrap_or(current)
            };
            let lo = add_handle(
                inner_rect.min.x,
                *self.range.start(),
                (0.)..=(self.range.end() - 1.),
            );
            let hi = add_handle(
                inner_rect.max.x,
                *self.range.end(),
                (self.range.start() + 1.)..=self.max,
            );
            let new_range = lo..=hi;

            *self.range = new_range;
        }

        out
    }
}
