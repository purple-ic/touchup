use std::borrow::Cow;
use std::fmt;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Sender, SyncSender};
use std::time::Duration;

use eframe::egui::load::SizedTexture;
use eframe::egui::{
    include_image, Align, Align2, Area, Color32, Context, FontSelection, Id, Image, ImageSource,
    Key, Label, Layout, Rect, Sense, Slider, Ui, Vec2, Widget, WidgetText,
};
use eframe::{egui, Frame};
use egui::load::Bytes;
use egui::{CursorIcon, TextureId};
use ffmpeg::format;
use rfd::{MessageButtons, MessageDialog, MessageLevel};
use tex::TextureArc;

use crate::export::ExportFollowUp;
use crate::player::r#impl::Player;
use crate::task::{TaskCommand, TaskStatus};
use crate::util::{time_diff, BoolExt};
use crate::{AuthArc, MessageManager};

pub mod r#impl;
pub mod tex;

pub struct PlayerUI {
    player: Player,
    last_preview: Option<Duration>,
    force_controls: bool,
    volume: f32,
    enable_sound: bool,
}

impl PlayerUI {
    pub fn is_closed(&self) -> bool {
        self.player.is_closed()
    }

    pub fn export(
        &self,
        trim: (f32, f32),
        audio_track: Option<usize>,
        status: SyncSender<TaskStatus>,
        path: PathBuf,
        id: u32,
        auth: AuthArc,
        follow_up: ExportFollowUp,
        task_cmds: Sender<TaskCommand>,
        #[cfg(feature = "async")] cancel_recv: Option<tokio::sync::oneshot::Receiver<()>>,
    ) {
        self.player.begin_export(
            trim,
            audio_track,
            status,
            path,
            id,
            auth,
            follow_up,
            task_cmds,
            #[cfg(feature = "async")]
            cancel_recv,
        )
    }

    pub fn seek_to(&mut self, to: Duration) {
        self.player.seek_to(to)
    }

    pub fn update_preview(&mut self, at: Duration) {
        if self
            .last_preview
            .map(|last_preview| time_diff(last_preview, at) > Duration::from_secs(1))
            .unwrap_or(true)
        {
            self.last_preview = Some(at);
            self.player.update_preview(at)
        }
    }

    pub fn preview_mode(&self) -> bool {
        self.player.preview_mode()
    }

    pub fn enable_preview_mode(&mut self) {
        self.player.enable_preview_mode()
    }

    pub fn nb_audio_tracks(&self) -> usize {
        self.player.nb_audio_tracks()
    }

    pub fn duration(&self) -> Duration {
        self.player.duration()
    }

    pub fn real_pos(&mut self) -> Duration {
        self.player.time().now()
    }

    pub fn set_audio_track(&mut self, idx: Option<usize>) {
        self.player.set_audio_track(idx)
    }

    pub fn new(
        ctx: &Context,
        msg: MessageManager,
        path: &(impl AsRef<Path> + ?Sized),
        frame: &mut Frame,
        texture: TextureArc,
    ) -> Option<Self> {
        let input = match format::input(path) {
            Ok(i) => i,
            Err(e) => {
                MessageDialog::new()
                    .set_title("Invalid file")
                    .set_description(format!(
                        "The provided file ({file}) cannot be opened.\n{e}",
                        file = path
                            .as_ref()
                            .file_name()
                            .expect("user should not be able to choose an input path that doesn't have a filename")
                            .to_str()
                            .unwrap_or("<filename cannot be displayed>")
                    ))
                    .set_buttons(MessageButtons::Ok)
                    .set_level(MessageLevel::Error)
                    .set_parent(frame)
                    .show();
                return None;
            }
        };
        let volume = ctx
            .data_mut(|d| d.get_persisted(Id::new("preferredVolume")))
            .unwrap_or(1.);
        let enable_sound =
            ctx.data_mut(|d| d.get_persisted(Id::new("enableSound")).unwrap_or(true));
        Some(Self {
            player: Player::new(
                Context::clone(ctx),
                msg,
                input,
                if enable_sound { volume } else { 0. },
                texture,
            )?,
            last_preview: None,
            force_controls: false,
            volume,
            enable_sound,
        })
    }

