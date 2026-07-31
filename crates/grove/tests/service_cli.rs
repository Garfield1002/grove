//! End-to-end lifecycle test for `grove serve`.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use grove_core::Paths;
use grove_core::ids;
use grove_core::ipc::{self, Command as IpcCommand, Notification};
use grove_core::protocol::{self, EventKind, Request};
use grove_core::state::{self, AgentRecord, Mutation, ProjectRecord, SlotRecord, State};
use grove_core::status::SessionStatus;

const GROVE: &str = env!("CARGO_BIN_EXE_grove");

struct ServiceProcess(Child);

impl Drop for ServiceProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start_service(paths: &Paths) -> ServiceProcess {
    let config_home = paths.config_dir.parent().expect("config home");
    let state_home = paths.state_dir.parent().expect("state home");
    let runtime_home = paths.runtime_dir.parent().expect("runtime home");
    ServiceProcess(
        Command::new(GROVE)
            .arg("serve")
            .env("XDG_CONFIG_HOME", config_home)
            .env("XDG_STATE_HOME", state_home)
            .env("XDG_RUNTIME_DIR", runtime_home)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("starts service"),
    )
}

fn wait_for_service(paths: &Paths) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if ipc::send_command(&paths.notify_socket(), &IpcCommand::Ping).unwrap_or(false) {
            return;
        }
        assert!(Instant::now() < deadline, "service did not bind its socket");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn command(paths: &Paths) -> Command {
    let mut command = Command::new(GROVE);
    command
        .env(
            "XDG_CONFIG_HOME",
            paths.config_dir.parent().expect("config home"),
        )
        .env(
            "XDG_STATE_HOME",
            paths.state_dir.parent().expect("state home"),
        )
        .env(
            "XDG_RUNTIME_DIR",
            paths.runtime_dir.parent().expect("runtime home"),
        );
    command
}

