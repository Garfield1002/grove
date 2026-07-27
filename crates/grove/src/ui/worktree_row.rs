//! The tree's leaf row, laid out as in direction 1c: a left accent edge, a
//! session dot, a name over a muted sublabel, and quiet right-aligned markers
//! for a locked or detached worktree.
//!
//! A leaf is one tmux window. A worktree with a single window *is* that row —
//! it takes the worktree's name, so nothing about the common case changed when
//! windows became part of the tree. A worktree with more gets [`header`], the
//! same dropdown a project has, with one leaf row per window under it.

use egui::{Align, Layout, Sense, Stroke, StrokeKind, Ui, vec2};
use grove_core::model::{Project, SessionPresence, Worktree};
use grove_core::status::SessionStatus;
use grove_core::tmux::WindowInfo;

use super::{icons, theme};

/// What a row stands for: it decides the row's name, and which menu entries
/// have nowhere else to live.
#[derive(Debug, Clone, Copy)]
pub enum Stands<'a> {
    /// The worktree itself, named after its branch. Its single window, if it
    /// has one, is this row.
    Worktree,
    /// The worktree standing in for its whole project, which has only this
    /// one. The row takes the project's name and carries its menu.
    Project(&'a Project),
    /// One window of a worktree that has several, named after the window.
    Window(&'a WindowInfo),
}

impl<'a> Stands<'a> {
    /// The project this row also stands for, if any.
    fn as_project(self) -> Option<&'a Project> {
        match self {
            Stands::Project(project) => Some(project),
            _ => None,
        }
    }

    fn as_window(self) -> Option<&'a WindowInfo> {
        match self {
            Stands::Window(window) => Some(window),
            _ => None,
        }
    }

    /// The row's name.
    fn name(self, worktree: &Worktree) -> String {
        match self {
            Stands::Worktree => worktree.label(),
            Stands::Project(project) => project.name.clone(),
            Stands::Window(window) => format!("{}: {}", window.index, window.name),
        }
    }
}

/// What the user did on a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowAction {
    Activate,
    Select,
    OpenInNewTerminal,
    OpenNewWindow,
    StartAgent,
    Refresh,
    Remove,
    /// Only offered on a row standing in for its project, which is the only
    /// place those two have no header of their own to hang off.
    CreateWorktree,
    RemoveProject,
    /// Put a number on this worktree, so `grove toggle <n>` opens it, or take
    /// the one it has off (`None`).
    SetSlot(Option<u8>),
    /// Fold or unfold a worktree's window list. Handled in the list itself:
    /// egui owns that state, and nothing outside the tree cares.
    Fold,
}

impl RowAction {
    /// What this row action means for the project and worktree it came from.
    ///
    /// `None` for the actions the list handles without leaving the UI.
    pub fn into_action(self, project_id: &str, worktree_id: &str) -> Option<super::Action> {
        use super::Action;
        let project_id = project_id.to_string();
        let worktree_id = worktree_id.to_string();
        Some(match self {
            RowAction::Activate => Action::ActivateWorktree {
                project_id,
                worktree_id,
            },
            RowAction::Select => Action::SelectWorktree {
                project_id,
                worktree_id,
            },
            RowAction::OpenInNewTerminal => Action::OpenInNewTerminal {
                project_id,
                worktree_id,
            },
            RowAction::OpenNewWindow => Action::OpenNewWindow {
                project_id,
                worktree_id,
            },
            RowAction::StartAgent => Action::StartAgent {
                project_id,
                worktree_id,
            },
            RowAction::Refresh => Action::RefreshProject(project_id),
            RowAction::Remove => Action::RemoveWorktree {
                project_id,
                worktree_id,
            },
            RowAction::CreateWorktree => Action::CreateWorktree(project_id),
            RowAction::RemoveProject => Action::RemoveProject(project_id),
            RowAction::SetSlot(slot) => Action::SetWorktreeSlot { worktree_id, slot },
            RowAction::Fold => return None,
        })
    }
}

