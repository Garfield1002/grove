//! One worktree row, laid out as in direction 1c: a left accent edge, a
//! session dot, the branch name over a muted sublabel, and quiet right-aligned
//! markers for a locked or detached worktree and a dirty working tree.

use egui::{Align, Layout, Sense, Stroke, StrokeKind, Ui, vec2};
use grove_core::model::{SessionPresence, Worktree};
use grove_core::status::SessionStatus;

use super::{icons, theme};

/// What the user did on a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowAction {
    Activate,
    Select,
    OpenInNewTerminal,
    StartAgent,
    Refresh,
    Remove,
}

/// A quiet right-edge marker. Only ever built from something git reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Marker {
    Locked,
    Detached,
    Dirty,
}

impl Marker {
    fn hint(self) -> &'static str {
        match self {
            Marker::Locked => "locked",
            Marker::Detached => "detached HEAD",
            Marker::Dirty => "uncommitted changes",
        }
    }

    fn color(self) -> egui::Color32 {
        match self {
            Marker::Locked | Marker::Detached => theme::TEXT_MUTED,
            Marker::Dirty => theme::WARNING,
        }
    }

    fn draw(self, painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
        match self {
            Marker::Locked => icons::lock(painter, rect, color),
            Marker::Detached => icons::unlink(painter, rect, color),
            Marker::Dirty => {
                painter.circle_filled(rect.center(), rect.width() * 0.26, color);
            }
        }
    }
}

/// At most three markers, held inline so drawing a row allocates nothing.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Markers {
    items: [Option<Marker>; 3],
}

impl Markers {
    fn push(&mut self, marker: Marker) {
        if let Some(slot) = self.items.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some(marker);
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = Marker> + '_ {
        self.items.iter().flatten().copied()
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.items.iter().all(Option::is_none)
    }
}

const MARKER_SIZE: f32 = 11.0;

