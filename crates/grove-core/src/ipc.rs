//! The `grove notify` protocol: a Unix socket in Grove's runtime directory.
//!
//! Agent wrappers and hooks run `grove notify --state attention`; that process
//! writes one line to this socket so a running GUI updates immediately, and
//! also sets the durable `@grove_attention` tmux option so the signal survives
//! the GUI being closed (ARCHITECTURE.md §1). The socket is the fast path, the
//! tmux option is the source of truth — neither alone is enough.
//!
//! The wire format is one newline-terminated line per notification:
//!
//! ```text
//! grove1<SEP>notify<SEP><worktree-id><SEP><state><SEP><message>
//! ```
//!
//! `<SEP>` is `\u{1}`, which cannot appear in a worktree id and is stripped
//! from messages. The leading version tag lets a future format change be
//! rejected cleanly by an older binary instead of being misread.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::Duration;

use crate::error::{Error, Result};
use crate::status::SessionStatus;

/// File name of the notify socket, inside the runtime directory.
pub const SOCKET_FILE: &str = "notify.sock";

/// Protocol version tag; the first field of every line.
pub const VERSION: &str = "grove1";

const SEP: char = '\u{1}';
const KIND_NOTIFY: &str = "notify";

/// How long `grove notify` waits on a GUI that has stopped reading. Short on
/// purpose: the notification is best-effort and must not stall an agent.
const WRITE_TIMEOUT: Duration = Duration::from_millis(500);

/// The maximum length of a notification message, in characters. Long enough for a
/// summary line, short enough that a hostile writer cannot make the GUI
/// allocate without bound.
pub const MAX_MESSAGE_LEN: usize = 200;

/// A status report from an agent wrapper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    /// The worktree id, i.e. `$GROVE_SESSION` inside a Grove session.
    pub worktree_id: String,
    pub state: SessionStatus,
    /// Optional one-line human summary, e.g. "needs permission to run tests".
    pub message: Option<String>,
}

impl Notification {
    pub fn new(worktree_id: impl Into<String>, state: SessionStatus) -> Self {
        Self {
            worktree_id: worktree_id.into(),
            state,
            message: None,
        }
    }

    pub fn with_message(mut self, message: Option<String>) -> Self {
        self.message = message
            .map(|m| sanitize_message(&m))
            .filter(|m| !m.is_empty());
        self
    }

    /// Render the wire line, without its trailing newline.
    pub fn encode(&self) -> String {
        let message = self.message.as_deref().unwrap_or("");
        format!(
            "{VERSION}{SEP}{KIND_NOTIFY}{SEP}{}{SEP}{}{SEP}{message}",
            self.worktree_id,
            self.state.label(),
        )
    }

    /// Parse one wire line.
    pub fn decode(line: &str) -> std::result::Result<Self, ProtocolError> {
        let line = line.trim_end_matches(['\n', '\r']);
        let mut fields = line.split(SEP);
        let (Some(version), Some(kind), Some(id), Some(state)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            return Err(ProtocolError::Malformed);
        };
        if version != VERSION {
            return Err(ProtocolError::Version(version.to_string()));
        }
        if kind != KIND_NOTIFY {
            return Err(ProtocolError::Kind(kind.to_string()));
        }
        if id.is_empty() {
            return Err(ProtocolError::Malformed);
        }
        let state =
            SessionStatus::parse(state).ok_or_else(|| ProtocolError::State(state.to_string()))?;
        let message = fields
            .next()
            .map(sanitize_message)
            .filter(|m| !m.is_empty());
        Ok(Self {
            worktree_id: id.to_string(),
            state,
            message,
        })
    }
}

/// Strip control characters and clamp the length.
///
/// Messages are shown in the UI and in desktop notifications, so a stray
/// escape sequence from an agent must not reach either.
fn sanitize_message(message: &str) -> String {
    let cleaned: String = message
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_MESSAGE_LEN)
        .collect();
    cleaned.trim().to_string()
}

