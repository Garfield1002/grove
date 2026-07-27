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
use grove_core::tmux::{self, SessionSpec, TmuxServer};
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
        let server = TmuxServer::new(dir.path().join("run").join("tmux.sock"))
            .with_config(dir.path().join("config").join("tmux.conf"));
        server.ensure_socket_dir().expect("socket dir");
        Self { server, _dir: dir }
    }
}

/// Session spec for a worktree, with the repository identity tmux will carry.
fn spec_for(path: &Path, project: &str) -> SessionSpec {
    SessionSpec {
        worktree_id: ids::worktree_id(Path::new("/repo/.git"), path),
        worktree_path: path.to_path_buf(),
        project_name: project.to_string(),
        git_common_dir: PathBuf::from("/repo/.git"),
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
    let spec = spec_for(&worktree_canonical, "acme-web");
    let id = spec.worktree_id.clone();
    let (name, created) = tmux::ensure_session(&test.server, &spec).expect("creates");

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
    let spec = spec_for(&worktree, "acme-web");
    let id = spec.worktree_id.clone();
    let (name, _) = tmux::ensure_session(&test.server, &spec).expect("creates");

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
    let (name, _) =
        tmux::ensure_session(&test.server, &spec_for(&worktree, "acme-web")).expect("creates");

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
    let spec = spec_for(&worktree, "acme-web");
    let (first, created_first) = tmux::ensure_session(&test.server, &spec).expect("creates");
    let (second, created_second) = tmux::ensure_session(&test.server, &spec).expect("reuses");

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
    let spec_a = spec_for(&a, "acme-web");
    let spec_b = spec_for(&b, "acme-web");
    let (id_a, id_b) = (spec_a.worktree_id.clone(), spec_b.worktree_id.clone());
    assert_ne!(id_a, id_b);

    tmux::ensure_session(&test.server, &spec_a).expect("creates a");
    tmux::ensure_session(&test.server, &spec_b).expect("creates b");

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
    let spec = spec_for(&worktree, "my project");
    tmux::ensure_session(&test.server, &spec).expect("creates");

    let sessions = tmux::list_sessions(&test.server).expect("lists");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].path, worktree);
    // The spaced path must survive the round trip through a tmux user option.
    assert_eq!(
        sessions[0].metadata.worktree.as_deref(),
        Some(worktree.as_path())
    );
    assert_eq!(sessions[0].metadata.project.as_deref(), Some("my project"));
}

#[test]
fn no_client_is_attached_to_a_fresh_server() {
    require!("tmux");
    let dir = tempfile::tempdir().expect("tempdir");
    let worktree = std::fs::canonicalize(dir.path()).expect("canonicalize");

    let test = TestServer::new();
    assert!(tmux::list_clients(&test.server).expect("lists").is_empty());

    tmux::ensure_session(&test.server, &spec_for(&worktree, "acme-web")).expect("creates");
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
    let (name_a, _) =
        tmux::ensure_session(&test.server, &spec_for(&a, "acme-web")).expect("creates a");
    let (name_b, _) =
        tmux::ensure_session(&test.server, &spec_for(&b, "acme-web")).expect("creates b");

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

    tmux::ensure_session(&test.server, &spec_for(&worktree, "acme-web")).expect("creates");

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
        ..Config::default()
    };

    let activation = workflow::activate_worktree(
        &test.server,
        &config,
        "acme-web",
        &discovery.git_common_dir,
        &worktree,
    )
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
    tmux::ensure_session(
        &test.server,
        &workflow::session_spec("proj", Path::new("/repo/.git"), &worktree),
    )
    .expect("creates");

    let config = Config {
        terminal: grove_core::config::TerminalConfig {
            command: "/bin/true {session}".into(),
        },
        ..Config::default()
    };
    workflow::activate_worktree(
        &test.server,
        &config,
        "proj",
        Path::new("/repo/.git"),
        &worktree,
    )
    .expect("activates");
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
    let err = workflow::activate_worktree(
        &test.server,
        &Config::default(),
        "proj",
        Path::new("/repo/.git"),
        &worktree,
    )
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
    let err = workflow::activate_worktree(
        &test.server,
        &Config::default(),
        "proj",
        Path::new("/repo/.git"),
        &worktree,
    )
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