/// A quiet right-edge marker. Only ever built from something git reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Marker {
    /// The worktree directory is gone (DESIGN.md §11). Nothing was removed —
    /// the row is marked, and stays.
    Unavailable,
    Locked,
    Detached,
}

impl Marker {
    fn hint(self) -> &'static str {
        match self {
            Marker::Unavailable => "the worktree directory is missing",
            Marker::Locked => "locked",
            Marker::Detached => "detached HEAD",
        }
    }

    fn color(self) -> egui::Color32 {
        match self {
            Marker::Locked | Marker::Detached => theme::TEXT_MUTED,
            Marker::Unavailable => theme::WARNING,
        }
    }

    fn draw(self, painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
        match self {
            Marker::Unavailable => icons::warning(painter, rect, color),
            Marker::Locked => icons::lock(painter, rect, color),
            Marker::Detached => icons::unlink(painter, rect, color),
        }
    }
}

/// At most four markers, held inline so drawing a row allocates nothing.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Markers {
    items: [Option<Marker>; 4],
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
/// The processor glyph beside the resource figures. Small: it labels the
/// numbers rather than competing with them.
const CPU_ICON: f32 = 9.0;
/// How far one level of the tree sits inside the one above it. Small on
/// purpose: enough to read as "under this", not so much that the names stop
/// lining up down the list.
pub const INDENT_STEP: f32 = 14.0;

/// The left inset of a row at this depth. Depth 0 is a row with no header
/// above it — the top level, whatever it happens to stand for.
pub fn indent(depth: u8) -> f32 {
    f32::from(depth) * INDENT_STEP
}

/// Draw a leaf row for a worktree, named after whatever it stands for.
///
/// `slot` is the number the user put on the worktree, if any — passed as
/// `None` for a window row, which stands for a window and not a worktree.
pub fn show(
    ui: &mut Ui,
    worktree: &Worktree,
    selected: bool,
    home: Option<&std::path::Path>,
    stands: Stands,
    depth: u8,
    slot: Option<u8>,
) -> Option<RowAction> {
    let project = stands.as_project();
    let mut action = None;
    let width = ui.available_width();
    let (outer, _) = ui.allocate_exact_size(vec2(width, theme::ROW_HEIGHT), Sense::hover());
    let rect = outer.with_min_x(outer.left() + indent(depth));
    // The row is interacted with at its drawn width, so the indent is a gutter
    // and not a click target that looks like nothing.
    let response = ui.interact(
        rect,
        ui.id().with((
            "grove-leaf",
            &worktree.id,
            stands.as_window().map(|window| window.index),
        )),
        Sense::click(),
    );
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
            // A 3 px-wide rect cannot carry a 9 px corner, so the edge is the
            // row's own rounded shape, clipped to the first few pixels: its
            // left corners then follow the row's curve exactly.
            let edge = egui::Rect::from_min_size(rect.min, vec2(theme::ROW_EDGE, rect.height()));
            painter
                .with_clip_rect(edge)
                .rect_filled(rect, radius, color);
        }

        let dot_center = egui::pos2(rect.left() + 18.0, rect.center().y);
        match (dot_presence(worktree, stands), worktree.status) {
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

        // The number the user put on this worktree, rightmost so the numbered
        // rows line up in a column of their own and the markers keep theirs.
        if let Some(number) = slot {
            marker_x = slot_badge(painter, number, marker_x, rect.center().y) - 5.0;
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
                stands.name(worktree),
                theme::FONT_BRANCH,
                name_color,
            ))
            .truncate()
            .selectable(false),
        );
        // On the selected row, the agent's own figures lead the sublabel: it
        // is the row the user is looking at, and putting them first means
        // truncation eats the path rather than the numbers.
        match resource_line(worktree, selected) {
            Some(resources) => {
                content.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    let (icon, _) =
                        ui.allocate_exact_size(egui::Vec2::splat(CPU_ICON), Sense::hover());
                    icons::cpu(ui.painter(), icon, theme::TEXT_MUTED);
                    ui.add(
                        egui::Label::new(theme::mono(resources, theme::FONT_SUB, theme::TEXT_DIM))
                            .selectable(false),
                    );
                    ui.add(
                        egui::Label::new(theme::label(
                            format!("· {}", sublabel(worktree, home, project.is_some())),
                            theme::FONT_SUB,
                            sublabel_color(worktree),
                        ))
                        .truncate()
                        .selectable(false),
                    );
                });
            }
            None => {
                content.add(
                    egui::Label::new(theme::label(
                        sublabel(worktree, home, project.is_some()),
                        theme::FONT_SUB,
                        sublabel_color(worktree),
                    ))
                    .truncate()
                    .selectable(false),
                );
            }
        }
    }

    if response.clicked() {
        action = Some(RowAction::Activate);
    }

    response.context_menu(|ui| menu(ui, worktree, stands, slot, &mut action));

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
        for line in hover_lines(worktree, slot) {
            ui.label(theme::label(line, theme::FONT_SMALL, theme::TEXT_MUTED));
        }
    });
    action
}

