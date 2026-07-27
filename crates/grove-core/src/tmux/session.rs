//! Sessions on Grove's private tmux server.
//!
//! One detached session per worktree, named `wt-<id>`, rooted in the worktree
//! and carrying `GROVE_SESSION=<id>` in its session environment so agent
//! wrappers can call `grove notify` without configuration.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use crate::error::{Error, ParseError, Result};
use crate::ids;
use crate::status::{self, SessionSignals};
use crate::tmux::server::TmuxServer;

const SOURCE: &str = "tmux list-sessions";
/// Field separator for `-F` formats. Chosen because it cannot appear in a
/// session name or a path.
const SEP: char = '\u{1}';
const SESSION_FORMAT: &str = concat!(
    "#{session_name}\u{1}#{session_path}\u{1}#{session_attached}",
    "\u{1}#{@grove_id}\u{1}#{@grove_project}\u{1}#{@grove_worktree}\u{1}#{@grove_repo}",
    "\u{1}#{@grove_attention}\u{1}#{session_activity}\u{1}#{session_alerts}",
);

/// Name of window 0 in a Grove session.
pub const SHELL_WINDOW: &str = "shell";
/// Environment variable exported into every Grove session.
pub const SESSION_ENV_VAR: &str = "GROVE_SESSION";

/// tmux session user options carrying Grove's mapping.
pub const OPT_ID: &str = "@grove_id";
pub const OPT_PROJECT: &str = "@grove_project";
pub const OPT_WORKTREE: &str = "@grove_worktree";
pub const OPT_REPO: &str = "@grove_repo";
/// Durable attention marker, set by `grove notify` (Milestone 4).
///
/// It lives on the tmux server rather than in Grove's memory so an attention
/// signal raised while the GUI is closed is still there when it reopens.
pub const OPT_ATTENTION: &str = "@grove_attention";
/// Value written to [`OPT_ATTENTION`] when attention is raised.
pub const ATTENTION_SET: &str = "1";

/// Everything needed to create a session for a worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSpec {
    /// Deterministic worktree id; the session is named `wt-<id>`.
    pub worktree_id: String,
    /// Canonical worktree path; the session's working directory.
    pub worktree_path: PathBuf,
    pub project_name: String,
    /// Canonical git-common-dir: the repository's identity.
    pub git_common_dir: PathBuf,
}

impl SessionSpec {
    pub fn session_name(&self) -> String {
        ids::session_name(&self.worktree_id)
    }
}

/// The `@grove_*` user options read back from a session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionMetadata {
    pub id: Option<String>,
    pub project: Option<String>,
    pub worktree: Option<PathBuf>,
    pub repo: Option<PathBuf>,
}

impl SessionMetadata {
    /// True when tmux carries the full mapping for this session, which is what
    /// restore and orphan association will rely on.
    pub fn is_complete(&self) -> bool {
        self.id.is_some() && self.worktree.is_some() && self.repo.is_some()
    }
}

/// A session as reported by tmux.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub name: String,
    pub path: PathBuf,
    /// Number of clients attached to this session.
    pub attached: u32,
    /// The `@grove_*` user options, absent for sessions Grove did not create.
    pub metadata: SessionMetadata,
    /// The durable `@grove_attention` marker is set on this session.
    pub attention: bool,
    /// `#{session_activity}`: seconds since the epoch of the last activity,
    /// or `None` when tmux reported nothing usable.
    pub activity_epoch: Option<u64>,
    /// tmux is flagging a bell for this session (`#{session_alerts}`).
    pub bell: bool,
}

impl SessionInfo {
    /// The Grove worktree id: the `@grove_id` user option when the server
    /// carries one, else the id encoded in the session name.
    pub fn worktree_id(&self) -> Option<&str> {
        self.metadata
            .id
            .as_deref()
            .or_else(|| ids::id_from_session_name(&self.name))
    }

