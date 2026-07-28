//! The `grove notify` protocol: a Unix socket in Grove's runtime directory.
//!
//! Agent wrappers and hooks run `grove notify --state attention`; that process
//! writes one line to this socket so a running GUI updates immediately, and
//! also sets the durable `@grove_attention` tmux option so the signal survives
//! the GUI being closed (ARCHITECTURE.md §1). The socket is the fast path, the
//! tmux option is the source of truth — neither alone is enough.
//!
//! The same socket carries `grove toggle`, which is the other direction of the
//! same idea: a shell command that reaches the running GUI (DESIGN.md §16).
//!
//! The wire format is one newline-terminated line per message:
//!
//! ```text
//! grove1<SEP>notify<SEP><worktree-id><SEP><state><SEP><message><SEP><window><SEP><agent><SEP><transcript>
//! grove1<SEP>toggle<SEP><slot>
//! ```
//!
//! `<SEP>` is `\u{1}`, which cannot appear in a worktree id and is stripped
//! from messages. The leading version tag lets a future format change be
//! rejected cleanly by an older binary instead of being misread.
//!
//! Everything after the state is optional and *additive*: the three trailing
//! fields were added after `grove1` shipped, so a line written by an older
//! `grove notify` simply stops early and decodes with them unset, and a line
//! written by a newer one is read up to whatever the reader understands. That
//! is why they are appended rather than inserted, and why an unusable value in
//! one of them is dropped rather than failing the whole report: none of them
//! is load-bearing for the status itself.

use std::io::{BufRead, BufReader, Cursor, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::{Error, Result};
use crate::status::SessionStatus;

/// File name of the notify socket, inside the runtime directory.
pub const SOCKET_FILE: &str = "notify.sock";
/// GUI-only delivery socket. `grove serve` owns [`SOCKET_FILE`] and forwards
/// commands here while the GUI is running.
pub const GUI_SOCKET_FILE: &str = "gui.sock";

/// Protocol version tag; the first field of every line.
pub const VERSION: &str = "grove1";

const SEP: char = '\u{1}';
const KIND_NOTIFY: &str = "notify";
const KIND_TOGGLE: &str = "toggle";
const KIND_PING: &str = "ping";
const KIND_GUI_READY: &str = "gui-ready";

/// How long `grove notify` waits on a GUI that has stopped reading. Short on
/// purpose: the notification is best-effort and must not stall an agent.
const WRITE_TIMEOUT: Duration = Duration::from_millis(500);

/// How long the GUI waits for a client to finish its line. The mirror image of
/// [`WRITE_TIMEOUT`], and load-bearing for the same reason in the other
/// direction: the listener handles one connection at a time, so a client that
/// connects and then stalls would otherwise hold it for ever and every later
/// `grove notify` and `grove toggle` would be lost in silence.
const READ_TIMEOUT: Duration = Duration::from_millis(500);

/// The maximum length of a notification message, in characters. Long enough for a
/// summary line, short enough that a hostile writer cannot make the GUI
/// allocate without bound.
pub const MAX_MESSAGE_LEN: usize = 200;

/// The maximum length of an agent's own session id, in characters. Agents mint
/// these; Grove only stores one and hands it back to the agent's own command,
/// so the cap is generous but finite.
pub const MAX_AGENT_SESSION_LEN: usize = 128;

/// The maximum length of a transcript path, in bytes. Longer than any path
/// Linux will accept, so a real one is never truncated.
pub const MAX_PATH_LEN: usize = 4096;

/// The most a single line on the socket may be. Every field is capped above,
/// so a longer line is not a report Grove could use — it is a writer that
/// should not be able to make the GUI allocate for it.
const MAX_LINE_LEN: u64 = 8 * 1024;

/// One message on the socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// An agent wrapper reporting a session's status.
    Notify(Notification),
    /// `grove toggle`: with a number, select the worktree carrying it and open
    /// its session; without one, the window itself is the subject.
    Toggle { slot: Option<u8> },
    /// Liveness probe consumed by the headless service.
    Ping,
    /// The GUI has bound its private socket; flush queued commands to it.
    GuiReady,
}

