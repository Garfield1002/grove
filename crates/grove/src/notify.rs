//! `grove notify` — the CLI agent wrappers and hooks call.
//!
//! Two deliveries, both best-effort, neither sufficient alone:
//!
//! 1. The durable `@grove_attention` tmux option, so a signal raised while the
//!    GUI is closed is still there when it opens.
//! 2. A line on the notify socket, so a running GUI reacts immediately instead
//!    of at the next poll.
//!
//! This runs inside an agent's hook, so it must be quiet and must not fail the
//! agent: a missing GUI, a missing tmux server and a missing session are all
//! normal states that exit 0. Only a usage error is worth a non-zero exit.

use grove_core::ipc::{self, Notification};
use grove_core::status::SessionStatus;
use grove_core::tmux::session;
use grove_core::{Paths, TmuxServer, ids};

pub const USAGE: &str = "\
grove notify — report a session's status to Grove

Usage:
  grove notify --state <state> [--session <id>] [--message <text>]

Options:
  --state <state>    one of: idle, working, attention
  --session <id>     worktree id; defaults to $GROVE_SESSION, which every
                     Grove-managed tmux session exports
  --message <text>   optional one-line summary shown with the status

Attention is sticky: it stays until you open the session. Reporting `idle`
or `working` does not clear it.
";

/// A parsed command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotifyArgs {
    pub worktree_id: String,
    pub state: SessionStatus,
    pub message: Option<String>,
}

/// Why a command line was rejected. Every variant is a usage error the caller
/// can fix; none of them are runtime conditions.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[allow(clippy::enum_variant_names)]
pub enum ArgsError {
    #[error("--state is required")]
    MissingState,
    #[error("`{0}` is not a state: expected idle, working or attention")]
    BadState(String),
    #[error("no session: pass --session <id> or run inside a Grove tmux session")]
    MissingSession,
    #[error("`{0}` is not a worktree id: expected 6 hex characters")]
    BadSession(String),
    #[error("{0} needs a value")]
    MissingValue(String),
    #[error("unknown option `{0}`")]
    Unknown(String),
}

/// Parse `notify`'s arguments, with `$GROVE_SESSION` as the session default.
///
/// `env_session` is passed in rather than read here so this stays testable
/// without touching the process environment.
pub fn parse_args(args: &[String], env_session: Option<&str>) -> Result<NotifyArgs, ArgsError> {
    let mut state: Option<SessionStatus> = None;
    let mut session: Option<String> = None;
    let mut message: Option<String> = None;

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let mut value = |name: &str| {
            iter.next()
                .cloned()
                .ok_or_else(|| ArgsError::MissingValue(name.to_string()))
        };
        match arg.as_str() {
            "--state" | "-s" => {
                let raw = value("--state")?;
                state = Some(SessionStatus::parse(&raw).ok_or(ArgsError::BadState(raw))?);
            }
            "--session" => session = Some(value("--session")?),
            "--message" | "-m" => message = Some(value("--message")?),
            other => {
                // `--state=attention` is the spelling people reach for first.
                if let Some(raw) = other.strip_prefix("--state=") {
                    state = Some(
                        SessionStatus::parse(raw).ok_or_else(|| ArgsError::BadState(raw.into()))?,
                    );
                } else if let Some(raw) = other.strip_prefix("--session=") {
                    session = Some(raw.to_string());
                } else if let Some(raw) = other.strip_prefix("--message=") {
                    message = Some(raw.to_string());
                } else {
                    return Err(ArgsError::Unknown(other.to_string()));
                }
            }
        }
    }

    let state = state.ok_or(ArgsError::MissingState)?;
    let worktree_id = session
        .or_else(|| env_session.map(str::to_string))
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
        .ok_or(ArgsError::MissingSession)?;
    // Validate here rather than in the GUI: an id that cannot name a session
    // would otherwise fail silently on both delivery paths.
    if !ids::is_worktree_id(&worktree_id) {
        return Err(ArgsError::BadSession(worktree_id));
    }

    Ok(NotifyArgs {
        worktree_id,
        state,
        message,
    })
}

/// What a notify run actually managed to deliver. Returned for the exit
/// message; neither being false is an error.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Delivery {
    /// The durable tmux option was written.
    pub marked: bool,
    /// A running GUI accepted the notification.
    pub delivered: bool,
}

