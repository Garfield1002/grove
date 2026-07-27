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

use grove_core::config::{Config, LoadedConfig};
use grove_core::config_write::{self, Edit};
use grove_core::git::{RefEntry, StatusSummary, WorktreeAdd};
use grove_core::model::{Project, SessionPresence, Worktree};
use grove_core::process::Invocation;
use grove_core::removal::RemovalReport;
use grove_core::state::State;
use grove_core::status::SessionStatus;
use grove_core::workflow::{self, Activation};
use grove_core::{Error, Paths, TmuxServer, config, git, state, terminal, tmux};

/// Work requested by the UI.
#[derive(Debug)]
pub enum Task {
    /// Load `config.toml`, auto-detecting a terminal on first run.
    LoadConfig,
    /// Register the project containing this path.
    OpenProject(PathBuf),
    /// Re-read a project's worktrees and sessions.
    RefreshProject {
        project_id: String,
        repository_path: PathBuf,
        git_common_dir: PathBuf,
    },
    /// Re-read the working-tree status of a project's worktrees. Queued
    /// after a refresh and after every git operation Grove performs.
    RefreshStatuses {
        project_id: String,
        worktrees: Vec<Worktree>,
    },
    /// Re-read session presence only.
    RefreshSessions,
    /// Unset `@grove_attention` on a session the user has just opened.
    ///
    /// The in-memory latch is cleared on the UI thread; this clears the
    /// durable half, which is what would otherwise re-raise attention on the
    /// next poll or after a restart.
    ClearAttention { session: String },
    /// Open a worktree: ensure the session, then switch or launch.
    Activate {
        project_name: String,
        git_common_dir: PathBuf,
        worktree: Box<Worktree>,
    },
    /// Attach an additional terminal without retargeting the primary client.
    OpenInNewTerminal {
        project_name: String,
        git_common_dir: PathBuf,
        worktree: Box<Worktree>,
    },
    /// Start the configured agent in a worktree's `agent` window.
    StartAgent {
        project_name: String,
        git_common_dir: PathBuf,
        worktree: Box<Worktree>,
    },
    /// Local and remote-tracking branches for the create-worktree dialog.
    LoadBaseRefs {
        project_id: String,
        repository_path: PathBuf,
    },
    /// `git worktree add`, then refresh, then optionally open the session.
    CreateWorktree {
        project_id: String,
        project_name: String,
        repository_path: PathBuf,
        git_common_dir: PathBuf,
        add: Box<WorktreeAdd>,
        open_after: bool,
    },
    /// Gather the safe-removal risk report. Reads only; removes nothing.
    GatherRemoval {
        project_id: String,
        worktree: Box<Worktree>,
    },
    /// Close one tmux session on the private server.
    CloseSession { project_id: String, session: String },
    /// `git worktree remove`. `force` only ever arrives from a second,
    /// explicit confirmation after git refused.
    RemoveWorktree {
        project_id: String,
        repository_path: PathBuf,
        git_common_dir: PathBuf,
        worktree_path: PathBuf,
        force: bool,
    },
    /// `git branch -d`, or `-D` after a second explicit confirmation.
    DeleteBranch {
        project_id: String,
        repository_path: PathBuf,
        git_common_dir: PathBuf,
        branch: String,
        force: bool,
    },
    /// Persist the project index.
    SaveState(Box<State>),
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
    SessionsRefreshed(HashMap<String, SessionPresence>),
    /// An agent was started in a session's `agent` window.
    AgentStarted {
        worktree_id: String,
        /// The systemd scope it runs in, when resource accounting is on.
        unit: Option<String>,
    },
    /// One poll of the status engine, keyed by worktree id. Sent by the
    /// poller thread, not by the worker.
    StatusPolled(HashMap<String, SessionStatus>),
    /// An explicit `grove notify` report arrived over the socket.
    Notified {
        worktree_id: String,
        state: SessionStatus,
        message: Option<String>,
    },
    Activated {
        worktree_id: String,
        activation: Activation,
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

    /// Queue "re-read this project's worktrees, then their statuses". Run
    /// after every git operation Grove performs, so the rows never show a
    /// status that predates the operation.
    fn queue_refresh(&self, project_id: &str, repository_path: &Path, git_common_dir: &Path) {
        self.enqueue(Task::RefreshProject {
            project_id: project_id.to_string(),
            repository_path: repository_path.to_path_buf(),
            git_common_dir: git_common_dir.to_path_buf(),
        });
    }
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

        Task::OpenProject(path) => {
            match workflow::open_project(&worker.server, &worker.config, &path) {
                Ok(project) => vec![Message::ProjectOpened(Box::new(project))],
                Err(e) => vec![Message::Failed(ErrorReport::new(
                    &format!("could not open {}", path.display()),
                    &e,
                ))],
            }
        }

        Task::RefreshProject {
            project_id,
            repository_path,
            git_common_dir,
        } => match workflow::refresh_project(
            &worker.server,
            &repository_path,
            &project_id,
            &git_common_dir,
        ) {
            Ok(worktrees) => {
                // Statuses are a second pass so the list appears immediately
                // and the per-worktree `git status` calls never delay it.
                worker.enqueue(Task::RefreshStatuses {
                    project_id: project_id.clone(),
                    worktrees: worktrees.clone(),
                });
                vec![Message::WorktreesRefreshed {
                    project_id,
                    worktrees,
                }]
            }
            Err(e) => vec![Message::Failed(ErrorReport::new(
                &format!("could not refresh {}", repository_path.display()),
                &e,
            ))],
        },

        Task::RefreshStatuses {
            project_id,
            worktrees,
        } => vec![Message::StatusesRefreshed {
            project_id,
            statuses: workflow::worktree_statuses(&worktrees),
        }],

        Task::StartAgent {
            project_name,
            git_common_dir,
            worktree,
        } => {
            let worktree_id = worktree.id.clone();
            match workflow::start_agent(
                &worker.server,
                &worker.config,
                &worker.paths.runtime_dir,
                &project_name,
                &git_common_dir,
                &worktree,
            ) {
                Ok(launch) => {
                    // The new window is activity tmux reports at once; a poll
                    // now is what makes the row react immediately.
                    worker.enqueue(Task::RefreshSessions);
                    vec![Message::AgentStarted {
                        worktree_id,
                        unit: launch.unit,
                    }]
                }
                Err(e) => vec![Message::Failed(ErrorReport::new(
                    "could not start the agent",
                    &e,
                ))],
            }
        }

        Task::ClearAttention { session } => {
            match tmux::session::clear_attention(&worker.server, &session) {
                // Nothing to report either way: the row already stopped
                // showing attention when the latch was cleared.
                Ok(_) => Vec::new(),
                Err(e) => vec![Message::Failed(ErrorReport::new(
                    "could not clear the session's attention marker",
                    &e,
                ))],
            }
        }

        Task::RefreshSessions => match workflow::session_presence(&worker.server) {
            Ok(presence) => vec![Message::SessionsRefreshed(presence)],
            Err(e) => vec![Message::Failed(ErrorReport::new(
                "could not list tmux sessions",
                &e,
            ))],
        },

        Task::Activate {
            project_name,
            git_common_dir,
            worktree,
        } => {
            let worktree_id = worktree.id.clone();
            match workflow::activate_worktree(
                &worker.server,
                &worker.config,
                &project_name,
                &git_common_dir,
                &worktree,
            ) {
                Ok(activation) => {
                    let mut messages = vec![Message::Activated {
                        worktree_id,
                        activation,
                    }];
                    // Presence changed: the row must stop saying "no session".
                    messages.extend(handle(worker, Task::RefreshSessions));
                    messages
                }
                Err(e) => {
                    let mut messages = vec![Message::Failed(ErrorReport::new(
                        &format!("could not open {}", worktree.label()),
                        &e,
                    ))];
                    // The session may have been created before the failure.
                    messages.extend(handle(worker, Task::RefreshSessions));
                    messages
                }
            }
        }

        Task::OpenInNewTerminal {
            project_name,
            git_common_dir,
            worktree,
        } => {
            let worktree_id = worktree.id.clone();
            let mut messages = match workflow::open_in_new_terminal(
                &worker.server,
                &worker.config,
                &project_name,
                &git_common_dir,
                &worktree,
            ) {
                Ok(activation) => vec![Message::Activated {
                    worktree_id,
                    activation,
                }],
                Err(e) => vec![Message::Failed(ErrorReport::new(
                    &format!("could not open a terminal on {}", worktree.label()),
                    &e,
                ))],
            };
            messages.extend(handle(worker, Task::RefreshSessions));
            messages
        }

        Task::LoadBaseRefs {
            project_id,
            repository_path,
        } => match git::list_refs(&repository_path) {
            Ok(refs) => vec![Message::BaseRefsLoaded {
                project_id,
                refs,
                current: git::current_branch(&repository_path).unwrap_or(None),
            }],
            Err(e) => vec![Message::Failed(ErrorReport::new(
                "could not list branches",
                &e,
            ))],
        },

        Task::CreateWorktree {
            project_id,
            project_name,
            repository_path,
            git_common_dir,
            add,
            open_after,
        } => match workflow::create_worktree(&repository_path, &add) {
            Ok(path) => {
                let mut messages = vec![Message::WorktreeCreated {
                    project_id: project_id.clone(),
                    path: path.clone(),
                }];
                match workflow::refresh_project(
                    &worker.server,
                    &repository_path,
                    &project_id,
                    &git_common_dir,
                ) {
                    Ok(worktrees) => {
                        worker.enqueue(Task::RefreshStatuses {
                            project_id: project_id.clone(),
                            worktrees: worktrees.clone(),
                        });
                        if open_after
                            && let Some(worktree) = worktrees.iter().find(|w| w.path == path)
                        {
                            worker.enqueue(Task::Activate {
                                project_name,
                                git_common_dir,
                                worktree: Box::new(worktree.clone()),
                            });
                        }
                        messages.push(Message::WorktreesRefreshed {
                            project_id,
                            worktrees,
                        });
                    }
                    Err(e) => messages.push(Message::Failed(ErrorReport::new(
                        "the worktree was created but the list could not be refreshed",
                        &e,
                    ))),
                }
                messages
            }
            Err(e) => vec![Message::Failed(ErrorReport::new(
                "could not create the worktree",
                &e,
            ))],
        },

        Task::GatherRemoval {
            project_id,
            worktree,
        } => match workflow::removal_inputs(&worker.server, &worktree) {
            Ok(inputs) => vec![Message::RemovalGathered {
                project_id,
                worktree_id: worktree.id.clone(),
                report: Box::new(grove_core::removal::assemble(&inputs)),
            }],
            Err(e) => vec![Message::Failed(ErrorReport::new(
                &format!("could not inspect {}", worktree.label()),
                &e,
            ))],
        },

        Task::CloseSession {
            project_id,
            session,
        } => match tmux::session::kill_session(&worker.server, &session) {
            Ok(()) => {
                worker.enqueue(Task::RefreshSessions);
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
            repository_path,
            git_common_dir,
            worktree_path,
            force,
        } => match git::worktree_remove(&repository_path, &worktree_path, force) {
            Ok(()) => {
                worker.queue_refresh(&project_id, &repository_path, &git_common_dir);
                vec![Message::RemovalDone {
                    project_id,
                    operation: RemovalOp::RemoveWorktree,
                    detail: format!(
                        "Removed the worktree {}. The branch was not touched.",
                        worktree_path.display()
                    ),
                }]
            }
            Err(e) => {
                // Nothing was removed; the dialog shows git's own refusal and
                // only then offers --force.
                worker.queue_refresh(&project_id, &repository_path, &git_common_dir);
                vec![Message::RemovalFailed {
                    project_id,
                    operation: RemovalOp::RemoveWorktree,
                    report: ErrorReport::new("could not remove the worktree", &e),
                }]
            }
        },

        Task::DeleteBranch {
            project_id,
            repository_path,
            git_common_dir,
            branch,
            force,
        } => match git::branch_delete(&repository_path, &branch, force) {
            Ok(()) => {
                worker.queue_refresh(&project_id, &repository_path, &git_common_dir);
                vec![Message::RemovalDone {
                    project_id,
                    operation: RemovalOp::DeleteBranch,
                    detail: format!("Deleted the branch {branch}."),
                }]
            }
            Err(e) => vec![Message::RemovalFailed {
                project_id,
                operation: RemovalOp::DeleteBranch,
                report: ErrorReport::new(&format!("could not delete {branch}"), &e),
            }],
        },

        Task::SaveState(state) => match state::save(&worker.paths.state_file(), &state) {
            Ok(()) => Vec::new(),
            Err(e) => vec![Message::Failed(ErrorReport::new(
                "could not save state.toml",
                &e,
            ))],
        },

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
