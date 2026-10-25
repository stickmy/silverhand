//! Human-session HTTP adapter for viewer-private attention preferences.
use crate::foundation::http_error::json_error as error;

#[cfg(test)]
mod tests;

use axum::{
    Json, Router,
    extract::{Extension, FromRequest, Path, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, put},
};
use garde::Validate;
use serde::Deserialize;
use serde_json::json;
use silverhand_backend_core::{
    conversation_messaging::messaging::collaboration::attention_preference::{
        AttentionPreferenceAftermath, AttentionPreferencePort, ChannelAttentionAdmissionRefusal,
        ChannelAttentionCommit, ChannelAttentionMode, ChannelAttentionWriteRefusal,
        WorkspaceAttentionAdmissionRefusal, WorkspaceAttentionCommit,
        WorkspaceAttentionReadRefusal, WorkspaceAttentionWriteRefusal,
        commit_channel_attention_preference, commit_workspace_attention_preference,
    },
    foundation::identity::{ConversationId, UserId, WorkspaceId},
};
use std::sync::Arc;

use crate::foundation::wire_admission::ValidatedJson;

pub const CHANNEL_PATH: &str = "/api/conversations/{id}/attention-preference";
pub const WORKSPACE_PATH: &str = "/api/workspaces/{id}/attention-preference";

/// `PUT` channel body — the reference `z.object({ mode: z.enum([...]) })` (NOT `.strict()`,
/// so an unknown key is STRIPPED, not refused). Serde owns the shape and enum: an unknown
/// value, wrong type, or missing key is a native decode fault; the shared [`ValidatedJson`]
/// object-only floor rejects an array before it can bind positionally. There is no
/// stateless rule, so every garde field is skipped. The wire enum is a thin server-side
/// mirror of the core `ChannelAttentionMode` (core stays wire-free).
#[derive(Debug, Deserialize, Validate)]
struct ChannelPreferenceBody {
    #[garde(skip)]
    mode: WireChannelAttentionMode,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WireChannelAttentionMode {
    All,
    Mentions,
    Mute,
}

impl From<WireChannelAttentionMode> for ChannelAttentionMode {
    fn from(value: WireChannelAttentionMode) -> Self {
        match value {
            WireChannelAttentionMode::All => ChannelAttentionMode::All,
            WireChannelAttentionMode::Mentions => ChannelAttentionMode::Mentions,
            WireChannelAttentionMode::Mute => ChannelAttentionMode::Mute,
        }
    }
}

/// `PUT` workspace body — the reference `z.object({ muted: z.boolean() })` (NOT `.strict()`,
/// unknown keys stripped). No stateless rule.
#[derive(Debug, Deserialize, Validate)]
struct WorkspacePreferenceBody {
    #[garde(skip)]
    muted: bool,
}

struct Inner<P, A> {
    port: P,
    aftermath: A,
}
struct AttentionState<P, A>(Arc<Inner<P, A>>);
impl<P, A> Clone for AttentionState<P, A> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}
pub fn router<P, A>(port: P, aftermath: A) -> Router
where
    P: AttentionPreferencePort + Send + Sync + 'static,
    A: AttentionPreferenceAftermath + Send + Sync + 'static,
{
    let state = AttentionState(Arc::new(Inner { port, aftermath }));
    Router::new()
        .route(CHANNEL_PATH, put(set_channel::<P, A>))
        .route(
            WORKSPACE_PATH,
            get(get_workspace::<P, A>).put(set_workspace::<P, A>),
        )
        .with_state(state)
}

/// Mutation-only registrar used by convergence mounts that own the two reviewed
/// write faces but not the pre-existing workspace preference read. Keeping this
/// split explicit prevents merging a multi-method leaf router from silently
/// widening the mounted method+path set.
pub fn write_router<P, A>(port: P, aftermath: A) -> Router
where
    P: AttentionPreferencePort + Send + Sync + 'static,
    A: AttentionPreferenceAftermath + Send + Sync + 'static,
{
    let state = AttentionState(Arc::new(Inner { port, aftermath }));
    Router::new()
        .route(CHANNEL_PATH, put(set_channel::<P, A>))
        .route(WORKSPACE_PATH, put(set_workspace::<P, A>))
        .with_state(state)
}

async fn set_channel<P: AttentionPreferencePort, A: AttentionPreferenceAftermath>(
    State(state): State<AttentionState<P, A>>,
    principal: Option<Extension<UserId>>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let Some(Extension(viewer)) = principal else {
        return internal();
    };
    let conversation = match ConversationId::parse(id) {
        Ok(v) => v,
        Err(_) => return error(StatusCode::BAD_REQUEST, "channel id required"),
    };
    // Target/access is resolved before the body is polled. The body therefore
    // cannot be retained across a SQLite writer reservation.
    let target = match state.0.port.admit_channel(&viewer, &conversation).await {
        Ok(Ok(v)) => v,
        Ok(Err(r)) => return channel_admission_refusal(r),
        Err(e) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&e))
                ),
                "channel attention admission failed"
            );
            return internal();
        }
    };
    // Authority-first: access is resolved before the body is polled, so the body is never
    // held across a SQLite writer reservation. The shared `ValidatedJson` extractor runs
    // deferred (after admit): structural faults keep native 415/400/422, no leaf parser.
    let value = match ValidatedJson::<ChannelPreferenceBody>::from_request(request, &()).await {
        Ok(ValidatedJson(body)) => body,
        Err(rejection) => return rejection.into_response(),
    };
    let mode = ChannelAttentionMode::from(value.mode);
    match commit_channel_attention_preference(&state.0.port, &state.0.aftermath, target, mode).await
    {
        Ok(Ok(done)) => {
            report_effect(done.first_failure());
            channel_success(done.committed())
        }
        Ok(Err(r)) => channel_write_refusal(r),
        Err(e) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&e))
                ),
                "channel attention write failed"
            );
            internal()
        }
    }
}