    pub fn draw(
        &mut self,
        trim: &RangeInclusive<f32>,
        ui: &mut Ui,
        current_texture: Option<&TextureId>,
    ) {
        let ctx = ui.ctx().clone() /* ctx is rc'd */;

        if !self.player.is_paused() {
            ctx.request_repaint_after(self.player.frame_time());
        }

        let [w, h] = self.player.resolution();

        let source = match current_texture {
            None => ImageSource::Bytes {
                uri: Cow::Borrowed("bytes://empty"),
                bytes: Bytes::Static(&[]),
            },
            Some(current_texture) => ImageSource::Texture(SizedTexture::new(
                *current_texture,
                Vec2::new(w as f32, h as f32),
            )),
        };
        let video_response = ui.add(Image::new(source).fit_to_exact_size(ui.available_size()));
        let video_rect = video_response.rect;

        //      1 = fully visible, 0 = invisible
        let control_opacity = ctx.animate_bool_with_time(
            Id::new("showVideoControls"),
            self.force_controls || ui.rect_contains_pointer(video_rect),
            0.3,
        );
        self.force_controls = false;

        Area::new(Id::new("videoControls"))
            .constrain_to(video_rect)
            .anchor(Align2::LEFT_BOTTOM, (0., 0.))
            .interactable(false)
            // .order(Order::Tooltip)
            .show(&ctx, |ui| {
                // ui.with_layer_id(LayerId::background(), |ui| {
                egui::Frame::dark_canvas(ui.style())
                    .multiply_with_opacity(control_opacity * 0.7)
                    .show(ui, |ui| {
                        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                            self.draw_controls(trim, video_rect, control_opacity, ui)
                        })
                    });
            });

        let (space, left, right) = ctx.input_mut(|i| {
            (
                i.key_pressed(Key::Space),
                i.key_pressed(Key::ArrowLeft),
                i.key_pressed(Key::ArrowRight),
            )
        });

        if space {
            self.player.user_toggle_pause(trim);
        }
        if left {
            self.player.backward();
        }
        if right {
            self.player.forward();
        }
    }

    fn draw_controls(
        &mut self,
        trim: &RangeInclusive<f32>,
        video_rect: Rect,
        control_opacity: f32,
        ui: &mut Ui,
    ) {
        ui.set_min_width(video_rect.width());
        ui.set_max_size(Vec2::new(video_rect.width(), 30.));

        {
            let style = ui.style_mut();
            let noninteractive = &mut style.visuals.widgets.noninteractive;
            noninteractive.fg_stroke.color = Color32::WHITE.gamma_multiply(control_opacity);
        }

        let play_pause_response = if self.player.is_paused() {
            Image::new(include_image!("../../embedded/play.svg"))
        } else {
            Image::new(include_image!("../../embedded/pause.svg"))
        }
        .sense(Sense::click())
        .tint(Color32::WHITE.gamma_multiply(control_opacity))
        .ui(ui)
        .on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_text(if self.player.is_paused() {
            "Resume"
        } else {
            "Pause"
        });

        let play_size = play_pause_response.rect.size();

        if play_pause_response.clicked() {
            self.player.user_toggle_pause(trim)
        }

        let mut str = String::new();

        let real_vid_pos = self.player.time().now();
        // pause if the video is out of trim bounds
        if !self.player.is_paused() && real_vid_pos.as_secs_f32() > *trim.end() {
            self.player.toggle_pause()
        }

        let video_pos = real_vid_pos.saturating_sub(Duration::from_secs_f32(*trim.start()));
        let duration = Duration::from_secs_f32(trim.end() - trim.start());

        write_duration(video_pos, &mut str)
            .expect("duration write should not fail when writing to String");
        Label::new(str).selectable(false).ui(ui);

        let mut str = String::new();
        write_duration(duration, &mut str)
            .expect("duration write should not fail when writing to String");
        let dur_text = WidgetText::from(str);
        let dur_galley =
            dur_text.into_galley(ui, None, ui.available_width(), FontSelection::Default);
        let rhs_size = dur_galley.size().x + play_size.x;

        let mut sec = real_vid_pos.as_secs_f32();
        let slider_response = ui
            .scope(|ui| {
                // new scope so we can change the style without affecting the rest
                {
                    let available_width = ui.available_width();

                    let style = ui.style_mut();
                    style.spacing.slider_width = available_width - rhs_size - 30.;
                    // todo: the trailing fill is drawn over the rail and the opacity stacks weirdly :/
                    style.visuals.selection.bg_fill =
                        Color32::from_gray(155).gamma_multiply(control_opacity);
                    style.visuals.widgets.inactive.bg_fill = style
                        .visuals
                        .widgets
                        .inactive
                        .bg_fill
                        .gamma_multiply(control_opacity);
                    style.visuals.widgets.inactive.fg_stroke.color = style
                        .visuals
                        .widgets
                        .active
                        .fg_stroke
                        .color
                        .gamma_multiply(control_opacity);
                }
                Slider::new::<f32>(&mut sec, trim.clone())
                    .show_value(false)
                    .trailing_fill(true)
                    .ui(ui)
            })
            .inner;

        self.force_controls |= slider_response.dragged();
        if slider_response.drag_stopped() {
            self.seek_to(Duration::from_secs_f32(sec))
        } else if slider_response.dragged() {
            if !self.preview_mode() {
                self.enable_preview_mode();
            }
            self.update_preview(Duration::from_secs_f32(sec))
        }

        ui.allocate_ui_with_layout(
            ui.available_size(),
            Layout::right_to_left(Align::Center),
            |ui| {
                ui.add_space(10.);
                let mut volume = Image::new(if self.enable_sound && self.volume > 0. {
                    include_image!("../../embedded/volume.svg")
                } else {
                    include_image!("../../embedded/volume-mute.svg")
                })
                .sense(Sense::click())
                .tint(Color32::WHITE.gamma_multiply(control_opacity))
                .ui(ui)
                .on_hover_cursor(CursorIcon::PointingHand);
                if volume.clicked() {
                    self.enable_sound.toggle();
                    ui.data_mut(|d| d.insert_persisted(Id::new("enableSound"), self.enable_sound));
                    self.player
                        .change_volume(if self.enable_sound { self.volume } else { 0. });
                }

                let mut show_tooltip = |ui: &mut Ui| {
                    ui.add_enabled_ui(self.enable_sound, |ui| {
                        // todo: somehow show the volume %
                        ui.style_mut().visuals.selection.bg_fill =
                            Color32::from_gray(155).gamma_multiply(control_opacity);
                        self.force_controls = true;
                        // ui.style_mut().spacing.slider_width = ui.available_height();

                        let slider_resp = Slider::new(&mut self.volume, 0. ..=2.)
                            .trailing_fill(true)
                            .vertical()
                            .show_value(false)
                            .ui(ui);

                        if slider_resp.dragged() {
                            self.player.change_volume(self.volume)
                        }
                        if slider_resp.drag_stopped() {
                            ui.ctx().data_mut(|d| {
                                d.insert_persisted(Id::new("preferredVolume"), self.volume)
                            })
                        }
                    });
                };
                let mut was_shown = false;
                volume = volume.on_hover_ui(|ui| {
                    was_shown = true;
                    show_tooltip(ui)
                });
                if !was_shown && volume.hovered() {
                    volume.show_tooltip_ui(show_tooltip)
                }

                Label::new(dur_galley).selectable(false).ui(ui)
            },
        );
    }
}

pub fn write_duration(duration: Duration, into: &mut impl fmt::Write) -> fmt::Result {
    let mut secs = duration.as_secs();
    let mut mins = secs / 60;
    secs %= 60;
    let hours = mins / 60;
    mins %= 60;

    if hours > 0 {
        write!(into, "{hours}:{mins:0>2}:{secs:0>2}")
    } else {
        write!(into, "{mins:0>2}:{secs:0>2}")
    }
}
