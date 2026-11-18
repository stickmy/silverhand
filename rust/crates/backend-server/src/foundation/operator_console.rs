//! Token-gated operator metrics and bounded user activity.

mod users;

use crate::{
    application_orchestration::daemon_delivery::OUTBOUND_PROTOCOL_VERSION,
    foundation::scope_event_transport::{DESKTOP_SERVER_API_EPOCH, MINIMUM_CLIENT_API_EPOCH},
};
use axum::{
    Json, Router,
    extract::{Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use sha2::{Digest, Sha256};
use silverhand_backend_core::application_orchestration::console_analytics::{
    ConsoleAnalyticsRead, ConsoleAnalyticsSnapshot, TIME_ZONE,
};
use std::{
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Mutex, Semaphore};

const CACHE_TTL: Duration = Duration::from_secs(30);
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
struct ConsoleState<R> {
    token_hash: [u8; 32],
    read: R,
    html: &'static str,
    cache: Arc<Mutex<Option<CachedSnapshot>>>,
    user_queries: Arc<Semaphore>,
}

struct CachedSnapshot {
    loaded_at: Instant,
    value: serde_json::Value,
}

pub fn router<R>(read: R, token_hash: [u8; 32], html: &'static str) -> Router
where
    R: ConsoleAnalyticsRead,
{
    let state = ConsoleState {
        token_hash,
        read,
        html,
        cache: Arc::new(Mutex::new(None)),
        user_queries: Arc::new(Semaphore::new(1)),
    };
    let api = Router::new()
        .route("/console/api/overview", get(overview::<R>))
        .route("/console/api/users", get(users::list::<R>))
        .route("/console/api/users/{user_id}", get(users::detail::<R>))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            authorize::<R>,
        ));

    Router::new()
        .route("/console", get(index::<R>))
        .merge(api)
        .with_state(state)
}

async fn authorize<R>(
    State(state): State<ConsoleState<R>>,
    request: Request,
    next: Next,
) -> Response
where
    R: ConsoleAnalyticsRead,
{
    let offered = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(|value| Sha256::digest(value.as_bytes()));
    let accepted = offered.is_some_and(|offered| constant_time_equal(&offered, &state.token_hash));
    metrics::counter!("silverhand_console_auth_requests_total", "result" => if accepted { "accepted" } else { "rejected" }).increment(1);
    if accepted {
        let mut response = next.run(request).await;
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-store, private"),
        );
        return response;
    }

    // A fixed audit event deliberately omits token, headers, URI/query and user ID.
    tracing::warn!(target: "operator_console_access", authorized = false, "console authorization rejected");
    let mut response = StatusCode::UNAUTHORIZED.into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, private"),
    );
    response.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Bearer realm=\"silverhand-console\""),
    );
    response
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }

    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

async fn overview<R>(State(state): State<ConsoleState<R>>) -> Response
where
    R: ConsoleAnalyticsRead,
{
    let mut cache = state.cache.lock().await;
    if let Some(cached) = cache.as_ref()
        && cached.loaded_at.elapsed() < CACHE_TTL
    {
        return json_response(cached.value.clone());
    }

    let now_ms = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_millis()).ok(),
        Err(_) => None,
    };
    let Some(now_ms) = now_ms else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let snapshot = match tokio::time::timeout(QUERY_TIMEOUT, state.read.read_snapshot(now_ms)).await
    {
        Ok(Ok(snapshot)) => snapshot,
        Ok(Err(error)) => {
            tracing::error!(
                error = crate::foundation::safe_log_field::safe_display(&error),
                "operator console analytics query failed"
            );
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        Err(_) => {
            tracing::warn!("operator console analytics query timed out");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };
    let value = snapshot_json(snapshot);
    *cache = Some(CachedSnapshot {
        loaded_at: Instant::now(),
        value: value.clone(),
    });

    json_response(value)
}

fn json_response(value: serde_json::Value) -> Response {
    let mut response = Json(value).into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, private"),
    );
    response
}

