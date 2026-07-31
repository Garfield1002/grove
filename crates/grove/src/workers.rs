//! Background worker.
//!
//! Every subprocess Grove runs happens here, on a single OS thread, never on
//! the egui thread (ARCHITECTURE.md §9). Work arrives as [`Task`] values over
//! an mpsc channel and results go back as [`Message`] values; after each send
//! the worker calls `Context::request_repaint` so the UI wakes up.
//!
//! There is no async runtime: the tasks are short blocking subprocess calls,
//! and serialising them on one thread also keeps concurrent git/tmux
//! invocations from racing each other.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};

use grove_core::claude::{self, HookChange};
use grove_core::config::{Config, LoadedConfig};
use grove_core::config_write::{self, Edit};
use grove_core::git::{RefEntry, StatusSummary, WorktreeAdd};
use grove_core::ipc::Notification;
use grove_core::model::{Project, SessionPresence, Worktree};
use grove_core::process::Invocation;
use grove_core::protocol::{self, Request};
use grove_core::reconcile::{ProjectRef, Reconciliation};
use grove_core::removal::RemovalReport;
use grove_core::state::State;
use grove_core::status::SessionReport;
use grove_core::tmux::WindowInfo;
use grove_core::workflow::{Activation, NewWindow};
use grove_core::{Error, Paths, TmuxServer, config, terminal};

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

/// Read or rewrite Claude Code's hook configuration.
///
/// The settings file is the user's, exactly as `config.toml` is: a copy is
/// taken before it is replaced, their own hooks survive, and a file Grove
/// cannot parse is reported rather than overwritten.
fn claude_hooks(op: HookOp) -> Result<HookChange, Error> {
    let path = claude::settings_path_from_env()?;
    match op {
        HookOp::Check => claude::hook_status(&path),
        HookOp::Install => claude::install_hooks(&path),
        HookOp::Uninstall => claude::uninstall_hooks(&path),
    }
}

/// Handle used by the UI to queue work.
pub struct Workers {
    tx: Sender<Task>,
    /// A spare sender for the message channel, so the poller and the notify
    /// listener can report into the same queue the UI already drains.
    messages: Sender<Message>,
}

impl Workers {
    /// Start the worker thread. Returns the handle and the receiving end of
    /// the result channel.
    pub fn start(paths: Paths, ctx: egui::Context) -> (Self, Receiver<Message>) {
        let (task_tx, task_rx) = channel::<Task>();
        let (msg_tx, msg_rx) = channel::<Message>();

        // If the thread cannot be spawned the UI still runs; tasks then queue
        // up unanswered rather than crashing the app.
        let own_tx = task_tx.clone();
        let worker_tx = msg_tx.clone();
        let spawned = std::thread::Builder::new()
            .name("grove-worker".into())
            .spawn(move || run(paths, task_rx, own_tx, worker_tx, ctx.clone()));
        if let Err(e) = spawned {
            eprintln!("grove: could not start the worker thread: {e}");
        }

        (
            Self {
                tx: task_tx,
                messages: msg_tx,
            },
            msg_rx,
        )
    }

    /// A sender for the UI's message queue, for the status threads.
    pub fn message_sender(&self) -> Sender<Message> {
        self.messages.clone()
    }

    /// Queue a task. A closed channel means the worker died; the UI keeps
    /// running and simply stops receiving updates.
    pub fn send(&self, task: Task) {
        if let Err(e) = self.tx.send(task) {
            eprintln!("grove: worker unavailable, dropping task: {e}");
        }
    }
}

struct WorkerState {
    paths: Paths,
    server: TmuxServer,
    config: Config,
    /// The worker's own end of the task channel, so a handler can queue a
    /// follow-up (status refresh after a git operation) without blocking the
    /// UI on it and without recursing.
    tasks: Sender<Task>,
}

impl WorkerState {
    fn enqueue(&self, task: Task) {
        let _ = self.tasks.send(task);
    }
}

const SERVICE_OPERATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

fn call_service(
    worker: &WorkerState,
    id: &str,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, Error> {
    let request = Request::new(id, method, params);
    let mut attempts = 0;
    let response = loop {
        match protocol::call_with_timeout(
            &worker.paths.notify_socket(),
            &request,
            SERVICE_OPERATION_TIMEOUT,
        ) {
            Ok(response) => break response,
            Err(error) if service_is_starting(&error) && attempts < 20 => {
                attempts += 1;
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(error) => return Err(service_error(method, error)),
        }
    };
    match (response.result, response.error) {
        (Some(result), None) if response.ok => Ok(result),
        (None, Some(error)) if !response.ok => Err(Error::io(
            format!("service method {method}"),
            std::io::Error::other(format!("{}: {}", error.code, error.message)),
        )),
        _ => Err(Error::io(
            format!("service method {method}"),
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "service returned an invalid response shape",
            ),
        )),
    }
}

fn service_is_starting(error: &protocol::Error) -> bool {
    matches!(
        error,
        protocol::Error::Io { context, source }
            if *context == "connect to Grove service"
                && matches!(
                    source.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                )
    )
}

fn service_error(method: &str, error: protocol::Error) -> Error {
    Error::io(
        format!("service method {method}"),
        std::io::Error::other(error.to_string()),
    )
}

fn load_state_through_service(worker: &WorkerState) -> Result<State, Error> {
    let value = call_service(
        worker,
        "gui-state-get",
        "state.get",
        serde_json::Value::Null,
    )?;
    serde_json::from_value(value).map_err(|error| {
        Error::io(
            "decode service state",
            std::io::Error::new(std::io::ErrorKind::InvalidData, error),
        )
    })
}

fn apply_state_intent(
    worker: &WorkerState,
    method: &str,
    params: serde_json::Value,
) -> Result<State, Error> {
    let value = call_service(worker, "gui-state-intent", method, params)?;
    value
        .get("state")
        .cloned()
        .ok_or_else(|| {
            Error::io(
                "decode service mutation",
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "service mutation response has no state",
                ),
            )
        })
        .and_then(|state| {
            serde_json::from_value(state).map_err(|error| {
                Error::io(
                    "decode service mutation state",
                    std::io::Error::new(std::io::ErrorKind::InvalidData, error),
                )
            })
        })
}

fn state_intent_messages(
    worker: &WorkerState,
    method: &str,
    params: serde_json::Value,
    context: &str,
    success: &str,
    reconcile: bool,
) -> Vec<Message> {
    match apply_state_intent(worker, method, params) {
        Ok(state) => vec![Message::StateUpdated {
            state: Box::new(state),
            status: success.to_string(),
            reconcile,
        }],
        Err(error) => vec![Message::Failed(ErrorReport::new(context, &error))],
    }
}

fn reconcile_through_service(
    worker: &WorkerState,
    projects: Vec<ProjectRef>,
) -> Result<(Reconciliation, State), Error> {
    let value = call_service(
        worker,
        "gui-reconcile",
        "state.reconcile",
        serde_json::json!({"projects": projects}),
    )?;
    #[derive(serde::Deserialize)]
    struct ServiceResult {
        reconciliation: Reconciliation,
        state: State,
    }
    serde_json::from_value::<ServiceResult>(value)
        .map(|result| (result.reconciliation, result.state))
        .map_err(|error| {
            Error::io(
                "decode service reconciliation",
                std::io::Error::new(std::io::ErrorKind::InvalidData, error),
            )
        })
}

fn run(
    paths: Paths,
    tasks: Receiver<Task>,
    task_tx: Sender<Task>,
    out: Sender<Message>,
    ctx: egui::Context,
) {
    // `-f` as well as `-S`: a private server started with `-S` alone would
    // still read the user's ~/.tmux.conf (ARCHITECTURE.md §2).
    let server = TmuxServer::new(paths.tmux_socket()).with_config(paths.tmux_config_file());
    let mut worker = WorkerState {
        paths,
        server,
        config: Config::default(),
        tasks: task_tx,
    };

    // The worker keeps its own sender, so this loop parks on `recv` until the
    // process exits rather than ending when the UI drops its handle. That is
    // deliberate: a handler must be able to queue its own follow-up work.
    while let Ok(task) = tasks.recv() {
        let messages = handle(&mut worker, task);
        for message in messages {
            if out.send(message).is_err() {
                return;
            }
        }
        ctx.request_repaint();
    }
}

