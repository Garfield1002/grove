//! One tmux window, drawn as a child row under its worktree.
//!
//! Windows are the third level of the tree: a worktree's row says which
//! session it has, and these say what is open inside it. They are only ever
//! rendered from what a poll of tmux reported — Grove never invents a window
//! that might not be there.

use egui::{Sense, Stroke, Ui, vec2};
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
pub const HEIGHT: f32 = 20.0;
/// Left inset, lining the window name up past the worktree's session dot.
const INDENT: f32 = 31.0;

/// Draw one window row. Returns what the user did on it.
pub fn show(ui: &mut Ui, window: &WindowInfo) -> Option<WindowAction> {
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(vec2(width, HEIGHT), Sense::click());
    let hovered = response.hovered();

    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        let row = rect.with_min_x(rect.left() + INDENT - 10.0);
        if hovered {
            painter.rect_filled(
                row,
                egui::CornerRadius::same(theme::ROW_RADIUS),
                theme::FIELD.gamma_multiply(0.5),
            );
        }

        // The active window is the one an attaching client lands on, so it is
        // the only one marked: a filled dot against a hollow one.
        let dot = egui::pos2(rect.left() + INDENT - 2.0, rect.center().y);
        if window.active {
            painter.circle_filled(dot, 2.5, theme::TEXT_DIM);
        } else {
            painter.circle_stroke(dot, 2.5, Stroke::new(1.0, theme::DOT_EMPTY));
        }

        let color = if window.active {
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
        painter.galley(egui::pos2(rect.left() + INDENT + 7.0, text_y), text, color);
    }

    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    if response.clicked() {
        return Some(WindowAction::Activate);
    }
    None
}