impl Command {
    /// Render the wire line, without its trailing newline.
    pub fn encode(&self) -> String {
        match self {
            Command::Notify(notification) => notification.encode(),
            Command::Toggle { slot } => {
                let slot = slot.map(|n| n.to_string()).unwrap_or_default();
                format!("{VERSION}{SEP}{KIND_TOGGLE}{SEP}{slot}")
            }
            Command::Ping => format!("{VERSION}{SEP}{KIND_PING}"),
            Command::GuiReady => format!("{VERSION}{SEP}{KIND_GUI_READY}"),
        }
    }

    /// Parse one wire line.
    pub fn decode(line: &str) -> std::result::Result<Self, ProtocolError> {
        let line = line.trim_end_matches(['\n', '\r']);
        let mut fields = line.split(SEP);
        let (Some(version), Some(kind)) = (fields.next(), fields.next()) else {
            return Err(ProtocolError::Malformed);
        };
        if version != VERSION {
            return Err(ProtocolError::Version(version.to_string()));
        }
        match kind {
            KIND_NOTIFY => {
                let (Some(id), Some(state)) = (fields.next(), fields.next()) else {
                    return Err(ProtocolError::Malformed);
                };
                if id.is_empty() {
                    return Err(ProtocolError::Malformed);
                }
                let state = SessionStatus::parse(state)
                    .ok_or_else(|| ProtocolError::State(state.to_string()))?;
                let message = fields
                    .next()
                    .map(sanitize_message)
                    .filter(|m| !m.is_empty());
                Ok(Command::Notify(Notification {
                    worktree_id: id.to_string(),
                    state,
                    message,
                    window: fields.next().and_then(parse_window),
                    agent_session: fields.next().and_then(sanitize_agent_session),
                    transcript: fields.next().and_then(sanitize_transcript),
                }))
            }
            KIND_TOGGLE => {
                let raw = fields.next().unwrap_or("");
                let slot = if raw.trim().is_empty() {
                    None
                } else {
                    Some(crate::state::parse_slot(raw).ok_or_else(|| {
                        ProtocolError::Slot(raw.chars().filter(|c| !c.is_control()).collect())
                    })?)
                };
                Ok(Command::Toggle { slot })
            }
            KIND_PING => Ok(Command::Ping),
            KIND_GUI_READY => Ok(Command::GuiReady),
            other => Err(ProtocolError::Kind(other.to_string())),
        }
    }
}

/// A status report from an agent wrapper.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Notification {
    /// The worktree id, i.e. `$GROVE_SESSION` inside a Grove session.
    pub worktree_id: String,
    pub state: SessionStatus,
    /// Optional one-line human summary, e.g. "needs permission to run tests".
    pub message: Option<String>,
    /// The tmux window the report came from, when the reporter could say which
    /// (`$TMUX_PANE` resolved against the server). It says *which* row of a
    /// worktree wants the user — the agent's window rather than the shell
    /// beside it — and nothing else: the status itself is still the session's.
    pub window: Option<u32>,
    /// The agent's own session id, as the agent names it. Grove stores the
    /// last one per worktree so it can offer to resume that conversation; it
    /// never parses or interprets it.
    pub agent_session: Option<String>,
    /// Where the agent keeps this session's transcript. An index entry Grove
    /// hands to the desktop on request — never read, never parsed.
    pub transcript: Option<PathBuf>,
}

impl Notification {
    pub fn new(worktree_id: impl Into<String>, state: SessionStatus) -> Self {
        Self {
            worktree_id: worktree_id.into(),
            state,
            message: None,
            window: None,
            agent_session: None,
            transcript: None,
        }
    }

    pub fn with_message(mut self, message: Option<String>) -> Self {
        self.message = message
            .map(|m| sanitize_message(&m))
            .filter(|m| !m.is_empty());
        self
    }

