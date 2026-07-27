//! The line-art backdrop that fills the empty space under a short list.
//!
//! Purely decorative: it is painted into the [`Order::Background`] layer, so
//! it can never sit over a row or intercept a click, and it is skipped
//! entirely once the list is long enough to fill the panel.
//!
//! The shipped PNG is white on transparent. The colour comes from
//! [`theme::BACKDROP`] as a tint at draw time, so this file — like every
//! other outside `theme` — holds no colour literal.

use egui::epaint::Vertex;
use egui::{Color32, Context, Mesh, Order, Pos2, Rect, TextureHandle, TextureOptions, pos2};

use super::theme;

/// Rasterised from `assets/backdrop.svg` at 640 px wide (`rsvg-convert -w
/// 640`). Regenerate both together; the SVG is the source of truth.
const BACKDROP_PNG: &[u8] = include_bytes!("../../assets/backdrop.png");

/// Below this much free height the art is a meaningless sliver, so nothing is
/// drawn rather than a strip of hillside.
const MIN_HEIGHT: f32 = 150.0;

/// Grid resolution of the fade mesh. The alpha ramp is a curve and egui
/// interpolates linearly between vertices, so it needs a handful of steps in
/// each direction to look smooth.
const FADE_COLS: usize = 8;
const FADE_ROWS: usize = 12;

/// How much of the art fades in at the cropped top and left edges. The
/// illustration bleeds off both — hills run to the left edge, branches to the
/// top — so without this it ends on a visible straight cut.
const FADE_ZONE: f32 = 0.35;

/// Height of the art as a fraction of the panel's. At 1 the whole
/// illustration is visible when the list is empty; above 1 it overflows the
/// top and is cropped there.
const HEIGHT_SCALE: f32 = 1.0;

/// Widest the art may get, as a fraction of the panel. A tall window would
/// otherwise scale it until it dominated the view.
const MAX_WIDTH_FRACTION: f32 = 0.8;

/// How far below the last row the art takes to reach full strength. Enough
/// that a list growing by one row dissolves it gradually instead of shearing
/// it off along the row's edge.
const REVEAL_DISTANCE: f32 = 120.0;

/// Decode the PNG once and keep the texture in egui's memory.
///
/// Decoding costs a few milliseconds on the frame that first needs it, which
/// is why it happens here rather than eagerly at startup: a list long enough
/// to fill the panel never pays for it at all.
fn texture(ctx: &Context) -> Option<TextureHandle> {
    let id = egui::Id::new("grove-backdrop-texture");
    if let Some(handle) = ctx.data(|d| d.get_temp::<TextureHandle>(id)) {
        return Some(handle);
    }

    let decoded =
        image::load_from_memory_with_format(BACKDROP_PNG, image::ImageFormat::Png).ok()?;
    let rgba = decoded.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    let image = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());

    let handle = ctx.load_texture("grove-backdrop", image, TextureOptions::LINEAR);
    ctx.data_mut(|d| d.insert_temp(id, handle.clone()));
    Some(handle)
}

/// Paint the backdrop pinned to the bottom-right of `panel`, revealed only
/// within `free` — the gap the list has not taken.
///
/// Size comes from `panel` and visibility from `free`, so adding a row
/// uncovers or covers more of the art without ever resizing it.
pub fn show(ctx: &Context, panel: Rect, free: Rect) {
    if free.height() < MIN_HEIGHT || panel.width() <= 0.0 {
        return;
    }
    let Some(texture) = texture(ctx) else {
        return;
    };
    let size = texture.size_vec2();
    if size.x <= 0.0 || size.y <= 0.0 {
        return;
    }

    let mut painter = ctx.layer_painter(egui::LayerId::new(Order::Background, id()));
    painter.set_clip_rect(free);
    painter.add(fade_mesh(
        texture.id(),
        placement(panel, size.y / size.x),
        free.top(),
    ));
}