/// Run `grove notify`.
pub fn run(args: &[String]) -> Result<Delivery, Box<dyn std::error::Error>> {
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        return Ok(Delivery::default());
    }
    let env_session = std::env::var(session::SESSION_ENV_VAR).ok();
    let parsed = match parse_args(args, env_session.as_deref()) {
        Ok(parsed) => parsed,
        Err(err) => {
            eprintln!("grove notify: {err}\n");
            eprint!("{USAGE}");
            std::process::exit(2);
        }
    };
    let paths = Paths::from_process_env()?;
    let mut delivery = Delivery::default();

    // The durable marker first: if this process is killed between the two, the
    // signal should survive rather than be lost.
    if parsed.state == SessionStatus::Attention {
        let server = TmuxServer::new(paths.tmux_socket()).with_config(paths.tmux_config_file());
        delivery.marked = session::set_attention(&server, &ids::session_name(&parsed.worktree_id))?;
    }

    let notification =
        Notification::new(parsed.worktree_id, parsed.state).with_message(parsed.message);
    delivery.delivered = ipc::send(&paths.notify_socket(), &notification)?;
    Ok(delivery)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn parses_a_full_command_line() {
        let parsed = parse_args(
            &args(&[
                "--state",
                "attention",
                "--session",
                "a1b2c3",
                "--message",
                "needs permission",
            ]),
            None,
        )
        .expect("valid");
        assert_eq!(
            parsed,
            NotifyArgs {
                worktree_id: "a1b2c3".into(),
                state: SessionStatus::Attention,
                message: Some("needs permission".into()),
            }
        );
    }

    #[test]
    fn accepts_the_equals_spelling_and_short_flags() {
        let parsed = parse_args(
            &args(&["--state=working", "--session=a1b2c3", "--message=building"]),
            None,
        )
        .expect("valid");
        assert_eq!(parsed.state, SessionStatus::Working);
        assert_eq!(parsed.worktree_id, "a1b2c3");
        assert_eq!(parsed.message.as_deref(), Some("building"));

        let short =
            parse_args(&args(&["-s", "idle", "-m", "done"]), Some("a1b2c3")).expect("valid");
        assert_eq!(short.state, SessionStatus::Idle);
        assert_eq!(short.message.as_deref(), Some("done"));
    }

    #[test]
    fn falls_back_to_the_session_environment_variable() {
        let parsed = parse_args(&args(&["--state", "idle"]), Some("a1b2c3")).expect("valid");
        assert_eq!(parsed.worktree_id, "a1b2c3");
    }

    #[test]
    fn an_explicit_session_beats_the_environment() {
        let parsed = parse_args(
            &args(&["--state", "idle", "--session", "ddeeff"]),
            Some("a1b2c3"),
        )
        .expect("valid");
        assert_eq!(parsed.worktree_id, "ddeeff");
    }

    #[test]
    fn requires_a_state() {
        assert_eq!(
            parse_args(&args(&["--session", "a1b2c3"]), None),
            Err(ArgsError::MissingState)
        );
    }

    #[test]
    fn requires_a_session_from_somewhere() {
        assert_eq!(
            parse_args(&args(&["--state", "idle"]), None),
            Err(ArgsError::MissingSession)
        );
        // An empty GROVE_SESSION is the same as none.
        assert_eq!(
            parse_args(&args(&["--state", "idle"]), Some("  ")),
            Err(ArgsError::MissingSession)
        );
    }

    #[test]
    fn rejects_a_session_that_cannot_name_a_session() {
        assert_eq!(
            parse_args(&args(&["--state", "idle", "--session", "not-an-id"]), None),
            Err(ArgsError::BadSession("not-an-id".into()))
        );
        // A full session name is a common mistake; it is not an id.
        assert_eq!(
            parse_args(&args(&["--state", "idle", "--session", "wt-a1b2c3"]), None),
            Err(ArgsError::BadSession("wt-a1b2c3".into()))
        );
    }

    #[test]
    fn rejects_a_bad_state_and_unknown_options() {
        assert_eq!(
            parse_args(&args(&["--state", "busy"]), Some("a1b2c3")),
            Err(ArgsError::BadState("busy".into()))
        );
        assert_eq!(
            parse_args(&args(&["--state=busy"]), Some("a1b2c3")),
            Err(ArgsError::BadState("busy".into()))
        );
        assert_eq!(
            parse_args(&args(&["--loud"]), Some("a1b2c3")),
            Err(ArgsError::Unknown("--loud".into()))
        );
    }

    #[test]
    fn reports_a_flag_missing_its_value() {
        assert_eq!(
            parse_args(&args(&["--state"]), Some("a1b2c3")),
            Err(ArgsError::MissingValue("--state".into()))
        );
        assert_eq!(
            parse_args(&args(&["--state", "idle", "--message"]), Some("a1b2c3")),
            Err(ArgsError::MissingValue("--message".into()))
        );
    }

    #[test]
    fn a_message_that_looks_like_a_flag_is_still_a_message() {
        let parsed = parse_args(
            &args(&["--state", "idle", "--message", "--state=attention"]),
            Some("a1b2c3"),
        )
        .expect("valid");
        assert_eq!(parsed.state, SessionStatus::Idle);
        assert_eq!(parsed.message.as_deref(), Some("--state=attention"));
    }
}