/// A worktree's dropdown header: the same shape as a project's, for a worktree
/// with more than one tmux window. The badge counts the windows under it.
///
/// It opens the worktree on click like a leaf row does — the disclosure
/// triangle is the affordance for folding it away, and a user who clicks the
/// name almost always means "give me this worktree".
pub fn header(
    ui: &mut Ui,
    worktree: &Worktree,
    stands: Stands,
    count: usize,
    openness: f32,
    depth: u8,
    slot: Option<u8>,
) -> Option<RowAction> {
    let mut action = None;
    let (outer, _) = ui.allocate_exact_size(
        vec2(ui.available_width(), theme::PROJECT_ROW_HEIGHT),
        Sense::hover(),
    );
    let rect = outer.with_min_x(outer.left() + indent(depth));
    let response = ui.interact(
        rect,
        ui.id().with(("grove-worktree-header", &worktree.id)),
        Sense::click(),
    );
    let hovered = response.hovered();

    // The disclosure triangle is its own target inside the header, so folding
    // and opening stay separate clicks.
    let disclosure_rect = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 10.0, rect.center().y),
        egui::Vec2::splat(18.0),
    );
    let disclosure = ui.interact(
        disclosure_rect,
        ui.id().with(("grove-worktree-fold", &worktree.id)),
        Sense::click(),
    );
    let more_rect = egui::Rect::from_center_size(
        egui::pos2(rect.right() - 11.0, rect.center().y),
        egui::Vec2::splat(18.0),
    );
    let more = ui.interact(
        more_rect,
        ui.id().with(("grove-worktree-more", &worktree.id)),
        Sense::click(),
    );

    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        if hovered {
            painter.rect_filled(
                rect,
                egui::CornerRadius::same(theme::ROW_RADIUS),
                theme::BADGE.gamma_multiply(0.6),
            );
        }

        icons::disclosure(
            painter,
            egui::Rect::from_center_size(disclosure_rect.center(), egui::Vec2::splat(9.0)),
            openness,
            theme::TEXT_MUTED,
        );

        // The status colour a leaf row would put on its edge is worth keeping
        // here: a folded worktree must still be able to say it wants the user.
        let name_color = match status_color(worktree) {
            Some(color) => color,
            None if worktree.session.exists() => theme::TEXT_DIM,
            None => theme::TEXT_MUTED,
        };
        let name = painter.layout_no_wrap(
            stands.name(worktree),
            egui::FontId::monospace(theme::FONT_PROJECT),
            name_color,
        );
        let name_left = rect.left() + 21.0;
        painter.galley(
            egui::pos2(name_left, rect.center().y - name.size().y / 2.0),
            name.clone(),
            name_color,
        );

        let badge = painter.layout_no_wrap(
            count.to_string(),
            egui::FontId::monospace(theme::FONT_SUB),
            theme::TEXT_GHOST,
        );
        let badge_rect = egui::Rect::from_center_size(
            egui::pos2(
                name_left + name.size().x + 8.0 + (badge.size().x + 12.0) / 2.0,
                rect.center().y,
            ),
            vec2(badge.size().x + 12.0, 15.0),
        );
        painter.rect_filled(
            badge_rect,
            egui::CornerRadius::same(theme::BADGE_RADIUS),
            theme::BADGE,
        );
        let badge_size = badge.size();
        painter.galley(
            badge_rect.center() - badge_size / 2.0,
            badge,
            theme::TEXT_GHOST,
        );

        // Left of the ellipsis, where a leaf row's would be: folding a
        // worktree away must not hide what `grove toggle <n>` opens.
        if let Some(number) = slot {
            slot_badge(painter, number, more_rect.left() - 4.0, rect.center().y);
        }

        if hovered || more.hovered() {
            icons::ellipsis(
                painter,
                more_rect.shrink(4.0),
                if more.hovered() {
                    theme::TEXT_DIM
                } else {
                    theme::TEXT_FAINT
                },
            );
        }
    }

    if disclosure.clicked() {
        action = Some(RowAction::Fold);
    } else if response.clicked() {
        action = Some(RowAction::Activate);
    }

    response.context_menu(|ui| menu(ui, worktree, stands, slot, &mut action));
    let more = more.on_hover_cursor(egui::CursorIcon::PointingHand);
    egui::Popup::menu(&more).show(|ui| menu(ui, worktree, stands, slot, &mut action));

    action
}

