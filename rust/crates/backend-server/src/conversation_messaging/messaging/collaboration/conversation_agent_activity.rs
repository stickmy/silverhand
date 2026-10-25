//! Authenticated HTTP adapter for persisted conversation-agent activity.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Extension, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Deserialize;
use serde_json::{Value, json};
use silverhand_backend_core::{
    conversation_messaging::messaging::collaboration::conversation_agent_activity::{
        ActivityBefore, ActivityEventId, ActivityPageRequest, ActivityPayload,
        AgentActivityReadPort, BoundConversationAgentActivityPage, ConversationActivityEvent,
        ConversationAgentActivityReadPort, ConversationAgentActivityRefusal,
    },
    foundation::identity::{AgentId, ConversationId, UserId},
};

use super::query_number::{CoercedInteger, coerce_integer};
use crate::foundation::http_error::{coded_error, json_error as error};

pub const CONVERSATION_AGENT_ACTIVITY_PATH: &str = "/api/conversations/{id}/agents/{aid}/activity";

struct ActivityState<P>(Arc<P>);

impl<P> Clone for ActivityState<P> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

pub fn router<P>(port: P) -> Router
where
    P: ConversationAgentActivityReadPort + Send + Sync + 'static,
{
    Router::new()
        .route(CONVERSATION_AGENT_ACTIVITY_PATH, get(read::<P>))
        .with_state(ActivityState(Arc::new(port)))
}

async fn read<P: ConversationAgentActivityReadPort>(
    State(state): State<ActivityState<P>>,
    principal: Option<Extension<UserId>>,
    Path((raw_conversation, raw_agent)): Path<(String, String)>,
    Query(query): Query<ActivityQuery>,
) -> Response {
    let Some(Extension(viewer)) = principal else {
        tracing::error!("conversation-agent-activity route reached without human principal");
        return internal();
    };
    let conversation = match ConversationId::parse(raw_conversation) {
        Ok(value) => value,
        Err(_) => {
            return error(
                StatusCode::BAD_REQUEST,
                "conversation id and agent id required",
            );
        }
    };
    let agent = match AgentId::parse(raw_agent) {
        Ok(value) => value,
        Err(_) => {
            return error(
                StatusCode::BAD_REQUEST,
                "conversation id and agent id required",
            );
        }
    };

    // TS resolves conversation visibility and target participation before it
    // parses pagination. This preflight preserves that observable order, but is
    // not an authority token: the read port rechecks both in its own snapshot.
    match state.0.admit(&viewer, &conversation, &agent).await {
        Ok(Ok(())) => {}
        Ok(Err(refusal)) => return refusal_wire(refusal),
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "conversation-agent-activity admission failed"
            );
            return internal();
        }
    }

    let page = match page_request(&query) {
        Ok(value) => value,
        Err(QueryRefusal::Limit) => {
            return coded_error(StatusCode::BAD_REQUEST, "invalid limit", "invalid_limit");
        }
        Err(QueryRefusal::Before) => {
            return coded_error(
                StatusCode::BAD_REQUEST,
                "invalid before_id",
                "invalid_before_id",
            );
        }
    };

    match state.0.read(&viewer, &conversation, &agent, page).await {
        Ok(Ok(value)) => match value.bind(&viewer, &conversation, &agent) {
            Ok(value) => match success(&value) {
                Ok(response) => response,
                Err(error) => {
                    tracing::error!(
                        error_chain = crate::foundation::safe_log_field::safe_display(
                            &(crate::foundation::error::describe(&error))
                        ),
                        "conversation-agent-activity payload projection failed"
                    );
                    internal()
                }
            },
            Err(error) => {
                tracing::error!(
                    error_chain = crate::foundation::safe_log_field::safe_display(
                        &(crate::foundation::error::describe(&error))
                    ),
                    "conversation-agent-activity identity seal failed"
                );
                internal()
            }
        },
        Ok(Err(refusal)) => refusal_wire(refusal),
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "conversation-agent-activity projection failed"
            );
            internal()
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum QueryRefusal {
    Limit,
    Before,
}

#[derive(Debug, Default, Deserialize)]
struct ActivityQuery {
    limit: Option<String>,
    before_id: Option<String>,
}

