//! The rows the window draws, and everything stamped onto them.
//!
//! Five things used to sit side by side as fields of `GroveApp`: the project
//! list, the daemon's state snapshot, the last polled statuses, the last known
//! tmux windows, and what agents have reported. They are not five independent
//! caches. Four of them are *stamped onto* the first, and every mutation of
//! any of them has to re-stamp the rest or the list starts lying — a refresh
//! blanks a status pill, a closed session keeps its windows, a note outlives
//! the window it described.
//!
//! Held as separate fields on a 2,400-line struct, that re-stamping was a
//! convention: whoever touched `projects` had to remember which of
//! `apply_session_statuses`, `apply_session_windows` and `apply_window_notes`
//! to call, and nothing failed if they forgot. Held here with private fields,
//! it is the type's own business — the mutators below re-stamp before they
//! return, and there is no way in from outside that skips it.
//!
//! Nothing here knows about egui, the worker, or the service. That is what
//! makes it testable, which the same logic on `GroveApp` was not: constructing
//! a `GroveApp` starts threads and a daemon.

use std::collections::{HashMap, HashSet};

use grove_core::git::StatusSummary;
use grove_core::ipc::Notification;
use grove_core::model::{Project, SessionPresence, Worktree};
use grove_core::notice::Notices;
use grove_core::reconcile::{ProjectRef, ProjectStatus};
use grove_core::state::{ProjectRecord, State};
use grove_core::status::{SessionReport, SessionStatus};
use grove_core::tmux::WindowInfo;

use crate::status_watch::WorktreeLabel;

#[derive(Default)]
pub(super) struct Rows {
    /// The rows themselves, in list order.
    projects: Vec<Project>,
    /// The daemon's last state snapshot. Read-only here: the service owns
    /// persistence and the GUI sends narrow mutations back to it.
    state: State,
    /// Last polled status per worktree id, kept so a refreshed worktree list
    /// shows its status immediately instead of blank until the next poll.
    statuses: HashMap<String, SessionReport>,
    /// Last known windows per tmux session name, kept for the same reason: a
    /// refreshed worktree list keeps its child rows instead of blinking empty.
    windows: HashMap<String, Vec<WindowInfo>>,
    /// What the last `grove notify` said, per worktree and per window. Held
    /// here rather than on the rows because a refresh rebuilds those, and a
    /// message has nothing to be re-derived from.
    notices: Notices,
}

impl Rows {
    // ------------------------------------------------------------- reading

    pub(super) fn projects(&self) -> &[Project] {
        &self.projects
    }

    pub(super) fn state(&self) -> &State {
        &self.state
    }

    pub(super) fn project(&self, project_id: &str) -> Option<&Project> {
        self.projects.iter().find(|p| p.id == project_id)
    }

    /// The worktree a (project, worktree) pair names, if both still exist.
    ///
    /// Every action that acts on a row resolves it this way first: a stale id
    /// from a click or a keystroke resolves to `None` and the action is
    /// dropped, rather than being applied to whatever is nearby.
    pub(super) fn worktree(&self, project_id: &str, worktree_id: &str) -> Option<&Worktree> {
        self.project(project_id)?.worktree(worktree_id)
    }

    /// What Grove knows about each project before git is consulted, for a
    /// reconciliation pass.
    pub(super) fn project_refs(&self) -> Vec<ProjectRef> {
        self.projects
            .iter()
            .map(|project| ProjectRef {
                id: project.id.clone(),
                name: project.name.clone(),
                repository_path: project.repository_path.clone(),
                git_common_dir: project.git_common_dir.clone(),
            })
            .collect()
    }

    /// What to call each worktree in a desktop notification.
    pub(super) fn labels(&self) -> HashMap<String, WorktreeLabel> {
        self.projects
            .iter()
            .flat_map(|project| {
                project.worktrees.iter().map(|worktree| {
                    (
                        worktree.id.clone(),
                        WorktreeLabel {
                            project: project.name.clone(),
                            worktree: worktree.label(),
                        },
                    )
                })
            })
            .collect()
    }

