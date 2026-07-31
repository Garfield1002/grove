//! `grove notify` — the CLI agent wrappers and hooks call.
//!
//! Two deliveries, both best-effort, neither sufficient alone:
//!
//! 1. The durable `@grove_attention` and `@grove_done` tmux options, so a
//!    signal raised while the GUI is closed is still there when it opens, and
//!    so a reader in another process — `grove wait`, the service — can learn
//!    it at all. The done marker is written on *every* report, because nothing
//!    else retracts it: an agent that finishes and then starts again says
//!    `working`, and that has to take the marker down.
//! 2. A line on the notify socket, so a running GUI reacts immediately instead
//!    of at the next poll.
//!
//! This runs inside an agent's hook, so it must be quiet and must not fail the
//! agent: a missing GUI, a missing tmux server and a missing session are all
//! normal states that exit 0. Only a usage error is worth a non-zero exit.

#[cfg(feature = "agents")]
use std::io::{IsTerminal, Read};

use grove_core::ipc::{self, Notification};
use grove_core::status::{AttentionReason, SessionStatus};
use grove_core::tmux::session;
use grove_core::{Paths, TmuxServer, ids};
#[cfg(feature = "agents")]
use grove_harness::claude::HookPayload;

pub const USAGE: &str = "\
grove notify — report a session's status to Grove

Usage:
  grove notify --state <state> [options]
  grove notify --hook [options]

Options:
  --state <state>       one of: idle, done, working, attention
  --reason <reason>     why the user is wanted: waiting, permission, blocked
                        or failed. Only meaningful with --state attention,
                        and rejected with any other state.
  --hook                read a Claude Code hook payload (JSON) on stdin and
                        take the state, message, conversation id and
                        transcript path from it. Explicit flags win.
  --session <id>        worktree id; defaults to $GROVE_SESSION, which every
                        Grove-managed tmux session exports
  --message <text>      optional one-line summary shown with the status
  --window <index>      the tmux window this is about; defaults to the one
                        holding $TMUX_PANE, so the report marks the row that
                        raised it rather than the whole worktree
  --agent-session <id>  the agent's own conversation id, so Grove can offer to
                        resume it later
  --transcript <path>   absolute path to that conversation's transcript

Attention is sticky: it stays until you open the session. Reporting `idle`
or `working` does not clear it.

`done` is sticky too, but against silence rather than against you: it says
the work here finished and wants nobody, and it lasts until the session is
active again. It is the one thing Grove cannot work out for itself — a quiet
session that finished and one that never started look identical from tmux.

`grove hooks install` writes the Claude Code configuration for --hook.
";

/// A parsed command line, resolved against the environment and any hook
/// payload: everything needed to send one report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotifyArgs {
    pub worktree_id: String,
    pub state: SessionStatus,
    pub reason: Option<AttentionReason>,
    pub message: Option<String>,
    pub window: Option<u32>,
    pub agent_session: Option<String>,
    pub transcript: Option<String>,
}

/// The command line before anything else has been consulted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    /// Read a hook payload from stdin.
    pub hook: bool,
    pub state: Option<SessionStatus>,
    pub reason: Option<AttentionReason>,
    pub session: Option<String>,
    pub message: Option<String>,
    pub window: Option<u32>,
    pub agent_session: Option<String>,
    pub transcript: Option<String>,
}

