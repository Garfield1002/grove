//! End-to-end lifecycle test for `grove serve`.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use grove_core::Paths;
use grove_core::ipc::{self, Command as IpcCommand, Notification};
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
