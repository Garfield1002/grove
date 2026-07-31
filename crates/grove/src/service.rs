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

use grove_core::git::WorktreeAdd;
use grove_core::ipc::{self, Command, Notification};
use grove_core::model::Worktree;
use grove_core::paths::ensure_private_dir;
use grove_core::process::Invocation;
use grove_core::protocol::{self, Event, EventKind, Request, Response};
use grove_core::reconcile::{self, ProjectRef, Reconciliation};
use grove_core::state::{AgentRecord, Mutation, ProjectRecord, SessionRecord, State};
use grove_core::status;
use grove_core::{Paths, Result, TmuxServer, config, git, query, state, terminal, tmux, workflow};
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
    let state_gate = Arc::new(Mutex::new(()));
    let service = Arc::new(Mutex::new(Service::with_state_gate(
        paths,
        Arc::clone(&events),
        Arc::clone(&state_gate),
    )));
    let api = Arc::new(ApiContext::with_state_gate(paths, events, state_gate));
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
    paths: Paths,
    state_file: std::path::PathBuf,
    server: TmuxServer,
    state_gate: Arc<Mutex<()>>,
    control_gate: Mutex<()>,
    idempotency: Mutex<HashMap<String, Response>>,
    events: Arc<EventHub>,
}

impl ApiContext {
    #[cfg(test)]
    fn new(paths: &Paths, events: Arc<EventHub>) -> Self {
        Self::with_state_gate(paths, events, Arc::new(Mutex::new(())))
    }

