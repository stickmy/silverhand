//! The touch / mode / order sort-mutation routes.
//!
//! Each route is thin over the ONE transactional [`ViewerSortMutationWriter`]: it
//! admits the body, builds the domain [`SortMutation`], and hands it to the writer,
//! which re-resolves the list's live existence / visibility / membership / group /
//! star-exclusion UNDER its own write lock, decides, writes, and returns the
//! committed `viewer_sort` post-image — all in one connection-bound transaction. The
//! route owns only body admission and the refusal→status mapping; it holds NO
//! pre-lock authority witness.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Extension, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use serde::Serialize;
use silverhand_backend_core::{
    foundation::identity::UserId,
    viewer_read_model::viewer_sort_state::{
        SortGroup, SortListKey, SortMode, SortMutation, SortMutationRefusal, ViewerSidebarRead,
        ViewerSortMutationWriter,
    },
};

use crate::foundation::wire_admission::{ValidatedJson, ValidatedJsonRejection};
use crate::foundation::wire_patch::RequiredPatchField;

use super::wire::ViewerSortStateDto;
use crate::foundation::http_error::{json_error as error, logged_500 as internal};

pub const SORT_TOUCH_PATH: &str = "/api/sort/touch";
pub const SORT_MODE_PATH: &str = "/api/sort/mode";
pub const SORT_ORDER_PATH: &str = "/api/sort/order";

/// The one dependency the three mutation routes share: the transactional writer.
pub struct SortMutationState<W> {
    pub writer: W,
    pub hub: crate::foundation::scope_event_transport::ScopeEventHub,
}

struct Shared<W>(Arc<SortMutationState<W>>);
impl<W> Clone for Shared<W> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

pub fn sort_mutation_router<W>(state: SortMutationState<W>) -> Router
where
    W: ViewerSortMutationWriter + ViewerSidebarRead + Send + Sync + 'static,
{
    Router::new()
        .route("/api/sort/sidebar", get(sidebar::<W>))
        .route(SORT_TOUCH_PATH, post(touch::<W>))
        .route(SORT_MODE_PATH, put(set_mode::<W>))
        .route(SORT_ORDER_PATH, put(reorder::<W>))
        .with_state(Shared(Arc::new(state)))
}

// ── POST /api/sort/touch ────────────────────────────────────────────────────

/// `SortTouchInputSchema`: `{ list_key, entity_id }`. Both are bare `z.string()`
/// in TS — a present non-string is a structural rejection, but an EMPTY string is
/// admitted and reaches the product refusal (`unknown entity_id`), so `entity_id`
/// is NOT length-narrowed here.
#[derive(serde::Deserialize, garde::Validate)]
struct TouchBody {
    #[garde(skip)]
    list_key: String,
    #[garde(skip)]
    entity_id: String,
}

async fn touch<W>(
    State(state): State<Shared<W>>,
    principal: Option<Extension<UserId>>,
    body: Result<ValidatedJson<TouchBody>, ValidatedJsonRejection>,
) -> Response
where
    W: ViewerSortMutationWriter + ViewerSidebarRead + Send + Sync + 'static,
{
    let Some(Extension(viewer)) = principal else {
        return registration_required();
    };
    let ValidatedJson(body) = match body {
        Ok(body) => body,
        Err(rejection) => return rejection.into_response(),
    };
    // `list_key` grammar: `workspaces` | `favorites` | `channels:*`. Only a string
    // outside the prefix grammar is invalid input; `channels:` (empty ws) is admitted
    // and becomes a list 404 at the writer, matching TS.
    let Some(list_key) = SortListKey::parse(&body.list_key) else {
        return invalid_input();
    };
    // Verb-specific ORDER hint: the reference refuses touch-favorites
    // BEFORE `resolveListGroups`, so a malformed/failed favorites projection can't
    // turn the fixed 400 into a 500. The writer keeps the defensive `FavoritesNotTouchable`.
    if matches!(list_key, SortListKey::Favorites) {
        return error(
            StatusCode::BAD_REQUEST,
            "favorites does not support touch; touch the home entity",
        );
    }
    apply(
        &state,
        &viewer,
        &list_key,
        SortMutation::Touch {
            entity_id: body.entity_id,
        },
        Echo::Touch,
    )
    .await
}

