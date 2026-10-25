//! The agent board-list route: `GET /internal/agents/{aid}/tasks`.
//!
//! A thin composition over three core authorities, in one fixed order: parse the query
//! grammar, resolve the opaque board scope the list is confined to
//! ([`resolve_task_list_boards`]), fetch the candidate rows that scope's coordinate narrows
//! ([`TaskCandidateFetch`]), then let the core trim them to what THIS viewer may see
//! ([`list_visible_tasks`]) and project each survivor to its wire summary. The handler owns
//! no authorization rule of its own: which boards, which rows, and which viewer are all the
//! core's, and the fetch coordinate is projected from the witness the core minted — never a
//! bare board list the wire assembled.
//!
//! IDENTITY ARRIVES ALREADY AUTHENTICATED, exactly as the send door: a parent runtime
//! credential middleware injects the [`RuntimePrincipal`], and the agent a list runs for is
//! ALWAYS read from it, never from the `{aid}` path segment — so the route cannot be aimed
//! at another agent's boards by rewriting the URL. This route's own authorization is the
//! board scope the core resolves, distinct from that authentication.
//!
//! Every failure that is not a caller's is a safe-500 that says nothing: a resolver or fetch
//! `PersistenceFailure`, or a committed row too corrupt to project, is logged once and
//! answered with a bare `500`. Caller-shaped refusals carry the reason the resolver knows —
//! `context over control`: a selector naming a workspace that does not exist is a `404`
//! (`unknown_workspace`), while a workspace that exists but the agent cannot read is a `403`
//! (`workspace_membership_required` or `capability_denied`); only a frame with no board
//! degrades to a bare `403`. The id-enumeration this split exposes is an accepted tradeoff.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Request, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use silverhand_backend_core::conversation_messaging::messaging::task::{
    AgentBoardResolver, AgentReadableBoards, BoardScopeSource, TaskCandidateFetch,
    TaskCandidateQuery, TaskListAccess, TaskListForbidden, TaskParticipationReads,
    TaskReadCapability, TaskWriteDoorReads, list_visible_tasks, resolve_task_list_boards,
};
use silverhand_backend_core::conversation_messaging::messaging::{
    DeliveryIntent, DirectPairAllocation, TargetAddress,
};
use silverhand_backend_core::foundation::identity::WorkspaceId;
use silverhand_backend_core::host_runtime::runtime_credential::RuntimePrincipal;
use silverhand_backend_core::identity_access::workspace_name::CanonicalWorkspaceName;

use crate::conversation_messaging::messaging::router::address_refusal;

use super::query::{InvalidQuery, parse_list_query};
use super::wire::{ProjectionCorruption, TaskListDto, project_summary};

/// Everything the route needs, supplied by whoever composes it: the store that answers the
/// board-resolution, capability, and participation authorities, and the candidate fetch.
///
/// The two are kept as SEPARATE ports rather than one bundle because they are two genuinely
/// different reads — the authority store settles "which boards, which rows may this viewer
/// see", the fetch reads the joined candidate set — and a composition may back them with
/// different handles. The four authority traits below are the same [`MessagingStore`] the
/// send door already holds; the fetch is the SQLite task store.
///
/// [`MessagingStore`]: silverhand_backend_core
pub struct TaskListState<B, F, T> {
    /// The board-resolution, `read_tasks` capability, and per-row participation authority.
    pub boards: B,
    /// The joined candidate fetch the resolved coordinate narrows.
    pub fetch: F,
    /// The address authority a `target` filter resolves through (read intent).
    pub targets: T,
}

/// The board-list route, on the path the reference uses. `B` is cloned once per request so
/// the core's `&mut self` authority ports have an owned handle; the clone is pool-backed and
/// cheap (the send door shares one `Arc` state for the same reason this holds one).
pub fn task_list_router<B, F, T>(state: TaskListState<B, F, T>) -> Router
where
    B: AgentBoardResolver
        + AgentReadableBoards
        + TaskReadCapability
        + TaskParticipationReads
        + TaskWriteDoorReads
        + Clone
        + Send
        + Sync
        + 'static,
    F: TaskCandidateFetch + Send + Sync + 'static,
    T: TargetAddress + Send + Sync + 'static,
{
    Router::new()
        .route("/internal/agents/{aid}/tasks", get(list_route::<B, F, T>))
        .with_state(Arc::new(state))
}

