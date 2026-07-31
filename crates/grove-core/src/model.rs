//! Domain types shared by the core logic and the UI.

use std::path::{Path, PathBuf};

use crate::git::WorktreeEntry;
use crate::git::status::StatusSummary;
use crate::ids;
use crate::status::SessionStatus;
use crate::tmux::WindowInfo;

/// A registered Git repository.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    /// Main worktree, or the bare repository directory.
    pub repository_path: PathBuf,
    /// Repository identity; half of every worktree-id hash.
    pub git_common_dir: PathBuf,
    /// Directory new worktrees are created under by default. Only a default:
    /// the create dialog's path field is always editable.
    pub default_worktree_path: PathBuf,
    pub is_expanded: bool,
    pub worktrees: Vec<Worktree>,
    /// Why the project could not be read, when reconciliation could not read
    /// it: the directory moved, a drive is unavailable, git refused
    /// (DESIGN.md §11). `None` is the normal state. Being unavailable never
    /// removes anything — the record and the last known rows stay.
    pub unavailable: Option<String>,
}

impl Project {
    pub fn worktree(&self, id: &str) -> Option<&Worktree> {
        self.worktrees.iter().find(|w| w.id == id)
    }

    pub fn is_available(&self) -> bool {
        self.unavailable.is_none()
    }
}

/// Where a project's new worktrees go by default: the configured parent
/// directory when the user set one, else beside the repository itself.
pub fn default_worktree_parent(configured: Option<&Path>, repository_path: &Path) -> PathBuf {
    if let Some(configured) = configured.filter(|p| !p.as_os_str().is_empty()) {
        return configured.to_path_buf();
    }
    repository_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| repository_path.to_path_buf())
}

/// Directory name suggested for a branch: `feature/auth` -> `feature-auth`.
///
/// A suggestion only — the user edits the path before anything is created —
/// but it must never produce a name that walks out of the parent directory.
pub fn worktree_dir_name(branch: &str) -> String {
    let mut name = String::with_capacity(branch.len());
    for ch in branch.chars() {
        if ch.is_alphanumeric() || matches!(ch, '.' | '_' | '-' | '+') {
            name.push(ch);
        } else if !name.ends_with('-') {
            name.push('-');
        }
    }
    let name = name.trim_matches(['-', '.']).to_string();
    if name.is_empty() {
        "worktree".to_string()
    } else {
        name
    }
}

/// The path the create-worktree dialog starts with.
pub fn suggest_worktree_path(parent: &Path, branch: &str) -> PathBuf {
    parent.join(worktree_dir_name(branch))
}

/// Whether Grove has a tmux session for a worktree.
///
/// Milestone 1 only distinguishes "there is a session" from "there is not".
/// Working / idle / attention (DESIGN.md §6) arrive with the poller in
/// Milestone 4; until then no status is displayed, rather than a fake one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPresence {
    /// No session on the private server for this worktree.
    #[default]
    None,
    /// A session exists, with no client attached.
    Detached,
    /// A session exists and a terminal is attached to it.
    Attached,
}

impl SessionPresence {
    pub fn exists(self) -> bool {
        !matches!(self, SessionPresence::None)
    }

    /// Sublabel text for a worktree row.
    pub fn label(self) -> &'static str {
        match self {
            SessionPresence::None => "no session",
            SessionPresence::Detached => "session",
            SessionPresence::Attached => "session · attached",
        }
    }
}

/// What one tmux window last reported about itself, via `grove notify
/// --window`.
///
/// Only windows that have reported get one. That absence is meaningful: a
/// worktree with no notes at all has nothing reporting per window, and its
/// window rows show the session's status as they always did; once *any* window
/// has spoken, a window without a note is a window with nothing to say, and
/// the row says so by staying quiet rather than repeating its neighbours.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WindowNote {
    /// tmux window index.
    pub index: u32,
    pub status: SessionStatus,
    /// The agent's own sentence, when it sent one.
    pub message: Option<String>,
}

