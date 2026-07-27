//! Resize handles for the undecorated window.
//!
//! Grove draws its own header, so the window is created with
//! `with_decorations(false)`. On Wayland a client-side-decorated window owns
//! its resize edges too: the compositor exposes none of its own, so without
//! this module the window simply cannot be resized (it *is* resizable — there
//! is just nothing to grab). Dragging in one of the eight zones below sends
//! [`egui::ViewportCommand::BeginResize`], which hands the interaction to the
//! compositor exactly as a decoration border would.
//!
//! The zones are registered in the background layer *before* the panels, the
//! same way the header's drag region is registered before the header's
//! buttons: a control drawn over a zone is registered later and therefore
//! wins the click. (For *drags* egui deliberately prefers the thinner of two
//! tied candidates, so that thin resize handles stay easy to hit; a wide
//! drag-sensing widget reaching into the band would lose to the band. Nothing
//! in Grove does: every panel's inner margin is wider than [`BAND`], so the
//! grab area contains no controls at all.)

use egui::{CursorIcon, Pos2, Rect, ResizeDirection, Sense};

/// Width of the grab band, in points. Matches the usual GTK/KDE CSD border.
pub const BAND: f32 = 6.0;

/// The eight resize zones of `rect`: the four edges, then the four corners.
///
/// Corners come last so that they are registered on top of the edges, and are
/// cut out of the edge rectangles as well, so the two can never disagree.
/// `band` is clamped so that a very small window still leaves a usable middle.
pub fn zones(rect: Rect, band: f32) -> Vec<(ResizeDirection, Rect)> {
    use ResizeDirection::*;

    let band = band
        .min(rect.width() / 3.0)
        .min(rect.height() / 3.0)
        .max(1.0);
    let (left, right) = (rect.left(), rect.right());
    let (top, bottom) = (rect.top(), rect.bottom());
    let corner =
        |x: f32, y: f32| Rect::from_min_max(Pos2::new(x, y), Pos2::new(x + band, y + band));

    vec![
        (
            West,
            Rect::from_min_max(
                Pos2::new(left, top + band),
                Pos2::new(left + band, bottom - band),
            ),
        ),
        (
            East,
            Rect::from_min_max(
                Pos2::new(right - band, top + band),
                Pos2::new(right, bottom - band),
            ),
        ),
        (
            North,
            Rect::from_min_max(
                Pos2::new(left + band, top),
                Pos2::new(right - band, top + band),
            ),
        ),
        (
            South,
            Rect::from_min_max(
                Pos2::new(left + band, bottom - band),
                Pos2::new(right - band, bottom),
            ),
        ),
        (NorthWest, corner(left, top)),
        (NorthEast, corner(right - band, top)),
        (SouthWest, corner(left, bottom - band)),
        (SouthEast, corner(right - band, bottom - band)),
    ]
}

/// The pointer shape for a zone.
pub fn cursor(direction: ResizeDirection) -> CursorIcon {
    match direction {
        ResizeDirection::North => CursorIcon::ResizeNorth,
        ResizeDirection::South => CursorIcon::ResizeSouth,
        ResizeDirection::East => CursorIcon::ResizeEast,
        ResizeDirection::West => CursorIcon::ResizeWest,
        ResizeDirection::NorthEast => CursorIcon::ResizeNorthEast,
        ResizeDirection::NorthWest => CursorIcon::ResizeNorthWest,
        ResizeDirection::SouthEast => CursorIcon::ResizeSouthEast,
        ResizeDirection::SouthWest => CursorIcon::ResizeSouthWest,
    }
}

/// Register the zones and start a compositor-side resize when one is dragged.
///
/// Called once per frame, before the panels, so every widget in the app is
/// registered after — and therefore on top of — the grab areas.
pub fn show(ctx: &egui::Context) {
    // A dialog covering the edge owns the pointer: egui would otherwise still
    // favour the thin band for a *drag*, and the window would start resizing
    // from under an open confirmation.
    if pointer_is_over_a_higher_layer(ctx) {
        return;
    }

    let screen = ctx.screen_rect();
    let ui = egui::Ui::new(
        ctx.clone(),
        egui::Id::new("grove-resize-edges"),
        egui::UiBuilder::new()
            .layer_id(egui::LayerId::background())
            .max_rect(screen),
    );
    for (direction, zone) in zones(screen, BAND) {
        let response = ui.interact(zone, ui.id().with(zone_name(direction)), Sense::drag());
        if response.hovered() || response.dragged() {
            ctx.set_cursor_icon(cursor(direction));
        }
        if response.drag_started() {
            ctx.send_viewport_cmd(egui::ViewportCommand::BeginResize(direction));
        }
    }
}