/// The menu a worktree row or header offers, wherever it was opened from.
fn menu(
    ui: &mut Ui,
    worktree: &Worktree,
    stands: Stands,
    slot: Option<u8>,
    action: &mut Option<RowAction>,
) {
    let open = match stands {
        Stands::Window(_) => "Open or switch to this window",
        _ => "Open or switch to session",
    };
    if ui.button(open).clicked() {
        *action = Some(RowAction::Activate);
        ui.close();
    }
    if ui.button("Open in a new terminal").clicked() {
        *action = Some(RowAction::OpenInNewTerminal);
        ui.close();
    }
    if ui.button("New tmux window").clicked() {
        *action = Some(RowAction::OpenNewWindow);
        ui.close();
    }
    if ui.button("Copy worktree path").clicked() {
        ui.ctx().copy_text(worktree.path.display().to_string());
        *action = Some(RowAction::Select);
        ui.close();
    }
    if ui.button("Start agent").clicked() {
        *action = Some(RowAction::StartAgent);
        ui.close();
    }
    if ui.button("Refresh").clicked() {
        *action = Some(RowAction::Refresh);
        ui.close();
    }
    // The keyboard is the fast path (Alt+<digit> on the selected row); this is
    // where the feature is discoverable at all. A window row is offered it too:
    // the number it would set is its worktree's, which is what the user means.
    ui.menu_button("Number for `grove toggle`", |ui| {
        ui.label(theme::label(
            "`grove toggle <n>` opens this worktree.",
            theme::FONT_SMALL,
            theme::TEXT_FAINT,
        ));
        for number in 1..=grove_core::state::MAX_SLOT {
            let held = slot == Some(number);
            let label = if held {
                format!("{number} ✓")
            } else {
                number.to_string()
            };
            if ui.button(label).clicked() {
                // Choosing the number it already carries takes it off, which
                // is what Alt+<digit> does too.
                *action = Some(RowAction::SetSlot((!held).then_some(number)));
                ui.close();
            }
        }
        if slot.is_some() {
            ui.separator();
            if ui.button("No number").clicked() {
                *action = Some(RowAction::SetSlot(None));
                ui.close();
            }
        }
    });
    // With no project header above it, this row is the only way to reach the
    // project's own actions.
    if let Some(project) = stands.as_project() {
        ui.separator();
        if ui.button("Create worktree…").clicked() {
            *action = Some(RowAction::CreateWorktree);
            ui.close();
        }
        if ui.button("Copy repository path").clicked() {
            ui.ctx()
                .copy_text(project.repository_path.display().to_string());
            *action = Some(RowAction::Select);
            ui.close();
        }
        if ui
            .button(theme::label(
                "Remove project from Grove",
                theme::FONT_BODY,
                theme::DANGER,
            ))
            .clicked()
        {
            *action = Some(RowAction::RemoveProject);
            ui.close();
        }
        ui.label(theme::label(
            "Removing a project only removes it from Grove.",
            theme::FONT_SUB,
            theme::TEXT_FAINT,
        ));
    }
    ui.separator();
    if ui
        .button(theme::label("Remove…", theme::FONT_BODY, theme::DANGER))
        .clicked()
    {
        *action = Some(RowAction::Remove);
        ui.close();
    }
    ui.label(theme::label(
        "Removal asks separately about the session, the worktree and the branch.",
        theme::FONT_SUB,
        theme::TEXT_FAINT,
    ));
}

