//! The status poller and the `grove notify` listener.
//!
//! Two threads, both feeding the same [`StatusEngine`] and the same UI message
//! channel (ARCHITECTURE.md §2):
//!
//! - the **poller** asks tmux for signals on a fixed cadence and folds them
//!   into the engine;
//! - the **listener** accepts connections on the notify socket and folds
//!   explicit reports in as they arrive, so attention appears immediately
//!   rather than up to one poll later. The same socket carries `grove toggle`,
//!   which touches neither the engine nor tmux and is passed straight to the
//!   UI.
//!
//! The engine is shared behind a mutex because three parties touch it: the two
//! threads here and the UI, which clears a worktree's attention latch when the
//! user opens its session. The lock is only ever held for the fold itself,
//! never across a subprocess or a send.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use grove_core::cgroup::Usage;
use grove_core::config::StatusConfig;
use grove_core::desktop::{self, Attention};
use grove_core::ipc::{self, Command, Notification};
use grove_core::status::{SessionReport, SessionStatus, StatusEngine};
use grove_core::{Error, Paths, TmuxServer, workflow};

use crate::workers::{ErrorReport, Message};

/// How often tmux is polled for session signals (ARCHITECTURE.md §1).
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// How often git status is re-read. Five times cheaper than the tmux poll
/// because it is five times more expensive: one `git status` per worktree,
/// against one pair of tmux calls for the whole server.
pub const GIT_INTERVAL: Duration = Duration::from_secs(10);

/// Whether a git-status refresh is due, given when the last one was.
///
/// Split out so the cadence is testable without waiting ten seconds.
pub fn git_refresh_due(last: Option<Instant>, now: Instant) -> bool {
    match last {
        None => true,
        Some(last) => now.saturating_duration_since(last) >= GIT_INTERVAL,
    }
}

/// The status engine, shared by the poller, the listener and the UI.
pub type SharedEngine = Arc<Mutex<StatusEngine>>;

/// Take the engine lock, recovering from a poisoned mutex.
///
/// A panic in one holder must not silently stop status updates for the rest of
/// the session: the engine holds only the attention latch, and the worst case
/// of using it after a panic is one stale latch, which opening the session
/// clears. Losing status entirely is the worse failure.
pub fn lock(engine: &SharedEngine) -> std::sync::MutexGuard<'_, StatusEngine> {
    engine
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// What the poller thread is asked to do between ticks.
#[derive(Debug)]
pub enum Control {
    /// Poll now rather than waiting out the interval — after activating a
    /// worktree, say, where the user expects the row to react at once.
    PollNow,
    /// The user changed the `[status]` configuration.
    Reconfigure(Box<StatusConfig>),
    /// Names of the worktrees and projects a notification should describe.
    Describe(HashMap<String, WorktreeLabel>),
}

/// How a worktree is named in a desktop notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeLabel {
    pub project: String,
    pub worktree: String,
}

/// Handle the UI keeps for the poller thread.
pub struct StatusWatch {
    engine: SharedEngine,
    control: Sender<Control>,
}

impl StatusWatch {
    /// Start both threads. Returns the handle; results arrive on `out`.
    pub fn start(paths: &Paths, out: Sender<Message>, ctx: egui::Context) -> Self {
        let engine = Arc::new(Mutex::new(StatusEngine::default()));
        let (control_tx, control_rx) = channel::<Control>();

        let server = TmuxServer::new(paths.tmux_socket()).with_config(paths.tmux_config_file());
        let poller = Poller {
            server,
            engine: Arc::clone(&engine),
            out: out.clone(),
            ctx: ctx.clone(),
            labels: HashMap::new(),
            notify_desktop: StatusConfig::default().desktop_notifications,
            known: HashMap::new(),
            previous_usage: HashMap::new(),
            last_git_refresh: None,
        };
        spawn("grove-poller", move || poller.run(control_rx));

        let socket = paths.notify_socket();
        let listener_engine = Arc::clone(&engine);
        let listener_control = control_tx.clone();
        spawn("grove-notify", move || {
            listen(socket, listener_engine, out, ctx, listener_control);
        });

        Self {
            engine,
            control: control_tx,
        }
    }

    /// The user opened a session: clear its attention latch, and clear the
    /// durable tmux option too if there was one to clear.
    ///
    /// Returns true when a latch was cleared, which the caller turns into the
    /// `clear-attention` task — this function itself runs on the UI thread and
    /// must not touch tmux.
    pub fn opened(&self, worktree_id: &str) -> bool {
        lock(&self.engine).opened(worktree_id)
    }

    /// Forget a worktree's latch, after its session is closed or it is removed.
    pub fn forget(&self, worktree_id: &str) {
        lock(&self.engine).forget(worktree_id);
    }

