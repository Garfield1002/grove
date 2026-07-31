//! Startup, refresh and restore reconciliation (ARCHITECTURE.md §7).
//!
//! Reconciliation diffs three sources: Grove's own `state.toml` index, what
//! `git worktree list --porcelain` reports for each registered project, and
//! what `tmux list-sessions` reports on the private server. It **marks**:
//! missing worktree paths become *unavailable*, sessions Grove knew about and
//! tmux no longer has become *stopped*, and sessions with no worktree become
//! *orphaned*. It never deletes a worktree, a branch, a session or a project
//! record — not even one that is absent from every other source
//! (ARCHITECTURE.md §8.1). Every removal stays a separate, separately
//! confirmed operation.
//!
//! Sessions are matched **primarily by the `@grove_*` user options** the tmux
//! server carries for each session, falling back to the `wt-<id>` session name
//! and then to the worktree path. All three survive losing `state.toml`: the
//! ids are deterministic (see [`crate::ids`]) and the mapping lives on the
//! tmux server itself, so restore re-derives the same ids and finds the same
//! sessions.
//!
//! The diff itself ([`reconcile`]) is pure: it takes already-gathered values
//! and runs no subprocess and touches no filesystem. [`reconcile_all`] is the
//! thin IO wrapper around it and must be called from a worker thread only.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::git;
use crate::model::{SessionPresence, Worktree, worktrees_from_entries};
use crate::tmux::{SessionInfo, TmuxServer};

/// A project as Grove's index knows it, before git is consulted.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProjectRef {
    pub id: String,
    pub name: String,
    /// Main worktree or bare repository directory.
    pub repository_path: PathBuf,
    /// Repository identity: `git rev-parse --git-common-dir`.
    pub git_common_dir: PathBuf,
}

/// What git had to say about one project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectSnapshot {
    pub project: ProjectRef,
    /// Why the project could not be read, if it could not be. The project is
    /// then shown as *unavailable* (DESIGN.md §11) and keeps everything it
    /// has: nothing is removed because a drive is unplugged.
    pub unavailable: Option<String>,
    /// The worktrees git reported, with [`Worktree::is_missing`] already set
    /// from the filesystem. Empty when the project is unavailable.
    pub worktrees: Vec<Worktree>,
}

impl ProjectSnapshot {
    /// A snapshot for a project git could not be read for.
    pub fn unavailable(project: ProjectRef, reason: impl Into<String>) -> Self {
        Self {
            project,
            unavailable: Some(reason.into()),
            worktrees: Vec::new(),
        }
    }
}

/// Why a live tmux session has no worktree to belong to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrphanReason {
    /// The session names a repository Grove has registered, but none of that
    /// project's worktrees matches it any more — the worktree was removed
    /// while the session kept running.
    WorktreeGone,
    /// The session is one of Grove's, but its repository is not a project
    /// Grove currently has registered.
    UnknownProject,
}

impl OrphanReason {
    pub fn description(self) -> &'static str {
        match self {
            OrphanReason::WorktreeGone => "its worktree is gone",
            OrphanReason::UnknownProject => "its project is not registered",
        }
    }
}

/// A session on the private server with no worktree behind it (DESIGN.md §11).
///
/// Grove offers to open it, associate it with a worktree, close it or ignore
/// it — four things the user chooses between. Reconciliation itself does none
/// of them.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OrphanSession {
    pub name: String,
    /// `@grove_id`, else the id in the `wt-<id>` name.
    pub worktree_id: Option<String>,
    /// `@grove_worktree`, else the session's own working directory.
    pub worktree_path: Option<PathBuf>,
    /// `@grove_repo`: the repository the session was created for.
    pub repo: Option<PathBuf>,
    /// `@grove_project`: the project name at creation time.
    pub project: Option<String>,
    pub attached: bool,
    pub reason: OrphanReason,
}

