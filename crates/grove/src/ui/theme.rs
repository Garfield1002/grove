//! The dark, color-forward theme from direction 1c in `grove.dc.html`.
//!
//! Status colours (green `WORKING`, amber `ACTION`) are deliberately *not*
//! used yet: Milestone 1 has no status detection, and showing a status colour
//! that nothing computes would be a lie. They are named here so Milestone 4
//! can light them up without re-picking the palette.

use egui::{Color32, CornerRadius, Margin, Stroke, Visuals};

pub const BG: Color32 = Color32::from_rgb(0x10, 0x12, 0x17);
pub const BG_SUNKEN: Color32 = Color32::from_rgb(0x0b, 0x0d, 0x11);
pub const BG_FOOTER: Color32 = Color32::from_rgb(0x0d, 0x0f, 0x13);
pub const FIELD: Color32 = Color32::from_rgb(0x17, 0x1a, 0x20);
pub const CHIP: Color32 = Color32::from_rgb(0x1c, 0x21, 0x29);
pub const BADGE: Color32 = Color32::from_rgb(0x1a, 0x1e, 0x25);

pub const TEXT: Color32 = Color32::from_rgb(0xe6, 0xe8, 0xec);
pub const TEXT_STRONG: Color32 = Color32::from_rgb(0xee, 0xf1, 0xf4);
pub const TEXT_DIM: Color32 = Color32::from_rgb(0xc7, 0xcd, 0xd4);
pub const TEXT_MUTED: Color32 = Color32::from_rgb(0x8b, 0x92, 0x9c);
pub const TEXT_FAINT: Color32 = Color32::from_rgb(0x5c, 0x62, 0x6b);

pub const HAIRLINE: Color32 = Color32::from_rgb(0x22, 0x26, 0x2d);
pub const DOT_IDLE: Color32 = Color32::from_rgb(0x79, 0x81, 0x8c);
pub const DOT_EMPTY: Color32 = Color32::from_rgb(0x4a, 0x4f, 0x57);

/// Selection accent. Distinct from the reserved status hues below.
pub const ACCENT: Color32 = Color32::from_rgb(0x5f, 0xa8, 0xd6);
pub const ACCENT_FILL: Color32 = Color32::from_rgb(0x18, 0x26, 0x31);

pub const DANGER: Color32 = Color32::from_rgb(0xff, 0x5f, 0x57);

// Reserved by direction 1c for Milestone 4 status pills, deliberately not
// defined as constants until something computes them: working `#4bc07d`,
// attention `#e0a44a`.

pub const ROW_RADIUS: u8 = 9;
pub const CHIP_RADIUS: u8 = 8;

/// Recommended window size: a narrow vertical panel (DESIGN.md §5).
pub const WINDOW_SIZE: [f32; 2] = [360.0, 720.0];
pub const MIN_WINDOW_SIZE: [f32; 2] = [280.0, 320.0];

pub fn apply(ctx: &egui::Context) {
    let mut visuals = Visuals::dark();
    visuals.panel_fill = BG;
    visuals.window_fill = BG;
    visuals.extreme_bg_color = FIELD;
    visuals.faint_bg_color = BADGE;
    visuals.override_text_color = Some(TEXT);
    visuals.window_stroke = Stroke::new(1.0, HAIRLINE);
    visuals.selection.bg_fill = ACCENT_FILL;
    visuals.selection.stroke = Stroke::new(1.0, ACCENT);

    let radius = CornerRadius::same(CHIP_RADIUS);
    for widget in [
        &mut visuals.widgets.noninteractive,
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        widget.corner_radius = radius;
    }
    visuals.widgets.noninteractive.bg_fill = BG;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, HAIRLINE);
    visuals.widgets.inactive.bg_fill = CHIP;
    visuals.widgets.inactive.weak_bg_fill = CHIP;
    visuals.widgets.hovered.bg_fill = FIELD;
    visuals.widgets.hovered.weak_bg_fill = FIELD;
    visuals.widgets.active.bg_fill = ACCENT_FILL;
    visuals.widgets.active.weak_bg_fill = ACCENT_FILL;
    ctx.set_visuals(visuals);

    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = egui::vec2(6.0, 4.0);
    style.spacing.button_padding = egui::vec2(8.0, 4.0);
    style.spacing.window_margin = Margin::same(10);
    ctx.set_style(style);
}

/// Monospace text, as the design uses for branch names and paths.
pub fn mono(text: impl Into<String>, size: f32, color: Color32) -> egui::RichText {
    egui::RichText::new(text)
        .monospace()
        .size(size)
        .color(color)
}

pub fn label(text: impl Into<String>, size: f32, color: Color32) -> egui::RichText {
    egui::RichText::new(text).size(size).color(color)
}