    /// The status signals for this session, given the current time and the
    /// commands running in its panes.
    pub fn signals(&self, now_epoch: u64, pane_commands: Vec<String>) -> SessionSignals {
        SessionSignals {
            activity_age: self
                .activity_epoch
                .map(|epoch| status::activity_age(now_epoch, epoch)),
            pane_commands,
            attention_flag: self.attention,
            bell: self.bell,
            // Filled in by the poller, which has the pane pids to resolve.
            usage: None,
        }
    }
}

/// Is a tmux user option's value truthy?
///
/// Grove writes `1`, but the option is user-visible and hand-settable, so
/// accept the usual spellings and treat explicit off values as unset.
fn option_is_set(value: &str) -> bool {
    !matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "" | "0" | "off" | "false" | "no"
    )
}

/// Parse the output of `list-sessions -F` with [`SESSION_FORMAT`].
pub fn parse_sessions(output: &str) -> std::result::Result<Vec<SessionInfo>, ParseError> {
    let mut sessions = Vec::new();
    for (index, raw) in output.lines().enumerate() {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line.trim().is_empty() {
            continue;
        }
        let mut fields = line.split(SEP);
        let (Some(name), Some(path), Some(attached)) =
            (fields.next(), fields.next(), fields.next())
        else {
            return Err(ParseError::new(
                SOURCE,
                index + 1,
                "expected name, path and attached count",
            ));
        };
        if name.is_empty() {
            return Err(ParseError::new(SOURCE, index + 1, "empty session name"));
        }
        let attached = attached.trim().parse::<u32>().map_err(|_| {
            ParseError::new(
                SOURCE,
                index + 1,
                format!("`{attached}` is not an attached-client count"),
            )
        })?;
        let mut next = || fields.next().map(str::to_string).filter(|v| !v.is_empty());
        let metadata = SessionMetadata {
            id: next(),
            project: next(),
            worktree: next().map(PathBuf::from),
            repo: next().map(PathBuf::from),
        };
        let attention = next().is_some_and(|v| option_is_set(&v));
        // An unparseable activity stamp is "unknown", not an error: it only
        // costs this session its "working" hint for one poll, and a poller
        // that failed the whole list over it would show nothing at all.
        let activity_epoch = next().and_then(|v| v.trim().parse::<u64>().ok());
        let bell = next().is_some_and(|v| v.split(',').any(|a| a.trim() == "bell"));
        sessions.push(SessionInfo {
            name: name.to_string(),
            path: PathBuf::from(path),
            attached,
            metadata,
            attention,
            activity_epoch,
            bell,
        });
    }
    Ok(sessions)
}

/// All sessions on the private server. An absent or stale socket yields an
/// empty list rather than an error: no server is a normal state.
pub fn list_sessions(server: &TmuxServer) -> Result<Vec<SessionInfo>> {
    let out = server.run_allow_failure(["list-sessions", "-F", SESSION_FORMAT])?;
    if !out.success {
        if TmuxServer::is_no_server(&out.stderr) {
            return Ok(Vec::new());
        }
        return Err(out.failure.into());
    }
    Ok(parse_sessions(&out.stdout)?)
}

/// Does a session with this name exist on the private server?
pub fn has_session(server: &TmuxServer, name: &str) -> Result<bool> {
    Ok(list_sessions(server)?.iter().any(|s| s.name == name))
}

/// Build the `new-session` invocation for a worktree, without running it.
pub fn new_session_args(spec: &SessionSpec) -> Vec<OsString> {
    vec![
        OsString::from("new-session"),
        OsString::from("-d"),
        OsString::from("-s"),
        OsString::from(spec.session_name()),
        OsString::from("-c"),
        spec.worktree_path.as_os_str().to_os_string(),
        OsString::from("-n"),
        OsString::from(SHELL_WINDOW),
        OsString::from("-e"),
        OsString::from(format!("{SESSION_ENV_VAR}={}", spec.worktree_id)),
    ]
}