    /// The (project id, worktree id) a number points at, if it still points at
    /// a row Grove is listing.
    ///
    /// A number that names nothing resolves to `None` and is left alone: it is
    /// a stale label, and the worktree it named may simply be on a project
    /// that is currently unavailable.
    pub(super) fn slot_target(&self, slot: u8) -> Option<(String, String)> {
        let worktree_id = self.state.slot_worktree(slot)?;
        self.projects
            .iter()
            .find(|project| project.worktree(worktree_id).is_some())
            .map(|project| (project.id.clone(), worktree_id.to_string()))
    }

    // ------------------------------------------------------------- writing

    /// Register a project, or replace the record of one already open. Returns
    /// the line to show for it.
    pub(super) fn add_project(&mut self, project: Project) -> String {
        match self.projects.iter_mut().find(|p| p.id == project.id) {
            Some(existing) => {
                let line = format!("{} is already open", project.name);
                *existing = project;
                line
            }
            None => {
                let line = format!(
                    "Registered {} ({} worktrees)",
                    project.name,
                    project.worktrees.len()
                );
                self.projects.push(project);
                line
            }
        }
    }

    /// Replace one project's rows with a freshly listed set.
    pub(super) fn refresh_worktrees(&mut self, project_id: &str, worktrees: Vec<Worktree>) {
        if let Some(project) = self.projects.iter_mut().find(|p| p.id == project_id) {
            project.worktrees = worktrees;
            // git answered, so the project is there after all.
            project.unavailable = None;
        }
        self.restamp();
    }

    /// Stamp fresh git statuses onto a project's rows. A worktree with no
    /// reading keeps whatever it had: never blanked, never invented.
    pub(super) fn apply_git_statuses(
        &mut self,
        project_id: &str,
        statuses: &HashMap<String, StatusSummary>,
    ) {
        if let Some(project) = self.projects.iter_mut().find(|p| p.id == project_id) {
            grove_core::workflow::apply_statuses(&mut project.worktrees, statuses);
        }
    }

    pub(super) fn apply_presence(&mut self, presence: &HashMap<String, SessionPresence>) {
        for project in &mut self.projects {
            grove_core::workflow::apply_session_presence(&mut project.worktrees, presence);
        }
        // Presence just changed, so a row that lost its session must lose its
        // status — and its windows — with it rather than waiting for the next
        // poll.
        self.restamp();
    }

    pub(super) fn set_statuses(&mut self, statuses: HashMap<String, SessionReport>) {
        self.statuses = statuses;
        self.stamp_statuses();
    }

    pub(super) fn set_windows(&mut self, windows: HashMap<String, Vec<WindowInfo>>) {
        self.windows = windows;
        self.stamp_windows();
    }

    /// Replace the read-only cache of daemon state. On bootstrap this also
    /// creates the project rows; later updates preserve live Git/tmux data
    /// already attached to those rows.
    pub(super) fn apply_daemon_state(&mut self, state: State, bootstrap: bool) {
        if bootstrap {
            self.projects = state.projects.iter().map(project_from).collect();
        } else {
            self.projects
                .retain(|project| state.find(&project.id).is_some());
            for project in &mut self.projects {
                if let Some(record) = state.find(&project.id) {
                    project.name = record.name.clone();
                    project.repository_path = record.repository_path.clone();
                    project.git_common_dir = record.git_common_dir.clone();
                    project.default_worktree_path = record.default_worktree_path.clone();
                    project.is_expanded = record.is_expanded;
                }
            }
            for record in &state.projects {
                if self.projects.iter().any(|project| project.id == record.id) {
                    continue;
                }
                self.projects.push(project_from(record));
            }
        }
        self.state = state;
        self.mark_stopped_sessions();
    }