#[test]
fn service_queues_a_report_until_the_gui_is_ready() {
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = Paths {
        config_dir: temp.path().join("config/grove"),
        state_dir: temp.path().join("state/grove"),
        runtime_dir: temp.path().join("run/grove"),
    };
    state::save(
        &paths.state_file(),
        &State {
            projects: vec![ProjectRecord {
                id: "project1".into(),
                name: "missing-project".into(),
                repository_path: temp.path().join("missing"),
                git_common_dir: temp.path().join("missing/.git"),
                default_worktree_path: temp.path().join("worktrees"),
                is_expanded: true,
            }],
            slots: vec![SlotRecord {
                number: 3,
                worktree_id: "abc123".into(),
            }],
            agents: vec![AgentRecord {
                worktree_id: "abc123".into(),
                session_id: "conversation-1".into(),
                transcript_path: temp.path().join("transcript.jsonl"),
            }],
            ..State::default()
        },
    )
    .expect("isolated state");
    let mut service = start_service(&paths);
    wait_for_service(&paths);
    assert!(
        service.0.try_wait().expect("service state").is_none(),
        "service should remain alive without a GUI"
    );

    let ping = protocol::call(
        &paths.notify_socket(),
        &Request::new("ping-1", "ping", serde_json::json!({})),
    )
    .expect("framed round trip");
    assert!(ping.ok);
    assert_eq!(ping.result.expect("result")["protocol"], protocol::VERSION);

    let unknown = protocol::call(
        &paths.notify_socket(),
        &Request::new("unknown-1", "not.real", serde_json::json!({})),
    )
    .expect("structured error");
    assert!(!unknown.ok);
    assert_eq!(unknown.error.expect("error").code, "method_not_found");

    let projects = protocol::call(
        &paths.notify_socket(),
        &Request::new("projects-1", "project.list", serde_json::json!({})),
    )
    .expect("project list");
    assert_eq!(
        projects.result.expect("result")["projects"][0]["id"],
        "project1"
    );

    let subscription = Request::new(
        "subscribe-1",
        "event.subscribe",
        serde_json::json!({
            "topics": [
                EventKind::StateChanged,
                EventKind::ReconciliationCompleted,
            ]
        }),
    );
    let (mut event_stream, subscribed) =
        protocol::open_subscription(&paths.notify_socket(), &subscription)
            .expect("opens subscription");
    assert!(subscribed.ok);
    let subscription_id = subscribed.result.expect("subscription result")["subscription_id"]
        .as_str()
        .expect("subscription id")
        .to_string();

    let project = ProjectRecord {
        id: "project2".into(),
        name: "service-owned".into(),
        repository_path: temp.path().join("service-owned"),
        git_common_dir: temp.path().join("service-owned/.git"),
        default_worktree_path: temp.path().join("worktrees"),
        is_expanded: false,
    };
    let mutated = protocol::call(
        &paths.notify_socket(),
        &Request::new(
            "state-1",
            "state.mutate",
            serde_json::json!({
                "mutation": Mutation::UpsertProject { record: project }
            }),
        ),
    )
    .expect("state mutation");
    assert!(mutated.ok);
    let changed = protocol::read_event(&mut event_stream).expect("state event");
    assert_eq!(changed.kind, EventKind::StateChanged);
    assert!(
        changed.payload["state"]["project"]
            .as_array()
            .is_some_and(|projects| projects.iter().any(|p| p["id"] == "project2"))
    );
    let numbered = protocol::call(
        &paths.notify_socket(),
        &Request::new(
            "state-slot",
            "state.mutate",
            serde_json::json!({
                "mutation": Mutation::AssignSlot {
                    number: 4,
                    worktree_id: "first".into(),
                }
            }),
        ),
    )
    .expect("slot mutation");
    assert!(numbered.ok);
    let changed = protocol::read_event(&mut event_stream).expect("slot state event");
    assert_eq!(changed.kind, EventKind::StateChanged);
    let saved = state::load(&paths.state_file()).expect("service saved state");
    assert!(saved.find("project2").is_some());

    let rejected = protocol::call(
        &paths.notify_socket(),
        &Request::new(
            "state-2",
            "state.mutate",
            serde_json::json!({"mutation": {"kind": "unknown"}}),
        ),
    )
    .expect("mutation rejection");
    assert!(!rejected.ok);
    assert_eq!(rejected.error.expect("error").code, "invalid_params");
    assert_eq!(
        state::load(&paths.state_file()).expect("state remains readable"),
        saved
    );

    let reconciled = protocol::call_with_timeout(
        &paths.notify_socket(),
        &Request::new(
            "reconcile-1",
            "state.reconcile",
            serde_json::json!({"projects": []}),
        ),
        Duration::from_secs(5),
    )
    .expect("service reconciliation");
    assert!(reconciled.ok);
    let result = reconciled.result.expect("reconciliation result");
    assert_eq!(result["reconciliation"]["projects"], serde_json::json!([]));
    assert!(
        result["state"]["project"]
            .as_array()
            .is_some_and(|projects| projects.iter().any(|p| p["id"] == "project2"))
    );
    let reconciliation = protocol::read_event(&mut event_stream).expect("reconciliation event");
    assert_eq!(reconciliation.kind, EventKind::ReconciliationCompleted);
    assert!(reconciliation.revision > changed.revision);

    let unsubscribed = protocol::call(
        &paths.notify_socket(),
        &Request::new(
            "unsubscribe-1",
            "event.unsubscribe",
            serde_json::json!({"subscription_id": subscription_id}),
        ),
    )
    .expect("unsubscribe");
    assert_eq!(unsubscribed.result.expect("result")["unsubscribed"], true);
    drop(event_stream);

    if Command::new("tmux").arg("-V").output().is_ok() {
        let snapshot = protocol::call(
            &paths.notify_socket(),
            &Request::new("snapshot-1", "session.snapshot", serde_json::json!({})),
        )
        .expect("snapshot");
        let result = snapshot.result.expect("result");
        assert_eq!(result["protocol_version"], protocol::VERSION);
        assert!(
            result["slots"]
                .as_array()
                .is_some_and(|slots| slots.iter().any(|slot| slot["number"] == 4))
        );
        assert_eq!(result["agents"][0]["worktree_id"], "abc123");
        assert!(
            result["unavailable_projects"]
                .as_array()
                .is_some_and(|projects| projects
                    .iter()
                    .any(|project| project["project_id"] == "project2"))
        );
    }

    // An impossible payload is isolated to its own connection and cannot
    // take down the listener.
    let mut malformed = UnixStream::connect(paths.notify_socket()).expect("connect malformed");
    let oversized = u32::try_from(protocol::MAX_PAYLOAD_LEN + 1).expect("fits");
    malformed
        .write_all(&oversized.to_be_bytes())
        .expect("write malformed header");
    drop(malformed);
    let after_malformed = protocol::call(
        &paths.notify_socket(),
        &Request::new("ping-2", "ping", serde_json::json!({})),
    )
    .expect("service survived malformed peer");
    assert!(after_malformed.ok);

    let notification =
        Notification::new("abc123", SessionStatus::Idle).with_message(Some("finished".into()));
    assert!(
        ipc::send(&paths.notify_socket(), &notification).expect("sends"),
        "the service accepted the report"
    );

    let gui = ipc::bind(&paths.gui_socket()).expect("bind simulated GUI");
    assert!(
        ipc::send_command(&paths.notify_socket(), &IpcCommand::GuiReady).expect("announces GUI")
    );
    let (stream, _) = gui.accept().expect("queued delivery");
    assert_eq!(
        ipc::read_command(stream).expect("valid command"),
        IpcCommand::Notify(notification)
    );
}

