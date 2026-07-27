//! Desktop notifications for raised attention (DESIGN.md §6).
//!
//! Grove shells out to `notify-send` rather than linking a D-Bus client: it is
//! present on every desktop Grove targets, it is one detached process a few
//! times an hour, and it keeps this crate dependency-free.
//!
//! Argument arrays only, and the agent-supplied message is passed as a value —
//! never interpolated into a string a shell would see.

use crate::error::Result;
use crate::process::{self, Invocation};

/// The `notify-send` executable.
pub const NOTIFY_SEND: &str = "notify-send";

/// Desktop-notification category, so a desktop that groups by app can.
const APP_NAME: &str = "Grove";

/// What a notification says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attention {
    /// Project name, for the summary line.
    pub project: String,
    /// Branch or worktree label.
    pub worktree: String,
    /// The agent's own message, already sanitized by [`crate::ipc`].
    pub message: Option<String>,
}

impl Attention {
    /// The notification's summary line.
    pub fn summary(&self) -> String {
        format!("{} · {}", self.project, self.worktree)
    }

    /// The notification's body.
    pub fn body(&self) -> String {
        match &self.message {
            Some(message) => message.clone(),
            None => "Needs your attention".to_string(),
        }
    }
}

/// Build the `notify-send` invocation, without running it.
///
/// `--` separates the options from the summary: a message or project name
/// starting with `-` must never be read as an option.
pub fn notify_command(attention: &Attention) -> Invocation {
    Invocation::new(NOTIFY_SEND)
        .arg("--app-name")
        .arg(APP_NAME)
        .arg("--urgency")
        .arg("normal")
        // Replaces an earlier notification for the same worktree instead of
        // stacking one per poll.
        .arg("--hint")
        .arg(format!(
            "string:x-canonical-private-synchronous:grove-{}",
            attention.worktree
        ))
        .arg("--")
        .arg(attention.summary())
        .arg(attention.body())
}

/// Post a desktop notification, detached.
///
/// Returns `Ok(false)` when `notify-send` is not installed: a missing
/// notifier is a normal state on a minimal desktop, and the status pill in
/// the UI is the real signal in any case.
///
/// Runs a subprocess: worker thread only.
pub fn notify(attention: &Attention) -> Result<bool> {
    if !process::is_on_path(NOTIFY_SEND) {
        return Ok(false);
    }
    notify_command(attention).spawn_detached()?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn attention() -> Attention {
        Attention {
            project: "acme-web".into(),
            worktree: "feature/auth".into(),
            message: Some("needs permission to run tests".into()),
        }
    }

    #[test]
    fn the_summary_names_the_project_and_worktree() {
        assert_eq!(attention().summary(), "acme-web · feature/auth");
    }

    #[test]
    fn a_missing_message_still_says_something_useful() {
        let mut attention = attention();
        attention.message = None;
        assert_eq!(attention.body(), "Needs your attention");
    }

    #[test]
    fn the_command_passes_the_text_as_values_after_a_separator() {
        let inv = notify_command(&attention());
        assert_eq!(inv.program, OsString::from("notify-send"));
        let args: Vec<String> = inv
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let separator = args
            .iter()
            .position(|a| a == "--")
            .expect("the option separator is present");
        assert_eq!(
            &args[separator + 1..],
            ["acme-web · feature/auth", "needs permission to run tests"]
        );
    }

    #[test]
    fn text_that_looks_like_an_option_stays_text() {
        // An agent controls the message; a branch can be named anything.
        let inv = notify_command(&Attention {
            project: "-p".into(),
            worktree: "--help".into(),
            message: Some("--version".into()),
        });
        let args: Vec<String> = inv
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let separator = args.iter().position(|a| a == "--").expect("separator");
        assert_eq!(&args[separator + 1..], ["-p · --help", "--version"]);
        assert!(
            !args[..separator].iter().any(|a| a == "--version"),
            "the message must not appear among the options"
        );
    }

    #[test]
    fn each_worktree_replaces_its_own_notification_only() {
        let mut other = attention();
        other.worktree = "main".into();
        let hint = |inv: &Invocation| {
            inv.args
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .find(|a| a.starts_with("string:x-canonical"))
                .expect("the synchronous hint is present")
        };
        assert_ne!(
            hint(&notify_command(&attention())),
            hint(&notify_command(&other))
        );
    }
}
