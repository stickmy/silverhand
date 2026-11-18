use super::{ConsoleState, QUERY_TIMEOUT, json_response};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use silverhand_backend_core::application_orchestration::console_analytics::{
    ConsoleAnalyticsRead, ConsoleUserActivity, ConsoleUsersQuery, TIME_ZONE, USER_PAGE_SIZE,
};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Deserialize)]
pub(super) struct ListQuery {
    #[serde(default = "default_days")]
    days: i64,
    #[serde(default)]
    q: String,
    #[serde(default = "default_page")]
    page: i64,
}

#[derive(Deserialize)]
pub(super) struct DetailQuery {
    #[serde(default = "default_days")]
    days: i64,
}

fn default_days() -> i64 {
    7
}
fn default_page() -> i64 {
    1
}

fn now_ms() -> Option<i64> {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_millis(),
    )
    .ok()
}

pub(super) async fn list<R: ConsoleAnalyticsRead>(
    State(state): State<ConsoleState<R>>,
    Query(query): Query<ListQuery>,
) -> Response {
    let query = ConsoleUsersQuery {
        days: query.days,
        search: query.q.trim().to_owned(),
        page: query.page,
    };
    if !query.is_valid() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Ok(_permit) = state.user_queries.try_acquire() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let Some(now_ms) = now_ms() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let result =
        tokio::time::timeout(QUERY_TIMEOUT, state.read.read_users(query.clone(), now_ms)).await;
    match result {
        Ok(Ok(snapshot)) => json_response(serde_json::json!({
            "generated_at": snapshot.generated_at_ms, "time_zone": TIME_ZONE,
            "days": query.days, "page": query.page, "page_size": USER_PAGE_SIZE,
            "total_matching": snapshot.total_matching,
            "users": snapshot.users.into_iter().map(user_json).collect::<Vec<_>>(),
            "top_users": snapshot.top_users.into_iter().map(user_json).collect::<Vec<_>>(),
        })),
        Ok(Err(_)) => {
            tracing::error!("operator console user list query failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

pub(super) async fn detail<R: ConsoleAnalyticsRead>(
    State(state): State<ConsoleState<R>>,
    Path(user_id): Path<String>,
    Query(query): Query<DetailQuery>,
) -> Response {
    if !matches!(query.days, 7 | 30)
        || user_id.is_empty()
        || user_id.len() > 128
        || user_id.chars().any(char::is_control)
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Ok(_permit) = state.user_queries.try_acquire() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let Some(now_ms) = now_ms() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let result = tokio::time::timeout(
        QUERY_TIMEOUT,
        state.read.read_user(user_id, query.days, now_ms),
    )
    .await;
    match result {
        Ok(Ok(Some(snapshot))) => json_response(serde_json::json!({
            "generated_at": snapshot.generated_at_ms, "time_zone": TIME_ZONE, "days": query.days,
            "user": user_json(snapshot.user),
            "daily_activity": snapshot.daily_activity.into_iter().map(|day| serde_json::json!({
                "date": day.date, "message_count": day.message_count,
            })).collect::<Vec<_>>(),
        })),
        Ok(Ok(None)) => StatusCode::NOT_FOUND.into_response(),
        Ok(Err(_)) => {
            tracing::error!("operator console user detail query failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

fn user_json(user: ConsoleUserActivity) -> serde_json::Value {
    serde_json::json!({
        "user_id": user.user_id, "name": user.name, "display_name": user.display_name,
        "message_count": user.message_count, "active_days": user.active_days,
        "conversation_count": user.conversation_count, "last_message_at": user.last_message_at,
        "last_authenticated_at": user.last_authenticated_at,
    })
}