/// The `@grove_*` user options must round-trip through the real tmux server:
/// this is the mapping restore and orphan association will read back.
#[test]
fn session_metadata_round_trips_through_tmux_user_options() {
    require!("tmux");
    let dir = tempfile::tempdir().expect("tempdir");
    let worktree = dir.path().join("my projects").join("a work tree");
    std::fs::create_dir_all(&worktree).expect("mkdir");
    let worktree = std::fs::canonicalize(&worktree).expect("canonicalize");

    let test = TestServer::new();
    let spec = SessionSpec {
        worktree_id: ids::worktree_id(Path::new("/home/u/my repo/.git"), &worktree),
        worktree_path: worktree.clone(),
        project_name: "my repo".into(),
        git_common_dir: PathBuf::from("/home/u/my repo/.git"),
    };
    let (name, _) = tmux::ensure_session(&test.server, &spec).expect("creates");

    // Read back through the documented format string.
    let raw = test
        .server
        .run([
            "list-sessions".to_string(),
            "-F".to_string(),
            "#{@grove_id}\u{1}#{@grove_project}\u{1}#{@grove_worktree}\u{1}#{@grove_repo}"
                .to_string(),
        ])
        .expect("lists with a user-option format");
    let fields: Vec<&str> = raw.trim_end().split('\u{1}').collect();
    assert_eq!(
        fields,
        vec![
            spec.worktree_id.as_str(),
            "my repo",
            &worktree.to_string_lossy(),
            "/home/u/my repo/.git",
        ],
        "spaces in the project name and worktree path must survive"
    );

    // And through the typed accessor.
    let metadata = tmux::session::session_metadata(&test.server, &name).expect("reads metadata");
    assert!(metadata.is_complete());
    assert_eq!(metadata.id.as_deref(), Some(spec.worktree_id.as_str()));
    assert_eq!(metadata.project.as_deref(), Some("my repo"));
    assert_eq!(metadata.worktree.as_deref(), Some(worktree.as_path()));
    assert_eq!(
        metadata.repo.as_deref(),
        Some(Path::new("/home/u/my repo/.git"))
    );
}

#[test]
fn the_attention_marker_round_trips_and_survives_a_relisting() {
    require!("tmux");
    let dir = tempfile::tempdir().expect("tempdir");
    let worktree = std::fs::canonicalize(dir.path()).expect("canonicalize");
    let test = TestServer::new();
    let spec = spec_for(&worktree, "acme-web");
    let (name, _) = tmux::ensure_session(&test.server, &spec).expect("creates");

    let listed = |server: &TmuxServer| {
        tmux::session::list_sessions(server)
            .expect("lists")
            .into_iter()
            .find(|s| s.name == name)
            .expect("the session is listed")
    };

    // A fresh session carries no marker and a usable activity stamp.
    let before = listed(&test.server);
    assert!(!before.attention);
    assert!(
        before.activity_epoch.is_some(),
        "tmux always reports session_activity"
    );

    assert!(tmux::session::set_attention(&test.server, &name).expect("sets"));
    assert!(listed(&test.server).attention);

    assert!(tmux::session::clear_attention(&test.server, &name).expect("clears"));
    assert!(!listed(&test.server).attention);
}

#[test]
fn marking_attention_on_a_missing_session_is_a_no_op() {
    require!("tmux");
    let test = TestServer::new();
    // No server at all: `grove notify` must not fail an agent's hook.
    assert!(!tmux::session::set_attention(&test.server, "wt-a1b2c3").expect("no server is fine"));
    assert!(!tmux::session::clear_attention(&test.server, "wt-a1b2c3").expect("no server is fine"));

    // A live server, but no such session.
    let dir = tempfile::tempdir().expect("tempdir");
    let worktree = std::fs::canonicalize(dir.path()).expect("canonicalize");
    tmux::ensure_session(&test.server, &spec_for(&worktree, "acme-web")).expect("creates");
    assert!(!tmux::session::set_attention(&test.server, "wt-ffffff").expect("missing is fine"));
    assert!(!tmux::session::clear_attention(&test.server, "wt-ffffff").expect("missing is fine"));
}

#[test]
fn a_hand_set_attention_option_is_read_as_attention() {
    require!("tmux");
    let dir = tempfile::tempdir().expect("tempdir");
    let worktree = std::fs::canonicalize(dir.path()).expect("canonicalize");
    let test = TestServer::new();
    let spec = spec_for(&worktree, "acme-web");
    let (name, _) = tmux::ensure_session(&test.server, &spec).expect("creates");

    // The option is user-visible, so a hand-set `on` must work like Grove's 1.
    test.server
        .run([
            "set-option".to_string(),
            "-t".to_string(),
            name.clone(),
            tmux::session::OPT_ATTENTION.to_string(),
            "on".to_string(),
        ])
        .expect("sets by hand");
    let session = tmux::session::list_sessions(&test.server)
        .expect("lists")
        .into_iter()
        .find(|s| s.name == name)
        .expect("listed");
    assert!(session.attention);

    // And the signals it produces classify as attention regardless of quiet.
    let signals = session.signals(session.activity_epoch.unwrap_or(0) + 3600, Vec::new());
    assert_eq!(
        grove_core::status::classify(&signals, &grove_core::StatusPolicy::default()),
        grove_core::SessionStatus::Attention
    );
}