/// Why a command line was rejected. Every variant is a usage error the caller
/// can fix; none of them are runtime conditions.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
// Every variant describes the same invalid CLI value and the shared suffix
// keeps diagnostics precise; removing it would make the names less clear.
#[allow(clippy::enum_variant_names)]
pub enum ArgsError {
    #[error("--state is required")]
    MissingState,
    #[error("`{0}` is not a state: expected idle, done, working or attention")]
    BadState(String),
    #[error("`{0}` is not a reason: expected waiting, permission, blocked or failed")]
    BadReason(String),
    #[error("--reason explains why the user is wanted; it needs --state attention, not `{0}`")]
    ReasonWithoutAttention(&'static str),
    #[error("no session: pass --session <id> or run inside a Grove tmux session")]
    MissingSession,
    #[error("`{0}` is not a worktree id: expected 6 hex characters")]
    BadSession(String),
    #[error("`{0}` is not a window index")]
    BadWindow(String),
    #[error("{0} needs a value")]
    MissingValue(String),
    #[error("unknown option `{0}`")]
    Unknown(String),
    #[cfg(feature = "agents")]
    #[error("--hook reads a JSON payload on stdin; it is not meant to be run by hand")]
    HookNeedsStdin,
}

/// Parse `notify`'s arguments. Nothing outside the argument list is consulted
/// here — the environment and any hook payload are applied by [`resolve`].
pub fn parse_options(args: &[String]) -> Result<Options, ArgsError> {
    let mut options = Options::default();

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let mut value = |name: &str| {
            iter.next()
                .cloned()
                .ok_or_else(|| ArgsError::MissingValue(name.to_string()))
        };
        match arg.as_str() {
            "--hook" => options.hook = true,
            "--state" | "-s" => {
                let raw = value("--state")?;
                options.state = Some(SessionStatus::parse(&raw).ok_or(ArgsError::BadState(raw))?);
            }
            "--reason" | "-r" => {
                let raw = value("--reason")?;
                options.reason =
                    Some(AttentionReason::parse(&raw).ok_or(ArgsError::BadReason(raw))?);
            }
            "--session" => options.session = Some(value("--session")?),
            "--message" | "-m" => options.message = Some(value("--message")?),
            "--window" | "-w" => {
                let raw = value("--window")?;
                options.window = Some(parse_window(&raw)?);
            }
            "--agent-session" => options.agent_session = Some(value("--agent-session")?),
            "--transcript" => options.transcript = Some(value("--transcript")?),
            other => {
                // `--state=attention` is the spelling people reach for first.
                if let Some(raw) = other.strip_prefix("--state=") {
                    options.state = Some(
                        SessionStatus::parse(raw).ok_or_else(|| ArgsError::BadState(raw.into()))?,
                    );
                } else if let Some(raw) = other.strip_prefix("--reason=") {
                    options.reason = Some(
                        AttentionReason::parse(raw)
                            .ok_or_else(|| ArgsError::BadReason(raw.into()))?,
                    );
                } else if let Some(raw) = other.strip_prefix("--session=") {
                    options.session = Some(raw.to_string());
                } else if let Some(raw) = other.strip_prefix("--message=") {
                    options.message = Some(raw.to_string());
                } else if let Some(raw) = other.strip_prefix("--window=") {
                    options.window = Some(parse_window(raw)?);
                } else if let Some(raw) = other.strip_prefix("--agent-session=") {
                    options.agent_session = Some(raw.to_string());
                } else if let Some(raw) = other.strip_prefix("--transcript=") {
                    options.transcript = Some(raw.to_string());
                } else {
                    return Err(ArgsError::Unknown(other.to_string()));
                }
            }
        }
    }
    Ok(options)
}

fn parse_window(raw: &str) -> Result<u32, ArgsError> {
    raw.trim()
        .parse()
        .map_err(|_| ArgsError::BadWindow(raw.to_string()))
}

/// The four things a hook payload can fill in, with no vendor in the shape.
///
/// [`resolve`] takes this rather than a `HookPayload` so the resolution rules —
/// a flag always beats the payload, a hook may be silent, a hand-run notify may
/// not — are one implementation that compiles with or without the `agents`
/// feature. Only the step that *produces* one of these knows about Claude Code.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookFields {
    pub state: Option<SessionStatus>,
    pub summary: Option<String>,
    pub agent_session: Option<String>,
    pub transcript: Option<String>,
}

#[cfg(feature = "agents")]
impl From<&HookPayload> for HookFields {
    fn from(payload: &HookPayload) -> Self {
        Self {
            state: payload.state(),
            summary: payload.summary(),
            agent_session: payload.session_id.clone(),
            transcript: payload.transcript_path.clone(),
        }
    }
}

