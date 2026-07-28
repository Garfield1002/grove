//! The working / idle / attention state machine (DESIGN.md §6).
//!
//! This module is pure: it turns a snapshot of signals about one session into
//! a status, and remembers the attention latch across polls. Everything that
//! talks to tmux lives in [`crate::tmux`]; the poller gathers signals there
//! and feeds them here.
//!
//! Two rules from CLAUDE.md are load-bearing and encoded in the tests:
//!
//! - Precedence is `attention > working > idle`.
//! - Attention **latches**: once raised it survives later polls until the user
//!   opens the session. Activity does not clear it — an agent that keeps
//!   printing while waiting for a permission answer still needs attention.
//!
//! Attention is never inferred from a process name or from terminal contents.
//! It comes from an explicit signal only: a `grove notify` call (durable in
//! the `@grove_attention` session option), or a tmux bell when the user has
//! opted into that.

use std::collections::HashSet;
use std::time::Duration;

/// How long a session stays "working" after its last pane activity.
pub const DEFAULT_WORKING_WINDOW: Duration = Duration::from_secs(10);

/// Process names that mark a session as working whenever they are running in
/// one of its panes, regardless of how quiet they are.
pub const DEFAULT_AGENT_COMMANDS: &[&str] = &["claude", "aider", "codex", "goose"];

/// The status of a session that exists.
///
/// A worktree with no session has no status at all; that case is
/// [`crate::model::SessionPresence::None`], not a variant here.
///
/// The `Ord` derive is the precedence rule: `Idle < Working < Attention`, so
/// merging two observations is `max`. Keep the variants in this order.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SessionStatus {
    /// The session exists but nothing has happened recently. Idle does not
    /// mean "finished" — only that Grove saw no activity.
    #[default]
    Idle,
    /// Recent pane activity, or a known agent process is running.
    Working,
    /// An explicit signal says the user is needed.
    Attention,
}

impl SessionStatus {
    /// Short label for a worktree row's status pill.
    pub fn label(self) -> &'static str {
        match self {
            SessionStatus::Idle => "idle",
            SessionStatus::Working => "working",
            SessionStatus::Attention => "attention",
        }
    }

    /// Parse the `--state` value of `grove notify`.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "idle" => Some(SessionStatus::Idle),
            "working" => Some(SessionStatus::Working),
            "attention" => Some(SessionStatus::Attention),
            _ => None,
        }
    }
}

/// Tunables for [`classify`]. Built from `config.toml`'s `[status]` section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusPolicy {
    /// A session is working if its last activity is no older than this.
    pub working_window: Duration,
    /// Process names that mean "an agent is running here".
    pub agent_commands: Vec<String>,
    /// Whether a tmux bell raises attention. Off by default: bells are noisy
    /// and the explicit `grove notify` path is the reliable one.
    pub bell_is_attention: bool,
}

impl Default for StatusPolicy {
    fn default() -> Self {
        Self {
            working_window: DEFAULT_WORKING_WINDOW,
            agent_commands: DEFAULT_AGENT_COMMANDS
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            bell_is_attention: false,
        }
    }
}

impl StatusPolicy {
    /// Does this pane command count as a running agent?
    ///
    /// tmux reports `pane_current_command` as a bare command name already, but
    /// wrappers can leave a path in it, so compare on the last path component
    /// and ignore case.
    fn is_agent_command(&self, command: &str) -> bool {
        let name = command.rsplit(['/', '\\']).next().unwrap_or(command).trim();
        if name.is_empty() {
            return false;
        }
        self.agent_commands
            .iter()
            .any(|agent| agent.eq_ignore_ascii_case(name))
    }

    /// Is an agent already running in a session, given its pane commands?
    ///
    /// The question a restart asks before resuming anything: an agent someone
    /// started by hand, in the shell window rather than Grove's `agent` one,
    /// counts just as much as one Grove launched itself. Resuming beside it
    /// would put two processes on the same conversation.
    pub fn agent_running(&self, pane_commands: &[String]) -> bool {
        pane_commands
            .iter()
            .any(|command| self.is_agent_command(command))
    }
}

/// One poll's worth of signals about a single session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionSignals {
    /// Time since the session's last activity (`#{session_activity}`), or
    /// `None` when tmux did not report a usable timestamp.
    pub activity_age: Option<Duration>,
    /// `pane_current_command` for every pane of the session.
    pub pane_commands: Vec<String>,
    /// The durable `@grove_attention` option is set on the session.
    pub attention_flag: bool,
    /// tmux is flagging a bell for this session.
    pub bell: bool,
    /// RAM and CPU of the session's scoped agents, when resource accounting
    /// is on and the scope could be read. `None` means "no figure", which the
    /// UI shows as nothing rather than as zero.
    pub usage: Option<crate::cgroup::Usage>,
    /// The session's windows, carried along because the poll already lists
    /// every pane. Not a status signal: the tree renders these as child rows.
    pub windows: Vec<crate::tmux::WindowInfo>,
}