/// Build one `set-option -t <session> <name> <value>` invocation.
pub fn set_option_args(session: &str, name: &str, value: &OsStr) -> Vec<OsString> {
    vec![
        OsString::from("set-option"),
        OsString::from("-t"),
        OsString::from(session),
        OsString::from(name),
        value.to_os_string(),
    ]
}

/// Build one `set-option -t <session> -u <name>` invocation, unsetting it.
pub fn unset_option_args(session: &str, name: &str) -> Vec<OsString> {
    vec![
        OsString::from("set-option"),
        OsString::from("-t"),
        OsString::from(session),
        OsString::from("-u"),
        OsString::from(name),
    ]
}

/// Raise the durable attention marker on a session.
///
/// Called by the `grove notify` CLI so the signal outlives the GUI, and it is
/// what a later poll reads back as [`SessionInfo::attention`]. A missing
/// session or server is not an error: the agent may have exited already, and
/// `notify` must never fail an agent's hook.
pub fn set_attention(server: &TmuxServer, session: &str) -> Result<bool> {
    let out = server.run_allow_failure(set_option_args(
        session,
        OPT_ATTENTION,
        OsStr::new(ATTENTION_SET),
    ))?;
    if out.success {
        return Ok(true);
    }
    if TmuxServer::is_missing_target(&out.stderr) {
        return Ok(false);
    }
    Err(out.failure.into())
}

/// Clear the durable attention marker, when the user opens the session.
pub fn clear_attention(server: &TmuxServer, session: &str) -> Result<bool> {
    let out = server.run_allow_failure(unset_option_args(session, OPT_ATTENTION))?;
    if out.success {
        return Ok(true);
    }
    if TmuxServer::is_missing_target(&out.stderr) {
        return Ok(false);
    }
    Err(out.failure.into())
}

/// The `@grove_*` user options to stamp on a new session, in order.
pub fn metadata_options(spec: &SessionSpec) -> Vec<(&'static str, OsString)> {
    vec![
        (OPT_ID, OsString::from(&spec.worktree_id)),
        (OPT_PROJECT, OsString::from(&spec.project_name)),
        (OPT_WORKTREE, spec.worktree_path.as_os_str().to_os_string()),
        (OPT_REPO, spec.git_common_dir.as_os_str().to_os_string()),
    ]
}

/// Write the `@grove_*` user options onto an existing session, so the tmux
/// server itself carries the worktree ↔ session mapping (ARCHITECTURE.md §2).
pub fn set_session_metadata(server: &TmuxServer, session: &str, spec: &SessionSpec) -> Result<()> {
    for (name, value) in metadata_options(spec) {
        server.run(set_option_args(session, name, &value))?;
    }
    Ok(())
}

/// Build one `rename-session -t <old> <new>` invocation.
pub fn rename_session_args(old: &str, new: &str) -> Vec<OsString> {
    vec![
        OsString::from("rename-session"),
        OsString::from("-t"),
        OsString::from(old),
        OsString::from(new),
    ]
}

/// Adopt an orphaned session as a worktree's session (DESIGN.md §11).
///
/// Renaming is what makes the session findable again by name after a
/// `state.toml` loss, and the `@grove_*` options are what make it findable
/// even if it is renamed by hand later. Nothing is created or destroyed here:
/// the panes, their processes and their history are the same session
/// throughout.
///
/// Runs subprocesses: worker thread only.
pub fn associate_session(
    server: &TmuxServer,
    current_name: &str,
    spec: &SessionSpec,
) -> Result<String> {
    let name = spec.session_name();
    if current_name != name {
        server.run(rename_session_args(current_name, &name))?;
    }
    set_session_metadata(server, &name, spec)?;
    Ok(name)
}

/// Read the `@grove_*` user options back from a session.
pub fn session_metadata(server: &TmuxServer, session: &str) -> Result<SessionMetadata> {
    Ok(list_sessions(server)?
        .into_iter()
        .find(|s| s.name == session)
        .map(|s| s.metadata)
        .unwrap_or_default())
}