    pub fn with_window(mut self, window: Option<u32>) -> Self {
        self.window = window;
        self
    }

    pub fn with_agent_session(mut self, id: Option<String>) -> Self {
        self.agent_session = id.as_deref().and_then(sanitize_agent_session);
        self
    }

    pub fn with_transcript(mut self, path: Option<String>) -> Self {
        self.transcript = path.as_deref().and_then(sanitize_transcript);
        self
    }

    /// Does this report carry anything worth remembering across restarts?
    pub fn has_agent_record(&self) -> bool {
        self.agent_session.is_some() || self.transcript.is_some()
    }

    /// Render the wire line, without its trailing newline.
    pub fn encode(&self) -> String {
        let message = self.message.as_deref().unwrap_or("");
        let window = self.window.map(|w| w.to_string()).unwrap_or_default();
        let agent = self.agent_session.as_deref().unwrap_or("");
        // Lossy: a path that is not UTF-8 cannot cross this line-oriented
        // protocol. It is an index entry for a menu item, so dropping the
        // record is better than corrupting the rest of the report.
        let transcript = self
            .transcript
            .as_deref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        format!(
            "{VERSION}{SEP}{KIND_NOTIFY}{SEP}{}{SEP}{}{SEP}{message}{SEP}{window}{SEP}{agent}{SEP}{transcript}",
            self.worktree_id,
            self.state.label(),
        )
    }

