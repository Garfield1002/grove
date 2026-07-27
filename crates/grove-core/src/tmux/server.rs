//! The private tmux server.
//!
//! Grove keeps its sessions on its own socket (`$XDG_RUNTIME_DIR/grove/
//! tmux.sock`). Every invocation goes through [`TmuxServer::command`], which
//! always prepends `-S <socket>`, so no code path can reach the user's
//! default server.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::paths::ensure_private_dir;
use crate::process::{CommandOutput, Invocation};

/// The tmux executable. Not configurable in Milestone 1.
pub const TMUX: &str = "tmux";

/// Handle to Grove's private tmux server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxServer {
    socket: PathBuf,
}

impl TmuxServer {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Create the socket's parent directory with 0700 permissions. tmux
    /// creates the socket itself.
    pub fn ensure_socket_dir(&self) -> Result<()> {
        match self.socket.parent() {
            Some(parent) => ensure_private_dir(parent),
            None => Ok(()),
        }
    }

    /// Build `tmux -S <socket> <args…>`.
    pub fn command<I, S>(&self, args: I) -> Invocation
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        Invocation::new(TMUX)
            .arg("-S")
            .arg(self.socket.as_os_str())
            .args(args)
    }

    /// Run a tmux command, treating a non-zero exit as an error.
    pub fn run<I, S>(&self, args: I) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.command(args).output()
    }

    /// Run a tmux command where failure may simply mean "no server running".
    pub fn run_allow_failure<I, S>(&self, args: I) -> Result<CommandOutput>
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.command(args).output_allow_failure()
    }

    /// Is this failure just "there is no server on that socket yet"?
    ///
    /// tmux exits non-zero with these messages when the socket is absent or
    /// stale, which is a normal state, not an error to report.
    pub fn is_no_server(stderr: &str) -> bool {
        let stderr = stderr.to_ascii_lowercase();
        stderr.contains("no server running")
            || stderr.contains("error connecting to")
            || stderr.contains("no such file or directory")
            || stderr.contains("server exited unexpectedly")
    }

    /// Terminate the private server. Only used by tests and by an explicit
    /// user action — never on Grove shutdown (FR-7: sessions outlive the GUI).
    pub fn kill_server(&self) -> Result<()> {
        let out = self.run_allow_failure(["kill-server"])?;
        if out.success || Self::is_no_server(&out.stderr) {
            return Ok(());
        }
        Err(out.failure.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_command_carries_the_private_socket() {
        let server = TmuxServer::new("/run/user/1000/grove/tmux.sock");
        let inv = server.command(["list-sessions"]);
        assert_eq!(inv.program, OsString::from("tmux"));
        assert_eq!(
            inv.args,
            vec![
                OsString::from("-S"),
                OsString::from("/run/user/1000/grove/tmux.sock"),
                OsString::from("list-sessions"),
            ]
        );
    }

    #[test]
    fn socket_paths_with_spaces_stay_one_argument() {
        let server = TmuxServer::new("/tmp/my runtime/grove/tmux.sock");
        let inv = server.command(["kill-server"]);
        assert_eq!(
            inv.args[1],
            OsString::from("/tmp/my runtime/grove/tmux.sock")
        );
        assert_eq!(inv.args.len(), 3);
    }

    #[test]
    fn recognises_no_server_messages() {
        assert!(TmuxServer::is_no_server("no server running on /tmp/x\n"));
        assert!(TmuxServer::is_no_server(
            "error connecting to /tmp/x (No such file or directory)"
        ));
        assert!(!TmuxServer::is_no_server("can't find session: wt-abc123"));
        assert!(!TmuxServer::is_no_server(""));
    }

    #[test]
    fn ensure_socket_dir_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().expect("tempdir");
        let server = TmuxServer::new(tmp.path().join("grove").join("tmux.sock"));
        server.ensure_socket_dir().expect("creates dir");
        let mode = std::fs::metadata(tmp.path().join("grove"))
            .expect("stat")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);
    }
}
