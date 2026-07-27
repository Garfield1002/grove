//! Real tmux integration against a throwaway private socket.
//!
//! Every test owns a [`TestServer`] guard whose `Drop` kills the server, so a
//! panicking test still cannot leave a tmux server (or the user's own server,
//! which is never touched) behind.

mod common;

use std::path::{Path, PathBuf};

use common::{have, init_repo, skip};
use grove_core::config::Config;
use grove_core::ids;
use grove_core::model::{SessionPresence, Worktree};
use grove_core::tmux::{self, TmuxServer};
use grove_core::workflow::{self, Activation};

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
        let server = TmuxServer::new(dir.path().join("run").join("tmux.sock"));
        server.ensure_socket_dir().expect("socket dir");
        Self { server, _dir: dir }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        // Must not panic during unwinding; a best-effort kill is enough.
        if let Err(e) = self.server.kill_server() {
            eprintln!("warning: could not kill the test tmux server: {e}");
        }
    }
}

fn worktree_at(path: &Path, git_common_dir: &Path, branch: &str) -> Worktree {
    use grove_core::git::WorktreeEntry;
    Worktree::from_entry(
        &WorktreeEntry {
            path: path.to_path_buf(),
            head: Some("0f2c8a1b3d4e5f60718293a4b5c6d7e8f9012345".into()),
            branch: Some(branch.to_string()),
            ..WorktreeEntry::default()
        },
        "p1",
        git_common_dir,
        true,
    )
}

#[test]
fn no_server_yet_is_an_empty_listing_not_an_error() {
    require!("tmux");
    let test = TestServer::new();
    assert!(
        tmux::list_sessions(&test.server)
            .expect("an absent socket is not an error")
            .is_empty()
    );
}

#[test]
fn creates_a_detached_session_rooted_in_the_worktree() {
    require!("tmux");
    let dir = tempfile::tempdir().expect("tempdir");
    let worktree = dir.path().join("wt-auth");
    std::fs::create_dir(&worktree).expect("mkdir");
    let worktree_canonical = std::fs::canonicalize(&worktree).expect("canonicalize");

    let test = TestServer::new();
    let id = ids::worktree_id(Path::new("/repo/.git"), &worktree_canonical);
    let (name, created) =
        tmux::ensure_session(&test.server, &id, &worktree_canonical).expect("creates");

    assert!(created);
    assert_eq!(name, format!("wt-{id}"));

    let sessions = tmux::list_sessions(&test.server).expect("lists");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].name, name);
    assert_eq!(
        sessions[0].path, worktree_canonical,
        "rooted in the worktree"
    );
    assert_eq!(sessions[0].attached, 0, "created detached");
    assert_eq!(sessions[0].worktree_id(), Some(id.as_str()));
}

#[test]
fn the_session_exports_grove_session() {
    require!("tmux");
    let dir = tempfile::tempdir().expect("tempdir");
    let worktree = std::fs::canonicalize(dir.path()).expect("canonicalize");

    let test = TestServer::new();
    let id = ids::worktree_id(Path::new("/repo/.git"), &worktree);
    let (name, _) = tmux::ensure_session(&test.server, &id, &worktree).expect("creates");

    let value = tmux::session::session_env(&test.server, &name, "GROVE_SESSION")
        .expect("reads the session environment");
    assert_eq!(value.as_deref(), Some(id.as_str()));
}

#[test]
fn window_zero_is_named_shell() {
    require!("tmux");
    let dir = tempfile::tempdir().expect("tempdir");
    let worktree = std::fs::canonicalize(dir.path()).expect("canonicalize");

    let test = TestServer::new();
    let id = ids::worktree_id(Path::new("/repo/.git"), &worktree);
    let (name, _) = tmux::ensure_session(&test.server, &id, &worktree).expect("creates");

    let windows = test
        .server
        .run([
            "list-windows".to_string(),
            "-t".to_string(),
            name,
            "-F".to_string(),
            "#{window_index}:#{window_name}".to_string(),
        ])
        .expect("lists windows");
    assert_eq!(windows.trim(), "0:shell");
}

#[test]
fn ensure_session_is_idempotent() {
    require!("tmux");
    let dir = tempfile::tempdir().expect("tempdir");
    let worktree = std::fs::canonicalize(dir.path()).expect("canonicalize");

    let test = TestServer::new();
    let id = ids::worktree_id(Path::new("/repo/.git"), &worktree);
    let (first, created_first) =
        tmux::ensure_session(&test.server, &id, &worktree).expect("creates");
    let (second, created_second) =
        tmux::ensure_session(&test.server, &id, &worktree).expect("reuses");

    assert!(created_first);
    assert!(!created_second, "the second call must reuse the session");
    assert_eq!(first, second);
    assert_eq!(tmux::list_sessions(&test.server).expect("lists").len(), 1);
}