/// Draw a worktree row.
pub fn show(
    ui: &mut Ui,
    worktree: &Worktree,
    selected: bool,
    home: Option<&std::path::Path>,
) -> Option<RowAction> {
    let mut action = None;
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(vec2(width, theme::ROW_HEIGHT), Sense::click());
    let hovered = response.hovered();

    // Right edge of the text column, moved left by whatever markers exist.
    let mut marker_x = rect.right() - 10.0;

    if ui.is_rect_visible(rect) {
        let radius = egui::CornerRadius::same(theme::ROW_RADIUS);
        let painter = ui.painter();
        if selected {
            painter.rect_filled(rect, radius, theme::ACCENT_FILL);
            painter.rect_stroke(
                rect,
                radius,
                Stroke::new(1.0, theme::ACCENT.gamma_multiply(0.5)),
                StrokeKind::Inside,
            );
        } else if hovered {
            painter.rect_filled(rect, radius, theme::FIELD.gamma_multiply(0.7));
        }

        // The accent edge slot from direction 1c. It carries the status once
        // there is one, and selection otherwise.
        let edge_color = match (status_color(worktree), selected, hovered) {
            (Some(status), _, _) => Some(status),
            (None, true, _) => Some(theme::ACCENT),
            (None, false, true) => Some(theme::HAIRLINE),
            (None, false, false) => None,
        };
        if let Some(color) = edge_color {
            let edge = egui::Rect::from_min_size(rect.min, vec2(theme::ROW_EDGE, rect.height()));
            painter.rect_filled(edge, radius, color);
        }

        let dot_center = egui::pos2(rect.left() + 18.0, rect.center().y);
        match (worktree.session, worktree.status) {
            // Attention gets its own mark, not just a colour: it is the one
            // state the user has to act on, and colour alone is not enough.
            (session, Some(SessionStatus::Attention)) if session.exists() => icons::bang(
                painter,
                egui::Rect::from_center_size(dot_center, egui::Vec2::splat(11.0)),
                theme::STATUS_ATTENTION,
            ),
            (session, Some(SessionStatus::Working)) if session.exists() => {
                painter.circle_filled(dot_center, 4.0, theme::STATUS_WORKING);
            }
            (SessionPresence::None, _) => {
                painter.circle_stroke(dot_center, 4.0, Stroke::new(1.4, theme::DOT_EMPTY));
            }
            (SessionPresence::Detached, _) => {
                painter.circle_filled(dot_center, 4.0, theme::DOT_IDLE);
            }
            (SessionPresence::Attached, _) => {
                painter.circle_filled(dot_center, 4.0, theme::TEXT_DIM);
            }
        }

        for marker in markers(worktree).iter() {
            marker_x -= MARKER_SIZE;
            marker.draw(
                painter,
                egui::Rect::from_center_size(
                    egui::pos2(marker_x + MARKER_SIZE / 2.0, rect.center().y),
                    egui::Vec2::splat(MARKER_SIZE),
                ),
                marker.color(),
            );
            marker_x -= 4.0;
        }

        let text_rect = rect
            .with_min_x(rect.left() + 31.0)
            .with_max_x(marker_x.max(rect.left() + 60.0))
            .shrink2(vec2(0.0, 6.0));
        let mut content = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(text_rect)
                .layout(Layout::top_down(Align::LEFT)),
        );
        content.spacing_mut().item_spacing.y = 2.0;

        let name_color = if selected {
            theme::TEXT_STRONG
        } else if worktree.session.exists() {
            theme::TEXT_DIM
        } else {
            theme::TEXT_MUTED
        };
        content.add(
            egui::Label::new(theme::mono(
                worktree.label(),
                theme::FONT_BRANCH,
                name_color,
            ))
            .truncate()
            .selectable(false),
        );
        content.add(
            egui::Label::new(theme::label(
                sublabel(worktree, home),
                theme::FONT_SUB,
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
        if ui.button("Start agent").clicked() {
            action = Some(RowAction::StartAgent);
            ui.close();
        }
        if ui.button("Refresh").clicked() {
            action = Some(RowAction::Refresh);
            ui.close();
        }
        ui.separator();
        if ui
            .button(theme::label("Remove…", theme::FONT_BODY, theme::DANGER))
            .clicked()
        {
            action = Some(RowAction::Remove);
            ui.close();
        }
        ui.label(theme::label(
            "Removal asks separately about the session, the worktree and the branch.",
            theme::FONT_SUB,
            theme::TEXT_FAINT,
        ));
    });

    // Built lazily: the tooltip allocates, and most rows are never hovered.
    response.on_hover_ui(|ui| {
        ui.add(
            egui::Label::new(theme::mono(
                worktree.path.display().to_string(),
                theme::FONT_SMALL,
                theme::TEXT_DIM,
            ))
            .wrap(),
        );
        for line in hover_lines(worktree) {
            ui.label(theme::label(line, theme::FONT_SMALL, theme::TEXT_MUTED));
        }
    });
    action
}

/// Tooltip lines after the path: the git summary and what each marker means.
fn hover_lines(worktree: &Worktree) -> Vec<String> {
    let mut lines = Vec::new();
    // What an agent said about itself comes first: it is the only line here
    // the user could not have worked out from the row.
    if let Some(message) = worktree
        .status_message
        .as_deref()
        .filter(|_| worktree.session.exists())
    {
        lines.push(message.to_string());
    }
    if let Some(status) = &worktree.git_status {
        lines.push(status.summary());
    }
    for marker in markers(worktree).iter() {
        lines.push(marker.hint().to_string());
    }
    lines
}

/// Right-edge markers, worst first.
fn markers(worktree: &Worktree) -> Markers {
    let mut markers = Markers::default();
    if worktree.is_locked {
        markers.push(Marker::Locked);
    }
    if worktree.is_detached {
        markers.push(Marker::Detached);
    }
    if worktree
        .git_status
        .as_ref()
        .is_some_and(|status| !status.is_clean())
    {
        markers.push(Marker::Dirty);
    }
    markers
}

/// The accent colour a row's status earns, if any.
///
/// Idle earns none: an idle session is the resting state, and colouring every
/// resting row would leave nothing for the two that matter to stand out from.
fn status_color(worktree: &Worktree) -> Option<egui::Color32> {
    if !worktree.session.exists() {
        return None;
    }
    match worktree.status? {
        SessionStatus::Attention => Some(theme::STATUS_ATTENTION),
        SessionStatus::Working => Some(theme::STATUS_WORKING),
        SessionStatus::Idle => None,
    }
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
    fn only_working_and_attention_colour_the_row() {
        let mut worktree = worktree();
        worktree.session = SessionPresence::Detached;

        worktree.status = Some(SessionStatus::Attention);
        assert_eq!(status_color(&worktree), Some(theme::STATUS_ATTENTION));

        worktree.status = Some(SessionStatus::Working);
        assert_eq!(status_color(&worktree), Some(theme::STATUS_WORKING));

        worktree.status = Some(SessionStatus::Idle);
        assert_eq!(
            status_color(&worktree),
            None,
            "idle is the resting state and earns no accent"
        );

        worktree.status = None;
        assert_eq!(status_color(&worktree), None);
    }

    #[test]
    fn a_worktree_with_no_session_is_never_coloured_by_status() {
        let mut worktree = worktree();
        worktree.session = SessionPresence::None;
        worktree.status = Some(SessionStatus::Attention);
        assert_eq!(status_color(&worktree), None);
    }

    #[test]
    fn an_agents_message_leads_the_tooltip() {
        let mut worktree = worktree();
        worktree.session = SessionPresence::Detached;
        worktree.status_message = Some("needs permission to run tests".into());
        worktree.git_status = Some(StatusSummary::default());
        assert_eq!(
            hover_lines(&worktree).first().map(String::as_str),
            Some("needs permission to run tests")
        );
    }

    #[test]
    fn a_message_without_a_session_is_not_shown() {
        let mut worktree = worktree();
        worktree.session = SessionPresence::None;
        worktree.status_message = Some("stale".into());
        assert!(!hover_lines(&worktree).iter().any(|l| l == "stale"));
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
        let hints: Vec<&str> = markers(&worktree).iter().map(Marker::hint).collect();
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

    /// Markers are drawn as vector shapes, never as font glyphs: the ones the
    /// design called for (`⚯`, `●`) are not in egui's proportional font chain.
    #[test]
    fn markers_fit_inline_and_stay_ordered() {
        let mut worktree = worktree();
        worktree.is_locked = true;
        worktree.git_status = Some(StatusSummary {
            modified: 1,
            ..StatusSummary::default()
        });
        let list: Vec<Marker> = markers(&worktree).iter().collect();
        assert_eq!(list, vec![Marker::Locked, Marker::Dirty]);
    }
}
