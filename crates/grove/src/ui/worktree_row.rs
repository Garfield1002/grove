//! One worktree row: session dot, branch name, sublabel, selection accent.

use egui::{Align, Layout, Sense, Stroke, StrokeKind, Ui, vec2};
use grove_core::model::{SessionPresence, Worktree};

use super::theme;

/// Draw a worktree row. Returns true when the user activated it.
pub fn show(
    ui: &mut Ui,
    worktree: &Worktree,
    selected: bool,
    home: Option<&std::path::Path>,
) -> bool {
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

        let text_rect = rect
            .with_min_x(rect.left() + 29.0)
            .with_max_x(rect.right() - 8.0)
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
                theme::TEXT_FAINT,
            ))
            .truncate()
            .selectable(false),
        );
    }

    response
        .on_hover_text(worktree.path.display().to_string())
        .clicked()
}

/// Sublabel: session state plus the abbreviated path, which is what a user
/// needs when two worktrees share a branch name.
fn sublabel(worktree: &Worktree, home: Option<&std::path::Path>) -> String {
    format!("{} · {}", worktree.sublabel(), worktree.short_path(home))
}