fn handle(worker: &mut WorkerState, task: Task) -> Vec<Message> {
    match task {
        Task::LoadState => match load_state_through_service(worker) {
            Ok(state) => vec![Message::StateLoaded(Box::new(state))],
            Err(e) => vec![Message::Failed(ErrorReport::new(
                "could not load daemon state",
                &e,
            ))],
        },
        Task::LoadConfig => {
            // Generate tmux.conf on first run rather than waiting for the
            // first tmux command, so it is there to be edited straight away.
            let mut messages = match worker.server.ensure_config_file() {
                Ok(()) => Vec::new(),
                Err(e) => vec![Message::Failed(ErrorReport::new(
                    "could not create tmux.conf",
                    &e,
                ))],
            };
            messages.extend(
                match config::load_or_init(&worker.paths.config_file(), terminal::detect) {
                    Ok(loaded) => {
                        worker.config = loaded.config.clone();
                        vec![Message::ConfigLoaded {
                            loaded: Box::new(loaded),
                        }]
                    }
                    Err(e) => vec![Message::Failed(ErrorReport::new(
                        "could not load config.toml",
                        &e,
                    ))],
                },
            );
            messages
        }

        Task::OpenProject {
            path,
            idempotency_key,
        } => {
            #[derive(serde::Deserialize)]
            struct OpenProjectResult {
                project: Project,
            }
            match call_service(
                worker,
                "gui-project-open",
                "project.open",
                serde_json::json!({
                    "path": path,
                    "idempotency_key": idempotency_key,
                }),
            )
            .and_then(|value| {
                serde_json::from_value::<OpenProjectResult>(value).map_err(|error| {
                    Error::io(
                        "decode opened project",
                        std::io::Error::new(std::io::ErrorKind::InvalidData, error),
                    )
                })
            }) {
                Ok(result) => vec![Message::ProjectOpened(Box::new(result.project))],
                Err(e) => vec![Message::Failed(ErrorReport::new(
                    &format!("could not open {}", path.display()),
                    &e,
                ))],
            }
        }

        Task::RefreshProject { project_id } => {
            #[derive(serde::Deserialize)]
            struct RefreshProjectResult {
                project_id: String,
                worktrees: Vec<Worktree>,
                statuses: HashMap<String, StatusSummary>,
            }
            match call_service(
                worker,
                "gui-project-refresh",
                "project.refresh",
                serde_json::json!({"project_id": project_id}),
            )
            .and_then(|value| {
                serde_json::from_value::<RefreshProjectResult>(value).map_err(|error| {
                    Error::io(
                        "decode refreshed project",
                        std::io::Error::new(std::io::ErrorKind::InvalidData, error),
                    )
                })
            }) {
                Ok(result) => vec![
                    Message::WorktreesRefreshed {
                        project_id: result.project_id.clone(),
                        worktrees: result.worktrees,
                    },
                    Message::StatusesRefreshed {
                        project_id: result.project_id,
                        statuses: result.statuses,
                    },
                ],
                Err(e) => vec![Message::Failed(ErrorReport::new(
                    &format!("could not refresh project {project_id}"),
                    &e,
                ))],
            }
        }

        Task::RefreshStatuses { project_id } => {
            #[derive(serde::Deserialize)]
            struct ProjectStatusesResult {
                project_id: String,
                statuses: HashMap<String, StatusSummary>,
            }
            match call_service(
                worker,
                "gui-project-statuses",
                "project.statuses",
                serde_json::json!({"project_id": project_id}),
            )
            .and_then(|value| {
                serde_json::from_value::<ProjectStatusesResult>(value).map_err(|error| {
                    Error::io(
                        "decode project statuses",
                        std::io::Error::new(std::io::ErrorKind::InvalidData, error),
                    )
                })
            }) {
                Ok(result) => vec![Message::StatusesRefreshed {
                    project_id: result.project_id,
                    statuses: result.statuses,
                }],
                Err(e) => vec![Message::Failed(ErrorReport::new(
                    &format!("could not refresh statuses for project {project_id}"),
                    &e,
                ))],
            }
        }

        Task::StartAgent {
            worktree_id,
            resume,
            idempotency_key,
        } => {
            #[derive(serde::Deserialize)]
            struct StartResult {
                unit: Option<String>,
            }
            let result = call_service(
                worker,
                "gui-start-agent",
                "agent.start",
                serde_json::json!({
                    "worktree_id": worktree_id,
                    "resume": resume,
                    "idempotency_key": idempotency_key,
                }),
            )
            .and_then(|value| {
                serde_json::from_value::<StartResult>(value).map_err(|error| {
                    Error::io(
                        "decode started agent",
                        std::io::Error::new(std::io::ErrorKind::InvalidData, error),
                    )
                })
            });
            match result {
                Ok(result) => {
                    // The new window is activity tmux reports at once; a poll
                    // now is what makes the row react immediately.
                    worker.enqueue(Task::RefreshSessions);
                    vec![Message::AgentStarted {
                        worktree_id,
                        unit: result.unit,
                    }]
                }
                Err(e) => vec![Message::Failed(ErrorReport::new(
                    "could not start the agent",
                    &e,
                ))],
            }
        }

        Task::ResumeAgents { idempotency_key } => {
            #[derive(serde::Deserialize)]
            struct ResumeFailure {
                worktree_path: PathBuf,
                message: String,
            }
            #[derive(serde::Deserialize)]
            struct ResumeResult {
                worktree_ids: Vec<String>,
                failures: Vec<ResumeFailure>,
            }
            let result = call_service(
                worker,
                "gui-resume-agents",
                "agent.resume_recorded",
                serde_json::json!({"idempotency_key": idempotency_key}),
            )
            .and_then(|value| {
                serde_json::from_value::<ResumeResult>(value).map_err(|error| {
                    Error::io(
                        "decode resumed agents",
                        std::io::Error::new(std::io::ErrorKind::InvalidData, error),
                    )
                })
            });
            let ResumeResult {
                worktree_ids,
                failures,
            } = match result {
                Ok(result) => result,
                Err(error) => {
                    return vec![Message::Failed(ErrorReport::new(
                        "could not resume recorded agents",
                        &error,
                    ))];
                }
            };
            let mut messages = failures
                .into_iter()
                .map(|failure| {
                    let error = Error::io(
                        format!("resume the agent in {}", failure.worktree_path.display()),
                        std::io::Error::other(failure.message),
                    );
                    Message::Failed(ErrorReport::new(
                        &format!(
                            "could not resume the agent in {}",
                            failure.worktree_path.display()
                        ),
                        &error,
                    ))
                })
                .collect::<Vec<_>>();
            if !worktree_ids.is_empty() {
                worker.enqueue(Task::RefreshSessions);
            }
            messages.push(Message::AgentsResumed { worktree_ids });
            messages
        }

        Task::ClaudeHooks(op) => match claude_hooks(op) {
            Ok(change) => vec![Message::ClaudeHooks {
                op,
                change: Box::new(change),
            }],
            Err(e) => vec![Message::Failed(ErrorReport::new(
                "could not read Claude Code's settings.json",
                &e,
            ))],
        },

        Task::ClearAttention {
            worktree_id,
            idempotency_key,
        } => {
            match call_service(
                worker,
                "gui-clear-attention",
                "session.attention.clear",
                serde_json::json!({
                    "worktree_id": worktree_id,
                    "idempotency_key": idempotency_key,
                }),
            ) {
                // Nothing to report either way: the row already stopped
                // showing attention when the latch was cleared.
                Ok(_) => Vec::new(),
                Err(e) => vec![Message::Failed(ErrorReport::new(
                    "could not clear the session's attention marker",
                    &e,
                ))],
            }
        }

        Task::Reconcile { projects } => match reconcile_through_service(worker, projects) {
            Ok((result, state)) => {
                // Statuses are a second pass, exactly as for a refresh: the
                // restored list appears at once and the per-worktree
                // `git status` calls never hold it up.
                for project in &result.projects {
                    if !project.worktrees.is_empty() {
                        worker.enqueue(Task::RefreshStatuses {
                            project_id: project.id.clone(),
                        });
                    }
                }
                vec![Message::Reconciled {
                    result: Box::new(result),
                    state: Box::new(state),
                }]
            }
            Err(e) => vec![Message::Failed(ErrorReport::new(
                "could not reconcile with git and tmux",
                &e,
            ))],
        },

        Task::OpenSession {
            session,
            idempotency_key,
        } => {
            #[derive(serde::Deserialize)]
            struct OpenOrphanResult {
                activation: Activation,
            }
            match call_service(
                worker,
                "gui-open-orphan-session",
                "session.orphan.open",
                serde_json::json!({
                    "session": session,
                    "idempotency_key": idempotency_key,
                }),
            )
            .and_then(|value| {
                serde_json::from_value::<OpenOrphanResult>(value).map_err(|error| {
                    Error::io(
                        "decode opened orphan session",
                        std::io::Error::new(std::io::ErrorKind::InvalidData, error),
                    )
                })
            }) {
                Ok(result) => vec![Message::SessionOpened {
                    activation: result.activation,
                }],
                Err(e) => vec![Message::Failed(ErrorReport::new(
                    &format!("could not open {session}"),
                    &e,
                ))],
            }
        }

        Task::AssociateSession {
            worktree_id,
            session,
            idempotency_key,
        } => match call_service(
            worker,
            "gui-associate-session",
            "session.associate",
            serde_json::json!({
                "worktree_id": worktree_id,
                "orphan_session": session,
                "idempotency_key": idempotency_key,
            }),
        ) {
            Ok(value) => match value.get("session").and_then(serde_json::Value::as_str) {
                Some(name) => vec![Message::Associated {
                    worktree_id,
                    session: name.to_string(),
                }],
                None => vec![Message::Failed(ErrorReport::new(
                    "could not decode associated session",
                    &Error::io(
                        "decode associated session",
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "service response has no session",
                        ),
                    ),
                ))],
            },
            Err(e) => vec![Message::Failed(ErrorReport::new(
                &format!("could not associate {session} with worktree {worktree_id}"),
                &e,
            ))],
        },

        Task::CloseOrphan {
            session,
            idempotency_key,
        } => {
            match call_service(
                worker,
                "gui-close-orphan",
                "session.close",
                serde_json::json!({
                    "session": session,
                    "idempotency_key": idempotency_key,
                }),
            ) {
                Ok(_) => vec![Message::OrphanClosed { session }],
                Err(e) => vec![Message::Failed(ErrorReport::new(
                    &format!("could not close {session}"),
                    &e,
                ))],
            }
        }

        Task::KillServer { idempotency_key } => match call_service(
            worker,
            "gui-stop-server",
            "server.stop",
            serde_json::json!({"idempotency_key": idempotency_key}),
        ) {
            Ok(_) => vec![Message::ServerKilled],
            Err(e) => vec![Message::Failed(ErrorReport::new(
                "could not kill the tmux server",
                &e,
            ))],
        },

        Task::RefreshSessions => {
            #[derive(serde::Deserialize)]
            struct RefreshSessionsResult {
                presence: HashMap<String, SessionPresence>,
                windows: HashMap<String, Vec<WindowInfo>>,
            }
            match call_service(
                worker,
                "gui-session-refresh",
                "session.refresh",
                serde_json::Value::Null,
            )
            .and_then(|value| {
                serde_json::from_value::<RefreshSessionsResult>(value).map_err(|error| {
                    Error::io(
                        "decode refreshed sessions",
                        std::io::Error::new(std::io::ErrorKind::InvalidData, error),
                    )
                })
            }) {
                Ok(result) => vec![Message::SessionsRefreshed {
                    presence: result.presence,
                    windows: result.windows,
                }],
                Err(e) => vec![Message::Failed(ErrorReport::new(
                    "could not list tmux sessions",
                    &e,
                ))],
            }
        }

        Task::Activate {
            worktree_id,
            idempotency_key,
        } => {
            #[derive(serde::Deserialize)]
            struct OpenResult {
                activation: Activation,
            }
            let result = call_service(
                worker,
                "gui-open-session",
                "session.open",
                serde_json::json!({
                    "worktree_id": worktree_id,
                    "idempotency_key": idempotency_key,
                }),
            )
            .and_then(|value| {
                serde_json::from_value::<OpenResult>(value).map_err(|error| {
                    Error::io(
                        "decode opened session",
                        std::io::Error::new(std::io::ErrorKind::InvalidData, error),
                    )
                })
            });
            match result {
                Ok(activation) => {
                    let mut messages = vec![Message::Activated {
                        worktree_id,
                        activation: activation.activation,
                    }];
                    // Presence changed: the row must stop saying "no session".
                    messages.extend(handle(worker, Task::RefreshSessions));
                    messages
                }
                Err(e) => {
                    let mut messages = vec![Message::Failed(ErrorReport::new(
                        &format!("could not open worktree {worktree_id}"),
                        &e,
                    ))];
                    // The session may have been created before the failure.
                    messages.extend(handle(worker, Task::RefreshSessions));
                    messages
                }
            }
        }

        Task::ActivateWindow {
            worktree_id,
            window_index,
            idempotency_key,
        } => {
            #[derive(serde::Deserialize)]
            struct OpenWindowResult {
                activation: Activation,
            }
            let result = call_service(
                worker,
                "gui-open-session-window",
                "session.window.open",
                serde_json::json!({
                    "worktree_id": worktree_id,
                    "window_index": window_index,
                    "idempotency_key": idempotency_key,
                }),
            )
            .and_then(|value| {
                serde_json::from_value::<OpenWindowResult>(value).map_err(|error| {
                    Error::io(
                        "decode opened session window",
                        std::io::Error::new(std::io::ErrorKind::InvalidData, error),
                    )
                })
            });
            let mut messages = match result {
                Ok(result) => vec![Message::Activated {
                    worktree_id,
                    activation: result.activation,
                }],
                Err(e) => vec![Message::Failed(ErrorReport::new(
                    &format!("could not open window {window_index} of worktree {worktree_id}"),
                    &e,
                ))],
            };
            // The session may have been created, and the active window has
            // moved: both are things the tree shows.
            messages.extend(handle(worker, Task::RefreshSessions));
            messages
        }

        Task::OpenInNewTerminal {
            worktree_id,
            idempotency_key,
        } => {
            #[derive(serde::Deserialize)]
            struct OpenTerminalResult {
                activation: Activation,
            }
            let result = call_service(
                worker,
                "gui-open-additional-terminal",
                "session.terminal.open",
                serde_json::json!({
                    "worktree_id": worktree_id,
                    "idempotency_key": idempotency_key,
                }),
            )
            .and_then(|value| {
                serde_json::from_value::<OpenTerminalResult>(value).map_err(|error| {
                    Error::io(
                        "decode additional terminal",
                        std::io::Error::new(std::io::ErrorKind::InvalidData, error),
                    )
                })
            });
            let mut messages = match result {
                Ok(result) => vec![Message::Activated {
                    worktree_id,
                    activation: result.activation,
                }],
                Err(e) => vec![Message::Failed(ErrorReport::new(
                    &format!("could not open a terminal on worktree {worktree_id}"),
                    &e,
                ))],
            };
            messages.extend(handle(worker, Task::RefreshSessions));
            messages
        }

        Task::OpenNewWindow {
            worktree_id,
            idempotency_key,
        } => {
            #[derive(serde::Deserialize)]
            struct CreateWindowResult {
                window: NewWindow,
            }
            let result = call_service(
                worker,
                "gui-create-session-window",
                "session.window.create",
                serde_json::json!({
                    "worktree_id": worktree_id,
                    "idempotency_key": idempotency_key,
                }),
            )
            .and_then(|value| {
                serde_json::from_value::<CreateWindowResult>(value).map_err(|error| {
                    Error::io(
                        "decode created session window",
                        std::io::Error::new(std::io::ErrorKind::InvalidData, error),
                    )
                })
            });
            let mut messages = match result {
                Ok(result) => vec![Message::WindowOpened {
                    worktree_id,
                    window: result.window,
                }],
                Err(e) => vec![Message::Failed(ErrorReport::new(
                    &format!("could not open a window on worktree {worktree_id}"),
                    &e,
                ))],
            };
            messages.extend(handle(worker, Task::RefreshSessions));
            messages
        }

        Task::LoadBaseRefs { project_id } => {
            #[derive(serde::Deserialize)]
            struct ProjectRefsResult {
                project_id: String,
                refs: Vec<RefEntry>,
                current: Option<String>,
            }
            match call_service(
                worker,
                "gui-project-refs",
                "project.refs",
                serde_json::json!({"project_id": project_id}),
            )
            .and_then(|value| {
                serde_json::from_value::<ProjectRefsResult>(value).map_err(|error| {
                    Error::io(
                        "decode project refs",
                        std::io::Error::new(std::io::ErrorKind::InvalidData, error),
                    )
                })
            }) {
                Ok(result) => vec![Message::BaseRefsLoaded {
                    project_id: result.project_id,
                    refs: result.refs,
                    current: result.current,
                }],
                Err(e) => vec![Message::Failed(ErrorReport::new(
                    "could not list branches",
                    &e,
                ))],
            }
        }

        Task::CreateWorktree {
            project_id,
            add,
            open_after,
            idempotency_key,
        } => {
            #[derive(serde::Deserialize)]
            struct CreateWorktreeResult {
                path: PathBuf,
                worktrees: Vec<Worktree>,
            }
            let result = call_service(
                worker,
                "gui-create-worktree",
                "worktree.create",
                serde_json::json!({
                    "project_id": project_id,
                    "add": add,
                    "idempotency_key": idempotency_key,
                }),
            )
            .and_then(|value| {
                serde_json::from_value::<CreateWorktreeResult>(value).map_err(|error| {
                    Error::io(
                        "decode created worktree",
                        std::io::Error::new(std::io::ErrorKind::InvalidData, error),
                    )
                })
            });
            match result {
                Ok(CreateWorktreeResult { path, worktrees }) => {
                    let mut messages = vec![Message::WorktreeCreated {
                        project_id: project_id.clone(),
                        path: path.clone(),
                    }];
                    worker.enqueue(Task::RefreshStatuses {
                        project_id: project_id.clone(),
                    });
                    if open_after && let Some(worktree) = worktrees.iter().find(|w| w.path == path)
                    {
                        worker.enqueue(Task::Activate {
                            worktree_id: worktree.id.clone(),
                            idempotency_key: format!("create-open-{}", grove_core::agent::nonce()),
                        });
                    }
                    messages.push(Message::WorktreesRefreshed {
                        project_id,
                        worktrees,
                    });
                    messages
                }
                Err(e) => vec![Message::Failed(ErrorReport::new(
                    "could not create the worktree",
                    &e,
                ))],
            }
        }

        Task::GatherRemoval { worktree_id } => {
            #[derive(serde::Deserialize)]
            struct InspectRemovalResult {
                project_id: String,
                worktree_id: String,
                report: RemovalReport,
            }
            match call_service(
                worker,
                "gui-removal-inspect",
                "removal.inspect",
                serde_json::json!({"worktree_id": worktree_id}),
            )
            .and_then(|value| {
                serde_json::from_value::<InspectRemovalResult>(value).map_err(|error| {
                    Error::io(
                        "decode removal inspection",
                        std::io::Error::new(std::io::ErrorKind::InvalidData, error),
                    )
                })
            }) {
                Ok(result) => vec![Message::RemovalGathered {
                    project_id: result.project_id,
                    worktree_id: result.worktree_id,
                    report: Box::new(result.report),
                }],
                Err(e) => vec![Message::Failed(ErrorReport::new(
                    &format!("could not inspect worktree {worktree_id}"),
                    &e,
                ))],
            }
        }

        Task::CloseSession {
            project_id,
            worktree_id,
            idempotency_key,
        } => match call_service(
            worker,
            "gui-close-worktree-session",
            "session.worktree.close",
            serde_json::json!({
                "worktree_id": worktree_id,
                "idempotency_key": idempotency_key,
            }),
        ) {
            Ok(value) => {
                worker.enqueue(Task::RefreshSessions);
                let session = value
                    .get("session")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("the worktree session");
                vec![Message::RemovalDone {
                    project_id,
                    operation: RemovalOp::CloseSession,
                    detail: format!("Closed the tmux session {session}."),
                }]
            }
            Err(e) => vec![Message::RemovalFailed {
                project_id,
                operation: RemovalOp::CloseSession,
                report: ErrorReport::new("could not close the session", &e),
            }],
        },

        Task::RemoveWorktree {
            project_id,
            worktree_id,
            force,
            idempotency_key,
        } => {
            #[derive(serde::Deserialize)]
            struct RemoveWorktreeResult {
                path: PathBuf,
                worktrees: Vec<Worktree>,
            }
            match call_service(
                worker,
                "gui-remove-worktree",
                "worktree.remove",
                serde_json::json!({
                    "worktree_id": worktree_id,
                    "force": force,
                    "idempotency_key": idempotency_key,
                }),
            )
            .and_then(|value| {
                serde_json::from_value::<RemoveWorktreeResult>(value).map_err(|error| {
                    Error::io(
                        "decode removed worktree",
                        std::io::Error::new(std::io::ErrorKind::InvalidData, error),
                    )
                })
            }) {
                Ok(result) => {
                    worker.enqueue(Task::RefreshStatuses {
                        project_id: project_id.clone(),
                    });
                    vec![
                        Message::RemovalDone {
                            project_id: project_id.clone(),
                            operation: RemovalOp::RemoveWorktree,
                            detail: format!(
                                "Removed the worktree {}. The branch was not touched.",
                                result.path.display()
                            ),
                        },
                        Message::WorktreesRefreshed {
                            project_id,
                            worktrees: result.worktrees,
                        },
                    ]
                }
                Err(e) => {
                    // Nothing was removed; the dialog shows git's own refusal and
                    // only then offers --force.
                    vec![Message::RemovalFailed {
                        project_id,
                        operation: RemovalOp::RemoveWorktree,
                        report: ErrorReport::new("could not remove the worktree", &e),
                    }]
                }
            }
        }

        Task::DeleteBranch {
            project_id,
            branch,
            force,
            idempotency_key,
        } => {
            #[derive(serde::Deserialize)]
            struct DeleteBranchResult {
                branch: String,
                worktrees: Vec<Worktree>,
            }
            match call_service(
                worker,
                "gui-delete-branch",
                "branch.delete",
                serde_json::json!({
                    "project_id": project_id,
                    "branch": branch,
                    "force": force,
                    "idempotency_key": idempotency_key,
                }),
            )
            .and_then(|value| {
                serde_json::from_value::<DeleteBranchResult>(value).map_err(|error| {
                    Error::io(
                        "decode deleted branch",
                        std::io::Error::new(std::io::ErrorKind::InvalidData, error),
                    )
                })
            }) {
                Ok(result) => {
                    worker.enqueue(Task::RefreshStatuses {
                        project_id: project_id.clone(),
                    });
                    vec![
                        Message::RemovalDone {
                            project_id: project_id.clone(),
                            operation: RemovalOp::DeleteBranch,
                            detail: format!("Deleted the branch {}.", result.branch),
                        },
                        Message::WorktreesRefreshed {
                            project_id,
                            worktrees: result.worktrees,
                        },
                    ]
                }
                Err(e) => vec![Message::RemovalFailed {
                    project_id,
                    operation: RemovalOp::DeleteBranch,
                    report: ErrorReport::new(&format!("could not delete {branch}"), &e),
                }],
            }
        }

        Task::SetProjectExpanded {
            project_id,
            expanded,
        } => state_intent_messages(
            worker,
            "project.expanded.set",
            serde_json::json!({"project_id": project_id, "expanded": expanded}),
            "could not update project visibility",
            "Updated project visibility.",
            false,
        ),
        Task::RemoveProject { project_id } => state_intent_messages(
            worker,
            "project.remove",
            serde_json::json!({"project_id": project_id}),
            "could not remove the project from Grove",
            "Removed from Grove. The repository is untouched.",
            false,
        ),
        Task::AssignSlot {
            number,
            worktree_id,
        } => state_intent_messages(
            worker,
            "slot.assign",
            serde_json::json!({"number": number, "worktree_id": worktree_id}),
            "could not assign the worktree number",
            &format!("`grove toggle {number}` now opens this worktree."),
            false,
        ),
        Task::ClearSlot { worktree_id } => state_intent_messages(
            worker,
            "slot.clear",
            serde_json::json!({"worktree_id": worktree_id}),
            "could not clear the worktree number",
            "Took the number off this worktree.",
            false,
        ),
        Task::IgnoreSession { session } => state_intent_messages(
            worker,
            "session.ignore",
            serde_json::json!({"session": session}),
            "could not ignore the session",
            "Ignored the session. It is still running.",
            true,
        ),
        Task::ClearIgnoredSessions => state_intent_messages(
            worker,
            "session.ignored.clear",
            serde_json::Value::Null,
            "could not restore ignored sessions",
            "Restored ignored sessions to the reconciliation list.",
            true,
        ),

        Task::SaveConfig(edits) => {
            let path = worker.paths.config_file();
            match config_write::apply(&path, &edits) {
                Ok(()) => {
                    let mut messages = vec![Message::ConfigSaved { path: path.clone() }];
                    // Re-read rather than trusting what was sent: the file is
                    // the truth, and a hand edit may have landed meanwhile.
                    messages.extend(handle(worker, Task::LoadConfig));
                    messages
                }
                Err(e) => vec![Message::Failed(ErrorReport::new(
                    &format!("could not save {}", path.display()),
                    &e,
                ))],
            }
        }

        Task::DetectTerminal => match terminal::detect() {
            Ok(template) => vec![Message::TerminalDetected {
                template: template.to_string(),
            }],
            Err(e) => vec![Message::Failed(ErrorReport::new(
                "could not detect a terminal",
                &e,
            ))],
        },

        Task::ProbeTerminal(command) => {
            let program = terminal::tokenize(&command)
                .ok()
                .and_then(|tokens| tokens.first().cloned())
                .map(|token| terminal::substitute_token(&token, &terminal::TemplateVars::default()))
                .unwrap_or_default();
            vec![Message::TerminalProbed {
                found: !program.is_empty() && grove_core::process::is_on_path(&program),
                program,
                command,
            }]
        }

        Task::OpenWithDesktop(path) => {
            match Invocation::new("xdg-open").arg(&path).spawn_detached() {
                Ok(()) => Vec::new(),
                Err(e) => vec![Message::Failed(ErrorReport::new(
                    &format!("could not open {}", path.display()),
                    &e,
                ))],
            }
        }

        Task::PickDirectory { target, start } => match pick_directory(start.as_deref()) {
            // Cancelled, or no portal answered: the typed field stays.
            None => Vec::new(),
            Some(path) => vec![Message::DirectoryPicked { target, path }],
        },
    }
}

