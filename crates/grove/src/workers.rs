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
use grove_core::git::{RefEntry, StatusSummary, WorktreeAdd};
use grove_core::model::{Project, SessionPresence, Worktree};
use grove_core::removal::RemovalReport;
use grove_core::state::State;
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
    Failed(ErrorReport),
}

/// Handle used by the UI to queue work.
pub struct Workers {
    tx: Sender<Task>,
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
        let spawned = std::thread::Builder::new()
            .name("grove-worker".into())
            .spawn(move || run(paths, task_rx, own_tx, msg_tx.clone(), ctx.clone()));
        if let Err(e) = spawned {
            eprintln!("grove: could not start the worker thread: {e}");
        }

        (Self { tx: task_tx }, msg_rx)
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grove_core::error::CommandFailure;

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
