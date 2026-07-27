//! Icons drawn as `epaint` primitives.
//!
//! **Why not glyphs.** egui's bundled `Proportional` family is
//! `Ubuntu-Light → NotoEmoji-Regular → emoji-icon-font`. Several characters
//! the mockup uses are in none of them and rendered as tofu boxes: the
//! fullwidth plus `＋` (U+FF0B) in the header, `⚯` (U+26AF) for a detached
//! HEAD, and `✕` (U+2715) in the removal dialog. Others (`●` U+25CF, `▾`
//! U+25BE, `⋯` U+22EF, `◉` U+25C9) exist only in *Hack*, which is in the
//! `Monospace` chain but **not** the `Proportional` one, so they were tofu
//! wherever the code asked for a proportional font.
//!
//! Painting them instead removes the whole class of bug, costs no font asset,
//! and lets the icons match the mockup's SVG line work exactly. The mockup's
//! icons are 24-grid strokes, so shapes here are written in unit coordinates
//! (`0.0..=1.0` over the icon's square) and mapped onto the target rect.

use egui::{Color32, Pos2, Rect, Response, Sense, Shape, Stroke, Ui, Vec2, pos2, vec2};

use super::theme;

/// Map unit coordinates onto `rect`.
fn at(rect: Rect, x: f32, y: f32) -> Pos2 {
    pos2(
        rect.left() + x * rect.width(),
        rect.top() + y * rect.height(),
    )
}

/// Stroke width that keeps the mockup's 2/24 line ratio at any icon size.
fn line_width(rect: Rect) -> f32 {
    (rect.width() * 0.085).clamp(1.0, 2.0)
}

/// A stroked, optionally closed polyline in unit coordinates.
fn path(painter: &egui::Painter, rect: Rect, pts: &[(f32, f32)], closed: bool, stroke: Stroke) {
    let points: Vec<Pos2> = pts.iter().map(|&(x, y)| at(rect, x, y)).collect();
    painter.add(if closed {
        Shape::closed_line(points, stroke)
    } else {
        Shape::line(points, stroke)
    });
}

/// Points along an arc of a circle, in unit coordinates.
fn arc(center: (f32, f32), radius: f32, from: f32, to: f32, steps: usize) -> Vec<(f32, f32)> {
    (0..=steps)
        .map(|i| {
            let t = from + (to - from) * (i as f32 / steps as f32);
            (center.0 + radius * t.cos(), center.1 + radius * t.sin())
        })
        .collect()
}

// ------------------------------------------------------------------ icons

/// `+` — the open-project action in the header.
pub fn plus(painter: &egui::Painter, rect: Rect, color: Color32) {
    let stroke = Stroke::new(line_width(rect).max(1.4), color);
    path(painter, rect, &[(0.5, 0.18), (0.5, 0.82)], false, stroke);
    path(painter, rect, &[(0.18, 0.5), (0.82, 0.5)], false, stroke);
}

/// A magnifying glass, as in the mockup's filter field.
pub fn magnifier(painter: &egui::Painter, rect: Rect, color: Color32) {
    let stroke = Stroke::new(line_width(rect), color);
    let circle = arc((0.46, 0.46), 0.29, 0.0, std::f32::consts::TAU, 24);
    path(painter, rect, &circle, true, stroke);
    path(painter, rect, &[(0.68, 0.68), (0.9, 0.9)], false, stroke);
}

/// An open folder, as on the mockup's "Open Project" footer entry.
pub fn folder(painter: &egui::Painter, rect: Rect, color: Color32) {
    let stroke = Stroke::new(line_width(rect), color);
    path(
        painter,
        rect,
        &[
            (0.12, 0.74),
            (0.12, 0.26),
            (0.2, 0.18),
            (0.38, 0.18),
            (0.46, 0.28),
            (0.88, 0.28),
            (0.88, 0.74),
            (0.8, 0.82),
            (0.2, 0.82),
        ],
        true,
        stroke,
    );
}

/// A cog, as on the mockup's settings affordance.
///
/// Drawn as one toothed silhouette rather than a circle with radial spokes:
/// spokes crossing the rim read as a ship's wheel, not a gear.
pub fn gear(painter: &egui::Painter, rect: Rect, color: Color32) {
    let stroke = Stroke::new(line_width(rect), color);
    const TEETH: usize = 7;
    const OUTER: f32 = 0.46;
    const ROOT: f32 = 0.32;
    let step = std::f32::consts::TAU / TEETH as f32;

    let mut outline = Vec::with_capacity(TEETH * 4);
    let mut push = |angle: f32, radius: f32| {
        let (sin, cos) = angle.sin_cos();
        outline.push((0.5 + radius * cos, 0.5 + radius * sin));
    };
    for i in 0..TEETH {
        let a = i as f32 * step;
        push(a - step * 0.17, OUTER);
        push(a + step * 0.17, OUTER);
        push(a + step * 0.33, ROOT);
        push(a + step * 0.67, ROOT);
    }
    path(painter, rect, &outline, true, stroke);

    let hub = arc((0.5, 0.5), 0.14, 0.0, std::f32::consts::TAU, 16);
    path(painter, rect, &hub, true, stroke);
}