    /// Apply one reconciliation pass to the rows and the index.
    ///
    /// It marks and it records; it removes nothing. An unavailable project
    /// keeps its record *and* its last known rows — a project on an unplugged
    /// drive must not look as though its worktrees were deleted.
    pub(super) fn apply_reconciliation(&mut self, projects: Vec<ProjectStatus>, state: State) {
        // The service owns state persistence and returns the exact state
        // snapshot it reconciled and atomically wrote.
        self.state = state;
        for status in projects {
            let Some(project) = self.projects.iter_mut().find(|p| p.id == status.id) else {
                continue;
            };
            project.unavailable = status.unavailable;
            if project.unavailable.is_none() {
                project.worktrees = status.worktrees;
            }
        }
        self.forget_stale_notices();
        self.restamp();
    }

    /// An explicit `grove notify` report, applied to the rows straight away.
    ///
    /// The status itself is left to the poller, which re-reads tmux a moment
    /// later — except for attention and done, which would otherwise not show
    /// until that poll lands.
    pub(super) fn apply_notification(&mut self, notification: &Notification) {
        let worktree_id = notification.worktree_id.as_str();
        let state = notification.state;
        let reason = notification.reason;
        self.notices.record(notification);
        if state == SessionStatus::Attention || state == SessionStatus::Done {
            // Keep any resource figures the last poll produced; only the
            // status is being overridden here.
            let report = self.statuses.entry(worktree_id.to_string()).or_default();
            // Attention outranks a "done" that arrives while it is raised: the
            // agent finishing a turn does not answer the question it asked.
            if !(state == SessionStatus::Done && report.status == SessionStatus::Attention) {
                report.status = state;
            }
        }
        for project in &mut self.projects {
            if let Some(worktree) = project.worktrees.iter_mut().find(|w| w.id == worktree_id) {
                worktree.status_message = notification.message.clone();
                if state == SessionStatus::Attention && worktree.session.exists() {
                    worktree.status = Some(SessionStatus::Attention);
                    worktree.attention_reason = reason;
                }
                // Applied here as well as at the next poll so a row says
                // "done" the moment the agent says so. Waiting for the poller
                // would leave up to a full interval where the thing the user
                // is watching for has happened and the list does not show it.
                if state == SessionStatus::Done
                    && worktree.session.exists()
                    && worktree.status != Some(SessionStatus::Attention)
                {
                    worktree.status = Some(SessionStatus::Done);
                }
            }
        }
        self.stamp_window_notes();
    }

    /// Drop everything the GUI was holding about a worktree the user has just
    /// opened. The durable half — the tmux option — is the worker's.
    pub(super) fn clear_attention(&mut self, worktree_id: &str) {
        self.statuses.remove(worktree_id);
        // The messages explained a state the user has now gone and looked at,
        // per window as well as for the worktree.
        self.notices.clear(worktree_id);
        for project in &mut self.projects {
            if let Some(worktree) = project.worktrees.iter_mut().find(|w| w.id == worktree_id) {
                worktree.status = None;
                worktree.status_message = None;
                worktree.attention_reason = None;
                worktree.window_notes.clear();
            }
        }
    }

    /// Forget the last reading for a worktree whose session or worktree has
    /// just been removed, so its latch cannot outlive it until the next poll.
    pub(super) fn forget(&mut self, worktree_id: &str) {
        self.statuses.remove(worktree_id);
    }

    // ------------------------------------------------------------ stamping

    /// Re-derive everything the rows carry from the caches behind them.
    ///
    /// The single place the mutators above go through, so "which of the three
    /// stamps does this change need?" is not a question a caller can get
    /// wrong: the answer is always all of them.
    fn restamp(&mut self) {
        self.mark_stopped_sessions();
        self.stamp_statuses();
        self.stamp_windows();
    }

    /// Re-derive *stopped* from the session index for every row without a live
    /// session, so a refresh cannot downgrade "session stopped" to "no
    /// session".
    fn mark_stopped_sessions(&mut self) {
        for project in &mut self.projects {
            for worktree in &mut project.worktrees {
                worktree.session_stopped =
                    !worktree.session.exists() && self.state.session(&worktree.id).is_some();
            }
        }
    }