/// Draw the number a worktree carries as a small badge whose right edge is at
/// `right`, and return its left edge so the caller can keep laying out
/// leftwards.
///
/// Shared by the leaf row and the header: a folded worktree must still show
/// what `grove toggle <n>` opens.
fn slot_badge(painter: &egui::Painter, number: u8, right: f32, center_y: f32) -> f32 {
    let digit = painter.layout_no_wrap(
        number.to_string(),
        egui::FontId::monospace(theme::FONT_SUB),
        theme::TEXT_GHOST,
    );
    let width = digit.size().x + 10.0;
    let left = right - width;
    let badge =
        egui::Rect::from_center_size(egui::pos2(left + width / 2.0, center_y), vec2(width, 15.0));
    painter.rect_filled(
        badge,
        egui::CornerRadius::same(theme::BADGE_RADIUS),
        theme::BADGE,
    );
    let size = digit.size();
    painter.galley(badge.center() - size / 2.0, digit, theme::TEXT_GHOST);
    left
}

/// What the dot on a row reports.
///
/// A window row's dot says whether that window is the session's current one —
/// the one thing a per-window row can say that the worktree's own presence
/// cannot. A live status still outranks it: attention and work are why the dot
/// exists at all.
fn dot_presence(worktree: &Worktree, stands: Stands) -> SessionPresence {
    match stands.as_window() {
        Some(window) if window.active => SessionPresence::Attached,
        Some(_) => SessionPresence::Detached,
        None => worktree.session,
    }
}

/// Tooltip lines after the path: the git summary, the row's number, and what
/// each marker means.
fn hover_lines(worktree: &Worktree, slot: Option<u8>) -> Vec<String> {
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
    // What the agent's own cgroup reports, when it has one.
    if let Some(resources) = worktree.resources.as_deref() {
        lines.push(resources.to_string());
    }
    if let Some(status) = &worktree.git_status {
        lines.push(status.summary());
    }
    if let Some(number) = slot {
        lines.push(format!("`grove toggle {number}` opens this worktree"));
    }
    for marker in markers(worktree).iter() {
        lines.push(marker.hint().to_string());
    }
    lines
}

/// Right-edge markers, worst first.
fn markers(worktree: &Worktree) -> Markers {
    let mut markers = Markers::default();
    if worktree.is_missing {
        markers.push(Marker::Unavailable);
    }
    if worktree.is_locked {
        markers.push(Marker::Locked);
    }
    if worktree.is_detached {
        markers.push(Marker::Detached);
    }
    // A dirty working tree earns no marker: the sublabel already counts the
    // modified and untracked files, and a dot beside it said the same thing
    // twice.
    markers
}

