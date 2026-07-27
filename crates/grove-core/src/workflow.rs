//! Orchestration of the Milestone 1 flows.
//!
//! These functions run subprocesses and must only be called from a worker
//! thread. They live in the core crate so the UI layer stays a thin renderer
//! and so the sequencing is testable without a display.

use std::collections::HashMap;
use std::path::Path;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::git;
use crate::ids;
use crate::model::{Project, SessionPresence, Worktree, worktrees_from_entries};
use crate::terminal::{self, TemplateVars};
use crate::tmux::{self, SessionSpec, TmuxServer};

/// Register the project containing `path`, with its worktrees and current
/// session presence.
pub fn open_project(server: &TmuxServer, path: &Path) -> Result<Project> {
    let discovery = git::discover_project(path)?;
    let id = ids::project_id(&discovery.git_common_dir);
    let worktrees = worktrees_from_entries(&discovery.worktrees, &id, &discovery.git_common_dir);
    let mut project = Project {
        id,
        name: discovery.name,
        repository_path: discovery.repository_path,
        git_common_dir: discovery.git_common_dir,
        is_expanded: true,
        worktrees,
    };
    apply_session_presence(&mut project.worktrees, &session_presence(server)?);
    Ok(project)
}

/// Re-read a project's worktrees from git and its sessions from tmux.
pub fn refresh_project(
    server: &TmuxServer,
    repository_path: &Path,
    project_id: &str,
    git_common_dir: &Path,
) -> Result<Vec<Worktree>> {
    let entries = git::worktree_list(repository_path)?;
    let mut worktrees = worktrees_from_entries(&entries, project_id, git_common_dir);
    apply_session_presence(&mut worktrees, &session_presence(server)?);
    Ok(worktrees)
}

/// Session presence on the private server, keyed by tmux session name.
pub fn session_presence(server: &TmuxServer) -> Result<HashMap<String, SessionPresence>> {
    Ok(tmux::list_sessions(server)?
        .into_iter()
        .map(|session| {
            let presence = if session.attached > 0 {
                SessionPresence::Attached
            } else {
                SessionPresence::Detached
            };
            (session.name, presence)
        })
        .collect())
}

/// Stamp session presence onto a worktree list.
pub fn apply_session_presence(
    worktrees: &mut [Worktree],
    presence: &HashMap<String, SessionPresence>,
) {
    for worktree in worktrees {
        worktree.session = presence
            .get(&worktree.session_name())
            .copied()
            .unwrap_or(SessionPresence::None);
    }
}

/// What activating a worktree actually did, so the UI can say so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Activation {
    /// An attached client was retargeted at the session.
    SwitchedClient { session: String, client_tty: String },
    /// No client was attached, so a terminal was launched.
    LaunchedTerminal { session: String, command: String },
}

impl Activation {
    pub fn session(&self) -> &str {
        match self {
            Activation::SwitchedClient { session, .. }
            | Activation::LaunchedTerminal { session, .. } => session,
        }
    }
}

/// The session Grove would create for a worktree of a project.
pub fn session_spec(project_name: &str, git_common_dir: &Path, worktree: &Worktree) -> SessionSpec {
    SessionSpec {
        worktree_id: worktree.id.clone(),
        worktree_path: worktree.path.clone(),
        project_name: project_name.to_string(),
        git_common_dir: git_common_dir.to_path_buf(),
    }
}

/// Open a worktree (DESIGN.md §5): verify the worktree still exists, ensure
/// its session exists, then switch the primary client if one is attached or
/// launch the configured terminal if not.
pub fn activate_worktree(
    server: &TmuxServer,
    config: &Config,
    project_name: &str,
    git_common_dir: &Path,
    worktree: &Worktree,
) -> Result<Activation> {
    if !worktree.path.is_dir() {
        return Err(Error::WorktreeMissing(worktree.path.clone()));
    }
    let spec = session_spec(project_name, git_common_dir, worktree);
    let (session, _created) = tmux::ensure_session(server, &spec)?;

    let clients = tmux::list_clients(server)?;
    if let Some(client) = tmux::primary_client(&clients) {
        tmux::switch_client(server, client, &session)?;
        return Ok(Activation::SwitchedClient {
            session,
            client_tty: client.tty.to_string_lossy().into_owned(),
        });
    }

    if !config.has_terminal() {
        return Err(Error::EmptyTerminalTemplate);
    }
    let vars = TemplateVars::new(
        server.socket(),
        &session,
        &worktree.path,
        project_name,
        &worktree.label(),
    );
    let invocation = terminal::launch(&config.terminal.command, &vars)?;
    Ok(Activation::LaunchedTerminal {
        command: terminal::preview(&invocation),
        session,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::WorktreeEntry;
    use std::path::PathBuf;

    fn worktree(path: &str) -> Worktree {
        Worktree::from_entry(
            &WorktreeEntry {
                path: PathBuf::from(path),
                branch: Some("main".into()),
                ..WorktreeEntry::default()
            },
            "p1",
            Path::new("/home/u/proj/.git"),
            true,
        )
    }

    #[test]
    fn presence_is_matched_by_deterministic_session_name() {
        let mut worktrees = vec![worktree("/home/u/proj"), worktree("/home/u/wt/feature")];
        let mut presence = HashMap::new();
        presence.insert(worktrees[0].session_name(), SessionPresence::Attached);
        apply_session_presence(&mut worktrees, &presence);
        assert_eq!(worktrees[0].session, SessionPresence::Attached);
        assert_eq!(worktrees[1].session, SessionPresence::None);
    }

    #[test]
    fn unrelated_sessions_are_ignored() {
        let mut worktrees = vec![worktree("/home/u/proj")];
        let mut presence = HashMap::new();
        presence.insert("scratch".to_string(), SessionPresence::Attached);
        presence.insert("wt-ffffff".to_string(), SessionPresence::Detached);
        apply_session_presence(&mut worktrees, &presence);
        assert_eq!(worktrees[0].session, SessionPresence::None);
    }

    #[test]
    fn activating_a_missing_worktree_fails_before_touching_tmux() {
        let server = TmuxServer::new("/tmp/grove-test-never-used.sock");
        let err = activate_worktree(
            &server,
            &Config::default(),
            "proj",
            Path::new("/home/u/proj/.git"),
            &worktree("/nonexistent-grove/wt"),
        )
        .expect_err("worktree is gone");
        assert!(matches!(err, Error::WorktreeMissing(_)));
    }

    #[test]
    fn a_session_spec_carries_the_whole_mapping() {
        let worktree = worktree("/home/u/wt/feature");
        let spec = session_spec("acme-web", Path::new("/home/u/proj/.git"), &worktree);
        assert_eq!(spec.session_name(), worktree.session_name());
        assert_eq!(spec.worktree_path, worktree.path);
        assert_eq!(spec.project_name, "acme-web");
        assert_eq!(spec.git_common_dir, Path::new("/home/u/proj/.git"));
    }

    #[test]
    fn activation_reports_which_path_it_took() {
        let switched = Activation::SwitchedClient {
            session: "wt-a1b2c3".into(),
            client_tty: "/dev/pts/3".into(),
        };
        assert_eq!(switched.session(), "wt-a1b2c3");
        let launched = Activation::LaunchedTerminal {
            session: "wt-a1b2c3".into(),
            command: "foot tmux".into(),
        };
        assert_eq!(launched.session(), "wt-a1b2c3");
    }
}
