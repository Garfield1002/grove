//! One tmux window, drawn as a child row under its worktree.
//!
//! Windows are the third level of the tree: a worktree's row says which
//! session it has, and these say what is open inside it. They are only ever
//! rendered from what a poll of tmux reported — Grove never invents a window
//! that might not be there.

use egui::{Sense, Stroke, StrokeKind, Ui, vec2};
use grove_core::tmux::WindowInfo;

use super::theme;

/// What the user did on a window row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowAction {
    /// Open this window: select it, then switch the attached client or launch
    /// a terminal — the same choice a worktree row makes.
    Activate,
}

/// Height of a child row: shorter than a worktree row, which is what makes the
/// worktree read as the parent of the group.
pub const HEIGHT: f32 = 24.0;
/// Left inset, lining the window name up past the worktree's session dot.
const INDENT: f32 = 31.0;

/// What a row's border says about its window, worst-first.
///
/// A bell outranks selection because it is the only one the user has not just
/// caused themselves; selection outranks *current* because the user's own
/// focus should be findable in a list where every session has a current window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Border {
    /// tmux rang a bell here: this window wants the user.
    Bell,
    Selected,
    /// The session's current window, where an attaching client lands.
    Current,
    None,
}

impl Border {
    fn of(window: &WindowInfo, selected: bool) -> Self {
        if window.bell {
            Border::Bell
        } else if selected {
            Border::Selected
        } else if window.active {
            Border::Current
        } else {
            Border::None
        }
    }

    fn color(self) -> Option<egui::Color32> {
        match self {
            Border::Bell => Some(theme::STATUS_ATTENTION),
            Border::Selected => Some(theme::ACCENT),
            Border::Current => Some(theme::STATUS_WORKING),
            Border::None => None,
        }
    }
}

/// Draw one window row. Returns what the user did on it.
pub fn show(ui: &mut Ui, window: &WindowInfo, selected: bool) -> Option<WindowAction> {
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(vec2(width, HEIGHT), Sense::click());
    let hovered = response.hovered();

    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        let row = rect
            .with_min_x(rect.left() + INDENT - 12.0)
            .shrink2(vec2(0.0, 1.0));
        let radius = theme::WINDOW_ROW_RADIUS;

        // The selected row is filled with the accent gradient, running up from
        // its bottom-left corner; every other row keeps the panel behind it.
        if selected {
            theme::diagonal_gradient(
                painter,
                row,
                radius as f32,
                theme::ACCENT_FILL,
                theme::ACCENT.gamma_multiply(0.35),
            );
        } else if hovered {
            painter.rect_filled(
                row,
                egui::CornerRadius::same(radius),
                theme::FIELD.gamma_multiply(0.5),
            );
        }

        if let Some(color) = Border::of(window, selected).color() {
            painter.rect_stroke(
                row,
                egui::CornerRadius::same(radius),
                Stroke::new(1.0, color),
                StrokeKind::Inside,
            );
        }

        let color = if selected || window.active {
            theme::TEXT_DIM
        } else {
            theme::TEXT_MUTED
        };
        let text = painter.layout_no_wrap(
            format!("{}: {}", window.index, window.name),
            egui::FontId::monospace(theme::FONT_SUB),
            color,
        );
        let text_y = rect.center().y - text.size().y / 2.0;
        painter.galley(egui::pos2(row.left() + 10.0, text_y), text, color);
    }

    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    if response.clicked() {
        return Some(WindowAction::Activate);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(active: bool, bell: bool) -> WindowInfo {
        WindowInfo {
            session: "wt-a1b2c3".into(),
            index: 1,
            name: "shell".into(),
            active,
            bell,
        }
    }

    #[test]
    fn a_bell_outranks_selection_and_the_current_window() {
        assert_eq!(Border::of(&window(true, true), true), Border::Bell);
        assert_eq!(
            Border::of(&window(true, true), false).color(),
            Some(theme::STATUS_ATTENTION)
        );
    }

    #[test]
    fn selection_outranks_the_current_window() {
        assert_eq!(Border::of(&window(true, false), true), Border::Selected);
        assert_eq!(
            Border::of(&window(true, false), true).color(),
            Some(theme::ACCENT)
        );
    }

    #[test]
    fn the_current_window_is_green_and_the_rest_are_unbordered() {
        assert_eq!(
            Border::of(&window(true, false), false).color(),
            Some(theme::STATUS_WORKING)
        );
        assert_eq!(Border::of(&window(false, false), false).color(), None);
    }
}