async fn list_route<B, F, T>(
    State(state): State<Arc<TaskListState<B, F, T>>>,
    request: Request,
) -> Response
where
    B: AgentBoardResolver
        + AgentReadableBoards
        + TaskReadCapability
        + TaskParticipationReads
        + TaskWriteDoorReads
        + Clone
        + Send
        + Sync
        + 'static,
    F: TaskCandidateFetch + Send + Sync + 'static,
    T: TargetAddress + Send + Sync + 'static,
{
    // Read, never derived: the agent is the authenticated principal, not the path segment.
    let Some(principal) = request.extensions().get::<RuntimePrincipal>().cloned() else {
        return composition_failure("agent credential");
    };

    let query = match parse_list_query(request.uri().query()) {
        Ok(query) => query,
        Err(invalid) => return invalid_query(invalid),
    };
    let limit = query.limit;

    // The core's authority ports are `&mut self`; a per-request clone gives them an owned
    // handle without a second app-wide store.
    let mut boards = state.boards.clone();

    let mut conversation = query.conversation_id;
    let selector = if let Some(target) = &query.target {
        let resolved = match state
            .targets
            .resolve(
                principal.agent(),
                target,
                DirectPairAllocation::Forbidden,
                DeliveryIntent::Read,
            )
            .await
        {
            Ok(Ok(resolved)) => resolved,
            Ok(Err(refusal)) => return address_refusal(refusal),
            Err(_) => return safe_500("resolve_target"),
        };
        if resolved.thread_root.is_some() {
            return thread_suffix_not_supported();
        }
        conversation = Some(resolved.conversation.clone());
        Some(BoardScopeSource::FrameConversation(resolved.conversation))
    } else if let Some(name) = &query.workspace_name {
        match workspace_named(&mut boards, name).await {
            Ok(Ok(workspace)) => Some(BoardScopeSource::SelectedWorkspace(workspace)),
            Ok(Err(refusal)) => return refusal,
            Err(_) => return safe_500("resolve_workspace_name"),
        }
    } else {
        query.workspace_id.map(BoardScopeSource::SelectedWorkspace)
    };

    let scope = match resolve_task_list_boards(&principal, selector.as_ref(), &mut boards).await {
        Ok(TaskListAccess::Authorized(scope)) => scope,
        Ok(TaskListAccess::UnknownWorkspace(workspace)) => return unknown_workspace(&workspace),
        Ok(TaskListAccess::Forbidden(reason)) => return forbidden(reason),
        Err(_) => return safe_500("resolve_boards"),
    };

    // The fetch narrows by the coordinate the witness projects; the per-row re-bind inside
    // `list_visible_tasks` stays the authority, so this `WHERE` carries no trust.
    let candidate_query = TaskCandidateQuery {
        boards: scope.fetch_coordinate(),
        conversation,
        statuses: query.statuses,
        task_numbers: query.task_numbers,
        assignee_agent: query.assignee_agent,
    };

    let candidates = match state.fetch.fetch_list_candidates(&candidate_query).await {
        Ok(candidates) => candidates,
        Err(_) => return safe_500("fetch_candidates"),
    };

    let rows = match list_visible_tasks(&scope, candidates, &mut boards, limit).await {
        Ok(rows) => rows,
        Err(_) => return safe_500("filter_visible"),
    };

    let mut tasks = Vec::with_capacity(rows.len());
    for row in &rows {
        match project_summary(row.summary()) {
            Ok(dto) => tasks.push(dto),
            Err(corruption) => return corrupt_row(corruption),
        }
    }

    (StatusCode::OK, Json(TaskListDto { tasks })).into_response()
}

/// The selected workspace does not exist — a `404` naming it, the parity of the TS
/// route's `unknown_workspace`. `context over control`: the caller learns the real
/// reason the server knows, and the id-enumeration this exposes is an accepted tradeoff.
fn unknown_workspace(workspace: &WorkspaceId) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({
            "error": format!("workspace {} not found", workspace.as_str()),
            "code": "unknown_workspace",
        })),
    )
        .into_response()
}