    /// Parse one wire line, which must be a notification.
    pub fn decode(line: &str) -> std::result::Result<Self, ProtocolError> {
        match Command::decode(line)? {
            Command::Notify(notification) => Ok(notification),
            Command::Toggle { .. } | Command::Ping | Command::GuiReady => {
                Err(ProtocolError::Kind(KIND_TOGGLE.to_string()))
            }
        }
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

/// Parse the window field. A value that is not a window index is dropped
/// rather than rejected: it is a hint about *which row* to mark, and losing it
/// must never cost the status report it travels with.
fn parse_window(raw: &str) -> Option<u32> {
    raw.trim().parse().ok()
}

/// Accept an agent's session id, or nothing.
///
/// Conservative on purpose: this string is stored in `state.toml` and later
/// substituted into the user's own agent command, so it is restricted to the
/// characters an id is actually made of. Anything else — a space, a quote, a
/// shell metacharacter — means this is not an id and it is dropped.
fn sanitize_agent_session(raw: &str) -> Option<String> {
    let id = raw.trim();
    if id.is_empty() || id.chars().count() > MAX_AGENT_SESSION_LEN {
        return None;
    }
    // Alphanumeric first: an id that begins with a dash would be read as an
    // option by the very command it is substituted into.
    if !id.starts_with(|c: char| c.is_ascii_alphanumeric()) {
        return None;
    }
    id.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
        .then(|| id.to_string())
}

/// Accept a transcript path, or nothing.
///
/// Absolute only: Grove hands this to the desktop opener from its own working
/// directory, where a relative path would name something else entirely.
fn sanitize_transcript(raw: &str) -> Option<PathBuf> {
    let cleaned: String = raw.chars().filter(|c| !c.is_control()).collect();
    let path = Path::new(cleaned.trim());
    (path.is_absolute() && cleaned.len() <= MAX_PATH_LEN).then(|| path.to_path_buf())
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
    #[error("`{0}` is not a number a worktree can carry: expected 1–9")]
    Slot(String),
}

/// The notify socket path inside a runtime directory.
pub fn socket_path(runtime_dir: &Path) -> std::path::PathBuf {
    runtime_dir.join(SOCKET_FILE)
}

pub fn gui_socket_path(runtime_dir: &Path) -> std::path::PathBuf {
    runtime_dir.join(GUI_SOCKET_FILE)
}

/// Send one notification to a running GUI.
///
/// Returns `Ok(false)` when no GUI is listening, which is a normal state, not
/// an error: the durable tmux option carries the signal until one starts.
pub fn send(socket: &Path, notification: &Notification) -> Result<bool> {
    send_command(socket, &Command::Notify(notification.clone()))
}

/// Send one command to a running GUI.
///
/// `Ok(false)` means nothing was listening. Each caller decides what that is:
/// for `notify` it is normal, for `toggle` it is "no GUI to toggle yet".
pub fn send_command(socket: &Path, command: &Command) -> Result<bool> {
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
    let line = format!("{}\n", command.encode());
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
        Err(err) => Err(Error::io("write to the notify socket", err)),
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

/// Read one command from an accepted connection.
///
/// Bounded in both directions, because the listener is serial and a client
/// controls both how much it sends and how long it takes:
///
/// - only the first line is read, and at most [`MAX_LINE_LEN`] bytes of it, so
///   a writer that never sends a newline cannot make the GUI buffer for ever;
/// - the socket carries a [`READ_TIMEOUT`], so a client that connects and then
///   stalls costs one short pause rather than every later notification.
///
/// Every way of failing collapses to [`ProtocolError::Malformed`]: nothing
/// usable arrived, and the listener logs it and takes the next connection.
pub fn read_command(stream: UnixStream) -> std::result::Result<Command, ProtocolError> {
    read_command_prefixed(stream, None)
}

/// Read a command after a protocol discriminator already consumed its first
/// byte from `stream`.
pub fn read_command_after_first(
    stream: UnixStream,
    first: u8,
) -> std::result::Result<Command, ProtocolError> {
    read_command_prefixed(stream, Some(first))
}

fn read_command_prefixed(
    stream: UnixStream,
    first: Option<u8>,
) -> std::result::Result<Command, ProtocolError> {
    // Refuse to read at all rather than read unbounded: an unarmed timeout is
    // exactly the state this function exists to avoid.
    if stream.set_read_timeout(Some(READ_TIMEOUT)).is_err() {
        return Err(ProtocolError::Malformed);
    }
    let mut line = String::new();
    let prefix = Cursor::new(first.into_iter().collect::<Vec<_>>());
    let mut reader = BufReader::new(prefix.chain(stream).take(MAX_LINE_LEN));
    match reader.read_line(&mut line) {
        // A timed-out read, a non-UTF-8 byte and a closed connection are all
        // the same answer here.
        Ok(0) | Err(_) => Err(ProtocolError::Malformed),
        // A full read with no newline in it is a line that never ended; the cap
        // truncated it, so decoding what arrived would risk acting on half a
        // message.
        Ok(_) if !line.ends_with('\n') => Err(ProtocolError::Malformed),
        Ok(_) => Command::decode(&line),
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
    fn round_trips_the_window_and_the_agent_record() {
        let notification = Notification::new("a1b2c3", SessionStatus::Attention)
            .with_message(Some("needs permission".into()))
            .with_window(Some(2))
            .with_agent_session(Some("0f3a-91bc".into()))
            .with_transcript(Some("/home/u/.claude/projects/x/0f3a.jsonl".into()));
        let decoded = Notification::decode(&notification.encode()).expect("valid");
        assert_eq!(decoded, notification);
        assert_eq!(decoded.window, Some(2));
        assert_eq!(decoded.agent_session.as_deref(), Some("0f3a-91bc"));
        assert_eq!(
            decoded.transcript.as_deref(),
            Some(Path::new("/home/u/.claude/projects/x/0f3a.jsonl"))
        );
    }

    /// The trailing fields are additive: a line from a `grove notify` that
    /// predates them is still a complete report.
    #[test]
    fn a_line_without_the_trailing_fields_still_decodes() {
        let old = "grove1\u{1}notify\u{1}a1b2c3\u{1}attention\u{1}needs permission";
        let decoded = Notification::decode(old).expect("valid");
        assert_eq!(decoded.message.as_deref(), Some("needs permission"));
        assert_eq!(decoded.window, None);
        assert_eq!(decoded.agent_session, None);
        assert_eq!(decoded.transcript, None);
        assert!(!decoded.has_agent_record());
    }

    /// And a reader that does not understand a future field must still read
    /// the ones it does.
    #[test]
    fn a_line_with_more_fields_than_expected_still_decodes() {
        let newer = "grove1\u{1}notify\u{1}a1b2c3\u{1}working\u{1}building\u{1}3\u{1}sid\u{1}/tmp/t.jsonl\u{1}something-later";
        let decoded = Notification::decode(newer).expect("valid");
        assert_eq!(decoded.state, SessionStatus::Working);
        assert_eq!(decoded.window, Some(3));
        assert_eq!(decoded.agent_session.as_deref(), Some("sid"));
    }

    /// A hint that cannot be used is dropped; the status it travelled with is
    /// not. Nothing here is worth failing a report over.
    #[test]
    fn unusable_trailing_fields_are_dropped_not_fatal() {
        let line = "grove1\u{1}notify\u{1}a1b2c3\u{1}attention\u{1}hi\u{1}not-a-window\u{1}id with spaces\u{1}relative/path.jsonl";
        let decoded = Notification::decode(line).expect("the report survives");
        assert_eq!(decoded.state, SessionStatus::Attention);
        assert_eq!(decoded.message.as_deref(), Some("hi"));
        assert_eq!(decoded.window, None, "a window index or nothing");
        assert_eq!(decoded.agent_session, None, "an id or nothing");
        assert_eq!(decoded.transcript, None, "an absolute path or nothing");
    }

    /// The id is substituted into the user's own agent command later, so
    /// anything a shell would notice is refused here, at the boundary.
    #[test]
    fn an_agent_session_id_is_restricted_to_id_characters() {
        for good in ["abc123", "0f3a-91bc-4d", "a.b_c:d", "A1"] {
            assert_eq!(
                Notification::new("a1b2c3", SessionStatus::Idle)
                    .with_agent_session(Some(good.into()))
                    .agent_session
                    .as_deref(),
                Some(good)
            );
        }
        for bad in [
            "id with spaces",
            "$(rm -rf /)",
            "a;b",
            "--flag",
            "a/b",
            "",
            "   ",
        ] {
            assert_eq!(
                Notification::new("a1b2c3", SessionStatus::Idle)
                    .with_agent_session(Some(bad.into()))
                    .agent_session,
                None,
                "{bad} is not an id"
            );
        }
        let long = "a".repeat(MAX_AGENT_SESSION_LEN + 1);
        assert_eq!(
            Notification::new("a1b2c3", SessionStatus::Idle)
                .with_agent_session(Some(long))
                .agent_session,
            None
        );
    }

    /// Grove opens this path from its own working directory, so a relative one
    /// would name a different file than the agent meant.
    #[test]
    fn a_transcript_path_must_be_absolute() {
        let absolute = Notification::new("a1b2c3", SessionStatus::Idle)
            .with_transcript(Some("/home/u/.claude/t.jsonl".into()));
        assert_eq!(
            absolute.transcript.as_deref(),
            Some(Path::new("/home/u/.claude/t.jsonl"))
        );
        for bad in ["t.jsonl", "../t.jsonl", "~/t.jsonl", ""] {
            assert_eq!(
                Notification::new("a1b2c3", SessionStatus::Idle)
                    .with_transcript(Some(bad.into()))
                    .transcript,
                None,
                "{bad} is not an absolute path"
            );
        }
    }

    /// A path with a separator in it must not become extra fields.
    #[test]
    fn a_transcript_cannot_smuggle_a_field_separator() {
        let notification = Notification::new("a1b2c3", SessionStatus::Idle)
            .with_transcript(Some("/tmp/a\u{1}b\nc.jsonl".into()));
        assert_eq!(
            notification.transcript.as_deref(),
            Some(Path::new("/tmp/abc.jsonl"))
        );
        assert_eq!(notification.encode().matches(SEP).count(), 7);
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
    fn round_trips_a_toggle_with_and_without_a_number() {
        for slot in [None, Some(1), Some(9)] {
            let command = Command::Toggle { slot };
            assert_eq!(Command::decode(&command.encode()).expect("valid"), command);
        }
    }

    #[test]
    fn rejects_a_toggle_number_out_of_range() {
        assert_eq!(
            Command::decode("grove1\u{1}toggle\u{1}0"),
            Err(ProtocolError::Slot("0".into()))
        );
        assert_eq!(
            Command::decode("grove1\u{1}toggle\u{1}12"),
            Err(ProtocolError::Slot("12".into()))
        );
        assert_eq!(
            Command::decode("grove1\u{1}toggle\u{1}three"),
            Err(ProtocolError::Slot("three".into()))
        );
    }

    /// A truncated toggle line still means "the window": the number is the
    /// optional part of that message, unlike every field of a notification.
    #[test]
    fn a_toggle_without_its_field_is_the_window() {
        assert_eq!(
            Command::decode("grove1\u{1}toggle"),
            Ok(Command::Toggle { slot: None })
        );
    }

    /// `read_notification`'s old contract, now that a second kind shares the
    /// socket: a toggle is not a notification and must not be read as one.
    #[test]
    fn a_toggle_is_not_a_notification() {
        assert_eq!(
            Notification::decode(&Command::Toggle { slot: Some(3) }.encode()),
            Err(ProtocolError::Kind("toggle".into()))
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
        // And the encoded line therefore stays a single well-formed record:
        // one separator before each field after the version tag.
        let encoded = notification.encode();
        assert_eq!(encoded.matches(SEP).count(), 7);
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
    fn bind_then_send_delivers_a_toggle() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = socket_path(dir.path());
        let listener = bind(&socket).expect("bind");
        let sent = Command::Toggle { slot: Some(4) };
        let expected = sent.clone();
        let writer = std::thread::spawn(move || send_command(&socket, &sent).expect("send"));
        let (stream, _) = listener.accept().expect("accept");
        assert_eq!(read_command(stream).expect("decoded"), expected);
        assert!(writer.join().expect("writer thread"));
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
        assert_eq!(
            read_command(stream).expect("decoded"),
            Command::Notify(expected)
        );
        assert!(writer.join().expect("writer thread"));
    }

    /// The listener is serial, so this is what keeps one stalled client from
    /// costing every later `grove notify` and `grove toggle`.
    #[test]
    fn a_client_that_stalls_is_given_up_on() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = socket_path(dir.path());
        let listener = bind(&socket).expect("bind");

        // Connect and then hold the connection open, saying nothing.
        let held = UnixStream::connect(&socket).expect("connect");
        let (stream, _) = listener.accept().expect("accept");

        let started = std::time::Instant::now();
        assert_eq!(read_command(stream), Err(ProtocolError::Malformed));
        let waited = started.elapsed();
        assert!(
            waited < READ_TIMEOUT * 4,
            "gave up after {waited:?}, which is not a bounded wait"
        );
        drop(held);

        // And the listener is still there for the next client, which is the
        // whole point.
        let sent = Command::Toggle { slot: Some(2) };
        let expected = sent.clone();
        let writer = std::thread::spawn(move || send_command(&socket, &sent).expect("send"));
        let (stream, _) = listener.accept().expect("accept");
        assert_eq!(read_command(stream).expect("decoded"), expected);
        assert!(writer.join().expect("writer thread"));
    }

    #[test]
    fn a_line_that_never_ends_is_refused_rather_than_buffered() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = socket_path(dir.path());
        let listener = bind(&socket).expect("bind");

        let flood = std::thread::spawn(move || {
            let mut stream = UnixStream::connect(&socket).expect("connect");
            // Well past the cap, and not a newline in it.
            let _ = stream.write_all(&vec![b'x'; MAX_LINE_LEN as usize * 2]);
            // Hold the connection so the reader cannot mistake this for EOF.
            std::thread::sleep(READ_TIMEOUT * 3);
        });

        let (stream, _) = listener.accept().expect("accept");
        assert_eq!(read_command(stream), Err(ProtocolError::Malformed));
        flood.join().expect("writer thread");
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