fn snapshot_json(snapshot: ConsoleAnalyticsSnapshot) -> serde_json::Value {
    let totals = snapshot.totals;
    serde_json::json!({
        "generated_at": snapshot.generated_at_ms,
        "time_zone": TIME_ZONE,
        "cache_ttl_seconds": CACHE_TTL.as_secs(),
        "totals": {
            "users": totals.users,
            "agents": totals.agents,
            "regular_workspaces": totals.regular_workspaces,
            "active_channels": totals.active_channels,
            "archived_channels": totals.archived_channels,
            "messages": totals.messages,
            "active_computers": totals.active_computers,
        },
        "activity_windows": snapshot.windows.into_iter().map(|window| serde_json::json!({
            "days": window.days,
            "authenticated_users": window.authenticated_users,
            "speaking_users": window.speaking_users,
            "human_messages": window.human_messages,
            "agent_messages": window.agent_messages,
        })).collect::<Vec<_>>(),
        "daily_activity": snapshot.daily_activity.into_iter().map(|day| serde_json::json!({
            "date": day.date,
            "speaking_users": day.speaking_users,
            "human_messages": day.human_messages,
            "agent_messages": day.agent_messages,
        })).collect::<Vec<_>>(),
        "agent_runtimes": snapshot.agent_runtimes.into_iter().map(|runtime| serde_json::json!({
            "runtime": runtime.category,
            "count": runtime.count,
        })).collect::<Vec<_>>(),
        "server_contract": {
            "current_desktop_api_epoch": DESKTOP_SERVER_API_EPOCH,
            "minimum_client_api_epoch": MINIMUM_CLIENT_API_EPOCH,
            "outbound_protocol_version": OUTBOUND_PROTOCOL_VERSION,
        }
    })
}

