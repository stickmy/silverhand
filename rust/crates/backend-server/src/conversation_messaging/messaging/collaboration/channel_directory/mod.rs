//! Human-session HTTP adapters for the channel-directory capability.
use crate::foundation::http_error::json_error as error;

mod create;

pub use create::create_router;

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Extension, Path, Query, State, rejection::QueryRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde_json::{Map, Value, json};
use silverhand_backend_core::{
    conversation_messaging::messaging::collaboration::channel_directory::{
        ActiveChannelDirectoryPort, ArchivedSince, ArchivedWorkspaceChannelDirectoryPort,
        BoundActiveWorkspaceChannelDirectory, BoundArchivedWorkspaceChannelDirectory,
        BoundGlobalArchivedChannelDirectory, ChannelDirectoryRow, ChannelSortMode,
        GlobalArchivedChannelDirectoryPort, GlobalArchivedDirectoryRefusal,
        WorkspaceDirectoryRefusal,
    },
    foundation::identity::{UserId, WorkspaceId},
};

use super::query_number::{CoercedInteger, coerce_integer};

pub const ACTIVE_PATH: &str = "/api/workspaces/{id}/channels";
pub const WORKSPACE_ARCHIVED_PATH: &str = "/api/workspaces/{id}/channels/archived";
pub const GLOBAL_ARCHIVED_PATH: &str = "/api/archived-channels";

struct DirectoryState<P>(Arc<P>);
impl<P> Clone for DirectoryState<P> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

pub fn router<P>(port: P) -> Router
where
    P: ActiveChannelDirectoryPort
        + ArchivedWorkspaceChannelDirectoryPort
        + GlobalArchivedChannelDirectoryPort
        + Send
        + Sync
        + 'static,
{
    Router::new()
        .route(ACTIVE_PATH, get(active::<P>))
        .route(WORKSPACE_ARCHIVED_PATH, get(workspace_archived::<P>))
        .route(GLOBAL_ARCHIVED_PATH, get(global_archived::<P>))
        .with_state(DirectoryState(Arc::new(port)))
}

async fn active<P>(
    State(state): State<DirectoryState<P>>,
    principal: Option<Extension<UserId>>,
    Path(raw_workspace): Path<String>,
) -> Response
where
    P: ActiveChannelDirectoryPort,
{
    let Some(Extension(viewer)) = principal else {
        tracing::error!("channel-directory active route reached without human principal");
        return internal();
    };
    let Ok(workspace) = WorkspaceId::parse(raw_workspace) else {
        return error(StatusCode::BAD_REQUEST, "workspace id required");
    };
    match state.0.active(&viewer, &workspace).await {
        Ok(Ok(value)) => match value.bind(&viewer, &workspace) {
            Ok(value) => active_success(&value),
            Err(error) => {
                tracing::error!(
                    error_chain = crate::foundation::safe_log_field::safe_display(
                        &(crate::foundation::error::describe(&error))
                    ),
                    "active channel-directory identity seal failed"
                );
                internal()
            }
        },
        Ok(Err(refusal)) => workspace_refusal(refusal),
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "active channel-directory read failed"
            );
            internal()
        }
    }
}

async fn workspace_archived<P>(
    State(state): State<DirectoryState<P>>,
    principal: Option<Extension<UserId>>,
    Path(raw_workspace): Path<String>,
) -> Response
where
    P: ArchivedWorkspaceChannelDirectoryPort,
{
    let Some(Extension(viewer)) = principal else {
        tracing::error!("channel-directory archived route reached without human principal");
        return internal();
    };
    let Ok(workspace) = WorkspaceId::parse(raw_workspace) else {
        return error(StatusCode::BAD_REQUEST, "workspace id required");
    };
    match state.0.archived(&viewer, &workspace).await {
        Ok(Ok(value)) => match value.bind(&viewer, &workspace) {
            Ok(value) => workspace_archived_success(&value),
            Err(error) => {
                tracing::error!(
                    error_chain = crate::foundation::safe_log_field::safe_display(
                        &(crate::foundation::error::describe(&error))
                    ),
                    "archived channel-directory identity seal failed"
                );
                internal()
            }
        },
        Ok(Err(refusal)) => workspace_refusal(refusal),
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "archived channel-directory read failed"
            );
            internal()
        }
    }
}

async fn global_archived<P>(
    State(state): State<DirectoryState<P>>,
    principal: Option<Extension<UserId>>,
    query: Result<Query<Vec<(String, String)>>, QueryRejection>,
) -> Response
where
    P: GlobalArchivedChannelDirectoryPort,
{
    let Some(Extension(viewer)) = principal else {
        tracing::error!("global archived-channel route reached without human principal");
        return internal();
    };
    let Ok(Query(pairs)) = query else {
        return invalid();
    };
    let Ok((workspace, since)) = parse_archived_query(pairs) else {
        return invalid();
    };
    match state
        .0
        .archived_global(&viewer, workspace.as_ref(), since)
        .await
    {
        Ok(Ok(value)) => match value.bind(&viewer, workspace.as_ref()) {
            Ok(value) => global_archived_success(&value),
            Err(error) => {
                tracing::error!(
                    error_chain = crate::foundation::safe_log_field::safe_display(
                        &(crate::foundation::error::describe(&error))
                    ),
                    "global archived-channel identity seal failed"
                );
                internal()
            }
        },
        Ok(Err(GlobalArchivedDirectoryRefusal::WorkspaceNotFound)) => {
            error(StatusCode::NOT_FOUND, "workspace not found")
        }
        Ok(Err(GlobalArchivedDirectoryRefusal::Forbidden)) => {
            error(StatusCode::FORBIDDEN, "forbidden")
        }
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "global archived-channel read failed"
            );
            internal()
        }
    }
}