/// The resource figures to show inline, if any.
///
/// Only on the selected row: every row carrying live numbers would make the
/// list twitch, and the tooltip has them for the others. Only when there is a
/// scoped agent to measure — `resources` is `None` otherwise, which is not the
/// same as zero.
fn resource_line(worktree: &Worktree, selected: bool) -> Option<&str> {
    if !selected || !worktree.session.exists() {
        return None;
    }
    worktree.resources.as_deref()
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
///
/// A row standing in for its project leads with the branch, which its name
/// no longer says.
fn sublabel(worktree: &Worktree, home: Option<&std::path::Path>, collapsed: bool) -> String {
    let tail = format!("{} · {}", worktree.sublabel(), worktree.short_path(home));
    if collapsed {
        format!("{} · {tail}", worktree.label())
    } else {
        tail
    }
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
    fn resource_figures_show_on_the_selected_row_only() {
        let mut worktree = worktree();
        worktree.session = SessionPresence::Detached;
        worktree.resources = Some("64%  1.4G".into());

        assert_eq!(resource_line(&worktree, true), Some("64%  1.4G"));
        assert_eq!(
            resource_line(&worktree, false),
            None,
            "unselected rows stay still; the tooltip has the figures"
        );
    }

    #[test]
    fn a_row_with_no_scoped_agent_shows_no_figures() {
        let mut worktree = worktree();
        worktree.session = SessionPresence::Detached;
        // No resource accounting, or no agent: not the same as zero usage.
        worktree.resources = None;
        assert_eq!(resource_line(&worktree, true), None);

        // And a closed session never shows a leftover reading.
        worktree.resources = Some("64%  1.4G".into());
        worktree.session = SessionPresence::None;
        assert_eq!(resource_line(&worktree, true), None);
    }

    #[test]
    fn an_agents_message_leads_the_tooltip() {
        let mut worktree = worktree();
        worktree.session = SessionPresence::Detached;
        worktree.status_message = Some("needs permission to run tests".into());
        worktree.git_status = Some(StatusSummary::default());
        assert_eq!(
            hover_lines(&worktree, None).first().map(String::as_str),
            Some("needs permission to run tests")
        );
    }

    #[test]
    fn a_message_without_a_session_is_not_shown() {
        let mut worktree = worktree();
        worktree.session = SessionPresence::None;
        worktree.status_message = Some("stale".into());
        assert!(!hover_lines(&worktree, None).iter().any(|l| l == "stale"));
    }

    /// The row shows the number as a badge; the tooltip is where the badge is
    /// explained, since a bare digit says nothing about what it is for.
    #[test]
    fn a_numbered_row_says_what_the_number_does() {
        let worktree = worktree();
        assert!(
            hover_lines(&worktree, Some(3))
                .iter()
                .any(|line| line == "`grove toggle 3` opens this worktree")
        );
        assert!(
            !hover_lines(&worktree, None)
                .iter()
                .any(|line| line.contains("grove toggle")),
            "an unnumbered row says nothing about numbers"
        );
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
    fn locked_and_detached_each_get_a_marker() {
        let mut worktree = worktree();
        worktree.is_locked = true;
        worktree.is_detached = true;
        let hints: Vec<&str> = markers(&worktree).iter().map(Marker::hint).collect();
        assert_eq!(hints, vec!["locked", "detached HEAD"]);
    }

    /// A worktree whose directory has gone leads the markers: it is the one
    /// that explains why nothing else on the row can be trusted.
    #[test]
    fn a_missing_worktree_is_marked_first() {
        let mut worktree = worktree();
        worktree.is_missing = true;
        worktree.is_locked = true;
        let hints: Vec<&str> = markers(&worktree).iter().map(Marker::hint).collect();
        assert_eq!(hints, vec!["the worktree directory is missing", "locked"]);
        assert!(
            hover_lines(&worktree, None)
                .iter()
                .any(|line| line == "the worktree directory is missing")
        );
    }

    #[test]
    fn every_marker_fits_at_once() {
        let mut worktree = worktree();
        worktree.is_missing = true;
        worktree.is_locked = true;
        worktree.is_detached = true;
        assert_eq!(markers(&worktree).iter().count(), 3);
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
            sublabel(&worktree, Some(Path::new("/home/u")), false),
            "3 mod · 1 untracked · no session · ~/wt/auth"
        );
    }

    /// A row standing in for its project takes the project's name, so the
    /// branch it is on has to move into the sublabel.
    #[test]
    fn a_collapsed_row_keeps_its_branch_in_the_sublabel() {
        let worktree = worktree();
        assert_eq!(
            sublabel(&worktree, Some(Path::new("/home/u")), true),
            "feature/auth · no session · ~/wt/auth"
        );
    }

    /// Markers are drawn as vector shapes, never as font glyphs: the ones the
    /// design called for (`⚯`, `●`) are not in egui's proportional font chain.
    #[test]
    fn markers_fit_inline_and_stay_ordered() {
        let mut worktree = worktree();
        worktree.is_locked = true;
        worktree.is_detached = true;
        let list: Vec<Marker> = markers(&worktree).iter().collect();
        assert_eq!(list, vec![Marker::Locked, Marker::Detached]);
    }

    fn project() -> Project {
        Project {
            id: "p1".into(),
            name: "acme-web".into(),
            repository_path: PathBuf::from("/home/u/acme-web"),
            git_common_dir: PathBuf::from("/home/u/acme-web/.git"),
            default_worktree_path: PathBuf::from("/home/u"),
            is_expanded: true,
            worktrees: Vec::new(),
            unavailable: None,
        }
    }

    fn window(index: u32, name: &str, active: bool) -> WindowInfo {
        WindowInfo {
            session: "wt-a1b2c3".into(),
            index,
            name: name.into(),
            active,
            bell: false,
        }
    }

    /// The three things a leaf row can stand for, each with its own name.
    #[test]
    fn a_row_is_named_after_what_it_stands_for() {
        let worktree = worktree();
        let project = project();
        let window = window(2, "agent", true);
        assert_eq!(Stands::Worktree.name(&worktree), "feature/auth");
        assert_eq!(Stands::Project(&project).name(&worktree), "acme-web");
        assert_eq!(Stands::Window(&window).name(&worktree), "2: agent");
    }

    /// A window row's dot reports that window, not the session as a whole:
    /// several rows of one session would otherwise all claim to be attached.
    #[test]
    fn a_window_rows_dot_reports_the_current_window() {
        let mut worktree = worktree();
        worktree.session = SessionPresence::Attached;
        let current = window(0, "shell", true);
        let other = window(1, "shell", false);
        assert_eq!(
            dot_presence(&worktree, Stands::Window(&current)),
            SessionPresence::Attached
        );
        assert_eq!(
            dot_presence(&worktree, Stands::Window(&other)),
            SessionPresence::Detached
        );
        assert_eq!(
            dot_presence(&worktree, Stands::Worktree),
            SessionPresence::Attached,
            "a worktree row still reports its own session"
        );
    }

    /// Folding is the list's own business; every other row action leaves it.
    #[test]
    fn folding_is_not_an_app_action() {
        assert_eq!(RowAction::Fold.into_action("p1", "a1b2c3"), None);
        assert!(RowAction::Activate.into_action("p1", "a1b2c3").is_some());
    }

    /// The dirty dot is gone: the sublabel counts the files instead.
    #[test]
    fn a_dirty_working_tree_earns_no_marker() {
        let mut worktree = worktree();
        worktree.git_status = Some(StatusSummary {
            modified: 1,
            ..StatusSummary::default()
        });
        assert!(markers(&worktree).is_empty());
        assert!(
            !hover_lines(&worktree, None)
                .iter()
                .any(|line| line == "uncommitted changes")
        );
    }
}