impl OrphanSession {
    /// One line describing the session for the restore UI.
    pub fn detail(&self) -> String {
        let mut parts = vec![self.reason.description().to_string()];
        if let Some(project) = &self.project {
            parts.push(project.clone());
        }
        if let Some(path) = &self.worktree_path {
            parts.push(path.display().to_string());
        }
        if self.attached {
            parts.push("attached".to_string());
        }
        parts.join(" · ")
    }
}

/// One project after reconciliation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProjectStatus {
    pub id: String,
    pub name: String,
    /// Why the project could not be read, if it could not be.
    pub unavailable: Option<String>,
    /// The rows to show, with session presence, *stopped* and *unavailable*
    /// already stamped on. Empty for an unavailable project, whose last known
    /// rows the UI keeps rather than blanking.
    pub worktrees: Vec<Worktree>,
}

/// The result of one reconciliation pass.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Reconciliation {
    pub projects: Vec<ProjectStatus>,
    pub orphans: Vec<OrphanSession>,
    /// Orphaned sessions the user has asked Grove to ignore. Counted, not
    /// listed: the UI offers to report them again.
    pub ignored: usize,
}

impl Reconciliation {
    pub fn unavailable_projects(&self) -> usize {
        self.projects
            .iter()
            .filter(|p| p.unavailable.is_some())
            .count()
    }

    fn worktrees(&self) -> impl Iterator<Item = &Worktree> {
        self.projects.iter().flat_map(|p| p.worktrees.iter())
    }

    pub fn missing_worktrees(&self) -> usize {
        self.worktrees().filter(|w| w.is_missing).count()
    }

    pub fn live_sessions(&self) -> usize {
        self.worktrees().filter(|w| w.session.exists()).count()
    }

    pub fn stopped_sessions(&self) -> usize {
        self.worktrees()
            .filter(|w| w.session_stopped && !w.session.exists())
            .count()
    }

    /// The status line after a restore: only what is actually true, so a
    /// clean reconciliation reads as one short sentence.
    pub fn summary(&self) -> String {
        let mut parts = vec![count(self.projects.len(), "project", "projects")];
        let counts = [
            (self.live_sessions(), "session running", "sessions running"),
            (self.stopped_sessions(), "stopped", "stopped"),
            (
                self.missing_worktrees(),
                "worktree unavailable",
                "worktrees unavailable",
            ),
            (
                self.unavailable_projects(),
                "project unavailable",
                "projects unavailable",
            ),
            (self.orphans.len(), "orphaned session", "orphaned sessions"),
        ];
        for (n, one, many) in counts {
            if n > 0 {
                parts.push(count(n, one, many));
            }
        }
        format!("Reconciled {}.", parts.join(", "))
    }
}

fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Is this a session Grove created, or one that merely lives on the private
/// server? Sessions the user started by hand are none of Grove's business:
/// they are neither adopted nor reported as orphans.
fn is_grove_session(session: &SessionInfo) -> bool {
    session.worktree_id().is_some()
        || session.metadata.worktree.is_some()
        || session.metadata.repo.is_some()
}