#[test]
fn distinct_worktrees_get_distinct_sessions() {
    require!("tmux");
    let dir = tempfile::tempdir().expect("tempdir");
    let a = dir.path().join("wt-a");
    let b = dir.path().join("wt-b");
    std::fs::create_dir(&a).expect("mkdir");
    std::fs::create_dir(&b).expect("mkdir");
    let (a, b) = (
        std::fs::canonicalize(&a).expect("canonicalize"),
        std::fs::canonicalize(&b).expect("canonicalize"),
    );

    let test = TestServer::new();
    let common = Path::new("/repo/.git");
    let id_a = ids::worktree_id(common, &a);
    let id_b = ids::worktree_id(common, &b);
    assert_ne!(id_a, id_b);

    tmux::ensure_session(&test.server, &id_a, &a).expect("creates a");
    tmux::ensure_session(&test.server, &id_b, &b).expect("creates b");

    let mut names: Vec<_> = tmux::list_sessions(&test.server)
        .expect("lists")
        .into_iter()
        .map(|s| s.name)
        .collect();
    names.sort();
    let mut expected = vec![ids::session_name(&id_a), ids::session_name(&id_b)];
    expected.sort();
    assert_eq!(names, expected);
}

#[test]
fn sessions_survive_in_worktrees_with_spaces_in_their_path() {
    require!("tmux");
    let dir = tempfile::tempdir().expect("tempdir");
    let worktree = dir.path().join("my projects").join("a work tree");
    std::fs::create_dir_all(&worktree).expect("mkdir");
    let worktree = std::fs::canonicalize(&worktree).expect("canonicalize");

    let test = TestServer::new();
    let id = ids::worktree_id(Path::new("/repo/.git"), &worktree);
    tmux::ensure_session(&test.server, &id, &worktree).expect("creates");

    let sessions = tmux::list_sessions(&test.server).expect("lists");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].path, worktree);
}

#[test]
fn no_client_is_attached_to_a_fresh_server() {
    require!("tmux");
    let dir = tempfile::tempdir().expect("tempdir");
    let worktree = std::fs::canonicalize(dir.path()).expect("canonicalize");

    let test = TestServer::new();
    assert!(tmux::list_clients(&test.server).expect("lists").is_empty());

    let id = ids::worktree_id(Path::new("/repo/.git"), &worktree);
    tmux::ensure_session(&test.server, &id, &worktree).expect("creates");
    let clients = tmux::list_clients(&test.server).expect("lists");
    assert!(
        clients.is_empty(),
        "a detached session has no client: {clients:?}"
    );
    assert!(tmux::primary_client(&clients).is_none());
}

#[test]
fn killing_a_session_leaves_the_others_alone() {
    require!("tmux");
    let dir = tempfile::tempdir().expect("tempdir");
    let a = dir.path().join("a");
    let b = dir.path().join("b");
    std::fs::create_dir(&a).expect("mkdir");
    std::fs::create_dir(&b).expect("mkdir");
    let (a, b) = (
        std::fs::canonicalize(&a).expect("canonicalize"),
        std::fs::canonicalize(&b).expect("canonicalize"),
    );

    let test = TestServer::new();
    let common = Path::new("/repo/.git");
    let (name_a, _) =
        tmux::ensure_session(&test.server, &ids::worktree_id(common, &a), &a).expect("creates a");
    let (name_b, _) =
        tmux::ensure_session(&test.server, &ids::worktree_id(common, &b), &b).expect("creates b");

    tmux::session::kill_session(&test.server, &name_a).expect("kills a");
    let names: Vec<_> = tmux::list_sessions(&test.server)
        .expect("lists")
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(names, vec![name_b]);
}

/// Grove's socket is private: sessions created here must be invisible to a
/// second socket, which stands in for the user's default server.
#[test]
fn sessions_are_invisible_on_another_socket() {
    require!("tmux");
    let dir = tempfile::tempdir().expect("tempdir");
    let worktree = std::fs::canonicalize(dir.path()).expect("canonicalize");

    let test = TestServer::new();
    let other = TestServer::new();
    assert_ne!(test.server.socket(), other.server.socket());

    let id = ids::worktree_id(Path::new("/repo/.git"), &worktree);
    tmux::ensure_session(&test.server, &id, &worktree).expect("creates");

    assert_eq!(tmux::list_sessions(&test.server).expect("lists").len(), 1);
    assert!(
        tmux::list_sessions(&other.server)
            .expect("lists")
            .is_empty(),
        "the other server must not see Grove's sessions"
    );
}