// ── PUT /api/sort/mode ──────────────────────────────────────────────────────

/// `SetSortModeInputSchema`: `{ list_key, group?, mode }`.
#[derive(serde::Deserialize, garde::Validate)]
struct SetModeBody {
    #[garde(skip)]
    list_key: String,
    #[garde(skip)]
    #[serde(default)]
    group: RequiredPatchField<String>,
    #[garde(skip)]
    mode: String,
}

async fn set_mode<W>(
    State(state): State<Shared<W>>,
    principal: Option<Extension<UserId>>,
    body: Result<ValidatedJson<SetModeBody>, ValidatedJsonRejection>,
) -> Response
where
    W: ViewerSortMutationWriter + ViewerSidebarRead + Send + Sync + 'static,
{
    let Some(Extension(viewer)) = principal else {
        return registration_required();
    };
    let ValidatedJson(body) = match body {
        Ok(body) => body,
        Err(rejection) => return rejection.into_response(),
    };
    let Some(list_key) = SortListKey::parse(&body.list_key) else {
        return invalid_input();
    };
    let Some(mode) = SortMode::parse(&body.mode) else {
        return invalid_input();
    };
    let group = match body.group.into_option() {
        Some(raw) => match SortGroup::parse(&raw) {
            Some(group) => Some(group),
            None => return invalid_input(),
        },
        None => None,
    };
    apply(
        &state,
        &viewer,
        &list_key,
        SortMutation::SetMode { group, mode },
        Echo::State,
    )
    .await
}

// ── PUT /api/sort/order ─────────────────────────────────────────────────────

/// `SortReorderInputSchema`: `{ list_key, group, ordered_ids }`, group required.
#[derive(serde::Deserialize, garde::Validate)]
struct ReorderBody {
    #[garde(skip)]
    list_key: String,
    #[garde(skip)]
    group: String,
    #[garde(skip)]
    ordered_ids: Vec<String>,
}

async fn reorder<W>(
    State(state): State<Shared<W>>,
    principal: Option<Extension<UserId>>,
    body: Result<ValidatedJson<ReorderBody>, ValidatedJsonRejection>,
) -> Response
where
    W: ViewerSortMutationWriter + ViewerSidebarRead + Send + Sync + 'static,
{
    let Some(Extension(viewer)) = principal else {
        return registration_required();
    };
    let ValidatedJson(body) = match body {
        Ok(body) => body,
        Err(rejection) => return rejection.into_response(),
    };
    let Some(list_key) = SortListKey::parse(&body.list_key) else {
        return invalid_input();
    };
    let Some(group) = SortGroup::parse(&body.group) else {
        return invalid_input();
    };
    // Verb-specific ORDER hint: the reference checks
    // `legalGroups(listKey).includes(group)` BEFORE `resolveListGroups`, so an illegal
    // group wins over a missing/forbidden list. The writer still re-validates the group
    // in-lock. (set-mode's TS order is the opposite — it resolves the list first — so
    // this pre-check is REORDER-specific, not moved into the shared writer.)
    if !list_key.legal_groups().contains(&group) {
        return group_refusal("group is not valid for this list", Some(group));
    }
    apply(
        &state,
        &viewer,
        &list_key,
        SortMutation::Reorder {
            group,
            ordered_ids: body.ordered_ids,
        },
        Echo::State,
    )
    .await
}

// ── shared apply + refusal mapping ───────────────────────────────────────────

/// Which success body a route echoes: touch returns `{ ok: true }`; mode/order
/// return `{ viewer_sort }`.
enum Echo {
    Touch,
    State,
}

