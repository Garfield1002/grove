//! The status poller and the service-delivery listener.
//!
//! Two threads, both feeding the same [`StatusEngine`] and the same UI message
//! channel (ARCHITECTURE.md §2):
//!
//! - the **poller** asks the daemon for its tmux observation on a fixed cadence
//!   and folds that snapshot into the engine;
//! - the **listener** accepts commands forwarded by `grove serve` on the
//!   GUI-only socket and folds explicit reports in as they arrive. The public
//!   notify socket belongs to the service and outlives this process.
//!
//! The engine is shared behind a mutex because three parties touch it: the two
//! threads here and the UI, which clears a worktree's attention latch when the
//! user opens its session. The lock is only ever held for the fold itself,
//! never across a subprocess or a send.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use grove_core::cgroup::Usage;
use grove_core::config::StatusConfig;
use grove_core::desktop::{self, Attention};
use grove_core::ipc::{self, Command, Notification};
use grove_core::protocol::{self, EventKind, Request};
use grove_core::status::{SessionReport, SessionSignals, SessionStatus, StatusEngine};
use grove_core::{Error, Paths, workflow};

use crate::workers::{ErrorReport, Message};

/// How often tmux is polled for session signals (ARCHITECTURE.md §1).
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// The cadence while nothing is on screen. Grove is a launcher: most of its
/// life is spent minimised or behind the terminal it launched, where a
/// two-second poll updates rows nobody can see. The slow tick is not zero
/// because a bell raised while Grove is hidden still has to become a desktop
/// notification — it just arrives up to this late.
pub const DORMANT_INTERVAL: Duration = Duration::from_secs(30);

/// How long without a painted frame means nothing is on screen.
///
/// Comfortably longer than [`POLL_INTERVAL`]: every poll asks for a repaint,
/// so a visible Grove paints at least that often and can never be mistaken
/// for a hidden one.
pub const DORMANT_AFTER: Duration = Duration::from_secs(6);

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

/// How long to wait before the next poll, given how long ago the UI painted.
pub fn interval(since_paint: Duration) -> Duration {
    if since_paint > DORMANT_AFTER {
        DORMANT_INTERVAL
    } else {
        POLL_INTERVAL
    }
}

/// When the UI last painted a frame, shared between the UI thread and the
/// poller.
///
/// This is Grove's answer to "is anyone looking?", and it is a better one than
/// window focus: the common way to use Grove is beside the terminal it
/// launched, visible but not focused, and status dots have to stay live there.
/// A frame is the honest signal — a Wayland surface that is minimised, on
/// another workspace or fully occluded stops getting frame callbacks, so the
/// repaint each poll asks for never becomes a frame. Where that is not true
/// (X11, an unoccluded window), Grove simply keeps its old cadence.
#[derive(Clone)]
pub struct PaintClock {
    start: Instant,
    /// Milliseconds after `start` of the last painted frame.
    last: Arc<AtomicU64>,
}

impl PaintClock {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            last: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Record a frame, returning how long it had been since the previous one.
    fn mark(&self) -> Duration {
        let now = self.millis();
        let previous = self.last.swap(now, Ordering::Relaxed);
        Duration::from_millis(now.saturating_sub(previous))
    }

    /// How long ago the UI last painted.
    fn since_paint(&self) -> Duration {
        Duration::from_millis(
            self.millis()
                .saturating_sub(self.last.load(Ordering::Relaxed)),
        )
    }

    /// A clock whose last frame was `gap` ago.
    #[cfg(test)]
    fn aged(gap: Duration) -> Self {
        Self {
            start: Instant::now()
                .checked_sub(gap)
                .expect("a gap shorter than the process"),
            last: Arc::new(AtomicU64::new(0)),
        }
    }

    fn millis(&self) -> u64 {
        // Saturating rather than wrapping: a process running for 584 million
        // years would otherwise start reporting fresh frames.
        self.start
            .elapsed()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX)
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
    paint: PaintClock,
}

