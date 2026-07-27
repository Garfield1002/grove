//! Starting an agent in a session, optionally inside its own systemd scope.
//!
//! An agent runs in its own tmux window beside the shell (DESIGN.md §7), so
//! the user can switch between them and so closing the agent does not close
//! the session.
//!
//! When resource accounting is on, the agent's command is wrapped in
//! `systemd-run --user --scope`, which puts it in its own cgroup: per-agent
//! RAM and CPU become readable from `/sys/fs/cgroup`, and a runaway agent can
//! later be capped or killed by scope without touching the session's shell
//! (ARCHITECTURE.md §1). Plain shells are never wrapped.
//!
//! The agent template is user configuration, so — exactly like the terminal
//! template — it is tokenized with shell rules **first** and Grove's values are
//! substituted into the tokens afterwards. A worktree path or branch name can
//! therefore never add an argument.

use std::ffi::OsString;
use std::path::Path;

use crate::error::Result;
use crate::process::Invocation;
use crate::terminal::{self, TemplateVars};

/// Name of the tmux window an agent runs in.
pub const AGENT_WINDOW: &str = "agent";

/// The systemd launcher.
pub const SYSTEMD_RUN: &str = "systemd-run";

/// When to wrap an agent command in a systemd scope.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Accounting {
    /// Wrap when a systemd user manager is present, else do not.
    #[default]
    Auto,
    Always,
    Never,
}

impl Accounting {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Accounting::Auto),
            "always" | "true" | "on" => Some(Accounting::Always),
            "never" | "false" | "off" => Some(Accounting::Never),
            _ => None,
        }
    }

    /// Should a command be wrapped, given whether systemd is available?
    pub fn wraps(self, systemd_available: bool) -> bool {
        match self {
            Accounting::Auto => systemd_available,
            Accounting::Always => true,
            Accounting::Never => false,
        }
    }
}

/// Is a systemd user manager running for this session?
///
/// `$XDG_RUNTIME_DIR/systemd` exists exactly when `systemd --user` is managing
/// the session, which is the condition `systemd-run --user` needs. Checking the
/// directory avoids running a subprocess to find out.
pub fn systemd_available(runtime_dir: &Path) -> bool {
    runtime_dir.join("systemd").is_dir() && crate::process::is_on_path(SYSTEMD_RUN)
}

/// The scope unit an agent runs in.
///
/// The nonce keeps a restarted agent from colliding with the scope of the one
/// it replaces, which systemd would refuse while the old unit is still being
/// cleaned up. Unit names may not contain `/`, so the id — six hex characters —
/// is the only Grove value in it.
pub fn scope_unit(worktree_id: &str, kind: &str, nonce: u64) -> String {
    format!("grove-{worktree_id}-{kind}-{nonce:x}.scope")
}

/// A nonce for [`scope_unit`], from the clock.
pub fn nonce() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Wrap an invocation in a transient systemd scope.
///
/// `--collect` makes systemd forget the unit once it exits, so a failed agent
/// does not leave a unit behind that would block the next one.
pub fn in_scope(unit: &str, invocation: Invocation) -> Invocation {
    let mut wrapped = Invocation::new(SYSTEMD_RUN)
        .arg("--user")
        .arg("--scope")
        .arg("--collect")
        .arg("--quiet")
        .arg(format!("--unit={unit}"))
        .arg("--");
    wrapped = wrapped.arg(invocation.program);
    wrapped.args(invocation.args)
}

/// Expand the agent template for a worktree.
pub fn expand(template: &str, vars: &TemplateVars) -> Result<Invocation> {
    terminal::expand(template, vars)
}

/// Build the `new-window` invocation that starts an agent in a session.
///
/// The command is passed as separate arguments after the tmux options, so tmux
/// executes it without a shell.
pub fn new_window_args(session: &str, worktree: &Path, command: &Invocation) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("new-window"),
        OsString::from("-t"),
        OsString::from(session),
        OsString::from("-n"),
        OsString::from(AGENT_WINDOW),
        OsString::from("-c"),
        worktree.as_os_str().to_os_string(),
        // Everything after this is the command, never tmux's own options.
        OsString::from("--"),
        command.program.clone(),
    ];
    args.extend(command.args.iter().cloned());
    args
}

/// Everything needed to start an agent, resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentLaunch {
    /// The tmux invocation, ready to run against the private server.
    pub args: Vec<OsString>,
    /// The scope unit, when one was used. `None` means unwrapped.
    pub unit: Option<String>,
}