/// Open the desktop's directory picker, blocking this worker thread (never
/// the UI thread) until the user answers. On Wayland `rfd` talks to
/// xdg-desktop-portal, so the dialog is a separate process and Grove keeps
/// painting. `None` means "cancelled, or no picker available".
#[cfg(feature = "native-file-picker")]
fn pick_directory(start: Option<&Path>) -> Option<PathBuf> {
    let mut dialog = rfd::FileDialog::new().set_title("Choose a directory");
    if let Some(start) = picker_start_dir(start) {
        dialog = dialog.set_directory(start);
    }
    dialog.pick_folder()
}

/// Where the picker should open: the typed path if it is a directory, else
/// its parent if that is one — a worktree path the user is about to create
/// does not exist yet, but the directory it goes in usually does.
#[cfg_attr(not(feature = "native-file-picker"), allow(dead_code))]
fn picker_start_dir(start: Option<&Path>) -> Option<&Path> {
    let start = start?;
    if start.is_dir() {
        return Some(start);
    }
    start.parent().filter(|parent| parent.is_dir())
}

/// Without the `native-file-picker` feature there is no picker: paths are
/// typed, which every field already supports.
#[cfg(not(feature = "native-file-picker"))]
fn pick_directory(_start: Option<&Path>) -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use grove_core::error::CommandFailure;
    use grove_core::protocol::Response;
    use std::os::unix::net::UnixListener;

    /// A worker bound to a throwaway config directory. Nothing here starts a
    /// tmux server: the tasks under test only touch files.
    fn worker(dir: &Path) -> WorkerState {
        let paths = Paths {
            config_dir: dir.join("config"),
            state_dir: dir.join("state"),
            runtime_dir: dir.join("run"),
        };
        let server = TmuxServer::new(paths.tmux_socket()).with_config(paths.tmux_config_file());
        WorkerState {
            paths,
            server,
            config: Config::default(),
            tasks: channel().0,
        }
    }

    const USER_FILE: &str = "\
# My own notes, and Grove had better keep them.
[terminal]
# the terminal I actually use
command = \"foot tmux -S {socket} attach-session -t {session}\"