async fn apply<W>(
    state: &Shared<W>,
    viewer: &UserId,
    list_key: &SortListKey,
    mutation: SortMutation,
    echo: Echo,
) -> Response
where
    W: ViewerSortMutationWriter + ViewerSidebarRead + Send + Sync + 'static,
{
    match state
        .0
        .writer
        .apply_sort_mutation(viewer, list_key, &mutation)
        .await
    {
        Ok(Ok(projection)) => {
            if matches!(list_key, SortListKey::Sidebar) {
                use crate::foundation::scope_event_wire::ProductEvent;
                use silverhand_backend_core::foundation::scope_event::EventScope;
                if let Err(error) = state.0.hub.publish(
                    EventScope::Viewer(viewer.clone()),
                    ProductEvent::SidebarSortChanged {},
                ) {
                    tracing::warn!(%error, "sidebar sort invalidation failed after commit");
                }
            }
            match echo {
                Echo::Touch => (StatusCode::OK, Json(OkBody { ok: true })).into_response(),
                Echo::State => (
                    StatusCode::OK,
                    Json(ViewerSortResponse {
                        viewer_sort: ViewerSortStateDto::from(projection),
                    }),
                )
                    .into_response(),
            }
        }
        Ok(Err(refusal)) => refusal_response(refusal),
        Err(cause) => internal(cause, "sort mutation write failed"),
    }
}

fn refusal_response(refusal: SortMutationRefusal) -> Response {
    match refusal {
        SortMutationRefusal::ReorderOnly => {
            error(StatusCode::BAD_REQUEST, "sidebar only supports reorder")
        }
        SortMutationRefusal::SidebarSetChanged => error(
            StatusCode::CONFLICT,
            "sidebar set changed; refetch /api/sort/sidebar",
        ),
        SortMutationRefusal::ListNotFound => error(StatusCode::NOT_FOUND, "list not found"),
        SortMutationRefusal::Forbidden => error(StatusCode::FORBIDDEN, "forbidden"),
        SortMutationRefusal::FavoritesNotTouchable => error(
            StatusCode::BAD_REQUEST,
            "favorites does not support touch; touch the home entity",
        ),
        SortMutationRefusal::UnknownEntity => {
            error(StatusCode::BAD_REQUEST, "unknown entity_id for this list")
        }
        SortMutationRefusal::GroupNotSupportedForList { group } => group_refusal(
            "group is not supported for this list (list-wide mode)",
            group,
        ),
        SortMutationRefusal::GroupNotValidForList { group } => {
            group_refusal("group is not valid for this list", group)
        }
        SortMutationRefusal::IdNotInGroup { id } => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "ordered_ids contains an id not in the declared group",
                "id": id,
            })),
        )
            .into_response(),
        SortMutationRefusal::NotExactMembers { expected_count } => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "ordered_ids must be exactly the live members of the group",
                "expected_count": expected_count,
            })),
        )
            .into_response(),
    }
}

#[derive(Serialize)]
struct ViewerSortResponse {
    viewer_sort: ViewerSortStateDto,
}

#[derive(Serialize)]
struct OkBody {
    ok: bool,
}

fn invalid_input() -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(serde_json::json!({ "error": "invalid input" })),
    )
        .into_response()
}

/// A `group`-scoped 400 refusal. Mirrors the reference `c.json({ error, group }, 400)`
/// where `group` is a `SortGroup | undefined`: an `undefined` group is DROPPED by
/// `JSON.stringify`, so a missing group must OMIT the key entirely (not emit
/// `"group":null`, which would be a second spelling the TS wire never produces).
fn group_refusal(message: &'static str, group: Option<SortGroup>) -> Response {
    let mut body = serde_json::Map::new();
    body.insert(
        "error".to_owned(),
        serde_json::Value::String(message.to_owned()),
    );
    if let Some(group) = group {
        body.insert(
            "group".to_owned(),
            serde_json::Value::String(group.as_str().to_owned()),
        );
    }
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::Value::Object(body)),
    )
        .into_response()
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

async fn sidebar<W>(
    State(state): State<Shared<W>>,
    principal: Option<Extension<UserId>>,
) -> Response
where
    W: ViewerSortMutationWriter + ViewerSidebarRead + Send + Sync + 'static,
{
    let Some(Extension(viewer)) = principal else {
        return registration_required();
    };
    match state.0.writer.sidebar_sort(&viewer).await {
        Ok(projection) => Json(ViewerSortResponse {
            viewer_sort: projection.into(),
        })
        .into_response(),
        Err(cause) => internal(cause, "sidebar sort read failed"),
    }
}
