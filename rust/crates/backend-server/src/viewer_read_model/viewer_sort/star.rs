//! `POST`/`DELETE /api/channels/{id}/star` — toggle THIS viewer's star on a
//! conversation.
//!
//! Both methods share one handler parameterized by the target star state. The route
//! is thin over the ONE transactional [`ViewerSortMutationWriter`]: it resolves the
//! conversation's home list (`channels:<ws>`, or `direct_pairs` for a direct pair)
//! and hands a `SetStarred` mutation to the writer, which — UNDER its own write
//! lock — re-resolves list existence (404) / visibility (403) / live membership
//! (404; for a direct pair: the viewer is its human endpoint), toggles the star
//! (and, on unstar, drops the favorites-rank orphan), and returns the committed
//! home-list post-image.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Extension, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use serde::Serialize;
use silverhand_backend_core::{
    foundation::identity::UserId,
    viewer_read_model::viewer_sort_state::{
        ChannelHomeReadPort, ChannelHomeVerdict, SortListKey, SortMutation, SortMutationRefusal,
        ViewerSortMutationWriter,
    },
};

use super::wire::ViewerSortStateDto;
use crate::foundation::http_error::logged_500 as internal;

/// The shared mounted path for both star methods.
pub const CHANNEL_STAR_PATH: &str = "/api/channels/{id}/star";

/// The two authorities the star routes bind: `homes` reads the conversation's home
/// (a missing id is `404 channel not found`, distinct from the list-not-found the
/// writer decides);
/// `writer` is the ONE transactional sort mutation authority. In production both are
/// shared-pool handles over the one messaging store.
pub struct ChannelStarState<H, W> {
    pub homes: H,
    pub writer: W,
}

struct Shared<H, W>(Arc<ChannelStarState<H, W>>);
impl<H, W> Clone for Shared<H, W> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

pub fn star_router<H, W>(state: ChannelStarState<H, W>) -> Router
where
    H: ChannelHomeReadPort + Send + Sync + 'static,
    W: ViewerSortMutationWriter + Send + Sync + 'static,
{
    Router::new()
        .route(CHANNEL_STAR_PATH, post(star::<H, W>).delete(unstar::<H, W>))
        .with_state(Shared(Arc::new(state)))
}

async fn star<H, W>(
    State(state): State<Shared<H, W>>,
    principal: Option<Extension<UserId>>,
    Path(id): Path<String>,
) -> Response
where
    H: ChannelHomeReadPort + Send + Sync + 'static,
    W: ViewerSortMutationWriter + Send + Sync + 'static,
{
    toggle(state, principal, id, true).await
}

async fn unstar<H, W>(
    State(state): State<Shared<H, W>>,
    principal: Option<Extension<UserId>>,
    Path(id): Path<String>,
) -> Response
where
    H: ChannelHomeReadPort + Send + Sync + 'static,
    W: ViewerSortMutationWriter + Send + Sync + 'static,
{
    toggle(state, principal, id, false).await
}

async fn toggle<H, W>(
    state: Shared<H, W>,
    principal: Option<Extension<UserId>>,
    id: String,
    starred: bool,
) -> Response
where
    H: ChannelHomeReadPort + Send + Sync + 'static,
    W: ViewerSortMutationWriter + Send + Sync + 'static,
{
    let Some(Extension(viewer)) = principal else {
        return registration_required();
    };
    if id.is_empty() {
        return error(StatusCode::BAD_REQUEST, "channel id required");
    }
    let state = &state.0;

    // Existence + home list (a missing id is `404 channel not found`). This read is a
    // pre-writer hint; the writer re-resolves the list's authority under its lock.
    let list_key = match state.homes.channel_home(&id).await {
        Ok(Some(ChannelHomeVerdict::WorkspacePlaced(home))) => {
            SortListKey::Channels(home.workspace.as_str().to_owned())
        }
        Ok(Some(ChannelHomeVerdict::DirectPair)) => SortListKey::DirectPairs,
        Ok(None) => return error(StatusCode::NOT_FOUND, "channel not found"),
        Err(cause) => return internal(cause, "channel-star home read failed"),
    };

    match state
        .writer
        .apply_sort_mutation(
            &viewer,
            &list_key,
            &SortMutation::SetStarred {
                entity_id: id,
                starred,
            },
        )
        .await
    {
        Ok(Ok(projection)) => (
            StatusCode::OK,
            Json(ViewerSortResponse {
                viewer_sort: ViewerSortStateDto::from(projection),
            }),
        )
            .into_response(),
        // The star routes render list-not-found / unknown-entity as `404 not found`
        // (the reference's `not found` after `getChannelLike` passed) and forbidden
        // as `403 forbidden`.
        Ok(Err(SortMutationRefusal::ListNotFound))
        | Ok(Err(SortMutationRefusal::UnknownEntity)) => error(StatusCode::NOT_FOUND, "not found"),
        Ok(Err(SortMutationRefusal::Forbidden)) => error(StatusCode::FORBIDDEN, "forbidden"),
        // No other refusal is reachable for a `SetStarred`.
        Ok(Err(_)) => internal_unreachable(),
        Err(cause) => internal(cause, "channel-star write failed"),
    }
}

#[derive(Serialize)]
struct ViewerSortResponse {
    viewer_sort: ViewerSortStateDto,
}

#[derive(Serialize)]
struct ErrorBody {
    error: &'static str,
}

fn error(status: StatusCode, message: &'static str) -> Response {
    (status, Json(ErrorBody { error: message })).into_response()
}

fn registration_required() -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(serde_json::json!({
            "error": "viewer has not authenticated yet",
            "code": "registration_required"
        })),
    )
        .into_response()
}

/// A `SetStarred` mutation can only ever yield list-not-found / forbidden /
/// unknown-entity; any other refusal is a construction bug, surfaced as a safe 500
/// rather than a wrong product answer.
fn internal_unreachable() -> Response {
    tracing::error!("channel-star writer returned an unreachable refusal");
    (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
}
