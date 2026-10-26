use crate::current_schema;
use crate::log_capture;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::{SinkExt, StreamExt};
use log_capture::LogBuffer;
use sha2::{Digest, Sha256};
use silverhand_backend_core::foundation::identity::UserId;
use silverhand_backend_core::identity_access::auth::{
    HumanSessionAuthenticator, SessionClock, SessionTtl,
};
use silverhand_backend_server::foundation::scope_event_transport::{
    AllowedOrigin, ConnectionLifecycle, DESKTOP_API_EPOCH_HEADER, EVENT_WEBSOCKET_PATH,
    EVENT_WS_SESSION_COOKIE, EventWebSocketConfig, ScopeEventHub, router,
};
use silverhand_backend_sqlite::{
    foundation::scope_event::SqliteEventScopeAuthority, identity_access::auth::SqliteSessionStore,
};
use sqlx::{Connection, SqliteConnection};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{
        Error as WebSocketError,
        client::IntoClientRequest,
        http::{
            HeaderValue, StatusCode,
            header::{AUTHORIZATION, COOKIE, ORIGIN},
        },
    },
};

const ORIGIN_VALUE: &str = "http://desktop.test";
const VALID: &str = "valid-token";
const REVOKED: &str = "revoked-token";
const EXPIRED: &str = "expired-token";

#[derive(Clone)]
struct FixedClock;

impl SessionClock for FixedClock {
    fn now_unix_millis(&self) -> i64 {
        10_000
    }
}

/// A do-nothing lifecycle for tests that exercise the WS transport alone and do
/// not assert on presence. Production wiring lives in `backend-app`; the crate's
/// only null implementation is test-scoped by design.
struct NullLifecycle;

impl ConnectionLifecycle for NullLifecycle {
    fn on_connect(&self, _viewer: &UserId, _connection_id: &str) {}
    fn on_disconnect(&self, _viewer: &UserId, _connection_id: &str) {}
}

async fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("scope-event-e2e.sqlite");
    let mut db = current_schema::create_current_schema(&path).await;
    sqlx::query(
        "INSERT INTO users (id, name, display_name, created_at, updated_at) VALUES ('user-1', 'user', 'User', 1, 1)",
    )
    .execute(&mut db)
    .await
    .unwrap();
    for (id, token, last_used, revoked_at) in [
        ("session-valid", VALID, 9_500, None),
        ("session-revoked", REVOKED, 9_500, Some(9_800)),
        ("session-expired", EXPIRED, 1, None),
    ] {
        sqlx::query(
            "INSERT INTO auth_sessions (id, user_id, token_hash, created_at, last_used_at, revoked_at, user_agent, ip_first_seen) VALUES (?, 'user-1', ?, 1, ?, ?, NULL, NULL)",
        )
        .bind(id)
        .bind(token_hash(token))
        .bind(last_used)
        .bind(revoked_at)
        .execute(&mut db)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO workspaces (id, name, name_key, owner_user_id, created_at, updated_at) VALUES ('workspace-1', 'Space', 'space', 'user-1', 1, 1)",
    )
    .execute(&mut db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO workspace_users (workspace_id, user_id, joined_at, role) VALUES ('workspace-1', 'user-1', 1, 'owner')",
    )
    .execute(&mut db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversations (id, workspace_id, placement_kind, roster_policy, title, title_key, status, visibility, created_at, updated_at) VALUES ('conversation-1', 'workspace-1', 'workspace', 'open', 'Channel', 'channel', 'open', 'private', 1, 1)",
    )
    .execute(&mut db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversation_users (conversation_id, user_id, joined_at) VALUES ('conversation-1', 'user-1', 1)",
    )
    .execute(&mut db)
    .await
    .unwrap();
    db.close().await.unwrap();
    (directory, path)
}