async fn index<R>(State(state): State<ConsoleState<R>>) -> Response
where
    R: ConsoleAnalyticsRead,
{
    let mut response = Html(state.html).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, private"),
    );
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("x-frame-options", HeaderValue::from_static("DENY"));
    headers.insert(
        "content-security-policy",
        HeaderValue::from_static(
            "default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'",
        ),
    );

    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use silverhand_backend_core::{
        application_orchestration::console_analytics::{
            ActivityWindow, CategoryCount, ConsoleTotals, DailyActivity,
        },
        foundation::persistence::PersistenceFailure,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;

    #[derive(Clone)]
    struct FakeRead(Arc<AtomicUsize>);

    impl ConsoleAnalyticsRead for FakeRead {
        async fn read_users(&self, _query: silverhand_backend_core::application_orchestration::console_analytics::ConsoleUsersQuery, now_ms: i64) -> Result<silverhand_backend_core::application_orchestration::console_analytics::ConsoleUsersSnapshot, PersistenceFailure>{
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(silverhand_backend_core::application_orchestration::console_analytics::ConsoleUsersSnapshot {
                generated_at_ms: now_ms, total_matching: 0, users: vec![], top_users: vec![],
            })
        }

        async fn read_user(&self, _user_id: String, _days: i64, _now_ms: i64) -> Result<Option<silverhand_backend_core::application_orchestration::console_analytics::ConsoleUserDetail>, PersistenceFailure>{
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(None)
        }

        async fn read_snapshot(
            &self,
            now_ms: i64,
        ) -> Result<ConsoleAnalyticsSnapshot, PersistenceFailure> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(ConsoleAnalyticsSnapshot {
                generated_at_ms: now_ms,
                totals: ConsoleTotals {
                    users: 2,
                    agents: 3,
                    regular_workspaces: 1,
                    active_channels: 4,
                    archived_channels: 0,
                    messages: 7,
                    active_computers: 2,
                },
                windows: vec![ActivityWindow {
                    days: 7,
                    authenticated_users: 2,
                    speaking_users: 1,
                    human_messages: 5,
                    agent_messages: 2,
                }],
                daily_activity: vec![DailyActivity {
                    date: "2026-09-09".to_owned(),
                    speaking_users: 1,
                    human_messages: 5,
                    agent_messages: 2,
                }],
                agent_runtimes: vec![CategoryCount {
                    category: "pi".to_owned(),
                    count: 3,
                }],
            })
        }
    }

    #[tokio::test]
    async fn overview_requires_the_exact_token_and_reuses_the_cached_snapshot() {
        let token = "01234567890123456789012345678901";
        let hash = Sha256::digest(token.as_bytes()).into();
        let calls = Arc::new(AtomicUsize::new(0));
        let app = router(FakeRead(calls.clone()), hash, "<html>console</html>");

        for offered in [
            None,
            Some(""),
            Some("Bearer wrong-token"),
            Some("Bearer 01234567890123456789012345678900"),
            Some("Basic 01234567890123456789012345678901"),
        ] {
            let mut request = Request::builder().uri("/console/api/overview");
            if let Some(value) = offered {
                request = request.header(header::AUTHORIZATION, value);
            }
            let unauthorized = app
                .clone()
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
            assert_eq!(
                unauthorized.headers()[header::CACHE_CONTROL],
                "no-store, private"
            );
            assert_eq!(
                unauthorized.headers()[header::WWW_AUTHENTICATE],
                "Bearer realm=\"silverhand-console\""
            );
            assert_eq!(
                calls.load(Ordering::SeqCst),
                0,
                "denied requests must not read analytics"
            );
        }

        for _ in 0..2 {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/console/api/overview")
                        .header(header::AUTHORIZATION, format!("Bearer {token}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body = axum::body::to_bytes(response.into_body(), 1_000_000)
                .await
                .unwrap();
            let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(value["totals"]["messages"], 7);
            assert_eq!(value["agent_runtimes"][0]["runtime"], "pi");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn index_exposes_no_data_and_sets_browser_isolation_headers() {
        let calls = Arc::new(AtomicUsize::new(0));
        let app = router(FakeRead(calls.clone()), [7; 32], "<html>console</html>");
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/console")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["x-frame-options"], "DENY");
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "no-store, private"
        );
        let policy = response.headers()["content-security-policy"]
            .to_str()
            .unwrap();
        for directive in [
            "default-src 'none'",
            "connect-src 'self'",
            "frame-ancestors 'none'",
            "base-uri 'none'",
        ] {
            assert!(
                policy
                    .split(';')
                    .map(str::trim)
                    .any(|value| value == directive)
            );
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "the public shell must not query private analytics"
        );
    }
    #[tokio::test]
    async fn user_routes_are_authorized_validated_and_never_cache_identity_responses() {
        let token = "01234567890123456789012345678901";
        let calls = Arc::new(AtomicUsize::new(0));
        let app = router(
            FakeRead(calls.clone()),
            Sha256::digest(token.as_bytes()).into(),
            "console",
        );
        for path in ["/console/api/users", "/console/api/users/u1?days=7"] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        for path in [
            "/console/api/users?days=365",
            "/console/api/users?page=0",
            "/console/api/users?page=10001",
            "/console/api/users/u1?days=1",
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .header(header::AUTHORIZATION, format!("Bearer {token}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{path}");
            assert_eq!(
                response.headers()[header::CACHE_CONTROL],
                "no-store, private"
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        for path in [
            "/console/api/users?days=7&page=1&q=someone",
            "/console/api/users?days=30",
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .header(header::AUTHORIZATION, format!("Bearer {token}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.headers()[header::CACHE_CONTROL],
                "no-store, private"
            );
            let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
                .await
                .unwrap();
            let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(value["page_size"], 25);
            assert_eq!(value["total_matching"], 0);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let missing = app
            .oneshot(
                Request::builder()
                    .uri("/console/api/users/missing")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    }
}
