//! Reconciliation against a real repository and a real tmux server.
//!
//! Everything here runs the real `git` and `tmux` binaries: git in temp
//! repositories, tmux on a throwaway private socket in a temp directory whose
//! server is killed on drop, even on panic. The user's own tmux server is
//! never addressed — every invocation carries `-S <temp socket>`.
//!
//! When a binary is missing the test prints a skip notice and returns rather
//! than passing silently.

mod common;

use std::path::{Path, PathBuf};

use common::{add_worktree, canonical, have, init_repo, skip};
use grove_core::model::SessionPresence;
use grove_core::reconcile::{self, OrphanReason, ProjectRef};
use grove_core::tmux::{self, SessionSpec, TmuxServer};
use grove_core::{git, ids, workflow};

macro_rules! require {
    ($($program:literal),+) => {
        $(
            if !have($program) {
                skip($program);
                return;
            }
        )+
    };
}

/// A private tmux server in a temp directory, killed on drop even on panic.
struct TestServer {
    server: TmuxServer,
    _dir: tempfile::TempDir,
}

impl TestServer {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let server = TmuxServer::new(dir.path().join("run").join("tmux.sock"))
            .with_config(dir.path().join("config").join("tmux.conf"));
        server.ensure_socket_dir().expect("socket dir");
        Self { server, _dir: dir }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Err(e) = self.server.kill_server() {
            eprintln!("warning: could not kill the test tmux server: {e}");
        }
    }
}

/// A repository with a `feature/auth` linked worktree beside it.
struct Fixture {
    dir: tempfile::TempDir,
    project: ProjectRef,
    auth: PathBuf,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("proj");
    init_repo(&repo);
    let auth = dir.path().join("wt-auth");
    add_worktree(&repo, &auth, "feature/auth");

    let repo = canonical(&repo);
    let git_common_dir = git::git_common_dir(&repo).expect("git-common-dir");
    Fixture {
        project: ProjectRef {
            id: ids::project_id(&git_common_dir),
            name: "proj".to_string(),
            repository_path: repo,
            git_common_dir,
        },
        auth: canonical(&auth),
        dir,
    }
}

fn spec_for(fixture: &Fixture, path: &Path) -> SessionSpec {
    SessionSpec {
        worktree_id: ids::worktree_id(&fixture.project.git_common_dir, path),
        worktree_path: path.to_path_buf(),
        project_name: fixture.project.name.clone(),
        git_common_dir: fixture.project.git_common_dir.clone(),
    }
}

#[test]
fn reconciles_a_real_repository_with_no_sessions() {
    require!("git", "tmux");
    let fixture = fixture();
    let test = TestServer::new();

    let result = reconcile::reconcile_all(
        &test.server,
        std::slice::from_ref(&fixture.project),
        &[],
        &[],
    )
    .expect("reconciles");

    assert_eq!(result.projects.len(), 1);
    let project = &result.projects[0];
    assert_eq!(project.unavailable, None);
    assert_eq!(project.worktrees.len(), 2, "the main worktree and wt-auth");
    assert!(project.worktrees[0].is_main);
    assert!(
        project.worktrees.iter().all(|w| !w.session.exists()),
        "no server has been started, so nothing has a session"
    );
    assert_eq!(result.live_sessions(), 0);
    assert_eq!(result.stopped_sessions(), 0);
    assert!(result.orphans.is_empty());
    assert!(fixture.dir.path().is_dir(), "the fixture outlives the test");
}

/// The whole point of deterministic ids: with `state.toml` thrown away, a
/// fresh reconciliation re-derives the same ids and finds the live sessions.
#[test]
fn a_live_session_is_reattached_after_losing_all_state() {
    require!("git", "tmux");
    let fixture = fixture();
    let test = TestServer::new();
    let spec = spec_for(&fixture, &fixture.auth);
    tmux::ensure_session(&test.server, &spec).expect("creates the session");

    // No recorded sessions at all: this is the post-state-loss case.
    let result = reconcile::reconcile_all(
        &test.server,
        std::slice::from_ref(&fixture.project),
        &[],
        &[],
    )
    .expect("reconciles");

    let auth = result.projects[0]
        .worktrees
        .iter()
        .find(|w| w.path == fixture.auth)
        .expect("the linked worktree is listed");
    assert_eq!(auth.session, SessionPresence::Detached);
    assert_eq!(auth.id, spec.worktree_id);
    assert!(!auth.session_stopped);
    assert!(
        result.orphans.is_empty(),
        "a matched session is not orphaned"
    );
}