/// A counter-clockwise circular arrow: refresh, and the Restore control.
pub fn refresh(painter: &egui::Painter, rect: Rect, color: Color32) {
    let stroke = Stroke::new(line_width(rect), color);
    const START: f32 = -1.9;
    const RADIUS: f32 = 0.3;
    let centre = (0.5, 0.52);
    path(
        painter,
        rect,
        &arc(centre, RADIUS, START, 3.6, 24),
        false,
        stroke,
    );

    // The head sits at the open end, pointing along the tangent the sweep
    // would continue in if it kept going backwards.
    let (sin, cos) = START.sin_cos();
    let tip = (centre.0 + RADIUS * cos, centre.1 + RADIUS * sin);
    let heading = (sin, -cos);
    let leg = |turn: f32| {
        let (s, c) = turn.sin_cos();
        (
            tip.0 - 0.2 * (heading.0 * c - heading.1 * s),
            tip.1 - 0.2 * (heading.0 * s + heading.1 * c),
        )
    };
    path(painter, rect, &[leg(0.5), tip, leg(-0.5)], false, stroke);
}

/// Three dots: the "more actions" affordance on a project row.
pub fn ellipsis(painter: &egui::Painter, rect: Rect, color: Color32) {
    let r = (rect.width() * 0.075).max(1.0);
    for x in [0.24, 0.5, 0.76] {
        painter.circle_filled(at(rect, x, 0.5), r, color);
    }
}

/// A downward caret for a menu that opens below.
pub fn caret_down(painter: &egui::Painter, rect: Rect, color: Color32) {
    let stroke = Stroke::new(line_width(rect).max(1.3), color);
    path(
        painter,
        rect,
        &[(0.26, 0.4), (0.5, 0.63), (0.74, 0.4)],
        false,
        stroke,
    );
}

/// The project expand/collapse triangle. `openness` is 0 (collapsed, pointing
/// right) to 1 (expanded, pointing down), so the rotation can be animated.
pub fn disclosure(painter: &egui::Painter, rect: Rect, openness: f32, color: Color32) {
    let center = rect.center();
    let size = rect.width().min(rect.height()) * 0.5;
    let angle = openness * std::f32::consts::FRAC_PI_2;
    let (sin, cos) = angle.sin_cos();
    // A right-pointing triangle in local space, rotated by `angle`.
    let points = [(-0.32, -0.6), (0.6, 0.0), (-0.32, 0.6)]
        .into_iter()
        .map(|(x, y)| center + vec2((x * cos - y * sin) * size, (x * sin + y * cos) * size))
        .collect();
    painter.add(Shape::convex_polygon(points, color, Stroke::NONE));
}

/// A padlock: the marker for a locked worktree.
pub fn lock(painter: &egui::Painter, rect: Rect, color: Color32) {
    let stroke = Stroke::new(line_width(rect).max(1.1), color);
    let shackle = arc(
        (0.5, 0.46),
        0.22,
        std::f32::consts::PI,
        std::f32::consts::TAU,
        12,
    );
    path(painter, rect, &shackle, false, stroke);
    let body = Rect::from_min_max(at(rect, 0.2, 0.46), at(rect, 0.8, 0.9));
    painter.rect_filled(body, egui::CornerRadius::same(1), color);
}

/// The marker for a detached HEAD: a commit sitting off the branch line. A
/// broken-chain drawing turns to mush at 11 px; this stays readable.
pub fn unlink(painter: &egui::Painter, rect: Rect, color: Color32) {
    let stroke = Stroke::new(line_width(rect).max(1.2), color);
    path(painter, rect, &[(0.22, 0.08), (0.22, 0.92)], false, stroke);
    let commit = arc((0.7, 0.5), 0.22, 0.0, std::f32::consts::TAU, 16);
    path(painter, rect, &commit, true, stroke);
}

/// A cross: the blocker bullet in the removal dialog.
pub fn cross(painter: &egui::Painter, rect: Rect, color: Color32) {
    let stroke = Stroke::new(line_width(rect).max(1.2), color);
    path(painter, rect, &[(0.22, 0.22), (0.78, 0.78)], false, stroke);
    path(painter, rect, &[(0.78, 0.22), (0.22, 0.78)], false, stroke);
}

/// A tick: "this executable was found on PATH", in the settings pane.
pub fn check(painter: &egui::Painter, rect: Rect, color: Color32) {
    let stroke = Stroke::new(line_width(rect).max(1.2), color);
    path(
        painter,
        rect,
        &[(0.18, 0.52), (0.42, 0.76), (0.84, 0.24)],
        false,
        stroke,
    );
}

