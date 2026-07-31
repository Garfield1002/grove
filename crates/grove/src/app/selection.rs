//! What the list has selected, and what the keyboard does to it.
//!
//! Two fields with one rule between them: exactly one row in the tree is ever
//! drawn as selected, so a window's highlight survives only while its own
//! worktree is the selected one. That rule used to be a stanza at the end of
//! `apply_action`, which meant it held for selections made by a click or a
//! keystroke and not for the half-dozen made while draining worker messages —
//! an agent starting, a session opening, a worktree being created. Here it is
//! [`Selection::select`]'s business, and there is no way to set one field
//! without it.
//!
//! The filter sits alongside them because it decides which rows exist to be
//! walked, not because it shares their invariant.

use grove_core::model::Project;

use crate::ui;

#[derive(Default)]
pub(super) struct Selection {
    worktree: Option<String>,
    /// The window row the user last opened, as (worktree id, window index).
    window: Option<(String, u32)>,
    /// The filter field's text. No invariant ties it to the two above; it is
    /// here because it decides what [`Selection::rows`] can walk.
    pub(super) filter: String,
}

impl Selection {
    pub(super) fn worktree(&self) -> Option<&str> {
        self.worktree.as_deref()
    }

    pub(super) fn window(&self) -> Option<(&str, u32)> {
        self.window
            .as_ref()
            .map(|(id, index)| (id.as_str(), *index))
    }

    /// Select a worktree. A window highlight belonging to another worktree is
    /// dropped: two rows must never both look selected.
    pub(super) fn select(&mut self, worktree_id: String) {
        if self
            .window
            .as_ref()
            .is_some_and(|(id, _)| id != &worktree_id)
        {
            self.window = None;
        }
        self.worktree = Some(worktree_id);
    }

    /// Select one of a worktree's window rows, and the worktree with it.
    pub(super) fn select_window(&mut self, worktree_id: String, index: u32) {
        self.window = Some((worktree_id.clone(), index));
        self.worktree = Some(worktree_id);
    }

    /// The worktree rows the user can see, as (project id, worktree id) pairs,
    /// in list order. Collapsed projects and filtered-out rows are not
    /// walkable.
    pub(super) fn rows(&self, projects: &[Project]) -> Vec<(String, String)> {
        visible_rows(projects, &self.filter)
    }

    /// The selected row, if it is one of the rows currently on screen.
    pub(super) fn row(&self, projects: &[Project]) -> Option<(String, String)> {
        let selected = self.worktree.as_ref()?;
        self.rows(projects)
            .into_iter()
            .find(|(_, worktree_id)| worktree_id == selected)
    }

    /// The project a keyboard shortcut acts on: the selected row's project,
    /// else the only project, else nothing.
    pub(super) fn context_project(&self, projects: &[Project]) -> Option<String> {
        if let Some((project_id, _)) = self.row(projects) {
            return Some(project_id);
        }
        match projects {
            [only] => Some(only.id.clone()),
            _ => None,
        }
    }

    /// Move the selection by one row, without wrapping at either end.
    pub(super) fn move_by(&mut self, projects: &[Project], delta: isize) {
        let rows = self.rows(projects);
        if let Some(next) = next_selection(&rows, self.worktree.as_deref(), delta) {
            self.select(next);
        }
    }
}

/// The worktree rows the user can see, as (project id, worktree id) pairs, in
/// list order.
fn visible_rows(projects: &[Project], filter: &str) -> Vec<(String, String)> {
    let needle = filter.trim().to_ascii_lowercase();
    let mut rows = Vec::new();
    for project in projects {
        if !project.is_expanded {
            continue;
        }
        for worktree in &project.worktrees {
            if ui::project_list::matches_filter(project, worktree, &needle) {
                rows.push((project.id.clone(), worktree.id.clone()));
            }
        }
    }
    rows
}

/// The worktree id Up/Down should move to. `None` when there is nothing to
/// select. The ends do not wrap: a held arrow key stops at the list edge.
fn next_selection(
    rows: &[(String, String)],
    selected: Option<&str>,
    delta: isize,
) -> Option<String> {
    if rows.is_empty() {
        return None;
    }
    let current = selected.and_then(|id| rows.iter().position(|(_, w)| w == id));
    let next = match current {
        Some(index) => (index as isize + delta).clamp(0, rows.len() as isize - 1) as usize,
        // Nothing selected yet: Down starts at the top, Up at the bottom.
        None if delta < 0 => rows.len() - 1,
        None => 0,
    };
    Some(rows[next].1.clone())
}

