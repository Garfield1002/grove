//! The "Open project" dialog and the error area.

use egui::{Context, Ui};

use crate::workers::ErrorReport;

use super::theme;

/// Outcome of the open-project dialog.
pub enum OpenProject {
    Idle,
    Cancelled,
    Confirmed(String),
}

/// Path entry for registering a project. A native folder picker would pull in
/// a portal dependency; Milestone 1 takes a typed path, which the worker then
/// validates with git.
pub fn open_project(ctx: &Context, path: &mut String) -> OpenProject {
    let mut outcome = OpenProject::Idle;
    let mut open = true;

    egui::Window::new("Open project")
        .collapsible(false)
        .resizable(false)
        .open(&mut open)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.set_min_width(300.0);
            ui.label(theme::label(
                "Path to a Git repository or any directory inside one.",
                11.0,
                theme::TEXT_MUTED,
            ));
            ui.add_space(6.0);
            let field = ui.add(
                egui::TextEdit::singleline(path)
                    .hint_text("/home/you/projects/acme-web")
                    .desired_width(f32::INFINITY),
            );
            field.request_focus();
            ui.add_space(4.0);
            ui.label(theme::label(
                "Choosing a linked worktree registers its project.",
                10.0,
                theme::TEXT_FAINT,
            ));
            ui.add_space(10.0);

            let submitted = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    outcome = OpenProject::Cancelled;
                }
                let can_open = !path.trim().is_empty();
                if (ui
                    .add_enabled(can_open, egui::Button::new("Open"))
                    .clicked()
                    || submitted)
                    && can_open
                {
                    outcome = OpenProject::Confirmed(path.trim().to_string());
                }
            });
        });

    if !open && matches!(outcome, OpenProject::Idle) {
        return OpenProject::Cancelled;
    }
    outcome
}

/// The error area: a concise message per failure with expandable diagnostics
/// that never hide git's or tmux's own output. Returns true when the user
/// dismissed everything.
pub fn errors(ui: &mut Ui, errors: &[ErrorReport]) -> bool {
    let mut dismissed = false;
    egui::Frame::new()
        .fill(theme::BG_SUNKEN)
        .inner_margin(egui::Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(theme::label(
                    format!(
                        "{} problem{}",
                        errors.len(),
                        if errors.len() == 1 { "" } else { "s" }
                    ),
                    11.0,
                    theme::DANGER,
                ));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("Dismiss").clicked() {
                        dismissed = true;
                    }
                });
            });
            egui::ScrollArea::vertical()
                .max_height(120.0)
                .show(ui, |ui| {
                    for (index, error) in errors.iter().enumerate().rev() {
                        ui.add(
                            egui::Label::new(theme::label(&error.summary, 11.0, theme::TEXT_DIM))
                                .wrap(),
                        );
                        if let Some(detail) = &error.detail {
                            egui::CollapsingHeader::new(theme::label(
                                "Show command output",
                                10.0,
                                theme::TEXT_FAINT,
                            ))
                            .id_salt(("grove-error", index))
                            .show(ui, |ui| {
                                ui.add(
                                    egui::Label::new(theme::mono(detail, 10.0, theme::TEXT_MUTED))
                                        .wrap(),
                                );
                            });
                        }
                        ui.add_space(2.0);
                    }
                });
        });
    dismissed
}