#[test]
fn a_session_that_has_gone_is_reported_as_stopped_not_recreated() {
    require!("git", "tmux");
    let fixture = fixture();
    let test = TestServer::new();
    let spec = spec_for(&fixture, &fixture.auth);
    tmux::ensure_session(&test.server, &spec).expect("creates");
    tmux::kill_session(&test.server, &spec.session_name()).expect("kills");

    let recorded = vec![spec.worktree_id.clone()];
    let result = reconcile::reconcile_all(
        &test.server,
        std::slice::from_ref(&fixture.project),
        &recorded,
        &[],
    )
    .expect("reconciles");

    let auth = result.projects[0]
        .worktrees
        .iter()
        .find(|w| w.path == fixture.auth)
        .expect("listed");
    assert!(auth.session_stopped);
    assert_eq!(auth.session, SessionPresence::None);
    assert_eq!(result.stopped_sessions(), 1);
    assert!(
        tmux::list_sessions(&test.server).expect("lists").is_empty(),
        "reconciliation must not have started anything"
    );
}

/// A worktree removed behind Grove's back leaves its session running. The
/// session becomes an orphan and stays exactly where it was.
#[test]
fn a_removed_worktree_leaves_an_orphaned_session_running() {
    require!("git", "tmux");
    let fixture = fixture();
    let test = TestServer::new();
    let spec = spec_for(&fixture, &fixture.auth);
    let name = spec.session_name();
    tmux::ensure_session(&test.server, &spec).expect("creates");

    git::worktree_remove(&fixture.project.repository_path, &fixture.auth, true)
        .expect("removes the worktree outside Grove");

    let result = reconcile::reconcile_all(
        &test.server,
        std::slice::from_ref(&fixture.project),
        &[],
        &[],
    )
    .expect("reconciles");

    assert_eq!(result.projects[0].worktrees.len(), 1, "only main is left");
    assert_eq!(result.orphans.len(), 1);
    let orphan = &result.orphans[0];
    assert_eq!(orphan.name, name);
    assert_eq!(
        orphan.worktree_id.as_deref(),
        Some(spec.worktree_id.as_str())
    );
    assert_eq!(
        orphan.worktree_path.as_deref(),
        Some(fixture.auth.as_path())
    );
    assert_eq!(
        orphan.reason,
        OrphanReason::WorktreeGone,
        "its repository is registered, so the worktree is what went"
    );
    assert_eq!(
        tmux::list_sessions(&test.server).expect("lists").len(),
        1,
        "the orphan is reported, never closed"
    );
}

