//! The vocabulary the UI and the workers speak.
//!
//! Every request the UI can make and every answer it can receive, in one file
//! and with no behaviour in it. This is the contract between the egui thread
//! and everything that must not run on it, so it is worth being able to read
//! the whole of it without the handlers in the way.

use std::collections::HashMap;
use std::path::PathBuf;

use grove_core::Error;
use grove_core::config::LoadedConfig;
use grove_core::config_write::Edit;
use grove_core::git::{RefEntry, StatusSummary, WorktreeAdd};
use grove_core::ipc::Notification;
use grove_core::model::{Project, SessionPresence, Worktree};
use grove_core::protocol;
use grove_core::reconcile::{ProjectRef, Reconciliation};
use grove_core::removal::RemovalReport;
use grove_core::state::State;
use grove_core::status::SessionReport;
use grove_core::tmux::WindowInfo;
use grove_core::workflow::{Activation, NewWindow};
use grove_harness::claude::HookChange;

/// Work requested by the UI.
#[derive(Debug)]
pub enum Task {
    /// Load the daemon-owned state used to bootstrap the GUI.
    LoadState,
    /// Load `config.toml`, auto-detecting a terminal on first run.
    LoadConfig,
    /// Register the project containing this path.
    OpenProject {
        path: PathBuf,
        idempotency_key: String,
    },
    /// Re-read a project's worktrees and sessions.
    RefreshProject {
        project_id: String,
    },
    /// Re-read the working-tree status of a project's worktrees. Queued
    /// after a refresh and after every git operation Grove performs.
    RefreshStatuses {
        project_id: String,
    },
    /// Re-read session presence only.
    RefreshSessions,
    /// Startup / refresh / restore reconciliation (ARCHITECTURE.md §7): diff
    /// Grove's index against `git worktree list` and `tmux list-sessions`.
    /// Marks; never deletes.
    Reconcile {
        projects: Vec<ProjectRef>,
    },
    /// Open an existing session by name — how an orphaned session is looked at
    /// before the user decides what to do with it. Creates nothing.
    OpenSession {
        session: String,
        idempotency_key: String,
    },
    /// Adopt an orphaned session as a worktree's session: rename and re-stamp
    /// its `@grove_*` options. Nothing is created or killed.
    AssociateSession {
        worktree_id: String,
        /// The orphan's current session name.
        session: String,
        idempotency_key: String,
    },
    /// Close an orphaned session, after its own confirmation. This is the
    /// tmux-session operation of the four, and never accompanies another.
    CloseOrphan {
        session: String,
        idempotency_key: String,
    },
    /// Unset `@grove_attention` on a session the user has just opened.
    ///
    /// The in-memory latch is cleared on the UI thread; this clears the
    /// durable half, which is what would otherwise re-raise attention on the
    /// next poll or after a restart.
    ClearAttention {
        worktree_id: String,
        idempotency_key: String,
    },
    /// Open a worktree: ensure the session, then switch or launch.
    Activate {
        worktree_id: String,
        idempotency_key: String,
    },
    /// Open one window of a worktree's session: ensure the session, select the
    /// window, then switch or launch.
    ActivateWindow {
        worktree_id: String,
        window_index: u32,
        idempotency_key: String,
    },
    /// Attach an additional terminal without retargeting the primary client.
    OpenInNewTerminal {
        worktree_id: String,
        idempotency_key: String,
    },
    /// Open an extra shell window inside a worktree's tmux session.
    OpenNewWindow {
        worktree_id: String,
        idempotency_key: String,
    },
    /// Start the configured agent in a worktree's `agent` window, either as a
    /// new conversation or resuming the one the agent last reported.
    StartAgent {
        worktree_id: String,
        /// The conversation to resume, when the user asked to resume one.
        resume: Option<String>,
        idempotency_key: String,
    },
    /// Bring back the conversations `state.toml` recorded, once per launch,
    /// in worktrees where no agent is running any more (DESIGN.md §11).
    ///
    /// Deciding needs one poll of the tmux server to know what is still
    /// running, so it happens here rather than on the UI thread — over the
    /// reconciled project list the UI already holds.
    ResumeAgents {
        idempotency_key: String,
    },
    /// Install or remove Grove's hooks in Claude Code's `settings.json`, or
    /// just look at what is there. File work, so never the UI thread.
    ClaudeHooks(HookOp),
    /// Local and remote-tracking branches for the create-worktree dialog.
    LoadBaseRefs {
        project_id: String,
    },
    /// `git worktree add`, then refresh, then optionally open the session.
    CreateWorktree {
        project_id: String,
        add: Box<WorktreeAdd>,
        open_after: bool,
        idempotency_key: String,
    },
    /// Gather the safe-removal risk report. Reads only; removes nothing.
    GatherRemoval {
        worktree_id: String,
    },
    /// Close one tmux session on the private server.
    CloseSession {
        project_id: String,
        worktree_id: String,
        idempotency_key: String,
    },
    /// `git worktree remove`. `force` only ever arrives from a second,
    /// explicit confirmation after git refused.
    RemoveWorktree {
        project_id: String,
        worktree_id: String,
        force: bool,
        idempotency_key: String,
    },
    /// `git branch -d`, or `-D` after a second explicit confirmation.
    DeleteBranch {
        project_id: String,
        branch: String,
        force: bool,
        idempotency_key: String,
    },
    /// Kill the private tmux server, after its own armed confirmation in the
    /// footer — every Grove session, and everything running inside one, ends.
    /// Never part of ordinary shutdown (FR-7: sessions outlive the GUI); only
    /// this explicit user action sends it.
    KillServer {
        idempotency_key: String,
    },
    SetProjectExpanded {
        project_id: String,
        expanded: bool,
    },
    RemoveProject {
        project_id: String,
    },
    AssignSlot {
        number: u8,
        worktree_id: String,
    },
    ClearSlot {
        worktree_id: String,
    },
    IgnoreSession {
        session: String,
    },
    ClearIgnoredSessions,
    /// Write the changed `config.toml` keys, then re-read the file.
    ///
    /// Surgical, key by key: the user's comments and formatting survive
    /// (ARCHITECTURE.md §4).
    SaveConfig(Vec<Edit>),
    /// Re-run terminal auto-detection for the settings pane. Writes nothing.
    DetectTerminal,
    /// Is this template's program on PATH? Filesystem work, so not the UI's.
    ProbeTerminal(String),
    /// Hand a path to the desktop (`xdg-open`), detached.
    OpenWithDesktop(PathBuf),
    /// Ask the desktop's directory picker for a path.
    PickDirectory {
        target: PickTarget,
        start: Option<PathBuf>,
    },
}

