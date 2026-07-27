//! Clients attached to Grove's private tmux server.
//!
//! Milestone 1 designates one primary client: selecting a worktree runs
//! `switch-client -c <tty> -t <session>`. When no client is attached, the
//! caller launches the configured terminal instead.

use std::path::PathBuf;

use crate::error::{ParseError, Result};
use crate::tmux::server::TmuxServer;

const SOURCE: &str = "tmux list-clients";
const SEP: char = '\u{1}';
const CLIENT_FORMAT: &str = "#{client_tty}\u{1}#{client_session}\u{1}#{client_activity}";

/// A terminal attached to the private server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientInfo {
    pub tty: PathBuf,
    pub session: String,
    /// tmux activity timestamp (unix seconds); used to pick the most recently
    /// used client as the primary one.
    pub activity: i64,
}

/// Parse the output of `list-clients -F` with [`CLIENT_FORMAT`].
pub fn parse_clients(output: &str) -> std::result::Result<Vec<ClientInfo>, ParseError> {
    let mut clients = Vec::new();
    for (index, raw) in output.lines().enumerate() {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line.trim().is_empty() {
            continue;
        }
        let mut fields = line.split(SEP);
        let (Some(tty), Some(session)) = (fields.next(), fields.next()) else {
            return Err(ParseError::new(
                SOURCE,
                index + 1,
                "expected tty and session name",
            ));
        };
        if tty.is_empty() {
            return Err(ParseError::new(SOURCE, index + 1, "empty client tty"));
        }
        let activity = fields
            .next()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(0);
        clients.push(ClientInfo {
            tty: PathBuf::from(tty),
            session: session.to_string(),
            activity,
        });
    }
    Ok(clients)
}

/// All clients on the private server; empty when no server is running.
pub fn list_clients(server: &TmuxServer) -> Result<Vec<ClientInfo>> {
    let out = server.run_allow_failure(["list-clients", "-F", CLIENT_FORMAT])?;
    if !out.success {
        if TmuxServer::is_no_server(&out.stderr) {
            return Ok(Vec::new());
        }
        return Err(out.failure.into());
    }
    Ok(parse_clients(&out.stdout)?)
}

/// Choose the primary client: the most recently active one.
pub fn primary_client(clients: &[ClientInfo]) -> Option<&ClientInfo> {
    clients.iter().max_by_key(|c| c.activity)
}

/// Point an attached client at a session.
pub fn switch_client(server: &TmuxServer, client: &ClientInfo, session: &str) -> Result<()> {
    server.run([
        std::ffi::OsString::from("switch-client"),
        std::ffi::OsString::from("-c"),
        client.tty.as_os_str().to_os_string(),
        std::ffi::OsString::from("-t"),
        std::ffi::OsString::from(session),
    ])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_client_listing() {
        let text =
            "/dev/pts/3\u{1}wt-a1b2c3\u{1}1700000000\n/dev/pts/7\u{1}wt-ddeeff\u{1}1700000900\n";
        let clients = parse_clients(text).expect("valid");
        assert_eq!(clients.len(), 2);
        assert_eq!(clients[0].tty, PathBuf::from("/dev/pts/3"));
        assert_eq!(clients[0].session, "wt-a1b2c3");
        assert_eq!(clients[0].activity, 1_700_000_000);
    }

    #[test]
    fn empty_output_means_no_client_is_attached() {
        assert!(parse_clients("").expect("valid").is_empty());
        assert!(parse_clients("\n").expect("valid").is_empty());
        assert!(primary_client(&[]).is_none());
    }

    #[test]
    fn the_primary_client_is_the_most_recently_active() {
        let clients = parse_clients(
            "/dev/pts/3\u{1}a\u{1}100\n/dev/pts/7\u{1}b\u{1}900\n/dev/pts/9\u{1}c\u{1}500\n",
        )
        .expect("valid");
        let primary = primary_client(&clients).expect("one client");
        assert_eq!(primary.tty, PathBuf::from("/dev/pts/7"));
    }

    #[test]
    fn a_missing_activity_field_is_tolerated() {
        let clients = parse_clients("/dev/pts/3\u{1}wt-a1b2c3\n").expect("valid");
        assert_eq!(clients[0].activity, 0);
        assert_eq!(clients[0].session, "wt-a1b2c3");
    }

    #[test]
    fn rejects_lines_without_a_session() {
        let err = parse_clients("/dev/pts/3\n").expect_err("truncated");
        assert!(err.reason.contains("expected tty and session"));
    }

    #[test]
    fn rejects_an_empty_tty() {
        let err = parse_clients("\u{1}wt-a1b2c3\u{1}0\n").expect_err("empty tty");
        assert!(err.reason.contains("empty client tty"));
    }
}
