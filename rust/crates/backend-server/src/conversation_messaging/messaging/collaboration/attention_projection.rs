//! Authenticated HTTP adapter for tiered-attention cold projections.

use crate::foundation::http_error::json_error as error;
use axum::{
    Json, Router,
    extract::{Extension, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde_json::{Map, Value, json};
use silverhand_backend_core::{
    conversation_messaging::messaging::collaboration::attention_projection::{
        AttentionBadgeRollup, AttentionModesRefusal, AttentionProjectionPort,
        WorkspaceAttentionModes,
    },
    foundation::identity::{UserId, WorkspaceId},
};
use std::sync::Arc;

pub const BADGES_PATH: &str = "/api/attention/badges";
pub const MODES_PATH: &str = "/api/attention/modes";

struct ProjectionState<P>(Arc<P>);
impl<P> Clone for ProjectionState<P> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

pub fn router<P>(port: P) -> Router
where
    P: AttentionProjectionPort + Send + Sync + 'static,
{
    Router::new()
        .route(BADGES_PATH, get(badges::<P>))
        .route(MODES_PATH, get(modes::<P>))
        .with_state(ProjectionState(Arc::new(port)))
}

async fn badges<P: AttentionProjectionPort>(
    State(state): State<ProjectionState<P>>,
    principal: Option<Extension<UserId>>,
) -> Response {
    let Some(Extension(viewer)) = principal else {
        return internal();
    };
    match state.0.badges(&viewer).await {
        Ok(value) => badge_wire(&value),
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "attention badge projection failed"
            );
            internal()
        }
    }
}

async fn modes<P: AttentionProjectionPort>(
    State(state): State<ProjectionState<P>>,
    principal: Option<Extension<UserId>>,
    Query(query): Query<Vec<(String, String)>>,
) -> Response {
    let Some(Extension(viewer)) = principal else {
        return internal();
    };
    // Hono's `query(name)` returns the first occurrence. A map/struct extractor
    // would reject or overwrite duplicates and silently change that wire rule.
    let Some(raw_workspace) = query
        .into_iter()
        .find_map(|(key, value)| (key == "workspace_id").then_some(value))
        .filter(|value| !value.is_empty())
    else {
        return error(StatusCode::BAD_REQUEST, "workspace_id required");
    };
    let workspace = match WorkspaceId::parse(raw_workspace) {
        Ok(value) => value,
        Err(_) => return error(StatusCode::FORBIDDEN, "forbidden"),
    };
    match state.0.modes(&viewer, &workspace).await {
        Ok(Ok(value)) => modes_wire(&workspace, &value),
        Ok(Err(AttentionModesRefusal::Forbidden)) => error(StatusCode::FORBIDDEN, "forbidden"),
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "attention mode projection failed"
            );
            internal()
        }
    }
}

fn badge_wire(value: &AttentionBadgeRollup) -> Response {
    let by_workspace = value
        .by_workspace()
        .iter()
        .map(|entry| {
            (
                entry.workspace().as_str().to_owned(),
                Value::from(entry.count().get()),
            )
        })
        .collect::<Map<String, Value>>();
    Json(json!({
        "by_workspace": by_workspace,
        "total": value.total().get(),
        "muted_workspace_ids": value.muted_workspaces().iter().map(WorkspaceId::as_str).collect::<Vec<_>>()
    }))
    .into_response()
}

fn modes_wire(workspace: &WorkspaceId, value: &WorkspaceAttentionModes) -> Response {
    let channels = value
        .channels()
        .iter()
        .map(|channel| {
            json!({
                "conversation_id": channel.conversation().as_str(),
                "mode": channel.mode().as_str(),
                "effective_mode": channel.effective_mode(value.muted()).as_str(),
                "directed_unread_count": channel.directed_unread_count().get(),
            })
        })
        .collect::<Vec<_>>();
    Json(json!({
        "workspace_id": workspace.as_str(),
        "muted": value.muted(),
        "channels": channels,
    }))
    .into_response()
}

fn internal() -> Response {
    error(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
}

#[cfg(test)]
mod tests {
    include!("attention_projection_tests.rs");
}
