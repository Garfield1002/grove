//! The dark, color-forward theme from direction 1c in `grove.dc.html`.
//!
//! Every colour here is lifted from the mockup's inline CSS, so the app and
//! the design document cannot drift apart. Nothing outside this module should
//! contain a `Color32::from_rgb` literal.
//!
//! Status colours (green `WORKING`, amber `ACTION`) are deliberately *not*
//! used for live state yet: Milestone 1–2 have no status detection, and
//! showing a status colour that nothing computes would be a lie. They are
//! named here so Milestone 4 can light them up without re-picking the palette.

use egui::{Color32, CornerRadius, Margin, Stroke, Visuals};

// ---------------------------------------------------------------- surfaces

/// Window body — mockup `#101217`.
pub const BG: Color32 = Color32::from_rgb(0x10, 0x12, 0x17);
/// Header strip — mockup `#0b0d11`.
pub const BG_SUNKEN: Color32 = Color32::from_rgb(0x0b, 0x0d, 0x11);
/// Footer strip — mockup `#0d0f13`.
pub const BG_FOOTER: Color32 = Color32::from_rgb(0x0d, 0x0f, 0x13);
/// Filter field and other sunken inputs — mockup `#171a20`.
pub const FIELD: Color32 = Color32::from_rgb(0x17, 0x1a, 0x20);
/// Header chips and icon buttons — mockup `#1c2129`.
pub const CHIP: Color32 = Color32::from_rgb(0x1c, 0x21, 0x29);
/// Worktree-count pill on a project row — mockup `#1a1e25`.
pub const BADGE: Color32 = Color32::from_rgb(0x1a, 0x1e, 0x25);
/// Dialog surface: one step above [`BG`] so a window reads as floating.
pub const BG_RAISED: Color32 = Color32::from_rgb(0x14, 0x16, 0x1c);

/// The mockup's `rgba(255,255,255,.06)` divider over [`BG`].
pub const HAIRLINE: Color32 = Color32::from_rgb(0x22, 0x26, 0x2d);
/// The subtler `rgba(255,255,255,.05)` border used on the filter field.
pub const BORDER: Color32 = Color32::from_rgb(0x1e, 0x21, 0x27);

// -------------------------------------------------------------------- text

pub const TEXT: Color32 = Color32::from_rgb(0xe6, 0xe8, 0xec);
pub const TEXT_STRONG: Color32 = Color32::from_rgb(0xee, 0xf1, 0xf4);
pub const TEXT_DIM: Color32 = Color32::from_rgb(0xc7, 0xcd, 0xd4);
pub const TEXT_MUTED: Color32 = Color32::from_rgb(0x8b, 0x92, 0x9c);
/// Quiet right-hand metadata — mockup `#6b7078`.
pub const TEXT_GHOST: Color32 = Color32::from_rgb(0x6b, 0x70, 0x78);
pub const TEXT_FAINT: Color32 = Color32::from_rgb(0x5c, 0x62, 0x6b);

/// The decorative line-art backdrop under a short list. Grove's quiet grey,
/// the same hue as [`TEXT_MUTED`]; the shipped PNG is white and is tinted
/// with this.
pub const BACKDROP: Color32 = TEXT_MUTED;
/// How strong the backdrop gets at its bottom edge, before the fade. Kept
/// separate from [`BACKDROP`] because a premultiplied `Color32` would give
/// the tint the wrong channel values.
pub const BACKDROP_ALPHA: u8 = 0xc8;

// ----------------------------------------------------------------- accents

pub const DOT_IDLE: Color32 = Color32::from_rgb(0x79, 0x81, 0x8c);
pub const DOT_EMPTY: Color32 = Color32::from_rgb(0x4a, 0x4f, 0x57);

/// Selection accent. Direction 1c tints the selected row with the *status*
/// colour, which Grove cannot know before Milestone 4, so selection borrows
/// direction 1a's blue `#4a90d9` (lightened slightly for contrast on `#101217`).
pub const ACCENT: Color32 = Color32::from_rgb(0x5f, 0xa8, 0xd6);
pub const ACCENT_FILL: Color32 = Color32::from_rgb(0x18, 0x26, 0x31);

pub const DANGER: Color32 = Color32::from_rgb(0xff, 0x5f, 0x57);
/// A destructive button's fill: dark enough to read white-on-red badly, so
/// the label is drawn in [`DANGER`] over it.
pub const DANGER_FILL: Color32 = Color32::from_rgb(0x2e, 0x14, 0x14);
/// Direction 1c's amber. Used for the risks in the removal dialog and the
/// dirty marker on a row.
pub const WARNING: Color32 = Color32::from_rgb(0xe0, 0xa4, 0x4a);

/// The `WORKING` state from the mockup (`#4bc07d`).
pub const STATUS_WORKING: Color32 = Color32::from_rgb(0x4b, 0xc0, 0x7d);
/// The `ACTION` state — the same amber as [`WARNING`].
pub const STATUS_ATTENTION: Color32 = WARNING;
// There is deliberately no STATUS_IDLE: idle is the resting state and keeps
// the neutral DOT_IDLE, so the two states that matter can stand out.

// ---------------------------------------------------------------- geometry

pub const ROW_RADIUS: u8 = 9;
pub const CHIP_RADIUS: u8 = 8;
pub const BADGE_RADIUS: u8 = 10;