/// Diff Grove's index against git and tmux.
///
/// `recorded` is the worktree ids `state.toml` has a session mapping for; a
/// worktree in that list whose session tmux no longer reports is *stopped*.
/// `ignored` is the orphan session names the user has silenced.
///
/// Pure: no subprocess, no filesystem. The caller gathers the inputs.
pub fn reconcile(
    snapshots: Vec<ProjectSnapshot>,
    sessions: &[SessionInfo],
    recorded: &[String],
    ignored: &[String],
) -> Reconciliation {
    // Primary key: the `@grove_id` user option, falling back to the `wt-<id>`
    // session name. Both are the same deterministic id.
    let mut by_id: HashMap<&str, &SessionInfo> = HashMap::new();
    // Fallback for a session whose id option was lost or hand-edited but whose
    // recorded worktree path still names a worktree Grove knows.
    let mut by_path: HashMap<&Path, &SessionInfo> = HashMap::new();
    for session in sessions {
        if !is_grove_session(session) {
            continue;
        }
        if let Some(id) = session.worktree_id() {
            by_id.entry(id).or_insert(session);
        }
        if let Some(path) = session.metadata.worktree.as_deref() {
            by_path.entry(path).or_insert(session);
        }
    }

    // Kept before the snapshots are consumed: an orphan whose `@grove_repo` is
    // a registered project has lost its worktree, while one whose repository
    // Grove does not know belongs to a project that is not registered here.
    let known_repos: Vec<PathBuf> = snapshots
        .iter()
        .map(|s| s.project.git_common_dir.clone())
        .collect();

    let mut claimed: Vec<&str> = Vec::new();
    let mut projects = Vec::new();
    for snapshot in snapshots {
        let repo = snapshot.project.git_common_dir.as_path();
        let mut worktrees = snapshot.worktrees;
        for worktree in &mut worktrees {
            let matched = by_id.get(worktree.id.as_str()).copied().or_else(|| {
                // The path fallback is only safe within one repository:
                // two projects can hold worktrees at the same path only if
                // one of them moved, and matching by path alone would then
                // hand a session to the wrong project. A session that
                // records no repository at all is matched on path, which
                // is all it offers.
                by_path
                    .get(worktree.path.as_path())
                    .copied()
                    .filter(|session| match session.metadata.repo.as_deref() {
                        Some(session_repo) => session_repo == repo,
                        None => true,
                    })
            });
            worktree.session = match matched {
                Some(session) if session.attached > 0 => SessionPresence::Attached,
                Some(_) => SessionPresence::Detached,
                None => SessionPresence::None,
            };
            // A record and no session is a session that stopped; a record and
            // a session is simply the normal case.
            worktree.session_stopped =
                matched.is_none() && recorded.iter().any(|id| id == &worktree.id);
            if let Some(session) = matched {
                claimed.push(session.name.as_str());
            }
        }
        projects.push(ProjectStatus {
            id: snapshot.project.id,
            name: snapshot.project.name,
            unavailable: snapshot.unavailable,
            worktrees,
        });
    }

    // Anything left over that Grove created but no worktree owns is an orphan.
    let mut orphans = Vec::new();
    let mut ignored_count = 0;
    for session in sessions {
        if !is_grove_session(session) || claimed.contains(&session.name.as_str()) {
            continue;
        }
        if ignored.iter().any(|name| name == &session.name) {
            ignored_count += 1;
            continue;
        }
        orphans.push(OrphanSession {
            name: session.name.clone(),
            worktree_id: session.worktree_id().map(str::to_string),
            worktree_path: session
                .metadata
                .worktree
                .clone()
                .or_else(|| Some(session.path.clone())),
            repo: session.metadata.repo.clone(),
            project: session.metadata.project.clone(),
            attached: session.attached > 0,
            reason: match session.metadata.repo.as_deref() {
                Some(repo) if known_repos.iter().any(|known| known == repo) => {
                    OrphanReason::WorktreeGone
                }
                _ => OrphanReason::UnknownProject,
            },
        });
    }

    Reconciliation {
        projects,
        orphans,
        ignored: ignored_count,
    }
}

/// Read one project from git, marking it unavailable rather than failing.
///
/// Runs subprocesses and stats the filesystem: worker thread only.
pub fn snapshot_project(project: &ProjectRef) -> ProjectSnapshot {
    if !project.repository_path.is_dir() {
        return ProjectSnapshot::unavailable(
            project.clone(),
            format!("{} is not there", project.repository_path.display()),
        );
    }
    match git::worktree_list(&project.repository_path) {
        Ok(entries) => {
            let mut worktrees =
                worktrees_from_entries(&entries, &project.id, &project.git_common_dir);
            for worktree in &mut worktrees {
                // Bare "worktrees" have no working tree to be missing.
                worktree.is_missing = !worktree.is_bare && !worktree.path.is_dir();
            }
            ProjectSnapshot {
                project: project.clone(),
                unavailable: None,
                worktrees,
            }
        }
        Err(e) => ProjectSnapshot::unavailable(project.clone(), e.to_string()),
    }
}