impl StatusWatch {
    /// Start the poller and delivery threads. Returns the handle; results
    /// arrive on `out`.
    pub fn start(paths: &Paths, out: Sender<Message>, ctx: egui::Context) -> Self {
        let engine = Arc::new(Mutex::new(StatusEngine::default()));
        let (control_tx, control_rx) = channel::<Control>();
        let paint = PaintClock::new();

        let poller = Poller {
            service_socket: paths.notify_socket(),
            engine: Arc::clone(&engine),
            out: out.clone(),
            ctx: ctx.clone(),
            paint: paint.clone(),
            labels: HashMap::new(),
            notify_desktop: StatusConfig::default().desktop_notifications,
            known: HashMap::new(),
            previous_usage: HashMap::new(),
            last_git_refresh: None,
        };
        spawn("grove-poller", move || poller.run(control_rx));

        let socket = paths.gui_socket();
        let service_socket = paths.notify_socket();
        let listener_out = out.clone();
        let listener_ctx = ctx.clone();
        spawn("grove-notify", move || {
            listen(socket, service_socket, listener_out, listener_ctx);
        });
        let event_socket = paths.notify_socket();
        let event_out = out.clone();
        let event_ctx = ctx.clone();
        spawn("grove-events", move || {
            subscribe(event_socket, event_out, event_ctx);
        });

        Self {
            engine,
            control: control_tx,
            paint,
        }
    }