/// Where the art goes, and how much of it is cropped away.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Placement {
    quad: Rect,
    /// Top of the visible slice in texture coordinates. Above this the art is
    /// cropped, which only happens when the aspect cap bites.
    uv_top: f32,
}

/// Fit the art to the panel's *height* and anchor it bottom-right.
///
/// Fitting to width instead is what the first attempt did, and on a wide,
/// shallow panel it scales a 941×1672 portrait until only hillside is left in
/// frame.
///
/// Deliberately a function of the panel and nothing else. Sizing against the
/// gap under the list instead makes the art grow and shrink by a visible step
/// every time a project is expanded, which reads as the illustration being
/// part of the list rather than behind it.
fn placement(panel: Rect, aspect: f32) -> Placement {
    let max_width = panel.width() * MAX_WIDTH_FRACTION;
    let width = (panel.height() * HEIGHT_SCALE / aspect).min(max_width);
    let uncropped_height = width * aspect;
    let height = uncropped_height.min(panel.height());

    Placement {
        quad: Rect::from_min_max(
            pos2(panel.right() - width, panel.bottom() - height),
            pos2(panel.right(), panel.bottom()),
        ),
        uv_top: 1.0 - height / uncropped_height,
    }
}

fn id() -> egui::Id {
    egui::Id::new("grove-backdrop")
}

/// A grid of quads whose vertex alpha ramps up from the top and left edges,
/// so the art dissolves into the panel instead of ending on a hard cut. The
/// bottom and right edges are the window's own, and need no fade.
fn fade_mesh(texture: egui::TextureId, placement: Placement, free_top: f32) -> Mesh {
    let Placement { quad, uv_top } = placement;
    let mut mesh = Mesh::with_texture(texture);

    for row in 0..=FADE_ROWS {
        let v = row as f32 / FADE_ROWS as f32;
        let y = quad.top() + quad.height() * v;
        for col in 0..=FADE_COLS {
            let u = col as f32 / FADE_COLS as f32;
            // Pushed directly rather than via `colored_vertex`, which asserts
            // the mesh is untextured — this one carries the art.
            mesh.vertices.push(Vertex {
                pos: pos2(quad.left() + quad.width() * u, y),
                uv: pos2(u, uv_top + (1.0 - uv_top) * v),
                color: tint(edge_fade(u) * edge_fade(v) * reveal(y, free_top)),
            });
        }
    }

    let stride = (FADE_COLS + 1) as u32;
    for row in 0..FADE_ROWS as u32 {
        for col in 0..FADE_COLS as u32 {
            let top_left = row * stride + col;
            mesh.add_triangle(top_left, top_left + 1, top_left + stride);
            mesh.add_triangle(top_left + 1, top_left + stride, top_left + stride + 1);
        }
    }
    mesh
}

/// Opacity multiplier `t` of the way in from a cropped edge: zero at the edge,
/// ramping to full over [`FADE_ZONE`] and flat after it. Squared, so the art
/// arrives late and reads as haze rather than as a gradient.
fn edge_fade(t: f32) -> f32 {
    let ramp = (t / FADE_ZONE).min(1.0);
    ramp * ramp
}

/// How much of the art shows at screen height `y`, given that the list ends
/// at `free_top`.
///
/// The clip rect alone would cut the art off on a hard horizontal line right
/// under the last row. This ramps it back in over [`REVEAL_DISTANCE`] so the
/// list appears to dissolve it rather than to guillotine it.
fn reveal(y: f32, free_top: f32) -> f32 {
    edge_fade(((y - free_top) / REVEAL_DISTANCE).clamp(0.0, 1.0) * FADE_ZONE)
}

/// The theme colour at a given opacity multiplier.
fn tint(strength: f32) -> Color32 {
    let alpha = f32::from(theme::BACKDROP_ALPHA) * strength;
    Color32::from_rgba_unmultiplied(
        theme::BACKDROP.r(),
        theme::BACKDROP.g(),
        theme::BACKDROP.b(),
        alpha.round() as u8,
    )
}

