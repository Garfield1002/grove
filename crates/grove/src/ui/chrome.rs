//! Chrome and lifecycle for Grove's detached windows.
//!
//! Grove's main window is a deliberately narrow vertical sliver, so Settings,
//! create-worktree and safe-removal do not render inside it: each opens as its
//! own toplevel through egui's multi-viewport support.
//!
//! **Immediate, not deferred.** [`egui::Context::show_viewport_immediate`]
//! runs the dialog's UI inline, on the UI thread, with an ordinary `FnMut`
//! closure — so a dialog keeps borrowing `&mut GroveApp` state (its form, the
//! worker handle, `Paths`) exactly as it did when it was an `egui::Window`.
//! A deferred viewport takes a `Fn + Send + Sync + 'static` callback, which
//! would force every dialog's state into an `Arc<Mutex<…>>` and split the
//! message plumbing in two, for a repaint saving that is worthless here: these
//! windows are open for seconds at a time and never animate.
//!
//! The chrome matches the main window (`main.rs`): undecorated, with the
//! header as the drag handle, [`super::window_edge`] on the four edges and
//! corners, and Esc / Ctrl+W / the header's ✕ as the close affordances the
//! compositor no longer provides.
//!
//! Placement is left alone on purpose: on Wayland the compositor, not the
//! client, decides where a new toplevel lands, so a builder only ever sets a
//! size.

use egui::{Context, Ui, ViewportBuilder, ViewportClass};

use super::{icons, theme, window_edge};

/// One dialog per kind, and asking for it again raises the one on screen.
///
/// Plain state with no egui in it: the one-instance rule and the focus
/// bookkeeping are unit-tested below.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detached<T> {
    form: Option<T>,
    /// Set when the window is asked for while already open; the next frame
    /// sends [`egui::ViewportCommand::Focus`] instead of building a second one.
    focus: bool,
}

impl<T> Default for Detached<T> {
    fn default() -> Self {
        Self {
            form: None,
            focus: false,
        }
    }
}

impl<T> Detached<T> {
    /// Open the window on `form`, raising it if it was already open.
    pub fn open(&mut self, form: T) {
        self.form = Some(form);
        self.focus = true;
    }

    /// Open the window, or focus the one already showing what was asked for.
    ///
    /// `is_same` decides whether the open window is already the right one; if
    /// it is, the user's typing survives and only focus is requested. Returns
    /// true when `form` was installed, which is the caller's cue to kick off
    /// whatever loading the new content needs.
    pub fn open_or_focus(&mut self, form: T, is_same: impl FnOnce(&T) -> bool) -> bool {
        match &self.form {
            Some(existing) if is_same(existing) => {
                self.focus = true;
                false
            }
            _ => {
                self.open(form);
                true
            }
        }
    }

    /// Raise the window if it is open; do nothing if it is not.
    pub fn request_focus(&mut self) {
        if self.form.is_some() {
            self.focus = true;
        }
    }

    /// Tear the window down. Idempotent: closing by ✕, Esc, Ctrl+W, the
    /// compositor and the dialog's own action can all land in one frame.
    pub fn close(&mut self) {
        self.form = None;
        self.focus = false;
    }

    pub fn is_open(&self) -> bool {
        self.form.is_some()
    }

    pub fn get(&self) -> Option<&T> {
        self.form.as_ref()
    }

    pub fn get_mut(&mut self) -> Option<&mut T> {
        self.form.as_mut()
    }

    /// Consume a pending focus request. A closed window never has one.
    pub fn take_focus_request(&mut self) -> bool {
        std::mem::take(&mut self.focus)
    }
}

