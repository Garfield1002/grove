//! Persistent local service.
//!
//! The service owns Grove's public runtime socket independently of the GUI.
//! Agent reports and toggles arrive here; live UI commands are forwarded to a
//! separate socket owned by the GUI, and useful commands are queued while the
//! GUI is absent.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use grove_core::ipc::{self, Command, Notification};
use grove_core::paths::ensure_private_dir;
use grove_core::process::Invocation;
use grove_core::protocol::{self, Event, EventKind, Request, Response};
use grove_core::reconcile::{self, ProjectRef, Reconciliation};
use grove_core::state::{STATE_VERSION, SessionRecord, State};
use grove_core::{Paths, Result, TmuxServer, query, state};
use serde_json::json;

const USAGE: &str = "\
grove serve — run Grove's persistent local service

Usage:
  grove serve
";

const GUI_LAUNCH_GRACE: Duration = Duration::from_secs(5);
const MAX_CLIENTS: usize = 32;
const SUBSCRIBER_QUEUE: usize = 32;

pub fn run(args: &[String], paths: &Paths) -> Result<()> {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        print!("{USAGE}");
        return Ok(());
    }
    if let Some(arg) = args.first() {
        return Err(grove_core::Error::io(
            format!("unexpected argument `{arg}`"),
            std::io::Error::from(std::io::ErrorKind::InvalidInput),
        ));
    }

    ensure_private_dir(&paths.runtime_dir)?;
    let listener = ipc::bind(&paths.notify_socket())?;
    let events = Arc::new(EventHub::default());
    let service = Arc::new(Mutex::new(Service::new(paths, Arc::clone(&events))));
    let api = Arc::new(ApiContext::new(paths, events));
    let active_clients = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                if active_clients
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |active| {
                        (active < MAX_CLIENTS).then_some(active + 1)
                    })
                    .is_err()
                {
                    eprintln!("grove serve: rejecting a client; {MAX_CLIENTS} are already active");
                    continue;
                }
                let service = Arc::clone(&service);
                let api = Arc::clone(&api);
                let clients = Arc::clone(&active_clients);
                if let Err(error) = std::thread::Builder::new()
                    .name("grove-service-client".into())
                    .spawn(move || {
                        let _guard = ClientGuard(clients);
                        handle_connection(stream, &service, &api);
                    })
                {
                    active_clients.fetch_sub(1, Ordering::Relaxed);
                    eprintln!("grove serve: could not isolate a client connection: {error}");
                }
            }
            Err(error) => eprintln!("grove serve: connection failed: {error}"),
        }
    }
    Ok(())
}

struct ClientGuard(Arc<AtomicUsize>);

impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

fn handle_connection(mut stream: UnixStream, service: &Arc<Mutex<Service>>, api: &Arc<ApiContext>) {
    if let Err(error) = protocol::configure(&stream) {
        eprintln!("grove serve: could not configure a connection: {error}");
        return;
    }
    let mut first = [0_u8; 1];
    match stream.read_exact(&mut first) {
        // Legacy commands always begin with the ASCII protocol tag `grove1`.
        Ok(()) if first[0] == b'g' => match ipc::read_command_after_first(stream, first[0]) {
            Ok(command) => lock(service).handle(command),
            Err(error) => eprintln!("grove serve: ignoring a legacy message: {error}"),
        },
        Ok(()) => handle_api(stream, first[0], api),
        Err(error) => eprintln!("grove serve: could not identify a client message: {error}"),
    }
}

fn handle_api(mut stream: UnixStream, first: u8, api: &ApiContext) {
    let request = match protocol::read_request_after_first(&mut stream, first) {
        Ok(request) => request,
        Err(error) => {
            eprintln!("grove serve: ignoring an invalid API request: {error}");
            return;
        }
    };
    if request.method == "event.subscribe" {
        serve_subscription(stream, &request, api);
        return;
    }
    let response = dispatch_api(&request, api);
    if let Err(error) = protocol::write_response(&mut stream, &response) {
        eprintln!("grove serve: could not write an API response: {error}");
    }
}

struct ApiContext {
    state_file: std::path::PathBuf,
    server: TmuxServer,
    state_gate: Mutex<()>,
    events: Arc<EventHub>,
}

impl ApiContext {
    fn new(paths: &Paths, events: Arc<EventHub>) -> Self {
        Self {
            state_file: paths.state_file(),
            // Read-only API calls must not create tmux configuration files.
            server: TmuxServer::new(paths.tmux_socket()),
            state_gate: Mutex::new(()),
            events,
        }
    }
}

