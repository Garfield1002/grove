//! End-to-end tests of the `grove notify` binary.
//!
//! These run the real executable with `XDG_RUNTIME_DIR` pointed at a temp
//! directory, so they never touch the user's own Grove socket or tmux server.

use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::{Command, Output};
use std::time::Duration;

use grove_core::ipc::{self, Notification};
use grove_core::status::SessionStatus;

/// The binary under test, built by cargo for this integration test.
const GROVE: &str = env!("CARGO_BIN_EXE_grove");

/// Run `grove notify` with an isolated runtime, config and state directory.
fn notify(runtime: &Path, args: &[&str]) -> Output {
    Command::new(GROVE)
        .arg("notify")
        .args(args)
        .env("XDG_RUNTIME_DIR", runtime)
        .env("XDG_CONFIG_HOME", runtime.join("config"))
        .env("XDG_STATE_HOME", runtime.join("state"))
        // Must not leak in from the terminal this test runs in: several tests
        // depend on there being no ambient session.
        .env_remove("GROVE_SESSION")
        .output()
        .expect("runs the grove binary")
}

fn runtime_dir(dir: &Path) -> std::path::PathBuf {
    let runtime = dir.join("run");
    std::fs::create_dir_all(runtime.join("grove")).expect("mkdir");
    runtime
}

#[test]
fn a_notification_reaches_a_listening_socket() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime = runtime_dir(dir.path());
    let socket = ipc::socket_path(&runtime.join("grove"));
    let listener = UnixListener::bind(&socket).expect("bind");

    let reader = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).expect("read");
        line
    });

    let output = notify(
        &runtime,
        &[
            "--state",
            "attention",
            "--session",
            "a1b2c3",
            "--message",
            "needs permission",
        ],
    );
    assert!(output.status.success(), "stderr: {:?}", output.stderr);

    let line = reader.join().expect("reader thread");
    let notification = Notification::decode(&line).expect("a well-formed notification");
    assert_eq!(notification.worktree_id, "a1b2c3");
    assert_eq!(notification.state, SessionStatus::Attention);
    assert_eq!(notification.message.as_deref(), Some("needs permission"));
}

#[test]
fn the_session_comes_from_the_environment_when_not_given() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime = runtime_dir(dir.path());
    let socket = ipc::socket_path(&runtime.join("grove"));
    let listener = UnixListener::bind(&socket).expect("bind");

    let reader = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).expect("read");
        line
    });

    let output = Command::new(GROVE)
        .args(["notify", "--state", "working"])
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("XDG_CONFIG_HOME", runtime.join("config"))
        .env("XDG_STATE_HOME", runtime.join("state"))
        .env("GROVE_SESSION", "ddeeff")
        .output()
        .expect("runs");
    assert!(output.status.success());

    let notification = Notification::decode(&reader.join().expect("reader")).expect("valid");
    assert_eq!(notification.worktree_id, "ddeeff");
    assert_eq!(notification.state, SessionStatus::Working);
}

#[test]
fn no_listener_and_no_tmux_server_is_still_a_success() {
    // The whole point: `grove notify` runs inside an agent's hook and must
    // never fail it just because the GUI is closed.
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime = runtime_dir(dir.path());

    let output = notify(&runtime, &["--state", "attention", "--session", "a1b2c3"]);
    assert!(
        output.status.success(),
        "exited {:?}, stderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stdout.is_empty() && output.stderr.is_empty(),
        "a hook's output must stay quiet"
    );
}

#[test]
fn a_stale_socket_file_does_not_fail_the_hook() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime = runtime_dir(dir.path());
    // What a crashed GUI leaves behind.
    std::fs::write(ipc::socket_path(&runtime.join("grove")), b"stale").expect("write");

    let output = notify(&runtime, &["--state", "idle", "--session", "a1b2c3"]);
    assert!(output.status.success());
}

#[test]
fn a_usage_error_exits_two_and_says_why() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime = runtime_dir(dir.path());

    let bad_state = notify(&runtime, &["--state", "busy", "--session", "a1b2c3"]);
    assert_eq!(bad_state.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&bad_state.stderr);
    assert!(stderr.contains("is not a state"), "stderr: {stderr}");

    // No --session and no GROVE_SESSION.
    let no_session = notify(&runtime, &["--state", "idle"]);
    assert_eq!(no_session.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&no_session.stderr).contains("no session"));

    let bad_session = notify(&runtime, &["--state", "idle", "--session", "wt-a1b2c3"]);
    assert_eq!(bad_session.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&bad_session.stderr).contains("not a worktree id"));
}

#[test]
fn help_is_available_and_sends_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime = runtime_dir(dir.path());
    let socket = ipc::socket_path(&runtime.join("grove"));
    let listener = UnixListener::bind(&socket).expect("bind");
    listener
        .set_nonblocking(true)
        .expect("a non-blocking accept");

    let output = notify(&runtime, &["--help"]);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("grove notify"));

    // Give a hypothetical connection time to land before asserting none did.
    std::thread::sleep(Duration::from_millis(50));
    assert!(
        listener.accept().is_err(),
        "--help must not send a notification"
    );
}

