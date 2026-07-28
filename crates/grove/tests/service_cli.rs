//! End-to-end lifecycle test for `grove serve`.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use grove_core::Paths;
use grove_core::ipc::{self, Command as IpcCommand, Notification};
use grove_core::protocol::{self, Request};
use grove_core::state::{self, AgentRecord, ProjectRecord, SlotRecord, State};
use grove_core::status::SessionStatus;

const GROVE: &str = env!("CARGO_BIN_EXE_grove");

struct ServiceProcess(Child);

impl Drop for ServiceProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
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
    let child = Command::new(GROVE)
        .arg("serve")
        .env("XDG_CONFIG_HOME", temp.path().join("config"))
        .env("XDG_STATE_HOME", temp.path().join("state"))
        .env("XDG_RUNTIME_DIR", temp.path().join("run"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("starts service");
    let mut service = ServiceProcess(child);

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if ipc::send_command(&paths.notify_socket(), &IpcCommand::Ping).unwrap_or(false) {
            break;
        }
        assert!(Instant::now() < deadline, "service did not bind its socket");
        std::thread::sleep(Duration::from_millis(20));
    }
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

    if Command::new("tmux").arg("-V").output().is_ok() {
        let snapshot = protocol::call(
            &paths.notify_socket(),
            &Request::new("snapshot-1", "session.snapshot", serde_json::json!({})),
        )
        .expect("snapshot");
        let result = snapshot.result.expect("result");
        assert_eq!(result["protocol_version"], protocol::VERSION);
        assert_eq!(result["slots"][0]["number"], 3);
        assert_eq!(result["agents"][0]["session_id"], "conversation-1");
        assert_eq!(result["unavailable_projects"][0]["project_id"], "project1");
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
