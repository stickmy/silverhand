//! `GET /internal/agents/{aid}/list-workspaces` — the agent's workspace list.
//!
//! Behind the runtime-credential perimeter: the acting owner is the credential's
//! agent, bound to the path `{aid}` by the shared runtime-actor gate BEFORE the
//! read. The durable workspace facts come from the reused
//! [`AgentWorkspaceListRead`]; the per-workspace `agent_dir` is a daemon-materialized
//! fact stamped by an [`AgentDirProbe`]. A transport/host/malformed probe fault is a
//! WHOLE-CALL typed failure (`Err(AgentDirFailure)`), never a per-entry
//! degradation; only within a SUCCESSFUL index does an id the daemon left unresolved
//! degrade to `agent_dir: null` per entry — exactly the reference's `adapter ??
//! null` inside a resolved index.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Extension, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Serialize;
use silverhand_backend_core::{
    foundation::identity::{AgentId, WorkspaceId},
    host_runtime::runtime_credential::RuntimePrincipal,
    identity_access::agent_workspace_list::{AgentListedWorkspace, AgentWorkspaceListRead},
};

use crate::foundation::http_error::logged_500 as internal;
use crate::host_runtime::runtime_credential::bind_runtime_actor;

pub const AGENT_LIST_WORKSPACES_PATH: &str = "/internal/agents/{aid}/list-workspaces";

/// A whole-call agent-dir probe failure: the port TYPE carries the TS
/// data-plane failure taxonomy (status/code/message) so a no-outbound / offline /
/// malformed-response fault projects a typed whole-response error, NOT a silent
/// all-entries-`null`. The route projects it verbatim.
#[derive(Clone, Debug)]
pub struct AgentDirFailure {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

/// Stamps the daemon-materialized `agent_dir` for the agent's workspaces via the
/// daemon `agent_workspace_index` RPC. A transport/host/malformed fault is a
/// whole-call `Err(AgentDirFailure)`; on success, an id the daemon leaves
/// unresolved is absent from the map → per-entry `agent_dir: null`. The probe is
/// NEVER called for an empty workspace set.
pub trait AgentDirProbe: Send + Sync + 'static {
    fn agent_dirs(
        &self,
        agent: &AgentId,
        workspaces: &[WorkspaceId],
    ) -> impl std::future::Future<
        Output = Result<std::collections::HashMap<WorkspaceId, String>, AgentDirFailure>,
    > + Send;
}

pub struct AgentListWorkspacesState<R, P> {
    pub reads: R,
    pub dirs: P,
}

struct Shared<R, P>(Arc<AgentListWorkspacesState<R, P>>);
impl<R, P> Clone for Shared<R, P> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

pub fn agent_list_workspaces_router<R, P>(state: AgentListWorkspacesState<R, P>) -> Router
where
    R: AgentWorkspaceListRead,
    P: AgentDirProbe,
{
    Router::new()
        .route(AGENT_LIST_WORKSPACES_PATH, get(list::<R, P>))
        .with_state(Shared(Arc::new(state)))
}

async fn list<R, P>(
    State(state): State<Shared<R, P>>,
    Path(aid): Path<String>,
    principal: Option<Extension<RuntimePrincipal>>,
) -> Response
where
    R: AgentWorkspaceListRead,
    P: AgentDirProbe,
{
    // IDENTITY GATE FIRST — the shared runtime-actor binding; a credential for
    // another agent is refused before the read is ever issued.
    let owner = match bind_runtime_actor(principal.as_ref().map(|e| &e.0), &aid) {
        Ok(principal) => principal.agent().clone(),
        Err(refusal) => return refusal.into_response(),
    };

    let workspaces = match state.0.reads.list_agent_workspaces(&owner).await {
        Ok(workspaces) => workspaces,
        Err(cause) => return internal(cause, "internal list-workspaces read failed"),
    };
    // Empty durable list is an EARLY success BEFORE any probe: the
    // reference returns `200 {workspaces:[]}` before agent existence / host / RPC, so
    // a ghost/no-workspace agent never becomes a 404/transport error.
    if workspaces.is_empty() {
        return (
            StatusCode::OK,
            Json(ListWorkspacesBody { workspaces: vec![] }),
        )
            .into_response();
    }
    let ids: Vec<WorkspaceId> = workspaces.iter().map(|w| w.id.clone()).collect();
    // The daemon `agent_workspace_index` probe. A transport/host/malformed fault is a
    // whole-response typed failure; success gives per-entry dirs (a missing id
    // degrades to `null`).
    let dirs = match state.0.dirs.agent_dirs(&owner, &ids).await {
        Ok(dirs) => dirs,
        Err(failure) => {
            return (
                failure.status,
                Json(serde_json::json!({
                    "error": failure.message,
                    "code": failure.code,
                })),
            )
                .into_response();
        }
    };
    let body = ListWorkspacesBody {
        workspaces: workspaces
            .into_iter()
            .map(|workspace| workspace_dto(workspace, &dirs))
            .collect(),
    };
    (StatusCode::OK, Json(body)).into_response()
}

fn workspace_dto(
    workspace: AgentListedWorkspace,
    dirs: &std::collections::HashMap<WorkspaceId, String>,
) -> AgentListedWorkspaceDto {
    let agent_dir = dirs.get(&workspace.id).cloned();
    AgentListedWorkspaceDto {
        id: workspace.id.into_inner(),
        name: workspace.name,
        owner_agent_id: workspace.owner_agent_id.map(AgentId::into_inner),
        archived: workspace.archived,
        updated_at: workspace.updated_at,
        agent_dir,
    }
}

/// `{ workspaces: [AgentListedWorkspace] }` — `AgentListWorkspacesDto`.
#[derive(Serialize)]
struct ListWorkspacesBody {
    workspaces: Vec<AgentListedWorkspaceDto>,
}

/// `AgentListedWorkspace`.
#[derive(Serialize)]
struct AgentListedWorkspaceDto {
    id: String,
    name: String,
    owner_agent_id: Option<String>,
    archived: bool,
    updated_at: i64,
    agent_dir: Option<String>,
}
