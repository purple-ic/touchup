use crate::util::Updatable;
use crate::util::{self, plural};
use eframe::epaint::Color32;
use egui::panel::TopBottomSide;
use egui::{Align, Id, Label, Layout, ProgressBar, RichText, TopBottomPanel, Widget, WidgetText};
use std::sync::mpsc;
use std::time::Duration;

pub fn draw_tasks(
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
        !status.is_finished() && !task.remove_requested && !task.status.is_closed()
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
    pub stage: TaskStage,
    // - usually works as a normalized (0 to 1) scale of progress
    // - when is +Infinity, represents a task that is currently being worked on but the progress can't be tracked
    // - when is -Infinity, represents a task that can't be worked on and is waiting for user input
    pub progress: f32,
}

impl TaskStatus {
    pub fn is_finished(&self) -> bool {
        self.stage.is_final() && self.progress >= 1. && self.progress.is_finite()
    }
}

pub struct Task {
    pub status: Updatable<TaskStatus>,
    pub id: u32,
    pub name: String,
    pub remove_requested: bool,
    #[cfg(feature = "async")]
    pub async_stopper: Option<util::AsyncCanceler>,
}

#[derive(Debug, Clone)]
pub enum TaskCommand {
    Rename { id: u32, new_name: String },
    Cancel { id: u32 },
}