/// Is the pointer over a window, popup or tooltip — anything drawn above the
/// panels?
fn pointer_is_over_a_higher_layer(ctx: &egui::Context) -> bool {
    let Some(pos) = ctx.input(|i| i.pointer.interact_pos()) else {
        return false;
    };
    ctx.layer_id_at(pos)
        .is_some_and(|layer| layer.order != egui::Order::Background)
}

/// Stable per-zone id fragment.
fn zone_name(direction: ResizeDirection) -> &'static str {
    match direction {
        ResizeDirection::North => "n",
        ResizeDirection::South => "s",
        ResizeDirection::East => "e",
        ResizeDirection::West => "w",
        ResizeDirection::NorthEast => "ne",
        ResizeDirection::NorthWest => "nw",
        ResizeDirection::SouthEast => "se",
        ResizeDirection::SouthWest => "sw",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window() -> Rect {
        Rect::from_min_size(Pos2::ZERO, egui::vec2(360.0, 720.0))
    }

    /// The zone a point falls in, mirroring the registration order: later
    /// (corner) zones win, exactly as egui's hit test resolves overlaps.
    fn zone_at(rect: Rect, pos: Pos2) -> Option<ResizeDirection> {
        zones(rect, BAND)
            .into_iter()
            .rev()
            .find(|(_, zone)| zone.contains(pos))
            .map(|(direction, _)| direction)
    }

    #[test]
    fn every_direction_has_exactly_one_zone() {
        let zones = zones(window(), BAND);
        assert_eq!(zones.len(), 8);
        let mut seen: Vec<String> = zones
            .iter()
            .map(|(d, _)| zone_name(*d).to_string())
            .collect();
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), 8, "eight distinct directions");
    }

    #[test]
    fn each_edge_and_corner_is_reachable() {
        let rect = window();
        let cases = [
            (Pos2::new(1.0, 300.0), ResizeDirection::West),
            (Pos2::new(358.0, 300.0), ResizeDirection::East),
            (Pos2::new(180.0, 1.0), ResizeDirection::North),
            (Pos2::new(180.0, 719.0), ResizeDirection::South),
            (Pos2::new(1.0, 1.0), ResizeDirection::NorthWest),
            (Pos2::new(359.0, 2.0), ResizeDirection::NorthEast),
            (Pos2::new(2.0, 718.0), ResizeDirection::SouthWest),
            (Pos2::new(359.0, 719.0), ResizeDirection::SouthEast),
        ];
        for (pos, expected) in cases {
            assert_eq!(zone_at(rect, pos), Some(expected), "at {pos:?}");
        }
    }

    #[test]
    fn corners_win_over_edges_where_they_meet() {
        let rect = window();
        // The corner square is exactly BAND × BAND; one point inside it is
        // covered by no edge, and the edges start where it ends.
        assert_eq!(
            zone_at(rect, Pos2::new(3.0, 3.0)),
            Some(ResizeDirection::NorthWest)
        );
        assert_eq!(
            zone_at(rect, Pos2::new(3.0, 10.0)),
            Some(ResizeDirection::West)
        );
        assert_eq!(
            zone_at(rect, Pos2::new(10.0, 3.0)),
            Some(ResizeDirection::North)
        );
    }

    #[test]
    fn the_content_area_is_not_a_grab_zone() {
        let rect = window();
        for pos in [
            Pos2::new(180.0, 360.0),
            Pos2::new(BAND + 1.0, 360.0),
            Pos2::new(360.0 - BAND - 1.0, 360.0),
            Pos2::new(180.0, BAND + 1.0),
            Pos2::new(180.0, 720.0 - BAND - 1.0),
        ] {
            assert_eq!(zone_at(rect, pos), None, "at {pos:?}");
        }
    }

    #[test]
    fn zones_stay_inside_the_window() {
        let rect = Rect::from_min_size(Pos2::new(17.0, 23.0), egui::vec2(360.0, 720.0));
        for (direction, zone) in zones(rect, BAND) {
            assert!(rect.contains_rect(zone), "{direction:?} escapes the window");
            assert!(zone.width() > 0.0 && zone.height() > 0.0);
        }
    }

    /// The band never eats the whole window, however small it gets: there is
    /// always a middle left for the content.
    #[test]
    fn a_tiny_window_still_has_a_middle() {
        let rect = Rect::from_min_size(Pos2::ZERO, egui::vec2(12.0, 12.0));
        let zones = zones(rect, BAND);
        assert_eq!(zones.len(), 8);
        for (_, zone) in &zones {
            assert!(rect.contains_rect(*zone));
        }
        assert_eq!(zone_at(rect, rect.center()), None);
    }

    #[test]
    fn every_zone_has_its_own_cursor() {
        let mut icons: Vec<CursorIcon> = zones(window(), BAND)
            .into_iter()
            .map(|(direction, _)| cursor(direction))
            .collect();
        let count = icons.len();
        icons.sort_by_key(|icon| format!("{icon:?}"));
        icons.dedup();
        assert_eq!(icons.len(), count, "no two zones share a cursor");
    }

    // ------------------------------------------- driving a real egui context
    //
    // The zones only work if egui actually routes the press to them, which is
    // a question about layer order, not about geometry. These tests press the
    // (headless) pointer down and read the viewport commands egui emitted.

    fn raw_input(rect: Rect, events: Vec<egui::Event>) -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(rect),
            events,
            ..Default::default()
        }
    }

    fn press_at(pos: Pos2) -> Vec<egui::Event> {
        vec![
            egui::Event::PointerMoved(pos),
            egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::default(),
            },
        ]
    }

    /// Run `build` for three passes — hover, press, small move — and collect
    /// every viewport command egui produced.
    fn drag_from(pos: Pos2, mut build: impl FnMut(&egui::Context)) -> Vec<egui::ViewportCommand> {
        let rect = window();
        let ctx = egui::Context::default();
        let mut commands = Vec::new();
        let passes = [
            vec![egui::Event::PointerMoved(pos)],
            press_at(pos),
            vec![egui::Event::PointerMoved(pos + egui::vec2(4.0, 4.0))],
        ];
        for events in passes {
            let output = ctx.run(raw_input(rect, events), &mut build);
            for viewport in output.viewport_output.values() {
                commands.extend(viewport.commands.iter().cloned());
            }
        }
        commands
    }

    fn resize_directions(commands: &[egui::ViewportCommand]) -> Vec<ResizeDirection> {
        commands
            .iter()
            .filter_map(|command| match command {
                egui::ViewportCommand::BeginResize(direction) => Some(*direction),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn pressing_in_a_zone_asks_the_compositor_to_resize() {
        for (direction, zone) in zones(window(), BAND) {
            let commands = drag_from(zone.center(), show);
            assert_eq!(
                resize_directions(&commands),
                vec![direction],
                "pressing the {direction:?} zone must begin exactly that resize"
            );
        }
    }

    #[test]
    fn pressing_in_the_middle_resizes_nothing() {
        let commands = drag_from(window().center(), show);
        assert!(resize_directions(&commands).is_empty());
    }

    /// The layering guarantee: a control drawn over a zone is registered
    /// later and still receives the press, exactly as the header's buttons do
    /// over the header's drag region.
    #[test]
    fn a_control_over_a_zone_still_receives_the_press() {
        let mut pressed = false;
        drag_from(Pos2::new(2.0, 300.0), |ctx| {
            show(ctx);
            egui::CentralPanel::default()
                .frame(egui::Frame::new())
                .show(ctx, |ui| {
                    if ui
                        .put(
                            Rect::from_min_size(Pos2::new(0.0, 290.0), egui::vec2(60.0, 30.0)),
                            egui::Button::new("over the edge"),
                        )
                        .is_pointer_button_down_on()
                    {
                        pressed = true;
                    }
                });
        });
        assert!(pressed, "the widget above the zone must receive the press");
    }

    /// A dialog is in a layer above the background, so it takes the press
    /// outright: no resize starts under an open window.
    #[test]
    fn a_window_over_a_zone_blocks_the_resize() {
        let commands = drag_from(Pos2::new(2.0, 300.0), |ctx| {
            show(ctx);
            egui::Window::new("dialog")
                .movable(false)
                .resizable(false)
                .current_pos(Pos2::ZERO)
                .show(ctx, |ui| {
                    ui.allocate_space(egui::vec2(360.0, 720.0));
                });
        });
        assert!(
            resize_directions(&commands).is_empty(),
            "an open dialog must swallow the press"
        );
    }
}