/// A Git worktree of a project.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Worktree {
    /// Deterministic id: 6 hex characters over (git-common-dir, path).
    pub id: String,
    pub project_id: String,
    pub path: PathBuf,
    /// `None` when detached or bare.
    pub branch: Option<String>,
    pub head_commit: Option<String>,
    pub is_main: bool,
    pub is_bare: bool,
    pub is_detached: bool,
    pub is_locked: bool,
    pub lock_reason: Option<String>,
    pub is_prunable: bool,
    pub prune_reason: Option<String>,
    /// The worktree directory is not there any more (DESIGN.md §11). Set by
    /// reconciliation, which marks it *unavailable* and removes nothing.
    pub is_missing: bool,
    /// Grove has a session record for this worktree but tmux no longer has the
    /// session: it is *stopped*, not "never started". The row stays usable and
    /// opening it starts a session again.
    pub session_stopped: bool,
    pub session: SessionPresence,
    /// The session's tmux windows, newest poll first-hand. Empty when there is
    /// no session, and also before the first poll lands — the tree shows no
    /// child rows rather than inventing a shell that may not be there.
    pub windows: Vec<WindowInfo>,
    /// Working-tree summary, filled in asynchronously by the worker. `None`
    /// means "not read yet", which the UI shows as nothing rather than as
    /// "clean".
    pub git_status: Option<StatusSummary>,
    /// Session status from the poller (Milestone 4). `None` means either that
    /// there is no session or that no poll has landed yet; both show no pill
    /// rather than a guessed one.
    pub status: Option<SessionStatus>,
    /// The message from the last `grove notify` for this worktree, shown while
    /// its status is still the one that message reported.
    pub status_message: Option<String>,
    /// What individual windows last reported about themselves, in index order.
    /// Empty means no window of this worktree has ever named itself, which is
    /// not the same as every window being quiet — see [`WindowNote`].
    pub window_notes: Vec<WindowNote>,
    /// RAM and CPU of the worktree's scoped agents, pre-rendered by the
    /// poller. `None` means there is no scoped agent — not that it uses
    /// nothing.
    pub resources: Option<String>,
}

impl Worktree {
    /// Build a worktree from a porcelain record.
    pub fn from_entry(
        entry: &WorktreeEntry,
        project_id: &str,
        git_common_dir: &Path,
        is_main: bool,
    ) -> Self {
        Self {
            id: ids::worktree_id(git_common_dir, &entry.path),
            project_id: project_id.to_string(),
            path: entry.path.clone(),
            branch: entry.branch.clone(),
            head_commit: entry.head.clone(),
            is_main,
            is_bare: entry.bare,
            is_detached: entry.detached,
            is_locked: entry.locked,
            lock_reason: entry.lock_reason.clone(),
            is_prunable: entry.prunable,
            prune_reason: entry.prune_reason.clone(),
            is_missing: false,
            session_stopped: false,
            session: SessionPresence::None,
            windows: Vec::new(),
            git_status: None,
            status: None,
            status_message: None,
            window_notes: Vec::new(),
            resources: None,
        }
    }

    /// tmux session name for this worktree.
    pub fn session_name(&self) -> String {
        ids::session_name(&self.id)
    }

    /// What window `index` last reported about itself, if it ever has.
    pub fn window_note(&self, index: u32) -> Option<&WindowNote> {
        self.window_notes.iter().find(|note| note.index == index)
    }

    /// The status to show for window `index`, if that window reports.
    ///
    /// A note refines the session's status; it never contradicts it. A window
    /// cannot be working while tmux says the whole session — every pane of it —
    /// has been quiet for longer than the working window, so the note is
    /// clamped to what the poller sees.
    ///
    /// That clamp is what makes a missed hook heal itself. An agent that
    /// reported starting a turn and never reported ending one — an interrupted
    /// turn, a crash, a Claude Code release that drops an event — would
    /// otherwise leave its row claiming work for as long as Grove ran. The
    /// poller has no such gap: it re-reads tmux every few seconds, and a quiet
    /// session is quiet whatever was last said about it.
    /// Attention is exempt: it is the one state Grove never infers and never
    /// withdraws on its own, and a window whose agent is waiting on the user
    /// is waiting whether or not anything has printed since.
    pub fn window_status(&self, index: u32) -> Option<SessionStatus> {
        let note = self.window_note(index)?;
        Some(match self.status {
            // Before the first poll there is nothing to clamp against, and the
            // report is the only thing known about the window.
            None => note.status,
            _ if note.status == SessionStatus::Attention => SessionStatus::Attention,
            Some(session) => note.status.min(session),
        })
    }

    /// Does anything about this worktree report per window?
    pub fn reports_per_window(&self) -> bool {
        !self.window_notes.is_empty()
    }

    /// Primary row label: the branch name, or a detached/bare marker.
    pub fn label(&self) -> String {
        if let Some(branch) = &self.branch {
            return branch.clone();
        }
        if self.is_bare {
            return "(bare)".to_string();
        }
        match &self.head_commit {
            Some(head) => format!("({})", &head[..head.len().min(7)]),
            None => "(no HEAD)".to_string(),
        }
    }