/// The area left under the last row, in screen coordinates.
///
/// `None` when the list already fills the panel — the caller then draws
/// nothing, which is the common case once a few projects are expanded.
pub fn free_space(panel: Rect, content_bottom: f32) -> Option<Rect> {
    let free = Rect::from_min_max(
        Pos2::new(panel.left(), content_bottom),
        Pos2::new(panel.right(), panel.bottom()),
    );
    (free.height() >= MIN_HEIGHT).then_some(free)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A non-default id: `TextureId::default()` is the untextured sentinel,
    /// and using it here hid a panic in `Mesh::colored_vertex`.
    fn texture_id() -> egui::TextureId {
        egui::TextureId::User(1)
    }

    fn panel() -> Rect {
        Rect::from_min_max(pos2(0.0, 0.0), pos2(800.0, 600.0))
    }

    #[test]
    fn a_short_list_leaves_room_for_the_backdrop() {
        let free = free_space(panel(), 100.0).expect("room under a short list");
        assert_eq!(free.top(), 100.0);
        assert_eq!(free.bottom(), 600.0);
        assert_eq!(free.width(), 800.0);
    }

    #[test]
    fn a_full_list_leaves_none() {
        assert!(free_space(panel(), 580.0).is_none());
        assert!(free_space(panel(), 600.0).is_none());
        // A list taller than the panel must not produce an inverted rect.
        assert!(free_space(panel(), 900.0).is_none());
    }

    #[test]
    fn the_threshold_is_a_sliver_not_zero() {
        // Just under: a strip of hillside is worse than nothing.
        assert!(free_space(panel(), 600.0 - MIN_HEIGHT + 1.0).is_none());
        assert!(free_space(panel(), 600.0 - MIN_HEIGHT).is_some());
    }

    #[test]
    fn the_fade_starts_clear_at_a_cropped_edge_and_reaches_full_strength() {
        assert_eq!(edge_fade(0.0), 0.0);
        assert_eq!(edge_fade(FADE_ZONE), 1.0);
        // Flat once past the zone: the body of the art is not a gradient.
        assert_eq!(edge_fade(1.0), 1.0);
        assert_eq!(tint(0.0).a(), 0);
        assert_eq!(tint(1.0).a(), theme::BACKDROP_ALPHA);
    }

    #[test]
    fn the_fade_never_gets_fainter_further_from_the_edge() {
        let mut previous = 0.0;
        for step in 0..=FADE_ROWS {
            let t = step as f32 / FADE_ROWS as f32;
            let fade = edge_fade(t);
            assert!(fade >= previous, "fade fell at {t}");
            previous = fade;
        }
    }

    #[test]
    fn a_wide_shallow_panel_fits_the_art_to_its_height() {
        // The bug the first version had: fitting a 941x1672 portrait to the
        // width of a wide panel leaves only hillside in frame.
        let aspect = 1672.0 / 941.0;
        let area = Rect::from_min_max(pos2(0.0, 0.0), pos2(900.0, 410.0));
        let p = placement(area, aspect);

        assert_eq!(p.quad.height(), area.height(), "fills the panel's height");
        assert!(p.quad.width() < area.width() * MAX_WIDTH_FRACTION + 0.01);
        assert!(p.uv_top.abs() < 0.001, "nothing cropped: {}", p.uv_top);
    }

    #[test]
    fn the_size_ignores_how_much_of_the_list_is_showing() {
        // Sizing against the free space instead makes the art grow and shrink
        // by a visible step every time a project is expanded.
        let panel = panel();
        let aspect = 1672.0 / 941.0;
        let fixed = placement(panel, aspect).quad.size();

        for content_bottom in [0.0, 100.0, 250.0, 400.0, 449.0] {
            let free = free_space(panel, content_bottom).expect("room");
            assert!(free.height() != panel.height() || content_bottom == 0.0);
            assert_eq!(
                placement(panel, aspect).quad.size(),
                fixed,
                "resized at content_bottom={content_bottom}"
            );
        }
    }

    #[test]
    fn the_art_is_anchored_to_the_bottom_right() {
        let area = Rect::from_min_max(pos2(0.0, 0.0), pos2(900.0, 620.0));
        let p = placement(area, 1672.0 / 941.0);
        assert_eq!(p.quad.right(), area.right());
        assert_eq!(p.quad.bottom(), area.bottom());
        assert!(p.quad.left() > area.left(), "leaves the list's side clear");
    }

    #[test]
    fn a_tall_empty_panel_caps_the_width_and_crops_instead() {
        let aspect = 1672.0 / 941.0;
        let area = Rect::from_min_max(pos2(0.0, 0.0), pos2(400.0, 1400.0));
        let p = placement(area, aspect);

        assert_eq!(p.quad.width(), area.width() * MAX_WIDTH_FRACTION);
        assert!(p.quad.height() <= area.height());
        assert!(p.uv_top >= 0.0 && p.uv_top < 1.0);
    }

    #[test]
    fn the_mesh_is_a_closed_grid_with_bottom_right_anchored_uvs() {
        let quad = Rect::from_min_max(pos2(600.0, 100.0), pos2(800.0, 600.0));
        let mesh = fade_mesh(texture_id(), Placement { quad, uv_top: 0.25 }, quad.top());

        assert_eq!(mesh.vertices.len(), (FADE_COLS + 1) * (FADE_ROWS + 1));
        assert_eq!(mesh.indices.len(), FADE_COLS * FADE_ROWS * 6);
        assert!(
            mesh.indices
                .iter()
                .all(|&i| (i as usize) < mesh.vertices.len()),
            "every index is in range"
        );

        // Cropping takes the top off: the bottom of the art always shows.
        assert_eq!(mesh.vertices[0].uv, pos2(0.0, 0.25));
        assert_eq!(mesh.vertices.last().expect("vertices").uv, pos2(1.0, 1.0));

        // Corners: transparent at the cropped top-left, full at bottom-right.
        assert_eq!(mesh.vertices[0].color.a(), 0);
        assert_eq!(
            mesh.vertices.last().expect("vertices").color.a(),
            theme::BACKDROP_ALPHA
        );
    }

    #[test]
    fn the_art_stays_pinned_to_the_window_corner_as_the_list_grows() {
        // The corner the art hangs from is the panel's own bottom-right —
        // the window's edge, not the last row's — and must not drift.
        let panel = panel();
        let aspect = 1672.0 / 941.0;

        let corners: Vec<_> = [100.0, 200.0, 300.0, 400.0]
            .iter()
            .filter_map(|&bottom| free_space(panel, bottom))
            .map(|_| placement(panel, aspect).quad.max)
            .collect();

        assert_eq!(corners.len(), 4, "all four leave room");
        assert!(
            corners
                .iter()
                .all(|&c| c == pos2(panel.right(), panel.bottom())),
            "the anchor moved: {corners:?}"
        );
    }

    #[test]
    fn the_list_dissolves_the_art_instead_of_shearing_it_off() {
        let free_top = 200.0;
        // Right at the last row: nothing shows, so there is no hard line.
        assert_eq!(reveal(free_top, free_top), 0.0);
        // Well clear of it: full strength.
        assert_eq!(reveal(free_top + REVEAL_DISTANCE, free_top), 1.0);
        assert_eq!(reveal(free_top + 1000.0, free_top), 1.0);
        // Above the last row it is clipped away entirely; never negative.
        assert_eq!(reveal(free_top - 50.0, free_top), 0.0);
    }

    #[test]
    fn the_reveal_only_ever_brightens_further_from_the_list() {
        let free_top = 200.0;
        let mut previous = 0.0;
        for step in 0..=20 {
            let y = free_top + REVEAL_DISTANCE * step as f32 / 10.0;
            let r = reveal(y, free_top);
            assert!(r >= previous, "reveal fell at y={y}");
            previous = r;
        }
    }
}