    fn with_state_gate(paths: &Paths, events: Arc<EventHub>, state_gate: Arc<Mutex<()>>) -> Self {
        Self {
            paths: paths.clone(),
            state_file: paths.state_file(),
            server: TmuxServer::new(paths.tmux_socket()).with_config(paths.tmux_config_file()),
            state_gate,
            control_gate: Mutex::new(()),
            idempotency: Mutex::new(HashMap::new()),
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
                Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => false,
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
struct MutateStateParams {
    mutation: Mutation,
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

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ControlParams {
    worktree_id: String,
    #[serde(default)]
    idempotency_key: Option<String>,
    #[serde(default)]
    resume: Option<String>,
    #[serde(default)]
    window_index: Option<u32>,
    #[serde(default)]
    orphan_session: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusParams {
    worktree_id: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ResumeRecordedParams {
    idempotency_key: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CloseSessionParams {
    session: String,
    idempotency_key: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenOrphanParams {
    session: String,
    idempotency_key: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StopServerParams {
    idempotency_key: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenProjectParams {
    path: std::path::PathBuf,
    idempotency_key: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RefreshProjectParams {
    project_id: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectStatusesParams {
    project_id: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectRefsParams {
    project_id: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct InspectRemovalParams {
    worktree_id: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectExpandedParams {
    project_id: String,
    expanded: bool,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectRemoveParams {
    project_id: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SlotAssignParams {
    number: u8,
    worktree_id: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct WorktreeIdentityParams {
    worktree_id: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionIgnoreParams {
    session: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateWorktreeParams {
    project_id: String,
    add: WorktreeAdd,
    idempotency_key: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoveWorktreeParams {
    worktree_id: String,
    force: bool,
    idempotency_key: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteBranchParams {
    project_id: String,
    branch: String,
    force: bool,
    idempotency_key: String,
}

#[derive(Clone, serde::Serialize)]
struct ResumeRecordedResult {
    worktree_ids: Vec<String>,
    failures: Vec<ResumeFailure>,
}

#[derive(Clone, serde::Serialize)]
struct ResumeFailure {
    worktree_path: std::path::PathBuf,
    message: String,
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
        "state.mutate"
            | "state.reconcile"
            | "event.subscribe"
            | "event.unsubscribe"
            | "session.ensure"
            | "session.attention.clear"
            | "session.associate"
            | "session.close"
            | "session.open"
            | "session.orphan.open"
            | "session.terminal.open"
            | "session.window.open"
            | "session.window.create"
            | "session.worktree.close"
            | "server.stop"
            | "project.open"
            | "project.refresh"
            | "project.statuses"
            | "project.refs"
            | "removal.inspect"
            | "project.expanded.set"
            | "project.remove"
            | "slot.assign"
            | "slot.clear"
            | "session.ignore"
            | "worktree.create"
            | "worktree.remove"
            | "branch.delete"
            | "agent.start"
            | "agent.resume_recorded"
            | "status.get"
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
        "project.refresh" => refresh_project(request, api),
        "project.statuses" => project_statuses(request, api),
        "project.refs" => project_refs(request, api),
        "project.expanded.set" => set_project_expanded(request, api),
        "project.remove" => remove_project(request, api),
        "slot.assign" => assign_slot(request, api),
        "slot.clear" => clear_slot(request, api),
        "session.ignore" => ignore_session(request, api),
        "session.ignored.clear" => apply_intent(request, api, Mutation::ClearIgnoredSessions),
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
        "session.refresh" => api_result(request, || {
            Ok(json!({
                "presence": workflow::session_presence(&api.server)?,
                "windows": workflow::session_windows(&api.server)?,
            }))
        }),
        "status.poll" => api_result(request, || {
            Ok(serde_json::to_value(workflow::poll_session_signals(
                &api.server,
                workflow::now_epoch(),
            )?)?)
        }),
        "state.get" => with_state(request, api, |state| Ok(serde_json::to_value(state)?)),
        "state.mutate" => mutate_state(request, api),
        "state.reconcile" => reconcile_state(request, api),
        "event.unsubscribe" => unsubscribe(request, api),
        "session.ensure"
        | "session.attention.clear"
        | "session.associate"
        | "session.open"
        | "session.terminal.open"
        | "session.window.open"
        | "session.window.create"
        | "session.worktree.close"
        | "agent.start" => control(request, api),
        "session.close" => close_session(request, api),
        "session.orphan.open" => open_orphan_session(request, api),
        "server.stop" => stop_server(request, api),
        "project.open" => open_project(request, api),
        "worktree.create" => create_worktree(request, api),
        "worktree.remove" => remove_worktree(request, api),
        "branch.delete" => delete_branch(request, api),
        "removal.inspect" => inspect_removal(request, api),
        "agent.resume_recorded" => resume_recorded(request, api),
        "status.get" => status_get(request, api),
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

fn control(request: &Request, api: &ApiContext) -> Response {
    let ControlParams {
        worktree_id,
        idempotency_key,
        resume,
        window_index,
        orphan_session,
    } = match serde_json::from_value(request.params.clone()) {
        Ok(params) => params,
        Err(error) => {
            return Response::error(&request.id, "invalid_params", error.to_string());
        }
    };
    if worktree_id.is_empty() || worktree_id.chars().any(char::is_control) {
        return Response::error(
            &request.id,
            "invalid_params",
            "worktree_id must be a non-empty printable value",
        );
    }
    if let Some(key) = idempotency_key.as_deref()
        && (key.is_empty()
            || key.len() > protocol::MAX_REQUEST_ID_LEN
            || key.chars().any(char::is_control))
    {
        return Response::error(
            &request.id,
            "invalid_params",
            "idempotency_key must be 1-128 printable bytes",
        );
    }
    if resume.is_some() && request.method != "agent.start" {
        return Response::error(
            &request.id,
            "invalid_params",
            "resume is accepted only by agent.start",
        );
    }
    if window_index.is_some() && request.method != "session.window.open" {
        return Response::error(
            &request.id,
            "invalid_params",
            "window_index is accepted only by session.window.open",
        );
    }
    if request.method == "session.window.open" && window_index.is_none() {
        return Response::error(
            &request.id,
            "invalid_params",
            "session.window.open requires window_index",
        );
    }
    if orphan_session.is_some() && request.method != "session.associate" {
        return Response::error(
            &request.id,
            "invalid_params",
            "orphan_session is accepted only by session.associate",
        );
    }
    if request.method == "session.associate" && orphan_session.is_none() {
        return Response::error(
            &request.id,
            "invalid_params",
            "session.associate requires orphan_session",
        );
    }
    if let Some(session) = orphan_session.as_deref()
        && (session.is_empty()
            || session.len() > protocol::MAX_REQUEST_ID_LEN
            || session.chars().any(char::is_control))
    {
        return Response::error(
            &request.id,
            "invalid_params",
            "orphan_session must be 1-128 printable bytes",
        );
    }
    if let Some(conversation) = resume.as_deref()
        && (conversation.is_empty()
            || conversation.len() > protocol::MAX_REQUEST_ID_LEN
            || conversation.chars().any(char::is_control))
    {
        return Response::error(
            &request.id,
            "invalid_params",
            "resume must be 1-128 printable bytes",
        );
    }
    let cache_key = idempotency_key.as_ref().map(|key| {
        format!(
            "{}:{worktree_id}:{resume:?}:{window_index:?}:{orphan_session:?}:{key}",
            request.method
        )
    });
    let _control = api
        .control_gate
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(key) = cache_key.as_deref()
        && let Some(response) = api
            .idempotency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(key)
            .cloned()
    {
        let mut replay = response;
        replay.id = request.id.clone();
        return replay;
    }

    let result = match run_control(
        &request.method,
        &worktree_id,
        resume.as_deref(),
        window_index,
        orphan_session.as_deref(),
        api,
    ) {
        Ok(result) => result,
        Err(error) => {
            return Response::error(&request.id, "control_failed", error.to_string());
        }
    };
    api.events.publish(
        EventKind::ControlCompleted,
        json!({
            "method": request.method,
            "worktree_id": worktree_id,
            "result": result.clone(),
        }),
    );
    let response = Response::success(&request.id, result);
    if let Some(key) = cache_key {
        api.idempotency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(key, response.clone());
    }
    response
}

fn close_session(request: &Request, api: &ApiContext) -> Response {
    let CloseSessionParams {
        session,
        idempotency_key,
    } = match serde_json::from_value(request.params.clone()) {
        Ok(params) => params,
        Err(error) => {
            return Response::error(&request.id, "invalid_params", error.to_string());
        }
    };
    if session.is_empty()
        || session.len() > protocol::MAX_REQUEST_ID_LEN
        || session.chars().any(char::is_control)
    {
        return Response::error(
            &request.id,
            "invalid_params",
            "session must be 1-128 printable bytes",
        );
    }
    if idempotency_key.is_empty()
        || idempotency_key.len() > protocol::MAX_REQUEST_ID_LEN
        || idempotency_key.chars().any(char::is_control)
    {
        return Response::error(
            &request.id,
            "invalid_params",
            "idempotency_key must be 1-128 printable bytes",
        );
    }
    let cache_key = format!("session.close:{session}:{idempotency_key}");
    let _control = api
        .control_gate
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(response) = api
        .idempotency
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&cache_key)
        .cloned()
    {
        let mut replay = response;
        replay.id = request.id.clone();
        return replay;
    }
    if let Err(error) = tmux::session::kill_session(&api.server, &session) {
        return Response::error(&request.id, "control_failed", error.to_string());
    }
    let result = json!({"session": session});
    api.events.publish(
        EventKind::ControlCompleted,
        json!({
            "method": request.method,
            "session": session,
            "result": result.clone(),
        }),
    );
    let response = Response::success(&request.id, result);
    api.idempotency
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(cache_key, response.clone());
    response
}

fn open_orphan_session(request: &Request, api: &ApiContext) -> Response {
    let OpenOrphanParams {
        session,
        idempotency_key,
    } = match serde_json::from_value(request.params.clone()) {
        Ok(params) => params,
        Err(error) => {
            return Response::error(&request.id, "invalid_params", error.to_string());
        }
    };
    if session.is_empty()
        || session.len() > protocol::MAX_REQUEST_ID_LEN
        || session.chars().any(char::is_control)
    {
        return Response::error(
            &request.id,
            "invalid_params",
            "session must be 1-128 printable bytes",
        );
    }
    if idempotency_key.is_empty()
        || idempotency_key.len() > protocol::MAX_REQUEST_ID_LEN
        || idempotency_key.chars().any(char::is_control)
    {
        return Response::error(
            &request.id,
            "invalid_params",
            "idempotency_key must be 1-128 printable bytes",
        );
    }
    let cache_key = format!("session.orphan.open:{session}:{idempotency_key}");
    let _control = api
        .control_gate
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(response) = api
        .idempotency
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&cache_key)
        .cloned()
    {
        let mut replay = response;
        replay.id = request.id.clone();
        return replay;
    }
    let result = (|| -> std::result::Result<serde_json::Value, Box<dyn std::error::Error>> {
        let listed = tmux::list_sessions(&api.server)?;
        let found = listed
            .iter()
            .find(|candidate| candidate.name == session)
            .ok_or_else(|| format!("unknown session `{session}`"))?;
        let loaded = config::load_or_init(&api.paths.config_file(), terminal::detect)?;
        let activation =
            workflow::open_session(&api.server, &loaded.config, &session, &found.path)?;
        Ok(json!({"session": session, "activation": activation}))
    })();
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            return Response::error(&request.id, "control_failed", error.to_string());
        }
    };
    api.events.publish(
        EventKind::ControlCompleted,
        json!({
            "method": request.method,
            "session": session,
            "result": result.clone(),
        }),
    );
    let response = Response::success(&request.id, result);
    api.idempotency
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(cache_key, response.clone());
    response
}

fn stop_server(request: &Request, api: &ApiContext) -> Response {
    let StopServerParams { idempotency_key } = match serde_json::from_value(request.params.clone())
    {
        Ok(params) => params,
        Err(error) => {
            return Response::error(&request.id, "invalid_params", error.to_string());
        }
    };
    if idempotency_key.is_empty()
        || idempotency_key.len() > protocol::MAX_REQUEST_ID_LEN
        || idempotency_key.chars().any(char::is_control)
    {
        return Response::error(
            &request.id,
            "invalid_params",
            "idempotency_key must be 1-128 printable bytes",
        );
    }
    let cache_key = format!("server.stop:{idempotency_key}");
    let _control = api
        .control_gate
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(response) = api
        .idempotency
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&cache_key)
        .cloned()
    {
        let mut replay = response;
        replay.id = request.id.clone();
        return replay;
    }
    if let Err(error) = api.server.kill_server() {
        return Response::error(&request.id, "control_failed", error.to_string());
    }
    let result = json!({"stopped": true});
    api.events.publish(
        EventKind::ControlCompleted,
        json!({"method": request.method, "result": result.clone()}),
    );
    let response = Response::success(&request.id, result);
    api.idempotency
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(cache_key, response.clone());
    response
}

fn valid_identity(value: &str) -> bool {
    !value.is_empty() && !value.chars().any(char::is_control)
}

fn set_project_expanded(request: &Request, api: &ApiContext) -> Response {
    let ProjectExpandedParams {
        project_id,
        expanded,
    } = match serde_json::from_value(request.params.clone()) {
        Ok(params) => params,
        Err(error) => {
            return Response::error(&request.id, "invalid_params", error.to_string());
        }
    };
    if !valid_identity(&project_id) {
        return Response::error(
            &request.id,
            "invalid_params",
            "project_id must be a non-empty printable value",
        );
    }
    apply_intent(
        request,
        api,
        Mutation::SetProjectExpanded {
            project_id,
            expanded,
        },
    )
}

fn remove_project(request: &Request, api: &ApiContext) -> Response {
    let ProjectRemoveParams { project_id } = match serde_json::from_value(request.params.clone()) {
        Ok(params) => params,
        Err(error) => {
            return Response::error(&request.id, "invalid_params", error.to_string());
        }
    };
    if !valid_identity(&project_id) {
        return Response::error(
            &request.id,
            "invalid_params",
            "project_id must be a non-empty printable value",
        );
    }
    apply_intent(request, api, Mutation::RemoveProject { project_id })
}

fn assign_slot(request: &Request, api: &ApiContext) -> Response {
    let SlotAssignParams {
        number,
        worktree_id,
    } = match serde_json::from_value(request.params.clone()) {
        Ok(params) => params,
        Err(error) => {
            return Response::error(&request.id, "invalid_params", error.to_string());
        }
    };
    if !(1..=grove_core::state::MAX_SLOT).contains(&number) || !valid_identity(&worktree_id) {
        return Response::error(
            &request.id,
            "invalid_params",
            "slot number and worktree_id must identify a valid assignment",
        );
    }
    apply_intent(
        request,
        api,
        Mutation::AssignSlot {
            number,
            worktree_id,
        },
    )
}

fn clear_slot(request: &Request, api: &ApiContext) -> Response {
    let WorktreeIdentityParams { worktree_id } =
        match serde_json::from_value(request.params.clone()) {
            Ok(params) => params,
            Err(error) => {
                return Response::error(&request.id, "invalid_params", error.to_string());
            }
        };
    if !valid_identity(&worktree_id) {
        return Response::error(
            &request.id,
            "invalid_params",
            "worktree_id must be a non-empty printable value",
        );
    }
    apply_intent(request, api, Mutation::ClearSlot { worktree_id })
}

fn ignore_session(request: &Request, api: &ApiContext) -> Response {
    let SessionIgnoreParams { session } = match serde_json::from_value(request.params.clone()) {
        Ok(params) => params,
        Err(error) => {
            return Response::error(&request.id, "invalid_params", error.to_string());
        }
    };
    if !valid_identity(&session) {
        return Response::error(
            &request.id,
            "invalid_params",
            "session must be a non-empty printable value",
        );
    }
    apply_intent(
        request,
        api,
        Mutation::IgnoreSession {
            session_name: session,
        },
    )
}

fn apply_intent(request: &Request, api: &ApiContext, mutation: Mutation) -> Response {
    let _guard = lock_state(api);
    let mut current = match state::load(&api.state_file) {
        Ok(state) => state,
        Err(error) => {
            return Response::error(&request.id, "state_read_failed", error.to_string());
        }
    };
    let changed = current.apply(mutation);
    if changed && let Err(error) = state::save(&api.state_file, &current) {
        return Response::error(&request.id, "state_write_failed", error.to_string());
    }
    if changed {
        api.events
            .publish(EventKind::StateChanged, json!({"state": current.clone()}));
    }
    Response::success(&request.id, json!({"changed": changed, "state": current}))
}

fn project_refs(request: &Request, api: &ApiContext) -> Response {
    let ProjectRefsParams { project_id } = match serde_json::from_value(request.params.clone()) {
        Ok(params) => params,
        Err(error) => {
            return Response::error(&request.id, "invalid_params", error.to_string());
        }
    };
    if project_id.is_empty() || project_id.chars().any(char::is_control) {
        return Response::error(
            &request.id,
            "invalid_params",
            "project_id must be a non-empty printable value",
        );
    }
    api_result(request, || {
        let state = state::load(&api.state_file)?;
        let project = state
            .projects
            .iter()
            .find(|project| project.id == project_id)
            .ok_or_else(|| format!("unknown project `{project_id}`"))?;
        Ok(json!({
            "project_id": project.id,
            "refs": git::list_refs(&project.repository_path)?,
            "current": git::current_branch(&project.repository_path)?,
        }))
    })
}

fn inspect_removal(request: &Request, api: &ApiContext) -> Response {
    let InspectRemovalParams { worktree_id } = match serde_json::from_value(request.params.clone())
    {
        Ok(params) => params,
        Err(error) => {
            return Response::error(&request.id, "invalid_params", error.to_string());
        }
    };
    if worktree_id.is_empty() || worktree_id.chars().any(char::is_control) {
        return Response::error(
            &request.id,
            "invalid_params",
            "worktree_id must be a non-empty printable value",
        );
    }
    api_result(request, || {
        let (project, worktree) = resolve_worktree(api, &worktree_id)?;
        let inputs = workflow::removal_inputs(&api.server, &worktree)?;
        Ok(json!({
            "project_id": project.id,
            "worktree_id": worktree.id,
            "report": grove_core::removal::assemble(&inputs),
        }))
    })
}

fn project_statuses(request: &Request, api: &ApiContext) -> Response {
    let ProjectStatusesParams { project_id } = match serde_json::from_value(request.params.clone())
    {
        Ok(params) => params,
        Err(error) => {
            return Response::error(&request.id, "invalid_params", error.to_string());
        }
    };
    if project_id.is_empty() || project_id.chars().any(char::is_control) {
        return Response::error(
            &request.id,
            "invalid_params",
            "project_id must be a non-empty printable value",
        );
    }
    api_result(request, || {
        let state = state::load(&api.state_file)?;
        let project = state
            .projects
            .iter()
            .find(|project| project.id == project_id)
            .ok_or_else(|| format!("unknown project `{project_id}`"))?;
        let worktrees = workflow::refresh_project(
            &api.server,
            &project.repository_path,
            &project.id,
            &project.git_common_dir,
        )?;
        Ok(json!({
            "project_id": project.id,
            "statuses": workflow::worktree_statuses(&worktrees),
        }))
    })
}

fn refresh_project(request: &Request, api: &ApiContext) -> Response {
    let RefreshProjectParams { project_id } = match serde_json::from_value(request.params.clone()) {
        Ok(params) => params,
        Err(error) => {
            return Response::error(&request.id, "invalid_params", error.to_string());
        }
    };
    if project_id.is_empty() || project_id.chars().any(char::is_control) {
        return Response::error(
            &request.id,
            "invalid_params",
            "project_id must be a non-empty printable value",
        );
    }
    api_result(request, || {
        let state = state::load(&api.state_file)?;
        let project = state
            .projects
            .iter()
            .find(|project| project.id == project_id)
            .ok_or_else(|| format!("unknown project `{project_id}`"))?;
        let worktrees = workflow::refresh_project(
            &api.server,
            &project.repository_path,
            &project.id,
            &project.git_common_dir,
        )?;
        let statuses = workflow::worktree_statuses(&worktrees);
        Ok(json!({
            "project_id": project.id,
            "worktrees": worktrees,
            "statuses": statuses,
        }))
    })
}

fn open_project(request: &Request, api: &ApiContext) -> Response {
    let OpenProjectParams {
        path,
        idempotency_key,
    } = match serde_json::from_value(request.params.clone()) {
        Ok(params) => params,
        Err(error) => {
            return Response::error(&request.id, "invalid_params", error.to_string());
        }
    };
    if path.as_os_str().is_empty() {
        return Response::error(
            &request.id,
            "invalid_params",
            "project path must be non-empty",
        );
    }
    if idempotency_key.is_empty()
        || idempotency_key.len() > protocol::MAX_REQUEST_ID_LEN
        || idempotency_key.chars().any(char::is_control)
    {
        return Response::error(
            &request.id,
            "invalid_params",
            "idempotency_key must be 1-128 printable bytes",
        );
    }
    let cache_key = format!("project.open:{}:{idempotency_key}", path.to_string_lossy());
    let _control = api
        .control_gate
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(response) = api
        .idempotency
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&cache_key)
        .cloned()
    {
        let mut replay = response;
        replay.id = request.id.clone();
        return replay;
    }

    let loaded = match config::load_or_init(&api.paths.config_file(), terminal::detect) {
        Ok(loaded) => loaded,
        Err(error) => {
            return Response::error(&request.id, "control_failed", error.to_string());
        }
    };
    let project = match workflow::open_project(&api.server, &loaded.config, &path) {
        Ok(project) => project,
        Err(error) => {
            return Response::error(&request.id, "control_failed", error.to_string());
        }
    };
    let _state = lock_state(api);
    let mut state = match state::load(&api.state_file) {
        Ok(state) => state,
        Err(error) => {
            return Response::error(&request.id, "state_read_failed", error.to_string());
        }
    };
    let changed = state.apply(Mutation::UpsertProject {
        record: ProjectRecord {
            id: project.id.clone(),
            name: project.name.clone(),
            repository_path: project.repository_path.clone(),
            git_common_dir: project.git_common_dir.clone(),
            default_worktree_path: project.default_worktree_path.clone(),
            is_expanded: project.is_expanded,
        },
    });
    if changed && let Err(error) = state::save(&api.state_file, &state) {
        return Response::error(&request.id, "state_write_failed", error.to_string());
    }
    if changed {
        api.events
            .publish(EventKind::StateChanged, json!({"state": state.clone()}));
    }
    let response = Response::success(
        &request.id,
        json!({"changed": changed, "project": project, "state": state}),
    );
    api.idempotency
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(cache_key, response.clone());
    response
}

fn create_worktree(request: &Request, api: &ApiContext) -> Response {
    let CreateWorktreeParams {
        project_id,
        add,
        idempotency_key,
    } = match serde_json::from_value(request.params.clone()) {
        Ok(params) => params,
        Err(error) => {
            return Response::error(&request.id, "invalid_params", error.to_string());
        }
    };
    if project_id.is_empty()
        || project_id.chars().any(char::is_control)
        || add.path.as_os_str().is_empty()
    {
        return Response::error(
            &request.id,
            "invalid_params",
            "project_id and worktree path must be non-empty printable values",
        );
    }
    for value in [add.new_branch.as_deref(), add.base_ref.as_deref()]
        .into_iter()
        .flatten()
    {
        if value.is_empty() || value.len() > 1024 || value.chars().any(char::is_control) {
            return Response::error(
                &request.id,
                "invalid_params",
                "branch and base ref must be 1-1024 printable bytes when present",
            );
        }
    }
    if idempotency_key.is_empty()
        || idempotency_key.len() > protocol::MAX_REQUEST_ID_LEN
        || idempotency_key.chars().any(char::is_control)
    {
        return Response::error(
            &request.id,
            "invalid_params",
            "idempotency_key must be 1-128 printable bytes",
        );
    }
    let cache_key = format!("worktree.create:{project_id}:{idempotency_key}");
    let _control = api
        .control_gate
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(response) = api
        .idempotency
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&cache_key)
        .cloned()
    {
        let mut replay = response;
        replay.id = request.id.clone();
        return replay;
    }
    let result = (|| -> std::result::Result<serde_json::Value, Box<dyn std::error::Error>> {
        let current = state::load(&api.state_file)?;
        let project = current
            .projects
            .iter()
            .find(|project| project.id == project_id)
            .ok_or_else(|| format!("unknown project `{project_id}`"))?;
        let path = workflow::create_worktree(&project.repository_path, &add)?;
        let worktrees = workflow::refresh_project(
            &api.server,
            &project.repository_path,
            &project.id,
            &project.git_common_dir,
        )?;
        Ok(json!({"project_id": project_id, "path": path, "worktrees": worktrees}))
    })();
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            return Response::error(&request.id, "control_failed", error.to_string());
        }
    };
    api.events.publish(
        EventKind::ControlCompleted,
        json!({
            "method": request.method,
            "project_id": project_id,
            "result": result.clone(),
        }),
    );
    let response = Response::success(&request.id, result);
    api.idempotency
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(cache_key, response.clone());
    response
}

fn remove_worktree(request: &Request, api: &ApiContext) -> Response {
    let RemoveWorktreeParams {
        worktree_id,
        force,
        idempotency_key,
    } = match serde_json::from_value(request.params.clone()) {
        Ok(params) => params,
        Err(error) => {
            return Response::error(&request.id, "invalid_params", error.to_string());
        }
    };
    if worktree_id.is_empty() || worktree_id.chars().any(char::is_control) {
        return Response::error(
            &request.id,
            "invalid_params",
            "worktree_id must be a non-empty printable value",
        );
    }
    if idempotency_key.is_empty()
        || idempotency_key.len() > protocol::MAX_REQUEST_ID_LEN
        || idempotency_key.chars().any(char::is_control)
    {
        return Response::error(
            &request.id,
            "invalid_params",
            "idempotency_key must be 1-128 printable bytes",
        );
    }
    let cache_key = format!("worktree.remove:{worktree_id}:{force}:{idempotency_key}");
    let _control = api
        .control_gate
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(response) = api
        .idempotency
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&cache_key)
        .cloned()
    {
        let mut replay = response;
        replay.id = request.id.clone();
        return replay;
    }
    let result = (|| -> std::result::Result<serde_json::Value, Box<dyn std::error::Error>> {
        let (project, worktree) = resolve_worktree(api, &worktree_id)?;
        git::worktree_remove(&project.repository_path, &worktree.path, force)?;
        let worktrees = workflow::refresh_project(
            &api.server,
            &project.repository_path,
            &project.id,
            &project.git_common_dir,
        )?;
        Ok(json!({
            "project_id": project.id,
            "worktree_id": worktree_id,
            "path": worktree.path,
            "worktrees": worktrees,
        }))
    })();
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            return Response::error(&request.id, "control_failed", error.to_string());
        }
    };
    api.events.publish(
        EventKind::ControlCompleted,
        json!({
            "method": request.method,
            "worktree_id": worktree_id,
            "result": result.clone(),
        }),
    );
    let response = Response::success(&request.id, result);
    api.idempotency
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(cache_key, response.clone());
    response
}

fn delete_branch(request: &Request, api: &ApiContext) -> Response {
    let DeleteBranchParams {
        project_id,
        branch,
        force,
        idempotency_key,
    } = match serde_json::from_value(request.params.clone()) {
        Ok(params) => params,
        Err(error) => {
            return Response::error(&request.id, "invalid_params", error.to_string());
        }
    };
    if project_id.is_empty()
        || project_id.chars().any(char::is_control)
        || branch.is_empty()
        || branch.len() > 1024
        || branch.chars().any(char::is_control)
    {
        return Response::error(
            &request.id,
            "invalid_params",
            "project_id and branch must be non-empty printable values",
        );
    }
    if idempotency_key.is_empty()
        || idempotency_key.len() > protocol::MAX_REQUEST_ID_LEN
        || idempotency_key.chars().any(char::is_control)
    {
        return Response::error(
            &request.id,
            "invalid_params",
            "idempotency_key must be 1-128 printable bytes",
        );
    }
    let cache_key = format!("branch.delete:{project_id}:{branch}:{force}:{idempotency_key}");
    let _control = api
        .control_gate
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(response) = api
        .idempotency
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&cache_key)
        .cloned()
    {
        let mut replay = response;
        replay.id = request.id.clone();
        return replay;
    }
    let result = (|| -> std::result::Result<serde_json::Value, Box<dyn std::error::Error>> {
        let current = state::load(&api.state_file)?;
        let project = current
            .projects
            .iter()
            .find(|project| project.id == project_id)
            .ok_or_else(|| format!("unknown project `{project_id}`"))?;
        git::branch_delete(&project.repository_path, &branch, force)?;
        let worktrees = workflow::refresh_project(
            &api.server,
            &project.repository_path,
            &project.id,
            &project.git_common_dir,
        )?;
        Ok(json!({
            "project_id": project.id,
            "branch": branch,
            "worktrees": worktrees,
        }))
    })();
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            return Response::error(&request.id, "control_failed", error.to_string());
        }
    };
    api.events.publish(
        EventKind::ControlCompleted,
        json!({
            "method": request.method,
            "project_id": project_id,
            "result": result.clone(),
        }),
    );
    let response = Response::success(&request.id, result);
    api.idempotency
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(cache_key, response.clone());
    response
}

fn run_control(
    method: &str,
    worktree_id: &str,
    resume: Option<&str>,
    window_index: Option<u32>,
    orphan_session: Option<&str>,
    api: &ApiContext,
) -> std::result::Result<serde_json::Value, Box<dyn std::error::Error>> {
    let (project, worktree) = resolve_worktree(api, worktree_id)?;
    let spec = workflow::session_spec(&project.name, &project.git_common_dir, &worktree);
    match method {
        "session.attention.clear" => {
            let session = worktree.session_name();
            let cleared = tmux::session::clear_attention(&api.server, &session)?;
            Ok(json!({
                "worktree_id": worktree.id,
                "session": session,
                "cleared": cleared,
            }))
        }
        "session.associate" => {
            let orphan = orphan_session.ok_or("session.associate requires orphan_session")?;
            let session = workflow::associate_session(
                &api.server,
                &project.name,
                &project.git_common_dir,
                &worktree,
                orphan,
            )?;
            record_control_session(api, &project.id, &worktree, &session)?;
            Ok(json!({
                "worktree_id": worktree.id,
                "session": session,
            }))
        }
        "session.ensure" => {
            let (session, created) = tmux::ensure_session(&api.server, &spec)?;
            record_control_session(api, &project.id, &worktree, &session)?;
            Ok(json!({
                "worktree_id": worktree.id,
                "session": session,
                "created": created,
            }))
        }
        "session.open" => {
            let loaded = config::load_or_init(&api.paths.config_file(), terminal::detect)?;
            let activation = workflow::activate_worktree(
                &api.server,
                &loaded.config,
                &project.name,
                &project.git_common_dir,
                &worktree,
            )?;
            record_control_session(api, &project.id, &worktree, activation.session())?;
            Ok(json!({
                "worktree_id": worktree.id,
                "session": activation.session(),
                "activation": activation,
            }))
        }
        "session.window.open" => {
            let loaded = config::load_or_init(&api.paths.config_file(), terminal::detect)?;
            let index = window_index.ok_or("session.window.open requires window_index")?;
            let activation = workflow::activate_window(
                &api.server,
                &loaded.config,
                &project.name,
                &project.git_common_dir,
                &worktree,
                index,
            )?;
            record_control_session(api, &project.id, &worktree, activation.session())?;
            Ok(json!({
                "worktree_id": worktree.id,
                "session": activation.session(),
                "window_index": index,
                "activation": activation,
            }))
        }
        "session.terminal.open" => {
            let loaded = config::load_or_init(&api.paths.config_file(), terminal::detect)?;
            let activation = workflow::open_in_new_terminal(
                &api.server,
                &loaded.config,
                &project.name,
                &project.git_common_dir,
                &worktree,
            )?;
            record_control_session(api, &project.id, &worktree, activation.session())?;
            Ok(json!({
                "worktree_id": worktree.id,
                "session": activation.session(),
                "activation": activation,
            }))
        }
        "session.window.create" => {
            let window = workflow::open_new_window(
                &api.server,
                &project.name,
                &project.git_common_dir,
                &worktree,
            )?;
            record_control_session(api, &project.id, &worktree, &window.session)?;
            Ok(json!({
                "worktree_id": worktree.id,
                "session": window.session,
                "window": window,
            }))
        }
        "session.worktree.close" => {
            let session = worktree.session_name();
            tmux::session::kill_session(&api.server, &session)?;
            forget_control_session(api, &worktree.id)?;
            Ok(json!({
                "worktree_id": worktree.id,
                "session": session,
            }))
        }
        "agent.start" => {
            let loaded = config::load_or_init(&api.paths.config_file(), terminal::detect)?;
            let start = resume.map_or(workflow::AgentStart::Fresh, workflow::AgentStart::Resume);
            let launch = workflow::start_agent(
                &api.server,
                &loaded.config,
                &api.paths.runtime_dir,
                &project.name,
                &project.git_common_dir,
                &worktree,
                start,
            )?;
            let session = worktree.session_name();
            record_control_session(api, &project.id, &worktree, &session)?;
            Ok(json!({
                "worktree_id": worktree.id,
                "session": session,
                "unit": launch.unit,
            }))
        }
        _ => Err(format!("unknown control method `{method}`").into()),
    }
}

fn resolve_worktree(
    api: &ApiContext,
    worktree_id: &str,
) -> std::result::Result<(grove_core::state::ProjectRecord, Worktree), Box<dyn std::error::Error>> {
    let state = state::load(&api.state_file)?;
    for project in state.projects {
        let Ok(entries) = git::worktree_list(&project.repository_path) else {
            continue;
        };
        for (index, entry) in entries.iter().enumerate() {
            let worktree =
                Worktree::from_entry(entry, &project.id, &project.git_common_dir, index == 0);
            if worktree.id == worktree_id {
                return Ok((project, worktree));
            }
        }
    }
    Err(format!("unknown worktree `{worktree_id}`").into())
}

fn record_control_session(
    api: &ApiContext,
    project_id: &str,
    worktree: &Worktree,
    session: &str,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let _state = lock_state(api);
    let mut state = state::load(&api.state_file)?;
    state.record_session(SessionRecord {
        worktree_id: worktree.id.clone(),
        project_id: project_id.to_string(),
        worktree_path: worktree.path.clone(),
        session_name: session.to_string(),
        last_activity_at: workflow::now_epoch(),
    });
    state::save(&api.state_file, &state)?;
    api.events
        .publish(EventKind::StateChanged, json!({"state": state}));
    Ok(())
}

fn forget_control_session(
    api: &ApiContext,
    worktree_id: &str,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let _guard = lock_state(api);
    let mut current = state::load(&api.state_file)?;
    if current.apply(Mutation::ForgetSession {
        worktree_id: worktree_id.to_string(),
    }) {
        state::save(&api.state_file, &current)?;
        api.events
            .publish(EventKind::StateChanged, json!({"state": current.clone()}));
    }
    Ok(())
}

fn status_get(request: &Request, api: &ApiContext) -> Response {
    let StatusParams { worktree_id } = match serde_json::from_value(request.params.clone()) {
        Ok(params) => params,
        Err(error) => {
            return Response::error(&request.id, "invalid_params", error.to_string());
        }
    };
    api_result(request, || {
        // A typo must not masquerade as a stopped session and satisfy a wait.
        let _ = resolve_worktree(api, &worktree_id)?;
        let loaded = config::load_or_init(&api.paths.config_file(), terminal::detect)?;
        let signals = workflow::poll_session_signals(&api.server, workflow::now_epoch())?;
        let value = match signals.get(&worktree_id) {
            Some(signals) => {
                let current = status::classify(signals, &loaded.config.status.policy());
                json!({"worktree_id": worktree_id, "status": current})
            }
            None => json!({"worktree_id": worktree_id, "status": "stopped"}),
        };
        Ok(value)
    })
}

fn resume_recorded(request: &Request, api: &ApiContext) -> Response {
    let ResumeRecordedParams { idempotency_key } =
        match serde_json::from_value(request.params.clone()) {
            Ok(params) => params,
            Err(error) => {
                return Response::error(&request.id, "invalid_params", error.to_string());
            }
        };
    if idempotency_key.is_empty()
        || idempotency_key.len() > protocol::MAX_REQUEST_ID_LEN
        || idempotency_key.chars().any(char::is_control)
    {
        return Response::error(
            &request.id,
            "invalid_params",
            "idempotency_key must be 1-128 printable bytes",
        );
    }

    let cache_key = format!("agent.resume_recorded:{idempotency_key}");
    let _control = api
        .control_gate
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(response) = api
        .idempotency
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&cache_key)
        .cloned()
    {
        let mut replay = response;
        replay.id = request.id.clone();
        return replay;
    }

    let result = match run_resume_recorded(api) {
        Ok(result) => result,
        Err(error) => {
            return Response::error(&request.id, "resume_failed", error.to_string());
        }
    };
    let response = match serde_json::to_value(result) {
        Ok(value) => Response::success(&request.id, value),
        Err(error) => Response::error(&request.id, "internal_error", error.to_string()),
    };
    if response.ok {
        api.idempotency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(cache_key, response.clone());
    }
    response
}

fn run_resume_recorded(
    api: &ApiContext,
) -> std::result::Result<ResumeRecordedResult, Box<dyn std::error::Error>> {
    let loaded = config::load_or_init(&api.paths.config_file(), terminal::detect)?;
    if !loaded.config.agents.resume_on_startup {
        return Ok(ResumeRecordedResult {
            worktree_ids: Vec::new(),
            failures: Vec::new(),
        });
    }
    let state = state::load(&api.state_file)?;
    if state.agents.is_empty() {
        return Ok(ResumeRecordedResult {
            worktree_ids: Vec::new(),
            failures: Vec::new(),
        });
    }
    let projects = state
        .projects
        .iter()
        .map(|project| ProjectRef {
            id: project.id.clone(),
            name: project.name.clone(),
            repository_path: project.repository_path.clone(),
            git_common_dir: project.git_common_dir.clone(),
        })
        .collect::<Vec<_>>();
    let reconciliation = reconcile::reconcile_all(
        &api.server,
        &projects,
        &state.recorded_session_ids(),
        &state.ignored_sessions,
    )?;
    let reconciled_projects = reconciliation
        .projects
        .into_iter()
        .filter_map(|status| {
            let record = state
                .projects
                .iter()
                .find(|record| record.id == status.id)?;
            Some(grove_core::model::Project {
                id: status.id,
                name: status.name,
                repository_path: record.repository_path.clone(),
                git_common_dir: record.git_common_dir.clone(),
                default_worktree_path: record.default_worktree_path.clone(),
                is_expanded: record.is_expanded,
                worktrees: status.worktrees,
                unavailable: status.unavailable,
            })
        })
        .collect::<Vec<_>>();
    let signals = workflow::poll_session_signals(&api.server, workflow::now_epoch())?;
    let plans = workflow::agents_to_resume(
        &reconciled_projects,
        &state.agents,
        &signals,
        &loaded.config.status.policy(),
    );

    let mut result = ResumeRecordedResult {
        worktree_ids: Vec::new(),
        failures: Vec::new(),
    };
    for plan in plans {
        match workflow::start_agent(
            &api.server,
            &loaded.config,
            &api.paths.runtime_dir,
            &plan.project_name,
            &plan.git_common_dir,
            &plan.worktree,
            workflow::AgentStart::Resume(&plan.session_id),
        ) {
            Ok(_) => {
                result.worktree_ids.push(plan.worktree.id.clone());
                let session = plan.worktree.session_name();
                if let Err(error) =
                    record_control_session(api, &plan.worktree.project_id, &plan.worktree, &session)
                {
                    result.failures.push(ResumeFailure {
                        worktree_path: plan.worktree.path,
                        message: format!(
                            "agent resumed, but its session could not be indexed: {error}"
                        ),
                    });
                }
            }
            Err(error) => result.failures.push(ResumeFailure {
                worktree_path: plan.worktree.path,
                message: error.to_string(),
            }),
        }
    }
    Ok(result)
}

fn mutate_state(request: &Request, api: &ApiContext) -> Response {
    let MutateStateParams { mutation } = match serde_json::from_value(request.params.clone()) {
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
    let changed = state.apply(mutation);
    if changed && let Err(error) = state::save(&api.state_file, &state) {
        return Response::error(&request.id, "state_write_failed", error.to_string());
    }
    if changed {
        api.events
            .publish(EventKind::StateChanged, json!({"state": state.clone()}));
    }
    Response::success(&request.id, json!({"changed": changed, "state": state}))
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
    state_file: std::path::PathBuf,
    state_gate: Arc<Mutex<()>>,
    pending_notifications: HashMap<String, Notification>,
    pending_slot: Option<u8>,
    gui_launching_since: Option<Instant>,
    events: Arc<EventHub>,
}

impl Service {
    #[cfg(test)]
    fn new(paths: &Paths, events: Arc<EventHub>) -> Self {
        Self::with_state_gate(paths, events, Arc::new(Mutex::new(())))
    }

    fn with_state_gate(paths: &Paths, events: Arc<EventHub>, state_gate: Arc<Mutex<()>>) -> Self {
        Self {
            gui_socket: paths.gui_socket(),
            state_file: paths.state_file(),
            state_gate,
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
                if let Err(error) = self.record_agent(&notification) {
                    eprintln!("grove serve: could not record agent metadata: {error}");
                }
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

    fn record_agent(&self, notification: &Notification) -> Result<()> {
        if !notification.has_agent_record() {
            return Ok(());
        }
        let _guard = self
            .state_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut current = state::load(&self.state_file)?;
        let changed = current.apply(Mutation::RecordAgent {
            record: AgentRecord {
                worktree_id: notification.worktree_id.clone(),
                session_id: notification.agent_session.clone().unwrap_or_default(),
                transcript_path: notification.transcript.clone().unwrap_or_default(),
            },
        });
        if changed {
            state::save(&self.state_file, &current)?;
            self.events
                .publish(EventKind::StateChanged, json!({"state": current}));
        }
        Ok(())
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
    use std::net::Shutdown;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::sync::mpsc::TryRecvError;

    use grove_core::state::ProjectRecord;
    use grove_core::status::SessionStatus;

    fn paths(root: &Path) -> Paths {
        Paths {
            config_dir: root.join("config"),
            state_dir: root.join("state"),
            runtime_dir: root.join("run"),
        }
    }

    fn request(method: &str, params: serde_json::Value) -> Request {
        Request::new(format!("{method}-test"), method, params)
    }

    fn error_code(response: Response) -> String {
        assert!(!response.ok, "expected an error response: {response:?}");
        response.error.expect("error response").code
    }

    fn project(root: &Path) -> ProjectRecord {
        ProjectRecord {
            id: "project-1".into(),
            name: "Grove".into(),
            repository_path: root.join("repository"),
            git_common_dir: root.join("repository/.git"),
            default_worktree_path: root.join("worktrees"),
            is_expanded: true,
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
    fn agent_metadata_is_persisted_before_gui_delivery_and_only_once() {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = paths(temp.path());
        std::fs::create_dir_all(&paths.state_dir).expect("state directory");
        let events = Arc::new(EventHub::default());
        let (_, state_events, _) =
            events.subscribe(HashSet::from([EventKind::StateChanged]), false);
        let mut service = Service::new(&paths, events);
        let notification = Notification::new("abc123", SessionStatus::Working)
            .with_agent_session(Some("conversation-1".into()))
            .with_transcript(Some("/tmp/conversation-1.jsonl".into()));

        service.handle(Command::Notify(notification.clone()));
        let stored = state::load(&paths.state_file()).expect("state");
        let agent = stored.agent("abc123").expect("agent record");
        assert_eq!(agent.session_id, "conversation-1");
        assert_eq!(
            agent.transcript_path,
            std::path::Path::new("/tmp/conversation-1.jsonl")
        );
        let event = state_events.recv().expect("state event");
        assert_eq!(event.kind, EventKind::StateChanged);

        service.handle(Command::Notify(notification));
        assert!(
            matches!(state_events.try_recv(), Err(TryRecvError::Empty)),
            "an identical report must not rewrite state or publish another state event"
        );
    }

    #[test]
    fn legacy_notifications_and_api_calls_share_one_state_gate() {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = paths(temp.path());
        let events = Arc::new(EventHub::default());
        let state_gate = Arc::new(Mutex::new(()));
        let service =
            Service::with_state_gate(&paths, Arc::clone(&events), Arc::clone(&state_gate));
        let api = ApiContext::with_state_gate(&paths, events, state_gate);
        assert!(Arc::ptr_eq(&service.state_gate, &api.state_gate));
    }

    #[test]
    fn connection_discriminator_routes_framed_api_and_legacy_notifications() {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = paths(temp.path());
        std::fs::create_dir_all(&paths.state_dir).expect("state directory");
        let events = Arc::new(EventHub::default());
        let state_gate = Arc::new(Mutex::new(()));
        let service = Arc::new(Mutex::new(Service::with_state_gate(
            &paths,
            Arc::clone(&events),
            Arc::clone(&state_gate),
        )));
        let api = Arc::new(ApiContext::with_state_gate(&paths, events, state_gate));

        let (mut client, server) = UnixStream::pair().expect("api pair");
        let api_service = Arc::clone(&service);
        let api_context = Arc::clone(&api);
        let api_thread =
            std::thread::spawn(move || handle_connection(server, &api_service, &api_context));
        protocol::write_json(
            &mut client,
            &Request::new(
                "ping-through-discriminator",
                "ping",
                serde_json::Value::Null,
            ),
        )
        .expect("ping request");
        let response: Response = protocol::read_json(&mut client).expect("ping response");
        assert!(response.ok);
        assert_eq!(response.id, "ping-through-discriminator");
        assert_eq!(
            response.result.as_ref().expect("ping result")["protocol"],
            protocol::VERSION
        );
        api_thread.join().expect("api handler");

        let notification = Notification::new("abc123", SessionStatus::Working)
            .with_agent_session(Some("conversation-1".into()));
        let (mut client, server) = UnixStream::pair().expect("legacy pair");
        let legacy_service = Arc::clone(&service);
        let legacy_context = Arc::clone(&api);
        let legacy_thread =
            std::thread::spawn(move || handle_connection(server, &legacy_service, &legacy_context));
        use std::io::Write as _;
        client
            .write_all(format!("{}\n", Command::Notify(notification).encode()).as_bytes())
            .expect("legacy notification");
        client
            .shutdown(Shutdown::Write)
            .expect("finish legacy notification");
        legacy_thread.join().expect("legacy handler");

        let stored = state::load(&paths.state_file()).expect("state");
        assert_eq!(
            stored
                .agent("abc123")
                .map(|agent| agent.session_id.as_str()),
            Some("conversation-1")
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
    fn read_only_api_methods_treat_absent_state_and_tmux_as_empty() {
        let temp = tempfile::tempdir().expect("tempdir");
        let api = ApiContext::new(&paths(temp.path()), Arc::new(EventHub::default()));

        let state = dispatch_api(&request("state.get", json!(null)), &api);
        assert!(state.ok);
        assert_eq!(
            state.result.expect("state")["version"],
            grove_core::state::STATE_VERSION
        );

        for method in [
            "project.list",
            "worktree.list",
            "session.list",
            "session.snapshot",
            "status.poll",
        ] {
            let response = dispatch_api(&request(method, json!({})), &api);
            assert!(response.ok, "{method}: {:?}", response.error);
        }
    }

    #[test]
    fn state_mutations_are_atomic_idempotent_and_publish_only_real_changes() {
        let temp = tempfile::tempdir().expect("tempdir");
        let events = Arc::new(EventHub::default());
        let api = ApiContext::new(&paths(temp.path()), Arc::clone(&events));
        let (_subscription, receiver, baseline) =
            events.subscribe(HashSet::from([EventKind::StateChanged]), false);
        assert_eq!(baseline, 0);

        let mutation = Mutation::UpsertProject {
            record: project(temp.path()),
        };
        let params = json!({"mutation": mutation});
        let first = dispatch_api(&request("state.mutate", params.clone()), &api);
        assert!(first.ok);
        let first_result = first.result.expect("mutation result");
        assert_eq!(first_result["changed"], true);
        assert_eq!(first_result["state"]["project"][0]["id"], "project-1");

        let event = receiver.recv().expect("state change event");
        assert_eq!(event.kind, EventKind::StateChanged);
        assert_eq!(event.revision, 1);
        assert_eq!(event.payload["state"]["project"][0]["id"], "project-1");

        let saved = state::load(&api.state_file).expect("saved state");
        assert_eq!(saved.projects, vec![project(temp.path())]);

        let duplicate = dispatch_api(&request("state.mutate", params), &api);
        assert!(duplicate.ok);
        assert_eq!(duplicate.result.expect("mutation result")["changed"], false);
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn dedicated_index_intents_validate_and_return_authoritative_state() {
        let temp = tempfile::tempdir().expect("tempdir");
        let api = ApiContext::new(&paths(temp.path()), Arc::new(EventHub::default()));
        let mut initial = State::default();
        initial.projects.push(project(temp.path()));
        state::save(&api.state_file, &initial).expect("initial state");

        for (method, params) in [
            ("project.expanded.set", json!({"project_id": ""})),
            (
                "project.expanded.set",
                json!({"project_id": "project-1", "expanded": false, "extra": true}),
            ),
            ("project.remove", json!({"project_id": "line\nbreak"})),
            ("slot.assign", json!({"number": 0, "worktree_id": "abc123"})),
            ("slot.assign", json!({"number": 1, "worktree_id": ""})),
            ("slot.clear", json!({"worktree_id": ""})),
            ("session.ignore", json!({"session": ""})),
        ] {
            assert_eq!(
                error_code(dispatch_api(&request(method, params), &api)),
                "invalid_params"
            );
        }

        for (method, params) in [
            (
                "project.expanded.set",
                json!({"project_id": "project-1", "expanded": false}),
            ),
            ("slot.assign", json!({"number": 2, "worktree_id": "abc123"})),
            ("session.ignore", json!({"session": "scratch"})),
        ] {
            let response = dispatch_api(&request(method, params), &api);
            assert!(response.ok, "{method}: {:?}", response.error);
            assert_eq!(
                response.result.as_ref().expect("intent result")["changed"],
                true
            );
        }
        let cleared = dispatch_api(
            &request("slot.clear", json!({"worktree_id": "abc123"})),
            &api,
        );
        assert!(cleared.ok);
        let restored = dispatch_api(
            &request("session.ignored.clear", serde_json::Value::Null),
            &api,
        );
        assert!(restored.ok);
        let removed = dispatch_api(
            &request("project.remove", json!({"project_id": "project-1"})),
            &api,
        );
        assert!(removed.ok);
        let final_state = state::load(&api.state_file).expect("final state");
        assert!(final_state.projects.is_empty());
        assert!(final_state.slots.is_empty());
        assert!(final_state.ignored_sessions.is_empty());
    }

    #[test]
    fn malformed_state_is_reported_and_never_replaced() {
        let temp = tempfile::tempdir().expect("tempdir");
        let api = ApiContext::new(&paths(temp.path()), Arc::new(EventHub::default()));
        std::fs::create_dir_all(api.state_file.parent().expect("state parent")).expect("mkdir");
        std::fs::write(&api.state_file, "this is not = [toml").expect("write corrupt state");

        let get = dispatch_api(&request("state.get", json!({})), &api);
        assert_eq!(error_code(get), "internal_error");

        let mutation = Mutation::UpsertProject {
            record: project(temp.path()),
        };
        let mutate = dispatch_api(
            &request("state.mutate", json!({"mutation": mutation})),
            &api,
        );
        assert_eq!(error_code(mutate), "state_read_failed");
        assert_eq!(
            std::fs::read_to_string(&api.state_file).expect("corrupt state survives"),
            "this is not = [toml"
        );
    }

    #[test]
    fn a_failed_atomic_state_write_is_returned_without_a_false_event() {
        let temp = tempfile::tempdir().expect("tempdir");
        let events = Arc::new(EventHub::default());
        let api = ApiContext::new(&paths(temp.path()), Arc::clone(&events));
        state::save(&api.state_file, &State::default()).expect("initial state");
        let state_dir = api.state_file.parent().expect("state directory");
        std::fs::set_permissions(state_dir, std::fs::Permissions::from_mode(0o500))
            .expect("make state directory read-only");
        let (_subscription, receiver, _) =
            events.subscribe(HashSet::from([EventKind::StateChanged]), false);

        let mutation = Mutation::UpsertProject {
            record: project(temp.path()),
        };
        let response = dispatch_api(
            &request("state.mutate", json!({"mutation": mutation})),
            &api,
        );
        std::fs::set_permissions(state_dir, std::fs::Permissions::from_mode(0o700))
            .expect("restore state directory");
        assert_eq!(error_code(response), "state_write_failed");
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
        assert!(
            state::load(&api.state_file)
                .expect("state survives")
                .projects
                .is_empty()
        );
    }

    #[test]
    fn malformed_mutations_and_unsubscriptions_are_rejected() {
        let temp = tempfile::tempdir().expect("tempdir");
        let events = Arc::new(EventHub::default());
        let api = ApiContext::new(&paths(temp.path()), Arc::clone(&events));

        let missing_mutation = dispatch_api(&request("state.mutate", json!({})), &api);
        assert_eq!(error_code(missing_mutation), "invalid_params");
        let extra_mutation = dispatch_api(
            &request(
                "state.mutate",
                json!({
                    "mutation": {"kind": "clear_ignored_sessions"},
                    "unexpected": true,
                }),
            ),
            &api,
        );
        assert_eq!(error_code(extra_mutation), "invalid_params");

        let malformed_unsubscribe =
            dispatch_api(&request("event.unsubscribe", json!({"wrong": "id"})), &api);
        assert_eq!(error_code(malformed_unsubscribe), "invalid_params");

        let (id, _receiver, _) = events.subscribe(HashSet::from([EventKind::StateChanged]), false);
        let removed = dispatch_api(
            &request("event.unsubscribe", json!({"subscription_id": id})),
            &api,
        );
        assert_eq!(
            removed.result.expect("unsubscribe result")["unsubscribed"],
            true
        );
        let absent = dispatch_api(
            &request(
                "event.unsubscribe",
                json!({"subscription_id": "already-absent"}),
            ),
            &api,
        );
        assert_eq!(
            absent.result.expect("unsubscribe result")["unsubscribed"],
            false
        );
    }

    #[test]
    fn control_requests_reject_ambiguous_identifiers_before_side_effects() {
        let temp = tempfile::tempdir().expect("tempdir");
        let api = ApiContext::new(&paths(temp.path()), Arc::new(EventHub::default()));

        for params in [
            json!({}),
            json!({"worktree_id": ""}),
            json!({"worktree_id": "line\nbreak"}),
            json!({"worktree_id": "abc123", "idempotency_key": ""}),
            json!({"worktree_id": "abc123", "idempotency_key": "line\nbreak"}),
            json!({
                "worktree_id": "abc123",
                "idempotency_key": "x".repeat(protocol::MAX_REQUEST_ID_LEN + 1),
            }),
            json!({"worktree_id": "abc123", "resume": "conversation"}),
            json!({"worktree_id": "abc123", "window_index": 7}),
            json!({"worktree_id": "abc123", "orphan_session": "scratch"}),
            json!({"worktree_id": "abc123", "unexpected": true}),
        ] {
            let response = dispatch_api(&request("session.ensure", params), &api);
            assert_eq!(error_code(response), "invalid_params");
        }

        for resume in [
            String::new(),
            "line\nbreak".to_string(),
            "x".repeat(protocol::MAX_REQUEST_ID_LEN + 1),
        ] {
            let response = dispatch_api(
                &request(
                    "agent.start",
                    json!({"worktree_id": "abc123", "resume": resume}),
                ),
                &api,
            );
            assert_eq!(error_code(response), "invalid_params");
        }

        let missing_window = dispatch_api(
            &request("session.window.open", json!({"worktree_id": "abc123"})),
            &api,
        );
        assert_eq!(error_code(missing_window), "invalid_params");

        for params in [
            json!({"worktree_id": "abc123"}),
            json!({"worktree_id": "abc123", "orphan_session": ""}),
            json!({"worktree_id": "abc123", "orphan_session": "line\nbreak"}),
            json!({
                "worktree_id": "abc123",
                "orphan_session": "x".repeat(protocol::MAX_REQUEST_ID_LEN + 1),
            }),
        ] {
            let response = dispatch_api(&request("session.associate", params), &api);
            assert_eq!(error_code(response), "invalid_params");
        }

        let unknown = dispatch_api(
            &request("session.ensure", json!({"worktree_id": "abc123"})),
            &api,
        );
        assert_eq!(error_code(unknown), "control_failed");
    }

    #[test]
    fn closing_sessions_rejects_ambiguous_identifiers_before_side_effects() {
        let temp = tempfile::tempdir().expect("tempdir");
        let api = ApiContext::new(&paths(temp.path()), Arc::new(EventHub::default()));

        for params in [
            json!({}),
            json!({"session": "", "idempotency_key": "close-1"}),
            json!({"session": "line\nbreak", "idempotency_key": "close-1"}),
            json!({
                "session": "x".repeat(protocol::MAX_REQUEST_ID_LEN + 1),
                "idempotency_key": "close-1",
            }),
            json!({"session": "scratch", "idempotency_key": ""}),
            json!({"session": "scratch", "idempotency_key": "line\nbreak"}),
            json!({
                "session": "scratch",
                "idempotency_key": "x".repeat(protocol::MAX_REQUEST_ID_LEN + 1),
            }),
            json!({
                "session": "scratch",
                "idempotency_key": "close-1",
                "unexpected": true,
            }),
        ] {
            for method in ["session.close", "session.orphan.open"] {
                let response = dispatch_api(&request(method, params.clone()), &api);
                assert_eq!(error_code(response), "invalid_params");
            }
        }
    }

    #[test]
    fn opening_an_orphan_resolves_live_tmux_metadata_in_the_service() {
        if std::process::Command::new(grove_core::tmux::server::TMUX)
            .arg("-V")
            .output()
            .is_err()
        {
            eprintln!("skipping: tmux is not installed");
            return;
        }
        let temp = tempfile::tempdir().expect("tempdir");
        let api = ApiContext::new(&paths(temp.path()), Arc::new(EventHub::default()));
        std::fs::create_dir_all(&api.paths.config_dir).expect("config directory");
        std::fs::create_dir_all(&api.paths.runtime_dir).expect("runtime directory");
        std::fs::write(api.paths.config_file(), "[terminal]\ncommand = \"true\"\n")
            .expect("terminal config");
        api.server.ensure_config_file().expect("tmux config");
        api.server
            .run([
                "new-session",
                "-d",
                "-s",
                "scratch",
                "-c",
                temp.path().to_str().expect("utf-8 temp path"),
            ])
            .expect("orphan session");

        let response = dispatch_api(
            &request(
                "session.orphan.open",
                json!({"session": "scratch", "idempotency_key": "open-1"}),
            ),
            &api,
        );
        assert!(response.ok, "{:?}", response.error);
        let result = response.result.expect("open result");
        assert_eq!(result["session"], "scratch");
        assert_eq!(result["activation"]["kind"], "launched_terminal");
        assert_eq!(result["activation"]["session"], "scratch");
        api.server.kill_server().expect("stop test server");
    }

    #[test]
    fn stopping_an_absent_private_server_is_idempotent_and_validated() {
        let temp = tempfile::tempdir().expect("tempdir");
        let api = ApiContext::new(&paths(temp.path()), Arc::new(EventHub::default()));
        std::fs::create_dir_all(&api.paths.runtime_dir).expect("runtime directory");

        for params in [
            json!({}),
            json!({"idempotency_key": ""}),
            json!({"idempotency_key": "line\nbreak"}),
            json!({"idempotency_key": "x".repeat(protocol::MAX_REQUEST_ID_LEN + 1)}),
            json!({"idempotency_key": "stop-1", "unexpected": true}),
        ] {
            let response = dispatch_api(&request("server.stop", params), &api);
            assert_eq!(error_code(response), "invalid_params");
        }

        let first = dispatch_api(
            &request("server.stop", json!({"idempotency_key": "stop-1"})),
            &api,
        );
        assert!(first.ok, "{:?}", first.error);
        assert_eq!(
            first.result.as_ref().expect("first result")["stopped"],
            true
        );
        let replay = dispatch_api(
            &Request::new(
                "server-stop-retry",
                "server.stop",
                json!({"idempotency_key": "stop-1"}),
            ),
            &api,
        );
        assert!(replay.ok, "{:?}", replay.error);
        assert_eq!(replay.result, first.result);
        assert_eq!(replay.id, "server-stop-retry");
    }

    #[test]
    fn worktree_creation_validates_intent_and_replays_without_duplicate_git_work() {
        let temp = tempfile::tempdir().expect("tempdir");
        let api = ApiContext::new(&paths(temp.path()), Arc::new(EventHub::default()));
        std::fs::create_dir_all(&api.paths.state_dir).expect("state directory");
        std::fs::create_dir_all(&api.paths.config_dir).expect("config directory");
        std::fs::create_dir_all(&api.paths.runtime_dir).expect("runtime directory");

        for params in [
            json!({}),
            json!({"path": "", "idempotency_key": "open-1"}),
            json!({"path": "/tmp/project", "idempotency_key": ""}),
            json!({"path": "/tmp/project", "idempotency_key": "line\nbreak"}),
            json!({
                "path": "/tmp/project",
                "idempotency_key": "x".repeat(protocol::MAX_REQUEST_ID_LEN + 1),
            }),
            json!({
                "path": "/tmp/project",
                "idempotency_key": "open-1",
                "repository_path": "/untrusted",
            }),
        ] {
            let response = dispatch_api(&request("project.open", params), &api);
            assert_eq!(error_code(response), "invalid_params");
        }
        for params in [
            json!({}),
            json!({"project_id": ""}),
            json!({"project_id": "line\nbreak"}),
            json!({"project_id": "project-1", "repository_path": "/untrusted"}),
        ] {
            for method in ["project.refresh", "project.statuses", "project.refs"] {
                let response = dispatch_api(&request(method, params.clone()), &api);
                assert_eq!(error_code(response), "invalid_params");
            }
        }
        for params in [
            json!({}),
            json!({"worktree_id": ""}),
            json!({"worktree_id": "line\nbreak"}),
            json!({"worktree_id": "abc123", "path": "/untrusted"}),
        ] {
            let response = dispatch_api(&request("removal.inspect", params), &api);
            assert_eq!(error_code(response), "invalid_params");
        }

        for params in [
            json!({}),
            json!({"project_id": "", "add": {"path": "/tmp/tree"}, "idempotency_key": "add-1"}),
            json!({"project_id": "project-1", "add": {"path": ""}, "idempotency_key": "add-1"}),
            json!({
                "project_id": "project-1",
                "add": {"path": "/tmp/tree", "new_branch": ""},
                "idempotency_key": "add-1",
            }),
            json!({
                "project_id": "project-1",
                "add": {"path": "/tmp/tree", "base_ref": "line\nbreak"},
                "idempotency_key": "add-1",
            }),
            json!({"project_id": "project-1", "add": {"path": "/tmp/tree"}, "idempotency_key": ""}),
        ] {
            let response = dispatch_api(&request("worktree.create", params), &api);
            assert_eq!(error_code(response), "invalid_params");
        }

        let repository = temp.path().join("repository");
        std::fs::create_dir_all(&repository).expect("repository directory");
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "grove@example.invalid"],
            vec!["config", "user.name", "Grove Test"],
            vec!["commit", "--allow-empty", "-qm", "initial"],
        ] {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(&repository)
                .output()
                .expect("run git");
            assert!(output.status.success(), "git failed: {output:?}");
        }
        let open_params = json!({
            "path": repository,
            "idempotency_key": "open-1",
        });
        let opened = dispatch_api(&request("project.open", open_params.clone()), &api);
        assert!(opened.ok, "{:?}", opened.error);
        assert_eq!(
            opened.result.as_ref().expect("open result")["changed"],
            true
        );
        let opened_project_id = opened.result.as_ref().expect("open result")["project"]["id"]
            .as_str()
            .expect("project id")
            .to_string();
        let open_replay = dispatch_api(
            &Request::new("open-retry", "project.open", open_params),
            &api,
        );
        assert!(open_replay.ok, "{:?}", open_replay.error);
        assert_eq!(open_replay.result, opened.result);
        assert_eq!(
            state::load(&api.state_file).expect("state").projects.len(),
            1
        );
        let refreshed = dispatch_api(
            &request("project.refresh", json!({"project_id": opened_project_id})),
            &api,
        );
        assert!(refreshed.ok, "{:?}", refreshed.error);
        assert_eq!(
            refreshed.result.as_ref().expect("refresh result")["project_id"],
            opened_project_id
        );
        assert_eq!(
            refreshed.result.as_ref().expect("refresh result")["worktrees"]
                .as_array()
                .map(Vec::len),
            Some(1)
        );
        let statuses = dispatch_api(
            &request("project.statuses", json!({"project_id": opened_project_id})),
            &api,
        );
        assert!(statuses.ok, "{:?}", statuses.error);
        assert_eq!(
            statuses.result.as_ref().expect("statuses result")["project_id"],
            opened_project_id
        );
        let refs = dispatch_api(
            &request("project.refs", json!({"project_id": opened_project_id})),
            &api,
        );
        assert!(refs.ok, "{:?}", refs.error);
        assert_eq!(
            refs.result.as_ref().expect("refs result")["project_id"],
            opened_project_id
        );
        let main_worktree_id =
            refreshed.result.as_ref().expect("refresh result")["worktrees"][0]["id"]
                .as_str()
                .expect("main worktree id");
        let inspection = dispatch_api(
            &request("removal.inspect", json!({"worktree_id": main_worktree_id})),
            &api,
        );
        assert!(inspection.ok, "{:?}", inspection.error);
        assert_eq!(
            inspection.result.as_ref().expect("inspection result")["worktree_id"],
            main_worktree_id
        );

        let path = temp.path().join("created-tree");
        let params = json!({
            "project_id": opened_project_id,
            "add": {"path": path, "new_branch": "feature"},
            "idempotency_key": "add-1",
        });
        let first = dispatch_api(&request("worktree.create", params.clone()), &api);
        assert!(first.ok, "{:?}", first.error);
        assert!(path.is_dir());
        let replay = dispatch_api(
            &Request::new("create-retry", "worktree.create", params),
            &api,
        );
        assert!(replay.ok, "{:?}", replay.error);
        assert_eq!(replay.result, first.result);
        assert_eq!(replay.id, "create-retry");

        for params in [
            json!({}),
            json!({"worktree_id": "", "force": false, "idempotency_key": "remove-1"}),
            json!({"worktree_id": "line\nbreak", "force": false, "idempotency_key": "remove-1"}),
            json!({"worktree_id": "abc123", "force": false, "idempotency_key": ""}),
        ] {
            let response = dispatch_api(&request("worktree.remove", params), &api);
            assert_eq!(error_code(response), "invalid_params");
        }
        let created_id = first.result.as_ref().expect("create result")["worktrees"]
            .as_array()
            .expect("worktrees")
            .iter()
            .find(|worktree| worktree["path"].as_str() == path.to_str())
            .and_then(|worktree| worktree["id"].as_str())
            .expect("created worktree id")
            .to_string();
        let remove_params = json!({
            "worktree_id": created_id,
            "force": false,
            "idempotency_key": "remove-1",
        });
        let removed = dispatch_api(&request("worktree.remove", remove_params.clone()), &api);
        assert!(removed.ok, "{:?}", removed.error);
        assert!(!path.exists());
        let remove_replay = dispatch_api(
            &Request::new("remove-retry", "worktree.remove", remove_params),
            &api,
        );
        assert!(remove_replay.ok, "{:?}", remove_replay.error);
        assert_eq!(remove_replay.result, removed.result);

        for params in [
            json!({}),
            json!({"project_id": "", "branch": "feature", "force": false, "idempotency_key": "delete-1"}),
            json!({"project_id": "project-1", "branch": "", "force": false, "idempotency_key": "delete-1"}),
            json!({"project_id": "project-1", "branch": "line\nbreak", "force": false, "idempotency_key": "delete-1"}),
            json!({"project_id": "project-1", "branch": "feature", "force": false, "idempotency_key": ""}),
        ] {
            let response = dispatch_api(&request("branch.delete", params), &api);
            assert_eq!(error_code(response), "invalid_params");
        }
        let delete_params = json!({
            "project_id": opened_project_id,
            "branch": "feature",
            "force": false,
            "idempotency_key": "delete-1",
        });
        let deleted = dispatch_api(&request("branch.delete", delete_params.clone()), &api);
        assert!(deleted.ok, "{:?}", deleted.error);
        let delete_replay = dispatch_api(
            &Request::new("delete-retry", "branch.delete", delete_params),
            &api,
        );
        assert!(delete_replay.ok, "{:?}", delete_replay.error);
        assert_eq!(delete_replay.result, deleted.result);
    }

    #[test]
    fn recorded_agent_resumption_validates_and_deduplicates_a_gui_launch() {
        let temp = tempfile::tempdir().expect("tempdir");
        let api = ApiContext::new(&paths(temp.path()), Arc::new(EventHub::default()));
        std::fs::create_dir_all(&api.paths.config_dir).expect("config directory");
        std::fs::write(
            api.paths.config_file(),
            "[agents]\nresume_on_startup = true\n",
        )
        .expect("config");

        for params in [
            json!({}),
            json!({"idempotency_key": ""}),
            json!({"idempotency_key": "line\nbreak"}),
            json!({"idempotency_key": "x".repeat(protocol::MAX_REQUEST_ID_LEN + 1)}),
            json!({"idempotency_key": "launch-1", "unexpected": true}),
        ] {
            let response = dispatch_api(&request("agent.resume_recorded", params), &api);
            assert_eq!(error_code(response), "invalid_params");
        }

        let first = dispatch_api(
            &request(
                "agent.resume_recorded",
                json!({"idempotency_key": "launch-1"}),
            ),
            &api,
        );
        assert!(first.ok, "{:?}", first.error);
        let result = first.result.expect("resume result");
        assert_eq!(result["worktree_ids"], json!([]));
        assert_eq!(result["failures"], json!([]));

        std::fs::create_dir_all(api.state_file.parent().expect("state parent")).expect("mkdir");
        std::fs::write(&api.state_file, "corrupt = [").expect("corrupt state");
        let mut replay_request = request(
            "agent.resume_recorded",
            json!({"idempotency_key": "launch-1"}),
        );
        replay_request.id = "resume-retry".into();
        let replay = dispatch_api(&replay_request, &api);
        assert!(replay.ok, "{:?}", replay.error);
        assert_eq!(replay.id, "resume-retry");

        let new_launch = dispatch_api(
            &request(
                "agent.resume_recorded",
                json!({"idempotency_key": "launch-2"}),
            ),
            &api,
        );
        assert_eq!(error_code(new_launch), "resume_failed");
    }

    #[test]
    fn subscription_handshake_rejects_every_ambiguous_shape() {
        fn response_for(request: &Request, api: &ApiContext) -> Response {
            let (server, mut client) = UnixStream::pair().expect("stream pair");
            serve_subscription(server, request, api);
            protocol::read_json(&mut client).expect("subscription response")
        }

        let temp = tempfile::tempdir().expect("tempdir");
        let api = ApiContext::new(&paths(temp.path()), Arc::new(EventHub::default()));

        let mut future = request("event.subscribe", json!({"topics": ["state_changed"]}));
        future.protocol += 1;
        assert_eq!(
            error_code(response_for(&future, &api)),
            "unsupported_protocol"
        );

        for params in [
            json!({}),
            json!({"topics": []}),
            json!({"topics": ["state_changed"], "client": "remote"}),
            json!({"topics": ["unknown"]}),
            json!({"topics": ["state_changed"], "unexpected": true}),
        ] {
            let subscribe = request("event.subscribe", params);
            assert_eq!(error_code(response_for(&subscribe, &api)), "invalid_params");
        }

        let non_streaming = dispatch_api(
            &request("event.subscribe", json!({"topics": ["state_changed"]})),
            &api,
        );
        assert_eq!(error_code(non_streaming), "invalid_subscription");
    }

    #[test]
    fn a_subscription_acknowledges_its_baseline_before_ordered_events() {
        let temp = tempfile::tempdir().expect("tempdir");
        let events = Arc::new(EventHub::default());
        let api = Arc::new(ApiContext::new(&paths(temp.path()), Arc::clone(&events)));
        let request = request(
            "event.subscribe",
            json!({
                "topics": ["state_changed"],
                "client": "gui",
            }),
        );
        let (server, mut client) = UnixStream::pair().expect("stream pair");
        let serving_api = Arc::clone(&api);
        let server_thread = std::thread::spawn(move || {
            serve_subscription(server, &request, &serving_api);
        });

        let acknowledgement: Response =
            protocol::read_json(&mut client).expect("subscription acknowledgement");
        assert!(acknowledgement.ok);
        let result = acknowledgement.result.expect("subscription metadata");
        assert_eq!(result["revision"], 0);
        let subscription_id = result["subscription_id"]
            .as_str()
            .expect("subscription id")
            .to_string();

        let ignored = events.publish(EventKind::NotificationReceived, json!({"ignored": true}));
        assert!(!ignored.delivered);
        let delivered = events.publish(EventKind::StateChanged, json!({"sequence": 1}));
        assert!(delivered.delivered);
        assert!(delivered.delivered_to_gui);
        let event = protocol::read_event(&mut client).expect("ordered event");
        assert_eq!(event.revision, 2);
        assert_eq!(event.kind, EventKind::StateChanged);
        assert_eq!(event.payload["sequence"], 1);

        client
            .shutdown(Shutdown::Both)
            .expect("close subscription connection");
        events.publish(EventKind::StateChanged, json!({"sequence": 2}));
        server_thread.join().expect("subscription server");
        assert!(
            !events.unsubscribe(&subscription_id),
            "disconnect removes the subscription"
        );
    }

    #[test]
    fn reconciliation_publishes_one_coherent_completion_snapshot() {
        let temp = tempfile::tempdir().expect("tempdir");
        let events = Arc::new(EventHub::default());
        let api = ApiContext::new(&paths(temp.path()), Arc::clone(&events));
        let (_subscription, receiver, _) = events.subscribe(
            HashSet::from([EventKind::StateChanged, EventKind::ReconciliationCompleted]),
            false,
        );

        let response = dispatch_api(&request("state.reconcile", json!({"projects": []})), &api);
        assert!(response.ok, "{:?}", response.error);
        let result = response.result.expect("reconciliation result");
        assert_eq!(result["reconciliation"]["projects"], json!([]));
        assert_eq!(result["reconciliation"]["orphans"], json!([]));
        assert_eq!(result["state"]["version"], grove_core::state::STATE_VERSION);

        let event = receiver.recv().expect("reconciliation event");
        assert_eq!(event.kind, EventKind::ReconciliationCompleted);
        assert_eq!(event.revision, 1);
        assert_eq!(event.payload["reconciliation"], result["reconciliation"]);
        assert_eq!(event.payload["state"], result["state"]);
        assert_eq!(
            receiver.try_recv(),
            Err(TryRecvError::Empty),
            "unchanged state emits no state-changed event"
        );
    }

    #[test]
    fn reconciliation_refuses_malformed_requests_and_corrupt_state() {
        let temp = tempfile::tempdir().expect("tempdir");
        let api = ApiContext::new(&paths(temp.path()), Arc::new(EventHub::default()));

        for params in [
            json!({}),
            json!({"projects": "not-a-list"}),
            json!({"projects": [], "unexpected": true}),
        ] {
            let response = dispatch_api(&request("state.reconcile", params), &api);
            assert_eq!(error_code(response), "invalid_params");
        }

        std::fs::create_dir_all(api.state_file.parent().expect("state parent")).expect("mkdir");
        std::fs::write(&api.state_file, "[[project]\nid = [").expect("write corrupt state");
        let response = dispatch_api(&request("state.reconcile", json!({"projects": []})), &api);
        assert_eq!(error_code(response), "state_read_failed");
        assert_eq!(
            std::fs::read_to_string(&api.state_file).expect("corrupt state survives"),
            "[[project]\nid = ["
        );
    }

    #[test]
    fn status_queries_never_turn_bad_ids_into_stopped_sessions() {
        let temp = tempfile::tempdir().expect("tempdir");
        let api = ApiContext::new(&paths(temp.path()), Arc::new(EventHub::default()));

        for params in [
            json!({}),
            json!({"worktree_id": "abc123", "unexpected": true}),
        ] {
            let response = dispatch_api(&request("status.get", params), &api);
            assert_eq!(error_code(response), "invalid_params");
        }

        let unknown = dispatch_api(
            &request("status.get", json!({"worktree_id": "abc123"})),
            &api,
        );
        assert!(!unknown.ok);
        let error = unknown.error.expect("status error");
        assert_eq!(error.code, "internal_error");
        assert!(
            error.message.contains("unknown worktree `abc123`"),
            "{}",
            error.message
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