/// Age of an activity timestamp, in whole seconds since the epoch.
///
/// tmux clocks and ours are the same clock, but a session's activity stamp can
/// still read a second or two into the future across a poll boundary; that is
/// treated as "just now" rather than as a huge age.
pub fn activity_age(now_epoch: u64, activity_epoch: u64) -> Duration {
    Duration::from_secs(now_epoch.saturating_sub(activity_epoch))
}

/// Whether a session's agent speaks for itself through `grove notify`.
///
/// This picks which of the two "working" rules applies. The process-name rule
/// — a running `claude` means work — is a stand-in for an agent that cannot
/// say what it is doing. It is also permanently true: an agent sitting at its
/// prompt with nothing to do is the same process as one mid-turn, so a session
/// judged that way can never be reported as finished. Where the agent does
/// report, the stand-in is worse than the thing it stands in for and is
/// dropped: pane activity decides, which a waiting agent stops producing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Reporting {
    /// Nothing in this session has ever reported.
    #[default]
    Silent,
    /// Its agent has reported at least once, so it will report again.
    Speaks,
}

/// The status implied by one poll of a session that has never reported.
pub fn classify(signals: &SessionSignals, policy: &StatusPolicy) -> SessionStatus {
    classify_as(signals, policy, Reporting::Silent)
}

/// The status implied by one poll, ignoring any latch.
pub fn classify_as(
    signals: &SessionSignals,
    policy: &StatusPolicy,
    reporting: Reporting,
) -> SessionStatus {
    if signals.attention_flag || (policy.bell_is_attention && signals.bell) {
        return SessionStatus::Attention;
    }
    let recently_active = signals
        .activity_age
        .is_some_and(|age| age <= policy.working_window);
    let agent_running =
        reporting == Reporting::Silent && policy.agent_running(&signals.pane_commands);
    if recently_active || agent_running {
        return SessionStatus::Working;
    }
    SessionStatus::Idle
}

/// What one poll concluded about a session: its status, and the resource
/// figures for its scoped agents when there are any.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SessionReport {
    pub status: SessionStatus,
    /// RAM and CPU time of the session's Grove scopes.
    pub usage: Option<crate::cgroup::Usage>,
    /// CPU percentage since the previous poll. Absent on the first poll of a
    /// session, and whenever the counter could not be compared.
    pub cpu_percent: Option<f32>,
}

impl SessionReport {
    pub fn new(status: SessionStatus) -> Self {
        Self {
            status,
            usage: None,
            cpu_percent: None,
        }
    }

    /// The resource line for a row: `64%  1.4G`, CPU first because it is what
    /// moves. Memory alone until a rate is known, since CPU needs two polls.
    ///
    /// `None` when there is no scoped agent — which is not the same as zero,
    /// and must not be rendered as "0M".
    pub fn resource_label(&self) -> Option<String> {
        let usage = self.usage?;
        Some(match self.cpu_percent {
            Some(cpu) => format!("{cpu:.0}%  {}", usage.memory_label()),
            None => usage.memory_label(),
        })
    }
}

/// Tracks the attention latch across polls, keyed by worktree id.
///
/// The engine holds no session state beyond the latch: everything else is
/// recomputed from each poll, so a dropped poll cannot leave a stale status.
#[derive(Debug, Clone, Default)]
pub struct StatusEngine {
    policy: StatusPolicy,
    latched: HashSet<String>,
    /// Worktrees whose agent has reported at least once this run. See
    /// [`Reporting`]: it decides whether the process-name rule applies.
    speaks: HashSet<String>,
}

impl StatusEngine {
    pub fn new(policy: StatusPolicy) -> Self {
        Self {
            policy,
            latched: HashSet::new(),
            speaks: HashSet::new(),
        }
    }

    pub fn policy(&self) -> &StatusPolicy {
        &self.policy
    }

    /// Replace the policy, e.g. after the user edits Settings.
    pub fn set_policy(&mut self, policy: StatusPolicy) {
        self.policy = policy;
    }