/// Fill the gaps in a command line from `$GROVE_SESSION` and, when `--hook`
/// was passed, the payload Claude Code delivered.
///
/// `Ok(None)` means there is nothing to report: the payload described an event
/// Grove has no opinion about, which is the normal answer for an event a newer
/// Claude Code has added. That is a success — a hook must not fail an agent
/// because Grove has not caught up with it.
///
/// A flag always beats the payload. Someone who spelled out `--state` in their
/// own hook configuration meant it.
pub fn resolve(
    options: Options,
    env_session: Option<&str>,
    payload: Option<&HookFields>,
) -> Result<Option<NotifyArgs>, ArgsError> {
    let state = match options.state.or_else(|| payload.and_then(|p| p.state)) {
        Some(state) => state,
        // Only a hook may be silent; a hand-run `notify` still has to say what
        // it is reporting.
        None if options.hook => return Ok(None),
        None => return Err(ArgsError::MissingState),
    };
    let worktree_id = match options
        .session
        .or_else(|| env_session.map(str::to_string))
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
    {
        Some(id) => id,
        // A hook with no session is an agent started outside Grove — a plain
        // terminal, or another multiplexer. There is no row to report about
        // and nothing has gone wrong, so the hook says nothing and exits 0.
        None if options.hook => return Ok(None),
        None => return Err(ArgsError::MissingSession),
    };
    // Validate here rather than in the GUI: an id that cannot name a session
    // would otherwise fail silently on both delivery paths.
    if !ids::is_worktree_id(&worktree_id) {
        return Err(ArgsError::BadSession(worktree_id));
    }
    // Checked once the state is settled, so `--hook --reason blocked` is judged
    // against the state the payload actually produced rather than the flag's
    // absence. A reason only ever qualifies an attention: attached to anything
    // else it would be recorded, never shown, and quietly mean nothing.
    if options.reason.is_some() && state != SessionStatus::Attention {
        return Err(ArgsError::ReasonWithoutAttention(state.label()));
    }

    Ok(Some(NotifyArgs {
        worktree_id,
        state,
        reason: options.reason,
        message: options
            .message
            .or_else(|| payload.and_then(|p| p.summary.clone())),
        window: options.window,
        agent_session: options
            .agent_session
            .or_else(|| payload.and_then(|p| p.agent_session.clone())),
        transcript: options
            .transcript
            .or_else(|| payload.and_then(|p| p.transcript.clone())),
    }))
}

/// Parse and resolve in one step, for a command line with no hook payload
/// behind it. The real path through [`run`] keeps the two apart, because a
/// payload has to be read in between.
#[cfg(test)]
pub fn parse_args(args: &[String], env_session: Option<&str>) -> Result<NotifyArgs, ArgsError> {
    resolve(parse_options(args)?, env_session, None)?.ok_or(ArgsError::MissingState)
}

/// What a notify run actually managed to deliver. Returned for the exit
/// message; neither being false is an error.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Delivery {
    /// The durable tmux option was written.
    pub marked: bool,
    /// The durable done marker was written or taken down. False covers both
    /// "no such session" and "nothing to change", which are the same to a
    /// caller: there is no session carrying a claim about being finished.
    pub marked_done: bool,
    /// A running GUI accepted the notification.
    pub delivered: bool,
}

/// The most stdin a hook payload may be. Claude Code's payloads are a few
/// hundred bytes; this is only here so a mistaken pipe cannot make a hook read
/// forever.
#[cfg(feature = "agents")]
const MAX_PAYLOAD_LEN: u64 = 256 * 1024;

/// Read the hook payload from stdin.
///
/// `None` for anything that is not a payload. Running `grove notify --hook` by
/// hand is the one case worth an error instead: it would otherwise sit there
/// waiting on a terminal that is never going to produce JSON.
#[cfg(feature = "agents")]
fn read_payload() -> Result<Option<HookPayload>, ArgsError> {
    if std::io::stdin().is_terminal() {
        return Err(ArgsError::HookNeedsStdin);
    }
    let mut input = String::new();
    if std::io::stdin()
        .take(MAX_PAYLOAD_LEN)
        .read_to_string(&mut input)
        .is_err()
    {
        return Ok(None);
    }
    Ok(HookPayload::parse(&input))
}

/// The hook payload for this run, if one was asked for and understood.
///
/// The only place in `notify` that knows which agent's hooks these are. Without
/// the `agents` feature there is no vendor to read a payload from, so `--hook`
/// is a flag with nothing behind it and every field stays unfilled — the run
/// then stands or falls on its own flags, exactly like a hand-run report.
#[cfg(feature = "agents")]
fn hook_fields(options: &Options) -> Result<Option<HookFields>, ArgsError> {
    if !options.hook {
        return Ok(None);
    }
    Ok(read_payload()?.as_ref().map(HookFields::from))
}

#[cfg(not(feature = "agents"))]
fn hook_fields(_options: &Options) -> Result<Option<HookFields>, ArgsError> {
    Ok(None)
}

/// Which window this report is about.
///
/// An explicit `--window` wins. Otherwise the pane the hook is running in
/// names it, which is what puts an agent's report on the agent's row instead
/// of on every row of the worktree. Every failure here is silent and yields
/// `None`: it is a refinement of a report, never a reason to lose one.
fn resolve_window(paths: &Paths, explicit: Option<u32>) -> Option<u32> {
    if explicit.is_some() {
        return explicit;
    }
    let pane = std::env::var(session::PANE_ENV_VAR).ok()?;
    let server = TmuxServer::new(paths.tmux_socket()).with_config(paths.tmux_config_file());
    session::window_of_pane(&server, &pane).ok().flatten()
}