/// Run `grove notify --hook` with a payload on stdin, as Claude Code does.
fn hook(runtime: &Path, session: Option<&str>, payload: &str) -> Output {
    use std::io::Write;
    let mut command = Command::new(GROVE);
    command
        .args(["notify", "--hook"])
        .env("XDG_RUNTIME_DIR", runtime)
        .env("XDG_CONFIG_HOME", runtime.join("config"))
        .env("XDG_STATE_HOME", runtime.join("state"))
        .env_remove("GROVE_SESSION")
        .env_remove("TMUX_PANE")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(session) = session {
        command.env("GROVE_SESSION", session);
    }
    let mut child = command.spawn().expect("runs the grove binary");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(payload.as_bytes())
        .expect("writes the payload");
    child.wait_with_output().expect("waits")
}

fn listen(socket: &Path) -> std::thread::JoinHandle<String> {
    let listener = UnixListener::bind(socket).expect("bind");
    std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).expect("read");
        line
    })
}

/// The whole Claude Code path in one go: the event on stdin becomes a report
/// on the socket, with the message, the conversation id and the transcript
/// that only the payload knew.
#[test]
fn a_claude_code_hook_payload_becomes_a_notification() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime = runtime_dir(dir.path());
    let reader = listen(&ipc::socket_path(&runtime.join("grove")));

    let output = hook(
        &runtime,
        Some("a1b2c3"),
        r#"{"hook_event_name": "Notification",
            "message": "Claude needs your permission to run cargo test",
            "session_id": "0f3a-91bc",
            "transcript_path": "/home/u/.claude/projects/x/0f3a.jsonl",
            "cwd": "/home/u/wt/auth"}"#,
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let notification = Notification::decode(&reader.join().expect("reader")).expect("valid");
    assert_eq!(notification.worktree_id, "a1b2c3");
    assert_eq!(notification.state, SessionStatus::Attention);
    assert_eq!(
        notification.message.as_deref(),
        Some("Claude needs your permission to run cargo test")
    );
    assert_eq!(notification.agent_session.as_deref(), Some("0f3a-91bc"));
    assert_eq!(
        notification.transcript.as_deref(),
        Some(Path::new("/home/u/.claude/projects/x/0f3a.jsonl"))
    );
}

/// A hook runs inside someone's agent. Every way of having nothing to say —
/// an event this Grove has never heard of, a payload that is not JSON, a
/// session that is not Grove's — exits 0 and stays quiet.
#[test]
fn a_hook_never_fails_the_agent_it_runs_inside() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime = runtime_dir(dir.path());

    for (session, payload) in [
        (
            Some("a1b2c3"),
            r#"{"hook_event_name": "SomethingNewIn2027"}"#,
        ),
        (Some("a1b2c3"), "not json at all"),
        (Some("a1b2c3"), ""),
        // Claude Code started outside Grove: no session to report about.
        (None, r#"{"hook_event_name": "Stop"}"#),
    ] {
        let output = hook(&runtime, session, payload);
        assert!(
            output.status.success(),
            "{payload:?} exited {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stdout.is_empty() && output.stderr.is_empty(),
            "{payload:?} was not quiet: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// `grove hooks` writes Claude Code's settings, and reads back what it wrote.
#[test]
fn the_hooks_command_installs_and_removes_itself() {
    let dir = tempfile::tempdir().expect("tempdir");
    let claude = dir.path().join("claude");
    std::fs::create_dir_all(&claude).expect("mkdir");
    let settings = claude.join("settings.json");
    std::fs::write(&settings, "{\"model\": \"opus\"}\n").expect("write");

    let hooks = |args: &[&str]| {
        Command::new(GROVE)
            .arg("hooks")
            .args(args)
            .env("CLAUDE_CONFIG_DIR", &claude)
            .output()
            .expect("runs")
    };

    let status = hooks(&["status"]);
    assert!(status.status.success());
    assert!(String::from_utf8_lossy(&status.stdout).contains("not installed"));

    assert!(hooks(&["install"]).status.success());
    let written = std::fs::read_to_string(&settings).expect("readable");
    assert!(written.contains("grove notify --hook"));
    assert!(written.contains("\"model\""), "the user's keys survive");
    assert!(
        String::from_utf8_lossy(&hooks(&["status"]).stdout).contains("installed on"),
        "an install must be visible to the next status"
    );

    assert!(hooks(&["uninstall"]).status.success());
    let after = std::fs::read_to_string(&settings).expect("readable");
    assert!(!after.contains("grove notify"));
    assert!(after.contains("\"model\""));

    // And the file was copied aside before each rewrite.
    let backups = std::fs::read_dir(&claude)
        .expect("read dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".bak"))
        .count();
    assert!(backups >= 1, "no backup was taken");
}

#[test]
fn an_agents_control_characters_never_reach_the_socket() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime = runtime_dir(dir.path());
    let socket = ipc::socket_path(&runtime.join("grove"));
    let listener = UnixListener::bind(&socket).expect("bind");

    let reader = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).expect("read");
        line
    });

    // A message carrying the field separator and a newline: unhandled, either
    // would split one notification into two records.
    notify(
        &runtime,
        &[
            "--state",
            "attention",
            "--session",
            "a1b2c3",
            "--message",
            "one\u{1}two\nthree",
        ],
    );

    let line = reader.join().expect("reader");
    assert_eq!(
        line.matches('\n').count(),
        1,
        "exactly one record: {line:?}"
    );
    let notification = Notification::decode(&line).expect("valid");
    assert_eq!(notification.message.as_deref(), Some("onetwothree"));
}