    /// Fold one poll of a session into the engine and return what to display.
    pub fn observe(&mut self, worktree_id: &str, signals: &SessionSignals) -> SessionStatus {
        let observed = classify_as(signals, &self.policy, self.reporting(worktree_id));
        if observed == SessionStatus::Attention {
            self.latched.insert(worktree_id.to_string());
            return SessionStatus::Attention;
        }
        if self.latched.contains(worktree_id) {
            return SessionStatus::Attention;
        }
        observed
    }

    /// Record an explicit `grove notify` report.
    ///
    /// Only `attention` is sticky. A `working` or `idle` report is a hint that
    /// the next poll will confirm or replace, and it does **not** clear a
    /// latch: an agent reporting progress while a permission prompt is still
    /// open must not silently drop the user's attention marker. Only opening
    /// the session clears it.
    /// Any report at all, of any state, also marks the session as one that
    /// speaks for itself — which is what stops the poller from reading its
    /// agent's process as work forever after it has said it is finished.
    pub fn notify(&mut self, worktree_id: &str, state: SessionStatus) {
        self.speaks.insert(worktree_id.to_string());
        if state == SessionStatus::Attention {
            self.latched.insert(worktree_id.to_string());
        }
    }

    /// Whether this worktree's agent has reported for itself this run.
    pub fn reporting(&self, worktree_id: &str) -> Reporting {
        match self.speaks.contains(worktree_id) {
            true => Reporting::Speaks,
            false => Reporting::Silent,
        }
    }

    /// The user opened this session: clear the latch.
    ///
    /// Returns true when a latch was actually cleared, which is the caller's
    /// signal to also unset the durable `@grove_attention` tmux option.
    pub fn opened(&mut self, worktree_id: &str) -> bool {
        self.latched.remove(worktree_id)
    }

    /// Is attention currently latched for this worktree?
    pub fn is_latched(&self, worktree_id: &str) -> bool {
        self.latched.contains(worktree_id)
    }

    /// Drop all memory of a worktree, after its session is closed or its
    /// worktree removed. This is bookkeeping, never a deletion trigger.
    pub fn forget(&mut self, worktree_id: &str) {
        self.latched.remove(worktree_id);
        self.speaks.remove(worktree_id);
    }

