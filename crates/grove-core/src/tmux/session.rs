//! Sessions on Grove's private tmux server.
//!
//! One detached session per worktree, named `wt-<id>`, rooted in the worktree
//! and carrying `GROVE_SESSION=<id>` in its session environment so agent
//! wrappers can call `grove notify` without configuration.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::error::{Error, ParseError, Result};
use crate::ids;
use crate::tmux::server::TmuxServer;

const SOURCE: &str = "tmux list-sessions";
/// Field separator for `-F` formats. Chosen because it cannot appear in a
/// session name or a path.
const SEP: char = '\u{1}';
const SESSION_FORMAT: &str = "#{session_name}\u{1}#{session_path}\u{1}#{session_attached}";

/// Name of window 0 in a Grove session.
pub const SHELL_WINDOW: &str = "shell";
/// Environment variable exported into every Grove session.
pub const SESSION_ENV_VAR: &str = "GROVE_SESSION";

/// A session as reported by tmux.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub name: String,
    pub path: PathBuf,
    /// Number of clients attached to this session.
    pub attached: u32,
}

impl SessionInfo {
    /// The Grove worktree id, when this is one of Grove's sessions.
    pub fn worktree_id(&self) -> Option<&str> {
        ids::id_from_session_name(&self.name)
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
        sessions.push(SessionInfo {
            name: name.to_string(),
            path: PathBuf::from(path),
            attached,
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
pub fn new_session_args(name: &str, worktree: &Path, worktree_id: &str) -> Vec<OsString> {
    vec![
        OsString::from("new-session"),
        OsString::from("-d"),
        OsString::from("-s"),
        OsString::from(name),
        OsString::from("-c"),
        worktree.as_os_str().to_os_string(),
        OsString::from("-n"),
        OsString::from(SHELL_WINDOW),
        OsString::from("-e"),
        OsString::from(format!("{SESSION_ENV_VAR}={worktree_id}")),
    ]
}

/// Create the detached session for a worktree.
pub fn create_session(server: &TmuxServer, worktree_id: &str, worktree: &Path) -> Result<String> {
    if !worktree.is_dir() {
        return Err(Error::WorktreeMissing(worktree.to_path_buf()));
    }
    server.ensure_socket_dir()?;
    let name = ids::session_name(worktree_id);
    server.run(new_session_args(&name, worktree, worktree_id))?;
    Ok(name)
}

/// Ensure the worktree's session exists, creating it if necessary. Returns the
/// session name and whether it had to be created.
pub fn ensure_session(
    server: &TmuxServer,
    worktree_id: &str,
    worktree: &Path,
) -> Result<(String, bool)> {
    let name = ids::session_name(worktree_id);
    if has_session(server, &name)? {
        return Ok((name, false));
    }
    let name = create_session(server, worktree_id, worktree)?;
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
    fn new_session_is_detached_rooted_named_and_carries_the_env_var() {
        let args = new_session_args("wt-a1b2c3", Path::new("/home/u/my wt"), "a1b2c3");
        let args: Vec<String> = args
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
    fn create_session_refuses_a_missing_worktree() {
        let server = TmuxServer::new("/tmp/grove-test-never-used.sock");
        let err = create_session(&server, "a1b2c3", Path::new("/nonexistent-grove/wt"))
            .expect_err("worktree is gone");
        assert!(matches!(err, Error::WorktreeMissing(_)));
    }
}