/// Resolve `--workspace <name>` to exactly one workspace; membership stays the board
/// resolver's. Same refusal codes as the by-number task doors.
async fn workspace_named<B: TaskWriteDoorReads>(
    reads: &mut B,
    raw: &str,
) -> Result<Result<WorkspaceId, Response>, ()> {
    let unknown = || {
        coded(
            StatusCode::NOT_FOUND,
            format!("no workspace named \"{raw}\""),
            "unknown_workspace",
            serde_json::json!({}),
        )
    };
    let Ok(canonical) = CanonicalWorkspaceName::parse(raw) else {
        return Ok(Err(unknown()));
    };
    let mut candidates = reads.workspaces_named(&canonical).await.map_err(|_| ())?;
    match candidates.len() {
        0 => Ok(Err(unknown())),
        1 => Ok(Ok(candidates.remove(0))),
        n => Ok(Err(coded(
            StatusCode::CONFLICT,
            format!("{n} workspaces named \"{raw}\"; disambiguate with --workspace-id"),
            "ambiguous_workspace",
            serde_json::json!({
                "workspace_candidates": candidates.iter().map(|w| w.as_str()).collect::<Vec<_>>(),
            }),
        ))),
    }
}

fn thread_suffix_not_supported() -> Response {
    coded(
        StatusCode::BAD_REQUEST,
        "task list filters by conversation, not a reply cluster".to_owned(),
        "thread_suffix_not_supported",
        serde_json::json!({}),
    )
}

fn coded(status: StatusCode, error: String, code: &str, extra: serde_json::Value) -> Response {
    let mut body = serde_json::json!({ "error": error, "code": code });
    if let (Some(map), serde_json::Value::Object(extra)) = (body.as_object_mut(), extra) {
        map.extend(extra);
    }
    (status, Json(body)).into_response()
}

/// A single named board the agent may not read — a `403` carrying the reason the
/// resolver knows, the parity of the TS route's `workspace_membership_required` and
/// `capability_denied` refusals (a bare `forbidden` only for a frame with no board).
fn forbidden(reason: TaskListForbidden) -> Response {
    let body = match reason {
        TaskListForbidden::NotAWorkspaceMember(workspace) => serde_json::json!({
            "error": format!("agent is not a member of workspace {}", workspace.as_str()),
            "code": "workspace_membership_required",
        }),
        // The workspace is the same `this workspace` the message names; the TS refusal
        // carries the capability, not the id, so it is not echoed here.
        TaskListForbidden::ReadTasksDenied(_) => serde_json::json!({
            "code": "capability_denied",
            "capability": "read_tasks",
            "message": "missing 'read_tasks' capability in this workspace",
            "details": { "capability": "read_tasks" },
        }),
        TaskListForbidden::NoReadableBoard => serde_json::json!({ "error": "forbidden" }),
    };
    (StatusCode::FORBIDDEN, Json(body)).into_response()
}

/// The query named a value outside its schema. A `400` whose machine `code` stays the stable
/// `invalid_query` and whose `message` names the offending field and the rule it broke — the
/// reference's zod `issues` array is a TS-schema artifact and is not reproduced (see the
/// Acceptance Summary).
fn invalid_query(invalid: InvalidQuery) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({
            "code": "invalid_query",
            "message": invalid.message(),
        })),
    )
        .into_response()
}

/// A principal the composition was supposed to inject is missing — reported once, never to
/// the caller, and never degraded to an anonymous identity (the send door's rule).
fn composition_failure(missing: &str) -> Response {
    tracing::error!(
        missing,
        "task-list route reached without its principal fact"
    );
    internal()
}

/// A committed row reached projection in a shape a correct writer never stores. Logged with
/// the offending column for the operator; the caller sees only a bare `500`.
fn corrupt_row(corruption: ProjectionCorruption) -> Response {
    tracing::error!(
        field = corruption.field(),
        "task-list projection rejected a corrupt committed row"
    );
    internal()
}

/// An authority (resolver, fetch, or participation) failed. The `PersistenceFailure` is
/// already erased of any spelling; the wire says nothing beyond the status.
fn safe_500(stage: &'static str) -> Response {
    tracing::error!(operation = "task.list", stage, "task read failed");
    internal()
}

fn internal() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "Internal Server Error",
    )
        .into_response()
}
