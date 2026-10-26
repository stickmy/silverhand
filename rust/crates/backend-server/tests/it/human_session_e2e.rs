use std::{
    io::{self, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use axum::{
    Extension, Json, Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
    routing::get,
};
use serde_json::{Value, json};
use silverhand_backend_core::{
    foundation::identity::UserId,
    identity_access::auth::{HumanSessionAuthenticator, SessionClock, SessionTtl},
};
use silverhand_backend_server::identity_access::auth::{
    PublicJsonUnauthorized, require_human_session,
};
use silverhand_backend_sqlite::identity_access::auth::SqliteSessionStore;
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use tempfile::TempDir;
use tower::ServiceExt;
use tracing_subscriber::fmt::MakeWriter;

use crate::current_schema;

const PATH: &str = "/api/whoami";
const TOKEN: &str = "valid-token";
const TOKEN_HASH: &str = "OXoqnFv14szsOMJZa2grsb0F_m5OzqbBDPQnVf8iVAM";

#[derive(Clone)]
struct FixedClock;

impl SessionClock for FixedClock {
    fn now_unix_millis(&self) -> i64 {
        10_000
    }
}

#[derive(Clone)]
struct Calls(Arc<AtomicUsize>);

async fn whoami(
    Extension(user_id): Extension<UserId>,
    Extension(calls): Extension<Calls>,
) -> Json<Value> {
    calls.0.fetch_add(1, Ordering::SeqCst);
    Json(json!({ "user_id": user_id.as_str() }))
}

async fn fixture() -> (TempDir, std::path::PathBuf, SqliteSessionStore) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("server-session.sqlite");
    let mut connection = current_schema::create_current_schema(&path).await;
    sqlx::query(
        "INSERT INTO users (id, name, display_name, created_at, updated_at)
         VALUES ('user-1', 'user-1', 'User One', 1, 1)",
    )
    .execute(&mut connection)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auth_sessions
            (id, user_id, token_hash, created_at, last_used_at, revoked_at,
             user_agent, ip_first_seen)
         VALUES ('session-1', 'user-1', ?, 1, 9500, NULL, NULL, NULL)",
    )
    .bind(TOKEN_HASH)
    .execute(&mut connection)
    .await
    .unwrap();
    connection.close().await.unwrap();
    let store = SqliteSessionStore::open(&path).await.unwrap();
    (directory, path, store)
}

fn app(store: SqliteSessionStore, calls: Calls) -> Router {
    let authority =
        HumanSessionAuthenticator::new(store, FixedClock, SessionTtl::from_millis(1_000).unwrap());
    let product = Router::new()
        .route(PATH, get(whoami))
        .layer(Extension(calls));
    require_human_session(product, authority, PublicJsonUnauthorized)
}

#[tokio::test]
async fn real_bearer_injects_the_single_typed_user_fact() {
    for authorization in ["Bearer valid-token", "bEaReR\t  valid-token  "] {
        let (_directory, _path, store) = fixture().await;
        let calls = Calls(Arc::new(AtomicUsize::new(0)));
        let response = request(&app(store, calls.clone()), Some(authorization), None).await;

        assert_eq!(response.0, StatusCode::OK, "{authorization:?}");
        assert_eq!(
            serde_json::from_str::<Value>(&response.2).unwrap(),
            json!({ "user_id": "user-1" })
        );
        assert_eq!(calls.0.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn missing_wrong_raw_and_legacy_identity_are_refused_before_the_handler() {
    for authorization in [
        None,
        Some("Basic valid-token"),
        Some("valid-token"),
        Some("Bearer unknown-token"),
    ] {
        let (_directory, path, store) = fixture().await;
        let calls = Calls(Arc::new(AtomicUsize::new(0)));
        let response = request(
            &app(store, calls.clone()),
            authorization,
            Some("forged-user"),
        )
        .await;

        assert_eq!(response.0, StatusCode::UNAUTHORIZED, "{authorization:?}");
        assert_eq!(
            serde_json::from_str::<Value>(&response.2).unwrap(),
            json!({ "error": "unauthorized" })
        );
        assert_eq!(calls.0.load(Ordering::SeqCst), 0, "{authorization:?}");
        assert_eq!(read_last_used_at(&path).await, 9_500, "{authorization:?}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn driver_failure_logs_one_complete_chain_and_leaks_no_credential() {
    let (_directory, path, store) = fixture().await;
    let options = SqliteConnectOptions::new()
        .filename(&path)
        .foreign_keys(true);
    let mut connection = SqliteConnection::connect_with(&options).await.unwrap();
    sqlx::query("DROP TABLE auth_sessions")
        .execute(&mut connection)
        .await
        .unwrap();
    connection.close().await.unwrap();

    let calls = Calls(Arc::new(AtomicUsize::new(0)));
    let logs = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .without_time()
        .with_target(false)
        .with_writer(logs.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let response = request(&app(store, calls.clone()), Some("Bearer valid-token"), None).await;

    assert_eq!(response.0, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(response.1.starts_with("text/plain"));
    assert_eq!(response.2, "Internal Server Error");
    assert_eq!(calls.0.load(Ordering::SeqCst), 0);
    let events = logs.events();
    let rendered = serde_json::to_string(&events).unwrap();
    assert!(!rendered.contains(TOKEN));
    assert!(!rendered.contains(TOKEN_HASH));
    let errors = events
        .into_iter()
        .filter(|event| event["level"] == "ERROR")
        .collect::<Vec<_>>();
    assert_eq!(errors.len(), 1);
    assert_eq!(
        errors[0]["fields"]["message"],
        "human session authentication failed"
    );
    let chain = errors[0]["fields"]["error_chain"].as_str().unwrap();
    assert!(chain.contains("no such table: auth_sessions"));
}

async fn request(
    app: &Router,
    authorization: Option<&str>,
    legacy_user: Option<&str>,
) -> (StatusCode, String, String) {
    let mut builder = Request::get(PATH);
    if let Some(authorization) = authorization {
        builder = builder.header("authorization", authorization);
    }
    if let Some(legacy_user) = legacy_user {
        builder = builder.header("x-silverhand-user-id", legacy_user);
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (
        status,
        content_type,
        String::from_utf8(body.to_vec()).unwrap(),
    )
}

async fn read_last_used_at(path: &std::path::Path) -> i64 {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .foreign_keys(true);
    let mut connection = SqliteConnection::connect_with(&options).await.unwrap();
    let value = sqlx::query_scalar("SELECT last_used_at FROM auth_sessions WHERE id = 'session-1'")
        .fetch_one(&mut connection)
        .await
        .unwrap();
    connection.close().await.unwrap();
    value
}

#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl LogBuffer {
    fn events(&self) -> Vec<Value> {
        let bytes = self.0.lock().unwrap().clone();
        String::from_utf8(bytes)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

struct LogWriter(LogBuffer);

impl Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for LogBuffer {
    type Writer = LogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        LogWriter(self.clone())
    }
}