    /// Record that the UI painted a frame, and say whether Grove was dormant
    /// until now — a frame after a long gap is the window coming back into
    /// view, and the caller turns that into an immediate poll rather than
    /// leaving the tree stale for the rest of the slow interval.
    pub fn painted(&self) -> bool {
        self.paint.mark() > DORMANT_AFTER
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

    /// Apply one explicit agent report to the latch and wake the poller.
    ///
    /// Delivery threads only transport notifications. The UI invokes this
    /// exactly once when it applies the same report to presentation state,
    /// keeping one ownership path for both legacy and service delivery.
    pub fn notified(&self, notification: &Notification) {
        lock(&self.engine).notify(
            &notification.worktree_id,
            notification.state,
            notification.reason,
        );
        self.send(Control::PollNow);
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
    service_socket: PathBuf,
    engine: SharedEngine,
    out: Sender<Message>,
    ctx: egui::Context,
    /// When the UI last painted, which sets the cadence.
    paint: PaintClock,
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
            // is how control messages arrive. The interval is taken after the
            // tick, so a poll that painted nothing slows the *next* wait.
            match control.recv_timeout(interval(self.paint.since_paint())) {
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

    /// Nothing has been on screen for a while, so this tick is a background
    /// one: it exists to catch attention, not to keep rows current.
    fn dormant(&self) -> bool {
        self.paint.since_paint() > DORMANT_AFTER
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
        let now_epoch = workflow::now_epoch();
        let mut signals = match poll_signals(&self.service_socket) {
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
            for (worktree_id, signal) in &mut signals {
                // Each window is judged on its own activity, so a busy agent
                // cannot make the empty shell beside it look busy too.
                engine.observe_windows(worktree_id, &mut signal.windows, now_epoch);
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

        // Git status is the expensive half — one `git status` per worktree —
        // and unlike attention it has no notification to raise, so nothing is
        // lost by leaving it until Grove is back on screen. `last_git_refresh`
        // is left alone while dormant, so the first tick after a frame finds
        // the refresh overdue and asks for it at once.
        if !self.dormant() && git_refresh_due(self.last_git_refresh, now) {
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

fn poll_signals(socket: &std::path::Path) -> Result<HashMap<String, SessionSignals>, Error> {
    let request = Request::new("gui-status-poll", "status.poll", serde_json::Value::Null);
    let response = protocol::call(socket, &request).map_err(|error| {
        Error::io(
            "poll status through Grove service",
            std::io::Error::other(error.to_string()),
        )
    })?;
    if let Some(error) = response.error {
        return Err(Error::io(
            "poll status through Grove service",
            std::io::Error::other(format!("{}: {}", error.code, error.message)),
        ));
    }
    let value = response.result.ok_or_else(|| {
        Error::io(
            "poll status through Grove service",
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "service status response has no result",
            ),
        )
    })?;
    serde_json::from_value(value).map_err(|error| {
        Error::io(
            "decode service status response",
            std::io::Error::new(std::io::ErrorKind::InvalidData, error),
        )
    })
}

/// Keep one service event stream open for the life of the GUI. A disconnect
/// is recoverable: the existing poller remains authoritative and this thread
/// retries until the UI channel closes.
fn subscribe(socket: std::path::PathBuf, out: Sender<Message>, ctx: egui::Context) {
    let request = Request::new(
        "gui-events",
        "event.subscribe",
        serde_json::json!({
            "client": "gui",
            "topics": [
                EventKind::StateChanged,
                EventKind::ReconciliationCompleted,
                EventKind::NotificationReceived,
                EventKind::ControlCompleted,
            ]
        }),
    );
    loop {
        let (mut stream, response) = match protocol::open_subscription(&socket, &request) {
            Ok(opened) => opened,
            Err(error) => {
                if out.send(Message::ServiceEventsUnavailable).is_err() {
                    return;
                }
                eprintln!("grove: service event stream unavailable: {error}");
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        if !response.ok {
            let detail = response
                .error
                .map(|error| format!("{}: {}", error.code, error.message))
                .unwrap_or_else(|| "invalid subscription response".to_string());
            eprintln!("grove: service rejected event subscription: {detail}");
            if out.send(Message::ServiceEventsUnavailable).is_err() {
                return;
            }
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }
        let revision = match subscription_revision(&response) {
            Ok(revision) => revision,
            Err(detail) => {
                eprintln!("grove: service returned an invalid event subscription: {detail}");
                if out.send(Message::ServiceEventsUnavailable).is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        if out
            .send(Message::ServiceEventsStarted { revision })
            .is_err()
        {
            return;
        }
        while let Ok(event) = protocol::read_event(&mut stream) {
            if out.send(Message::ServiceEvent(Box::new(event))).is_err() {
                return;
            }
            ctx.request_repaint();
        }
        if out.send(Message::ServiceEventsUnavailable).is_err() {
            return;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn subscription_revision(response: &protocol::Response) -> Result<u64, serde_json::Error> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Acknowledgement {
        revision: u64,
    }

    serde_json::from_value::<Acknowledgement>(
        response.result.clone().unwrap_or(serde_json::Value::Null),
    )
    .map(|acknowledgement| acknowledgement.revision)
}

/// Accept notifications until the channel to the UI closes.
fn listen(
    socket: std::path::PathBuf,
    service_socket: std::path::PathBuf,
    out: Sender<Message>,
    ctx: egui::Context,
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

    // The service may have been spawned moments before this thread. Announce
    // readiness with a short bounded retry so reports queued during GUI
    // startup are delivered instead of waiting for another command.
    for _ in 0..20 {
        match ipc::send_command(&service_socket, &Command::GuiReady) {
            Ok(true) => break,
            Ok(false) => std::thread::sleep(Duration::from_millis(50)),
            Err(error) => {
                eprintln!("grove: could not announce the GUI to the service: {error}");
                break;
            }
        }
    }

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
            Command::Notify(notification) => Message::Notified(Box::new(notification)),
            // These are consumed by the service and should never be
            // forwarded. Ignore them defensively rather than inventing a UI
            // action if a malformed client writes to the private socket.
            Command::Ping | Command::GuiReady => continue,
        };
        if out.send(message).is_err() {
            return;
        }
        ctx.request_repaint();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grove_core::cgroup::Usage;
    use grove_core::protocol::Response;
    use grove_core::tmux::WindowInfo;
    use std::os::unix::net::UnixListener;
    use std::sync::mpsc::TryRecvError;

    fn serve_once(socket: &std::path::Path, response: Response) -> std::thread::JoinHandle<()> {
        let listener = UnixListener::bind(socket).expect("bind service socket");
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept status request");
            let request = protocol::read_request(&mut stream).expect("status request");
            assert_eq!(request.method, "status.poll");
            protocol::write_response(&mut stream, &response).expect("status response");
        })
    }

    fn serve_subscription_once(
        socket: &std::path::Path,
        response: Response,
    ) -> std::thread::JoinHandle<()> {
        let listener = UnixListener::bind(socket).expect("bind service socket");
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept subscription");
            let request = protocol::read_request(&mut stream).expect("subscription request");
            assert_eq!(request.method, "event.subscribe");
            protocol::write_response(&mut stream, &response).expect("subscription response");
        })
    }

    fn poller(
        socket: PathBuf,
        out: Sender<Message>,
        paint: PaintClock,
        engine: SharedEngine,
    ) -> Poller {
        Poller {
            service_socket: socket,
            engine,
            out,
            ctx: egui::Context::default(),
            paint,
            labels: HashMap::new(),
            notify_desktop: false,
            known: HashMap::new(),
            previous_usage: HashMap::new(),
            last_git_refresh: None,
        }
    }

    fn signal(cpu_usec: u64, windows: Vec<WindowInfo>) -> SessionSignals {
        SessionSignals {
            activity_age: Some(Duration::from_secs(3)),
            pane_commands: vec!["claude".into()],
            usage: Some(Usage {
                memory_bytes: 4096,
                cpu_usec,
            }),
            windows,
            ..SessionSignals::default()
        }
    }

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
        lock(&engine).notify("a1b2c3", SessionStatus::Attention, None);
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
    fn a_painting_ui_holds_the_fast_cadence() {
        assert_eq!(interval(Duration::ZERO), POLL_INTERVAL);
        assert_eq!(
            interval(POLL_INTERVAL),
            POLL_INTERVAL,
            "one poll's worth of silence is what a visible Grove looks like"
        );
        assert_eq!(
            interval(DORMANT_AFTER),
            POLL_INTERVAL,
            "the threshold itself is still awake"
        );
    }

    #[test]
    fn a_ui_that_stopped_painting_slows_the_poller() {
        assert_eq!(
            interval(DORMANT_AFTER + Duration::from_millis(1)),
            DORMANT_INTERVAL
        );
        assert_eq!(interval(Duration::from_secs(3600)), DORMANT_INTERVAL);
    }

    #[test]
    fn a_clock_that_has_just_marked_reads_as_awake() {
        let clock = PaintClock::new();
        clock.mark();
        assert!(
            clock.since_paint() < DORMANT_AFTER,
            "a frame was painted a moment ago"
        );
        assert_eq!(interval(clock.since_paint()), POLL_INTERVAL);
    }

    #[test]
    fn a_frame_after_a_long_gap_reports_the_gap_and_wakes_the_clock() {
        let gap = DORMANT_AFTER + Duration::from_secs(10);
        let clock = PaintClock::aged(gap);
        assert!(
            clock.since_paint() >= gap,
            "no frames since the clock began"
        );
        assert_eq!(interval(clock.since_paint()), DORMANT_INTERVAL);

        // The window came back: the frame reports the whole gap, which is what
        // `StatusWatch::painted` turns into an immediate poll.
        assert!(clock.mark() >= gap);
        assert!(clock.since_paint() < DORMANT_AFTER);
        assert_eq!(interval(clock.since_paint()), POLL_INTERVAL);
    }

    #[test]
    fn applying_a_notification_once_latches_attention_and_wakes_the_poller() {
        let engine: SharedEngine = Arc::new(Mutex::new(StatusEngine::default()));
        let (control, received) = channel();
        let watch = StatusWatch {
            engine: Arc::clone(&engine),
            control,
            paint: PaintClock::new(),
        };
        watch.notified(&Notification::new("a1b2c3", SessionStatus::Attention));
        assert!(lock(&engine).is_latched("a1b2c3"));
        assert!(matches!(
            received.recv().expect("poll request"),
            Control::PollNow
        ));

        // And a later "working" report does not undo it.
        watch.notified(&Notification::new("a1b2c3", SessionStatus::Working));
        assert!(lock(&engine).is_latched("a1b2c3"));
        assert!(matches!(
            received.recv().expect("second poll request"),
            Control::PollNow
        ));
    }

    #[test]
    fn status_poll_decodes_the_daemons_complete_observation() {
        let temp = tempfile::tempdir().expect("tempdir");
        let socket = temp.path().join("service.sock");
        let signals = HashMap::from([(
            "abc123".to_string(),
            SessionSignals {
                activity_age: Some(Duration::from_secs(3)),
                pane_commands: vec!["claude".into()],
                attention_flag: true,
                done_flag: false,
                bell: false,
                usage: Some(Usage {
                    memory_bytes: 4096,
                    cpu_usec: 99,
                }),
                windows: vec![WindowInfo {
                    session: "wt-abc123".into(),
                    index: 1,
                    name: "agent".into(),
                    active: true,
                    bell: false,
                    title: Some("reviewing".into()),
                    activity_epoch: Some(123),
                    commands: vec!["claude".into()],
                    status: None,
                }],
            },
        )]);
        let response = Response::success(
            "gui-status-poll",
            serde_json::to_value(&signals).expect("serializable signals"),
        );
        let server = serve_once(&socket, response);

        assert_eq!(poll_signals(&socket).expect("status poll"), signals);
        server.join().expect("service thread");
    }

    #[test]
    fn status_poll_preserves_service_and_decode_failures() {
        let temp = tempfile::tempdir().expect("tempdir");
        let absent = temp.path().join("absent.sock");
        assert!(
            poll_signals(&absent)
                .expect_err("absent daemon")
                .to_string()
                .contains("connect to Grove service")
        );

        let rejected_socket = temp.path().join("rejected.sock");
        let rejected = Response::error("gui-status-poll", "tmux_failed", "private server refused");
        let rejected_server = serve_once(&rejected_socket, rejected);
        let error = poll_signals(&rejected_socket).expect_err("service rejection");
        assert!(
            error
                .to_string()
                .contains("tmux_failed: private server refused")
        );
        rejected_server.join().expect("service thread");

        let malformed_socket = temp.path().join("malformed.sock");
        let malformed = Response::success("gui-status-poll", serde_json::json!({"abc123": 7}));
        let malformed_server = serve_once(&malformed_socket, malformed);
        let error = poll_signals(&malformed_socket).expect_err("malformed observation");
        assert!(error.to_string().contains("decode service status response"));
        malformed_server.join().expect("service thread");
    }

    #[test]
    fn a_rejected_event_subscription_stops_when_the_ui_is_gone() {
        let temp = tempfile::tempdir().expect("tempdir");
        let socket = temp.path().join("service.sock");
        let server = serve_subscription_once(
            &socket,
            Response::error("gui-events", "subscription_rejected", "unsupported client"),
        );
        let (out, messages) = channel();
        drop(messages);

        subscribe(socket, out, egui::Context::default());

        server.join().expect("service thread");
    }

    #[test]
    fn subscription_acknowledgements_require_one_unsigned_revision() {
        assert_eq!(
            subscription_revision(&Response::success(
                "gui-events",
                serde_json::json!({"revision": 42}),
            ))
            .expect("valid acknowledgement"),
            42
        );
        for result in [
            serde_json::Value::Null,
            serde_json::json!({}),
            serde_json::json!({"revision": "42"}),
            serde_json::json!({"revision": -1}),
            serde_json::json!({"revision": 42, "unexpected": true}),
        ] {
            assert!(
                subscription_revision(&Response::success("gui-events", result)).is_err(),
                "accepted malformed subscription acknowledgement"
            );
        }
    }

    #[test]
    fn a_malformed_subscription_acknowledgement_never_invents_a_baseline() {
        let temp = tempfile::tempdir().expect("tempdir");
        let socket = temp.path().join("service.sock");
        let server = serve_subscription_once(
            &socket,
            Response::success("gui-events", serde_json::json!({})),
        );
        let (out, messages) = channel();
        drop(messages);

        subscribe(socket, out, egui::Context::default());

        server.join().expect("service thread");
    }

    #[test]
    fn a_tick_folds_the_daemon_snapshot_and_emits_one_coherent_update() {
        let temp = tempfile::tempdir().expect("tempdir");
        let socket = temp.path().join("service.sock");
        let window = WindowInfo {
            session: "wt-abc123".into(),
            index: 1,
            name: "agent".into(),
            active: true,
            bell: false,
            title: Some("reviewing".into()),
            activity_epoch: Some(123),
            commands: vec!["claude".into()],
            status: None,
        };
        let signals = HashMap::from([("abc123".to_string(), signal(100, vec![window.clone()]))]);
        let server = serve_once(
            &socket,
            Response::success(
                "gui-status-poll",
                serde_json::to_value(signals).expect("signals"),
            ),
        );
        let (out, messages) = channel();
        let paint = PaintClock::new();
        paint.mark();
        let engine: SharedEngine = Arc::new(Mutex::new(StatusEngine::default()));
        lock(&engine).notify("gone", SessionStatus::Attention, None);
        let mut poller = poller(socket, out, paint, Arc::clone(&engine));

        poller.tick();
        server.join().expect("service thread");

        match messages.recv().expect("windows") {
            Message::WindowsPolled(windows) => {
                assert_eq!(windows["wt-abc123"].len(), 1);
                assert_eq!(windows["wt-abc123"][0].name, window.name);
                assert_eq!(windows["wt-abc123"][0].status, Some(SessionStatus::Working));
            }
            other => panic!("expected windows, got {other:?}"),
        }
        match messages.recv().expect("statuses") {
            Message::StatusPolled(reports) => {
                let report = &reports["abc123"];
                assert_eq!(report.status, SessionStatus::Working);
                assert_eq!(report.usage.map(|usage| usage.memory_bytes), Some(4096));
                assert_eq!(report.cpu_percent, None, "the first reading is a baseline");
            }
            other => panic!("expected statuses, got {other:?}"),
        }
        assert!(matches!(
            messages.recv().expect("git refresh"),
            Message::GitStatusDue
        ));
        assert!(!lock(&engine).is_latched("gone"));
        assert!(poller.previous_usage.contains_key("abc123"));
        assert!(poller.last_git_refresh.is_some());
    }

    #[test]
    fn a_failed_tick_reports_once_and_changes_no_poll_state() {
        let temp = tempfile::tempdir().expect("tempdir");
        let socket = temp.path().join("service.sock");
        let server = serve_once(
            &socket,
            Response::error("gui-status-poll", "tmux_failed", "private server refused"),
        );
        let (out, messages) = channel();
        let engine: SharedEngine = Arc::new(Mutex::new(StatusEngine::default()));
        let mut poller = poller(socket, out, PaintClock::new(), engine);

        poller.tick();
        server.join().expect("service thread");

        assert!(matches!(
            messages.recv().expect("failure"),
            Message::Failed(_)
        ));
        assert!(matches!(messages.try_recv(), Err(TryRecvError::Empty)));
        assert!(poller.known.is_empty());
        assert!(poller.previous_usage.is_empty());
        assert_eq!(poller.last_git_refresh, None);
    }

    #[test]
    fn controls_update_only_the_poller_policy_and_labels() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (out, _) = channel();
        let engine: SharedEngine = Arc::new(Mutex::new(StatusEngine::default()));
        let mut poller = poller(
            temp.path().join("unused.sock"),
            out,
            PaintClock::new(),
            Arc::clone(&engine),
        );
        let labels = HashMap::from([(
            "abc123".into(),
            WorktreeLabel {
                project: "Grove".into(),
                worktree: "main".into(),
            },
        )]);
        poller.apply(Control::Describe(labels.clone()));
        assert_eq!(poller.labels, labels);

        let config = StatusConfig {
            desktop_notifications: true,
            working_window_secs: 42,
            ..StatusConfig::default()
        };
        poller.apply(Control::Reconfigure(Box::new(config)));
        assert!(poller.notify_desktop);
        assert_eq!(
            lock(&engine).policy().working_window,
            Duration::from_secs(42)
        );

        poller.apply(Control::PollNow);
        assert!(poller.notify_desktop, "poll-now changes no policy");
    }

    #[test]
    fn cpu_rates_require_two_readings_and_missing_usage_resets_nothing() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (out, _) = channel();
        let engine: SharedEngine = Arc::new(Mutex::new(StatusEngine::default()));
        let mut poller = poller(
            temp.path().join("unused.sock"),
            out,
            PaintClock::new(),
            engine,
        );
        let first = Instant::now();
        let baseline = Usage {
            memory_bytes: 1024,
            cpu_usec: 1_000_000,
        };
        assert_eq!(poller.cpu_percent("abc123", None, first), None);
        assert_eq!(poller.cpu_percent("abc123", Some(baseline), first), None);

        let current = Usage {
            memory_bytes: 2048,
            cpu_usec: 1_500_000,
        };
        assert_eq!(
            poller.cpu_percent("abc123", Some(current), first + Duration::from_secs(1)),
            Some(50.0)
        );
        assert_eq!(
            poller.previous_usage["abc123"].0, current,
            "the newest cumulative counter becomes the next baseline"
        );
    }

    #[test]
    fn a_dormant_tick_never_requests_expensive_git_status() {
        let temp = tempfile::tempdir().expect("tempdir");
        let socket = temp.path().join("service.sock");
        let server = serve_once(
            &socket,
            Response::success("gui-status-poll", serde_json::json!({})),
        );
        let (out, messages) = channel();
        let engine: SharedEngine = Arc::new(Mutex::new(StatusEngine::default()));
        let mut poller = poller(
            socket,
            out,
            PaintClock::aged(DORMANT_AFTER + Duration::from_secs(1)),
            engine,
        );

        poller.tick();
        server.join().expect("service thread");
        assert!(matches!(
            messages.recv().expect("windows"),
            Message::WindowsPolled(_)
        ));
        assert!(matches!(
            messages.recv().expect("statuses"),
            Message::StatusPolled(_)
        ));
        assert!(matches!(messages.try_recv(), Err(TryRecvError::Empty)));
        assert_eq!(
            poller.last_git_refresh, None,
            "the first visible tick must still find git refresh overdue"
        );
    }
}