    fn stamp_statuses(&mut self) {
        for project in &mut self.projects {
            grove_core::workflow::apply_session_status(&mut project.worktrees, &self.statuses);
        }
    }

    fn stamp_windows(&mut self) {
        for project in &mut self.projects {
            grove_core::workflow::apply_session_windows(&mut project.worktrees, &self.windows);
        }
        // The notes hang off the windows, so they are restamped with them: a
        // note for a window that has closed must not outlive its row.
        self.stamp_window_notes();
    }

    fn stamp_window_notes(&mut self) {
        for project in &mut self.projects {
            grove_core::workflow::apply_window_notes(&mut project.worktrees, &self.notices);
        }
    }

    /// Drop reports for worktrees reconciliation no longer lists, so a Grove
    /// left open for weeks cannot accumulate them.
    ///
    /// Bookkeeping only, and deliberately not tied to `state.toml`: forgetting
    /// what an agent said about a row that is gone removes nothing anywhere.
    fn forget_stale_notices(&mut self) {
        if self.notices.is_empty() {
            return;
        }
        let live: HashSet<&str> = self
            .projects
            .iter()
            .flat_map(|project| project.worktrees.iter().map(|w| w.id.as_str()))
            .collect();
        self.notices.retain_ids(|id| live.contains(id));
    }
}

