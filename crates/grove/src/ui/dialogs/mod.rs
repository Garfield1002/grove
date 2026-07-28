//! Dialogs: open project, create worktree, safe removal, and the error area.
//!
//! The three forms are their own OS windows ([`crate::ui::chrome`]); only the
//! error area below renders inside the main window.

pub mod create_worktree;
pub mod open_project;
pub mod removal;

use egui::Ui;

use crate::workers::ErrorReport;

use super::theme;

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