fn page_request(query: &ActivityQuery) -> Result<ActivityPageRequest, QueryRefusal> {
    let limit = match query.limit.as_deref() {
        None => 100,
        Some(raw) => match coerce_integer(raw).map_err(|_| QueryRefusal::Limit)? {
            CoercedInteger::Persisted(value) if value >= 1 => value.min(200) as u16,
            CoercedInteger::BeyondPersistedRange => 200,
            CoercedInteger::Persisted(_) => return Err(QueryRefusal::Limit),
        },
    };
    let before = match query.before_id.as_deref() {
        None => None,
        Some(raw) => Some(
            match coerce_integer(raw).map_err(|_| QueryRefusal::Before)? {
                CoercedInteger::Persisted(value) if value >= 1 => ActivityBefore::Persisted(
                    ActivityEventId::new(value).map_err(|_| QueryRefusal::Before)?,
                ),
                CoercedInteger::BeyondPersistedRange => ActivityBefore::BeyondPersistedRange,
                CoercedInteger::Persisted(_) => return Err(QueryRefusal::Before),
            },
        ),
    };
    ActivityPageRequest::new(limit, before).map_err(|_| QueryRefusal::Limit)
}

#[derive(Debug)]
enum ActivityWireError {
    Payload,
}

impl std::fmt::Display for ActivityWireError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("activity payload is not a JSON object")
    }
}

impl std::error::Error for ActivityWireError {}

fn success(page: &BoundConversationAgentActivityPage) -> Result<Response, ActivityWireError> {
    let events = page
        .events()
        .iter()
        .map(event_wire)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Json(json!({
        "events": events,
        "next_before_id": page.next_before().map(ActivityEventId::get),
    }))
    .into_response())
}

fn event_wire(event: &ConversationActivityEvent) -> Result<Value, ActivityWireError> {
    let payload = match event.payload() {
        ActivityPayload::Null => Value::Null,
        ActivityPayload::ObjectJson(raw) => {
            let parsed: Value =
                serde_json::from_str(raw).map_err(|_| ActivityWireError::Payload)?;
            if !parsed.is_object() {
                return Err(ActivityWireError::Payload);
            }
            parsed
        }
    };
    Ok(json!({
        "id": event.id().get(),
        "conversation_id": event.conversation().as_str(),
        "agent_id": event.agent().map(AgentId::as_str),
        "kind": event.kind().as_str(),
        "payload": payload,
        "created_at": event.created_at().get(),
    }))
}

fn refusal_wire(refusal: ConversationAgentActivityRefusal) -> Response {
    match refusal {
        ConversationAgentActivityRefusal::NotFound => {
            error(StatusCode::NOT_FOUND, "conversation not found")
        }
        ConversationAgentActivityRefusal::Forbidden => error(StatusCode::FORBIDDEN, "forbidden"),
        ConversationAgentActivityRefusal::TargetNotParticipant => error(
            StatusCode::NOT_FOUND,
            "agent is not a member of this conversation",
        ),
    }
}

fn internal() -> Response {
    error(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
}

/// Canonical detailed stream for immutable agent-scoped agents.
pub fn agent_router<P>(port: P) -> Router
where
    P: AgentActivityReadPort + 'static,
{
    Router::new()
        .route("/api/agents/{aid}/activity", get(read_agent::<P>))
        .with_state(ActivityState(Arc::new(port)))
}
async fn read_agent<P: AgentActivityReadPort>(
    State(state): State<ActivityState<P>>,
    principal: Option<Extension<UserId>>,
    Path(raw): Path<String>,
    Query(query): Query<ActivityQuery>,
) -> Response {
    let Some(Extension(viewer)) = principal else {
        return error(StatusCode::UNAUTHORIZED, "unauthorized");
    };
    let Ok(agent) = AgentId::parse(raw) else {
        return error(StatusCode::BAD_REQUEST, "invalid agent id");
    };
    let page = match page_request(&query) {
        Ok(v) => v,
        Err(_) => return error(StatusCode::BAD_REQUEST, "invalid pagination"),
    };
    match state.0.read_agent_activity(&viewer, &agent, page).await {
        Ok(Some(page)) if page.bind(&viewer, &agent).is_ok() => {
            let events = page
                .events()
                .iter()
                .map(|event| {
                    let mut wire = event_wire(event)?;
                    wire.as_object_mut()
                        .expect("event object")
                        .remove("conversation_id");
                    Ok(wire)
                })
                .collect::<Result<Vec<_>, ActivityWireError>>();
            match events {
                Ok(events) => Json(json!({
                    "agent_id": agent.as_str(),
                    "runtime_scope": "agent",
                    "events": events,
                    "next_before_id": page.next_before().map(ActivityEventId::get),
                }))
                .into_response(),
                Err(_) => internal(),
            }
        }
        Ok(None) => error(StatusCode::NOT_FOUND, "agent not found"),
        _ => internal(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    include!("conversation_agent_activity_tests.rs");
}
