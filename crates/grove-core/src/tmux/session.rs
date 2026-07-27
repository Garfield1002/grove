//! Sessions on Grove's private tmux server.
//!
//! One detached session per worktree, named `wt-<id>`, rooted in the worktree
//! and carrying `GROVE_SESSION=<id>` in its session environment so agent
//! wrappers can call `grove notify` without configuration.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use crate::error::{Error, ParseError, Result};
use crate::ids;
use crate::tmux::server::TmuxServer;

const SOURCE: &str = "tmux list-sessions";
/// Field separator for `-F` formats. Chosen because it cannot appear in a
/// session name or a path.
const SEP: char = '\u{1}';
const SESSION_FORMAT: &str = concat!(
    "#{session_name}\u{1}#{session_path}\u{1}#{session_attached}",
    "\u{1}#{@grove_id}\u{1}#{@grove_project}\u{1}#{@grove_worktree}\u{1}#{@grove_repo}",
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
        sessions.push(SessionInfo {
            name: name.to_string(),
            path: PathBuf::from(path),
            attached,
            metadata,
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
        if TmuxServer::is_no_server(&out.stderr) || out.stderr.contains("can't find") {
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