/// The 1..=9 digit pressed with Alt this frame, if any.
///
/// Zero is not among them: the numbers are 1–9 (`state::MAX_SLOT`), and Alt+0
/// meaning nothing is better than it quietly meaning something else.
pub(super) fn pressed_digit(input: &egui::InputState) -> Option<u8> {
    if !input.modifiers.alt {
        return None;
    }
    const DIGITS: [(egui::Key, u8); 9] = [
        (egui::Key::Num1, 1),
        (egui::Key::Num2, 2),
        (egui::Key::Num3, 3),
        (egui::Key::Num4, 4),
        (egui::Key::Num5, 5),
        (egui::Key::Num6, 6),
        (egui::Key::Num7, 7),
        (egui::Key::Num8, 8),
        (egui::Key::Num9, 9),
    ];
    DIGITS
        .iter()
        .find(|(key, _)| input.key_pressed(*key))
        .map(|(_, digit)| *digit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use grove_core::git::WorktreeEntry;
    use grove_core::model::Worktree;
    use std::path::PathBuf;

    fn project(id: &str, name: &str, branches: &[&str]) -> Project {
        let git_common_dir = PathBuf::from(format!("/home/u/{name}/.git"));
        Project {
            id: id.to_string(),
            name: name.to_string(),
            repository_path: PathBuf::from(format!("/home/u/{name}")),
            git_common_dir: git_common_dir.clone(),
            default_worktree_path: PathBuf::from("/home/u"),
            is_expanded: true,
            worktrees: branches
                .iter()
                .map(|branch| {
                    Worktree::from_entry(
                        &WorktreeEntry {
                            path: PathBuf::from(format!("/home/u/wt/{name}-{branch}")),
                            branch: Some((*branch).to_string()),
                            ..WorktreeEntry::default()
                        },
                        id,
                        &git_common_dir,
                        false,
                    )
                })
                .collect(),
            unavailable: None,
        }
    }

    fn ids(rows: &[(String, String)]) -> Vec<String> {
        rows.iter().map(|(_, w)| w.clone()).collect()
    }

    #[test]
    fn alt_and_a_digit_is_what_assigns_a_number() {
        assert_eq!(digit_press(egui::Key::Num3, egui::Modifiers::ALT), Some(3));
        assert_eq!(digit_press(egui::Key::Num9, egui::Modifiers::ALT), Some(9));
        // Without Alt the digits belong to the filter field.
        assert_eq!(digit_press(egui::Key::Num3, egui::Modifiers::NONE), None);
        assert_eq!(digit_press(egui::Key::Num3, egui::Modifiers::COMMAND), None);
        // Zero is not a number a worktree can carry.
        assert_eq!(digit_press(egui::Key::Num0, egui::Modifiers::ALT), None);
    }

    /// Run one headless frame carrying a single key press, and ask what
    /// `pressed_digit` made of it.
    fn digit_press(key: egui::Key, modifiers: egui::Modifiers) -> Option<u8> {
        let ctx = egui::Context::default();
        let mut digit = None;
        let _ = ctx.run(
            egui::RawInput {
                // `InputState::modifiers` comes from here, not from the events.
                modifiers,
                events: vec![egui::Event::Key {
                    key,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers,
                }],
                ..Default::default()
            },
            |ctx| digit = ctx.input(pressed_digit),
        );
        digit
    }

    #[test]
    fn visible_rows_follow_the_list_order_across_projects() {
        let projects = vec![
            project("p1", "acme", &["main", "feature"]),
            project("p2", "design", &["main"]),
        ];
        let rows = visible_rows(&projects, "");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].0, "p1");
        assert_eq!(rows[2].0, "p2");
    }

    #[test]
    fn a_collapsed_project_is_not_walkable() {
        let mut projects = vec![
            project("p1", "acme", &["main", "feature"]),
            project("p2", "design", &["main"]),
        ];
        projects[0].is_expanded = false;
        let rows = visible_rows(&projects, "");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "p2");
    }

    #[test]
    fn the_filter_narrows_what_the_keyboard_walks() {
        let projects = vec![project("p1", "acme", &["main", "feature/auth"])];
        let rows = visible_rows(&projects, "auth");
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].1, projects[0].worktrees[1].id,
            "only the matching row is selectable"
        );
    }

    #[test]
    fn selection_moves_one_row_at_a_time_and_stops_at_the_ends() {
        let projects = vec![project("p1", "acme", &["a", "b", "c"])];
        let rows = visible_rows(&projects, "");
        let all = ids(&rows);

        assert_eq!(
            next_selection(&rows, Some(&all[0]), 1).as_ref(),
            Some(&all[1])
        );
        assert_eq!(
            next_selection(&rows, Some(&all[1]), -1).as_ref(),
            Some(&all[0])
        );
        assert_eq!(
            next_selection(&rows, Some(&all[0]), -1).as_ref(),
            Some(&all[0]),
            "the top does not wrap to the bottom"
        );
        assert_eq!(
            next_selection(&rows, Some(&all[2]), 1).as_ref(),
            Some(&all[2]),
            "the bottom does not wrap to the top"
        );
    }

    #[test]
    fn with_nothing_selected_down_starts_at_the_top_and_up_at_the_bottom() {
        let projects = vec![project("p1", "acme", &["a", "b", "c"])];
        let rows = visible_rows(&projects, "");
        let all = ids(&rows);
        assert_eq!(next_selection(&rows, None, 1).as_ref(), Some(&all[0]));
        assert_eq!(next_selection(&rows, None, -1).as_ref(), Some(&all[2]));
    }

    #[test]
    fn a_selection_that_is_no_longer_visible_restarts_from_the_edge() {
        let projects = vec![project("p1", "acme", &["a", "b"])];
        let rows = visible_rows(&projects, "");
        let all = ids(&rows);
        assert_eq!(
            next_selection(&rows, Some("gone"), 1).as_ref(),
            Some(&all[0])
        );
    }

    #[test]
    fn an_empty_list_selects_nothing() {
        assert_eq!(next_selection(&[], None, 1), None);
        assert_eq!(next_selection(&[], Some("a1b2c3"), -1), None);
        assert!(visible_rows(&[], "").is_empty());
    }

    // ------------------------------------------------ one selected row only

    /// Selecting another worktree drops a window highlight belonging to the
    /// one being left, whatever route the selection took.
    #[test]
    fn a_window_highlight_does_not_outlive_its_own_worktree() {
        let mut selection = Selection::default();
        selection.select_window("a1b2c3".to_string(), 2);
        assert_eq!(selection.worktree(), Some("a1b2c3"));
        assert_eq!(selection.window(), Some(("a1b2c3", 2)));

        // Re-selecting the same worktree keeps it: the window is still its own.
        selection.select("a1b2c3".to_string());
        assert_eq!(selection.window(), Some(("a1b2c3", 2)));

        selection.select("d4e5f6".to_string());
        assert_eq!(selection.worktree(), Some("d4e5f6"));
        assert_eq!(
            selection.window(),
            None,
            "two rows must never both look selected"
        );
    }

    /// Walking the list with the arrow keys is a selection like any other, so
    /// it drops a window highlight too.
    #[test]
    fn moving_off_a_worktree_drops_its_window_highlight() {
        let projects = vec![project("p1", "acme", &["a", "b"])];
        let first = projects[0].worktrees[0].id.clone();
        let mut selection = Selection::default();
        selection.select_window(first, 1);

        selection.move_by(&projects, 1);
        assert_eq!(
            selection.worktree(),
            Some(projects[0].worktrees[1].id.as_str())
        );
        assert_eq!(selection.window(), None);
    }

    #[test]
    fn the_context_project_is_the_selected_row_then_the_only_project() {
        let projects = vec![project("p1", "acme", &["a"])];
        let selection = Selection::default();
        assert_eq!(
            selection.context_project(&projects),
            Some("p1".to_string()),
            "nothing selected, but there is only one project"
        );

        let two = vec![
            project("p1", "acme", &["a"]),
            project("p2", "design", &["a"]),
        ];
        assert_eq!(
            Selection::default().context_project(&two),
            None,
            "two projects and no selection names nothing"
        );

        let mut selection = Selection::default();
        selection.select(two[1].worktrees[0].id.clone());
        assert_eq!(selection.context_project(&two), Some("p2".to_string()));
    }

    /// A selected row that the filter has hidden is not the selected *row*:
    /// shortcuts act on what is on screen.
    #[test]
    fn a_row_the_filter_hides_is_not_the_selected_row() {
        let projects = vec![project("p1", "acme", &["main", "feature/auth"])];
        let mut selection = Selection::default();
        selection.select(projects[0].worktrees[0].id.clone());
        assert!(selection.row(&projects).is_some());

        selection.filter = "auth".to_string();
        assert!(selection.row(&projects).is_none());
    }
}
