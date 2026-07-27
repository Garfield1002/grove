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
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};

use grove_core::config::{Config, LoadedConfig};
use grove_core::model::{Project, SessionPresence, Worktree};
use grove_core::state::State;
use grove_core::workflow::{self, Activation};
use grove_core::{Error, Paths, TmuxServer, config, state, terminal};

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
    /// Re-read session presence only.
    RefreshSessions,
    /// Open a worktree: ensure the session, then switch or launch.
    Activate {
        project_name: String,
        worktree: Box<Worktree>,
    },
    /// Persist the project index.
    SaveState(Box<State>),
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
    SessionsRefreshed(HashMap<String, SessionPresence>),
    Activated {
        worktree_id: String,
        activation: Activation,
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
        let spawned = std::thread::Builder::new()
            .name("grove-worker".into())
            .spawn(move || run(paths, task_rx, msg_tx.clone(), ctx.clone()));
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
}

fn run(paths: Paths, tasks: Receiver<Task>, out: Sender<Message>, ctx: egui::Context) {
    let server = TmuxServer::new(paths.tmux_socket());
    let mut worker = WorkerState {
        paths,
        server,
        config: Config::default(),
    };

    // The channel closes when the app exits; that ends the loop.
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
            }
        }

        Task::OpenProject(path) => match workflow::open_project(&worker.server, &path) {
            Ok(project) => vec![Message::ProjectOpened(Box::new(project))],
            Err(e) => vec![Message::Failed(ErrorReport::new(
                &format!("could not open {}", path.display()),
                &e,
            ))],
        },

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
            Ok(worktrees) => vec![Message::WorktreesRefreshed {
                project_id,
                worktrees,
            }],
            Err(e) => vec![Message::Failed(ErrorReport::new(
                &format!("could not refresh {}", repository_path.display()),
                &e,
            ))],
        },

        Task::RefreshSessions => match workflow::session_presence(&worker.server) {
            Ok(presence) => vec![Message::SessionsRefreshed(presence)],
            Err(e) => vec![Message::Failed(ErrorReport::new(
                "could not list tmux sessions",
                &e,
            ))],
        },

        Task::Activate {
            project_name,
            worktree,
        } => {
            let worktree_id = worktree.id.clone();
            match workflow::activate_worktree(
                &worker.server,
                &worker.config,
                &project_name,
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