/// Associating adopts the session under the worktree's name; the next
/// reconciliation matches it as an ordinary session.
#[test]
fn associating_an_orphan_adopts_it_without_recreating_anything() {
    require!("git", "tmux");
    let fixture = fixture();
    let test = TestServer::new();

    // A session created by hand, under a name that is not Grove's, in the
    // worktree's directory.
    test.server
        .run([
            "new-session",
            "-d",
            "-s",
            "hand made",
            "-c",
            &fixture.auth.to_string_lossy(),
        ])
        .expect("creates a session by hand");
    let pane_pid_before = tmux::list_panes(&test.server, "hand made").expect("panes")[0].pid;

    // It carries no `@grove_*` options, so Grove leaves it entirely alone.
    let before = reconcile::reconcile_all(
        &test.server,
        std::slice::from_ref(&fixture.project),
        &[],
        &[],
    )
    .expect("reconciles");
    assert!(
        before.orphans.is_empty(),
        "a session the user made is not Grove's business"
    );

    let worktree = before.projects[0]
        .worktrees
        .iter()
        .find(|w| w.path == fixture.auth)
        .expect("listed")
        .clone();
    let name = workflow::associate_session(
        &test.server,
        &fixture.project.name,
        &fixture.project.git_common_dir,
        &worktree,
        "hand made",
    )
    .expect("associates");
    assert_eq!(name, worktree.session_name());

    let after = reconcile::reconcile_all(
        &test.server,
        std::slice::from_ref(&fixture.project),
        &[],
        &[],
    )
    .expect("reconciles");
    let auth = after.projects[0]
        .worktrees
        .iter()
        .find(|w| w.path == fixture.auth)
        .expect("listed");
    assert_eq!(auth.session, SessionPresence::Detached);
    assert!(after.orphans.is_empty());
    assert_eq!(
        tmux::list_panes(&test.server, &name).expect("panes")[0].pid,
        pane_pid_before,
        "the same session, not a new one: the pane's process survived"
    );
}

#[test]
fn ignoring_an_orphan_silences_it_without_closing_it() {
    require!("git", "tmux");
    let fixture = fixture();
    let test = TestServer::new();
    let spec = spec_for(&fixture, &fixture.dir.path().join("never-existed"));
    let name = spec.session_name();
    test.server
        .run([
            "new-session",
            "-d",
            "-s",
            name.as_str(),
            "-c",
            &fixture.project.repository_path.to_string_lossy(),
        ])
        .expect("creates");
    tmux::session::set_session_metadata(&test.server, &name, &spec).expect("stamps");

    let listed = reconcile::reconcile_all(
        &test.server,
        std::slice::from_ref(&fixture.project),
        &[],
        &[],
    )
    .expect("reconciles");
    assert_eq!(listed.orphans.len(), 1);

    let ignored = vec![name.clone()];
    let silent = reconcile::reconcile_all(
        &test.server,
        std::slice::from_ref(&fixture.project),
        &[],
        &ignored,
    )
    .expect("reconciles");
    assert!(silent.orphans.is_empty());
    assert_eq!(silent.ignored, 1);
    assert!(
        tmux::has_session(&test.server, &name).expect("lists"),
        "ignoring must never close the session"
    );
}

/// A project whose directory has moved is *unavailable*, and reconciliation
/// keeps its record rather than dropping the project.
#[test]
fn a_moved_project_is_marked_unavailable_and_kept() {
    require!("git", "tmux");
    let fixture = fixture();
    let test = TestServer::new();
    let moved = ProjectRef {
        repository_path: fixture.dir.path().join("moved-away"),
        ..fixture.project.clone()
    };

    let result = reconcile::reconcile_all(&test.server, std::slice::from_ref(&moved), &[], &[])
        .expect("reconciles");

    assert_eq!(result.projects.len(), 1);
    assert_eq!(result.projects[0].id, moved.id);
    assert!(result.projects[0].unavailable.is_some());
    assert!(result.projects[0].worktrees.is_empty());
    assert_eq!(result.unavailable_projects(), 1);
    assert!(
        fixture.project.repository_path.is_dir(),
        "nothing on disk was touched"
    );
}

/// A worktree whose directory was deleted (without git being told) is
/// *unavailable*: marked, still listed, nothing pruned.
#[test]
fn a_deleted_worktree_directory_is_marked_unavailable() {
    require!("git", "tmux");
    let fixture = fixture();
    let test = TestServer::new();
    std::fs::remove_dir_all(&fixture.auth).expect("delete the worktree directory");

    let result = reconcile::reconcile_all(
        &test.server,
        std::slice::from_ref(&fixture.project),
        &[],
        &[],
    )
    .expect("reconciles");

    let auth = result.projects[0]
        .worktrees
        .iter()
        .find(|w| w.path == fixture.auth)
        .expect("git still lists it");
    assert!(auth.is_missing);
    assert_eq!(result.missing_worktrees(), 1);
    assert!(auth.sublabel().contains("unavailable"));
}
