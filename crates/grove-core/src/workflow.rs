//! Orchestration of the Milestone 1 flows.
//!
//! These functions run subprocesses and must only be called from a worker
//! thread. They live in the core crate so the UI layer stays a thin renderer
//! and so the sequencing is testable without a display.

use std::collections::HashMap;
use std::path::Path;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::git::{self, StatusSummary, WorktreeAdd};
use crate::ids;
use crate::model::{
    Project, SessionPresence, Worktree, default_worktree_parent, worktrees_from_entries,
};
use crate::removal::{RemovalInputs, Unpushed};
use crate::terminal::{self, TemplateVars};
use crate::tmux::{self, SessionSpec, TmuxServer};

/// Register the project containing `path`, with its worktrees and current
/// session presence.
pub fn open_project(server: &TmuxServer, config: &Config, path: &Path) -> Result<Project> {
    let discovery = git::discover_project(path)?;
    let id = ids::project_id(&discovery.git_common_dir);
    let worktrees = worktrees_from_entries(&discovery.worktrees, &id, &discovery.git_common_dir);
    let default_worktree_path =
        default_worktree_parent(config.default_worktree_parent(), &discovery.repository_path);
    let mut project = Project {
        id,
        name: discovery.name,
        repository_path: discovery.repository_path,
        git_common_dir: discovery.git_common_dir,
        default_worktree_path,
        is_expanded: true,
        worktrees,
    };
    apply_session_presence(&mut project.worktrees, &session_presence(server)?);
    Ok(project)
}

/// Read the working-tree status of every worktree, keyed by worktree id
/// (DESIGN.md §18).
///
/// Bare and missing worktrees have no working tree, so they are skipped
/// rather than reported as errors, and a single failing worktree does not
/// hide the others: this runs on the worker to keep sublabels fresh, not as
/// part of any operation the user is waiting on.
///
/// Runs subprocesses: worker thread only.
pub fn worktree_statuses(worktrees: &[Worktree]) -> HashMap<String, StatusSummary> {
    let mut statuses = HashMap::new();
    for worktree in worktrees {
        if worktree.is_bare || !worktree.path.is_dir() {
            continue;
        }
        if let Ok(status) = git::status_summary(&worktree.path) {
            statuses.insert(worktree.id.clone(), status);
        }
    }
    statuses
}

/// Stamp statuses onto a worktree list, leaving worktrees with no reading
/// untouched.
pub fn apply_statuses(worktrees: &mut [Worktree], statuses: &HashMap<String, StatusSummary>) {
    for worktree in worktrees {
        if let Some(status) = statuses.get(&worktree.id) {
            worktree.git_status = Some(status.clone());
        }
    }
}

/// Create a worktree, then report where it landed (DESIGN.md §10).
///
/// Git's own stderr survives a failure untouched, which is what the create
/// dialog shows.
///
/// Runs a subprocess: worker thread only.
pub fn create_worktree(repository_path: &Path, add: &WorktreeAdd) -> Result<std::path::PathBuf> {
    git::worktree_add(repository_path, add)
}

/// Gather everything the safe-removal dialog must display *before* offering
/// any destructive operation (DESIGN.md §13).
///
/// Best effort by design: a worktree whose directory has vanished, or whose
/// branch tracks nothing, still produces a report — with the unknowns named
/// as unknown. Nothing here removes, kills or deletes anything.
///
/// Runs subprocesses: worker thread only.
pub fn removal_inputs(server: &TmuxServer, worktree: &Worktree) -> Result<RemovalInputs> {
    let status = git::status_summary(&worktree.path).ok();

    let unpushed = match &status {
        Some(status) => match &status.upstream {
            Some(upstream) => match git::status::unpushed_count(&worktree.path, upstream) {
                Ok(count) => Unpushed::Count(count),
                Err(e) => Unpushed::Unknown(e.to_string()),
            },
            None if status.detached => Unpushed::Unknown("HEAD is detached".to_string()),
            None => Unpushed::NoUpstream,
        },
        None => Unpushed::Unknown("the worktree status could not be read".to_string()),
    };

    let session_name = worktree.session_name();
    let session = tmux::has_session(server, &session_name)?.then_some(session_name.clone());
    let panes = match &session {
        Some(session) => tmux::list_panes(server, session)?,
        None => Vec::new(),
    };

    Ok(RemovalInputs {
        worktree_path: worktree.path.clone(),
        branch: worktree.branch.clone(),
        is_main: worktree.is_main,
        is_locked: worktree.is_locked,
        lock_reason: worktree.lock_reason.clone(),
        status,
        unpushed,
        session,
        panes,
    })
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
    fn statuses_are_matched_by_worktree_id_and_never_invented() {
        let mut worktrees = vec![worktree("/home/u/proj"), worktree("/home/u/wt/feature")];
        let mut statuses = HashMap::new();
        statuses.insert(
            worktrees[0].id.clone(),
            StatusSummary {
                modified: 2,
                ..StatusSummary::default()
            },
        );
        statuses.insert("ffffff".to_string(), StatusSummary::default());
        apply_statuses(&mut worktrees, &statuses);
        assert_eq!(
            worktrees[0].git_status.as_ref().map(|s| s.modified),
            Some(2)
        );
        assert_eq!(
            worktrees[1].git_status, None,
            "a worktree with no reading must not be shown as clean"
        );
    }

    #[test]
    fn a_previous_status_survives_a_refresh_that_could_not_read_it() {
        let mut worktrees = vec![worktree("/home/u/proj")];
        worktrees[0].git_status = Some(StatusSummary {
            untracked: 1,
            ..StatusSummary::default()
        });
        apply_statuses(&mut worktrees, &HashMap::new());
        assert!(worktrees[0].git_status.is_some());
    }

    #[test]
    fn bare_and_missing_worktrees_are_not_asked_for_a_status() {
        let mut bare = worktree("/nonexistent-grove/bare");
        bare.is_bare = true;
        let missing = worktree("/nonexistent-grove/gone");
        // Neither runs git: both are skipped before any subprocess.
        assert!(worktree_statuses(&[bare, missing]).is_empty());
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