async fn get_workspace<P: AttentionPreferencePort, A: AttentionPreferenceAftermath>(
    State(state): State<AttentionState<P, A>>,
    principal: Option<Extension<UserId>>,
    Path(id): Path<String>,
) -> Response {
    let Some(Extension(viewer)) = principal else {
        return internal();
    };
    let workspace = match WorkspaceId::parse(id) {
        Ok(v) => v,
        Err(_) => return error(StatusCode::BAD_REQUEST, "workspace id required"),
    };
    match state.0.port.get_workspace(&viewer, workspace).await {
        Ok(Ok(v)) => {
            Json(json!({"workspace_id":v.workspace().as_str(),"muted":v.muted()})).into_response()
        }
        Ok(Err(r)) => workspace_read_refusal(r),
        Err(e) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&e))
                ),
                "workspace attention read failed"
            );
            internal()
        }
    }
}

async fn set_workspace<P: AttentionPreferencePort, A: AttentionPreferenceAftermath>(
    State(state): State<AttentionState<P, A>>,
    principal: Option<Extension<UserId>>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let Some(Extension(viewer)) = principal else {
        return internal();
    };
    let workspace = match WorkspaceId::parse(id) {
        Ok(v) => v,
        Err(_) => return error(StatusCode::BAD_REQUEST, "workspace id required"),
    };
    let target = match state
        .0
        .port
        .admit_workspace_write(&viewer, &workspace)
        .await
    {
        Ok(Ok(v)) => v,
        Ok(Err(r)) => return workspace_admission_refusal(r),
        Err(e) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&e))
                ),
                "workspace attention admission failed"
            );
            return internal();
        }
    };
    let muted = match ValidatedJson::<WorkspacePreferenceBody>::from_request(request, &()).await {
        Ok(ValidatedJson(body)) => body.muted,
        Err(rejection) => return rejection.into_response(),
    };
    match commit_workspace_attention_preference(&state.0.port, &state.0.aftermath, target, muted)
        .await
    {
        Ok(Ok(done)) => {
            report_effect(done.first_failure());
            workspace_success(done.committed())
        }
        Ok(Err(r)) => workspace_write_refusal(r),
        Err(e) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&e))
                ),
                "workspace attention write failed"
            );
            internal()
        }
    }
}
fn internal() -> Response {
    error(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
}
fn channel_admission_refusal(value: ChannelAttentionAdmissionRefusal) -> Response {
    match value {
        ChannelAttentionAdmissionRefusal::ChannelNotFound => {
            error(StatusCode::NOT_FOUND, "channel not found")
        }
        ChannelAttentionAdmissionRefusal::Forbidden => error(StatusCode::FORBIDDEN, "forbidden"),
    }
}
fn channel_write_refusal(value: ChannelAttentionWriteRefusal) -> Response {
    match value {
        ChannelAttentionWriteRefusal::ChannelNotFound => {
            error(StatusCode::NOT_FOUND, "channel not found")
        }
        ChannelAttentionWriteRefusal::Forbidden => error(StatusCode::FORBIDDEN, "forbidden"),
    }
}
fn workspace_read_refusal(value: WorkspaceAttentionReadRefusal) -> Response {
    match value {
        WorkspaceAttentionReadRefusal::Forbidden => error(StatusCode::FORBIDDEN, "forbidden"),
        WorkspaceAttentionReadRefusal::WorkspaceNotFound => {
            error(StatusCode::NOT_FOUND, "workspace not found")
        }
    }
}
fn workspace_admission_refusal(value: WorkspaceAttentionAdmissionRefusal) -> Response {
    match value {
        WorkspaceAttentionAdmissionRefusal::Forbidden => error(StatusCode::FORBIDDEN, "forbidden"),
        WorkspaceAttentionAdmissionRefusal::WorkspaceNotFound => {
            error(StatusCode::NOT_FOUND, "workspace not found")
        }
    }
}
fn workspace_write_refusal(value: WorkspaceAttentionWriteRefusal) -> Response {
    match value {
        WorkspaceAttentionWriteRefusal::Forbidden => error(StatusCode::FORBIDDEN, "forbidden"),
        WorkspaceAttentionWriteRefusal::WorkspaceNotFound => {
            error(StatusCode::NOT_FOUND, "workspace not found")
        }
    }
}
fn channel_success(value: &impl ChannelAttentionCommit) -> Response {
    Json(json!({"conversation_id":value.conversation().as_str(),"mode":value.mode().as_str()}))
        .into_response()
}
fn workspace_success(value: &impl WorkspaceAttentionCommit) -> Response {
    Json(json!({"workspace_id":value.workspace().as_str(),"muted":value.muted()})).into_response()
}
fn report_effect(
    error:Option<&silverhand_backend_core::conversation_messaging::messaging::collaboration::attention_preference::AttentionPreferenceEffectFailure>,
) {
    if let Some(error) = error {
        tracing::error!(
            error_chain = crate::foundation::safe_log_field::safe_display(
                &(crate::foundation::error::describe(error))
            ),
            "attention preference effect failed"
        );
    }
}