    /// Secondary row label: the git summary once it has been read, the
    /// session state, and anything unusual about the worktree itself.
    pub fn sublabel(&self) -> String {
        let mut parts = Vec::new();
        if let Some(status) = &self.git_status {
            parts.push(status.summary());
        }
        // The session status replaces the bare "session" label once a poll has
        // landed: "working" already says the session exists.
        match self.status.filter(|_| self.session.exists()) {
            Some(status) => parts.push(match self.session {
                SessionPresence::Attached => format!("{} · attached", status.label()),
                _ => status.label().to_string(),
            }),
            // A session Grove knew about and tmux no longer has is *stopped*;
            // saying "no session" would read as though there never was one.
            None if self.session_stopped && !self.session.exists() => {
                parts.push("session stopped".to_string())
            }
            None => parts.push(self.session.label().to_string()),
        }
        if self.is_missing {
            parts.push("unavailable".to_string());
        }
        if self.is_detached {
            parts.push("detached".to_string());
        }
        if self.is_locked {
            parts.push(match &self.lock_reason {
                Some(reason) => format!("locked: {reason}"),
                None => "locked".to_string(),
            });
        }
        if self.is_prunable {
            parts.push("prunable".to_string());
        }
        parts.join(" · ")
    }

    /// Compact path shown when it differs usefully from the branch name.
    pub fn short_path(&self, home: Option<&Path>) -> String {
        if let Some(home) = home
            && let Ok(rest) = self.path.strip_prefix(home)
        {
            return format!("~/{}", rest.display());
        }
        self.path.display().to_string()
    }
}

