//! One worktree row: session dot, branch name, git-status sublabel, markers
//! for a locked or detached worktree, selection accent, and the context menu.

use egui::{Align, Layout, Sense, Stroke, StrokeKind, Ui, vec2};
use grove_core::model::{SessionPresence, Worktree};

use super::theme;

/// What the user did on a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowAction {
    Activate,
    Select,
    OpenInNewTerminal,
    Refresh,
    Remove,
}

/// Draw a worktree row.
pub fn show(
    ui: &mut Ui,
    worktree: &Worktree,
    selected: bool,
    home: Option<&std::path::Path>,
) -> Option<RowAction> {
    let mut action = None;
    let height = 40.0;
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(vec2(width, height), Sense::click());
    let hovered = response.hovered();

    if ui.is_rect_visible(rect) {
        let radius = egui::CornerRadius::same(theme::ROW_RADIUS);
        if selected {
            ui.painter().rect_filled(rect, radius, theme::ACCENT_FILL);
            ui.painter().rect_stroke(
                rect,
                radius,
                Stroke::new(1.0, theme::ACCENT.gamma_multiply(0.5)),
                StrokeKind::Inside,
            );
            // Accent edge, as in direction 1c.
            let edge = egui::Rect::from_min_size(rect.min, vec2(3.0, rect.height()));
            ui.painter().rect_filled(edge, radius, theme::ACCENT);
        } else if hovered {
            ui.painter()
                .rect_filled(rect, radius, theme::FIELD.gamma_multiply(0.7));
        }

        let dot_center = egui::pos2(rect.left() + 17.0, rect.center().y);
        match worktree.session {
            SessionPresence::None => {
                ui.painter()
                    .circle_stroke(dot_center, 4.0, Stroke::new(1.4, theme::DOT_EMPTY));
            }
            SessionPresence::Detached => {
                ui.painter().circle_filled(dot_center, 4.0, theme::DOT_IDLE);
            }
            SessionPresence::Attached => {
                ui.painter().circle_filled(dot_center, 4.0, theme::TEXT_DIM);
            }
        }

        // Markers on the right edge: a lock, a detached HEAD, and a dirty
        // working tree. Only ever drawn from something git actually reported.
        let mut marker_x = rect.right() - 8.0;
        for (glyph, color, _) in markers(worktree) {
            let galley = ui.painter().layout_no_wrap(
                glyph.to_string(),
                egui::FontId::proportional(10.0),
                color,
            );
            marker_x -= galley.size().x;
            ui.painter().galley(
                egui::pos2(marker_x, rect.center().y - galley.size().y / 2.0),
                galley,
                color,
            );
            marker_x -= 5.0;
        }

        let text_rect = rect
            .with_min_x(rect.left() + 29.0)
            .with_max_x(marker_x.max(rect.left() + 60.0))
            .shrink2(vec2(0.0, 5.0));
        let mut content = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(text_rect)
                .layout(Layout::top_down(Align::LEFT)),
        );
        content.spacing_mut().item_spacing.y = 1.0;

        let name_color = if selected {
            theme::TEXT_STRONG
        } else if worktree.session.exists() {
            theme::TEXT_DIM
        } else {
            theme::TEXT_MUTED
        };
        content.add(
            egui::Label::new(theme::mono(worktree.label(), 12.5, name_color))
                .truncate()
                .selectable(false),
        );
        content.add(
            egui::Label::new(theme::label(
                sublabel(worktree, home),
                9.5,
                sublabel_color(worktree),
            ))
            .truncate()
            .selectable(false),
        );
    }

    if response.clicked() {
        action = Some(RowAction::Activate);
    }

    response.context_menu(|ui| {
        if ui.button("Open or switch to session").clicked() {
            action = Some(RowAction::Activate);
            ui.close();
        }
        if ui.button("Open in a new terminal").clicked() {
            action = Some(RowAction::OpenInNewTerminal);
            ui.close();
        }
        if ui.button("Copy worktree path").clicked() {
            ui.ctx().copy_text(worktree.path.display().to_string());
            action = Some(RowAction::Select);
            ui.close();
        }
        if ui.button("Refresh").clicked() {
            action = Some(RowAction::Refresh);
            ui.close();
        }
        ui.separator();
        if ui.button("Remove…").clicked() {
            action = Some(RowAction::Remove);
            ui.close();
        }
        ui.label(theme::label(
            "Removal asks separately about the session, the worktree and the branch.",
            9.5,
            theme::TEXT_FAINT,
        ));
    });

    response.on_hover_text(hover_text(worktree));
    action
}