/// The builder for a detached window: Grove's own chrome, a content-sized
/// default and a floor below which the layout stops making sense.
///
/// The toplevel is titled `Grove — <title>` so it is recognisable in a task
/// switcher, while `title` alone is what the window's own header shows.
///
/// No position is set — see the module docs.
pub fn viewport(title: &str, size: [f32; 2], min_size: [f32; 2]) -> ViewportBuilder {
    ViewportBuilder::default()
        .with_title(format!("Grove — {title}"))
        .with_app_id("grove")
        .with_decorations(false)
        .with_resizable(true)
        .with_inner_size(size)
        .with_min_inner_size(min_size)
}

/// What a key press in a detached window means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogKey {
    /// Close this window only.
    Close,
    /// Quit Grove.
    Quit,
}

/// Decide what the keyboard just asked a detached window to do.
///
/// Ctrl+W closes the window, exactly like Esc and the header's ✕: "close
/// window" is what it means in every other application. Ctrl+Q keeps meaning
/// "quit the application", so it closes Grove from whichever window has focus
/// — the main window's behaviour, unchanged, reachable from the dialogs too.
/// Quitting wins if both arrive in the same frame.
pub fn dialog_key(
    esc: bool,
    ctrl_w: bool,
    ctrl_q: bool,
    close_requested: bool,
) -> Option<DialogKey> {
    if ctrl_q {
        return Some(DialogKey::Quit);
    }
    if esc || ctrl_w || close_requested {
        return Some(DialogKey::Close);
    }
    None
}

/// What one frame of a detached window produced.
pub struct Dialog<R> {
    /// Whatever the content returned this frame.
    pub inner: R,
    /// The user asked for the window to go away.
    pub close: bool,
}

/// Draw one frame of a detached window: resize edges, the draggable header,
/// then `add_contents` in a scrolling body.
///
/// `class` comes from the viewport callback. If the backend cannot give Grove
/// a second toplevel (`ViewportClass::Embedded`), the dialog falls back to an
/// in-window `egui::Window` — cramped, but never lost.
pub fn show<R: Default>(
    ctx: &Context,
    class: ViewportClass,
    title: &str,
    add_contents: impl FnOnce(&mut Ui) -> R,
) -> Dialog<R> {
    if class == ViewportClass::Embedded {
        return embedded(ctx, title, add_contents);
    }

    // Registered before the panels, so every control in the window is
    // registered after — and therefore on top of — the grab bands.
    window_edge::show(ctx);

    let mut close = false;
    // Panel state is stored by id in egui's global map, so the header of each
    // detached window gets its own — unlike `CentralPanel`, which egui already
    // namespaces by viewport.
    let header =
        egui::TopBottomPanel::top(egui::Id::new((ctx.viewport_id(), "grove-dialog-header")))
            .frame(
                egui::Frame::new()
                    .fill(theme::BG_SUNKEN)
                    .inner_margin(egui::Margin::symmetric(theme::PANEL_MARGIN_X, 8)),
            )
            .show(ctx, |ui| header(ui, title));
    close |= header.inner;
    hairline(
        ctx,
        header.response.rect.left_bottom(),
        ctx.screen_rect().width(),
    );

    let inner = egui::CentralPanel::default()
        .frame(
            egui::Frame::new()
                .fill(theme::BG)
                .inner_margin(egui::Margin::symmetric(theme::PANEL_MARGIN_X + 2, 12)),
        )
        .show(ctx, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, add_contents)
                .inner
        })
        .inner;

    let (esc, ctrl_w, ctrl_q) = ctx.input(|i| {
        (
            i.key_pressed(egui::Key::Escape),
            i.modifiers.command && i.key_pressed(egui::Key::W),
            i.modifiers.command && i.key_pressed(egui::Key::Q),
        )
    });
    let close_requested = ctx.input(|i| i.viewport().close_requested());
    match dialog_key(esc, ctrl_w, ctrl_q, close_requested) {
        Some(DialogKey::Close) => close = true,
        Some(DialogKey::Quit) => {
            ctx.send_viewport_cmd_to(egui::ViewportId::ROOT, egui::ViewportCommand::Close);
        }
        None => {}
    }

    Dialog { inner, close }
}

