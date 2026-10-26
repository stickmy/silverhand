//! Authenticated composition e2e for GET /api/workspaces (#task:2190).
//!
//! Drives the REAL served composition — `require_human_session` in front of the
//! production `workspaces::router` over the SQLite `SqliteWorkspaceColdRead`
//! adapter — to prove three properties the driver-free unit tests cannot:
//!
//!   * a valid human session yields the literal expected workspace envelope,
//!     and the cold read runs exactly once;
//!   * every authentication refusal (no bearer, unknown / revoked / expired
//!     session) is a 401 that NEVER reaches the cold read — the product answer is
//!     not computed for an unauthenticated caller;
//!   * both 500 flavors — an authentication-store fault and a cold-read fault —
//!     are safe: a bare `text/plain` "Internal Server Error" body with no bearer
//!     or token-hash in it, and EXACTLY ONE operator ERROR event carrying the
//!     complete source chain (#task:2190 P1-6).
//!
//! The historical seed is shared with enrollment and startup fixtures; only
//! the auth row is added here. This is the wire/SQLite envelope's primary owner.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use silverhand_backend_core::{
    foundation::identity::UserId,
    foundation::persistence::PersistenceFailure,
    identity_access::auth::{HumanSessionAuthenticator, SessionClock, SessionTtl},
    identity_access::workspace_cold_read::{WorkspaceColdRead, WorkspaceColdReadUnitOfWork},
};
use silverhand_backend_server::identity_access::{
    auth::{PublicJsonUnauthorized, require_human_session},
    workspaces::{self, WORKSPACES_PATH},
};
use silverhand_backend_sqlite::{
    identity_access::auth::SqliteSessionStore,
    identity_access::workspace_cold_read::SqliteWorkspaceColdRead,
};
use sqlx::{Connection, Executor, SqliteConnection};
use tempfile::TempDir;
use tower::ServiceExt;

use crate::current_schema;
use crate::log_capture;

use log_capture::LogBuffer;

/// Shared historical workspace seed, joined here by the auth row.
const SEED_SQL: &str =
    include_str!("../../../../../rust/test-fixtures/fixtures/workspace-cold-read/seed.sql");
const EXPECTED_ENVELOPE: &str = include_str!("../fixtures/workspace-cold-read-envelope.json");

const TOKEN: &str = "valid-token";
const TOKEN_HASH: &str = "OXoqnFv14szsOMJZa2grsb0F_m5OzqbBDPQnVf8iVAM";

#[derive(Clone)]
struct FixedClock;

impl SessionClock for FixedClock {
    fn now_unix_millis(&self) -> i64 {
        10_000
    }
}

/// Wraps the production adapter to witness whether the product answer was ever
/// computed — the refusal cases must leave this at zero.
#[derive(Clone)]
struct CountingColdRead {
    inner: SqliteWorkspaceColdRead,
    calls: Arc<AtomicUsize>,
}

impl WorkspaceColdReadUnitOfWork for CountingColdRead {
    async fn load_workspace_cold_read(
        &self,
        viewer: &UserId,
    ) -> Result<WorkspaceColdRead, PersistenceFailure> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.load_workspace_cold_read(viewer).await
    }
}

struct Fixture {
    _directory: TempDir,
    path: std::path::PathBuf,
    store: SqliteSessionStore,
    cold_read: CountingColdRead,
    calls: Arc<AtomicUsize>,
}

async fn fixture() -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("workspaces-http.sqlite");
    let mut db = current_schema::create_current_schema(&path).await;
    db.execute(sqlx::raw_sql(SEED_SQL))
        .await
        .expect("shared cold-read seed must load against the current schema");
    // The auth row the shared seed omits: a live session for the seed's viewer,
    // valid under FixedClock (now 10_000, last_used_at 9_500, TTL 1_000ms).
    db.execute(sqlx::raw_sql(
        "INSERT INTO auth_sessions (id, user_id, token_hash, created_at, last_used_at, \
         revoked_at, user_agent, ip_first_seen) VALUES \
         ('session-1', 'user-1', 'OXoqnFv14szsOMJZa2grsb0F_m5OzqbBDPQnVf8iVAM', 1, 9500, NULL, NULL, NULL)",
    ))
    .await
    .unwrap();
    db.close().await.unwrap();

    let store = SqliteSessionStore::open(&path).await.unwrap();
    let inner = SqliteWorkspaceColdRead::open(&path).await.unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    Fixture {
        _directory: directory,
        path,
        store,
        cold_read: CountingColdRead {
            inner,
            calls: calls.clone(),
        },
        calls,
    }
}

fn app(fixture: &Fixture) -> Router {
    let authority = HumanSessionAuthenticator::new(
        fixture.store.clone(),
        FixedClock,
        SessionTtl::from_millis(1_000).unwrap(),
    );
    require_human_session(
        workspaces::router(fixture.cold_read.clone()),
        authority,
        PublicJsonUnauthorized,
    )
}