/// Tooltip: the full path, the git summary, and what each marker means.
fn hover_text(worktree: &Worktree) -> String {
    let mut lines = vec![worktree.path.display().to_string()];
    if let Some(status) = &worktree.git_status {
        lines.push(status.summary());
    }
    for (_, _, hint) in markers(worktree) {
        lines.push(hint.to_string());
    }
    lines.join("\n")
}

/// Right-edge markers, worst first.
fn markers(worktree: &Worktree) -> Vec<(&'static str, egui::Color32, &'static str)> {
    let mut markers = Vec::new();
    if worktree.is_locked {
        markers.push(("🔒", theme::TEXT_MUTED, "locked"));
    }
    if worktree.is_detached {
        markers.push(("⚯", theme::TEXT_MUTED, "detached HEAD"));
    }
    if worktree
        .git_status
        .as_ref()
        .is_some_and(|status| !status.is_clean())
    {
        markers.push(("●", theme::WARNING, "uncommitted changes"));
    }
    markers
}

fn sublabel_color(worktree: &Worktree) -> egui::Color32 {
    match &worktree.git_status {
        Some(status) if status.operation.is_some() => theme::WARNING,
        _ => theme::TEXT_FAINT,
    }
}

/// Sublabel: the git summary and session state, plus the abbreviated path,
/// which is what a user needs when two worktrees share a branch name.
fn sublabel(worktree: &Worktree, home: Option<&std::path::Path>) -> String {
    format!("{} · {}", worktree.sublabel(), worktree.short_path(home))
}

#[cfg(test)]
mod tests {
    use super::*;
    use grove_core::git::status::Operation;
    use grove_core::git::{StatusSummary, WorktreeEntry};
    use std::path::{Path, PathBuf};

    fn worktree() -> Worktree {
        Worktree::from_entry(
            &WorktreeEntry {
                path: PathBuf::from("/home/u/wt/auth"),
                branch: Some("feature/auth".into()),
                ..WorktreeEntry::default()
            },
            "p1",
            Path::new("/home/u/proj/.git"),
            false,
        )
    }

    #[test]
    fn a_clean_worktree_has_no_markers() {
        let mut worktree = worktree();
        worktree.git_status = Some(StatusSummary::default());
        assert!(markers(&worktree).is_empty());
    }

    #[test]
    fn an_unread_status_shows_no_dirty_marker() {
        assert!(
            markers(&worktree()).is_empty(),
            "a marker must never be drawn from a status Grove has not read"
        );
    }

    #[test]
    fn locked_detached_and_dirty_each_get_a_marker() {
        let mut worktree = worktree();
        worktree.is_locked = true;
        worktree.is_detached = true;
        worktree.git_status = Some(StatusSummary {
            modified: 1,
            ..StatusSummary::default()
        });
        let hints: Vec<&str> = markers(&worktree).iter().map(|m| m.2).collect();
        assert_eq!(
            hints,
            vec!["locked", "detached HEAD", "uncommitted changes"]
        );
    }

    #[test]
    fn an_operation_in_progress_colours_the_sublabel() {
        let mut worktree = worktree();
        assert_eq!(sublabel_color(&worktree), theme::TEXT_FAINT);
        worktree.git_status = Some(StatusSummary {
            operation: Some(Operation::Merge),
            ..StatusSummary::default()
        });
        assert_eq!(sublabel_color(&worktree), theme::WARNING);
    }

    #[test]
    fn the_sublabel_carries_the_status_the_session_and_the_path() {
        let mut worktree = worktree();
        worktree.git_status = Some(StatusSummary {
            modified: 3,
            untracked: 1,
            ..StatusSummary::default()
        });
        assert_eq!(
            sublabel(&worktree, Some(Path::new("/home/u"))),
            "3 mod · 1 untracked · no session · ~/wt/auth"
        );
    }
}