async fn running(
    path: &std::path::Path,
) -> (
    std::net::SocketAddr,
    silverhand_backend_server::foundation::scope_event_transport::EventWebSocketController,
    tokio::task::JoinHandle<()>,
) {
    let sessions = HumanSessionAuthenticator::new(
        SqliteSessionStore::open(path).await.unwrap(),
        FixedClock,
        SessionTtl::from_millis(1_000).unwrap(),
    );
    let scopes = SqliteEventScopeAuthority::open(path).await.unwrap();
    let config =
        EventWebSocketConfig::production([AllowedOrigin::parse(ORIGIN_VALUE).unwrap()]).unwrap();
    let (app, controller) = router(
        ScopeEventHub::production().unwrap(),
        sessions,
        scopes,
        std::sync::Arc::new(NullLifecycle),
        config,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (address, controller, server)
}

#[tokio::test]
async fn cookie_origin_session_and_epoch_are_all_required_in_order() {
    for (name, token, origin, epoch, authorization, expected) in [
        (
            "valid",
            Some(VALID),
            Some(ORIGIN_VALUE),
            Some("4"),
            None,
            StatusCode::SWITCHING_PROTOCOLS,
        ),
        (
            "unknown",
            Some("unknown"),
            Some(ORIGIN_VALUE),
            Some("4"),
            None,
            StatusCode::UNAUTHORIZED,
        ),
        (
            "revoked",
            Some(REVOKED),
            Some(ORIGIN_VALUE),
            Some("4"),
            None,
            StatusCode::UNAUTHORIZED,
        ),
        (
            "expired",
            Some(EXPIRED),
            Some(ORIGIN_VALUE),
            Some("4"),
            None,
            StatusCode::UNAUTHORIZED,
        ),
        (
            "missing-origin",
            Some(VALID),
            None,
            Some("4"),
            None,
            StatusCode::FORBIDDEN,
        ),
        (
            "foreign-origin",
            Some(VALID),
            Some("http://evil.test"),
            Some("4"),
            None,
            StatusCode::FORBIDDEN,
        ),
        (
            "authorization-only",
            None,
            Some(ORIGIN_VALUE),
            Some("4"),
            Some("Bearer valid-token"),
            StatusCode::UNAUTHORIZED,
        ),
        (
            "previous-epoch",
            Some(VALID),
            Some(ORIGIN_VALUE),
            Some("3"),
            None,
            StatusCode::UPGRADE_REQUIRED,
        ),
        (
            "old-epoch",
            Some(VALID),
            Some(ORIGIN_VALUE),
            None,
            None,
            StatusCode::UPGRADE_REQUIRED,
        ),
    ] {
        let (_directory, path) = fixture().await;
        let (address, controller, server) = running(&path).await;
        let result = connect(address, token, origin, epoch, authorization).await;
        if expected == StatusCode::SWITCHING_PROTOCOLS {
            let (mut socket, response) = result.unwrap();
            assert_eq!(response.status(), expected, "{name}");
            assert_eq!(next_json(&mut socket).await["type"], "authenticated");
            for scope in ["viewer:self", "conversation:conversation-1"] {
                socket
                    .send(tokio_tungstenite::tungstenite::Message::Text(
                        serde_json::json!({ "type": "subscribe", "scope": scope })
                            .to_string()
                            .into(),
                    ))
                    .await
                    .unwrap();
                assert_eq!(next_json(&mut socket).await["type"], "subscribed");
            }
            socket.close(None).await.unwrap();
        } else {
            assert_eq!(http_status(result.unwrap_err()), expected, "{name}");
        }
        controller.close().await;
        assert_eq!(controller.active_connection_count(), 0, "{name}");
        server.abort();
    }

    let (_directory, path) = fixture().await;
    let (address, controller, server) = running(&path).await;
    let result = connect(address, Some("unknown"), Some(ORIGIN_VALUE), None, None).await;
    assert_eq!(http_status(result.unwrap_err()), StatusCode::UNAUTHORIZED);
    assert_eq!(last_used(&path, "session-valid").await, 9_500);
    controller.close().await;
    server.abort();

    let (_directory, path) = fixture().await;
    let (address, controller, server) = running(&path).await;
    let result = connect(address, Some(VALID), Some(ORIGIN_VALUE), None, None).await;
    assert_eq!(
        http_status(result.unwrap_err()),
        StatusCode::UPGRADE_REQUIRED
    );
    // Authentication still precedes the epoch gate, but recent activity is read-only.
    assert_eq!(last_used(&path, "session-valid").await, 9_500);
    controller.close().await;
    server.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn auth_store_failure_is_one_safe_preupgrade_error() {
    let (_directory, path) = fixture().await;
    let (address, controller, server) = running(&path).await;
    let mut db = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
        .await
        .unwrap();
    sqlx::query("DROP TABLE auth_sessions")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();

    let logs = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .without_time()
        .with_target(false)
        .with_writer(logs.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let error = connect(address, Some(VALID), Some(ORIGIN_VALUE), Some("4"), None)
        .await
        .unwrap_err();
    assert_eq!(http_status(error), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(controller.active_connection_count(), 0);

    let errors = logs
        .events()
        .into_iter()
        .filter(|event| event["level"] == "ERROR")
        .collect::<Vec<_>>();
    assert_eq!(errors.len(), 1);
    assert_eq!(
        errors[0]["fields"]["message"],
        "event websocket session authentication failed"
    );
    let chain = errors[0]["fields"]["error_chain"].as_str().unwrap();
    assert!(chain.contains("no such table: auth_sessions"));
    assert!(!chain.contains(VALID));
    assert!(!chain.contains(&token_hash(VALID)));
    controller.close().await;
    server.abort();
}

fn token_hash(token: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(token.as_bytes()))
}

async fn connect(
    address: std::net::SocketAddr,
    token: Option<&str>,
    origin: Option<&str>,
    epoch: Option<&str>,
    authorization: Option<&str>,
) -> Result<
    (
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        tokio_tungstenite::tungstenite::handshake::client::Response,
    ),
    WebSocketError,
> {
    let mut request = format!("ws://{address}{EVENT_WEBSOCKET_PATH}")
        .into_client_request()
        .unwrap();
    if let Some(token) = token {
        request.headers_mut().insert(
            COOKIE,
            HeaderValue::from_str(&format!("{EVENT_WS_SESSION_COOKIE}={token}")).unwrap(),
        );
    }
    if let Some(origin) = origin {
        request
            .headers_mut()
            .insert(ORIGIN, origin.parse().unwrap());
    }
    if let Some(epoch) = epoch {
        request
            .headers_mut()
            .insert(DESKTOP_API_EPOCH_HEADER, epoch.parse().unwrap());
    }
    if let Some(authorization) = authorization {
        request
            .headers_mut()
            .insert(AUTHORIZATION, authorization.parse().unwrap());
    }
    connect_async(request).await
}

fn http_status(error: WebSocketError) -> StatusCode {
    let WebSocketError::Http(response) = error else {
        panic!("expected HTTP refusal: {error}")
    };
    response.status()
}

async fn last_used(path: &std::path::Path, id: &str) -> i64 {
    let mut db = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
        .await
        .unwrap();
    let value = sqlx::query_scalar("SELECT last_used_at FROM auth_sessions WHERE id = ?")
        .bind(id)
        .fetch_one(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    value
}

async fn next_json<S>(socket: &mut tokio_tungstenite::WebSocketStream<S>) -> serde_json::Value
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let tokio_tungstenite::tungstenite::Message::Text(frame) =
        socket.next().await.unwrap().unwrap()
    else {
        panic!("expected text frame")
    };
    serde_json::from_str(&frame).unwrap()
}

async fn sidebar_http_and_ws(
    path: &std::path::Path,
) -> (
    axum::Router,
    std::net::SocketAddr,
    silverhand_backend_server::foundation::scope_event_transport::EventWebSocketController,
    tokio::task::JoinHandle<()>,
) {
    use silverhand_backend_server::{
        identity_access::auth::{PublicJsonUnauthorized, require_human_session},
        viewer_read_model::viewer_sort::{SortMutationState, sort_mutation_router},
    };
    use silverhand_backend_sqlite::conversation_messaging::messaging::MessagingStore;

    let mut db = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    sqlx::query("INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES('user-2','second','Second',1,1)").execute(&mut db).await.unwrap();
    sqlx::query("INSERT INTO auth_sessions(id,user_id,token_hash,created_at,last_used_at) VALUES('second-session','user-2',?,1,9500)").bind(token_hash("second-token")).execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    let hub = ScopeEventHub::production().unwrap();
    let sessions = HumanSessionAuthenticator::new(
        SqliteSessionStore::open(&path).await.unwrap(),
        FixedClock,
        SessionTtl::from_millis(1_000).unwrap(),
    );
    let http = require_human_session(
        sort_mutation_router(SortMutationState {
            writer: MessagingStore::open(&path).await.unwrap().viewer_sort(),
            hub: hub.clone(),
        }),
        sessions.clone(),
        PublicJsonUnauthorized,
    );
    let (websocket, controller) = router(
        hub,
        sessions,
        SqliteEventScopeAuthority::open(&path).await.unwrap(),
        std::sync::Arc::new(NullLifecycle),
        EventWebSocketConfig::production([AllowedOrigin::parse(ORIGIN_VALUE).unwrap()]).unwrap(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, websocket).await.unwrap() });

    (http, address, controller, server)
}

async fn sidebar_request(
    app: &axum::Router,
    body: Option<serde_json::Value>,
    authenticated: bool,
) -> (StatusCode, serde_json::Value) {
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use tower::ServiceExt;
    let mut request = Request::builder()
        .method(if body.is_some() { "PUT" } else { "GET" })
        .uri(if body.is_some() {
            "/api/sort/order"
        } else {
            "/api/sort/sidebar"
        });
    if authenticated {
        request = request.header("authorization", "Bearer valid-token");
    }
    let response = app
        .clone()
        .oneshot(
            request
                .header("content-type", "application/json")
                .body(Body::from(body.map(|v| v.to_string()).unwrap_or_default()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let json =
        serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap();
    (status, json)
}

async fn subscribe_viewer(
    address: std::net::SocketAddr,
    token: &str,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let (mut socket, _) = connect(address, Some(token), Some(ORIGIN_VALUE), Some("4"), None)
        .await
        .unwrap();
    assert_eq!(next_json(&mut socket).await["type"], "authenticated");
    socket
        .send(tokio_tungstenite::tungstenite::Message::Text(
            serde_json::json!({"type":"subscribe","scope":"viewer:self"})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    assert_eq!(next_json(&mut socket).await["type"], "subscribed");
    socket
}

#[tokio::test]
async fn sidebar_http_commit_emits_viewer_hint_and_conflicts_do_not_write() {
    let (_directory, path) = fixture().await;
    let (http, address, controller, server) = sidebar_http_and_ws(&path).await;
    let mut socket = subscribe_viewer(address, VALID).await;
    let mut other = subscribe_viewer(address, "second-token").await;

    assert_eq!(
        sidebar_request(&http, None, false).await.0,
        StatusCode::UNAUTHORIZED
    );
    for (list_key, group) in [("workspaces", "regular"), ("sidebar", "agent_space")] {
        let retired = serde_json::json!({
            "list_key": list_key, "group": group, "ordered_ids": []
        });
        assert_eq!(
            sidebar_request(&http, Some(retired), true).await.0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }

    let body = serde_json::json!({"list_key":"sidebar","group":"sidebar","ordered_ids":["workspace:workspace-1", "section:direct_messages", "section:starred"]});
    let (status, value) = sidebar_request(&http, Some(body), true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        value["viewer_sort"]["entries"][0]["entity_id"],
        "workspace:workspace-1"
    );
    let event = next_json(&mut socket).await;
    assert!(
        event.to_string().contains("sidebar_sort_changed"),
        "{event}"
    );
    assert!(!event.to_string().contains("workspace-1"));

    let stale = serde_json::json!({"list_key":"sidebar","group":"sidebar","ordered_ids":[]});
    assert_eq!(
        sidebar_request(&http, Some(stale), true).await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        sidebar_request(&http, None, true).await,
        (StatusCode::OK, value)
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), other.next())
            .await
            .is_err()
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), socket.next())
            .await
            .is_err(),
        "refused reorder must not emit another hint"
    );

    other.close(None).await.unwrap();
    socket.close(None).await.unwrap();
    controller.close().await;
    server.abort();
}