#[derive(Default)]
struct EventHub {
    revision: AtomicU64,
    next_subscriber: AtomicU64,
    subscribers: Mutex<HashMap<String, Subscriber>>,
}

struct Subscriber {
    topics: HashSet<EventKind>,
    sender: SyncSender<Event>,
    replaces_legacy_gui_delivery: bool,
}

#[derive(Default)]
struct PublishOutcome {
    delivered: bool,
    delivered_to_gui: bool,
}

impl EventHub {
    fn subscribe(
        &self,
        topics: HashSet<EventKind>,
        replaces_legacy_gui_delivery: bool,
    ) -> (String, Receiver<Event>, u64) {
        let id = format!(
            "sub-{}",
            self.next_subscriber.fetch_add(1, Ordering::Relaxed) + 1
        );
        let (sender, receiver) = sync_channel(SUBSCRIBER_QUEUE);
        let mut subscribers = lock_subscribers(self);
        let revision = self.revision.load(Ordering::Relaxed);
        subscribers.insert(
            id.clone(),
            Subscriber {
                topics,
                sender,
                replaces_legacy_gui_delivery,
            },
        );
        (id, receiver, revision)
    }

    fn unsubscribe(&self, id: &str) -> bool {
        lock_subscribers(self).remove(id).is_some()
    }

    fn publish(&self, kind: EventKind, payload: serde_json::Value) -> PublishOutcome {
        let mut subscribers = lock_subscribers(self);
        // Registration and revision assignment share this lock, giving the
        // acknowledgement baseline and subsequent events one total order.
        let revision = self.revision.fetch_add(1, Ordering::Relaxed) + 1;
        let event = Event::new(revision, kind, payload);
        let mut outcome = PublishOutcome::default();
        subscribers.retain(|_, subscriber| {
            if !subscriber.topics.contains(&kind) {
                return true;
            }
            match subscriber.sender.try_send(event.clone()) {
                Ok(()) => {
                    outcome.delivered = true;
                    outcome.delivered_to_gui |= subscriber.replaces_legacy_gui_delivery;
                    true
                }
                Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => false,
            }
        });
        outcome
    }
}