fn parse_archived_query(
    pairs: Vec<(String, String)>,
) -> Result<(Option<WorkspaceId>, Option<ArchivedSince>), ()> {
    // Hono's `c.req.query(name)` publishes first-occurrence semantics. A map or
    // struct extractor would reject a duplicate and turn a later value into a
    // new 400 contract.
    let workspace = pairs
        .iter()
        .find_map(|(key, value)| (key == "workspace_id").then(|| value.clone()))
        .filter(|value| {
            !crate::conversation_messaging::messaging::admission::ecmascript_trim(value).is_empty()
        })
        .map(WorkspaceId::parse)
        .transpose()
        .map_err(|_| ())?;
    let since = match pairs
        .into_iter()
        .find_map(|(key, value)| (key == "since").then_some(value))
        .filter(|value| !value.is_empty())
    {
        None => None,
        Some(raw) => Some(match coerce_integer(&raw).map_err(|_| ())? {
            CoercedInteger::Persisted(value) if value >= 0 => {
                ArchivedSince::from_ecmascript_number(value as f64).map_err(|_| ())?
            }
            CoercedInteger::Persisted(_) => return Err(()),
            CoercedInteger::BeyondPersistedRange => {
                ArchivedSince::from_ecmascript_number(9_223_372_036_854_775_808.0)
                    .map_err(|_| ())?
            }
        }),
    };
    Ok((workspace, since))
}

fn active_success(value: &BoundActiveWorkspaceChannelDirectory) -> Response {
    let modes = value
        .sort()
        .mode()
        .map(ChannelSortMode::as_str)
        .map(|mode| Map::from_iter([(String::new(), Value::String(mode.to_owned()))]))
        .unwrap_or_default();
    Json(json!({
        "channels": value.rows().iter().map(|row| channel(row, value.workspace())).collect::<Vec<_>>(),
        "viewer_sort": {
            "list_key": format!("channels:{}", value.workspace().as_str()),
            "modes": modes,
            "entries": value.sort().entries().iter().map(|entry| json!({
                "entity_id": entry.conversation().as_str(),
                "rank": entry.rank(),
                "last_accessed_at": entry.last_accessed_at(),
                "starred_at": entry.starred_at(),
            })).collect::<Vec<_>>(),
        }
    }))
    .into_response()
}

fn workspace_archived_success(value: &BoundArchivedWorkspaceChannelDirectory) -> Response {
    Json(json!({"channels": value.rows().iter().map(|row| channel(row, value.workspace())).collect::<Vec<_>>() })).into_response()
}

fn global_archived_success(value: &BoundGlobalArchivedChannelDirectory) -> Response {
    Json(json!({
        "channels": value.rows().iter().map(|row| json!({
            "channel": identity_channel(row.channel(), row.workspace()),
            "workspace": {
                "id": row.workspace().as_str(),
                "name": row.workspace_name(),
                "archived_at": row.workspace_archived_at(),
            }
        })).collect::<Vec<_>>()
    }))
    .into_response()
}

fn channel(row: &ChannelDirectoryRow, workspace: &WorkspaceId) -> Value {
    json!({
        "id": row.conversation().as_str(),
        "workspace_id": workspace.as_str(),
        "placement_kind": "workspace",
        "roster_policy": row.roster().as_str(),
        "title": row.title(),
        "status": row.status().as_str(),
        "spawned_by_agent_id": row.spawned_by_agent().map(|agent| agent.as_str()),
        "closed_at": row.closed_at(),
        "close_summary": row.close_summary(),
        "archived_at": row.archived_at(),
        "latest_message_seq": row.latest_message_seq(),
        "created_at": row.created_at(),
        "updated_at": row.updated_at(),
        "visibility": row.visibility().as_str(),
        "is_member": row.is_member(),
    })
}

fn identity_channel(row: &ChannelDirectoryRow, workspace: &WorkspaceId) -> Value {
    let mut value = channel(row, workspace);
    value
        .as_object_mut()
        .expect("channel projection is an object")
        .remove("is_member");
    value
}

fn workspace_refusal(refusal: WorkspaceDirectoryRefusal) -> Response {
    match refusal {
        WorkspaceDirectoryRefusal::NotFound => error(StatusCode::NOT_FOUND, "workspace not found"),
        WorkspaceDirectoryRefusal::Forbidden => error(StatusCode::FORBIDDEN, "forbidden"),
    }
}

fn invalid() -> Response {
    error(StatusCode::BAD_REQUEST, "invalid input")
}

fn internal() -> Response {
    error(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
}

#[cfg(test)]
mod tests;