/// The full Milestone 1 click path against real git and real tmux, with no
/// client attached: it must create the session and launch the "terminal".
#[test]
fn activating_a_worktree_creates_the_session_and_launches_the_terminal() {
    require!("git", "tmux");
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("acme-web");
    init_repo(&repo);
    let discovery = grove_core::git::discover_project(&repo).expect("discovers");
    let project_id = ids::project_id(&discovery.git_common_dir);
    let worktrees = grove_core::model::worktrees_from_entries(
        &discovery.worktrees,
        &project_id,
        &discovery.git_common_dir,
    );
    let worktree = worktrees[0].clone();

    // A "terminal" that records the arguments it was launched with, so the
    // test asserts on the real spawn path rather than a mock.
    let marker = dir.path().join("launched.txt");
    let launcher = dir.path().join("fake-terminal.sh");
    std::fs::write(
        &launcher,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\n",
            shell_words::quote(&marker.to_string_lossy()),
        ),
    )
    .expect("write launcher");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    let test = TestServer::new();
    let config = Config {
        terminal: grove_core::config::TerminalConfig {
            command: format!(
                "{} attach {{session}} {{worktree}} {{project}} {{branch}}",
                shell_words::quote(&launcher.to_string_lossy())
            ),
        },
    };

    let activation = workflow::activate_worktree(&test.server, &config, "acme-web", &worktree)
        .expect("activates");
    let session = match &activation {
        Activation::LaunchedTerminal { session, .. } => session.clone(),
        other => panic!("expected a terminal launch with no client attached, got {other:?}"),
    };
    assert_eq!(session, worktree.session_name());

    // The session exists and is rooted in the worktree.
    let sessions = tmux::list_sessions(&test.server).expect("lists");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].name, session);
    assert_eq!(sessions[0].path, worktree.path);

    // The terminal really was spawned, with each value as one argv entry.
    let launched = wait_for_file(&marker);
    let args: Vec<&str> = launched.lines().collect();
    let worktree_path = worktree.path.to_string_lossy().into_owned();
    assert_eq!(
        args,
        vec![
            "attach",
            session.as_str(),
            worktree_path.as_str(),
            "acme-web",
            "main",
        ]
    );

    // Presence is now reported for the row.
    let presence = workflow::session_presence(&test.server).expect("presence");
    assert_eq!(
        presence.get(&session).copied(),
        Some(SessionPresence::Detached)
    );
}

#[test]
fn activation_reuses_an_existing_session() {
    require!("tmux");
    let dir = tempfile::tempdir().expect("tempdir");
    let worktree_path = std::fs::canonicalize(dir.path()).expect("canonicalize");
    let worktree = worktree_at(&worktree_path, Path::new("/repo/.git"), "main");

    let test = TestServer::new();
    tmux::ensure_session(&test.server, &worktree.id, &worktree.path).expect("creates");

    let config = Config {
        terminal: grove_core::config::TerminalConfig {
            command: "/bin/true {session}".into(),
        },
    };
    workflow::activate_worktree(&test.server, &config, "proj", &worktree).expect("activates");
    assert_eq!(
        tmux::list_sessions(&test.server).expect("lists").len(),
        1,
        "no duplicate session"
    );
}

#[test]
fn activation_refuses_a_worktree_that_has_disappeared() {
    require!("tmux");
    let dir = tempfile::tempdir().expect("tempdir");
    let gone = dir.path().join("gone");
    let worktree = worktree_at(&gone, Path::new("/repo/.git"), "main");

    let test = TestServer::new();
    let err = workflow::activate_worktree(&test.server, &Config::default(), "proj", &worktree)
        .expect_err("worktree is missing");
    assert!(err.to_string().contains("no longer exists"));
    assert!(
        tmux::list_sessions(&test.server).expect("lists").is_empty(),
        "no session may be created for a missing worktree"
    );
}

#[test]
fn activation_without_a_terminal_template_reports_it_after_creating_the_session() {
    require!("tmux");
    let dir = tempfile::tempdir().expect("tempdir");
    let worktree_path = std::fs::canonicalize(dir.path()).expect("canonicalize");
    let worktree = worktree_at(&worktree_path, Path::new("/repo/.git"), "main");

    let test = TestServer::new();
    let err = workflow::activate_worktree(&test.server, &Config::default(), "proj", &worktree)
        .expect_err("no terminal configured");
    assert!(err.to_string().contains("terminal command template"));
    // The session is still there: the user can retry after fixing config.toml.
    assert_eq!(tmux::list_sessions(&test.server).expect("lists").len(), 1);
}

/// Poll for the launcher's output: the terminal is spawned detached, so the
/// write races with the assertion.
fn wait_for_file(path: &PathBuf) -> String {
    for _ in 0..200 {
        if let Ok(text) = std::fs::read_to_string(path)
            && !text.is_empty()
        {
            return text;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    panic!(
        "the terminal was never launched: {} is empty",
        path.display()
    );
}