/// Create the detached session for a worktree and stamp its metadata.
pub fn create_session(server: &TmuxServer, spec: &SessionSpec) -> Result<String> {
    if !spec.worktree_path.is_dir() {
        return Err(Error::WorktreeMissing(spec.worktree_path.clone()));
    }
    server.ensure_socket_dir()?;
    let name = spec.session_name();
    server.run(new_session_args(spec))?;
    set_session_metadata(server, &name, spec)?;
    Ok(name)
}

/// Ensure the worktree's session exists, creating it if necessary. Returns the
/// session name and whether it had to be created.
pub fn ensure_session(server: &TmuxServer, spec: &SessionSpec) -> Result<(String, bool)> {
    let name = spec.session_name();
    if has_session(server, &name)? {
        return Ok((name, false));
    }
    let name = create_session(server, spec)?;
    Ok((name, true))
}

/// Read a variable from a session's environment.
pub fn session_env(server: &TmuxServer, name: &str, var: &str) -> Result<Option<String>> {
    let out = server.run_allow_failure(["show-environment", "-t", name, var])?;
    if !out.success {
        return Ok(None);
    }
    let prefix = format!("{var}=");
    Ok(out
        .stdout
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .map(str::to_string))
}

/// A pane of a session, with the process it is currently running.
///
/// Gathered before offering to remove a worktree so the dialog can say what
/// would be interrupted (DESIGN.md §13). Grove records the command name only;
/// it never reads terminal contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneInfo {
    pub session: String,
    pub pid: u32,
    /// `pane_current_command` as tmux reports it.
    pub command: String,
}

const PANE_SOURCE: &str = "tmux list-panes";
const PANE_FORMAT: &str = "#{session_name}\u{1}#{pane_pid}\u{1}#{pane_current_command}";

/// Parse the output of `list-panes -F` with [`PANE_FORMAT`].
pub fn parse_panes(output: &str) -> std::result::Result<Vec<PaneInfo>, ParseError> {
    let mut panes = Vec::new();
    for (index, raw) in output.lines().enumerate() {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line.trim().is_empty() {
            continue;
        }
        let mut fields = line.split(SEP);
        let (Some(session), Some(pid), Some(command)) =
            (fields.next(), fields.next(), fields.next())
        else {
            return Err(ParseError::new(
                PANE_SOURCE,
                index + 1,
                "expected session, pid and command",
            ));
        };
        let pid = pid.trim().parse::<u32>().map_err(|_| {
            ParseError::new(PANE_SOURCE, index + 1, format!("`{pid}` is not a pid"))
        })?;
        panes.push(PaneInfo {
            session: session.to_string(),
            pid,
            command: command.trim().to_string(),
        });
    }
    Ok(panes)
}

/// Every pane of one session. A missing session or server is an empty list:
/// "there is nothing running" is a normal answer, not an error.
pub fn list_panes(server: &TmuxServer, session: &str) -> Result<Vec<PaneInfo>> {
    let out = server.run_allow_failure(["list-panes", "-s", "-t", session, "-F", PANE_FORMAT])?;
    if !out.success {
        if TmuxServer::is_missing_target(&out.stderr) {
            return Ok(Vec::new());
        }
        return Err(out.failure.into());
    }
    Ok(parse_panes(&out.stdout)?)
}

/// Every pane on the server, across all sessions.
///
/// One invocation for the whole poll: the alternative is a `list-panes` per
/// session, which is what makes a 2 s cadence expensive once a user has a
/// dozen worktrees open.
pub fn list_all_panes(server: &TmuxServer) -> Result<Vec<PaneInfo>> {
    let out = server.run_allow_failure(["list-panes", "-a", "-F", PANE_FORMAT])?;
    if !out.success {
        if TmuxServer::is_missing_target(&out.stderr) {
            return Ok(Vec::new());
        }
        return Err(out.failure.into());
    }
    Ok(parse_panes(&out.stdout)?)
}

