//! Talking to the daemon.
//!
//! Every worker path that needs the service goes through here, so the retry
//! window while a just-spawned `grove serve` is still binding its socket, and
//! the way a protocol error becomes a reportable one, are decided once rather
//! than at each call site.

use grove_core::Error;
use grove_core::protocol::{self, Request};
use grove_core::reconcile::{ProjectRef, Reconciliation};
use grove_core::state::State;

use super::{ErrorReport, Message, RemovalOp, WorkerState};

pub(super) const SERVICE_OPERATION_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(30);

fn call_service(
    worker: &WorkerState,
    id: &str,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, Error> {
    let request = Request::new(id, method, params);
    let mut attempts = 0;
    let response = loop {
        match protocol::call_with_timeout(
            &worker.paths.notify_socket(),
            &request,
            SERVICE_OPERATION_TIMEOUT,
        ) {
            Ok(response) => break response,
            Err(error) if service_is_starting(&error) && attempts < 20 => {
                attempts += 1;
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(error) => return Err(service_error(method, error)),
        }
    };
    match (response.result, response.error) {
        (Some(result), None) if response.ok => Ok(result),
        (None, Some(error)) if !response.ok => Err(Error::io(
            format!("service method {method}"),
            std::io::Error::other(format!("{}: {}", error.code, error.message)),
        )),
        _ => Err(Error::io(
            format!("service method {method}"),
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "service returned an invalid response shape",
            ),
        )),
    }
}

/// The reply of a call whose only interesting outcome is that it succeeded.
///
/// `session.attention.clear`, `session.close` and `server.stop` answer with a
/// body no caller reads. Naming that lets them go through [`service_result`]
/// like everything else, rather than being a second spelling of the same call
/// and the same error path that happens to ignore its `Ok`.
pub(super) type NoReply = serde::de::IgnoredAny;

/// Call the service and decode its reply.
///
/// The decode names the *method* rather than the task, which is the half a
/// reader of the log does not already have.
fn service_decoded<T>(
    worker: &WorkerState,
    id: &str,
    method: &str,
    params: serde_json::Value,
) -> Result<T, Error>
where
    T: serde::de::DeserializeOwned,
{
    call_service(worker, id, method, params).and_then(|value| {
        serde_json::from_value::<T>(value).map_err(|error| {
            Error::io(
                format!("decode {method} response"),
                std::io::Error::new(std::io::ErrorKind::InvalidData, error),
            )
        })
    })
}

/// Call the service, decode its reply, and turn either outcome into messages.
///
/// Twenty-five of the worker's tasks are the same operation with different
/// nouns: send one request, decode one result shape, and report the failure if
/// there is one. Written out per task that came to about nine hundred lines of
/// which only the method name, the parameters and the mapping differed — the
/// call, the decode and the error path were character-identical each time.
///
/// This is deliberately a function taking data rather than a trait with an
/// implementation per task. The tasks do not differ in *behaviour*, only in
/// four values, and polymorphism over things that behave identically would
/// reproduce the same boilerplate inside every impl.
///
/// [`state_intent_messages`] is the same idea for the mutation subset, which
/// already had it; this is that pattern for the tasks that read.
pub(super) fn service_result<T, F>(
    worker: &WorkerState,
    id: &str,
    method: &str,
    params: serde_json::Value,
    failure: &str,
    ok: F,
) -> Vec<Message>
where
    T: serde::de::DeserializeOwned,
    F: FnOnce(T) -> Vec<Message>,
{
    match service_decoded(worker, id, method, params) {
        Ok(value) => ok(value),
        Err(error) => vec![Message::Failed(ErrorReport::new(failure, &error))],
    }
}

/// Which of the four separate destructive steps a removal failure belongs to.
pub(super) struct Removal {
    pub(super) project_id: String,
    pub(super) operation: RemovalOp,
}

/// [`service_result`] for the three removal steps.
///
/// They differ from every other task in their *failure* alone: not a
/// `Message::Failed` banner but a `RemovalFailed` naming the project and the
/// operation, because the removal dialog reports which of the four separate
/// steps refused and only then offers the next one — `--force` after git's own
/// refusal. Folding them into [`service_result`] would have meant losing that,
/// and leaving them out would have meant three more copies of the call and the
/// decode.
///
/// The project id reaches the success closure rather than being cloned into it
/// at each call site: exactly one of the two arms consumes it.
pub(super) fn removal_result<T, F>(
    worker: &WorkerState,
    id: &str,
    method: &str,
    params: serde_json::Value,
    removal: Removal,
    failure: &str,
    ok: F,
) -> Vec<Message>
where
    T: serde::de::DeserializeOwned,
    F: FnOnce(T, String) -> Vec<Message>,
{
    let Removal {
        project_id,
        operation,
    } = removal;
    match service_decoded(worker, id, method, params) {
        Ok(value) => ok(value, project_id),
        Err(error) => vec![Message::RemovalFailed {
            project_id,
            operation,
            report: ErrorReport::new(failure, &error),
        }],
    }
}

pub(super) fn service_is_starting(error: &protocol::Error) -> bool {
    matches!(
        error,
        protocol::Error::Io { context, source }
            if *context == "connect to Grove service"
                && matches!(
                    source.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                )
    )
}

pub(super) fn service_error(method: &str, error: protocol::Error) -> Error {
    Error::io(
        format!("service method {method}"),
        std::io::Error::other(error.to_string()),
    )
}

pub(super) fn load_state_through_service(worker: &WorkerState) -> Result<State, Error> {
    let value = call_service(
        worker,
        "gui-state-get",
        "state.get",
        serde_json::Value::Null,
    )?;
    serde_json::from_value(value).map_err(|error| {
        Error::io(
            "decode service state",
            std::io::Error::new(std::io::ErrorKind::InvalidData, error),
        )
    })
}

pub(super) fn apply_state_intent(
    worker: &WorkerState,
    method: &str,
    params: serde_json::Value,
) -> Result<State, Error> {
    let value = call_service(worker, "gui-state-intent", method, params)?;
    value
        .get("state")
        .cloned()
        .ok_or_else(|| {
            Error::io(
                "decode service mutation",
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "service mutation response has no state",
                ),
            )
        })
        .and_then(|state| {
            serde_json::from_value(state).map_err(|error| {
                Error::io(
                    "decode service mutation state",
                    std::io::Error::new(std::io::ErrorKind::InvalidData, error),
                )
            })
        })
}

pub(super) fn state_intent_messages(
    worker: &WorkerState,
    method: &str,
    params: serde_json::Value,
    context: &str,
    success: &str,
    reconcile: bool,
) -> Vec<Message> {
    match apply_state_intent(worker, method, params) {
        Ok(state) => vec![Message::StateUpdated {
            state: Box::new(state),
            status: success.to_string(),
            reconcile,
        }],
        Err(error) => vec![Message::Failed(ErrorReport::new(context, &error))],
    }
}

pub(super) fn reconcile_through_service(
    worker: &WorkerState,
    projects: Vec<ProjectRef>,
) -> Result<(Reconciliation, State), Error> {
    let value = call_service(
        worker,
        "gui-reconcile",
        "state.reconcile",
        serde_json::json!({"projects": projects}),
    )?;
    #[derive(serde::Deserialize)]
    struct ServiceResult {
        reconciliation: Reconciliation,
        state: State,
    }
    serde_json::from_value::<ServiceResult>(value)
        .map(|result| (result.reconciliation, result.state))
        .map_err(|error| {
            Error::io(
                "decode service reconciliation",
                std::io::Error::new(std::io::ErrorKind::InvalidData, error),
            )
        })
}
