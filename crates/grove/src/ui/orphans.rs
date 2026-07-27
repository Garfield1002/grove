//! Orphaned sessions: tmux sessions on the private server with no worktree
//! behind them (DESIGN.md §11).
//!
//! The section only ever *offers* the four choices — open, associate, close,
//! ignore. Reconciliation itself has already decided nothing: every session
//! listed here is still running exactly as it was.

use egui::{Sense, Ui, vec2};
use grove_core::model::Project;
use grove_core::reconcile::OrphanSession;

use super::{icons, theme};

/// What the user chose for one orphaned session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrphanAction {
    /// Look at the session before deciding anything about it.
    Open(String),
    /// Adopt it as a worktree's session.
    Associate {
        session: String,
        project_id: String,
        worktree_id: String,
    },
    /// Close it. The first click only arms the confirmation.
    Close(String),
    /// Stop reporting it. Nothing is closed.
    Ignore(String),
    /// Report the ignored sessions again.
    ShowIgnored,
}

/// Draw the orphaned-session section. Returns nothing when there is neither an
/// orphan nor an ignored session to mention.
pub fn show(
    ui: &mut Ui,
    orphans: &[OrphanSession],
    ignored: usize,
    armed: Option<&str>,
    projects: &[Project],
) -> Option<OrphanAction> {
    if orphans.is_empty() && ignored == 0 {
        return None;
    }
    let mut action = None;

    ui.add_space(6.0);
    ui.horizontal(|ui| {
        let (icon, _) = ui.allocate_exact_size(egui::Vec2::splat(12.0), Sense::hover());
        icons::warning(ui.painter(), icon, theme::WARNING);
        ui.add_space(4.0);
        ui.label(theme::label(
            format!(
                "{} orphaned session{}",
                orphans.len(),
                if orphans.len() == 1 { "" } else { "s" }
            ),
            theme::FONT_BODY,
            theme::TEXT_DIM,
        ));
    });
    if !orphans.is_empty() {
        ui.label(theme::label(
            "Running on Grove's tmux server with no worktree. Nothing has been closed.",
            theme::FONT_SMALL,
            theme::TEXT_FAINT,
        ));
    }

    for orphan in orphans {
        if let Some(chosen) = row(ui, orphan, armed == Some(orphan.name.as_str()), projects) {
            action = Some(chosen);
        }
    }

    if ignored > 0 {
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.label(theme::label(
                format!("{ignored} ignored"),
                theme::FONT_SMALL,
                theme::TEXT_FAINT,
            ));
            if ui
                .button(theme::label(
                    "Show again",
                    theme::FONT_SMALL,
                    theme::TEXT_MUTED,
                ))
                .clicked()
            {
                action = Some(OrphanAction::ShowIgnored);
            }
        });
    }
    ui.add_space(8.0);
    action
}

/// One orphan: its session name over what is known about it, and an ellipsis
/// menu with the four choices.
fn row(
    ui: &mut Ui,
    orphan: &OrphanSession,
    armed: bool,
    projects: &[Project],
) -> Option<OrphanAction> {
    let mut action = None;
    let (rect, response) = ui.allocate_exact_size(
        vec2(ui.available_width(), theme::ROW_HEIGHT),
        Sense::click(),
    );

    let more_rect = egui::Rect::from_center_size(
        egui::pos2(rect.right() - 11.0, rect.center().y),
        egui::Vec2::splat(18.0),
    );
    let more = ui.interact(
        more_rect,
        ui.id().with(("grove-orphan-more", &orphan.name)),
        Sense::click(),
    );

    if ui.is_rect_visible(rect) {
        let radius = egui::CornerRadius::same(theme::ROW_RADIUS);
        let painter = ui.painter();
        if response.hovered() || more.hovered() {
            painter.rect_filled(rect, radius, theme::FIELD.gamma_multiply(0.7));
        }
        // The same accent-edge slot the worktree rows use, in the warning
        // colour: this row needs a decision, but it is not an error.
        painter.rect_filled(
            egui::Rect::from_min_size(rect.min, vec2(theme::ROW_EDGE, rect.height())),
            radius,
            theme::WARNING,
        );

        let name = painter.layout_no_wrap(
            orphan.name.clone(),
            egui::FontId::monospace(theme::FONT_BRANCH),
            theme::TEXT_DIM,
        );
        let left = rect.left() + 14.0;
        painter.galley(egui::pos2(left, rect.top() + 7.0), name, theme::TEXT_DIM);
        let detail = painter.layout(
            orphan.detail(),
            egui::FontId::proportional(theme::FONT_SUB),
            theme::TEXT_FAINT,
            (more_rect.left() - left).max(60.0),
        );
        painter.galley(
            egui::pos2(left, rect.top() + 23.0),
            detail,
            theme::TEXT_FAINT,
        );
        icons::ellipsis(
            painter,
            more_rect.shrink(4.0),
            if more.hovered() {
                theme::TEXT_DIM
            } else {
                theme::TEXT_FAINT
            },
        );
    }

    let menu = |ui: &mut Ui, action: &mut Option<OrphanAction>| {
        if ui.button("Open session").clicked() {
            *action = Some(OrphanAction::Open(orphan.name.clone()));
            ui.close();
        }
        ui.menu_button("Associate with worktree", |ui| {
            let mut offered = false;
            for project in projects {
                for worktree in &project.worktrees {
                    // Only a worktree with no session of its own: adopting
                    // over a live session would leave two sessions claiming
                    // one name, which tmux refuses anyway.
                    if worktree.session.exists() || worktree.is_missing {
                        continue;
                    }
                    offered = true;
                    if ui
                        .button(format!("{} · {}", project.name, worktree.label()))
                        .clicked()
                    {
                        *action = Some(OrphanAction::Associate {
                            session: orphan.name.clone(),
                            project_id: project.id.clone(),
                            worktree_id: worktree.id.clone(),
                        });
                        ui.close();
                    }
                }
            }
            if !offered {
                ui.label(theme::label(
                    "Every worktree already has a session.",
                    theme::FONT_SMALL,
                    theme::TEXT_FAINT,
                ));
            }
        });
        if ui.button("Ignore").clicked() {
            *action = Some(OrphanAction::Ignore(orphan.name.clone()));
            ui.close();
        }
        ui.separator();
        let label = if armed {
            format!("Confirm: close {}", orphan.name)
        } else {
            "Close session…".to_string()
        };
        if ui
            .button(theme::label(label, theme::FONT_BODY, theme::DANGER))
            .clicked()
        {
            *action = Some(OrphanAction::Close(orphan.name.clone()));
            ui.close();
        }
        ui.label(theme::label(
            "Closing ends the session and its processes. No worktree or branch is touched.",
            theme::FONT_SUB,
            theme::TEXT_FAINT,
        ));
    };

    response.context_menu(|ui| menu(ui, &mut action));
    let more = more.on_hover_cursor(egui::CursorIcon::PointingHand);
    egui::Popup::menu(&more).show(|ui| menu(ui, &mut action));
    action
}