    pub fn send(&self, control: Control) {
        // A dead poller means no more status updates; the rest of the UI is
        // unaffected, so this is reported and not fatal.
        if let Err(e) = self.control.send(control) {
            eprintln!("grove: status poller unavailable: {e}");
        }
    }
}

fn spawn(name: &str, body: impl FnOnce() + Send + 'static) {
    if let Err(e) = std::thread::Builder::new().name(name.into()).spawn(body) {
        eprintln!("grove: could not start the {name} thread: {e}");
    }
}

struct Poller {
    server: TmuxServer,
    engine: SharedEngine,
    out: Sender<Message>,
    ctx: egui::Context,
    labels: HashMap<String, WorktreeLabel>,
    notify_desktop: bool,
    /// The status last reported per worktree, so a desktop notification fires
    /// on the transition into attention rather than on every poll that still
    /// sees it.
    known: HashMap<String, SessionStatus>,
    /// The previous usage reading per worktree and when it was taken. CPU is a
    /// cumulative counter, so a percentage needs two readings.
    previous_usage: HashMap<String, (Usage, Instant)>,
    /// When git status was last asked for. The poller only rings the bell;
    /// the UI owns the worktree lists and the worker runs the git commands.
    last_git_refresh: Option<Instant>,
}

impl Poller {
    fn run(mut self, control: Receiver<Control>) {
        loop {
            self.tick();
            // One wait serves both purposes: it is the poll interval, and it
            // is how control messages arrive.
            match control.recv_timeout(POLL_INTERVAL) {
                Ok(message) => {
                    self.apply(message);
                    // Drain anything else already queued before polling again.
                    while let Ok(message) = control.try_recv() {
                        self.apply(message);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                // The UI dropped its handle: the app is shutting down.
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    fn apply(&mut self, message: Control) {
        match message {
            Control::PollNow => {}
            Control::Reconfigure(config) => {
                self.notify_desktop = config.desktop_notifications;
                lock(&self.engine).set_policy(config.policy());
            }
            Control::Describe(labels) => self.labels = labels,
        }
    }

    fn tick(&mut self) {
        let signals = match workflow::poll_session_signals(&self.server, workflow::now_epoch()) {
            Ok(signals) => signals,
            Err(e) => {
                self.report(&e);
                return;
            }
        };

        let now = Instant::now();
        // CPU rates first: they mutate the previous-reading map, and the
        // engine lock below must not be held across anything else.
        let cpu: HashMap<String, Option<f32>> = signals
            .iter()
            .map(|(id, signal)| (id.clone(), self.cpu_percent(id, signal.usage, now)))
            .collect();
        let mut reports = HashMap::with_capacity(signals.len());
        let mut raised = Vec::new();
        {
            let mut engine = lock(&self.engine);
            // Latches for worktrees whose sessions are gone would otherwise
            // accumulate for the life of the process.
            engine.retain_ids(|id| signals.contains_key(id));
            for (worktree_id, signal) in &signals {
                let status = engine.observe(worktree_id, signal);
                if status == SessionStatus::Attention
                    && self.known.get(worktree_id) != Some(&SessionStatus::Attention)
                {
                    raised.push(worktree_id.clone());
                }
                reports.insert(
                    worktree_id.clone(),
                    SessionReport {
                        status,
                        usage: signal.usage,
                        cpu_percent: cpu.get(worktree_id).copied().flatten(),
                    },
                );
            }
        }
        self.known = reports
            .iter()
            .map(|(id, r)| (id.clone(), r.status))
            .collect();
        // Drop readings for sessions that are gone, so the map cannot grow
        // without bound over a long-running process.
        self.previous_usage.retain(|id, _| signals.contains_key(id));

        for worktree_id in raised {
            self.announce(&worktree_id, None);
        }
        // The poll already listed every pane, so the windows come free with
        // it: this is what keeps the tree's child rows current when a window
        // is created inside tmux rather than from Grove.
        self.emit(Message::WindowsPolled(workflow::group_windows(
            signals
                .values()
                .flat_map(|signal| signal.windows.iter().cloned())
                .collect(),
        )));
        self.emit(Message::StatusPolled(reports));

        if git_refresh_due(self.last_git_refresh, now) {
            self.last_git_refresh = Some(now);
            self.emit(Message::GitStatusDue);
        }
    }

    /// CPU percentage since this worktree's previous reading, recording the
    /// current one for the next poll.
    fn cpu_percent(
        &mut self,
        worktree_id: &str,
        usage: Option<Usage>,
        now: Instant,
    ) -> Option<f32> {
        let usage = usage?;
        let percent = self
            .previous_usage
            .get(worktree_id)
            .and_then(|(previous, taken)| {
                usage.cpu_percent(*previous, now.saturating_duration_since(*taken))
            });
        self.previous_usage
            .insert(worktree_id.to_string(), (usage, now));
        percent
    }

    /// Post a desktop notification for a worktree that just raised attention.
    fn announce(&self, worktree_id: &str, message: Option<String>) {
        if !self.notify_desktop {
            return;
        }
        // A worktree Grove has no name for is one the UI is not showing;
        // a notification naming a bare id would help nobody.
        let Some(label) = self.labels.get(worktree_id) else {
            return;
        };
        let attention = Attention {
            project: label.project.clone(),
            worktree: label.worktree.clone(),
            message,
        };
        if let Err(e) = desktop::notify(&attention) {
            // Not surfaced in the UI: a failing notifier must not push an
            // error banner in front of the user every poll.
            eprintln!("grove: could not post a desktop notification: {e}");
        }
    }

    fn report(&self, error: &Error) {
        self.emit(Message::Failed(ErrorReport::new(
            "could not poll tmux for session status",
            error,
        )));
    }

    fn emit(&self, message: Message) {
        if self.out.send(message).is_ok() {
            self.ctx.request_repaint();
        }
    }
}

/// Accept notifications until the channel to the UI closes.
fn listen(
    socket: std::path::PathBuf,
    engine: SharedEngine,
    out: Sender<Message>,
    ctx: egui::Context,
    control: Sender<Control>,
) {
    let listener = match ipc::bind(&socket) {
        Ok(listener) => listener,
        Err(e) => {
            // Grove still works without it; `grove notify` then delivers
            // through the durable tmux option alone, which the poller reads.
            eprintln!("grove: not listening for `grove notify`: {e}");
            return;
        }
    };

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(e) => {
                eprintln!("grove: notify connection failed: {e}");
                continue;
            }
        };
        let command = match ipc::read_command(stream) {
            Ok(command) => command,
            Err(e) => {
                eprintln!("grove: ignoring a message: {e}");
                continue;
            }
        };
        let message = match command {
            // A toggle is pure UI: nothing to fold, and no reason to make
            // tmux answer a question before the window reacts.
            Command::Toggle { slot } => Message::Toggled { slot },
            Command::Notify(notification) => {
                fold(&engine, &notification);
                let Notification {
                    worktree_id,
                    state,
                    message,
                } = notification;
                // The poller owns the labels and the desktop notification, and
                // it re-reads tmux anyway; asking it to poll now is what turns
                // an immediate report into an immediate row update.
                let _ = control.send(Control::PollNow);
                Message::Notified {
                    worktree_id,
                    state,
                    message,
                }
            }
        };
        if out.send(message).is_err() {
            return;
        }
        ctx.request_repaint();
    }
}

fn fold(engine: &SharedEngine, notification: &Notification) {
    lock(engine).notify(&notification.worktree_id, notification.state);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_poisoned_engine_is_still_usable() {
        let engine: SharedEngine = Arc::new(Mutex::new(StatusEngine::default()));
        let poisoned = Arc::clone(&engine);
        // Poison the mutex from another thread.
        let _ = std::thread::spawn(move || {
            let _guard = poisoned.lock().expect("lock");
            panic!("poison it");
        })
        .join();

        assert!(engine.is_poisoned());
        lock(&engine).notify("a1b2c3", SessionStatus::Attention);
        assert!(
            lock(&engine).is_latched("a1b2c3"),
            "status must survive a panic elsewhere"
        );
    }

    #[test]
    fn git_refreshes_on_the_first_tick_then_on_its_own_cadence() {
        let start = Instant::now();
        assert!(
            git_refresh_due(None, start),
            "the first poll must not wait ten seconds for a status"
        );
        assert!(!git_refresh_due(Some(start), start));
        // The tmux poll runs four more times before git does.
        assert!(!git_refresh_due(
            Some(start),
            start + Duration::from_secs(8)
        ));
        assert!(git_refresh_due(Some(start), start + GIT_INTERVAL));
        assert!(git_refresh_due(
            Some(start),
            start + Duration::from_secs(30)
        ));
    }

    #[test]
    fn folding_a_notification_latches_attention() {
        let engine: SharedEngine = Arc::new(Mutex::new(StatusEngine::default()));
        fold(
            &engine,
            &Notification::new("a1b2c3", SessionStatus::Attention),
        );
        assert!(lock(&engine).is_latched("a1b2c3"));

        // And a later "working" report does not undo it.
        fold(
            &engine,
            &Notification::new("a1b2c3", SessionStatus::Working),
        );
        assert!(lock(&engine).is_latched("a1b2c3"));
    }
}