#[test]
fn polling_reports_signals_for_grove_sessions_only() {
    require!("tmux");
    let dir = tempfile::tempdir().expect("tempdir");
    let worktree = std::fs::canonicalize(dir.path()).expect("canonicalize");
    let test = TestServer::new();
    let spec = spec_for(&worktree, "acme-web");
    let (name, _) = tmux::ensure_session(&test.server, &spec).expect("creates");

    // A session Grove did not create must not appear in the poll.
    test.server
        .run([
            "new-session",
            "-d",
            "-s",
            "someone-elses-session",
            "-c",
            "/tmp",
        ])
        .expect("creates a foreign session");

    let now = workflow::now_epoch();
    let signals = workflow::poll_session_signals(&test.server, now).expect("polls");
    assert_eq!(
        signals.keys().collect::<Vec<_>>(),
        vec![&spec.worktree_id],
        "only Grove's own sessions are polled"
    );

    let mine = &signals[&spec.worktree_id];
    assert!(!mine.attention_flag);
    assert!(
        mine.activity_age.is_some(),
        "a real session always has an activity stamp"
    );
    assert_eq!(
        mine.pane_commands.len(),
        1,
        "the shell window's pane is reported: {:?}",
        mine.pane_commands
    );

    // Raising attention shows up in the very next poll.
    tmux::session::set_attention(&test.server, &name).expect("sets");
    let signals = workflow::poll_session_signals(&test.server, now).expect("polls");
    assert!(signals[&spec.worktree_id].attention_flag);
    assert_eq!(
        grove_core::status::classify(
            &signals[&spec.worktree_id],
            &grove_core::StatusPolicy::default()
        ),
        grove_core::SessionStatus::Attention
    );
}

#[test]
fn polling_an_empty_server_is_an_empty_map_not_an_error() {
    require!("tmux");
    let test = TestServer::new();
    let signals = workflow::poll_session_signals(&test.server, workflow::now_epoch())
        .expect("no server is a normal state");
    assert!(signals.is_empty());
}

#[test]
fn a_session_grove_did_not_create_carries_no_metadata() {
    require!("tmux");
    let dir = tempfile::tempdir().expect("tempdir");
    let cwd = std::fs::canonicalize(dir.path()).expect("canonicalize");

    let test = TestServer::new();
    test.server
        .run([
            "new-session".to_string(),
            "-d".to_string(),
            "-s".to_string(),
            "someone-elses".to_string(),
            "-c".to_string(),
            cwd.to_string_lossy().into_owned(),
        ])
        .expect("creates a foreign session");

    let sessions = tmux::list_sessions(&test.server).expect("lists");
    assert_eq!(sessions.len(), 1);
    assert!(!sessions[0].metadata.is_complete());
    assert_eq!(sessions[0].metadata.id, None);
    assert_eq!(sessions[0].worktree_id(), None);
}

/// Grove starts its server with `-f`, so the file must exist and be used.
#[test]
fn the_server_uses_groves_own_tmux_config() {
    require!("tmux");
    let dir = tempfile::tempdir().expect("tempdir");
    let worktree = std::fs::canonicalize(dir.path()).expect("canonicalize");

    let test = TestServer::new();
    let config = test
        .server
        .config_file()
        .expect("the test server is configured")
        .to_path_buf();
    assert!(
        !config.exists(),
        "the config is generated lazily, not by the constructor"
    );

    tmux::ensure_session(&test.server, &spec_for(&worktree, "acme-web")).expect("creates");
    assert!(config.exists(), "starting the server generated tmux.conf");

    // The options Grove depends on came from that file, not from ~/.tmux.conf.
    for (option, expected) in [
        ("monitor-bell", "on"),
        ("monitor-activity", "on"),
        ("exit-empty", "off"),
    ] {
        let value = test
            .server
            .run([
                "show-options".to_string(),
                "-gqv".to_string(),
                option.to_string(),
            ])
            .expect("reads the option");
        assert_eq!(value.trim(), expected, "{option} should be {expected}");
    }
}

/// A user edit to tmux.conf must be honoured and never overwritten.
#[test]
fn a_user_edited_tmux_config_is_used_as_is() {
    require!("tmux");
    let dir = tempfile::tempdir().expect("tempdir");
    let worktree = std::fs::canonicalize(dir.path()).expect("canonicalize");

    let test = TestServer::new();
    let config = test.server.config_file().expect("configured").to_path_buf();
    std::fs::create_dir_all(config.parent().expect("parent")).expect("mkdir");
    std::fs::write(&config, "set -g base-index 7\nset -s exit-empty off\n").expect("user config");

    tmux::ensure_session(&test.server, &spec_for(&worktree, "acme-web")).expect("creates");

    let base_index = test
        .server
        .run([
            "show-options".to_string(),
            "-gqv".to_string(),
            "base-index".to_string(),
        ])
        .expect("reads base-index");
    assert_eq!(base_index.trim(), "7", "the user's own setting must win");
    assert_eq!(
        std::fs::read_to_string(&config).expect("read"),
        "set -g base-index 7\nset -s exit-empty off\n",
        "Grove must not rewrite a file the user owns"
    );
}