/// Height of a worktree row: mockup `10px` padding around a 12.5/9.5 pair.
pub const ROW_HEIGHT: f32 = 42.0;
/// Height of a project header row.
pub const PROJECT_ROW_HEIGHT: f32 = 28.0;
/// Width of the left accent edge on a row (mockup `width:3px`).
pub const ROW_EDGE: f32 = 3.0;
/// Square side of a header/footer icon button (mockup `26px`).
pub const ICON_BUTTON: f32 = 26.0;
/// Height of the filter field (mockup `30px`).
pub const FIELD_HEIGHT: f32 = 30.0;

/// Panel insets, matching the mockup's `padding` on each region.
pub const PANEL_MARGIN_X: i8 = 12;
pub const LIST_MARGIN_X: i8 = 10;

// ------------------------------------------------------------- typography

pub const FONT_TITLE: f32 = 14.0;
pub const FONT_PROJECT: f32 = 12.0;
pub const FONT_BRANCH: f32 = 12.5;
pub const FONT_BODY: f32 = 11.5;
pub const FONT_CHIP: f32 = 11.0;
pub const FONT_SMALL: f32 = 10.0;
pub const FONT_SUB: f32 = 9.5;

/// Recommended window size: a narrow vertical panel (DESIGN.md §5).
pub const WINDOW_SIZE: [f32; 2] = [360.0, 720.0];
/// Smallest useful window: the narrow layout with the header, a row or two,
/// and the footer still legible. Resizing (`ui::window_edge`) stops here.
pub const MIN_WINDOW_SIZE: [f32; 2] = [280.0, 360.0];

pub fn apply(ctx: &egui::Context) {
    let mut visuals = Visuals::dark();
    visuals.panel_fill = BG;
    visuals.window_fill = BG_RAISED;
    visuals.extreme_bg_color = FIELD;
    visuals.faint_bg_color = BADGE;
    visuals.override_text_color = Some(TEXT);
    visuals.window_stroke = Stroke::new(1.0_f32, HAIRLINE);
    visuals.window_corner_radius = CornerRadius::same(12);
    visuals.window_shadow = egui::epaint::Shadow {
        offset: [0, 12],
        blur: 32,
        spread: 0,
        color: Color32::from_black_alpha(160),
    };
    visuals.popup_shadow = egui::epaint::Shadow {
        offset: [0, 6],
        blur: 16,
        spread: 0,
        color: Color32::from_black_alpha(140),
    };
    visuals.selection.bg_fill = ACCENT_FILL;
    visuals.selection.stroke = Stroke::new(1.0_f32, ACCENT);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, TEXT);

    let radius = CornerRadius::same(CHIP_RADIUS);
    for widget in [
        &mut visuals.widgets.noninteractive,
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        widget.corner_radius = radius;
        widget.expansion = 0.0;
    }
    visuals.widgets.noninteractive.bg_fill = BG;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, HAIRLINE);
    visuals.widgets.inactive.bg_fill = CHIP;
    visuals.widgets.inactive.weak_bg_fill = CHIP;
    visuals.widgets.inactive.bg_stroke = Stroke::NONE;
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, TEXT_DIM);
    visuals.widgets.hovered.bg_fill = HAIRLINE;
    visuals.widgets.hovered.weak_bg_fill = HAIRLINE;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, BORDER);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, TEXT_STRONG);
    visuals.widgets.active.bg_fill = ACCENT_FILL;
    visuals.widgets.active.weak_bg_fill = ACCENT_FILL;
    visuals.widgets.active.bg_stroke = Stroke::new(1.0_f32, ACCENT);
    visuals.widgets.open.bg_fill = FIELD;
    visuals.widgets.open.weak_bg_fill = FIELD;
    ctx.set_visuals(visuals);

    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = egui::vec2(6.0, 4.0);
    style.spacing.button_padding = egui::vec2(9.0, 5.0);
    style.spacing.window_margin = Margin::same(14);
    style.spacing.menu_margin = Margin::same(6);
    style.spacing.interact_size.y = 22.0;
    style.visuals.menu_corner_radius = CornerRadius::same(10);
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

/// Placeholder text for a field: the same size as what will be typed there,
/// in the faint colour that says it is not a value.
///
/// Always use this for `TextEdit::hint_text`. A hint passed as a plain `&str`
/// is coloured by `override_text_color` (`apply` sets it), which egui resolves
/// *before* the weak colour it would otherwise paint the hint with — so the
/// placeholder comes out looking exactly like typed text. A `RichText` with a
/// colour of its own wins instead. No size or family is set here, so the hint
/// keeps the font of the field it sits in.
pub fn hint(text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text).color(TEXT_FAINT)
}

/// A menu entry with its keyboard shortcut greyed out at the right edge.
///
/// The shortcut gets an explicit colour rather than egui's own weak tint, for
/// the same reason [`hint`] does: `apply` sets `override_text_color`, and a
/// `RichText` that names its colour is the only kind that survives it.
pub fn menu_item(text: impl Into<egui::RichText>, shortcut: &str) -> egui::Button<'static> {
    egui::Button::new(text.into()).shortcut_text(label(shortcut, FONT_SMALL, TEXT_FAINT))
}

/// A section caption: small, uppercase-weight, muted.
pub fn caption(text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text).size(FONT_CHIP).color(TEXT_MUTED)
}