/// The fallback for a backend without multiple viewports.
fn embedded<R: Default>(
    ctx: &Context,
    title: &str,
    add_contents: impl FnOnce(&mut Ui) -> R,
) -> Dialog<R> {
    let mut open = true;
    let inner = egui::Window::new(title)
        .collapsible(false)
        .resizable(false)
        .open(&mut open)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, add_contents)
        .and_then(|response| response.inner)
        .unwrap_or_default();
    Dialog {
        inner,
        close: !open,
    }
}

/// The header of a detached window: the title, the ✕, and a drag handle
/// underneath both. Returns true when ✕ was clicked.
fn header(ui: &mut Ui, title: &str) -> bool {
    let bar = egui::Rect::from_min_size(
        ui.cursor().min,
        egui::vec2(ui.available_width(), theme::ICON_BUTTON),
    );
    drag_region(ui, bar, "grove-dialog-titlebar");

    let mut close = false;
    ui.horizontal(|ui| {
        ui.set_min_height(theme::ICON_BUTTON);
        ui.label(theme::label(title, theme::FONT_TITLE, theme::TEXT_STRONG).strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if icons::button(ui, true, icons::cross)
                .on_hover_text("Close (Esc)")
                .clicked()
            {
                close = true;
            }
        });
    });
    close
}

/// Turn `bar` into the viewport's drag handle.
///
/// The region is interacted with *first*, which in egui puts it below anything
/// drawn into the same strip afterwards: a click on a control is never a drag.
pub fn drag_region(ui: &mut Ui, bar: egui::Rect, id_salt: &str) {
    let drag = ui.interact(bar, ui.id().with(id_salt), egui::Sense::click_and_drag());
    if drag.drag_started() {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }
}

