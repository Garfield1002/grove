//! Persistent local service.
//!
//! The service owns Grove's public runtime socket independently of the GUI.
//! Agent reports and toggles arrive here; live UI commands are forwarded to a
//! separate socket owned by the GUI, and useful commands are queued while the
//! GUI is absent.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use grove_core::ipc::{self, Command, Notification};
use grove_core::paths::ensure_private_dir;
use grove_core::process::Invocation;
use grove_core::{Paths, Result};

const USAGE: &str = "\
grove serve — run Grove's persistent local service

Usage:
  grove serve
";

const GUI_LAUNCH_GRACE: Duration = Duration::from_secs(5);

pub fn run(args: &[String], paths: &Paths) -> Result<()> {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        print!("{USAGE}");
        return Ok(());
    }
    if let Some(arg) = args.first() {
        return Err(grove_core::Error::io(
            format!("unexpected argument `{arg}`"),
            std::io::Error::from(std::io::ErrorKind::InvalidInput),
        ));
    }

    ensure_private_dir(&paths.runtime_dir)?;
    let listener = ipc::bind(&paths.notify_socket())?;
    let mut service = Service::new(paths);
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => match ipc::read_command(stream) {
                Ok(command) => service.handle(command),
                Err(error) => eprintln!("grove serve: ignoring a message: {error}"),
            },
            Err(error) => eprintln!("grove serve: connection failed: {error}"),
        }
    }
    Ok(())
}

/// Ensure a service is accepting commands, starting this executable in
/// service mode when necessary. Startup is intentionally non-blocking: the
/// durable tmux attention marker still covers reports during the short race.
pub fn ensure_running(paths: &Paths) -> Result<()> {
    if ipc::send_command(&paths.notify_socket(), &Command::Ping)? {
        return Ok(());
    }
    let executable = std::env::current_exe()
        .map_err(|error| grove_core::Error::io("locate the Grove executable", error))?;
    Invocation::new(executable).arg("serve").spawn_detached()
}

struct Service {
    gui_socket: std::path::PathBuf,
    pending_notifications: HashMap<String, Notification>,
    pending_slot: Option<u8>,
    gui_launching_since: Option<Instant>,
}

impl Service {
    fn new(paths: &Paths) -> Self {
        Self {
            gui_socket: paths.gui_socket(),
            pending_notifications: HashMap::new(),
            pending_slot: None,
            gui_launching_since: None,
        }
    }

    fn handle(&mut self, command: Command) {
        match command {
            Command::Ping => {}
            Command::GuiReady => {
                self.gui_launching_since = None;
                self.flush();
            }
            Command::Notify(notification) => {
                if !self.forward(&Command::Notify(notification.clone())) {
                    // Only the latest report per worktree matters. Attention
                    // itself is additionally durable in tmux.
                    self.pending_notifications
                        .insert(notification.worktree_id.clone(), notification);
                }
            }
            Command::Toggle { slot } => {
                if self.forward(&Command::Toggle { slot }) {
                    return;
                }
                self.pending_slot = slot;
                if let Err(error) = self.launch_gui() {
                    eprintln!("grove serve: could not launch Grove: {error}");
                }
            }
        }
    }

    fn forward(&self, command: &Command) -> bool {
        match ipc::send_command(&self.gui_socket, command) {
            Ok(delivered) => delivered,
            Err(error) => {
                eprintln!("grove serve: could not reach the GUI: {error}");
                false
            }
        }
    }

    fn flush(&mut self) {
        let pending = std::mem::take(&mut self.pending_notifications);
        for (id, notification) in pending {
            if !self.forward(&Command::Notify(notification.clone())) {
                self.pending_notifications.insert(id, notification);
            }
        }
        if let Some(slot) = self.pending_slot {
            if self.forward(&Command::Toggle { slot: Some(slot) }) {
                self.pending_slot = None;
            }
        }
    }

    fn launch_gui(&mut self) -> Result<()> {
        if self
            .gui_launching_since
            .is_some_and(|since| since.elapsed() < GUI_LAUNCH_GRACE)
        {
            return Ok(());
        }
        let executable = std::env::current_exe()
            .map_err(|error| grove_core::Error::io("locate the Grove executable", error))?;
        Invocation::new(executable).spawn_detached()?;
        self.gui_launching_since = Some(Instant::now());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use grove_core::status::SessionStatus;

    fn paths(root: &Path) -> Paths {
        Paths {
            config_dir: root.join("config"),
            state_dir: root.join("state"),
            runtime_dir: root.join("run"),
        }
    }

    #[test]
    fn notifications_queue_by_worktree_and_latest_wins() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut service = Service::new(&paths(temp.path()));
        service.handle(Command::Notify(Notification::new(
            "abc123",
            SessionStatus::Working,
        )));
        service.handle(Command::Notify(Notification::new(
            "abc123",
            SessionStatus::Idle,
        )));
        assert_eq!(service.pending_notifications.len(), 1);
        assert_eq!(
            service.pending_notifications["abc123"].state,
            SessionStatus::Idle
        );
    }

    #[test]
    fn a_gui_ready_flushes_queued_notifications() {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = paths(temp.path());
        std::fs::create_dir_all(&paths.runtime_dir).expect("mkdir");
        let listener = ipc::bind(&paths.gui_socket()).expect("bind");
        let mut service = Service::new(&paths);
        service.pending_notifications.insert(
            "abc123".into(),
            Notification::new("abc123", SessionStatus::Attention),
        );

        service.handle(Command::GuiReady);
        let (stream, _) = listener.accept().expect("accept");
        assert!(matches!(
            ipc::read_command(stream).expect("command"),
            Command::Notify(_)
        ));
        assert!(service.pending_notifications.is_empty());
    }
}
