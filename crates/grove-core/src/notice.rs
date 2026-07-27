//! What the last `grove notify` said, per worktree and per window.
//!
//! The status *state machine* lives in [`crate::status`]; this is the other
//! half of a report — the human sentence that came with it, and which window
//! it came from. They are kept apart deliberately: a status is recomputed from
//! tmux on every poll and must never be remembered, whereas a message is only
//! ever true because an agent said it, and there is nothing to re-derive it
//! from.
//!
//! Nothing here is durable. A message survives a poll, not a restart: the
//! durable half of attention is the `@grove_attention` tmux option, which says
//! *that* a session wants the user and never claims to know why.
//!
//! Windows are a refinement, never a contradiction. A report that names a
//! window is also the worktree's most recent report, because a folded worktree
//! row still has to be able to say what its agent said.

use std::collections::{BTreeMap, HashMap};

use crate::ipc::Notification;
use crate::status::SessionStatus;

/// One report, as the UI shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub state: SessionStatus,
    pub message: Option<String>,
}

/// Everything reported about one worktree.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct WorktreeNotices {
    /// The most recent report, whichever window it named.
    latest: Option<Notice>,
    /// The most recent report from each window that named itself.
    windows: BTreeMap<u32, Notice>,
}

/// The last report from every worktree Grove has heard from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Notices {
    entries: HashMap<String, WorktreeNotices>,
}

impl Notices {
    /// Fold one report in.
    pub fn record(&mut self, notification: &Notification) {
        let notice = Notice {
            state: notification.state,
            message: notification.message.clone(),
        };
        let entry = self
            .entries
            .entry(notification.worktree_id.clone())
            .or_default();
        if let Some(window) = notification.window {
            entry.windows.insert(window, notice.clone());
        }
        entry.latest = Some(notice);
    }

    /// The worktree's most recent report, whichever window made it.
    pub fn worktree(&self, worktree_id: &str) -> Option<&Notice> {
        self.entries.get(worktree_id)?.latest.as_ref()
    }

    /// What one window last reported about itself.
    pub fn window(&self, worktree_id: &str, index: u32) -> Option<&Notice> {
        self.entries.get(worktree_id)?.windows.get(&index)
    }

    /// Has any window of this worktree spoken for itself?
    ///
    /// The UI needs this to tell "this window has nothing to report" from
    /// "nothing here reports per window at all". In the second case a window
    /// row falls back to its worktree's status, which is how every row behaved
    /// before windows could report; in the first, silence is information and
    /// the row shows it as silence.
    pub fn has_windows(&self, worktree_id: &str) -> bool {
        self.entries
            .get(worktree_id)
            .is_some_and(|entry| !entry.windows.is_empty())
    }

    /// Every window that has reported for this worktree, in index order.
    pub fn windows(&self, worktree_id: &str) -> impl Iterator<Item = (u32, &Notice)> {
        self.entries
            .get(worktree_id)
            .into_iter()
            .flat_map(|entry| entry.windows.iter().map(|(index, notice)| (*index, notice)))
    }

    /// Forget a worktree's reports.
    ///
    /// Called when the user opens the session, which is also what clears the
    /// attention latch: the messages explained a state the user has now seen.
    /// Bookkeeping only — this closes no session and removes nothing.
    pub fn clear(&mut self, worktree_id: &str) -> bool {
        self.entries.remove(worktree_id).is_some()
    }