# where I keep my worktrees
[worktrees]
default_parent = \"/home/u/trees\"
";

    /// The footer confirmation sends only an idempotency key. The daemon owns
    /// the private tmux server and tells the GUI when shutdown has completed.
    #[test]
    fn confirmed_server_shutdown_uses_only_the_daemon_result() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut worker = worker(tmp.path());
        std::fs::create_dir_all(&worker.paths.runtime_dir).expect("runtime directory");
        let listener =
            UnixListener::bind(worker.paths.notify_socket()).expect("bind service socket");
        let service = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept server stop");
            let request = protocol::read_request(&mut stream).expect("server stop request");
            assert_eq!(request.method, "server.stop");
            assert_eq!(request.params["idempotency_key"], "stop-test");
            protocol::write_response(
                &mut stream,
                &Response::success(&request.id, serde_json::json!({"stopped": true})),
            )
            .expect("server stop response");
        });

        let messages = handle(
            &mut worker,
            Task::KillServer {
                idempotency_key: "stop-test".into(),
            },
        );
        service.join().expect("service thread");
        assert!(matches!(messages.as_slice(), [Message::ServerKilled]));
    }

    #[test]
    fn project_registration_uses_only_the_daemon_result() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut worker = worker(tmp.path());
        std::fs::create_dir_all(&worker.paths.runtime_dir).expect("runtime directory");
        let listener =
            UnixListener::bind(worker.paths.notify_socket()).expect("bind service socket");
        let project_path = tmp.path().join("repository");
        let response_path = project_path.clone();
        let service = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept project open");
            let request = protocol::read_request(&mut stream).expect("project open request");
            assert_eq!(request.method, "project.open");
            assert_eq!(request.params["path"].as_str(), response_path.to_str());
            assert_eq!(request.params["idempotency_key"], "open-test");
            assert!(request.params.get("repository_path").is_none());
            assert!(request.params.get("git_common_dir").is_none());
            protocol::write_response(
                &mut stream,
                &Response::success(
                    &request.id,
                    serde_json::json!({
                        "changed": true,
                        "project": {
                            "id": "project-1",
                            "name": "Grove",
                            "repository_path": response_path,
                            "git_common_dir": response_path.join(".git"),
                            "default_worktree_path": response_path,
                            "is_expanded": true,
                            "worktrees": [],
                            "unavailable": null,
                        },
                        "state": {},
                    }),
                ),
            )
            .expect("project open response");

            let (mut stream, _) = listener.accept().expect("accept malformed project open");
            let request =
                protocol::read_request(&mut stream).expect("malformed project open request");
            protocol::write_response(
                &mut stream,
                &Response::success(&request.id, serde_json::json!({"changed": false})),
            )
            .expect("malformed project open response");
        });

        let messages = handle(
            &mut worker,
            Task::OpenProject {
                path: project_path,
                idempotency_key: "open-test".into(),
            },
        );
        assert!(matches!(
            messages.as_slice(),
            [Message::ProjectOpened(project)] if project.id == "project-1"
        ));
        let messages = handle(
            &mut worker,
            Task::OpenProject {
                path: tmp.path().join("another-repository"),
                idempotency_key: "open-malformed".into(),
            },
        );
        assert!(matches!(messages.as_slice(), [Message::Failed(_)]));
        service.join().expect("service thread");
    }

    #[test]
    fn index_changes_use_dedicated_daemon_intents() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut worker = worker(tmp.path());
        std::fs::create_dir_all(&worker.paths.runtime_dir).expect("runtime directory");
        let listener =
            UnixListener::bind(worker.paths.notify_socket()).expect("bind service socket");
        let expected = [
            (
                "project.expanded.set",
                serde_json::json!({"project_id": "project-1", "expanded": false}),
            ),
            (
                "project.remove",
                serde_json::json!({"project_id": "project-1"}),
            ),
            (
                "slot.assign",
                serde_json::json!({"number": 2, "worktree_id": "abc123"}),
            ),
            ("slot.clear", serde_json::json!({"worktree_id": "abc123"})),
            ("session.ignore", serde_json::json!({"session": "scratch"})),
            ("session.ignored.clear", serde_json::Value::Null),
        ];
        let service = std::thread::spawn(move || {
            for (method, params) in expected {
                let (mut stream, _) = listener.accept().expect("accept state intent");
                let request = protocol::read_request(&mut stream).expect("state intent request");
                assert_eq!(request.method, method);
                assert_eq!(request.params, params);
                assert_ne!(request.method, "state.mutate");
                protocol::write_response(
                    &mut stream,
                    &Response::success(
                        &request.id,
                        serde_json::json!({"changed": true, "state": {}}),
                    ),
                )
                .expect("state intent response");
            }
        });

        for task in [
            Task::SetProjectExpanded {
                project_id: "project-1".into(),
                expanded: false,
            },
            Task::RemoveProject {
                project_id: "project-1".into(),
            },
            Task::AssignSlot {
                number: 2,
                worktree_id: "abc123".into(),
            },
            Task::ClearSlot {
                worktree_id: "abc123".into(),
            },
            Task::IgnoreSession {
                session: "scratch".into(),
            },
            Task::ClearIgnoredSessions,
        ] {
            assert!(matches!(
                handle(&mut worker, task).as_slice(),
                [Message::StateUpdated { .. }]
            ));
        }
        service.join().expect("service thread");
    }

    #[test]
    fn project_refresh_uses_only_daemon_owned_repository_metadata() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut worker = worker(tmp.path());
        std::fs::create_dir_all(&worker.paths.runtime_dir).expect("runtime directory");
        let listener =
            UnixListener::bind(worker.paths.notify_socket()).expect("bind service socket");
        let service = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept project refresh");
            let request = protocol::read_request(&mut stream).expect("project refresh request");
            assert_eq!(request.method, "project.refresh");
            assert_eq!(request.params["project_id"], "project-1");
            assert_eq!(
                request.params.as_object().map(serde_json::Map::len),
                Some(1)
            );
            protocol::write_response(
                &mut stream,
                &Response::success(
                    &request.id,
                    serde_json::json!({
                        "project_id": "project-1",
                        "worktrees": [],
                        "statuses": {},
                    }),
                ),
            )
            .expect("project refresh response");

            let (mut stream, _) = listener.accept().expect("accept malformed refresh");
            let request = protocol::read_request(&mut stream).expect("malformed refresh request");
            protocol::write_response(
                &mut stream,
                &Response::success(&request.id, serde_json::json!({"project_id": "project-1"})),
            )
            .expect("malformed refresh response");
        });

        let messages = handle(
            &mut worker,
            Task::RefreshProject {
                project_id: "project-1".into(),
            },
        );
        assert!(matches!(
            messages.as_slice(),
            [
                Message::WorktreesRefreshed { project_id, .. },
                Message::StatusesRefreshed {
                    project_id: statuses_id,
                    ..
                }
            ] if project_id == "project-1" && statuses_id == "project-1"
        ));
        let messages = handle(
            &mut worker,
            Task::RefreshProject {
                project_id: "project-1".into(),
            },
        );
        assert!(matches!(messages.as_slice(), [Message::Failed(_)]));
        service.join().expect("service thread");
    }

    #[test]
    fn status_refresh_uses_only_the_daemon_project_identity() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut worker = worker(tmp.path());
        std::fs::create_dir_all(&worker.paths.runtime_dir).expect("runtime directory");
        let listener =
            UnixListener::bind(worker.paths.notify_socket()).expect("bind service socket");
        let service = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept status refresh");
            let request = protocol::read_request(&mut stream).expect("status refresh request");
            assert_eq!(request.method, "project.statuses");
            assert_eq!(
                request.params,
                serde_json::json!({"project_id": "project-1"})
            );
            protocol::write_response(
                &mut stream,
                &Response::success(
                    &request.id,
                    serde_json::json!({
                        "project_id": "project-1",
                        "statuses": {},
                    }),
                ),
            )
            .expect("status refresh response");

            let (mut stream, _) = listener.accept().expect("accept malformed statuses");
            let request = protocol::read_request(&mut stream).expect("malformed statuses request");
            protocol::write_response(
                &mut stream,
                &Response::success(&request.id, serde_json::json!({"project_id": "project-1"})),
            )
            .expect("malformed statuses response");
        });

        let messages = handle(
            &mut worker,
            Task::RefreshStatuses {
                project_id: "project-1".into(),
            },
        );
        assert!(matches!(
            messages.as_slice(),
            [Message::StatusesRefreshed { project_id, .. }] if project_id == "project-1"
        ));
        let messages = handle(
            &mut worker,
            Task::RefreshStatuses {
                project_id: "project-1".into(),
            },
        );
        assert!(matches!(messages.as_slice(), [Message::Failed(_)]));
        service.join().expect("service thread");
    }

    #[test]
    fn session_refresh_uses_only_the_daemon_observation() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut worker = worker(tmp.path());
        std::fs::create_dir_all(&worker.paths.runtime_dir).expect("runtime directory");
        let listener =
            UnixListener::bind(worker.paths.notify_socket()).expect("bind service socket");
        let service = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept session refresh");
            let request = protocol::read_request(&mut stream).expect("session refresh request");
            assert_eq!(request.method, "session.refresh");
            assert!(request.params.is_null());
            protocol::write_response(
                &mut stream,
                &Response::success(
                    &request.id,
                    serde_json::json!({"presence": {}, "windows": {}}),
                ),
            )
            .expect("session refresh response");

            let (mut stream, _) = listener.accept().expect("accept malformed sessions");
            let request = protocol::read_request(&mut stream).expect("malformed sessions request");
            protocol::write_response(
                &mut stream,
                &Response::success(&request.id, serde_json::json!({"presence": {}})),
            )
            .expect("malformed sessions response");
        });

        let messages = handle(&mut worker, Task::RefreshSessions);
        assert!(matches!(
            messages.as_slice(),
            [Message::SessionsRefreshed { presence, windows }]
                if presence.is_empty() && windows.is_empty()
        ));
        let messages = handle(&mut worker, Task::RefreshSessions);
        assert!(matches!(messages.as_slice(), [Message::Failed(_)]));
        service.join().expect("service thread");
    }

    #[test]
    fn branch_refs_use_only_the_daemon_project_identity() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut worker = worker(tmp.path());
        std::fs::create_dir_all(&worker.paths.runtime_dir).expect("runtime directory");
        let listener =
            UnixListener::bind(worker.paths.notify_socket()).expect("bind service socket");
        let service = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept refs");
            let request = protocol::read_request(&mut stream).expect("refs request");
            assert_eq!(request.method, "project.refs");
            assert_eq!(
                request.params,
                serde_json::json!({"project_id": "project-1"})
            );
            protocol::write_response(
                &mut stream,
                &Response::success(
                    &request.id,
                    serde_json::json!({
                        "project_id": "project-1",
                        "refs": [{"name": "main", "is_remote": false}],
                        "current": "main",
                    }),
                ),
            )
            .expect("refs response");
        });

        let messages = handle(
            &mut worker,
            Task::LoadBaseRefs {
                project_id: "project-1".into(),
            },
        );
        service.join().expect("service thread");
        assert!(matches!(
            messages.as_slice(),
            [Message::BaseRefsLoaded {
                project_id,
                refs,
                current: Some(current),
            }] if project_id == "project-1" && refs[0].name == "main" && current == "main"
        ));
    }

    #[test]
    fn removal_inspection_uses_only_the_daemon_worktree_identity() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut worker = worker(tmp.path());
        std::fs::create_dir_all(&worker.paths.runtime_dir).expect("runtime directory");
        let listener =
            UnixListener::bind(worker.paths.notify_socket()).expect("bind service socket");
        let service = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept inspection");
            let request = protocol::read_request(&mut stream).expect("inspection request");
            assert_eq!(request.method, "removal.inspect");
            assert_eq!(request.params, serde_json::json!({"worktree_id": "abc123"}));
            protocol::write_response(
                &mut stream,
                &Response::success(
                    &request.id,
                    serde_json::json!({
                        "project_id": "project-1",
                        "worktree_id": "abc123",
                        "report": {
                            "worktree_path": "/repo",
                            "branch": "main",
                            "findings": [],
                            "can_remove_worktree": false,
                            "can_delete_branch": true,
                            "can_close_session": false,
                            "loses_work": false,
                        },
                    }),
                ),
            )
            .expect("inspection response");
        });

        let messages = handle(
            &mut worker,
            Task::GatherRemoval {
                worktree_id: "abc123".into(),
            },
        );
        service.join().expect("service thread");
        assert!(matches!(
            messages.as_slice(),
            [Message::RemovalGathered {
                project_id,
                worktree_id,
                ..
            }] if project_id == "project-1" && worktree_id == "abc123"
        ));
    }

    #[test]
    fn worktree_creation_sends_only_project_identity_and_intent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut worker = worker(tmp.path());
        std::fs::create_dir_all(&worker.paths.runtime_dir).expect("runtime directory");
        let listener =
            UnixListener::bind(worker.paths.notify_socket()).expect("bind service socket");
        let created = tmp.path().join("created");
        let response_path = created.clone();
        let service = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept create");
            let request = protocol::read_request(&mut stream).expect("create request");
            assert_eq!(request.method, "worktree.create");
            assert_eq!(request.params["project_id"], "project-1");
            assert_eq!(request.params["add"]["new_branch"], "feature");
            assert_eq!(request.params["idempotency_key"], "create-test");
            assert!(request.params.get("repository_path").is_none());
            protocol::write_response(
                &mut stream,
                &Response::success(
                    &request.id,
                    serde_json::json!({
                        "project_id": "project-1",
                        "path": response_path,
                        "worktrees": [],
                    }),
                ),
            )
            .expect("create response");
        });

        let messages = handle(
            &mut worker,
            Task::CreateWorktree {
                project_id: "project-1".into(),
                add: Box::new(WorktreeAdd {
                    path: created.clone(),
                    new_branch: Some("feature".into()),
                    base_ref: None,
                }),
                open_after: false,
                idempotency_key: "create-test".into(),
            },
        );
        service.join().expect("service thread");
        assert!(matches!(
            messages.first(),
            Some(Message::WorktreeCreated { project_id, path })
                if project_id == "project-1" && path == &created
        ));
    }

    #[test]
    fn worktree_removal_sends_only_live_identity_and_confirmation_level() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut worker = worker(tmp.path());
        std::fs::create_dir_all(&worker.paths.runtime_dir).expect("runtime directory");
        let listener =
            UnixListener::bind(worker.paths.notify_socket()).expect("bind service socket");
        let removed = tmp.path().join("removed");
        let response_path = removed.clone();
        let service = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept remove");
            let request = protocol::read_request(&mut stream).expect("remove request");
            assert_eq!(request.method, "worktree.remove");
            assert_eq!(request.params["worktree_id"], "abc123");
            assert_eq!(request.params["force"], true);
            assert_eq!(request.params["idempotency_key"], "remove-test");
            assert!(request.params.get("repository_path").is_none());
            assert!(request.params.get("worktree_path").is_none());
            protocol::write_response(
                &mut stream,
                &Response::success(
                    &request.id,
                    serde_json::json!({
                        "project_id": "project-1",
                        "worktree_id": "abc123",
                        "path": response_path,
                        "worktrees": [],
                    }),
                ),
            )
            .expect("remove response");
        });

        let messages = handle(
            &mut worker,
            Task::RemoveWorktree {
                project_id: "project-1".into(),
                worktree_id: "abc123".into(),
                force: true,
                idempotency_key: "remove-test".into(),
            },
        );
        service.join().expect("service thread");
        assert!(matches!(
            messages.first(),
            Some(Message::RemovalDone {
                operation: RemovalOp::RemoveWorktree,
                detail,
                ..
            }) if detail.contains(&removed.display().to_string())
        ));
    }

    #[test]
    fn branch_deletion_sends_only_project_identity_and_confirmation_level() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut worker = worker(tmp.path());
        std::fs::create_dir_all(&worker.paths.runtime_dir).expect("runtime directory");
        let listener =
            UnixListener::bind(worker.paths.notify_socket()).expect("bind service socket");
        let service = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept branch delete");
            let request = protocol::read_request(&mut stream).expect("branch delete request");
            assert_eq!(request.method, "branch.delete");
            assert_eq!(request.params["project_id"], "project-1");
            assert_eq!(request.params["branch"], "feature");
            assert_eq!(request.params["force"], true);
            assert_eq!(request.params["idempotency_key"], "delete-test");
            assert!(request.params.get("repository_path").is_none());
            protocol::write_response(
                &mut stream,
                &Response::success(
                    &request.id,
                    serde_json::json!({
                        "project_id": "project-1",
                        "branch": "feature",
                        "worktrees": [],
                    }),
                ),
            )
            .expect("delete response");

            let (mut malformed_stream, _) = listener.accept().expect("accept malformed delete");
            let malformed_request =
                protocol::read_request(&mut malformed_stream).expect("malformed delete request");
            protocol::write_response(
                &mut malformed_stream,
                &Response::success(
                    &malformed_request.id,
                    serde_json::json!({"branch": "feature"}),
                ),
            )
            .expect("malformed delete response");
        });

        let messages = handle(
            &mut worker,
            Task::DeleteBranch {
                project_id: "project-1".into(),
                branch: "feature".into(),
                force: true,
                idempotency_key: "delete-test".into(),
            },
        );
        assert!(matches!(
            messages.first(),
            Some(Message::RemovalDone {
                operation: RemovalOp::DeleteBranch,
                detail,
                ..
            }) if detail.contains("feature")
        ));

        let malformed = handle(
            &mut worker,
            Task::DeleteBranch {
                project_id: "project-1".into(),
                branch: "feature".into(),
                force: false,
                idempotency_key: "delete-malformed".into(),
            },
        );
        service.join().expect("service thread");
        assert!(matches!(
            malformed.as_slice(),
            [Message::RemovalFailed {
                operation: RemovalOp::DeleteBranch,
                ..
            }]
        ));
    }

    #[test]
    fn recorded_agent_resumption_uses_only_the_daemon_result() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut worker = worker(tmp.path());
        std::fs::create_dir_all(&worker.paths.runtime_dir).expect("runtime directory");
        let listener =
            UnixListener::bind(worker.paths.notify_socket()).expect("bind service socket");
        let service = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept resume request");
            let request = protocol::read_request(&mut stream).expect("resume request");
            assert_eq!(request.method, "agent.resume_recorded");
            assert_eq!(request.params["idempotency_key"], "launch-test");
            let response = Response::success(
                &request.id,
                serde_json::json!({
                    "worktree_ids": ["abc123"],
                    "failures": [{
                        "worktree_path": "/work/failed",
                        "message": "agent executable disappeared",
                    }],
                }),
            );
            protocol::write_response(&mut stream, &response).expect("resume response");
        });

        let messages = handle(
            &mut worker,
            Task::ResumeAgents {
                idempotency_key: "launch-test".into(),
            },
        );
        service.join().expect("service thread");
        assert!(matches!(messages.first(), Some(Message::Failed(_))));
        assert!(matches!(
            messages.last(),
            Some(Message::AgentsResumed { worktree_ids })
                if worktree_ids == &["abc123".to_string()]
        ));
    }

    #[test]
    fn explicit_agent_start_sends_the_complete_intent_to_the_daemon() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut worker = worker(tmp.path());
        std::fs::create_dir_all(&worker.paths.runtime_dir).expect("runtime directory");
        let listener =
            UnixListener::bind(worker.paths.notify_socket()).expect("bind service socket");
        let service = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept agent start");
            let request = protocol::read_request(&mut stream).expect("agent start request");
            assert_eq!(request.method, "agent.start");
            assert_eq!(request.params["worktree_id"], "abc123");
            assert_eq!(request.params["resume"], "conversation-7");
            assert_eq!(request.params["idempotency_key"], "start-test");
            let response = Response::success(
                &request.id,
                serde_json::json!({
                    "worktree_id": "abc123",
                    "session": "wt-abc123",
                    "unit": "grove-agent-abc123.scope",
                }),
            );
            protocol::write_response(&mut stream, &response).expect("agent start response");
        });

        let messages = handle(
            &mut worker,
            Task::StartAgent {
                worktree_id: "abc123".into(),
                resume: Some("conversation-7".into()),
                idempotency_key: "start-test".into(),
            },
        );
        service.join().expect("service thread");
        assert!(matches!(
            messages.as_slice(),
            [Message::AgentStarted { worktree_id, unit }]
                if worktree_id == "abc123"
                    && unit.as_deref() == Some("grove-agent-abc123.scope")
        ));
    }

    #[test]
    fn worktree_activation_uses_only_the_daemon_result() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut worker = worker(tmp.path());
        std::fs::create_dir_all(&worker.paths.runtime_dir).expect("runtime directory");
        let listener =
            UnixListener::bind(worker.paths.notify_socket()).expect("bind service socket");
        let service = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept session open");
            let request = protocol::read_request(&mut stream).expect("session open request");
            assert_eq!(request.method, "session.open");
            assert_eq!(request.params["worktree_id"], "abc123");
            assert_eq!(request.params["idempotency_key"], "open-test");
            let response = Response::success(
                &request.id,
                serde_json::json!({
                    "worktree_id": "abc123",
                    "session": "wt-abc123",
                    "activation": {
                        "kind": "switched_client",
                        "session": "wt-abc123",
                        "client_tty": "/dev/pts/7",
                    },
                }),
            );
            protocol::write_response(&mut stream, &response).expect("session open response");
        });

        let messages = handle(
            &mut worker,
            Task::Activate {
                worktree_id: "abc123".into(),
                idempotency_key: "open-test".into(),
            },
        );
        service.join().expect("service thread");
        assert!(matches!(
            messages.first(),
            Some(Message::Activated {
                worktree_id,
                activation: Activation::SwitchedClient {
                    session,
                    client_tty,
                },
            }) if worktree_id == "abc123"
                && session == "wt-abc123"
                && client_tty == "/dev/pts/7"
        ));
    }

    #[test]
    fn window_activation_uses_only_the_daemon_result() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut worker = worker(tmp.path());
        std::fs::create_dir_all(&worker.paths.runtime_dir).expect("runtime directory");
        let listener =
            UnixListener::bind(worker.paths.notify_socket()).expect("bind service socket");
        let service = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept window open");
            let request = protocol::read_request(&mut stream).expect("window open request");
            assert_eq!(request.method, "session.window.open");
            assert_eq!(request.params["worktree_id"], "abc123");
            assert_eq!(request.params["window_index"], 7);
            assert_eq!(request.params["idempotency_key"], "window-test");
            let response = Response::success(
                &request.id,
                serde_json::json!({
                    "worktree_id": "abc123",
                    "session": "wt-abc123",
                    "window_index": 7,
                    "activation": {
                        "kind": "launched_terminal",
                        "session": "wt-abc123",
                        "command": "foot tmux attach",
                    },
                }),
            );
            protocol::write_response(&mut stream, &response).expect("window open response");
        });

        let messages = handle(
            &mut worker,
            Task::ActivateWindow {
                worktree_id: "abc123".into(),
                window_index: 7,
                idempotency_key: "window-test".into(),
            },
        );
        service.join().expect("service thread");
        assert!(matches!(
            messages.first(),
            Some(Message::Activated {
                worktree_id,
                activation: Activation::LaunchedTerminal { session, command },
            }) if worktree_id == "abc123"
                && session == "wt-abc123"
                && command == "foot tmux attach"
        ));
    }

    #[test]
    fn terminal_and_window_creation_use_only_daemon_results() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut worker = worker(tmp.path());
        std::fs::create_dir_all(&worker.paths.runtime_dir).expect("runtime directory");
        let listener =
            UnixListener::bind(worker.paths.notify_socket()).expect("bind service socket");
        let service = std::thread::spawn(move || {
            let (mut terminal_stream, _) = listener.accept().expect("accept terminal open");
            let terminal_request =
                protocol::read_request(&mut terminal_stream).expect("terminal request");
            assert_eq!(terminal_request.method, "session.terminal.open");
            assert_eq!(terminal_request.params["worktree_id"], "abc123");
            assert_eq!(terminal_request.params["idempotency_key"], "terminal-test");
            protocol::write_response(
                &mut terminal_stream,
                &Response::success(
                    &terminal_request.id,
                    serde_json::json!({
                        "worktree_id": "abc123",
                        "session": "wt-abc123",
                        "activation": {
                            "kind": "launched_terminal",
                            "session": "wt-abc123",
                            "command": "foot tmux attach",
                        },
                    }),
                ),
            )
            .expect("terminal response");

            let (mut refresh_stream, _) = listener.accept().expect("accept session refresh");
            let refresh_request =
                protocol::read_request(&mut refresh_stream).expect("refresh request");
            assert_eq!(refresh_request.method, "session.refresh");
            protocol::write_response(
                &mut refresh_stream,
                &Response::success(
                    &refresh_request.id,
                    serde_json::json!({"presence": {}, "windows": {}}),
                ),
            )
            .expect("refresh response");

            let (mut window_stream, _) = listener.accept().expect("accept window create");
            let window_request =
                protocol::read_request(&mut window_stream).expect("window request");
            assert_eq!(window_request.method, "session.window.create");
            assert_eq!(window_request.params["worktree_id"], "abc123");
            assert_eq!(window_request.params["idempotency_key"], "create-test");
            protocol::write_response(
                &mut window_stream,
                &Response::success(
                    &window_request.id,
                    serde_json::json!({
                        "worktree_id": "abc123",
                        "session": "wt-abc123",
                        "window": {
                            "session": "wt-abc123",
                            "window": "3",
                        },
                    }),
                ),
            )
            .expect("window response");

            let (mut refresh_stream, _) = listener.accept().expect("accept session refresh");
            let refresh_request =
                protocol::read_request(&mut refresh_stream).expect("refresh request");
            assert_eq!(refresh_request.method, "session.refresh");
            protocol::write_response(
                &mut refresh_stream,
                &Response::success(
                    &refresh_request.id,
                    serde_json::json!({"presence": {}, "windows": {}}),
                ),
            )
            .expect("refresh response");
        });

        let terminal_messages = handle(
            &mut worker,
            Task::OpenInNewTerminal {
                worktree_id: "abc123".into(),
                idempotency_key: "terminal-test".into(),
            },
        );
        assert!(matches!(
            terminal_messages.first(),
            Some(Message::Activated {
                activation: Activation::LaunchedTerminal { .. },
                ..
            })
        ));

        let window_messages = handle(
            &mut worker,
            Task::OpenNewWindow {
                worktree_id: "abc123".into(),
                idempotency_key: "create-test".into(),
            },
        );
        service.join().expect("service thread");
        assert!(matches!(
            window_messages.first(),
            Some(Message::WindowOpened { worktree_id, window })
                if worktree_id == "abc123"
                    && window.session == "wt-abc123"
                    && window.window == "3"
        ));
    }

    #[test]
    fn clearing_attention_uses_worktree_identity_through_the_daemon() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut worker = worker(tmp.path());
        std::fs::create_dir_all(&worker.paths.runtime_dir).expect("runtime directory");
        let listener =
            UnixListener::bind(worker.paths.notify_socket()).expect("bind service socket");
        let service = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept attention clear");
            let request = protocol::read_request(&mut stream).expect("attention clear request");
            assert_eq!(request.method, "session.attention.clear");
            assert_eq!(request.params["worktree_id"], "abc123");
            assert_eq!(request.params["idempotency_key"], "attention-test");
            protocol::write_response(
                &mut stream,
                &Response::success(
                    &request.id,
                    serde_json::json!({
                        "worktree_id": "abc123",
                        "session": "wt-abc123",
                        "cleared": true,
                    }),
                ),
            )
            .expect("attention clear response");

            let (mut failed_stream, _) = listener.accept().expect("accept failed clear");
            let failed_request =
                protocol::read_request(&mut failed_stream).expect("failed clear request");
            protocol::write_response(
                &mut failed_stream,
                &Response::error(
                    &failed_request.id,
                    "control_failed",
                    "tmux rejected the mutation",
                ),
            )
            .expect("failed clear response");
        });

        let messages = handle(
            &mut worker,
            Task::ClearAttention {
                worktree_id: "abc123".into(),
                idempotency_key: "attention-test".into(),
            },
        );
        assert!(messages.is_empty());

        let failed = handle(
            &mut worker,
            Task::ClearAttention {
                worktree_id: "abc123".into(),
                idempotency_key: "attention-failure".into(),
            },
        );
        service.join().expect("service thread");
        assert!(matches!(failed.as_slice(), [Message::Failed(_)]));
    }

    #[test]
    fn orphan_association_uses_worktree_identity_through_the_daemon() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut worker = worker(tmp.path());
        std::fs::create_dir_all(&worker.paths.runtime_dir).expect("runtime directory");
        let listener =
            UnixListener::bind(worker.paths.notify_socket()).expect("bind service socket");
        let service = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept association");
            let request = protocol::read_request(&mut stream).expect("association request");
            assert_eq!(request.method, "session.associate");
            assert_eq!(request.params["worktree_id"], "abc123");
            assert_eq!(request.params["orphan_session"], "scratch");
            assert_eq!(request.params["idempotency_key"], "associate-test");
            protocol::write_response(
                &mut stream,
                &Response::success(
                    &request.id,
                    serde_json::json!({
                        "worktree_id": "abc123",
                        "session": "wt-abc123",
                    }),
                ),
            )
            .expect("association response");
        });

        let messages = handle(
            &mut worker,
            Task::AssociateSession {
                worktree_id: "abc123".into(),
                session: "scratch".into(),
                idempotency_key: "associate-test".into(),
            },
        );
        service.join().expect("service thread");
        assert!(matches!(
            messages.as_slice(),
            [Message::Associated {
                worktree_id,
                session,
            }] if worktree_id == "abc123" && session == "wt-abc123"
        ));
    }

    #[test]
    fn confirmed_orphan_closure_uses_the_daemon_and_surfaces_rejection() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut worker = worker(tmp.path());
        std::fs::create_dir_all(&worker.paths.runtime_dir).expect("runtime directory");
        let listener =
            UnixListener::bind(worker.paths.notify_socket()).expect("bind service socket");
        let service = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept close");
            let request = protocol::read_request(&mut stream).expect("close request");
            assert_eq!(request.method, "session.close");
            assert_eq!(request.params["session"], "scratch");
            assert_eq!(request.params["idempotency_key"], "close-test");
            protocol::write_response(
                &mut stream,
                &Response::success(&request.id, serde_json::json!({"session": "scratch"})),
            )
            .expect("close response");

            let (mut failed_stream, _) = listener.accept().expect("accept failed close");
            let failed_request =
                protocol::read_request(&mut failed_stream).expect("failed close request");
            protocol::write_response(
                &mut failed_stream,
                &Response::error(&failed_request.id, "control_failed", "session is busy"),
            )
            .expect("failed close response");
        });

        let closed = handle(
            &mut worker,
            Task::CloseOrphan {
                session: "scratch".into(),
                idempotency_key: "close-test".into(),
            },
        );
        assert!(matches!(
            closed.as_slice(),
            [Message::OrphanClosed { session }] if session == "scratch"
        ));

        let failed = handle(
            &mut worker,
            Task::CloseOrphan {
                session: "busy".into(),
                idempotency_key: "close-failure".into(),
            },
        );
        service.join().expect("service thread");
        assert!(matches!(failed.as_slice(), [Message::Failed(_)]));
    }

    #[test]
    fn confirmed_worktree_session_closure_uses_daemon_identity() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut worker = worker(tmp.path());
        std::fs::create_dir_all(&worker.paths.runtime_dir).expect("runtime directory");
        let listener =
            UnixListener::bind(worker.paths.notify_socket()).expect("bind service socket");
        let service = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept worktree close");
            let request = protocol::read_request(&mut stream).expect("worktree close request");
            assert_eq!(request.method, "session.worktree.close");
            assert_eq!(request.params["worktree_id"], "abc123");
            assert_eq!(request.params["idempotency_key"], "worktree-close-test");
            assert!(request.params.get("session").is_none());
            protocol::write_response(
                &mut stream,
                &Response::success(
                    &request.id,
                    serde_json::json!({
                        "worktree_id": "abc123",
                        "session": "wt-abc123",
                    }),
                ),
            )
            .expect("worktree close response");
        });

        let messages = handle(
            &mut worker,
            Task::CloseSession {
                project_id: "project-1".into(),
                worktree_id: "abc123".into(),
                idempotency_key: "worktree-close-test".into(),
            },
        );
        service.join().expect("service thread");
        assert!(matches!(
            messages.as_slice(),
            [Message::RemovalDone {
                project_id,
                operation: RemovalOp::CloseSession,
                detail,
            }] if project_id == "project-1" && detail.contains("wt-abc123")
        ));
    }

    #[test]
    fn opening_an_orphan_uses_daemon_session_metadata() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut worker = worker(tmp.path());
        std::fs::create_dir_all(&worker.paths.runtime_dir).expect("runtime directory");
        let listener =
            UnixListener::bind(worker.paths.notify_socket()).expect("bind service socket");
        let service = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept orphan open");
            let request = protocol::read_request(&mut stream).expect("orphan open request");
            assert_eq!(request.method, "session.orphan.open");
            assert_eq!(request.params["session"], "scratch");
            assert_eq!(request.params["idempotency_key"], "orphan-open-test");
            assert!(request.params.get("cwd").is_none());
            protocol::write_response(
                &mut stream,
                &Response::success(
                    &request.id,
                    serde_json::json!({
                        "session": "scratch",
                        "activation": {
                            "kind": "switched_client",
                            "session": "scratch",
                            "client_tty": "/dev/pts/9",
                        },
                    }),
                ),
            )
            .expect("orphan open response");
        });

        let messages = handle(
            &mut worker,
            Task::OpenSession {
                session: "scratch".into(),
                idempotency_key: "orphan-open-test".into(),
            },
        );
        service.join().expect("service thread");
        assert!(matches!(
            messages.as_slice(),
            [Message::SessionOpened {
                activation: Activation::SwitchedClient {
                    session,
                    client_tty,
                },
            }] if session == "scratch" && client_tty == "/dev/pts/9"
        ));
    }

    /// The whole save path the Settings pane uses: surgical edit, atomic
    /// write, re-read, and the comments still there afterwards.
    #[test]
    fn saving_config_keeps_the_users_comments_and_reloads_the_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut worker = worker(tmp.path());
        std::fs::create_dir_all(&worker.paths.config_dir).expect("mkdir");
        let file = worker.paths.config_file();
        std::fs::write(&file, USER_FILE).expect("write");

        let messages = handle(
            &mut worker,
            Task::SaveConfig(vec![Edit::string(
                config_write::TERMINAL_COMMAND,
                "kitty tmux -S {socket} attach -t {session}",
            )]),
        );

        let text = std::fs::read_to_string(&file).expect("read");
        assert_eq!(
            text,
            USER_FILE.replace(
                "\"foot tmux -S {socket} attach-session -t {session}\"",
                "\"kitty tmux -S {socket} attach -t {session}\""
            ),
            "only the edited value may change"
        );
        assert!(matches!(
            messages.first(),
            Some(Message::ConfigSaved { .. })
        ));
        let reloaded = messages
            .iter()
            .find_map(|m| match m {
                Message::ConfigLoaded { loaded } => Some(&loaded.config),
                _ => None,
            })
            .expect("the file is re-read after the write");
        assert_eq!(
            reloaded.terminal.command,
            "kitty tmux -S {socket} attach -t {session}"
        );
        assert_eq!(worker.config, *reloaded, "the worker adopts what it read");
    }

    #[test]
    fn a_config_that_cannot_be_edited_fails_without_touching_the_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut worker = worker(tmp.path());
        std::fs::create_dir_all(&worker.paths.config_dir).expect("mkdir");
        let file = worker.paths.config_file();
        std::fs::write(&file, "[terminal\n").expect("write");

        let messages = handle(
            &mut worker,
            Task::SaveConfig(vec![Edit::string(config_write::TERMINAL_COMMAND, "foot")]),
        );
        assert!(matches!(messages.as_slice(), [Message::Failed(_)]));
        assert_eq!(std::fs::read_to_string(&file).expect("read"), "[terminal\n");
    }

    #[test]
    fn probing_a_template_reports_its_program() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut worker = worker(tmp.path());

        let messages = handle(
            &mut worker,
            Task::ProbeTerminal("sh -c {session}".to_string()),
        );
        match messages.as_slice() {
            [
                Message::TerminalProbed {
                    command,
                    program,
                    found,
                },
            ] => {
                assert_eq!(command, "sh -c {session}");
                assert_eq!(program, "sh");
                assert!(*found, "sh is on PATH everywhere Grove runs");
            }
            other => panic!("unexpected {other:?}"),
        }

        let messages = handle(
            &mut worker,
            Task::ProbeTerminal("grove-definitely-not-real -e tmux".to_string()),
        );
        match messages.as_slice() {
            [Message::TerminalProbed { found, program, .. }] => {
                assert_eq!(program, "grove-definitely-not-real");
                assert!(!found);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn probing_a_broken_template_finds_nothing_rather_than_guessing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut worker = worker(tmp.path());
        for command in ["   ", "foot 'unclosed"] {
            match handle(&mut worker, Task::ProbeTerminal(command.to_string())).as_slice() {
                [Message::TerminalProbed { found, program, .. }] => {
                    assert!(!found);
                    assert!(program.is_empty());
                }
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    #[test]
    fn the_picker_opens_at_the_nearest_existing_directory() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("worktrees");
        std::fs::create_dir(&dir).expect("mkdir");
        let missing = dir.join("not-created-yet");

        assert_eq!(picker_start_dir(Some(&dir)), Some(dir.as_path()));
        assert_eq!(
            picker_start_dir(Some(&missing)),
            Some(dir.as_path()),
            "a worktree path that does not exist yet opens in its parent"
        );
        assert_eq!(picker_start_dir(Some(Path::new("/nope/nope/nope"))), None);
        assert_eq!(picker_start_dir(None), None);
    }

    /// Built without the feature there is no portal to ask, so the task is a
    /// no-op and the typed field simply stays as the user left it. (With the
    /// feature the picker needs a desktop portal, which a test run has not
    /// got — that path is exercised by hand, see the smoke checklist.)
    #[cfg(not(feature = "native-file-picker"))]
    #[test]
    fn without_the_feature_a_pick_request_changes_nothing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut worker = worker(tmp.path());
        assert_eq!(pick_directory(Some(tmp.path())), None);
        let messages = handle(
            &mut worker,
            Task::PickDirectory {
                target: PickTarget::ProjectPath,
                start: Some(tmp.path().to_path_buf()),
            },
        );
        assert!(messages.is_empty(), "unexpected {messages:?}");
    }

    #[test]
    fn an_error_report_keeps_the_context_and_the_original_stderr() {
        let failure = CommandFailure {
            program: "git".into(),
            args: vec!["-C".into(), "/home/u/proj".into(), "worktree".into()],
            status: Some(128),
            stdout: String::new(),
            stderr: "fatal: not a git repository\n".into(),
        };
        let report = ErrorReport::new("could not open /home/u/proj", &Error::from(failure));
        assert_eq!(
            report.summary,
            "could not open /home/u/proj: fatal: not a git repository"
        );
        let detail = report.detail.expect("diagnostics are retained");
        assert!(detail.contains("$ git -C /home/u/proj worktree"));
        assert!(detail.contains("exit status: 128"));
        assert!(detail.contains("fatal: not a git repository"));
    }

    #[test]
    fn errors_without_command_output_have_no_expandable_detail() {
        let report = ErrorReport::new("could not save state.toml", &Error::EmptyTerminalTemplate);
        assert!(report.summary.starts_with("could not save state.toml: "));
        assert!(report.detail.is_none());
    }
}