/// Why a line could not be understood.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProtocolError {
    #[error("malformed notification")]
    Malformed,
    #[error("unsupported protocol version `{0}`")]
    Version(String),
    #[error("unknown message kind `{0}`")]
    Kind(String),
    #[error("unknown state `{0}`")]
    State(String),
}

/// The notify socket path inside a runtime directory.
pub fn socket_path(runtime_dir: &Path) -> std::path::PathBuf {
    runtime_dir.join(SOCKET_FILE)
}

/// Send one notification to a running GUI.
///
/// Returns `Ok(false)` when no GUI is listening, which is a normal state, not
/// an error: the durable tmux option carries the signal until one starts.
pub fn send(socket: &Path, notification: &Notification) -> Result<bool> {
    let mut stream = match UnixStream::connect(socket) {
        Ok(stream) => stream,
        // Nothing listening, or a socket file left behind by a crashed GUI.
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(false);
        }
        Err(err) => return Err(Error::io(format!("connect to {}", socket.display()), err)),
    };
    stream
        .set_write_timeout(Some(WRITE_TIMEOUT))
        .map_err(|err| Error::io("set the notify write timeout", err))?;
    let line = format!("{}\n", notification.encode());
    match stream.write_all(line.as_bytes()) {
        Ok(()) => Ok(true),
        // The GUI exited between connect and write, or stopped reading.
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::WouldBlock
                    | std::io::ErrorKind::TimedOut
            ) =>
        {
            Ok(false)
        }
        Err(err) => Err(Error::io("write a notification", err)),
    }
}

/// Bind the notify socket, replacing one left behind by a crashed GUI.
///
/// A socket file that nothing answers on is stale by definition — a Unix
/// socket cannot be connected to after its owner exits — so it is safe to
/// unlink. A socket that *does* answer means another Grove is running, and
/// this returns [`Error::Io`] with `AddrInUse` rather than stealing it.
pub fn bind(socket: &Path) -> Result<UnixListener> {
    if socket.exists() {
        match UnixStream::connect(socket) {
            Ok(_) => {
                return Err(Error::io(
                    format!("bind {}", socket.display()),
                    std::io::Error::new(
                        std::io::ErrorKind::AddrInUse,
                        "another Grove is already listening",
                    ),
                ));
            }
            Err(_) => {
                std::fs::remove_file(socket).map_err(|err| {
                    Error::io(format!("remove the stale socket {}", socket.display()), err)
                })?;
            }
        }
    }
    UnixListener::bind(socket).map_err(|err| Error::io(format!("bind {}", socket.display()), err))
}

