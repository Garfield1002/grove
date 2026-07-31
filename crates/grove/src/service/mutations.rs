//! State mutation, reconciliation, and the shared response helpers.
//!
//! The service is the sole writer of `state.toml`, and this is where that
//! ownership is exercised: every path through here takes one lock and ends in
//! an atomic save, so two requests cannot interleave into a half-written index.

use grove_core::protocol::EventKind;
use grove_core::protocol::Request;
use grove_core::protocol::Response;
use grove_core::reconcile::Reconciliation;
use grove_core::state::SessionRecord;
use grove_core::state::State;
use grove_core::{reconcile, state};
use serde_json::json;

use super::ApiContext;
use super::params::{MutateStateParams, ReconcileParams, ReconcileResult};

pub(super) fn mutate_state(request: &Request, api: &ApiContext) -> Response {
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

pub(super) fn reconcile_state(request: &Request, api: &ApiContext) -> Response {
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

pub(super) fn lock_state(api: &ApiContext) -> std::sync::MutexGuard<'_, ()> {
    api.state_gate
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(super) fn record_live_sessions(state: &mut State, result: &Reconciliation) -> bool {
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

pub(super) fn with_state(
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

pub(super) fn api_result(
    request: &Request,
    operation: impl FnOnce() -> std::result::Result<serde_json::Value, Box<dyn std::error::Error>>,
) -> Response {
    match operation() {
        Ok(result) => Response::success(&request.id, result),
        Err(error) => Response::error(&request.id, "internal_error", error.to_string()),
    }
}