/// The mockup's divider, in the background layer so panels paint under it.
fn hairline(ctx: &Context, at: egui::Pos2, width: f32) {
    ctx.layer_painter(egui::LayerId::new(
        egui::Order::Background,
        egui::Id::new("grove-dialog-hairline"),
    ))
    .hline(
        at.x..=(at.x + width),
        at.y - 0.5,
        egui::Stroke::new(1.0, theme::HAIRLINE),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Form {
        id: String,
        typed: String,
    }

    fn form(id: &str) -> Form {
        Form {
            id: id.to_string(),
            typed: String::new(),
        }
    }

    #[test]
    fn a_fresh_window_is_closed_and_wants_no_focus() {
        let mut window: Detached<Form> = Detached::default();
        assert!(!window.is_open());
        assert!(window.get().is_none());
        assert!(!window.take_focus_request(), "nothing to raise");
        window.request_focus();
        assert!(
            !window.take_focus_request(),
            "a closed window is not raised"
        );
    }

    #[test]
    fn opening_asks_for_the_window_to_be_raised_exactly_once() {
        let mut window = Detached::default();
        window.open(form("p1"));
        assert!(window.is_open());
        assert!(window.take_focus_request());
        assert!(!window.take_focus_request(), "the request is consumed");
    }

    /// The one-instance rule: asking again for the dialog already on screen
    /// raises it and keeps what the user has typed.
    #[test]
    fn re_opening_the_same_dialog_focuses_instead_of_duplicating() {
        let mut window = Detached::default();
        window.open(form("p1"));
        window.take_focus_request();
        if let Some(open) = window.get_mut() {
            open.typed = "feature/auth".into();
        }

        let installed = window.open_or_focus(form("p1"), |open| open.id == "p1");
        assert!(!installed, "the open window is the one that was asked for");
        assert!(window.take_focus_request(), "it is raised instead");
        assert_eq!(
            window.get().map(|f| f.typed.as_str()),
            Some("feature/auth"),
            "the user's typing survives"
        );
    }

    #[test]
    fn asking_for_a_different_subject_replaces_the_content() {
        let mut window = Detached::default();
        window.open(form("p1"));
        window.take_focus_request();

        let installed = window.open_or_focus(form("p2"), |open| open.id == "p2");
        assert!(installed, "the caller must reload for the new subject");
        assert_eq!(window.get().map(|f| f.id.as_str()), Some("p2"));
        assert!(window.take_focus_request());
    }

    #[test]
    fn opening_a_closed_window_through_open_or_focus_installs_the_form() {
        let mut window: Detached<Form> = Detached::default();
        assert!(window.open_or_focus(form("p1"), |_| true));
        assert!(window.is_open());
    }

    #[test]
    fn closing_is_idempotent_and_drops_any_pending_focus() {
        let mut window = Detached::default();
        window.open(form("p1"));
        window.close();
        window.close();
        assert!(!window.is_open());
        assert!(!window.take_focus_request());
    }

    // ------------------------------------------------- the keyboard policy

    #[test]
    fn esc_ctrl_w_and_the_compositor_all_close_the_dialog_window() {
        assert_eq!(
            dialog_key(true, false, false, false),
            Some(DialogKey::Close),
            "Esc"
        );
        assert_eq!(
            dialog_key(false, true, false, false),
            Some(DialogKey::Close),
            "Ctrl+W"
        );
        assert_eq!(
            dialog_key(false, false, false, true),
            Some(DialogKey::Close),
            "the compositor asked"
        );
    }

    #[test]
    fn ctrl_q_quits_grove_from_a_dialog_window() {
        assert_eq!(dialog_key(false, false, true, false), Some(DialogKey::Quit));
    }

    #[test]
    fn quitting_wins_over_closing_in_the_same_frame() {
        assert_eq!(dialog_key(true, true, true, true), Some(DialogKey::Quit));
    }

    #[test]
    fn an_idle_frame_asks_for_nothing() {
        assert_eq!(dialog_key(false, false, false, false), None);
    }

    // ------------------------------------------- driving a real egui context
    //
    // The policy above only matters if the window actually reads the keyboard
    // after its panels have had their say. These run one headless frame of a
    // detached window and look at what came out.

    fn key(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    /// One frame of a detached window: whether it asked to close, and every
    /// viewport command it emitted.
    fn frame(
        modifiers: egui::Modifiers,
        events: Vec<egui::Event>,
    ) -> (bool, Vec<(egui::ViewportId, egui::ViewportCommand)>) {
        let ctx = Context::default();
        let mut close = false;
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(520.0, 400.0),
                )),
                // `InputState::modifiers` comes from here, not from the events.
                modifiers,
                events,
                ..Default::default()
            },
            |ctx| {
                let dialog = show(ctx, ViewportClass::Immediate, "Settings", |ui| {
                    ui.label("content");
                });
                close = dialog.close;
            },
        );
        let commands = output
            .viewport_output
            .iter()
            .flat_map(|(id, viewport)| viewport.commands.iter().map(|c| (*id, c.clone())))
            .collect();
        (close, commands)
    }

    #[test]
    fn a_quiet_frame_leaves_the_window_open() {
        let (close, _) = frame(egui::Modifiers::default(), Vec::new());
        assert!(!close);
    }

    #[test]
    fn esc_reaches_the_window_through_its_panels() {
        let (close, _) = frame(
            egui::Modifiers::default(),
            vec![key(egui::Key::Escape, egui::Modifiers::default())],
        );
        assert!(close);
    }

    #[test]
    fn ctrl_q_in_a_dialog_closes_the_main_viewport_and_not_the_dialog() {
        let (close, commands) = frame(
            egui::Modifiers::COMMAND,
            vec![key(egui::Key::Q, egui::Modifiers::COMMAND)],
        );
        assert!(!close, "the dialog does not tear itself down; Grove exits");
        assert!(
            commands
                .iter()
                .any(|(id, command)| *id == egui::ViewportId::ROOT
                    && matches!(command, egui::ViewportCommand::Close)),
            "Ctrl+Q must ask the root viewport to close: {commands:?}"
        );
    }
}
