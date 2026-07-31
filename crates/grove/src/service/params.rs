//! The typed request and response bodies of the service API.
//!
//! Separated from the handlers because they are the API's actual surface: this
//! is the file to read to see what a caller may send, without the machinery of
//! what happens next. Every one denies unknown fields, so a caller's typo is an
//! error rather than a silently ignored intention.

use std::collections::HashSet;

use grove_core::git::WorktreeAdd;
use grove_core::protocol::EventKind;
use grove_core::reconcile::{ProjectRef, Reconciliation};
use grove_core::state::{Mutation, State};

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct MutateStateParams {
    pub(super) mutation: Mutation,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReconcileParams {
    pub(super) projects: Vec<ProjectRef>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SubscribeParams {
    pub(super) topics: HashSet<EventKind>,
    #[serde(default)]
    pub(super) client: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct UnsubscribeParams {
    pub(super) subscription_id: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ControlParams {
    pub(super) worktree_id: String,
    #[serde(default)]
    pub(super) idempotency_key: Option<String>,
    #[serde(default)]
    pub(super) resume: Option<String>,
    #[serde(default)]
    pub(super) window_index: Option<u32>,
    #[serde(default)]
    pub(super) orphan_session: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StatusParams {
    pub(super) worktree_id: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ResumeRecordedParams {
    pub(super) idempotency_key: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CloseSessionParams {
    pub(super) session: String,
    pub(super) idempotency_key: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OpenOrphanParams {
    pub(super) session: String,
    pub(super) idempotency_key: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StopServerParams {
    pub(super) idempotency_key: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OpenProjectParams {
    pub(super) path: std::path::PathBuf,
    pub(super) idempotency_key: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RefreshProjectParams {
    pub(super) project_id: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProjectStatusesParams {
    pub(super) project_id: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProjectRefsParams {
    pub(super) project_id: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct InspectRemovalParams {
    pub(super) worktree_id: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProjectExpandedParams {
    pub(super) project_id: String,
    pub(super) expanded: bool,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProjectRemoveParams {
    pub(super) project_id: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SlotAssignParams {
    pub(super) number: u8,
    pub(super) worktree_id: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WorktreeIdentityParams {
    pub(super) worktree_id: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SessionIgnoreParams {
    pub(super) session: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CreateWorktreeParams {
    pub(super) project_id: String,
    pub(super) add: WorktreeAdd,
    pub(super) idempotency_key: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RemoveWorktreeParams {
    pub(super) worktree_id: String,
    pub(super) force: bool,
    pub(super) idempotency_key: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DeleteBranchParams {
    pub(super) project_id: String,
    pub(super) branch: String,
    pub(super) force: bool,
    pub(super) idempotency_key: String,
}

#[derive(Clone, serde::Serialize)]
pub(super) struct ResumeRecordedResult {
    pub(super) worktree_ids: Vec<String>,
    pub(super) failures: Vec<ResumeFailure>,
}

#[derive(Clone, serde::Serialize)]
pub(super) struct ResumeFailure {
    pub(super) worktree_path: std::path::PathBuf,
    pub(super) message: String,
}

#[derive(serde::Serialize)]
pub(super) struct ReconcileResult<'a> {
    pub(super) reconciliation: &'a Reconciliation,
    pub(super) state: &'a State,
}