/// What to do about Grove's hooks in Claude Code's settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookOp {
    /// Read the file and report what is installed. Writes nothing.
    Check,
    Install,
    Uninstall,
}

/// Which path field a picked directory belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickTarget {
    /// The open-project dialog's repository path.
    ProjectPath,
    /// The create-worktree dialog's directory.
    WorktreePath,
    /// The settings pane's default worktree parent.
    WorktreeParent,
}

/// One of the destructive operations the removal dialog offers separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemovalOp {
    CloseSession,
    RemoveWorktree,
    DeleteBranch,
}

impl RemovalOp {
    pub fn label(self) -> &'static str {
        match self {
            RemovalOp::CloseSession => "close the tmux session",
            RemovalOp::RemoveWorktree => "remove the git worktree",
            RemovalOp::DeleteBranch => "delete the branch",
        }
    }
}

/// A failure to show in the UI, with git's or tmux's own output preserved.
#[derive(Debug, Clone)]
pub struct ErrorReport {
    pub summary: String,
    pub detail: Option<String>,
}

impl ErrorReport {
    pub fn new(context: &str, error: &Error) -> Self {
        Self {
            summary: format!("{context}: {error}"),
            detail: error.diagnostics(),
        }
    }
}

/// Results sent back to the UI.
#[derive(Debug)]
pub enum Message {
    /// The authoritative daemon state used to bootstrap the GUI.
    StateLoaded(Box<State>),
    /// The authoritative state after one narrow mutation.
    StateUpdated {
        state: Box<State>,
        status: String,
        reconcile: bool,
    },
    ConfigLoaded {
        loaded: Box<LoadedConfig>,
    },
    ProjectOpened(Box<Project>),
    WorktreesRefreshed {
        project_id: String,
        worktrees: Vec<Worktree>,
    },
    StatusesRefreshed {
        project_id: String,
        statuses: HashMap<String, StatusSummary>,
    },
    /// Session presence and each session's windows, both keyed by tmux session
    /// name. They travel together because a row's child rows must not lag its
    /// "no session" line.
    SessionsRefreshed {
        presence: HashMap<String, SessionPresence>,
        windows: HashMap<String, Vec<WindowInfo>>,
    },
    /// One reconciliation pass: every project's rows, plus the orphaned
    /// sessions the user is being offered a choice about.
    Reconciled {
        result: Box<Reconciliation>,
        state: Box<State>,
    },
    /// An orphaned session was opened; nothing about the index changed.
    SessionOpened {
        activation: Activation,
    },
    /// An orphaned session was adopted by a worktree.
    Associated {
        worktree_id: String,
        session: String,
    },
    /// An orphaned session was closed, on its own confirmation.
    OrphanClosed {
        session: String,
    },
    /// Startup resumed the conversations whose agents were gone. Carries the
    /// worktree ids so the rows can be selected as they come back, and is
    /// sent even when it resumed nothing: that is the answer to "did anything
    /// happen?", and silence would look like a failure.
    AgentsResumed {
        worktree_ids: Vec<String>,
    },
    /// An agent was started in a session's `agent` window.
    AgentStarted {
        worktree_id: String,
        /// The systemd scope it runs in, when resource accounting is on.
        unit: Option<String>,
    },
    /// One poll of the status engine, keyed by worktree id. Sent by the
    /// poller thread, not by the worker.
    StatusPolled(HashMap<String, SessionReport>),
    /// The windows every Grove session has, keyed by tmux session name. Comes
    /// from the same poll, so a window opened inside tmux shows up too.
    WindowsPolled(HashMap<String, Vec<WindowInfo>>),
    /// The git-status cadence elapsed. The poller cannot run git itself: it
    /// has no worktree lists, so the UI turns this into per-project refreshes.
    GitStatusDue,
    /// A `grove toggle` arrived over the socket: with a number, the worktree
    /// carrying it is selected and its session opened; without one, the window
    /// is the subject.
    Toggled {
        slot: Option<u8>,
    },
    /// An explicit `grove notify` report arrived over the socket.
    Notified(Box<Notification>),
    /// A revisioned update streamed by the persistent service. Polling and
    /// direct task responses remain the recovery path if this connection is
    /// interrupted or a bounded subscriber queue overflows.
    ServiceEvent(Box<protocol::Event>),
    /// Baseline revision returned when a subscription is established. Events
    /// begin after this point, and a reconnect may belong to a restarted
    /// service whose counter starts over.
    ServiceEventsStarted {
        revision: u64,
    },
    /// The subscription disconnected or could not start. This is not a user
    /// error: request an authoritative poll while the listener reconnects.
    ServiceEventsUnavailable,
    /// Claude Code's hook configuration, after a check, an install or a
    /// removal.
    ClaudeHooks {
        op: HookOp,
        change: Box<HookChange>,
    },
    Activated {
        worktree_id: String,
        activation: Activation,
    },
    /// An extra shell window was opened inside a worktree's session.
    WindowOpened {
        worktree_id: String,
        window: NewWindow,
    },
    BaseRefsLoaded {
        project_id: String,
        refs: Vec<RefEntry>,
        current: Option<String>,
    },
    WorktreeCreated {
        project_id: String,
        path: PathBuf,
    },
    RemovalGathered {
        project_id: String,
        worktree_id: String,
        report: Box<RemovalReport>,
    },
    /// A destructive operation succeeded. The dialog stays open: the other
    /// three operations are still the user's to make, one at a time.
    RemovalDone {
        project_id: String,
        operation: RemovalOp,
        detail: String,
    },
    /// A destructive operation failed. The dialog shows git's own refusal and
    /// only then offers the forced variant.
    RemovalFailed {
        project_id: String,
        operation: RemovalOp,
        report: ErrorReport,
    },
    /// The private tmux server is down. Grove quits when this arrives — and
    /// only then, so a failed kill leaves the app running with its error
    /// shown instead of silently abandoning live sessions.
    ServerKilled,
    /// `config.toml` was written. The reloaded config follows as
    /// [`Message::ConfigLoaded`].
    ConfigSaved {
        path: PathBuf,
    },
    /// Auto-detection's answer, for the settings pane's revert action.
    TerminalDetected {
        template: String,
    },
    /// The result of a PATH probe, with the template it was run for.
    TerminalProbed {
        command: String,
        program: String,
        found: bool,
    },
    /// The user chose a directory in the desktop's picker.
    DirectoryPicked {
        target: PickTarget,
        path: PathBuf,
    },
    Failed(ErrorReport),
}
