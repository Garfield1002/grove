//! Handlers for projects, worktrees and branches.
//!
//! The registration and lifecycle half of the API, kept apart from the
//! per-session control half: these change what Grove knows about, rather than
//! what is running. The destructive ones — removing a worktree, deleting a
//! branch — resolve an exact target and report what they touched.

use grove_core::protocol::EventKind;
use grove_core::protocol::Request;
use grove_core::protocol::Response;
use grove_core::state::Mutation;
use grove_core::state::ProjectRecord;
use grove_core::{config, git, protocol, state, terminal, workflow};
use serde_json::json;

use super::params::{
    CreateWorktreeParams, DeleteBranchParams, InspectRemovalParams, OpenProjectParams,
    ProjectExpandedParams, ProjectRefsParams, ProjectRemoveParams, ProjectStatusesParams,
    RefreshProjectParams, RemoveWorktreeParams, SessionIgnoreParams, SlotAssignParams,
    WorktreeIdentityParams,
};
use super::{ApiContext, api_result, lock_state, resolve_worktree};

pub(super) fn valid_identity(value: &str) -> bool {
    !value.is_empty() && !value.chars().any(char::is_control)
}

pub(super) fn set_project_expanded(request: &Request, api: &ApiContext) -> Response {
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

pub(super) fn remove_project(request: &Request, api: &ApiContext) -> Response {
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

pub(super) fn assign_slot(request: &Request, api: &ApiContext) -> Response {
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

pub(super) fn clear_slot(request: &Request, api: &ApiContext) -> Response {
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

pub(super) fn ignore_session(request: &Request, api: &ApiContext) -> Response {
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

pub(super) fn apply_intent(request: &Request, api: &ApiContext, mutation: Mutation) -> Response {
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

pub(super) fn project_refs(request: &Request, api: &ApiContext) -> Response {
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

pub(super) fn inspect_removal(request: &Request, api: &ApiContext) -> Response {
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

pub(super) fn project_statuses(request: &Request, api: &ApiContext) -> Response {
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

pub(super) fn refresh_project(request: &Request, api: &ApiContext) -> Response {
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

pub(super) fn open_project(request: &Request, api: &ApiContext) -> Response {
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

pub(super) fn create_worktree(request: &Request, api: &ApiContext) -> Response {
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

pub(super) fn remove_worktree(request: &Request, api: &ApiContext) -> Response {
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

pub(super) fn delete_branch(request: &Request, api: &ApiContext) -> Response {
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