/// Resolve an agent launch: expand the template, optionally wrap it in a
/// scope, and build the tmux `new-window` arguments.
pub fn launch(
    template: &str,
    vars: &TemplateVars,
    session: &str,
    worktree: &Path,
    worktree_id: &str,
    accounting: Accounting,
    systemd_available: bool,
) -> Result<AgentLaunch> {
    let command = expand(template, vars)?;
    let (command, unit) = if accounting.wraps(systemd_available) {
        let unit = scope_unit(worktree_id, AGENT_WINDOW, nonce());
        (in_scope(&unit, command), Some(unit))
    } else {
        (command, None)
    };
    Ok(AgentLaunch {
        args: new_window_args(session, worktree, &command),
        unit,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn vars() -> TemplateVars {
        TemplateVars::new(
            Path::new("/run/user/1000/grove/tmux.sock"),
            "wt-a1b2c3",
            Path::new("/home/u/my worktrees/auth"),
            "acme-web",
            "feature/auth",
        )
    }

    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn accounting_parses_the_configured_spellings() {
        assert_eq!(Accounting::parse("auto"), Some(Accounting::Auto));
        assert_eq!(Accounting::parse(" Always "), Some(Accounting::Always));
        assert_eq!(Accounting::parse("off"), Some(Accounting::Never));
        assert_eq!(Accounting::parse("maybe"), None);
        assert_eq!(Accounting::default(), Accounting::Auto);
    }

    #[test]
    fn auto_follows_systemd_and_the_others_do_not() {
        assert!(Accounting::Auto.wraps(true));
        assert!(!Accounting::Auto.wraps(false));
        assert!(Accounting::Always.wraps(false));
        assert!(!Accounting::Never.wraps(true));
    }

    #[test]
    fn a_scope_unit_names_the_worktree_and_is_a_legal_unit_name() {
        let unit = scope_unit("a1b2c3", AGENT_WINDOW, 0x2a);
        assert_eq!(unit, "grove-a1b2c3-agent-2a.scope");
        assert!(
            !unit.contains('/') && !unit.contains(' '),
            "systemd unit names take neither"
        );
    }

    #[test]
    fn restarting_an_agent_gets_a_fresh_unit() {
        assert_ne!(
            scope_unit("a1b2c3", AGENT_WINDOW, 1),
            scope_unit("a1b2c3", AGENT_WINDOW, 2)
        );
    }

    #[test]
    fn a_scope_passes_the_command_after_a_separator() {
        let inner = Invocation::new("claude").arg("--resume");
        let wrapped = in_scope("grove-a1b2c3-agent-1.scope", inner);
        assert_eq!(wrapped.program, OsString::from("systemd-run"));
        let args = strings(&wrapped.args);
        let separator = args.iter().position(|a| a == "--").expect("separator");
        assert_eq!(&args[separator + 1..], ["claude", "--resume"]);
        assert!(args.contains(&"--unit=grove-a1b2c3-agent-1.scope".to_string()));
        assert!(args.contains(&"--collect".to_string()));
    }

    #[test]
    fn the_window_command_survives_a_path_with_spaces() {
        let command = Invocation::new("claude");
        let args = new_window_args(
            "wt-a1b2c3",
            Path::new("/home/u/my worktrees/auth"),
            &command,
        );
        let args = strings(&args);
        assert_eq!(
            args,
            [
                "new-window",
                "-t",
                "wt-a1b2c3",
                "-n",
                "agent",
                "-c",
                "/home/u/my worktrees/auth",
                "--",
                "claude",
            ]
        );
    }

    #[test]
    fn a_template_is_tokenized_before_values_are_substituted() {
        // The branch is `feature/auth` and the path has a space in it; neither
        // may become extra arguments.
        let launch = launch(
            "claude --branch {branch} --dir {worktree}",
            &vars(),
            "wt-a1b2c3",
            Path::new("/home/u/my worktrees/auth"),
            "a1b2c3",
            Accounting::Never,
            false,
        )
        .expect("expands");
        let args = strings(&launch.args);
        let separator = args.iter().position(|a| a == "--").expect("separator");
        assert_eq!(
            &args[separator + 1..],
            [
                "claude",
                "--branch",
                "feature/auth",
                "--dir",
                "/home/u/my worktrees/auth",
            ]
        );
        assert_eq!(launch.unit, None);
    }

    #[test]
    fn a_quoted_template_argument_stays_one_argument() {
        let launch = launch(
            "claude -p 'review the diff'",
            &vars(),
            "wt-a1b2c3",
            Path::new("/w"),
            "a1b2c3",
            Accounting::Never,
            false,
        )
        .expect("expands");
        let args = strings(&launch.args);
        assert!(args.contains(&"review the diff".to_string()));
    }

    #[test]
    fn a_value_containing_shell_syntax_is_not_reinterpreted() {
        // The one thing tokenize-then-substitute exists to prevent.
        let vars = TemplateVars::new(
            Path::new("/s"),
            "wt-a1b2c3",
            Path::new("/home/u/w"),
            "acme",
            "feature/$(touch pwned); echo",
        );
        let launch = launch(
            "claude --branch {branch}",
            &vars,
            "wt-a1b2c3",
            Path::new("/w"),
            "a1b2c3",
            Accounting::Never,
            false,
        )
        .expect("expands");
        let args = strings(&launch.args);
        assert_eq!(
            args.last().map(String::as_str),
            Some("feature/$(touch pwned); echo"),
            "the branch stays exactly one argument, uninterpreted"
        );
    }

    #[test]
    fn wrapping_puts_systemd_run_between_tmux_and_the_agent() {
        let launch = launch(
            "claude",
            &vars(),
            "wt-a1b2c3",
            Path::new("/w"),
            "a1b2c3",
            Accounting::Always,
            false,
        )
        .expect("expands");
        let args = strings(&launch.args);
        let tmux_separator = args.iter().position(|a| a == "--").expect("separator");
        assert_eq!(args[tmux_separator + 1], "systemd-run");
        assert!(launch.unit.is_some_and(|u| u.starts_with("grove-a1b2c3-")));
        assert_eq!(
            args.last().map(String::as_str),
            Some("claude"),
            "the agent is still the last word"
        );
    }

    #[test]
    fn an_empty_template_is_an_error_not_an_empty_command() {
        assert!(
            launch(
                "   ",
                &vars(),
                "wt-a1b2c3",
                Path::new("/w"),
                "a1b2c3",
                Accounting::Never,
                false,
            )
            .is_err()
        );
    }

    #[test]
    fn systemd_is_detected_from_the_runtime_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(
            !systemd_available(dir.path()),
            "no systemd directory means no user manager"
        );
        std::fs::create_dir(dir.path().join("systemd")).expect("mkdir");
        // Whether it now reports true depends on systemd-run being installed,
        // so only the negative case is asserted unconditionally.
        let expected = crate::process::is_on_path(SYSTEMD_RUN);
        assert_eq!(systemd_available(dir.path()), expected);
        let _ = PathBuf::new();
    }
}
