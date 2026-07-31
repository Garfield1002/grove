//! The safe-removal risk report (DESIGN.md §13, ARCHITECTURE.md §8).
//!
//! Removing a project from Grove, closing a tmux session, removing a git
//! worktree and deleting a branch are **four separate operations**, each with
//! its own confirmation. Nothing here performs any of them: this module only
//! assembles the facts the dialog must show *before* the user decides, and
//! decides whether the destructive options may be offered at all.
//!
//! [`assemble`] is a pure function over gathered inputs, so every rule below
//! is testable without a repository, a worktree or a tmux server.

use std::path::PathBuf;

use crate::git::status::StatusSummary;
use crate::tmux::session::PaneInfo;

/// What is known about commits that exist only in this worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unpushed {
    /// `git rev-list --count <upstream>..HEAD` succeeded.
    Count(u32),
    /// The branch tracks nothing, so "unpushed" has no meaning here. Shown as
    /// unknown — never as zero.
    NoUpstream,
    /// The count could not be taken (detached HEAD, unborn branch, git error).
    Unknown(String),
}

impl Unpushed {
    /// One line for the dialog.
    pub fn describe(&self) -> String {
        match self {
            Unpushed::Count(0) => "none — every commit is on the upstream".to_string(),
            Unpushed::Count(1) => "1 commit not on the upstream".to_string(),
            Unpushed::Count(n) => format!("{n} commits not on the upstream"),
            Unpushed::NoUpstream => "unknown — no upstream".to_string(),
            Unpushed::Unknown(reason) => format!("unknown — {reason}"),
        }
    }

    /// Would removing the worktree risk losing commits?
    pub fn is_risky(&self) -> bool {
        !matches!(self, Unpushed::Count(0))
    }
}

/// Shell and multiplexer process names that mean "nothing is running".
const SHELLS: [&str; 9] = [
    "bash", "zsh", "fish", "sh", "dash", "ksh", "tcsh", "csh", "tmux",
];

/// Is `command` a plain shell? Compared on the basename, and a leading `-`
/// (login shells appear as `-bash`) is ignored.
pub fn is_shell(command: &str) -> bool {
    let name = command
        .rsplit('/')
        .next()
        .unwrap_or(command)
        .trim_start_matches('-');
    SHELLS.contains(&name)
}

/// Panes running something other than a shell.
///
/// Only ever used to warn *more*: a pane that looks idle never removes a
/// confirmation step.
pub fn busy_panes(panes: &[PaneInfo]) -> Vec<&PaneInfo> {
    panes
        .iter()
        .filter(|pane| !is_shell(&pane.command))
        .collect()
}

/// Everything gathered about a worktree before offering to remove it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovalInputs {
    pub worktree_path: PathBuf,
    pub branch: Option<String>,
    pub is_main: bool,
    pub is_locked: bool,
    pub lock_reason: Option<String>,
    /// `None` when the status could not be read (the directory is gone).
    pub status: Option<StatusSummary>,
    pub unpushed: Unpushed,
    /// tmux session name, when one exists on the private server.
    pub session: Option<String>,
    pub panes: Vec<PaneInfo>,
}

impl RemovalInputs {
    /// Inputs for a worktree with nothing gathered yet.
    pub fn new(worktree_path: impl Into<PathBuf>) -> Self {
        Self {
            worktree_path: worktree_path.into(),
            branch: None,
            is_main: false,
            is_locked: false,
            lock_reason: None,
            status: None,
            unpushed: Unpushed::Unknown("not checked".into()),
            session: None,
            panes: Vec::new(),
        }
    }
}

/// One line of the report, with how alarming it is.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Context: true but harmless.
    Note,
    /// Something would be lost or interrupted.
    Warning,
    /// The operation is not offered at all.
    Blocker,
}

/// A fact the dialog must display before the user decides.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Finding {
    pub severity: Severity,
    pub text: String,
}

impl Finding {
    fn note(text: impl Into<String>) -> Self {
        Self {
            severity: Severity::Note,
            text: text.into(),
        }
    }
    fn warning(text: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warning,
            text: text.into(),
        }
    }
    fn blocker(text: impl Into<String>) -> Self {
        Self {
            severity: Severity::Blocker,
            text: text.into(),
        }
    }
}