async fn request(app: &Router, authorization: Option<&str>) -> (StatusCode, String, String) {
    let mut builder = Request::get(WORKSPACES_PATH);
    if let Some(authorization) = authorization {
        builder = builder.header("authorization", authorization);
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

#[derive(Clone, Copy, Debug)]
enum Refusal {
    Missing,
    Unknown,
    Revoked,
    Expired,
}

async fn mutate_session(path: &std::path::Path, refusal: Refusal) {
    let mut db = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    match refusal {
        Refusal::Revoked => {
            sqlx::query("UPDATE auth_sessions SET revoked_at = 9999 WHERE id = 'session-1'")
                .execute(&mut db)
                .await
                .unwrap();
        }
        Refusal::Expired => {
            sqlx::query("UPDATE auth_sessions SET last_used_at = 1 WHERE id = 'session-1'")
                .execute(&mut db)
                .await
                .unwrap();
        }
        Refusal::Missing | Refusal::Unknown => unreachable!("these cases do not mutate a session"),
    }
    db.close().await.unwrap();
}

#[tokio::test]
async fn valid_session_returns_the_shared_envelope_through_one_cold_read() {
    let fixture = fixture().await;
    let response = request(&app(&fixture), Some("Bearer valid-token")).await;

    assert_eq!(response.0, StatusCode::OK);
    assert_eq!(response.1, "application/json");
    assert_eq!(
        serde_json::from_str::<Value>(&response.2).unwrap(),
        serde_json::from_str::<Value>(EXPECTED_ENVELOPE).unwrap(),
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn authentication_refusals_never_enter_the_cold_read() {
    for refusal in [
        Refusal::Missing,
        Refusal::Unknown,
        Refusal::Revoked,
        Refusal::Expired,
    ] {
        let fixture = fixture().await;
        match refusal {
            Refusal::Revoked | Refusal::Expired => mutate_session(&fixture.path, refusal).await,
            Refusal::Missing | Refusal::Unknown => {}
        }
        let authorization = match refusal {
            Refusal::Missing => None,
            Refusal::Unknown => Some("Bearer unknown-token"),
            Refusal::Revoked | Refusal::Expired => Some("Bearer valid-token"),
        };
        let response = request(&app(&fixture), authorization).await;
        assert_eq!(response.0, StatusCode::UNAUTHORIZED, "{refusal:?}");
        assert_eq!(
            serde_json::from_str::<Value>(&response.2).unwrap(),
            json!({"error": "unauthorized"}),
            "{refusal:?}",
        );
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 0, "{refusal:?}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn authentication_store_failure_is_safe_and_never_enters_the_cold_read() {
    let fixture = fixture().await;
    let mut db = SqliteConnection::connect(&format!("sqlite:{}", fixture.path.display()))
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
    let response = request(&app(&fixture), Some("Bearer valid-token")).await;

    assert_eq!(response.0, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(response.1.starts_with("text/plain"));
    assert_eq!(response.2, "Internal Server Error");
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);

    let errors = logs
        .events()
        .into_iter()
        .filter(|event| event["level"] == "ERROR")
        .collect::<Vec<_>>();
    assert_eq!(errors.len(), 1);
    assert_eq!(
        errors[0]["fields"]["message"],
        "human session authentication failed"
    );
    let chain = errors[0]["fields"]["error_chain"].as_str().unwrap();
    assert!(chain.contains("no such table: auth_sessions"), "{chain}");
    assert!(!chain.contains(TOKEN));
    assert!(!chain.contains(TOKEN_HASH));
}

#[tokio::test(flavor = "current_thread")]
async fn read_failure_is_one_safe_500_with_the_complete_source_chain() {
    let fixture = fixture().await;
    let mut db = SqliteConnection::connect(&format!("sqlite:{}", fixture.path.display()))
        .await
        .unwrap();
    sqlx::query("DROP TABLE workspaces")
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
    let response = request(&app(&fixture), Some("Bearer valid-token")).await;

    assert_eq!(response.0, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(response.1.starts_with("text/plain"));
    assert_eq!(response.2, "Internal Server Error");
    // The authenticated caller DID reach the cold read; it is the read that failed.
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);

    let errors = logs
        .events()
        .into_iter()
        .filter(|event| event["level"] == "ERROR")
        .collect::<Vec<_>>();
    assert_eq!(errors.len(), 1);
    assert_eq!(
        errors[0]["fields"]["message"],
        "workspaces cold read failed"
    );
    let chain = errors[0]["fields"]["error_chain"].as_str().unwrap();
    assert!(chain.contains("no such table: workspaces"), "{chain}");
    assert!(!chain.contains(TOKEN));
    assert!(!chain.contains(TOKEN_HASH));
}