/// Read one notification from an accepted connection.
///
/// Only the first line is read: each `grove notify` process sends one and
/// closes, and reading further would let a stuck writer hold the listener.
pub fn read_notification(stream: UnixStream) -> std::result::Result<Notification, ProtocolError> {
    let mut line = String::new();
    let mut reader = BufReader::new(stream);
    // A read error and an empty line are the same thing here: nothing usable
    // arrived. The listener logs and moves on either way.
    match reader.read_line(&mut line) {
        Ok(0) | Err(_) => Err(ProtocolError::Malformed),
        Ok(_) => Notification::decode(&line),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_notification() {
        let notification = Notification::new("a1b2c3", SessionStatus::Attention)
            .with_message(Some("needs permission".into()));
        let decoded = Notification::decode(&notification.encode()).expect("valid");
        assert_eq!(decoded, notification);
    }

    #[test]
    fn round_trips_without_a_message() {
        let notification = Notification::new("a1b2c3", SessionStatus::Working);
        let decoded = Notification::decode(&notification.encode()).expect("valid");
        assert_eq!(decoded.message, None);
        assert_eq!(decoded.state, SessionStatus::Working);
    }

    #[test]
    fn decodes_a_line_with_its_newline() {
        let line = format!(
            "{}\n",
            Notification::new("a1b2c3", SessionStatus::Idle).encode()
        );
        assert_eq!(
            Notification::decode(&line).expect("valid").worktree_id,
            "a1b2c3"
        );
    }

    #[test]
    fn rejects_a_foreign_version() {
        let line = "grove9\u{1}notify\u{1}a1b2c3\u{1}idle\u{1}";
        assert_eq!(
            Notification::decode(line),
            Err(ProtocolError::Version("grove9".into()))
        );
    }

    #[test]
    fn rejects_an_unknown_kind_and_state() {
        assert_eq!(
            Notification::decode("grove1\u{1}shutdown\u{1}a1b2c3\u{1}idle\u{1}"),
            Err(ProtocolError::Kind("shutdown".into()))
        );
        assert_eq!(
            Notification::decode("grove1\u{1}notify\u{1}a1b2c3\u{1}panic\u{1}"),
            Err(ProtocolError::State("panic".into()))
        );
    }

    #[test]
    fn rejects_truncated_and_empty_lines() {
        assert_eq!(
            Notification::decode("grove1\u{1}notify\u{1}a1b2c3"),
            Err(ProtocolError::Malformed)
        );
        assert_eq!(Notification::decode(""), Err(ProtocolError::Malformed));
        assert_eq!(
            Notification::decode("grove1\u{1}notify\u{1}\u{1}idle"),
            Err(ProtocolError::Malformed)
        );
    }

    #[test]
    fn a_message_cannot_smuggle_control_characters() {
        // An agent controls this text; it reaches the UI and libnotify.
        let notification = Notification::new("a1b2c3", SessionStatus::Attention)
            .with_message(Some("bad\u{1}field\u{7}bell\nnewline\x1b[31m".into()));
        assert_eq!(
            notification.message.as_deref(),
            Some("badfieldbellnewline[31m")
        );
        // And the encoded line therefore stays a single well-formed record.
        let encoded = notification.encode();
        assert_eq!(encoded.matches(SEP).count(), 4);
        assert!(!encoded.contains('\n'));
        assert_eq!(Notification::decode(&encoded).expect("valid"), notification);
    }

    #[test]
    fn a_long_message_is_clamped() {
        let notification = Notification::new("a1b2c3", SessionStatus::Idle)
            .with_message(Some("x".repeat(MAX_MESSAGE_LEN * 3)));
        assert_eq!(
            notification.message.as_deref().map(str::len),
            Some(MAX_MESSAGE_LEN)
        );
    }

    #[test]
    fn a_blank_message_becomes_none() {
        let notification =
            Notification::new("a1b2c3", SessionStatus::Idle).with_message(Some("  \n ".into()));
        assert_eq!(notification.message, None);
    }

    #[test]
    fn sending_to_a_missing_socket_is_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = socket_path(dir.path());
        let sent = send(&socket, &Notification::new("a1b2c3", SessionStatus::Idle))
            .expect("a missing socket is a normal state");
        assert!(!sent);
    }

    #[test]
    fn sending_to_a_stale_socket_file_is_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = socket_path(dir.path());
        std::fs::write(&socket, b"not a socket").expect("write");
        let sent =
            send(&socket, &Notification::new("a1b2c3", SessionStatus::Idle)).expect("no error");
        assert!(!sent);
    }

    #[test]
    fn bind_then_send_delivers_the_notification() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = socket_path(dir.path());
        let listener = bind(&socket).expect("bind");
        let sent = Notification::new("a1b2c3", SessionStatus::Attention)
            .with_message(Some("waiting".into()));
        let expected = sent.clone();
        let writer = std::thread::spawn(move || send(&socket, &sent).expect("send"));
        let (stream, _) = listener.accept().expect("accept");
        assert_eq!(read_notification(stream).expect("decoded"), expected);
        assert!(writer.join().expect("writer thread"));
    }

    #[test]
    fn bind_replaces_a_stale_socket_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = socket_path(dir.path());
        std::fs::write(&socket, b"left behind by a crash").expect("write");
        let _listener = bind(&socket).expect("a stale file is replaced");
    }

    #[test]
    fn bind_refuses_to_steal_a_live_socket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = socket_path(dir.path());
        let _first = bind(&socket).expect("first bind");
        let err = bind(&socket).expect_err("a live socket is not stolen");
        assert!(err.to_string().contains("already listening"));
    }
}