/// The assembled report shown by the removal dialog.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RemovalReport {
    pub worktree_path: PathBuf,
    pub branch: Option<String>,
    pub findings: Vec<Finding>,
    /// May "remove git worktree" be offered? False for the main worktree,
    /// which git itself cannot remove.
    pub can_remove_worktree: bool,
    /// May "delete branch" be offered? Only when there is a branch at all.
    pub can_delete_branch: bool,
    /// Is there a session to close?
    pub can_close_session: bool,
    /// Would removal destroy work that exists nowhere else?
    pub loses_work: bool,
}

impl RemovalReport {
    pub fn has_blockers(&self) -> bool {
        self.findings
            .iter()
            .any(|f| f.severity == Severity::Blocker)
    }

    pub fn warnings(&self) -> impl Iterator<Item = &Finding> {
        self.findings
            .iter()
            .filter(|f| f.severity == Severity::Warning)
    }
}

/// Assemble the report. Pure: no git, no tmux, no filesystem.
pub fn assemble(inputs: &RemovalInputs) -> RemovalReport {
    let mut findings = Vec::new();
    let mut loses_work = false;

    if inputs.is_main {
        findings.push(Finding::blocker(
            "This is the main worktree. Git cannot remove it, and Grove will not try.",
        ));
    }

    if inputs.is_locked {
        findings.push(Finding::warning(match &inputs.lock_reason {
            Some(reason) => format!("The worktree is locked: {reason}"),
            None => "The worktree is locked.".to_string(),
        }));
    }

    match &inputs.status {
        Some(status) => {
            if let Some(operation) = status.operation {
                findings.push(Finding::warning(format!(
                    "A git operation is in progress ({}).",
                    operation.label()
                )));
                loses_work = true;
            }
            if status.conflicted > 0 {
                findings.push(Finding::warning(format!(
                    "{} file(s) with unresolved conflicts.",
                    status.conflicted
                )));
                loses_work = true;
            }
            if status.staged > 0 || status.modified > 0 {
                findings.push(Finding::warning(format!(
                    "Uncommitted changes: {} staged, {} modified.",
                    status.staged, status.modified
                )));
                loses_work = true;
            }
            if status.untracked > 0 {
                findings.push(Finding::warning(format!(
                    "{} untracked file(s) — these are not in git at all.",
                    status.untracked
                )));
                loses_work = true;
            }
            if status.is_clean() {
                findings.push(Finding::note("The working tree is clean."));
            }
        }
        None => findings.push(Finding::warning(
            "The working tree status could not be read; assume there are changes.",
        )),
    }

    let unpushed = format!("Unpushed commits: {}.", inputs.unpushed.describe());
    if inputs.unpushed.is_risky() {
        findings.push(Finding::warning(unpushed));
        if matches!(inputs.unpushed, Unpushed::Count(n) if n > 0) {
            loses_work = true;
        }
    } else {
        findings.push(Finding::note(unpushed));
    }

    match &inputs.session {
        Some(session) => {
            let busy = busy_panes(&inputs.panes);
            if busy.is_empty() {
                findings.push(Finding::note(format!(
                    "tmux session {session} is open with {} idle pane(s).",
                    inputs.panes.len()
                )));
            } else {
                let list = busy
                    .iter()
                    .map(|pane| format!("{} (pid {})", pane.command, pane.pid))
                    .collect::<Vec<_>>()
                    .join(", ");
                findings.push(Finding::warning(format!(
                    "tmux session {session} is running: {list}."
                )));
            }
        }
        None => findings.push(Finding::note("There is no tmux session for this worktree.")),
    }

    RemovalReport {
        worktree_path: inputs.worktree_path.clone(),
        branch: inputs.branch.clone(),
        can_remove_worktree: !inputs.is_main,
        can_delete_branch: inputs.branch.is_some(),
        can_close_session: inputs.session.is_some(),
        loses_work,
        findings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::status::Operation;

    fn clean_status() -> StatusSummary {
        StatusSummary {
            branch: Some("feature/auth".into()),
            upstream: Some("origin/feature/auth".into()),
            ahead: Some(0),
            behind: Some(0),
            ..StatusSummary::default()
        }
    }

    fn safe_inputs() -> RemovalInputs {
        RemovalInputs {
            branch: Some("feature/auth".into()),
            status: Some(clean_status()),
            unpushed: Unpushed::Count(0),
            ..RemovalInputs::new("/home/u/wt/auth")
        }
    }

    fn pane(session: &str, pid: u32, command: &str) -> PaneInfo {
        PaneInfo {
            session: session.into(),
            pid,
            command: command.into(),
            window_index: 0,
            window_name: "shell".into(),
            window_active: true,
            window_bell: false,
            window_activity_epoch: None,
            active: true,
            title: String::new(),
        }
    }

    fn texts(report: &RemovalReport) -> String {
        report
            .findings
            .iter()
            .map(|f| f.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_clean_pushed_sessionless_worktree_is_safe() {
        let report = assemble(&safe_inputs());
        assert!(!report.has_blockers());
        assert!(!report.loses_work);
        assert_eq!(report.warnings().count(), 0);
        assert!(report.can_remove_worktree);
        assert!(report.can_delete_branch);
        assert!(!report.can_close_session);
        assert!(texts(&report).contains("The working tree is clean."));
        assert!(texts(&report).contains("every commit is on the upstream"));
        assert!(texts(&report).contains("no tmux session"));
    }

    /// The safety invariant with no exception: the main worktree is never
    /// removable, whatever else the report says.
    #[test]
    fn the_main_worktree_can_never_be_removed() {
        let report = assemble(&RemovalInputs {
            is_main: true,
            ..safe_inputs()
        });
        assert!(!report.can_remove_worktree);
        assert!(report.has_blockers());
        assert!(texts(&report).contains("main worktree"));
    }

    #[test]
    fn uncommitted_and_untracked_files_are_reported_separately() {
        let report = assemble(&RemovalInputs {
            status: Some(StatusSummary {
                staged: 2,
                modified: 3,
                untracked: 4,
                ..clean_status()
            }),
            ..safe_inputs()
        });
        let text = texts(&report);
        assert!(text.contains("2 staged, 3 modified"));
        assert!(text.contains("4 untracked file(s)"));
        assert!(report.loses_work);
        assert!(!report.has_blockers(), "dirty is a warning, not a blocker");
        assert!(report.can_remove_worktree);
    }

    #[test]
    fn conflicts_and_an_in_progress_operation_are_warnings() {
        let report = assemble(&RemovalInputs {
            status: Some(StatusSummary {
                conflicted: 2,
                operation: Some(Operation::Merge),
                ..clean_status()
            }),
            ..safe_inputs()
        });
        let text = texts(&report);
        assert!(text.contains("MERGING"));
        assert!(text.contains("2 file(s) with unresolved conflicts"));
        assert!(report.loses_work);
    }

    #[test]
    fn an_unreadable_status_is_treated_as_dangerous() {
        let report = assemble(&RemovalInputs {
            status: None,
            ..safe_inputs()
        });
        assert!(texts(&report).contains("could not be read"));
        assert_eq!(report.warnings().count(), 1);
    }

    #[test]
    fn unpushed_commits_are_a_warning_and_a_loss() {
        let report = assemble(&RemovalInputs {
            unpushed: Unpushed::Count(3),
            ..safe_inputs()
        });
        assert!(texts(&report).contains("3 commits not on the upstream"));
        assert!(report.loses_work);
    }

    /// "Unknown" must never be rendered as "none": no upstream means Grove
    /// cannot tell, and the dialog has to say so.
    #[test]
    fn no_upstream_is_unknown_not_zero() {
        let report = assemble(&RemovalInputs {
            unpushed: Unpushed::NoUpstream,
            ..safe_inputs()
        });
        assert!(texts(&report).contains("unknown — no upstream"));
        assert_eq!(report.warnings().count(), 1);
        // Unknown is not proof of loss, but it is not safe either.
        assert!(!report.loses_work);
    }

    #[test]
    fn an_unavailable_count_keeps_gits_reason() {
        let report = assemble(&RemovalInputs {
            unpushed: Unpushed::Unknown("HEAD is detached".into()),
            ..safe_inputs()
        });
        assert!(texts(&report).contains("unknown — HEAD is detached"));
    }

    #[test]
    fn unpushed_descriptions_are_singular_and_plural() {
        assert_eq!(
            Unpushed::Count(0).describe(),
            "none — every commit is on the upstream"
        );
        assert_eq!(
            Unpushed::Count(1).describe(),
            "1 commit not on the upstream"
        );
        assert_eq!(
            Unpushed::Count(2).describe(),
            "2 commits not on the upstream"
        );
        assert!(!Unpushed::Count(0).is_risky());
        assert!(Unpushed::Count(1).is_risky());
        assert!(Unpushed::NoUpstream.is_risky());
    }

    #[test]
    fn a_locked_worktree_is_a_warning_not_a_blocker() {
        let report = assemble(&RemovalInputs {
            is_locked: true,
            lock_reason: Some("on a removable drive".into()),
            ..safe_inputs()
        });
        assert!(texts(&report).contains("locked: on a removable drive"));
        assert!(!report.has_blockers());
        assert!(report.can_remove_worktree, "git's own refusal decides");
    }

    #[test]
    fn a_lock_without_a_reason_still_reports_the_lock() {
        let report = assemble(&RemovalInputs {
            is_locked: true,
            ..safe_inputs()
        });
        assert!(texts(&report).contains("The worktree is locked."));
    }

    #[test]
    fn running_processes_are_listed_with_their_pids() {
        let report = assemble(&RemovalInputs {
            session: Some("wt-a1b2c3".into()),
            panes: vec![
                pane("wt-a1b2c3", 4242, "bash"),
                pane("wt-a1b2c3", 4343, "cargo"),
            ],
            ..safe_inputs()
        });
        let text = texts(&report);
        assert!(text.contains("cargo (pid 4343)"));
        assert!(
            !text.contains("bash (pid 4242)"),
            "an idle shell is not busy"
        );
        assert!(report.can_close_session);
        assert_eq!(report.warnings().count(), 1);
    }

    #[test]
    fn a_session_of_idle_shells_is_only_a_note() {
        let report = assemble(&RemovalInputs {
            session: Some("wt-a1b2c3".into()),
            panes: vec![pane("wt-a1b2c3", 1, "-zsh")],
            ..safe_inputs()
        });
        assert_eq!(report.warnings().count(), 0);
        assert!(texts(&report).contains("1 idle pane(s)"));
        assert!(report.can_close_session);
    }

    #[test]
    fn shells_are_recognised_by_basename_and_login_dash() {
        for shell in ["bash", "-bash", "/usr/bin/zsh", "fish", "sh", "tmux"] {
            assert!(is_shell(shell), "{shell} should count as a shell");
        }
        for busy in ["cargo", "claude", "vim", "node", "python3", "bashful"] {
            assert!(!is_shell(busy), "{busy} should count as running work");
        }
    }

    #[test]
    fn a_detached_worktree_offers_no_branch_deletion() {
        let report = assemble(&RemovalInputs {
            branch: None,
            ..safe_inputs()
        });
        assert!(!report.can_delete_branch);
        assert!(report.can_remove_worktree);
    }

    #[test]
    fn every_finding_is_kept_for_display() {
        let report = assemble(&RemovalInputs {
            is_main: true,
            is_locked: true,
            status: Some(StatusSummary {
                staged: 1,
                untracked: 1,
                ..clean_status()
            }),
            unpushed: Unpushed::Count(2),
            session: Some("wt-a1b2c3".into()),
            panes: vec![pane("wt-a1b2c3", 9, "claude")],
            ..safe_inputs()
        });
        // main + locked + staged + untracked + unpushed + running session
        assert_eq!(report.findings.len(), 6);
        assert!(report.has_blockers());
        assert!(report.loses_work);
    }
}