#[test]
fn control_cli_ensures_idempotent_sessions_and_waits_with_a_timeout() {
    if Command::new("tmux").arg("-V").output().is_err() {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = Paths {
        config_dir: temp.path().join("config/grove"),
        state_dir: temp.path().join("state/grove"),
        runtime_dir: temp.path().join("run/grove"),
    };
    let repository = temp.path().join("repo");
    std::fs::create_dir_all(&repository).expect("repo dir");
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repository)
            .status()
            .expect("git init")
            .success()
    );
    let git_common_dir = std::fs::canonicalize(repository.join(".git")).expect("git common dir");
    let repository = std::fs::canonicalize(repository).expect("repository");
    let worktree_id = ids::worktree_id(&git_common_dir, &repository);
    state::save(
        &paths.state_file(),
        &State {
            projects: vec![ProjectRecord {
                id: "project-control".into(),
                name: "control".into(),
                repository_path: repository.clone(),
                git_common_dir: git_common_dir.clone(),
                default_worktree_path: temp.path().join("worktrees"),
                is_expanded: true,
            }],
            ..State::default()
        },
    )
    .expect("state");
    let _service = start_service(&paths);
    wait_for_service(&paths);

    let stopped = command(&paths)
        .args([
            "wait",
            &worktree_id,
            "--status",
            "stopped",
            "--timeout",
            "1",
        ])
        .output()
        .expect("wait command");
    assert!(
        stopped.status.success(),
        "{}",
        String::from_utf8_lossy(&stopped.stderr)
    );

    let subscription = Request::new(
        "control-events",
        "event.subscribe",
        serde_json::json!({"topics": [EventKind::ControlCompleted]}),
    );
    let (mut events, subscribed) =
        protocol::open_subscription(&paths.notify_socket(), &subscription).expect("subscription");
    assert!(subscribed.ok);

    let first = command(&paths)
        .args([
            "session",
            "ensure",
            &worktree_id,
            "--idempotency-key",
            "same-request",
        ])
        .output()
        .expect("ensure");
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first_json: serde_json::Value = serde_json::from_slice(&first.stdout).expect("ensure json");
    assert_eq!(first_json["created"], true);
    let completed = protocol::read_event(&mut events).expect("completion event");
    assert_eq!(completed.kind, EventKind::ControlCompleted);
    assert_eq!(completed.payload["worktree_id"], worktree_id);

    let replay = command(&paths)
        .args([
            "session",
            "ensure",
            &worktree_id,
            "--idempotency-key",
            "same-request",
        ])
        .output()
        .expect("idempotent replay");
    assert!(replay.status.success());
    assert_eq!(replay.stdout, first.stdout);

    let sessions = protocol::call(
        &paths.notify_socket(),
        &Request::new("sessions", "session.list", serde_json::json!({})),
    )
    .expect("sessions");
    assert_eq!(
        sessions.result.expect("session result")["sessions"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );

    let timed_out = command(&paths)
        .args([
            "wait",
            &worktree_id,
            "--status",
            "stopped",
            "--timeout",
            "0",
        ])
        .output()
        .expect("timeout");
    assert!(!timed_out.status.success());

    let _ = Command::new("tmux")
        .arg("-S")
        .arg(paths.tmux_socket())
        .arg("kill-server")
        .status();
}