    /// Drop latches for worktrees that no longer exist, so the set cannot grow
    /// without bound over a long-running session.
    pub fn retain_ids<F: Fn(&str) -> bool>(&mut self, keep: F) {
        self.latched.retain(|id| keep(id));
        self.speaks.retain(|id| keep(id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signals_idle() -> SessionSignals {
        SessionSignals {
            activity_age: Some(Duration::from_secs(600)),
            ..SessionSignals::default()
        }
    }

    #[test]
    fn precedence_is_attention_over_working_over_idle() {
        assert!(SessionStatus::Attention > SessionStatus::Working);
        assert!(SessionStatus::Working > SessionStatus::Idle);
        assert_eq!(
            SessionStatus::Idle.max(SessionStatus::Attention),
            SessionStatus::Attention
        );
    }

    #[test]
    fn quiet_session_is_idle() {
        assert_eq!(
            classify(&signals_idle(), &StatusPolicy::default()),
            SessionStatus::Idle
        );
    }

    #[test]
    fn recent_activity_is_working() {
        let signals = SessionSignals {
            activity_age: Some(Duration::from_secs(3)),
            ..SessionSignals::default()
        };
        assert_eq!(
            classify(&signals, &StatusPolicy::default()),
            SessionStatus::Working
        );
    }

    #[test]
    fn activity_exactly_at_the_window_still_counts() {
        let signals = SessionSignals {
            activity_age: Some(DEFAULT_WORKING_WINDOW),
            ..SessionSignals::default()
        };
        assert_eq!(
            classify(&signals, &StatusPolicy::default()),
            SessionStatus::Working
        );
    }

    #[test]
    fn activity_past_the_window_is_idle() {
        let signals = SessionSignals {
            activity_age: Some(DEFAULT_WORKING_WINDOW + Duration::from_secs(1)),
            ..SessionSignals::default()
        };
        assert_eq!(
            classify(&signals, &StatusPolicy::default()),
            SessionStatus::Idle
        );
    }

    #[test]
    fn missing_activity_timestamp_is_not_working() {
        let signals = SessionSignals::default();
        assert_eq!(
            classify(&signals, &StatusPolicy::default()),
            SessionStatus::Idle
        );
    }

    #[test]
    fn a_quiet_agent_process_is_still_working() {
        let signals = SessionSignals {
            activity_age: Some(Duration::from_secs(3600)),
            pane_commands: vec!["bash".into(), "claude".into()],
            ..SessionSignals::default()
        };
        assert_eq!(
            classify(&signals, &StatusPolicy::default()),
            SessionStatus::Working
        );
    }

    #[test]
    fn agent_match_ignores_case_and_leading_path() {
        let policy = StatusPolicy::default();
        assert!(policy.is_agent_command("claude"));
        assert!(policy.is_agent_command("Claude"));
        assert!(policy.is_agent_command("/usr/local/bin/claude"));
        assert!(!policy.is_agent_command("claudia"));
        assert!(!policy.is_agent_command(""));
        assert!(!policy.is_agent_command("bash"));
    }

    #[test]
    fn an_agent_process_alone_never_raises_attention() {
        // CLAUDE.md: attention is never inferred from a process name.
        let signals = SessionSignals {
            pane_commands: vec!["claude".into()],
            ..SessionSignals::default()
        };
        assert_eq!(
            classify(&signals, &StatusPolicy::default()),
            SessionStatus::Working
        );
    }

    #[test]
    fn attention_flag_beats_activity() {
        let signals = SessionSignals {
            activity_age: Some(Duration::from_secs(1)),
            pane_commands: vec!["claude".into()],
            attention_flag: true,
            bell: false,
            usage: None,
            windows: Vec::new(),
        };
        assert_eq!(
            classify(&signals, &StatusPolicy::default()),
            SessionStatus::Attention
        );
    }

    #[test]
    fn bell_raises_attention_only_when_opted_in() {
        let signals = SessionSignals {
            bell: true,
            ..signals_idle()
        };
        assert_eq!(
            classify(&signals, &StatusPolicy::default()),
            SessionStatus::Idle
        );
        let opted_in = StatusPolicy {
            bell_is_attention: true,
            ..StatusPolicy::default()
        };
        assert_eq!(classify(&signals, &opted_in), SessionStatus::Attention);
    }

    #[test]
    fn custom_working_window_is_honoured() {
        let policy = StatusPolicy {
            working_window: Duration::from_secs(60),
            ..StatusPolicy::default()
        };
        let signals = SessionSignals {
            activity_age: Some(Duration::from_secs(30)),
            ..SessionSignals::default()
        };
        assert_eq!(classify(&signals, &policy), SessionStatus::Working);
    }

    #[test]
    fn activity_age_clamps_a_future_timestamp_to_zero() {
        assert_eq!(activity_age(100, 40), Duration::from_secs(60));
        assert_eq!(activity_age(100, 100), Duration::ZERO);
        assert_eq!(activity_age(100, 140), Duration::ZERO);
    }

    #[test]
    fn attention_latches_across_later_quiet_polls() {
        let mut engine = StatusEngine::default();
        let raised = SessionSignals {
            attention_flag: true,
            ..SessionSignals::default()
        };
        assert_eq!(engine.observe("abc123", &raised), SessionStatus::Attention);
        // The durable option was consumed; later polls see nothing special.
        assert_eq!(
            engine.observe("abc123", &signals_idle()),
            SessionStatus::Attention
        );
        assert!(engine.is_latched("abc123"));
    }

    #[test]
    fn new_activity_does_not_clear_the_latch() {
        let mut engine = StatusEngine::default();
        engine.notify("abc123", SessionStatus::Attention);
        let busy = SessionSignals {
            activity_age: Some(Duration::from_secs(1)),
            ..SessionSignals::default()
        };
        assert_eq!(engine.observe("abc123", &busy), SessionStatus::Attention);
    }

    #[test]
    fn opening_the_session_clears_the_latch() {
        let mut engine = StatusEngine::default();
        engine.notify("abc123", SessionStatus::Attention);
        assert!(engine.opened("abc123"));
        assert_eq!(
            engine.observe("abc123", &signals_idle()),
            SessionStatus::Idle
        );
        // Opening again has nothing to clear.
        assert!(!engine.opened("abc123"));
    }

    #[test]
    fn a_working_report_does_not_clear_a_latch() {
        let mut engine = StatusEngine::default();
        engine.notify("abc123", SessionStatus::Attention);
        engine.notify("abc123", SessionStatus::Working);
        engine.notify("abc123", SessionStatus::Idle);
        assert!(engine.is_latched("abc123"));
    }

    #[test]
    fn latches_are_per_worktree() {
        let mut engine = StatusEngine::default();
        engine.notify("aaaaaa", SessionStatus::Attention);
        assert_eq!(
            engine.observe("bbbbbb", &signals_idle()),
            SessionStatus::Idle
        );
        assert!(engine.is_latched("aaaaaa"));
    }

    /// The bug this exists to stop: an agent that has said it is finished
    /// still has its process running, and reading that process as work would
    /// mean a row that never stops claiming to be busy.
    #[test]
    fn a_reporting_agents_process_is_not_taken_as_work() {
        let quiet_agent = SessionSignals {
            activity_age: Some(Duration::from_secs(300)),
            pane_commands: vec!["bash".into(), "claude".into()],
            ..SessionSignals::default()
        };
        let policy = StatusPolicy::default();

        // Nothing reports here, so the process is all there is to go on.
        assert_eq!(
            classify_as(&quiet_agent, &policy, Reporting::Silent),
            SessionStatus::Working
        );
        assert_eq!(
            classify_as(&quiet_agent, &policy, Reporting::Speaks),
            SessionStatus::Idle,
            "an agent that reports is waiting for a prompt, not working"
        );
    }

    /// Reporting only drops the process rule. A reporting agent that is
    /// actually doing something keeps its pane busy, and that still counts —
    /// so does a build the user started in another window of the session.
    #[test]
    fn a_reporting_session_is_still_working_while_its_panes_are_busy() {
        let busy = SessionSignals {
            activity_age: Some(Duration::from_secs(2)),
            pane_commands: vec!["claude".into()],
            ..SessionSignals::default()
        };
        assert_eq!(
            classify_as(&busy, &StatusPolicy::default(), Reporting::Speaks),
            SessionStatus::Working
        );
    }

    #[test]
    fn one_report_marks_a_session_as_one_that_speaks() {
        let mut engine = StatusEngine::default();
        let quiet_agent = SessionSignals {
            activity_age: Some(Duration::from_secs(300)),
            pane_commands: vec!["claude".into()],
            ..SessionSignals::default()
        };
        assert_eq!(engine.reporting("abc123"), Reporting::Silent);
        assert_eq!(
            engine.observe("abc123", &quiet_agent),
            SessionStatus::Working
        );

        // Any state does it: the point is that this agent talks, not what it
        // said.
        engine.notify("abc123", SessionStatus::Working);
        assert_eq!(engine.reporting("abc123"), Reporting::Speaks);
        assert_eq!(engine.observe("abc123", &quiet_agent), SessionStatus::Idle);
        assert_eq!(
            engine.observe("def456", &quiet_agent),
            SessionStatus::Working,
            "a session that has never reported is unaffected"
        );
    }

    /// A latch outranks everything, including a session judged idle now that
    /// its agent's process no longer counts.
    #[test]
    fn attention_still_latches_over_a_reporting_session() {
        let mut engine = StatusEngine::default();
        engine.notify("abc123", SessionStatus::Attention);
        assert_eq!(
            engine.observe("abc123", &signals_idle()),
            SessionStatus::Attention
        );
    }

    #[test]
    fn forget_and_retain_drop_stale_latches() {
        let mut engine = StatusEngine::default();
        engine.notify("aaaaaa", SessionStatus::Attention);
        engine.notify("bbbbbb", SessionStatus::Attention);
        engine.forget("aaaaaa");
        assert!(!engine.is_latched("aaaaaa"));
        assert_eq!(engine.reporting("aaaaaa"), Reporting::Silent);
        engine.retain_ids(|_| false);
        assert!(!engine.is_latched("bbbbbb"));
        assert_eq!(engine.reporting("bbbbbb"), Reporting::Silent);
    }

    #[test]
    fn a_report_without_a_scope_shows_no_resource_line() {
        // No scoped agent is not the same as an agent using nothing.
        let report = SessionReport::new(SessionStatus::Working);
        assert_eq!(report.resource_label(), None);
    }

    #[test]
    fn a_report_shows_memory_alone_until_a_rate_is_known() {
        let mut report = SessionReport::new(SessionStatus::Working);
        report.usage = Some(crate::cgroup::Usage {
            memory_bytes: 566_853_632,
            cpu_usec: 0,
        });
        // The first poll of a session has nothing to compare against.
        assert_eq!(report.resource_label().as_deref(), Some("540M"));

        report.cpu_percent = Some(64.0);
        assert_eq!(report.resource_label().as_deref(), Some("64%  540M"));
    }

    #[test]
    fn parse_accepts_the_notify_state_names() {
        assert_eq!(
            SessionStatus::parse("attention"),
            Some(SessionStatus::Attention)
        );
        assert_eq!(
            SessionStatus::parse(" Working "),
            Some(SessionStatus::Working)
        );
        assert_eq!(SessionStatus::parse("IDLE"), Some(SessionStatus::Idle));
        assert_eq!(SessionStatus::parse("busy"), None);
        assert_eq!(SessionStatus::parse(""), None);
    }
}
