//! Dialogs: open project, create worktree, safe removal, and the error area.

pub mod create_worktree;
pub mod removal;

use egui::{Context, Ui};

use crate::workers::ErrorReport;

use super::theme;

/// Outcome of the open-project dialog.
pub enum OpenProject {
    Idle,
    Cancelled,
    Confirmed(String),
    /// Ask for the desktop's directory picker; the field stays typeable.
    Browse,
}

/// Path entry for registering a project. The path is always typeable — the
/// worker validates it with git either way; with the `native-file-picker`
/// feature a folder button additionally fills the field from the desktop's
/// own portal dialog.
pub fn open_project(ctx: &Context, path: &mut String) -> OpenProject {
    let mut outcome = OpenProject::Idle;
    let mut open = true;

    egui::Window::new("Open project")
        .collapsible(false)
        .resizable(false)
        .open(&mut open)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.set_min_width(320.0);
            ui.label(theme::label(
                "Path to a Git repository or any directory inside one.",
                theme::FONT_BODY,
                theme::TEXT_MUTED,
            ));
            ui.add_space(8.0);
            let mut field = None;
            ui.horizontal(|ui| {
                let browse = super::NATIVE_FILE_PICKER;
                let reserved = if browse {
                    theme::ICON_BUTTON + 8.0
                } else {
                    0.0
                };
                let width = (ui.available_width() - reserved).max(120.0);
                let response = ui.add(
                    egui::TextEdit::singleline(path)
                        .hint_text("/home/you/projects/acme-web")
                        .desired_width(width),
                );
                response.request_focus();
                field = Some(response);
                if browse
                    && super::icons::button(ui, true, super::icons::folder)
                        .on_hover_text("Choose a directory")
                        .clicked()
                {
                    outcome = OpenProject::Browse;
                }
            });
            let Some(field) = field else { return };
            ui.add_space(6.0);
            ui.label(theme::label(
                "Choosing a linked worktree registers its project.",
                theme::FONT_SMALL,
                theme::TEXT_FAINT,
            ));
            ui.add_space(12.0);

            let submitted = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            ui.horizontal(|ui| {
                let can_open = !path.trim().is_empty();
                let open_button = egui::Button::new(theme::label(
                    "Open",
                    theme::FONT_BODY,
                    if can_open {
                        theme::TEXT_STRONG
                    } else {
                        theme::TEXT_FAINT
                    },
                ))
                .fill(theme::ACCENT_FILL)
                .stroke(egui::Stroke::new(1.0, theme::ACCENT.gamma_multiply(0.6)));
                if (ui.add_enabled(can_open, open_button).clicked() || submitted) && can_open {
                    outcome = OpenProject::Confirmed(path.trim().to_string());
                }
                if ui
                    .button(theme::label("Cancel", theme::FONT_BODY, theme::TEXT_DIM))
                    .clicked()
                {
                    outcome = OpenProject::Cancelled;
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
        .inner_margin(egui::Margin::symmetric(theme::PANEL_MARGIN_X, 9))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let (bullet, _) =
                    ui.allocate_exact_size(egui::Vec2::splat(12.0), egui::Sense::hover());
                super::icons::warning(ui.painter(), bullet, theme::DANGER);
                ui.label(theme::label(
                    format!(
                        "{} problem{}",
                        errors.len(),
                        if errors.len() == 1 { "" } else { "s" }
                    ),
                    theme::FONT_BODY,
                    theme::DANGER,
                ));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .button(theme::label(
                            "Dismiss",
                            theme::FONT_SMALL,
                            theme::TEXT_MUTED,
                        ))
                        .clicked()
                    {
                        dismissed = true;
                    }
                });
            });
            ui.add_space(2.0);
            egui::ScrollArea::vertical()
                .max_height(140.0)
                .show(ui, |ui| {
                    for (index, error) in errors.iter().enumerate().rev() {
                        ui.add(
                            egui::Label::new(theme::label(
                                &error.summary,
                                theme::FONT_BODY,
                                theme::TEXT_DIM,
                            ))
                            .wrap(),
                        );
                        if let Some(detail) = &error.detail {
                            egui::CollapsingHeader::new(theme::label(
                                "Show command output",
                                theme::FONT_SMALL,
                                theme::TEXT_FAINT,
                            ))
                            .id_salt(("grove-error", index))
                            .show(ui, |ui| {
                                // git's and tmux's own output, monospaced and
                                // never abridged (ARCHITECTURE.md §8.5).
                                egui::Frame::new()
                                    .fill(theme::BG)
                                    .corner_radius(egui::CornerRadius::same(6))
                                    .inner_margin(egui::Margin::symmetric(8, 6))
                                    .show(ui, |ui| {
                                        ui.add(
                                            egui::Label::new(theme::mono(
                                                detail,
                                                theme::FONT_SMALL,
                                                theme::TEXT_MUTED,
                                            ))
                                            .wrap(),
                                        );
                                    });
                            });
                        }
                        ui.add_space(4.0);
                    }
                });
        });
    dismissed
}