/// The row a project record describes, before git has been consulted.
fn project_from(record: &ProjectRecord) -> Project {
    Project {
        id: record.id.clone(),
        name: record.name.clone(),
        repository_path: record.repository_path.clone(),
        git_common_dir: record.git_common_dir.clone(),
        default_worktree_path: record.default_worktree_path.clone(),
        is_expanded: record.is_expanded,
        worktrees: Vec::new(),
        unavailable: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grove_core::git::WorktreeEntry;
    use grove_core::state::SessionRecord;
    use grove_core::status::AttentionReason;
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

    fn record(id: &str, name: &str) -> ProjectRecord {
        ProjectRecord {
            id: id.to_string(),
            name: name.to_string(),
            repository_path: PathBuf::from(format!("/home/u/{name}")),
            git_common_dir: PathBuf::from(format!("/home/u/{name}/.git")),
            default_worktree_path: PathBuf::from("/home/u"),
            is_expanded: true,
        }
    }

    /// One project's rows, with a live session on the first of them.
    fn rows_with(projects: Vec<Project>) -> Rows {
        Rows {
            projects,
            ..Rows::default()
        }
    }

    fn first_worktree(rows: &Rows) -> &Worktree {
        &rows.projects[0].worktrees[0]
    }

    /// Give a worktree a live tmux session, the way a presence poll does.
    ///
    /// Presence is keyed by *session name*, not by worktree id: `wt-<id>` is
    /// what tmux answers with.
    fn with_session(rows: &mut Rows, worktree_id: &str) {
        let name = rows
            .projects
            .iter()
            .flat_map(|project| project.worktrees.iter())
            .find(|worktree| worktree.id == worktree_id)
            .expect("a worktree to attach a session to")
            .session_name();
        rows.apply_presence(&HashMap::from([(name, SessionPresence::Detached)]));
    }

    // ------------------------------------------------------------- reading

    #[test]
    fn project_refs_carry_the_repository_identity_reconciliation_needs() {
        let rows = rows_with(vec![project("p1", "acme", &["main"])]);
        let refs = rows.project_refs();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].id, "p1");
        assert_eq!(refs[0].name, "acme");
        assert_eq!(refs[0].repository_path, PathBuf::from("/home/u/acme"));
        assert_eq!(
            refs[0].git_common_dir,
            PathBuf::from("/home/u/acme/.git"),
            "matching is by repository identity, not by name"
        );
        assert!(Rows::default().project_refs().is_empty());
    }

    #[test]
    fn a_number_resolves_to_its_row() {
        let mut rows = rows_with(vec![
            project("p1", "acme", &["main", "feature"]),
            project("p2", "design", &["main"]),
        ]);
        let target = rows.projects[1].worktrees[0].id.clone();
        rows.state.assign_slot(3, &target);
        assert_eq!(rows.slot_target(3), Some(("p2".to_string(), target)));
    }

    /// A number Grove cannot resolve must select nothing at all — never the
    /// nearest row, and never a row from another project.
    #[test]
    fn an_unassigned_or_stale_number_resolves_to_nothing() {
        let mut rows = rows_with(vec![project("p1", "acme", &["main"])]);
        assert_eq!(rows.slot_target(3), None, "never assigned");

        rows.state.assign_slot(3, "deadbe");
        assert_eq!(
            rows.slot_target(3),
            None,
            "points at a worktree Grove is not listing"
        );

        // A collapsed project still holds its rows: the number is about the
        // worktree, not about what the list happens to be showing.
        let id = rows.projects[0].worktrees[0].id.clone();
        rows.projects[0].is_expanded = false;
        rows.state.assign_slot(3, &id);
        assert!(rows.slot_target(3).is_some());
    }

    /// A stale id from a click or a keystroke resolves to nothing rather than
    /// to whatever is nearby.
    #[test]
    fn a_worktree_is_only_found_under_its_own_project() {
        let rows = rows_with(vec![
            project("p1", "acme", &["main"]),
            project("p2", "design", &["main"]),
        ]);
        let id = rows.projects[0].worktrees[0].id.clone();
        assert!(rows.worktree("p1", &id).is_some());
        assert!(
            rows.worktree("p2", &id).is_none(),
            "a worktree must not be found under another project"
        );
        assert!(rows.worktree("gone", &id).is_none());
        assert!(rows.worktree("p1", "deadbe").is_none());
    }

    #[test]
    fn labels_name_every_row_by_project_and_worktree() {
        let rows = rows_with(vec![project("p1", "acme", &["main", "feature"])]);
        let labels = rows.labels();
        assert_eq!(labels.len(), 2);
        let id = &rows.projects[0].worktrees[0].id;
        assert_eq!(labels[id].project, "acme");
        assert_eq!(labels[id].worktree, rows.projects[0].worktrees[0].label());
    }

    // ------------------------------------------------------------- writing

    #[test]
    fn opening_a_project_twice_replaces_its_record_rather_than_listing_it_twice() {
        let mut rows = Rows::default();
        assert_eq!(
            rows.add_project(project("p1", "acme", &["main"])),
            "Registered acme (1 worktrees)"
        );
        assert_eq!(
            rows.add_project(project("p1", "acme", &["main", "feature"])),
            "acme is already open"
        );
        assert_eq!(rows.projects.len(), 1);
        assert_eq!(rows.projects[0].worktrees.len(), 2);
    }

    /// A worktree with a session record but no live session is *stopped*, not
    /// "never started": the row stays usable and offers to open it again.
    #[test]
    fn a_session_grove_has_a_record_for_reads_as_stopped_not_absent() {
        let mut rows = rows_with(vec![project("p1", "acme", &["main"])]);
        let id = first_worktree(&rows).id.clone();
        assert!(!first_worktree(&rows).session_stopped, "no record yet");

        let mut state = State {
            projects: vec![record("p1", "acme")],
            ..State::default()
        };
        state.record_session(SessionRecord {
            worktree_id: id.clone(),
            project_id: "p1".to_string(),
            session_name: format!("wt-{id}"),
            ..SessionRecord::default()
        });
        rows.apply_daemon_state(state, false);
        assert!(
            first_worktree(&rows).session_stopped,
            "a record with no live session is a stopped session"
        );

        // And a live session is not "stopped".
        with_session(&mut rows, &id);
        assert!(!first_worktree(&rows).session_stopped);
    }

    /// The reason `Rows` owns all five caches: a refresh that only replaced
    /// the rows would blank every status pill until the next poll.
    #[test]
    fn refreshing_a_projects_rows_restamps_what_was_already_polled() {
        let mut rows = rows_with(vec![project("p1", "acme", &["main"])]);
        let id = first_worktree(&rows).id.clone();
        rows.set_statuses(HashMap::from([(
            id.clone(),
            SessionReport {
                status: SessionStatus::Working,
                ..SessionReport::default()
            },
        )]));
        with_session(&mut rows, &id);
        assert_eq!(first_worktree(&rows).status, Some(SessionStatus::Working));

        // A freshly listed set of rows carries no status of its own.
        let mut fresh = project("p1", "acme", &["main"]).worktrees;
        fresh[0].session = SessionPresence::Detached;
        rows.refresh_worktrees("p1", fresh);
        assert_eq!(
            first_worktree(&rows).status,
            Some(SessionStatus::Working),
            "the last poll is re-stamped, not thrown away"
        );
        assert!(rows.projects[0].unavailable.is_none());
    }

    // -------------------------------------------------------- notifications

    /// Attention outranks a "done" that arrives while it is raised: an agent
    /// finishing a turn does not answer the question it asked.
    #[test]
    fn done_does_not_overwrite_a_raised_attention() {
        let mut rows = rows_with(vec![project("p1", "acme", &["main"])]);
        let id = first_worktree(&rows).id.clone();
        with_session(&mut rows, &id);

        let mut attention = Notification::new(&id, SessionStatus::Attention);
        attention.reason = Some(AttentionReason::Blocked);
        rows.apply_notification(&attention);
        assert_eq!(first_worktree(&rows).status, Some(SessionStatus::Attention));
        assert_eq!(
            first_worktree(&rows).attention_reason,
            Some(AttentionReason::Blocked)
        );

        rows.apply_notification(&Notification::new(&id, SessionStatus::Done));
        assert_eq!(
            first_worktree(&rows).status,
            Some(SessionStatus::Attention),
            "done must not answer the question attention asked"
        );
        assert_eq!(
            rows.statuses[&id].status,
            SessionStatus::Attention,
            "nor in the cache the next poll reads"
        );
    }

    /// A row says "done" the moment the agent says so, rather than waiting up
    /// to a full poll interval.
    #[test]
    fn done_shows_at_once_on_a_row_with_a_session() {
        let mut rows = rows_with(vec![project("p1", "acme", &["main"])]);
        let id = first_worktree(&rows).id.clone();
        with_session(&mut rows, &id);

        let mut done = Notification::new(&id, SessionStatus::Done);
        done.message = Some("tests pass".to_string());
        rows.apply_notification(&done);
        assert_eq!(first_worktree(&rows).status, Some(SessionStatus::Done));
        assert_eq!(
            first_worktree(&rows).status_message.as_deref(),
            Some("tests pass")
        );
    }

    /// Status is never invented for a row with no session behind it.
    #[test]
    fn a_report_for_a_row_without_a_session_sets_no_status() {
        let mut rows = rows_with(vec![project("p1", "acme", &["main"])]);
        let id = first_worktree(&rows).id.clone();
        rows.apply_notification(&Notification::new(&id, SessionStatus::Attention));
        assert_eq!(
            first_worktree(&rows).status,
            None,
            "no session, so no status pill"
        );
    }

    /// Opening a session drops the whole latch, or the row would go on asking
    /// for a user who has just answered.
    #[test]
    fn opening_a_worktree_clears_everything_its_attention_was_carrying() {
        let mut rows = rows_with(vec![project("p1", "acme", &["main"])]);
        let id = first_worktree(&rows).id.clone();
        with_session(&mut rows, &id);

        let mut attention = Notification::new(&id, SessionStatus::Attention);
        attention.reason = Some(AttentionReason::WaitingInput);
        attention.message = Some("which branch?".to_string());
        rows.apply_notification(&attention);

        rows.clear_attention(&id);
        let worktree = first_worktree(&rows);
        assert_eq!(worktree.status, None);
        assert_eq!(worktree.status_message, None);
        assert_eq!(worktree.attention_reason, None);
        assert!(worktree.window_notes.is_empty());
        assert!(
            !rows.statuses.contains_key(&id),
            "the cache the next poll reads must forget it too"
        );
    }

    // ------------------------------------------------------- state & reconcile

    #[test]
    fn bootstrap_builds_the_rows_and_a_later_snapshot_keeps_their_git_data() {
        let mut rows = Rows::default();
        let state = State {
            projects: vec![record("p1", "acme"), record("p2", "design")],
            ..State::default()
        };
        rows.apply_daemon_state(state.clone(), true);
        assert_eq!(rows.projects.len(), 2);
        assert!(rows.projects[0].worktrees.is_empty());

        // Live git data arrives on the rows.
        rows.refresh_worktrees("p1", project("p1", "acme", &["main"]).worktrees);
        assert_eq!(rows.projects[0].worktrees.len(), 1);

        // A later snapshot renames the project without discarding those rows.
        let mut renamed = state.clone();
        renamed.projects[0].name = "acme-two".to_string();
        rows.apply_daemon_state(renamed, false);
        assert_eq!(rows.projects[0].name, "acme-two");
        assert_eq!(
            rows.projects[0].worktrees.len(),
            1,
            "a state update must not blank live git data"
        );
    }

    #[test]
    fn a_project_the_snapshot_drops_leaves_the_list_and_a_new_one_joins_it() {
        let mut rows = Rows::default();
        let state = State {
            projects: vec![record("p1", "acme")],
            ..State::default()
        };
        rows.apply_daemon_state(state, true);

        let next = State {
            projects: vec![record("p2", "design")],
            ..State::default()
        };
        rows.apply_daemon_state(next, false);
        assert_eq!(rows.projects.len(), 1);
        assert_eq!(rows.projects[0].id, "p2");
    }

    /// An unavailable project keeps its record *and* its last known rows: a
    /// project on an unplugged drive must not look as though its worktrees
    /// were deleted.
    #[test]
    fn reconciliation_marks_an_unavailable_project_without_emptying_it() {
        let mut rows = rows_with(vec![project("p1", "acme", &["main", "feature"])]);
        rows.apply_reconciliation(
            vec![ProjectStatus {
                id: "p1".to_string(),
                name: "acme".to_string(),
                unavailable: Some("the drive is not mounted".to_string()),
                worktrees: Vec::new(),
            }],
            State::default(),
        );
        assert_eq!(
            rows.projects[0].unavailable.as_deref(),
            Some("the drive is not mounted")
        );
        assert_eq!(
            rows.projects[0].worktrees.len(),
            2,
            "an unreadable project must keep the rows it last had"
        );
    }

    /// Reports for rows reconciliation no longer lists are dropped, so a Grove
    /// left open for weeks cannot accumulate them.
    #[test]
    fn reconciliation_forgets_reports_for_rows_that_are_gone() {
        let mut rows = rows_with(vec![project("p1", "acme", &["main"])]);
        let id = first_worktree(&rows).id.clone();
        rows.apply_notification(&Notification::new(&id, SessionStatus::Attention));
        assert!(!rows.notices.is_empty());

        rows.apply_reconciliation(
            vec![ProjectStatus {
                id: "p1".to_string(),
                name: "acme".to_string(),
                unavailable: None,
                worktrees: Vec::new(),
            }],
            State::default(),
        );
        assert!(
            rows.notices.worktree(&id).is_none(),
            "a report for a row that is gone is dropped"
        );
    }

    /// Reconciliation marks and records; it never removes a project.
    #[test]
    fn reconciliation_never_drops_a_project_it_was_not_told_about() {
        let mut rows = rows_with(vec![
            project("p1", "acme", &["main"]),
            project("p2", "design", &["main"]),
        ]);
        rows.apply_reconciliation(Vec::new(), State::default());
        assert_eq!(
            rows.projects.len(),
            2,
            "a pass that mentions nothing removes nothing"
        );
    }
}
