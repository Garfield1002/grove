//! Read-only public views over Grove's current Git, tmux, and index state.
//!
//! These types are deliberately independent of the GUI. They are the first
//! stable automation surface for the CLI and are intended to be reused by a
//! future long-running Grove service.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Serialize;

use crate::Result;
use crate::git;
use crate::model::Worktree;
use crate::state::State;
use crate::tmux::{self, TmuxServer};

/// Version of the public JSON response shapes in this module.
pub const API_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectList {
    pub version: u32,
    pub projects: Vec<ProjectView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectView {
    pub id: String,
    pub name: String,
    pub repository_path: PathBuf,
    pub git_common_dir: PathBuf,
    pub default_worktree_path: PathBuf,
}

/// List the projects in Grove's index.
///
/// This intentionally performs no Git or filesystem access. The index says
/// what the user registered; `list_worktrees` reports whether each repository
/// can currently be read.
pub fn list_projects(state: &State) -> ProjectList {
    ProjectList {
        version: API_VERSION,
        projects: state
            .projects
            .iter()
            .map(|project| ProjectView {
                id: project.id.clone(),
                name: project.name.clone(),
                repository_path: project.repository_path.clone(),
                git_common_dir: project.git_common_dir.clone(),
                default_worktree_path: project.default_worktree_path.clone(),
            })
            .collect(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorktreeList {
    pub version: u32,
    pub worktrees: Vec<WorktreeView>,
    /// Registered projects Git could not currently inspect. One unavailable
    /// repository must not hide healthy worktrees from every other project.
    pub unavailable_projects: Vec<UnavailableProject>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnavailableProject {
    pub project_id: String,
    pub name: String,
    pub error: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    None,
    Stopped,
    Detached,
    Attached,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorktreeView {
    pub id: String,
    pub project_id: String,
    pub project_name: String,
    pub path: PathBuf,
    pub branch: Option<String>,
    pub head_commit: Option<String>,
    pub is_main: bool,
    pub is_bare: bool,
    pub is_detached: bool,
    pub is_locked: bool,
    pub lock_reason: Option<String>,
    pub is_prunable: bool,
    pub prune_reason: Option<String>,
    pub slot: Option<u8>,
    pub session_name: String,
    pub session_state: SessionState,
}

/// Read every registered repository and report its current Git worktrees.
///
/// Git and tmux are the sources of truth. A stale session record is reported
/// as `stopped`; it never causes a session to be created.
pub fn list_worktrees(state: &State, server: &TmuxServer) -> Result<WorktreeList> {
    let live_sessions = tmux::list_sessions(server)?;
    Ok(worktrees_from_live(state, &live_sessions))
}

fn worktrees_from_live(state: &State, live_sessions: &[tmux::SessionInfo]) -> WorktreeList {
    let live_by_id: HashMap<&str, &tmux::SessionInfo> = live_sessions
        .iter()
        .filter_map(|session| session.worktree_id().map(|id| (id, session)))
        .collect();
    let mut worktrees = Vec::new();
    let mut unavailable_projects = Vec::new();

    for project in &state.projects {
        let entries = match git::worktree_list(&project.repository_path) {
            Ok(entries) => entries,
            Err(error) => {
                unavailable_projects.push(UnavailableProject {
                    project_id: project.id.clone(),
                    name: project.name.clone(),
                    error: error.to_string(),
                });
                continue;
            }
        };

        for (index, entry) in entries.iter().enumerate() {
            let worktree =
                Worktree::from_entry(entry, &project.id, &project.git_common_dir, index == 0);
            let session_state = match live_by_id.get(worktree.id.as_str()) {
                Some(session) if session.attached > 0 => SessionState::Attached,
                Some(_) => SessionState::Detached,
                None if state.session(&worktree.id).is_some() => SessionState::Stopped,
                None => SessionState::None,
            };
            worktrees.push(WorktreeView {
                id: worktree.id.clone(),
                project_id: project.id.clone(),
                project_name: project.name.clone(),
                path: worktree.path.clone(),
                branch: worktree.branch.clone(),
                head_commit: worktree.head_commit.clone(),
                is_main: worktree.is_main,
                is_bare: worktree.is_bare,
                is_detached: worktree.is_detached,
                is_locked: worktree.is_locked,
                lock_reason: worktree.lock_reason.clone(),
                is_prunable: worktree.is_prunable,
                prune_reason: worktree.prune_reason.clone(),
                slot: state.slot(&worktree.id),
                session_name: worktree.session_name(),
                session_state,
            });
        }
    }

    WorktreeList {
        version: API_VERSION,
        worktrees,
        unavailable_projects,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionList {
    pub version: u32,
    pub sessions: Vec<SessionView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionView {
    pub name: String,
    pub worktree_id: Option<String>,
    pub project_name: Option<String>,
    pub worktree_path: Option<PathBuf>,
    pub git_common_dir: Option<PathBuf>,
    pub attached_clients: u32,
    pub attention: bool,
    pub last_activity_at: Option<u64>,
    pub bell: bool,
    /// Whether the session carries Grove's complete tmux metadata.
    pub managed: bool,
}

/// List live sessions on Grove's private tmux server.
pub fn list_sessions(server: &TmuxServer) -> Result<SessionList> {
    Ok(sessions_from_live(tmux::list_sessions(server)?))
}

fn sessions_from_live(live: Vec<tmux::SessionInfo>) -> SessionList {
    let sessions = live
        .into_iter()
        .map(|session| SessionView {
            worktree_id: session.worktree_id().map(str::to_string),
            project_name: session.metadata.project.clone(),
            worktree_path: session.metadata.worktree.clone(),
            git_common_dir: session.metadata.repo.clone(),
            attached_clients: session.attached,
            attention: session.attention,
            last_activity_at: session.activity_epoch,
            bell: session.bell,
            managed: session.metadata.is_complete(),
            name: session.name,
        })
        .collect();
    SessionList {
        version: API_VERSION,
        sessions,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Snapshot {
    pub version: u32,
    pub service_version: &'static str,
    pub protocol_version: u32,
    pub projects: Vec<ProjectView>,
    pub worktrees: Vec<WorktreeView>,
    pub unavailable_projects: Vec<UnavailableProject>,
    pub sessions: Vec<SessionView>,
    pub windows: Vec<WindowView>,
    pub slots: Vec<SlotView>,
    pub agents: Vec<AgentView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WindowView {
    pub session_name: String,
    pub index: u32,
    pub name: String,
    pub active: bool,
    pub bell: bool,
    pub title: Option<String>,
    pub last_activity_at: Option<u64>,
    pub commands: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SlotView {
    pub number: u8,
    pub worktree_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentView {
    pub worktree_id: String,
    pub session_id: String,
    pub transcript_path: PathBuf,
}

/// Collect one coherent service bootstrap view.
///
/// State is supplied as one already-loaded snapshot. Live tmux sessions and
/// panes are each listed exactly once, then reused to derive every public
/// record in this response.
pub fn snapshot(state: &State, server: &TmuxServer) -> Result<Snapshot> {
    let live_sessions = tmux::list_sessions(server)?;
    let panes = tmux::list_all_panes(server)?;
    let projects = list_projects(state);
    let worktrees = worktrees_from_live(state, &live_sessions);
    let sessions = sessions_from_live(live_sessions);
    let windows = tmux::windows_of(&panes)
        .into_iter()
        .map(|window| WindowView {
            session_name: window.session,
            index: window.index,
            name: window.name,
            active: window.active,
            bell: window.bell,
            title: window.title,
            last_activity_at: window.activity_epoch,
            commands: window.commands,
        })
        .collect();
    Ok(Snapshot {
        version: API_VERSION,
        service_version: env!("CARGO_PKG_VERSION"),
        protocol_version: crate::protocol::VERSION,
        projects: projects.projects,
        worktrees: worktrees.worktrees,
        unavailable_projects: worktrees.unavailable_projects,
        sessions: sessions.sessions,
        windows,
        slots: state
            .slots
            .iter()
            .map(|slot| SlotView {
                number: slot.number,
                worktree_id: slot.worktree_id.clone(),
            })
            .collect(),
        agents: state
            .agents
            .iter()
            .map(|agent| AgentView {
                worktree_id: agent.worktree_id.clone(),
                session_id: agent.session_id.clone(),
                transcript_path: agent.transcript_path.clone(),
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::ProjectRecord;
    use crate::tmux::SessionMetadata;
    use std::path::Path;

    #[test]
    fn project_view_is_public_data_not_ui_state() {
        let state = State {
            projects: vec![ProjectRecord {
                id: "abc123".into(),
                name: "grove".into(),
                repository_path: "/src/grove".into(),
                git_common_dir: "/src/grove/.git".into(),
                default_worktree_path: "/src".into(),
                is_expanded: false,
            }],
            ..State::default()
        };

        let value = serde_json::to_value(list_projects(&state)).expect("serializes");
        assert_eq!(value["version"], API_VERSION);
        assert_eq!(value["projects"][0]["id"], "abc123");
        assert_eq!(value["projects"][0]["repository_path"], "/src/grove");
        assert!(value["projects"][0].get("is_expanded").is_none());
    }

    #[test]
    fn public_enum_names_are_stable_snake_case() {
        assert_eq!(
            serde_json::to_string(&SessionState::None).expect("serializes"),
            "\"none\""
        );
        assert_eq!(
            serde_json::to_string(&SessionState::Stopped).expect("serializes"),
            "\"stopped\""
        );
    }

    #[test]
    fn session_views_preserve_every_live_signal_and_management_field() {
        let list = sessions_from_live(vec![tmux::SessionInfo {
            name: "renamed-session".into(),
            path: "/work/tree".into(),
            attached: 2,
            metadata: SessionMetadata {
                id: Some("abc123".into()),
                project: Some("Grove".into()),
                worktree: Some("/work/tree".into()),
                repo: Some("/repo/.git".into()),
            },
            attention: true,
            activity_epoch: Some(1234),
            bell: true,
        }]);

        assert_eq!(list.version, API_VERSION);
        assert_eq!(list.sessions.len(), 1);
        let session = &list.sessions[0];
        assert_eq!(session.name, "renamed-session");
        assert_eq!(session.worktree_id.as_deref(), Some("abc123"));
        assert_eq!(session.project_name.as_deref(), Some("Grove"));
        assert_eq!(
            session.worktree_path.as_deref(),
            Some(Path::new("/work/tree"))
        );
        assert_eq!(
            session.git_common_dir.as_deref(),
            Some(Path::new("/repo/.git"))
        );
        assert_eq!(session.attached_clients, 2);
        assert!(session.attention);
        assert_eq!(session.last_activity_at, Some(1234));
        assert!(session.bell);
        assert!(session.managed);
    }
}