/// Kill one session. Never called implicitly: closing a session is its own
/// confirmed operation (ARCHITECTURE.md §8.2).
pub fn kill_session(server: &TmuxServer, name: &str) -> Result<()> {
    let out = server.run_allow_failure(["kill-session", "-t", name])?;
    if out.success || TmuxServer::is_no_server(&out.stderr) {
        return Ok(());
    }
    Err(out.failure.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_session_listing() {
        let text = "wt-a1b2c3\u{1}/home/u/proj\u{1}1\nwt-ddeeff\u{1}/home/u/wt/feature\u{1}0\n";
        let sessions = parse_sessions(text).expect("valid");
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].name, "wt-a1b2c3");
        assert_eq!(sessions[0].path, PathBuf::from("/home/u/proj"));
        assert_eq!(sessions[0].attached, 1);
        assert_eq!(sessions[0].worktree_id(), Some("a1b2c3"));
        assert_eq!(sessions[1].attached, 0);
    }

    #[test]
    fn parses_paths_with_spaces() {
        let sessions =
            parse_sessions("wt-a1b2c3\u{1}/home/u/my projects/the repo\u{1}0\n").expect("valid");
        assert_eq!(
            sessions[0].path,
            PathBuf::from("/home/u/my projects/the repo")
        );
    }

    #[test]
    fn ignores_foreign_session_names_but_still_lists_them() {
        let sessions = parse_sessions("scratch\u{1}/home/u\u{1}0\n").expect("valid");
        assert_eq!(sessions[0].name, "scratch");
        assert_eq!(sessions[0].worktree_id(), None);
    }

    #[test]
    fn empty_output_is_an_empty_list() {
        assert!(parse_sessions("").expect("valid").is_empty());
        assert!(parse_sessions("\n \n").expect("valid").is_empty());
    }

    #[test]
    fn rejects_truncated_lines() {
        let err = parse_sessions("wt-a1b2c3\u{1}/home/u/proj\n").expect_err("truncated");
        assert_eq!(err.line, 1);
        assert!(err.reason.contains("expected name, path"));
    }

    #[test]
    fn rejects_a_non_numeric_attached_count() {
        let err = parse_sessions("wt-a1b2c3\u{1}/p\u{1}many\n").expect_err("bad count");
        assert!(err.reason.contains("attached-client count"));
    }

    #[test]
    fn rejects_an_empty_session_name() {
        let err = parse_sessions("\u{1}/p\u{1}0\n").expect_err("empty name");
        assert!(err.reason.contains("empty session name"));
    }

    /// A full line as tmux emits it for a Grove session with attention raised.
    #[test]
    fn parses_the_status_fields() {
        let text = "wt-a1b2c3\u{1}/w\u{1}0\u{1}a1b2c3\u{1}proj\u{1}/w\u{1}/g\u{1}1\u{1}1753600000\u{1}bell\n";
        let sessions = parse_sessions(text).expect("valid");
        assert!(sessions[0].attention);
        assert_eq!(sessions[0].activity_epoch, Some(1_753_600_000));
        assert!(sessions[0].bell);
    }

    #[test]
    fn absent_status_fields_default_to_quiet() {
        let text = "wt-a1b2c3\u{1}/w\u{1}0\u{1}a1b2c3\u{1}proj\u{1}/w\u{1}/g\n";
        let sessions = parse_sessions(text).expect("valid");
        assert!(!sessions[0].attention);
        assert_eq!(sessions[0].activity_epoch, None);
        assert!(!sessions[0].bell);
    }

    #[test]
    fn an_unset_attention_option_is_not_attention() {
        // tmux renders an unset user option as the empty string; the option is
        // hand-editable, so explicit off values count as unset too.
        for value in ["", "0", "off", "false", "no", "OFF"] {
            let text =
                format!("wt-a1b2c3\u{1}/w\u{1}0\u{1}a1b2c3\u{1}p\u{1}/w\u{1}/g\u{1}{value}\n");
            let sessions = parse_sessions(&text).expect("valid");
            assert!(!sessions[0].attention, "{value} should not be attention");
        }
        for value in ["1", "yes", "on", "true"] {
            let text =
                format!("wt-a1b2c3\u{1}/w\u{1}0\u{1}a1b2c3\u{1}p\u{1}/w\u{1}/g\u{1}{value}\n");
            let sessions = parse_sessions(&text).expect("valid");
            assert!(sessions[0].attention, "{value} should be attention");
        }
    }

    #[test]
    fn alerts_are_matched_exactly_within_the_list() {
        let line = |alerts: &str| {
            format!(
                "wt-a1b2c3\u{1}/w\u{1}0\u{1}a1b2c3\u{1}p\u{1}/w\u{1}/g\u{1}\u{1}0\u{1}{alerts}\n"
            )
        };
        for alerts in ["bell", "activity,bell", "bell,silence"] {
            let sessions = parse_sessions(&line(alerts)).expect("valid");
            assert!(sessions[0].bell, "{alerts} carries a bell");
        }
        for alerts in ["", "activity", "silence", "activity,silence"] {
            let sessions = parse_sessions(&line(alerts)).expect("valid");
            assert!(!sessions[0].bell, "{alerts} carries no bell");
        }
    }

    #[test]
    fn an_unparseable_activity_stamp_is_unknown_not_an_error() {
        let text = "wt-a1b2c3\u{1}/w\u{1}0\u{1}a1b2c3\u{1}p\u{1}/w\u{1}/g\u{1}\u{1}soon\n";
        let sessions = parse_sessions(text).expect("valid");
        assert_eq!(sessions[0].activity_epoch, None);
    }

    #[test]
    fn signals_carry_the_activity_age_and_flags() {
        let text =
            "wt-a1b2c3\u{1}/w\u{1}0\u{1}a1b2c3\u{1}p\u{1}/w\u{1}/g\u{1}1\u{1}1000\u{1}bell\n";
        let sessions = parse_sessions(text).expect("valid");
        let signals = sessions[0].signals(1042, vec!["claude".into()]);
        assert_eq!(
            signals.activity_age,
            Some(std::time::Duration::from_secs(42))
        );
        assert_eq!(signals.pane_commands, vec!["claude".to_string()]);
        assert!(signals.attention_flag);
        assert!(signals.bell);
    }

    #[test]
    fn attention_option_args_set_and_unset_the_same_key() {
        let set = set_option_args("wt-a1b2c3", OPT_ATTENTION, OsStr::new(ATTENTION_SET));
        assert_eq!(
            set,
            ["set-option", "-t", "wt-a1b2c3", "@grove_attention", "1"]
        );
        let unset = unset_option_args("wt-a1b2c3", OPT_ATTENTION);
        assert_eq!(
            unset,
            ["set-option", "-t", "wt-a1b2c3", "-u", "@grove_attention"]
        );
    }

    #[test]
    fn parses_the_grove_user_options() {
        let text = "wt-a1b2c3\u{1}/home/u/wt/auth\u{1}0\u{1}a1b2c3\u{1}acme-web\u{1}/home/u/wt/auth\u{1}/home/u/proj/.git\n";
        let sessions = parse_sessions(text).expect("valid");
        let metadata = &sessions[0].metadata;
        assert_eq!(metadata.id.as_deref(), Some("a1b2c3"));
        assert_eq!(metadata.project.as_deref(), Some("acme-web"));
        assert_eq!(metadata.worktree, Some(PathBuf::from("/home/u/wt/auth")));
        assert_eq!(metadata.repo, Some(PathBuf::from("/home/u/proj/.git")));
        assert!(metadata.is_complete());
    }

    #[test]
    fn unset_user_options_expand_to_nothing_and_stay_absent() {
        // tmux expands an unset user option to the empty string.
        let sessions =
            parse_sessions("scratch\u{1}/home/u\u{1}0\u{1}\u{1}\u{1}\u{1}\n").expect("valid");
        assert_eq!(sessions[0].metadata, SessionMetadata::default());
        assert!(!sessions[0].metadata.is_complete());
        assert_eq!(sessions[0].worktree_id(), None);
    }

    #[test]
    fn the_user_option_id_wins_over_the_session_name() {
        let sessions = parse_sessions("renamed\u{1}/home/u\u{1}0\u{1}a1b2c3\u{1}p\u{1}/w\u{1}/g\n")
            .expect("valid");
        assert_eq!(sessions[0].worktree_id(), Some("a1b2c3"));
    }

    fn spec() -> SessionSpec {
        SessionSpec {
            worktree_id: "a1b2c3".into(),
            worktree_path: PathBuf::from("/home/u/my wt"),
            project_name: "acme web".into(),
            git_common_dir: PathBuf::from("/home/u/my proj/.git"),
        }
    }

    #[test]
    fn new_session_is_detached_rooted_named_and_carries_the_env_var() {
        let args: Vec<String> = new_session_args(&spec())
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            vec![
                "new-session",
                "-d",
                "-s",
                "wt-a1b2c3",
                "-c",
                "/home/u/my wt",
                "-n",
                "shell",
                "-e",
                "GROVE_SESSION=a1b2c3",
            ]
        );
    }

    #[test]
    fn metadata_options_cover_the_whole_mapping() {
        let options: Vec<(&str, String)> = metadata_options(&spec())
            .into_iter()
            .map(|(name, value)| (name, value.to_string_lossy().into_owned()))
            .collect();
        assert_eq!(
            options,
            vec![
                ("@grove_id", "a1b2c3".to_string()),
                ("@grove_project", "acme web".to_string()),
                ("@grove_worktree", "/home/u/my wt".to_string()),
                ("@grove_repo", "/home/u/my proj/.git".to_string()),
            ]
        );
    }

    #[test]
    fn set_option_keeps_values_with_spaces_in_one_argument() {
        let args: Vec<String> = set_option_args(
            "wt-a1b2c3",
            "@grove_worktree",
            std::ffi::OsStr::new("/home/u/my wt"),
        )
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
        assert_eq!(
            args,
            vec![
                "set-option",
                "-t",
                "wt-a1b2c3",
                "@grove_worktree",
                "/home/u/my wt",
            ]
        );
    }

    #[test]
    fn rename_keeps_names_with_odd_characters_in_one_argument() {
        assert_eq!(
            rename_session_args("my session", "wt-a1b2c3"),
            ["rename-session", "-t", "my session", "wt-a1b2c3"]
        );
    }

    #[test]
    fn parses_a_pane_listing() {
        let text = "wt-a1b2c3\u{1}4242\u{1}bash\nwt-a1b2c3\u{1}4343\u{1}cargo\n";
        let panes = parse_panes(text).expect("valid");
        assert_eq!(panes.len(), 2);
        assert_eq!(panes[0].session, "wt-a1b2c3");
        assert_eq!(panes[0].pid, 4242);
        assert_eq!(panes[0].command, "bash");
        assert_eq!(panes[1].command, "cargo");
    }

    #[test]
    fn an_empty_pane_listing_is_not_an_error() {
        assert!(parse_panes("").expect("valid").is_empty());
        assert!(parse_panes("\n \n").expect("valid").is_empty());
    }

    #[test]
    fn rejects_a_truncated_pane_line() {
        let err = parse_panes("wt-a1b2c3\u{1}4242\n").expect_err("truncated");
        assert!(err.reason.contains("expected session, pid and command"));
    }

    #[test]
    fn rejects_a_non_numeric_pid() {
        let err = parse_panes("wt-a1b2c3\u{1}none\u{1}bash\n").expect_err("bad pid");
        assert!(err.reason.contains("is not a pid"));
    }

    #[test]
    fn create_session_refuses_a_missing_worktree() {
        let server = TmuxServer::new("/tmp/grove-test-never-used.sock");
        let spec = SessionSpec {
            worktree_path: PathBuf::from("/nonexistent-grove/wt"),
            ..spec()
        };
        let err = create_session(&server, &spec).expect_err("worktree is gone");
        assert!(matches!(err, Error::WorktreeMissing(_)));
    }
}
