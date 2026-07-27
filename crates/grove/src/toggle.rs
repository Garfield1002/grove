//! `grove toggle` — the CLI a keyboard shortcut binds to.
//!
//! Two shapes, one command:
//!
//! - `grove toggle` is about the window. A running Grove closes; if none is
//!   running, one starts. Hiding and re-showing a window is not something a
//!   Wayland client can ask for (winit's `set_visible` is a documented no-op
//!   there), so closing and starting is the toggle, not a stand-in for one.
//!   The tmux sessions are untouched either way — outliving the GUI is the
//!   whole point of them (FR-7).
//! - `grove toggle <n>` is about a worktree: the one the user put `n` on in
//!   the GUI. It selects that row and opens its session, exactly as pressing
//!   Enter on it does. With no GUI running, one starts and does it as soon as
//!   the first reconciliation has told it what exists.
//!
//! Nothing here talks to git or tmux; it writes one line to the notify socket
//! and exits, or reports that the GUI must be started instead.

use grove_core::ipc::{self, Command};
use grove_core::state;

pub const USAGE: &str = "\
grove toggle — bring Grove, or one numbered worktree, to hand

Usage:
  grove toggle          start Grove, or close the running one
  grove toggle <1-9>    open the session of the worktree carrying that number

Give a worktree its number in Grove: select the row and press Alt+<digit>,
or use “Number for `grove toggle`” in its context menu. Alt+<same digit>
takes it off again.
The numbers live in state.toml and are only labels — they never name
anything git or tmux knows about.
";

/// Why a command line was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ArgsError {
    #[error("`{0}` is not a number a worktree can carry: expected 1–9")]
    BadSlot(String),
    #[error("unexpected argument `{0}`")]
    Unexpected(String),
    #[error("unknown option `{0}`")]
    Unknown(String),
}

/// Parse `toggle`'s arguments: at most one number.
pub fn parse_args(args: &[String]) -> Result<Option<u8>, ArgsError> {
    let mut slot = None;
    for arg in args {
        if arg.starts_with('-') {
            return Err(ArgsError::Unknown(arg.clone()));
        }
        if slot.is_some() {
            return Err(ArgsError::Unexpected(arg.clone()));
        }
        slot = Some(state::parse_slot(arg).ok_or_else(|| ArgsError::BadSlot(arg.clone()))?);
    }
    Ok(slot)
}

/// What `main` should do once the toggle has been attempted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    /// A running Grove took the command; this process is done.
    Done,
    /// No Grove was listening: start the GUI, and apply this toggle once it
    /// knows what exists.
    LaunchGui { slot: Option<u8> },
}

/// Run `grove toggle`.
pub fn run(args: &[String]) -> Result<Next, Box<dyn std::error::Error>> {
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        return Ok(Next::Done);
    }
    let slot = match parse_args(args) {
        Ok(slot) => slot,
        Err(err) => {
            eprintln!("grove toggle: {err}\n");
            eprint!("{USAGE}");
            std::process::exit(2);
        }
    };
    let paths = grove_core::Paths::from_process_env()?;
    let delivered = ipc::send_command(&paths.notify_socket(), &Command::Toggle { slot })?;
    Ok(if delivered {
        Next::Done
    } else {
        Next::LaunchGui { slot }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn no_argument_means_the_window() {
        assert_eq!(parse_args(&[]), Ok(None));
    }

    #[test]
    fn parses_a_number() {
        assert_eq!(parse_args(&args(&["1"])), Ok(Some(1)));
        assert_eq!(parse_args(&args(&["9"])), Ok(Some(9)));
    }

    #[test]
    fn rejects_numbers_no_worktree_can_carry() {
        assert_eq!(
            parse_args(&args(&["0"])),
            Err(ArgsError::BadSlot("0".into()))
        );
        assert_eq!(
            parse_args(&args(&["10"])),
            Err(ArgsError::BadSlot("10".into()))
        );
        assert_eq!(
            parse_args(&args(&["main"])),
            Err(ArgsError::BadSlot("main".into()))
        );
    }

    #[test]
    fn rejects_a_second_argument_and_unknown_options() {
        assert_eq!(
            parse_args(&args(&["1", "2"])),
            Err(ArgsError::Unexpected("2".into()))
        );
        assert_eq!(
            parse_args(&args(&["--slot=1"])),
            Err(ArgsError::Unknown("--slot=1".into()))
        );
    }

    /// With nothing listening, the command is not an error: it is the "start
    /// Grove" half of the toggle.
    #[test]
    fn a_missing_gui_asks_for_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = ipc::socket_path(dir.path());
        let delivered =
            ipc::send_command(&socket, &Command::Toggle { slot: Some(3) }).expect("no error");
        assert!(!delivered);
    }
}