    /// Drop reports for worktrees that are no longer listed, so a long-running
    /// Grove cannot accumulate them without bound.
    pub fn retain_ids<F: Fn(&str) -> bool>(&mut self, keep: F) {
        self.entries.retain(|id, _| keep(id));
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(id: &str, state: SessionStatus, message: &str) -> Notification {
        Notification::new(id, state).with_message(Some(message.to_string()))
    }

    #[test]
    fn a_report_without_a_window_is_the_worktrees_own() {
        let mut notices = Notices::default();
        notices.record(&report(
            "a1b2c3",
            SessionStatus::Attention,
            "needs permission",
        ));
        assert_eq!(
            notices.worktree("a1b2c3").map(|n| n.message.as_deref()),
            Some(Some("needs permission"))
        );
        assert!(!notices.has_windows("a1b2c3"));
        assert_eq!(notices.window("a1b2c3", 0), None);
    }

    /// A window's report is both: the window said it, and it is also the last
    /// thing the worktree said — which is what a folded row shows.
    #[test]
    fn a_windows_report_is_also_the_worktrees_latest() {
        let mut notices = Notices::default();
        notices.record(
            &report("a1b2c3", SessionStatus::Attention, "needs permission").with_window(Some(1)),
        );
        assert_eq!(
            notices.window("a1b2c3", 1).map(|n| n.state),
            Some(SessionStatus::Attention)
        );
        assert_eq!(
            notices.worktree("a1b2c3").map(|n| n.message.as_deref()),
            Some(Some("needs permission"))
        );
        assert!(notices.has_windows("a1b2c3"));
    }

    #[test]
    fn windows_of_one_worktree_report_independently() {
        let mut notices = Notices::default();
        notices.record(
            &report("a1b2c3", SessionStatus::Attention, "permission?").with_window(Some(1)),
        );
        notices
            .record(&report("a1b2c3", SessionStatus::Working, "cargo test").with_window(Some(2)));
        assert_eq!(
            notices.window("a1b2c3", 1).map(|n| n.state),
            Some(SessionStatus::Attention),
            "window 2 reporting says nothing about window 1"
        );
        assert_eq!(
            notices.window("a1b2c3", 2).map(|n| n.message.as_deref()),
            Some(Some("cargo test"))
        );
        assert_eq!(
            notices
                .windows("a1b2c3")
                .map(|(index, _)| index)
                .collect::<Vec<_>>(),
            vec![1, 2],
            "in index order, as the tree lists them"
        );
    }

    #[test]
    fn a_newer_report_replaces_the_windows_previous_one() {
        let mut notices = Notices::default();
        notices.record(
            &report("a1b2c3", SessionStatus::Attention, "permission?").with_window(Some(1)),
        );
        notices
            .record(&report("a1b2c3", SessionStatus::Working, "carrying on").with_window(Some(1)));
        assert_eq!(
            notices.window("a1b2c3", 1).map(|n| n.message.as_deref()),
            Some(Some("carrying on"))
        );
    }

    #[test]
    fn worktrees_do_not_share_reports() {
        let mut notices = Notices::default();
        notices.record(&report("aaaaaa", SessionStatus::Attention, "mine").with_window(Some(1)));
        assert_eq!(notices.worktree("bbbbbb"), None);
        assert_eq!(notices.window("bbbbbb", 1), None);
        assert!(!notices.has_windows("bbbbbb"));
    }

    #[test]
    fn clearing_a_worktree_forgets_its_windows_too() {
        let mut notices = Notices::default();
        notices.record(
            &report("a1b2c3", SessionStatus::Attention, "permission?").with_window(Some(1)),
        );
        notices.record(&report("ddeeff", SessionStatus::Working, "busy"));
        assert!(notices.clear("a1b2c3"));
        assert!(!notices.clear("a1b2c3"), "already cleared");
        assert_eq!(notices.window("a1b2c3", 1), None);
        assert!(notices.worktree("ddeeff").is_some(), "only that worktree");
    }

    #[test]
    fn retain_drops_worktrees_that_are_gone() {
        let mut notices = Notices::default();
        notices.record(&report("aaaaaa", SessionStatus::Working, "one"));
        notices.record(&report("bbbbbb", SessionStatus::Working, "two"));
        notices.retain_ids(|id| id == "aaaaaa");
        assert!(notices.worktree("aaaaaa").is_some());
        assert_eq!(notices.worktree("bbbbbb"), None);
        notices.retain_ids(|_| false);
        assert!(notices.is_empty());
    }

    /// A report with no message still marks the window: "this one is working"
    /// is worth showing even when the agent had nothing to say about it.
    #[test]
    fn a_report_without_a_message_still_marks_its_window() {
        let mut notices = Notices::default();
        notices.record(&Notification::new("a1b2c3", SessionStatus::Working).with_window(Some(3)));
        assert_eq!(
            notices.window("a1b2c3", 3),
            Some(&Notice {
                state: SessionStatus::Working,
                message: None
            })
        );
    }
}