fn lock_subscribers(events: &EventHub) -> std::sync::MutexGuard<'_, HashMap<String, Subscriber>> {
    events
        .subscribers
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplaceStateParams {
    state: State,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ReconcileParams {
    projects: Vec<ProjectRef>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SubscribeParams {
    topics: HashSet<EventKind>,
    #[serde(default)]
    client: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct UnsubscribeParams {
    subscription_id: String,
}

#[derive(serde::Serialize)]
struct ReconcileResult<'a> {
    reconciliation: &'a Reconciliation,
    state: &'a State,
}

fn dispatch_api(request: &Request, api: &ApiContext) -> Response {
    if request.protocol != protocol::VERSION {
        return Response::error(
            &request.id,
            "unsupported_protocol",
            format!(
                "protocol {} is unsupported; this service speaks {}",
                request.protocol,
                protocol::VERSION
            ),
        );
    }
    let accepts_params = matches!(
        request.method.as_str(),
        "state.replace" | "state.reconcile" | "event.unsubscribe"
    );
    if !accepts_params
        && !matches!(&request.params, serde_json::Value::Null)
        && !request
            .params
            .as_object()
            .is_some_and(serde_json::Map::is_empty)
    {
        return Response::error(
            &request.id,
            "invalid_params",
            format!("`{}` does not accept parameters", request.method),
        );
    }
    match request.method.as_str() {
        "ping" => Response::success(
            &request.id,
            json!({
                "service_version": env!("CARGO_PKG_VERSION"),
                "protocol": protocol::VERSION,
            }),
        ),
        "project.list" => with_state(request, api, |state| {
            Ok(serde_json::to_value(query::list_projects(state))?)
        }),
        "worktree.list" => with_state(request, api, |state| {
            Ok(serde_json::to_value(query::list_worktrees(
                state,
                &api.server,
            )?)?)
        }),
        "session.list" => api_result(request, || {
            Ok(serde_json::to_value(query::list_sessions(&api.server)?)?)
        }),
        "session.snapshot" => with_state(request, api, |state| {
            Ok(serde_json::to_value(query::snapshot(state, &api.server)?)?)
        }),
        "state.replace" => replace_state(request, api),
        "state.reconcile" => reconcile_state(request, api),
        "event.unsubscribe" => unsubscribe(request, api),
        "event.subscribe" => Response::error(
            &request.id,
            "invalid_subscription",
            "event.subscribe must use a streaming connection",
        ),
        method => Response::error(
            &request.id,
            "method_not_found",
            format!("unknown service method `{method}`"),
        ),
    }
}

fn serve_subscription(mut stream: UnixStream, request: &Request, api: &ApiContext) {
    if request.protocol != protocol::VERSION {
        let response = Response::error(
            &request.id,
            "unsupported_protocol",
            format!(
                "protocol {} is unsupported; this service speaks {}",
                request.protocol,
                protocol::VERSION
            ),
        );
        let _ = protocol::write_response(&mut stream, &response);
        return;
    }
    let SubscribeParams { topics, client } = match serde_json::from_value(request.params.clone()) {
        Ok(params) => params,
        Err(error) => {
            let response = Response::error(&request.id, "invalid_params", error.to_string());
            let _ = protocol::write_response(&mut stream, &response);
            return;
        }
    };
    if topics.is_empty() {
        let response = Response::error(
            &request.id,
            "invalid_params",
            "at least one event topic is required",
        );
        let _ = protocol::write_response(&mut stream, &response);
        return;
    }
    if client.as_deref().is_some_and(|client| client != "gui") {
        let response = Response::error(
            &request.id,
            "invalid_params",
            "subscription client must be `gui` when present",
        );
        let _ = protocol::write_response(&mut stream, &response);
        return;
    }
    let (subscription_id, events, revision) = api
        .events
        .subscribe(topics, client.as_deref() == Some("gui"));
    let response = Response::success(
        &request.id,
        json!({
            "subscription_id": subscription_id,
            "revision": revision,
        }),
    );
    if protocol::write_response(&mut stream, &response).is_err() {
        api.events.unsubscribe(&subscription_id);
        return;
    }
    while let Ok(event) = events.recv() {
        if protocol::write_json(&mut stream, &event).is_err() {
            break;
        }
    }
    api.events.unsubscribe(&subscription_id);
}

fn unsubscribe(request: &Request, api: &ApiContext) -> Response {
    let UnsubscribeParams { subscription_id } = match serde_json::from_value(request.params.clone())
    {
        Ok(params) => params,
        Err(error) => {
            return Response::error(&request.id, "invalid_params", error.to_string());
        }
    };
    Response::success(
        &request.id,
        json!({"unsubscribed": api.events.unsubscribe(&subscription_id)}),
    )
}

fn replace_state(request: &Request, api: &ApiContext) -> Response {
    let ReplaceStateParams { mut state } = match serde_json::from_value(request.params.clone()) {
        Ok(params) => params,
        Err(error) => {
            return Response::error(&request.id, "invalid_params", error.to_string());
        }
    };
    if state.version != STATE_VERSION {
        return Response::error(
            &request.id,
            "invalid_params",
            format!(
                "state schema {} is unsupported; expected {}",
                state.version, STATE_VERSION
            ),
        );
    }
    state.normalize();
    let _guard = lock_state(api);
    match state::save(&api.state_file, &state) {
        Ok(()) => {
            api.events
                .publish(EventKind::StateChanged, json!({"state": state}));
            Response::success(&request.id, json!({"saved": true}))
        }
        Err(error) => Response::error(&request.id, "state_write_failed", error.to_string()),
    }
}

fn reconcile_state(request: &Request, api: &ApiContext) -> Response {
    let ReconcileParams { projects } = match serde_json::from_value(request.params.clone()) {
        Ok(params) => params,
        Err(error) => {
            return Response::error(&request.id, "invalid_params", error.to_string());
        }
    };
    let _guard = lock_state(api);
    let mut state = match state::load(&api.state_file) {
        Ok(state) => state,
        Err(error) => {
            return Response::error(&request.id, "state_read_failed", error.to_string());
        }
    };
    let result = match reconcile::reconcile_all(
        &api.server,
        &projects,
        &state.recorded_session_ids(),
        &state.ignored_sessions,
    ) {
        Ok(result) => result,
        Err(error) => {
            return Response::error(&request.id, "reconcile_failed", error.to_string());
        }
    };
    let state_changed = record_live_sessions(&mut state, &result);
    if state_changed {
        if let Err(error) = state::save(&api.state_file, &state) {
            return Response::error(&request.id, "state_write_failed", error.to_string());
        }
        api.events
            .publish(EventKind::StateChanged, json!({"state": state.clone()}));
    }
    api.events.publish(
        EventKind::ReconciliationCompleted,
        json!({"reconciliation": result.clone(), "state": state.clone()}),
    );
    match serde_json::to_value(ReconcileResult {
        reconciliation: &result,
        state: &state,
    }) {
        Ok(value) => Response::success(&request.id, value),
        Err(error) => Response::error(&request.id, "internal_error", error.to_string()),
    }
}

fn lock_state(api: &ApiContext) -> std::sync::MutexGuard<'_, ()> {
    api.state_gate
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn record_live_sessions(state: &mut State, result: &Reconciliation) -> bool {
    let before = state.sessions.clone();
    let now = grove_core::workflow::now_epoch();
    for project in &result.projects {
        for worktree in &project.worktrees {
            if worktree.session.exists() {
                state.record_session(SessionRecord {
                    worktree_id: worktree.id.clone(),
                    project_id: project.id.clone(),
                    worktree_path: worktree.path.clone(),
                    session_name: worktree.session_name(),
                    last_activity_at: now,
                });
            }
        }
    }
    before.len() != state.sessions.len()
        || before.iter().zip(&state.sessions).any(|(a, b)| {
            a.worktree_id != b.worktree_id
                || a.project_id != b.project_id
                || a.session_name != b.session_name
                || a.worktree_path != b.worktree_path
        })
}

fn with_state(
    request: &Request,
    api: &ApiContext,
    operation: impl FnOnce(
        &grove_core::state::State,
    ) -> std::result::Result<serde_json::Value, Box<dyn std::error::Error>>,
) -> Response {
    api_result(request, || {
        let state = state::load(&api.state_file)?;
        operation(&state)
    })
}

fn api_result(
    request: &Request,
    operation: impl FnOnce() -> std::result::Result<serde_json::Value, Box<dyn std::error::Error>>,
) -> Response {
    match operation() {
        Ok(result) => Response::success(&request.id, result),
        Err(error) => Response::error(&request.id, "internal_error", error.to_string()),
    }
}

fn lock(service: &Arc<Mutex<Service>>) -> std::sync::MutexGuard<'_, Service> {
    service
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Ensure a service is accepting commands, starting this executable in
/// service mode when necessary. Startup is intentionally non-blocking: the
/// durable tmux attention marker still covers reports during the short race.
pub fn ensure_running(paths: &Paths) -> Result<()> {
    if ipc::send_command(&paths.notify_socket(), &Command::Ping)? {
        return Ok(());
    }
    let executable = std::env::current_exe()
        .map_err(|error| grove_core::Error::io("locate the Grove executable", error))?;
    Invocation::new(executable).arg("serve").spawn_detached()
}

struct Service {
    gui_socket: std::path::PathBuf,
    pending_notifications: HashMap<String, Notification>,
    pending_slot: Option<u8>,
    gui_launching_since: Option<Instant>,
    events: Arc<EventHub>,
}

impl Service {
    fn new(paths: &Paths, events: Arc<EventHub>) -> Self {
        Self {
            gui_socket: paths.gui_socket(),
            pending_notifications: HashMap::new(),
            pending_slot: None,
            gui_launching_since: None,
            events,
        }
    }

    fn handle(&mut self, command: Command) {
        match command {
            Command::Ping => {}
            Command::GuiReady => {
                self.gui_launching_since = None;
                self.flush();
            }
            Command::Notify(notification) => {
                let delivered = self.events.publish(
                    EventKind::NotificationReceived,
                    json!({"notification": notification.clone()}),
                );
                if delivered.delivered_to_gui {
                    return;
                }
                if !self.forward(&Command::Notify(notification.clone())) {
                    // Only the latest report per worktree matters. Attention
                    // itself is additionally durable in tmux.
                    self.pending_notifications
                        .insert(notification.worktree_id.clone(), notification);
                }
            }
            Command::Toggle { slot } => {
                if self.forward(&Command::Toggle { slot }) {
                    return;
                }
                self.pending_slot = slot;
                if let Err(error) = self.launch_gui() {
                    eprintln!("grove serve: could not launch Grove: {error}");
                }
            }
        }
    }

    fn forward(&self, command: &Command) -> bool {
        match ipc::send_command(&self.gui_socket, command) {
            Ok(delivered) => delivered,
            Err(error) => {
                eprintln!("grove serve: could not reach the GUI: {error}");
                false
            }
        }
    }

    fn flush(&mut self) {
        let pending = std::mem::take(&mut self.pending_notifications);
        for (id, notification) in pending {
            if !self.forward(&Command::Notify(notification.clone())) {
                self.pending_notifications.insert(id, notification);
            }
        }
        if let Some(slot) = self.pending_slot {
            if self.forward(&Command::Toggle { slot: Some(slot) }) {
                self.pending_slot = None;
            }
        }
    }

    fn launch_gui(&mut self) -> Result<()> {
        if self
            .gui_launching_since
            .is_some_and(|since| since.elapsed() < GUI_LAUNCH_GRACE)
        {
            return Ok(());
        }
        let executable = std::env::current_exe()
            .map_err(|error| grove_core::Error::io("locate the Grove executable", error))?;
        Invocation::new(executable).spawn_detached()?;
        self.gui_launching_since = Some(Instant::now());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use grove_core::status::SessionStatus;

    fn paths(root: &Path) -> Paths {
        Paths {
            config_dir: root.join("config"),
            state_dir: root.join("state"),
            runtime_dir: root.join("run"),
        }
    }

    #[test]
    fn notifications_queue_by_worktree_and_latest_wins() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut service = Service::new(&paths(temp.path()), Arc::new(EventHub::default()));
        service.handle(Command::Notify(Notification::new(
            "abc123",
            SessionStatus::Working,
        )));
        service.handle(Command::Notify(Notification::new(
            "abc123",
            SessionStatus::Idle,
        )));
        assert_eq!(service.pending_notifications.len(), 1);
        assert_eq!(
            service.pending_notifications["abc123"].state,
            SessionStatus::Idle
        );
    }

    #[test]
    fn a_gui_ready_flushes_queued_notifications() {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = paths(temp.path());
        std::fs::create_dir_all(&paths.runtime_dir).expect("mkdir");
        let listener = ipc::bind(&paths.gui_socket()).expect("bind");
        let mut service = Service::new(&paths, Arc::new(EventHub::default()));
        service.pending_notifications.insert(
            "abc123".into(),
            Notification::new("abc123", SessionStatus::Attention),
        );

        service.handle(Command::GuiReady);
        let (stream, _) = listener.accept().expect("accept");
        assert!(matches!(
            ipc::read_command(stream).expect("command"),
            Command::Notify(_)
        ));
        assert!(service.pending_notifications.is_empty());
    }

    #[test]
    fn api_dispatch_reports_versions_and_unknown_methods() {
        let temp = tempfile::tempdir().expect("tempdir");
        let api = ApiContext::new(&paths(temp.path()), Arc::new(EventHub::default()));
        let ping = dispatch_api(&Request::new("one", "ping", json!({})), &api);
        assert!(ping.ok);
        assert_eq!(ping.result.expect("result")["protocol"], protocol::VERSION);

        let unknown = dispatch_api(&Request::new("two", "missing", json!({})), &api);
        assert!(!unknown.ok);
        assert_eq!(unknown.error.expect("error").code, "method_not_found");

        let invalid = dispatch_api(
            &Request::new("params", "ping", json!({"unexpected": true})),
            &api,
        );
        assert!(!invalid.ok);
        assert_eq!(invalid.error.expect("error").code, "invalid_params");

        let mut future = Request::new("three", "ping", json!({}));
        future.protocol = protocol::VERSION + 1;
        let unsupported = dispatch_api(&future, &api);
        assert!(!unsupported.ok);
        assert_eq!(
            unsupported.error.expect("error").code,
            "unsupported_protocol"
        );
    }

    #[test]
    fn a_slow_subscriber_is_dropped_without_blocking_publishers() {
        let events = EventHub::default();
        let (id, _receiver, _) = events.subscribe(HashSet::from([EventKind::StateChanged]), false);
        for revision in 0..SUBSCRIBER_QUEUE {
            let outcome = events.publish(EventKind::StateChanged, json!({"n": revision}));
            assert!(outcome.delivered);
        }
        let overflow = events.publish(EventKind::StateChanged, json!({"n": "overflow"}));
        assert!(!overflow.delivered);
        assert!(
            !events.unsubscribe(&id),
            "the full subscriber was already isolated"
        );
    }

    #[test]
    fn only_a_gui_subscription_replaces_legacy_notification_delivery() {
        let events = EventHub::default();
        let (_observer, observer_events, _) =
            events.subscribe(HashSet::from([EventKind::NotificationReceived]), false);
        let observed = events.publish(EventKind::NotificationReceived, json!({}));
        assert!(observed.delivered);
        assert!(!observed.delivered_to_gui);
        drop(observer_events);

        let (_gui, _gui_events, _) =
            events.subscribe(HashSet::from([EventKind::NotificationReceived]), true);
        let delivered = events.publish(EventKind::NotificationReceived, json!({}));
        assert!(delivered.delivered_to_gui);
    }
}