/// A warning triangle: the warning bullet in the removal dialog.
pub fn warning(painter: &egui::Painter, rect: Rect, color: Color32) {
    let stroke = Stroke::new(line_width(rect).max(1.1), color);
    path(
        painter,
        rect,
        &[(0.5, 0.12), (0.94, 0.86), (0.06, 0.86)],
        true,
        stroke,
    );
    path(painter, rect, &[(0.5, 0.4), (0.5, 0.62)], false, stroke);
    painter.circle_filled(at(rect, 0.5, 0.75), stroke.width * 0.6, color);
}

/// The attention bang from DESIGN.md §6: a stroke over a dot, sized to sit in
/// a row's session-dot slot.
///
/// Drawn rather than typed: egui's bundled fonts have no glyph for most of the
/// marks the mockup uses, and a missing glyph renders as tofu.
pub fn bang(painter: &egui::Painter, rect: Rect, color: Color32) {
    let stroke = Stroke::new(line_width(rect).max(1.4), color);
    path(painter, rect, &[(0.5, 0.06), (0.5, 0.58)], false, stroke);
    painter.circle_filled(at(rect, 0.5, 0.87), stroke.width * 0.72, color);
}

/// A processor die with pins, labelling the resource figures on a row.
///
/// Named `cpu` rather than `chip` because `chip` is already the header's
/// pill-shaped button widget below.
///
/// Drawn small (around 9 px), so the pins are single strokes and the die is a
/// plain square — anything more detailed turns to mush at this size.
pub fn cpu(painter: &egui::Painter, rect: Rect, color: Color32) {
    let stroke = Stroke::new(line_width(rect).max(1.0), color);
    // The die, inset to leave room for the pins on all four sides.
    path(
        painter,
        rect,
        &[(0.26, 0.26), (0.74, 0.26), (0.74, 0.74), (0.26, 0.74)],
        true,
        stroke,
    );
    // Two pins per side, at the thirds.
    for t in [0.42, 0.58] {
        path(painter, rect, &[(t, 0.06), (t, 0.26)], false, stroke);
        path(painter, rect, &[(t, 0.74), (t, 0.94)], false, stroke);
        path(painter, rect, &[(0.06, t), (0.26, t)], false, stroke);
        path(painter, rect, &[(0.74, t), (0.94, t)], false, stroke);
    }
}

// ---------------------------------------------------------------- widgets

/// A square icon button with the mockup's chip background.
///
/// `draw` receives the inner rect, already inset from the chip.
pub fn button(
    ui: &mut Ui,
    enabled: bool,
    draw: impl FnOnce(&egui::Painter, Rect, Color32),
) -> Response {
    let size = Vec2::splat(theme::ICON_BUTTON);
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(size, sense);
    let response = response.on_hover_cursor(if enabled {
        egui::CursorIcon::PointingHand
    } else {
        egui::CursorIcon::Default
    });

    if ui.is_rect_visible(rect) {
        let (fill, tint) = match (enabled, response.hovered()) {
            (false, _) => (theme::CHIP.gamma_multiply(0.5), theme::TEXT_FAINT),
            (true, false) => (theme::CHIP, theme::TEXT_DIM),
            (true, true) => (theme::HAIRLINE, theme::TEXT_STRONG),
        };
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(theme::CHIP_RADIUS), fill);
        draw(ui.painter(), rect.shrink(theme::ICON_BUTTON * 0.28), tint);
    }
    response
}

/// A labelled chip: an icon and a word, as the mockup's Restore control.
pub fn chip(
    ui: &mut Ui,
    text: &str,
    enabled: bool,
    draw: impl FnOnce(&egui::Painter, Rect, Color32),
) -> Response {
    let font = egui::FontId::proportional(theme::FONT_CHIP);
    let galley = ui.painter().layout_no_wrap(
        text.to_owned(),
        font,
        if enabled {
            theme::TEXT_DIM
        } else {
            theme::TEXT_FAINT
        },
    );
    let height = theme::ICON_BUTTON;
    let width = 9.0 + 12.0 + 5.0 + galley.size().x + 9.0;
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(vec2(width, height), sense);

    if ui.is_rect_visible(rect) {
        let (fill, tint) = match (enabled, response.hovered()) {
            (false, _) => (theme::CHIP.gamma_multiply(0.5), theme::TEXT_FAINT),
            (true, false) => (theme::CHIP, theme::TEXT_DIM),
            (true, true) => (theme::HAIRLINE, theme::TEXT_STRONG),
        };
        let painter = ui.painter();
        painter.rect_filled(rect, egui::CornerRadius::same(theme::CHIP_RADIUS), fill);
        let icon = Rect::from_center_size(
            pos2(rect.left() + 9.0 + 6.0, rect.center().y),
            Vec2::splat(12.0),
        );
        draw(painter, icon, tint);
        painter.galley(
            pos2(icon.right() + 5.0, rect.center().y - galley.size().y / 2.0),
            galley,
            tint,
        );
    }
    response
}