/// Run `grove notify`.
pub fn run(args: &[String]) -> Result<Delivery, Box<dyn std::error::Error>> {
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        return Ok(Delivery::default());
    }
    let env_session = std::env::var(session::SESSION_ENV_VAR).ok();
    let parsed = match parse_options(args).and_then(|options| {
        let payload = hook_fields(&options)?;
        resolve(options, env_session.as_deref(), payload.as_ref())
    }) {
        // An event Grove has no opinion about. Nothing to send, nothing wrong.
        Ok(None) => return Ok(Delivery::default()),
        Ok(Some(parsed)) => parsed,
        Err(err) => {
            eprintln!("grove notify: {err}\n");
            eprint!("{USAGE}");
            std::process::exit(2);
        }
    };
    let paths = Paths::from_process_env()?;
    let mut delivery = Delivery::default();

    // The durable marker first: if this process is killed between the two, the
    // signal should survive rather than be lost. It is deliberately the
    // session's and not the window's — the durable half says *that* a worktree
    // wants the user, which is what survives a restart; which window said so
    // is live detail the GUI holds while it is running.
    if parsed.state == SessionStatus::Attention {
        let server = TmuxServer::new(paths.tmux_socket()).with_config(paths.tmux_config_file());
        delivery.marked = session::set_attention(&server, &ids::session_name(&parsed.worktree_id))?;
    }
    // The done marker is the durable half of `done`, and unlike attention it is
    // written on every state: nothing else retracts it. An agent that finished
    // and then started again says `working`, and that has to take the marker
    // down, or the row would keep reporting a finish that has been overtaken.
    {
        let server = TmuxServer::new(paths.tmux_socket()).with_config(paths.tmux_config_file());
        let session = ids::session_name(&parsed.worktree_id);
        delivery.marked_done = match parsed.state {
            SessionStatus::Done => session::set_done(&server, &session)?,
            _ => session::clear_done(&server, &session)?,
        };
    }

    let notification = Notification::new(parsed.worktree_id, parsed.state)
        .with_message(parsed.message)
        .with_window(resolve_window(&paths, parsed.window))
        .with_agent_session(parsed.agent_session)
        .with_transcript(parsed.transcript)
        .with_reason(parsed.reason);
    delivery.delivered = ipc::send(&paths.notify_socket(), &notification)?;
    Ok(delivery)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    #[cfg(feature = "agents")]
    fn payload(json: &str) -> HookFields {
        HookFields::from(&HookPayload::parse(json).expect("valid payload"))
    }

    #[test]
    fn a_reason_needs_an_attention_to_explain() {
        // Recorded but never shown would be the silent-degradation case: the
        // caller believes it said something and Grove believes nothing was said.
        for state in ["done", "working", "idle"] {
            let err = parse_args(
                &args(&[
                    "--state",
                    state,
                    "--reason",
                    "blocked",
                    "--session",
                    "a1b2c3",
                ]),
                None,
            )
            .expect_err("a reason on a non-attention state is a usage error");
            assert!(
                matches!(err, ArgsError::ReasonWithoutAttention(_)),
                "{state} with a reason should be rejected, got {err:?}"
            );
        }
        parse_args(
            &args(&[
                "--state",
                "attention",
                "--reason",
                "blocked",
                "--session",
                "a1b2c3",
            ]),
            None,
        )
        .expect("attention carries a reason");
    }

    #[test]
    fn a_reason_is_accepted_joined_or_separate_and_rejected_when_unknown() {
        let joined = parse_args(
            &args(&["--state=attention", "--reason=failed", "--session=a1b2c3"]),
            None,
        )
        .expect("valid");
        assert_eq!(joined.reason, Some(AttentionReason::Failed));

        let err = parse_args(
            &args(&["--state", "attention", "--reason", "sideways"]),
            Some("a1b2c3"),
        )
        .expect_err("unknown reason");
        assert!(matches!(err, ArgsError::BadReason(value) if value == "sideways"));
    }

    #[test]
    fn done_is_a_state_the_command_line_accepts() {
        let parsed = parse_args(&args(&["--state", "done"]), Some("a1b2c3")).expect("valid");
        assert_eq!(parsed.state, SessionStatus::Done);
        assert_eq!(parsed.reason, None);
    }

    #[test]
    fn parses_a_full_command_line() {
        let parsed = parse_args(
            &args(&[
                "--state",
                "attention",
                "--reason",
                "permission",
                "--session",
                "a1b2c3",
                "--message",
                "needs permission",
                "--window",
                "2",
                "--agent-session",
                "0f3a",
                "--transcript",
                "/tmp/0f3a.jsonl",
            ]),
            None,
        )
        .expect("valid");
        assert_eq!(
            parsed,
            NotifyArgs {
                worktree_id: "a1b2c3".into(),
                state: SessionStatus::Attention,
                reason: Some(AttentionReason::Permission),
                message: Some("needs permission".into()),
                window: Some(2),
                agent_session: Some("0f3a".into()),
                transcript: Some("/tmp/0f3a.jsonl".into()),
            }
        );
    }

    #[cfg(feature = "agents")]
    #[test]
    fn a_hook_payload_supplies_the_whole_report() {
        let payload = payload(
            "{\"hook_event_name\": \"Notification\", \"message\": \"Claude needs permission\", \
              \"session_id\": \"0f3a\", \"transcript_path\": \"/tmp/0f3a.jsonl\"}",
        );
        let parsed = resolve(
            parse_options(&args(&["--hook"])).expect("parses"),
            Some("a1b2c3"),
            Some(&payload),
        )
        .expect("resolves")
        .expect("a report");
        assert_eq!(parsed.state, SessionStatus::Attention);
        assert_eq!(parsed.message.as_deref(), Some("Claude needs permission"));
        assert_eq!(parsed.agent_session.as_deref(), Some("0f3a"));
        assert_eq!(parsed.transcript.as_deref(), Some("/tmp/0f3a.jsonl"));
    }

    /// Someone who spelled a flag out in their own hook configuration meant
    /// it; the payload fills gaps, it does not overrule.
    #[cfg(feature = "agents")]
    #[test]
    fn an_explicit_flag_beats_the_payload() {
        let payload = payload(
            "{\"hook_event_name\": \"Notification\", \"message\": \"from the payload\", \
              \"session_id\": \"0f3a\"}",
        );
        let parsed = resolve(
            parse_options(&args(&[
                "--hook",
                "--state",
                "working",
                "--message",
                "mine",
                "--agent-session",
                "ddee",
            ]))
            .expect("parses"),
            Some("a1b2c3"),
            Some(&payload),
        )
        .expect("resolves")
        .expect("a report");
        assert_eq!(parsed.state, SessionStatus::Working);
        assert_eq!(parsed.message.as_deref(), Some("mine"));
        assert_eq!(parsed.agent_session.as_deref(), Some("ddee"));
    }

    /// A newer Claude Code will send events this Grove has never heard of.
    /// Saying nothing is success: a hook must not fail inside an agent.
    #[cfg(feature = "agents")]
    #[test]
    fn a_hook_event_with_no_meaning_reports_nothing() {
        let payload = payload("{\"hook_event_name\": \"SomethingNew\", \"session_id\": \"0f3a\"}");
        let resolved = resolve(
            parse_options(&args(&["--hook"])).expect("parses"),
            Some("a1b2c3"),
            Some(&payload),
        )
        .expect("resolves");
        assert_eq!(resolved, None);
    }

    /// Claude Code started in a plain terminal has no Grove session to report
    /// about. That is not an error either — there is simply no row.
    #[cfg(feature = "agents")]
    #[test]
    fn a_hook_outside_a_grove_session_reports_nothing() {
        let payload = payload("{\"hook_event_name\": \"Stop\"}");
        let resolved = resolve(
            parse_options(&args(&["--hook"])).expect("parses"),
            None,
            Some(&payload),
        )
        .expect("resolves");
        assert_eq!(resolved, None);
    }

    /// Without `--hook` the same silences are usage errors: a hand-run notify
    /// that says nothing, or names nothing, is a mistake rather than a no-op.
    #[test]
    fn a_state_and_a_session_are_still_required_without_a_hook() {
        assert_eq!(
            resolve(Options::default(), Some("a1b2c3"), None),
            Err(ArgsError::MissingState)
        );
        assert_eq!(
            resolve(
                Options {
                    state: Some(SessionStatus::Idle),
                    ..Options::default()
                },
                None,
                None
            ),
            Err(ArgsError::MissingSession)
        );
    }

    #[test]
    fn a_window_must_be_a_window_index() {
        assert_eq!(
            parse_options(&args(&["--window", "two"])),
            Err(ArgsError::BadWindow("two".into()))
        );
        assert_eq!(
            parse_options(&args(&["--window=-1"])),
            Err(ArgsError::BadWindow("-1".into()))
        );
        assert_eq!(
            parse_options(&args(&["--window=0"])).expect("valid").window,
            Some(0)
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
