//! `GET /internal/agents/{aid}/list-channels` — a workspace's channels the agent
//! can see.
//!
//! Behind the runtime-credential perimeter (the acting agent is the credential,
//! bound to `{aid}` first). Scope resolution mirrors the reference: a
//! ConversationSlot credential defaults to its source conversation's workspace; an
//! explicit `workspace_id` overrides it and, when it differs, is gated by the
//! agent's cross-workspace reach; an AgentSlot credential (no source) MUST name a
//! `workspace_id`. The target workspace then requires the agent's `read_messages`
//! capability. The visible set is the reused member∪discovery read.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Extension, FromRequestParts, Path, Query, State},
    http::StatusCode,
    http::request::Parts,
    response::{IntoResponse, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};
use silverhand_backend_core::{
    conversation_messaging::messaging::collaboration::agent_channel_list::{
        AgentChannelListRead, AgentListedChannel, ListChannelsRefusal, ListChannelsResult,
    },
    conversation_messaging::messaging::collaboration::message_target_coordinates::ConversationPlacementKind,
    foundation::identity::WorkspaceId,
    host_runtime::runtime_credential::RuntimePrincipal,
};

use crate::conversation_messaging::messaging::agent_capability_wire::capability_denied;
use crate::foundation::http_error::{json_error as error, logged_500 as internal};
use crate::host_runtime::runtime_credential::bind_runtime_actor;

pub const AGENT_LIST_CHANNELS_PATH: &str = "/internal/agents/{aid}/list-channels";

/// Only the read authority: it resolves the scope + authorizes in one snapshot.
pub struct AgentListChannelsState<R> {
    pub reads: R,
}

struct Shared<R>(Arc<AgentListChannelsState<R>>);
impl<R> Clone for Shared<R> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

pub fn agent_list_channels_router<R>(state: AgentListChannelsState<R>) -> Router
where
    R: AgentChannelListRead,
{
    Router::new()
        .route(AGENT_LIST_CHANNELS_PATH, get(list::<R>))
        .with_state(Shared(Arc::new(state)))
}

/// `AgentListChannelsQuerySchema`: `{ workspace_id?: string (min 1) }`. The reference
/// normalizes an empty string to absent BEFORE Zod (`c.req.query('workspace_id') ||
/// undefined`), so `?workspace_id=` defaults to the source — NOT a bad query (#24).
#[derive(Deserialize)]
struct ListChannelsQuery {
    workspace_id: Option<String>,
}

async fn list<R>(
    State(state): State<Shared<R>>,
    Path(aid): Path<String>,
    principal: Option<Extension<RuntimePrincipal>>,
    mut parts: Parts,
) -> Response
where
    R: AgentChannelListRead,
{
    // IDENTITY FIRST (#25): bind `{aid}` before parsing the query.
    let principal = match bind_runtime_actor(principal.as_ref().map(|e| &e.0), &aid) {
        Ok(principal) => principal,
        Err(refusal) => return refusal.into_response(),
    };
    let agent = principal.agent().clone();
    let Query(query) = match Query::<ListChannelsQuery>::from_request_parts(&mut parts, &()).await {
        Ok(query) => query,
        Err(_) => return invalid_query(),
    };
    // Empty `workspace_id` normalizes to absent (default-to-source), matching the TS
    // `|| undefined` (#24); a present non-empty value must parse.
    let explicit_workspace = match query.workspace_id {
        Some(raw) if raw.is_empty() => None,
        Some(raw) => match WorkspaceId::parse(raw) {
            Ok(id) => Some(id),
            Err(_) => return invalid_query(),
        },
        None => None,
    };

    let source = principal.source().cloned();
    let result = match state
        .0
        .reads
        .list_channels(&agent, source.as_ref(), explicit_workspace.as_ref())
        .await
    {
        Ok(Ok(result)) => result,
        Ok(Err(refusal)) => return list_refusal(refusal),
        Err(cause) => return internal(cause, "list-channels read failed"),
    };

    let ListChannelsResult {
        workspace,
        channels,
    } = result;
    (
        StatusCode::OK,
        Json(ListChannelsBody {
            workspace_id: workspace.into_inner(),
            channels: channels.into_iter().map(channel_dto).collect(),
        }),
    )
        .into_response()
}

/// Map the in-snapshot refusal to the reference wire. The workspace-scope refusal
/// carries the source placement kind for the reference message.
fn list_refusal(refusal: ListChannelsRefusal) -> Response {
    match refusal {
        ListChannelsRefusal::SourceNotFound => {
            error(StatusCode::NOT_FOUND, "conversation not found")
        }
        ListChannelsRefusal::WorkspaceForbidden => error(StatusCode::FORBIDDEN, "forbidden"),
        ListChannelsRefusal::CapabilityDenied => capability_denied("read_messages"),
        ListChannelsRefusal::WorkspaceScopeRequired {
            source_placement_kind,
        } => workspace_scope_required(source_placement_kind),
    }
}

fn channel_dto(channel: AgentListedChannel) -> AgentListedChannelDto {
    AgentListedChannelDto {
        id: channel.id.into_inner(),
        title: channel.title,
        roster_policy: channel.roster_policy.as_str(),
        visibility: channel.visibility.as_str(),
        archived: channel.archived,
        is_member: channel.is_member,
        updated_at: channel.updated_at,
    }
}

/// `{ workspace_id, channels: [AgentListedChannel] }` — `AgentListChannelsDto`.
#[derive(Serialize)]
struct ListChannelsBody {
    workspace_id: String,
    channels: Vec<AgentListedChannelDto>,
}

/// `AgentListedChannelSchema`.
#[derive(Serialize)]
struct AgentListedChannelDto {
    id: String,
    title: String,
    roster_policy: &'static str,
    visibility: &'static str,
    archived: bool,
    is_member: bool,
    updated_at: i64,
}

fn invalid_query() -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": "invalid query" })),
    )
        .into_response()
}

/// The workspace-scope refusal: the message provenance-branches on the
/// caller. An AgentSlot / null source uses the generic phrasing; a workspace-less
/// direct-pair source uses the `resolveAgentQueryWorkspaceId` list-channels variant
/// carrying the placement kind. Both are `400 workspace_scope_required`.
fn workspace_scope_required(source_placement_kind: Option<ConversationPlacementKind>) -> Response {
    let error = match source_placement_kind {
        Some(kind) => format!(
            "this conversation has no workspace (placement_kind={}), so a workspace-scoped query must name one: pass workspace_id",
            kind.as_str()
        ),
        None => "agent-scoped channel listing must name workspace_id".to_owned(),
    };
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({
            "error": error,
            "code": "workspace_scope_required"
        })),
    )
        .into_response()
}
