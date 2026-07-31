//! Handlers for the operations a caller performs on one line of work.
//!
//! Every one resolves its target from a worktree id plus service-owned state
//! and live git, so a caller never supplies a path or a command. They share the
//! control gate and the idempotency cache, which is why they sit together:
//! those two are what make a retry safe, and a handler added here without them
//! would be the one that starts a second agent.

use grove_core::model::Worktree;
use grove_core::protocol::EventKind;
use grove_core::protocol::Request;
use grove_core::protocol::Response;
use grove_core::reconcile::ProjectRef;
use grove_core::state::Mutation;
use grove_core::state::SessionRecord;
use grove_core::{config, git, protocol, reconcile, state, status, terminal, tmux, workflow};
use serde_json::json;

use super::params::{
    CloseSessionParams, ControlParams, OpenOrphanParams, ResumeFailure, ResumeRecordedParams,
    ResumeRecordedResult, StatusParams, StopServerParams,
};
use super::{ApiContext, api_result, lock_state};

pub(super) fn control(request: &Request, api: &ApiContext) -> Response {
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

pub(super) fn close_session(request: &Request, api: &ApiContext) -> Response {
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

pub(super) fn open_orphan_session(request: &Request, api: &ApiContext) -> Response {
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

pub(super) fn stop_server(request: &Request, api: &ApiContext) -> Response {
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

pub(super) fn run_control(
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

pub(super) fn resolve_worktree(
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

pub(super) fn record_control_session(
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

pub(super) fn forget_control_session(
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

pub(super) fn status_get(request: &Request, api: &ApiContext) -> Response {
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

pub(super) fn resume_recorded(request: &Request, api: &ApiContext) -> Response {
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

pub(super) fn run_resume_recorded(
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