/// Reconcile every registered project against git and tmux.
///
/// One `list-sessions` for the whole pass, plus one `worktree list` per
/// project. A project git refuses is reported as unavailable; only tmux
/// failing (which "no server running" is not) fails the whole pass.
///
/// Runs subprocesses: worker thread only.
pub fn reconcile_all(
    server: &TmuxServer,
    projects: &[ProjectRef],
    recorded: &[String],
    ignored: &[String],
) -> Result<Reconciliation> {
    let sessions = crate::tmux::list_sessions(server)?;
    let snapshots = projects.iter().map(snapshot_project).collect();
    Ok(reconcile(snapshots, &sessions, recorded, ignored))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids;
    use crate::tmux::SessionMetadata;
    use crate::tmux::session::parse_sessions;

    const REPO: &str = "/home/u/proj/.git";

    fn project_ref() -> ProjectRef {
        ProjectRef {
            id: "p1".into(),
            name: "acme-web".into(),
            repository_path: PathBuf::from("/home/u/proj"),
            git_common_dir: PathBuf::from(REPO),
        }
    }

    fn worktree(path: &str) -> Worktree {
        use crate::git::WorktreeEntry;
        Worktree::from_entry(
            &WorktreeEntry {
                path: PathBuf::from(path),
                branch: Some("main".into()),
                ..WorktreeEntry::default()
            },
            "p1",
            Path::new(REPO),
            false,
        )
    }

    fn snapshot(paths: &[&str]) -> ProjectSnapshot {
        ProjectSnapshot {
            project: project_ref(),
            unavailable: None,
            worktrees: paths.iter().map(|p| worktree(p)).collect(),
        }
    }

    /// A session as tmux reports it, with the full `@grove_*` mapping.
    fn session(name: &str, path: &str, attached: u32) -> SessionInfo {
        SessionInfo {
            name: name.to_string(),
            path: PathBuf::from(path),
            attached,
            metadata: SessionMetadata {
                id: ids::id_from_session_name(name).map(str::to_string),
                project: Some("acme-web".into()),
                worktree: Some(PathBuf::from(path)),
                repo: Some(PathBuf::from(REPO)),
            },
            attention: false,
            done: false,
            activity_epoch: None,
            bell: false,
        }
    }

    fn grove_session_for(path: &str, attached: u32) -> SessionInfo {
        let id = ids::worktree_id(Path::new(REPO), Path::new(path));
        session(&ids::session_name(&id), path, attached)
    }

    fn only(reconciliation: &Reconciliation) -> &[Worktree] {
        &reconciliation.projects[0].worktrees
    }

    #[test]
    fn a_live_session_is_matched_by_its_grove_id_option() {
        let result = reconcile(
            vec![snapshot(&["/home/u/proj", "/home/u/wt/auth"])],
            &[grove_session_for("/home/u/wt/auth", 0)],
            &[],
            &[],
        );
        assert_eq!(only(&result)[0].session, SessionPresence::None);
        assert_eq!(only(&result)[1].session, SessionPresence::Detached);
        assert_eq!(result.live_sessions(), 1);
        assert!(result.orphans.is_empty());
    }

    #[test]
    fn an_attached_session_is_reported_as_attached() {
        let result = reconcile(
            vec![snapshot(&["/home/u/wt/auth"])],
            &[grove_session_for("/home/u/wt/auth", 2)],
            &[],
            &[],
        );
        assert_eq!(only(&result)[0].session, SessionPresence::Attached);
    }

    /// The point of the user options: a session renamed by hand is still that
    /// worktree's session, because `@grove_id` says so.
    #[test]
    fn the_user_option_matches_a_session_whose_name_was_changed() {
        let path = "/home/u/wt/auth";
        let id = ids::worktree_id(Path::new(REPO), Path::new(path));
        let mut renamed = session("scratch", path, 0);
        renamed.metadata.id = Some(id);

        let result = reconcile(vec![snapshot(&[path])], &[renamed], &[], &[]);
        assert_eq!(only(&result)[0].session, SessionPresence::Detached);
        assert!(
            result.orphans.is_empty(),
            "a renamed session is matched, not orphaned"
        );
    }

    /// And with no user options at all — an old session, or a server that lost
    /// them — the deterministic `wt-<id>` name still finds it.
    #[test]
    fn the_session_name_is_the_fallback_key() {
        let path = "/home/u/wt/auth";
        let id = ids::worktree_id(Path::new(REPO), Path::new(path));
        let bare = SessionInfo {
            metadata: SessionMetadata::default(),
            ..session(&ids::session_name(&id), path, 0)
        };
        let result = reconcile(vec![snapshot(&[path])], &[bare], &[], &[]);
        assert_eq!(only(&result)[0].session, SessionPresence::Detached);
    }

    /// Last fallback: the recorded worktree path, for a session whose id
    /// option was hand-edited into nonsense and whose name is not ours.
    #[test]
    fn the_worktree_path_is_the_last_fallback_key() {
        let path = "/home/u/wt/auth";
        let mut odd = session("my-work", path, 0);
        odd.metadata.id = None;
        let result = reconcile(vec![snapshot(&[path])], &[odd], &[], &[]);
        assert_eq!(only(&result)[0].session, SessionPresence::Detached);
        assert!(result.orphans.is_empty());
    }

    /// The path fallback must not reach across repositories: a session that
    /// records a different repo is that repo's, whatever its directory says.
    #[test]
    fn the_path_fallback_stays_inside_one_repository() {
        let path = "/home/u/wt/auth";
        let mut elsewhere = session("someones-session", path, 0);
        elsewhere.metadata.id = None;
        elsewhere.metadata.repo = Some(PathBuf::from("/home/u/other/.git"));

        let result = reconcile(vec![snapshot(&[path])], &[elsewhere], &[], &[]);
        assert_eq!(only(&result)[0].session, SessionPresence::None);
        assert_eq!(result.orphans.len(), 1);
    }

    #[test]
    fn a_worktree_with_a_record_and_no_session_is_stopped() {
        let path = "/home/u/wt/auth";
        let id = ids::worktree_id(Path::new(REPO), Path::new(path));
        let result = reconcile(vec![snapshot(&[path])], &[], &[id], &[]);
        assert!(only(&result)[0].session_stopped);
        assert_eq!(only(&result)[0].session, SessionPresence::None);
        assert_eq!(result.stopped_sessions(), 1);
        assert_eq!(only(&result)[0].sublabel(), "session stopped");
    }

    #[test]
    fn a_worktree_grove_never_had_a_session_for_is_not_stopped() {
        let result = reconcile(vec![snapshot(&["/home/u/wt/auth"])], &[], &[], &[]);
        assert!(!only(&result)[0].session_stopped);
        assert_eq!(result.stopped_sessions(), 0);
    }

    /// A record for a session that is in fact still running must not make the
    /// row claim it stopped.
    #[test]
    fn a_record_never_overrides_a_live_session() {
        let path = "/home/u/wt/auth";
        let id = ids::worktree_id(Path::new(REPO), Path::new(path));
        let result = reconcile(
            vec![snapshot(&[path])],
            &[grove_session_for(path, 0)],
            &[id],
            &[],
        );
        assert!(!only(&result)[0].session_stopped);
    }

    #[test]
    fn a_session_with_no_worktree_is_orphaned_never_closed() {
        let result = reconcile(
            vec![snapshot(&["/home/u/proj"])],
            &[grove_session_for("/home/u/wt/deleted", 1)],
            &[],
            &[],
        );
        assert_eq!(result.orphans.len(), 1);
        let orphan = &result.orphans[0];
        assert_eq!(
            orphan.worktree_path,
            Some(PathBuf::from("/home/u/wt/deleted"))
        );
        assert_eq!(orphan.reason, OrphanReason::WorktreeGone);
        assert!(orphan.attached);
        assert!(orphan.detail().contains("its worktree is gone"));
    }

    #[test]
    fn a_session_from_an_unregistered_project_says_so() {
        let mut foreign = grove_session_for("/home/u/other/wt", 0);
        foreign.metadata.repo = Some(PathBuf::from("/home/u/other/.git"));
        let result = reconcile(vec![snapshot(&["/home/u/proj"])], &[foreign], &[], &[]);
        assert_eq!(result.orphans[0].reason, OrphanReason::UnknownProject);
    }

    /// Sessions the user started by hand on the private server are not Grove's
    /// business: never adopted, never reported, never touched.
    #[test]
    fn foreign_sessions_are_ignored_entirely() {
        let scratch = SessionInfo {
            metadata: SessionMetadata::default(),
            ..session("scratch", "/home/u", 0)
        };
        let result = reconcile(vec![snapshot(&["/home/u/proj"])], &[scratch], &[], &[]);
        assert!(result.orphans.is_empty());
    }

    #[test]
    fn an_ignored_orphan_is_counted_but_not_listed() {
        let orphan = grove_session_for("/home/u/wt/deleted", 0);
        let name = orphan.name.clone();
        let result = reconcile(
            vec![snapshot(&["/home/u/proj"])],
            &[orphan],
            &[],
            std::slice::from_ref(&name),
        );
        assert!(result.orphans.is_empty());
        assert_eq!(result.ignored, 1);

        // And un-ignoring it brings it straight back.
        let result = reconcile(
            vec![snapshot(&["/home/u/proj"])],
            &[grove_session_for("/home/u/wt/deleted", 0)],
            &[],
            &[],
        );
        assert_eq!(result.orphans.len(), 1);
        assert_eq!(result.orphans[0].name, name);
    }

    #[test]
    fn an_unavailable_project_keeps_its_record_and_reports_the_reason() {
        let result = reconcile(
            vec![ProjectSnapshot::unavailable(
                project_ref(),
                "/home/u/proj is not there",
            )],
            &[],
            &[],
            &[],
        );
        assert_eq!(result.projects.len(), 1, "the project is never dropped");
        assert_eq!(
            result.projects[0].unavailable.as_deref(),
            Some("/home/u/proj is not there")
        );
        assert_eq!(result.unavailable_projects(), 1);
    }

    /// A worktree whose directory has gone is marked, not removed, and its
    /// still-running session is still matched to it.
    #[test]
    fn a_missing_worktree_is_marked_unavailable_and_keeps_its_session() {
        let path = "/home/u/wt/auth";
        let mut snapshot = snapshot(&[path]);
        snapshot.worktrees[0].is_missing = true;
        let result = reconcile(vec![snapshot], &[grove_session_for(path, 0)], &[], &[]);
        assert!(only(&result)[0].is_missing);
        assert_eq!(only(&result)[0].session, SessionPresence::Detached);
        assert_eq!(result.missing_worktrees(), 1);
    }

    #[test]
    fn two_projects_do_not_steal_each_others_sessions() {
        let other = ProjectRef {
            id: "p2".into(),
            name: "design".into(),
            repository_path: PathBuf::from("/home/u/design"),
            git_common_dir: PathBuf::from("/home/u/design/.git"),
        };
        // The same path in a different repository hashes to a different id.
        let shared = "/home/u/shared";
        let mut second = ProjectSnapshot {
            project: other,
            unavailable: None,
            worktrees: vec![worktree(shared)],
        };
        second.worktrees[0].id =
            ids::worktree_id(Path::new("/home/u/design/.git"), Path::new(shared));

        let result = reconcile(
            vec![snapshot(&[shared]), second],
            &[grove_session_for(shared, 0)],
            &[],
            &[],
        );
        assert_eq!(
            result.projects[0].worktrees[0].session,
            SessionPresence::Detached
        );
        assert_eq!(
            result.projects[1].worktrees[0].session,
            SessionPresence::None,
            "matching is by (repository, path), never by path alone"
        );
    }

    #[test]
    fn nothing_at_all_reconciles_cleanly() {
        let result = reconcile(Vec::new(), &[], &[], &[]);
        assert_eq!(result, Reconciliation::default());
        assert_eq!(result.summary(), "Reconciled 0 projects.");
    }

    #[test]
    fn the_summary_mentions_only_what_is_true() {
        let path = "/home/u/wt/auth";
        let result = reconcile(
            vec![snapshot(&["/home/u/proj", path])],
            &[grove_session_for(path, 0)],
            &[],
            &[],
        );
        assert_eq!(result.summary(), "Reconciled 1 project, 1 session running.");

        let result = reconcile(
            vec![snapshot(&["/home/u/proj"])],
            &[grove_session_for("/home/u/wt/gone", 0)],
            &[],
            &[],
        );
        assert_eq!(
            result.summary(),
            "Reconciled 1 project, 1 orphaned session."
        );
    }

    /// Reconciliation must survive whatever tmux hands it, including a listing
    /// with empty user-option fields and unusual names.
    #[test]
    fn malformed_and_partial_session_metadata_is_tolerated() {
        let text = "\
scratch\u{1}/home/u\u{1}0\u{1}\u{1}\u{1}\u{1}\n\
wt-a1b2c3\u{1}/home/u/wt/x\u{1}0\n\
wt-zzzzzz\u{1}/home/u/wt/y\u{1}0\u{1}\u{1}\u{1}/home/u/wt/y\u{1}\n";
        let sessions = parse_sessions(text).expect("tmux output parses");
        let result = reconcile(vec![snapshot(&["/home/u/proj"])], &sessions, &[], &[]);

        // `scratch` carries nothing of Grove's and is left alone; the other
        // two are Grove's (one by name, one by its worktree option).
        let names: Vec<&str> = result.orphans.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(names, vec!["wt-a1b2c3", "wt-zzzzzz"]);
        assert_eq!(result.orphans[0].worktree_id.as_deref(), Some("a1b2c3"));
        assert_eq!(
            result.orphans[1].worktree_id, None,
            "`wt-zzzzzz` is not a hex id, so there is no id to report"
        );
        assert_eq!(result.orphans[1].reason, OrphanReason::UnknownProject);
    }

    /// Two sessions claiming the same worktree (the user renamed one by hand):
    /// the first wins the row and the other is reported, rather than either
    /// being closed or silently dropped.
    #[test]
    fn a_duplicate_session_for_one_worktree_becomes_an_orphan() {
        let path = "/home/u/wt/auth";
        let first = grove_session_for(path, 0);
        let mut second = session("wt-auth-copy", path, 0);
        second.metadata.id = None;

        let result = reconcile(vec![snapshot(&[path])], &[first.clone(), second], &[], &[]);
        assert_eq!(only(&result)[0].session, SessionPresence::Detached);
        assert_eq!(result.orphans.len(), 1);
        assert_eq!(result.orphans[0].name, "wt-auth-copy");
    }

    #[test]
    fn snapshot_reports_a_missing_directory_as_unavailable() {
        let snapshot = snapshot_project(&ProjectRef {
            repository_path: PathBuf::from("/nonexistent-grove/proj"),
            ..project_ref()
        });
        assert!(snapshot.unavailable.is_some());
        assert!(snapshot.worktrees.is_empty());
    }
}