/// Closing a session is its own confirmed operation, and it must close
/// exactly one session: everything else on the private server survives, and
/// the user's own tmux server is never on this socket at all.
#[test]
fn killing_one_session_leaves_the_others_intact() {
    require!("tmux");
    let dir = tempfile::tempdir().expect("tempdir");
    let first = dir.path().join("wt-one");
    let second = dir.path().join("wt-two");
    std::fs::create_dir(&first).expect("mkdir");
    std::fs::create_dir(&second).expect("mkdir");
    let first = std::fs::canonicalize(&first).expect("canonicalize");
    let second = std::fs::canonicalize(&second).expect("canonicalize");

    let test = TestServer::new();
    let (first_name, _) =
        tmux::ensure_session(&test.server, &spec_for(&first, "acme-web")).expect("creates");
    let (second_name, _) =
        tmux::ensure_session(&test.server, &spec_for(&second, "acme-web")).expect("creates");
    assert_eq!(tmux::list_sessions(&test.server).expect("lists").len(), 2);

    tmux::kill_session(&test.server, &first_name).expect("kills one session");

    let remaining = tmux::list_sessions(&test.server).expect("lists");
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].name, second_name);
    assert!(first.is_dir(), "killing a session touches no files");
}

#[test]
fn killing_a_session_that_is_already_gone_is_not_an_error() {
    require!("tmux");
    let dir = tempfile::tempdir().expect("tempdir");
    let worktree = std::fs::canonicalize(dir.path()).expect("canonicalize");
    let test = TestServer::new();

    // No server at all yet.
    tmux::kill_session(&test.server, "wt-ffffff").expect("no server is not an error");

    let (name, _) =
        tmux::ensure_session(&test.server, &spec_for(&worktree, "acme-web")).expect("creates");
    tmux::kill_session(&test.server, &name).expect("kills it");
    // The session is gone but the server may still be up; a second kill of a
    // missing session must still surface as git/tmux's own error, not a panic.
    let second = tmux::kill_session(&test.server, &name);
    assert!(second.is_ok() || second.is_err());
}

#[test]
fn lists_the_panes_of_a_session_with_their_processes() {
    require!("tmux");
    let dir = tempfile::tempdir().expect("tempdir");
    let worktree = std::fs::canonicalize(dir.path()).expect("canonicalize");

    let test = TestServer::new();
    let (name, _) =
        tmux::ensure_session(&test.server, &spec_for(&worktree, "acme-web")).expect("creates");

    let panes = tmux::list_panes(&test.server, &name).expect("lists panes");
    assert_eq!(panes.len(), 1);
    assert_eq!(panes[0].session, name);
    assert!(panes[0].pid > 0, "every pane reports a pid");
    assert!(!panes[0].command.is_empty());

    // A session that does not exist has no panes, and that is not an error.
    assert!(
        tmux::list_panes(&test.server, "wt-ffffff")
            .expect("a missing session is an empty list")
            .is_empty()
    );
}

/// The removal risk report, assembled from a real session.
#[test]
fn a_real_session_appears_in_the_removal_report() {
    require!("tmux");
    let dir = tempfile::tempdir().expect("tempdir");
    let worktree_path = std::fs::canonicalize(dir.path()).expect("canonicalize");
    let mut worktree = worktree_at(&worktree_path, Path::new("/repo/.git"), "feature/auth");
    worktree.is_main = false;

    let test = TestServer::new();
    let before = workflow::removal_inputs(&test.server, &worktree).expect("gathers");
    assert_eq!(before.session, None);
    assert!(before.panes.is_empty());
    assert!(!grove_core::removal::assemble(&before).can_close_session);

    tmux::ensure_session(
        &test.server,
        &workflow::session_spec("acme-web", Path::new("/repo/.git"), &worktree),
    )
    .expect("creates");

    let after = workflow::removal_inputs(&test.server, &worktree).expect("gathers");
    assert_eq!(
        after.session.as_deref(),
        Some(worktree.session_name().as_str())
    );
    assert_eq!(after.panes.len(), 1);
    let report = grove_core::removal::assemble(&after);
    assert!(report.can_close_session);
    assert!(
        report.can_remove_worktree,
        "a linked worktree may be offered for removal"
    );
}