/// Build the worktree list of a project from porcelain records. The first
/// record git reports is always the main worktree.
pub fn worktrees_from_entries(
    entries: &[WorktreeEntry],
    project_id: &str,
    git_common_dir: &Path,
) -> Vec<Worktree> {
    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| Worktree::from_entry(entry, project_id, git_common_dir, index == 0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str) -> WorktreeEntry {
        WorktreeEntry {
            path: PathBuf::from(path),
            head: Some("0f2c8a1b3d4e5f60718293a4b5c6d7e8f9012345".into()),
            branch: Some("main".into()),
            ..WorktreeEntry::default()
        }
    }

    #[test]
    fn the_first_entry_is_the_main_worktree() {
        let entries = vec![entry("/home/u/proj"), entry("/home/u/wt/feature")];
        let worktrees = worktrees_from_entries(&entries, "p1", Path::new("/home/u/proj/.git"));
        assert!(worktrees[0].is_main);
        assert!(!worktrees[1].is_main);
        assert_ne!(worktrees[0].id, worktrees[1].id);
        assert_eq!(
            worktrees[0].session_name(),
            format!("wt-{}", worktrees[0].id)
        );
    }

    #[test]
    fn ids_depend_on_the_repository_not_just_the_path() {
        let entries = vec![entry("/home/u/shared")];
        let a = worktrees_from_entries(&entries, "p1", Path::new("/home/u/a/.git"));
        let b = worktrees_from_entries(&entries, "p2", Path::new("/home/u/b/.git"));
        assert_ne!(a[0].id, b[0].id);
    }

    /// A report that work started, with no report that it ended, must not
    /// outlive what tmux says about the session it came from.
    #[test]
    fn a_window_cannot_stay_working_in_a_quiet_session() {
        let mut worktree = Worktree::from_entry(&entry("/w"), "p", Path::new("/g"), true);
        worktree.window_notes = vec![WindowNote {
            index: 1,
            status: SessionStatus::Working,
            message: Some("running the tests".into()),
        }];

        assert_eq!(
            worktree.window_status(1),
            Some(SessionStatus::Working),
            "before the first poll the report is all there is"
        );

        worktree.status = Some(SessionStatus::Working);
        assert_eq!(worktree.window_status(1), Some(SessionStatus::Working));

        worktree.status = Some(SessionStatus::Idle);
        assert_eq!(
            worktree.window_status(1),
            Some(SessionStatus::Idle),
            "the session has been quiet for longer than the working window"
        );

        assert_eq!(worktree.window_status(0), None, "window 0 never reported");
    }

    /// The clamp is one-way: it can only quieten a window, never raise one.
    #[test]
    fn a_quiet_window_stays_quiet_in_a_busy_session() {
        let mut worktree = Worktree::from_entry(&entry("/w"), "p", Path::new("/g"), true);
        worktree.status = Some(SessionStatus::Attention);
        worktree.window_notes = vec![
            WindowNote {
                index: 1,
                status: SessionStatus::Idle,
                message: None,
            },
            WindowNote {
                index: 2,
                status: SessionStatus::Attention,
                message: Some("needs permission".into()),
            },
        ];
        assert_eq!(worktree.window_status(1), Some(SessionStatus::Idle));
        assert_eq!(worktree.window_status(2), Some(SessionStatus::Attention));

        // A quiet poll cannot take a window's attention away either: only the
        // user opening the session does that, and that clears the note too.
        worktree.status = Some(SessionStatus::Idle);
        assert_eq!(worktree.window_status(2), Some(SessionStatus::Attention));
    }

    #[test]
    fn labels_cover_branch_detached_and_bare() {
        let mut worktree = Worktree::from_entry(&entry("/w"), "p", Path::new("/g"), true);
        assert_eq!(worktree.label(), "main");

        worktree.branch = None;
        worktree.is_detached = true;
        assert_eq!(worktree.label(), "(0f2c8a1)");

        worktree.head_commit = None;
        assert_eq!(worktree.label(), "(no HEAD)");

        worktree.is_bare = true;
        assert_eq!(worktree.label(), "(bare)");
    }

    #[test]
    fn sublabels_lead_with_the_git_summary_once_it_is_known() {
        use crate::git::status::StatusSummary;
        let mut worktree = Worktree::from_entry(&entry("/w"), "p", Path::new("/g"), true);
        assert_eq!(
            worktree.sublabel(),
            "no session",
            "an unread status shows nothing, not `clean`"
        );

        worktree.git_status = Some(StatusSummary::default());
        assert_eq!(worktree.sublabel(), "clean · no session");

        worktree.git_status = Some(StatusSummary {
            modified: 3,
            untracked: 2,
            ..StatusSummary::default()
        });
        worktree.session = SessionPresence::Detached;
        assert_eq!(worktree.sublabel(), "3 mod · 2 untracked · session");
    }

    #[test]
    fn default_worktree_parent_prefers_the_configured_directory() {
        assert_eq!(
            default_worktree_parent(Some(Path::new("/home/u/worktrees")), Path::new("/home/u/p")),
            PathBuf::from("/home/u/worktrees")
        );
        assert_eq!(
            default_worktree_parent(None, Path::new("/home/u/projects/acme")),
            PathBuf::from("/home/u/projects")
        );
        assert_eq!(
            default_worktree_parent(Some(Path::new("")), Path::new("/home/u/projects/acme")),
            PathBuf::from("/home/u/projects")
        );
        // A repository at the filesystem root still yields a usable parent.
        assert_eq!(
            default_worktree_parent(None, Path::new("/")),
            PathBuf::from("/")
        );
    }

    #[test]
    fn suggested_directory_names_are_flat_and_cannot_escape_the_parent() {
        assert_eq!(worktree_dir_name("feature/auth"), "feature-auth");
        assert_eq!(worktree_dir_name("main"), "main");
        assert_eq!(worktree_dir_name("fix/v1.2_hot fix"), "fix-v1.2_hot-fix");
        assert_eq!(worktree_dir_name("../../etc"), "etc");
        assert_eq!(worktree_dir_name("//"), "worktree");
        assert_eq!(worktree_dir_name(""), "worktree");
        assert_eq!(worktree_dir_name("wörk/träe"), "wörk-träe");

        let path = suggest_worktree_path(Path::new("/home/u/wt"), "feature/auth");
        assert_eq!(path, PathBuf::from("/home/u/wt/feature-auth"));
        assert_eq!(
            suggest_worktree_path(Path::new("/home/u/wt"), "../escape"),
            PathBuf::from("/home/u/wt/escape")
        );
    }

    #[test]
    fn sublabels_report_session_presence_and_worktree_flags() {
        let mut worktree = Worktree::from_entry(&entry("/w"), "p", Path::new("/g"), true);
        assert_eq!(worktree.sublabel(), "no session");

        worktree.session = SessionPresence::Detached;
        assert_eq!(worktree.sublabel(), "session");

        worktree.session = SessionPresence::Attached;
        assert_eq!(worktree.sublabel(), "session · attached");

        worktree.is_detached = true;
        worktree.is_locked = true;
        worktree.lock_reason = Some("removable drive".into());
        worktree.is_prunable = true;
        assert_eq!(
            worktree.sublabel(),
            "session · attached · detached · locked: removable drive · prunable"
        );
    }

    #[test]
    fn a_polled_status_replaces_the_bare_session_label() {
        let mut worktree = Worktree::from_entry(&entry("/w"), "p", Path::new("/g"), true);
        worktree.session = SessionPresence::Detached;
        worktree.status = Some(SessionStatus::Working);
        assert_eq!(worktree.sublabel(), "working");

        worktree.session = SessionPresence::Attached;
        worktree.status = Some(SessionStatus::Attention);
        assert_eq!(worktree.sublabel(), "attention · attached");
    }

    #[test]
    fn a_status_without_a_session_is_never_shown() {
        let mut worktree = Worktree::from_entry(&entry("/w"), "p", Path::new("/g"), true);
        worktree.session = SessionPresence::None;
        worktree.status = Some(SessionStatus::Working);
        assert_eq!(worktree.sublabel(), "no session");
    }

    #[test]
    fn a_stopped_session_says_so_instead_of_no_session() {
        let mut worktree = Worktree::from_entry(&entry("/w"), "p", Path::new("/g"), true);
        worktree.session_stopped = true;
        assert_eq!(worktree.sublabel(), "session stopped");

        // Once tmux has it again, the record is history and the live state
        // wins — the row must never claim a running session is stopped.
        worktree.session = SessionPresence::Detached;
        assert_eq!(worktree.sublabel(), "session");
    }

    #[test]
    fn a_missing_worktree_is_labelled_unavailable() {
        let mut worktree = Worktree::from_entry(&entry("/w"), "p", Path::new("/g"), true);
        worktree.is_missing = true;
        assert_eq!(worktree.sublabel(), "no session · unavailable");
        worktree.session_stopped = true;
        assert_eq!(worktree.sublabel(), "session stopped · unavailable");
    }

    #[test]
    fn a_project_is_available_until_reconciliation_says_otherwise() {
        let mut project = Project {
            id: "p1".into(),
            name: "acme".into(),
            repository_path: PathBuf::from("/home/u/acme"),
            git_common_dir: PathBuf::from("/home/u/acme/.git"),
            default_worktree_path: PathBuf::from("/home/u"),
            is_expanded: true,
            worktrees: Vec::new(),
            unavailable: None,
        };
        assert!(project.is_available());
        project.unavailable = Some("the directory is gone".into());
        assert!(!project.is_available());
    }

    #[test]
    fn session_presence_reports_existence() {
        assert!(!SessionPresence::None.exists());
        assert!(SessionPresence::Detached.exists());
        assert!(SessionPresence::Attached.exists());
        assert_eq!(SessionPresence::default(), SessionPresence::None);
    }

    #[test]
    fn short_path_abbreviates_the_home_directory() {
        let worktree =
            Worktree::from_entry(&entry("/home/u/wt/feature"), "p", Path::new("/g"), false);
        assert_eq!(
            worktree.short_path(Some(Path::new("/home/u"))),
            "~/wt/feature"
        );
        assert_eq!(worktree.short_path(None), "/home/u/wt/feature");
        assert_eq!(
            worktree.short_path(Some(Path::new("/elsewhere"))),
            "/home/u/wt/feature"
        );
    }

    #[test]
    fn locked_and_prunable_flags_survive_conversion() {
        let entry = WorktreeEntry {
            locked: true,
            lock_reason: Some("on a usb stick".into()),
            prunable: true,
            prune_reason: Some("gitdir missing".into()),
            ..entry("/w")
        };
        let worktree = Worktree::from_entry(&entry, "p", Path::new("/g"), false);
        assert!(worktree.is_locked && worktree.is_prunable);
        assert_eq!(worktree.lock_reason.as_deref(), Some("on a usb stick"));
        assert_eq!(worktree.prune_reason.as_deref(), Some("gitdir missing"));
    }

    #[test]
    fn project_lookup_by_worktree_id() {
        let entries = vec![entry("/home/u/proj")];
        let worktrees = worktrees_from_entries(&entries, "p1", Path::new("/g"));
        let id = worktrees[0].id.clone();
        let project = Project {
            id: "p1".into(),
            name: "proj".into(),
            repository_path: PathBuf::from("/home/u/proj"),
            git_common_dir: PathBuf::from("/g"),
            default_worktree_path: PathBuf::from("/home/u"),
            is_expanded: true,
            worktrees,
            unavailable: None,
        };
        assert!(project.worktree(&id).is_some());
        assert!(project.worktree("zzzzzz").is_none());
    }
}
