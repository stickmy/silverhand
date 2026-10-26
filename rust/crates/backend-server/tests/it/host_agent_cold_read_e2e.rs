//! Public composition e2e for GET /api/presence (#task:2195, batch 1).
//!
//! Drives the REAL served route — the production `viewer_read_model::user_presence_http::router`
//! over the SQLite `SqliteUserPresenceColdRead` adapter AND the real
//! `PresenceGraceTimers` registry as the `OnlineViewers` authority — to prove
//! properties the driver-free unit tests cannot:
//!
//!   * the whole wire body is the two authorities composed once over real
//!     infrastructure: a viewer the live registry reports present suppresses its
//!     stored stamp, an absent viewer keeps it, and the cold read runs exactly
//!     once;
//!   * a cold-read fault is safe — a bare `text/plain` "Internal Server Error"
//!     with no stored value in it, and EXACTLY ONE operator ERROR event carrying
//!     the complete source chain — whether the fault is a broken table or a
//!     committed row whose stored stamp is outside the wire-safe range (caught at
//!     decode, BEFORE liveness, so an online viewer whose stamp WOULD be
//!     suppressed still cannot hide a corrupt one);
//!   * a missing principal is a fail-closed safe 500 that reads NOTHING.
//!
//! The route is AUTHENTICATED: in the TypeScript source the composition perimeter
//! (public-bearer middleware + registered-viewer gate) fronts `/api/presence`,
//! which is on neither allowlist, so an anonymous caller never reaches the handler.
//! This e2e stands in for that perimeter by injecting a `UserId` extension on the
//! success paths, exactly as the real session layer would. Turning a missing
//! principal into the external `401 {"error":"unauthorized"}` is the composition
//! root's job (#task:2197); here the handler's own obligation — refuse fail-closed
//! rather than leak the whole set — is exercised directly by omitting the
//! extension.

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
    viewer_read_model::presence::{LastSeenAt, UserPresenceFacts},
    viewer_read_model::presence_grace::{GraceWindow, PresenceFact},
    viewer_read_model::user_presence_cold_read::UserPresenceReadModel,
    viewer_read_model::user_presence_last_seen::LastSeenSink,
};
use silverhand_backend_server::{
    host_runtime::host_agent_background::presence_grace_timer::{
        PresenceFactSink, PresenceGraceTimers,
    },
    viewer_read_model::user_presence_http::{PRESENCE_PATH, router},
};
use silverhand_backend_sqlite::viewer_read_model::user_presence_cold_read::SqliteUserPresenceColdRead;
use sqlx::{Connection, Executor, SqliteConnection};
use tempfile::TempDir;
use tower::ServiceExt;

use crate::current_schema;
use crate::log_capture;

use log_capture::LogBuffer;

/// The registry needs a persistence and a publication sink; presence liveness
/// here is decided purely by `on_connect`, so neither sink has to do anything.
#[derive(Clone)]
struct NoOpSinks;

impl LastSeenSink for NoOpSinks {
    async fn touch_last_seen(
        &self,
        _user_id: UserId,
        _last_seen_at: LastSeenAt,
    ) -> Result<(), PersistenceFailure> {
        Ok(())
    }
}

impl PresenceFactSink for NoOpSinks {
    type Error = std::convert::Infallible;

    fn publish(&self, _fact: PresenceFact) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// Wraps the production adapter to witness that the cold read ran exactly once.
#[derive(Clone)]
struct CountingColdRead {
    inner: SqliteUserPresenceColdRead,
    calls: Arc<AtomicUsize>,
}

impl UserPresenceReadModel for CountingColdRead {
    async fn list_user_presence_facts(&self) -> Result<Vec<UserPresenceFacts>, PersistenceFailure> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.list_user_presence_facts().await
    }
}

struct Fixture {
    _directory: TempDir,
    path: std::path::PathBuf,
    cold_read: CountingColdRead,
    timers: PresenceGraceTimers<NoOpSinks, NoOpSinks>,
    calls: Arc<AtomicUsize>,
}

/// Two viewers in creation order: `user-early` never connects (offline, keeps its
/// stamp) and `user-online` connects (present, so its stamp is suppressed).
async fn fixture() -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("presence-http.sqlite");
    let mut db = current_schema::create_current_schema(&path).await;
    db.execute(sqlx::raw_sql(
        "INSERT INTO users (id, name, display_name, created_at, updated_at, last_seen_at) VALUES \
         ('user-early', 'early', 'Early', 100, 100, 500), \
         ('user-online', 'online', 'Online', 200, 200, 900)",
    ))
    .await
    .unwrap();
    db.close().await.unwrap();

    let inner = SqliteUserPresenceColdRead::open(&path).await.unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let timers = PresenceGraceTimers::new(
        NoOpSinks,
        NoOpSinks,
        GraceWindow::from_millis_or_default(Some(20_000)),
    );
    Fixture {
        _directory: directory,
        path,
        cold_read: CountingColdRead {
            inner,
            calls: calls.clone(),
        },
        timers,
        calls,
    }
}

fn app(fixture: &Fixture) -> Router {
    router(fixture.cold_read.clone(), fixture.timers.clone())
}

async fn get(app: &Router) -> (StatusCode, String, String) {
    let response = app
        .clone()
        .oneshot(
            Request::get(PRESENCE_PATH)
                .extension(UserId::parse("viewer-1").unwrap())
                .body(Body::empty())
                .unwrap(),
        )
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

#[tokio::test]
async fn the_body_is_the_two_authorities_composed_once_over_real_infrastructure() {
    let fixture = fixture().await;
    // A live connection makes `user-online` present through the real registry.
    fixture
        .timers
        .on_connect(&UserId::parse("user-online").unwrap(), "conn-1");

    let (status, content_type, body) = get(&app(&fixture)).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type, "application/json");
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap(),
        json!({
            "users": [
                { "user_id": "user-early", "presence": "offline", "last_seen_at": 500 },
                { "user_id": "user-online", "presence": "online", "last_seen_at": null },
            ]
        })
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn a_cold_read_fault_is_one_safe_500_with_the_complete_source_chain() {
    let fixture = fixture().await;
    let mut db = SqliteConnection::connect(&format!("sqlite:{}", fixture.path.display()))
        .await
        .unwrap();
    sqlx::query("DROP TABLE users")
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
    let (status, content_type, body) = get(&app(&fixture)).await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(content_type.starts_with("text/plain"));
    assert_eq!(body, "Internal Server Error");
    // The read was attempted; it is the read that failed.
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);

    let errors = logs
        .events()
        .into_iter()
        .filter(|event| event["level"] == "ERROR")
        .collect::<Vec<_>>();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0]["fields"]["message"], "presence cold read failed");
    let chain = errors[0]["fields"]["error_chain"].as_str().unwrap();
    assert!(chain.contains("no such table: users"), "{chain}");
}

#[tokio::test]
async fn a_missing_principal_is_a_safe_500_and_never_reads() {
    let fixture = fixture().await;
    // No extension: the authenticating perimeter never ran, so the handler must
    // refuse fail-closed WITHOUT touching the store — never leaking the set.
    let response = app(&fixture)
        .oneshot(Request::get(PRESENCE_PATH).body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn an_online_viewer_with_a_corrupt_stored_stamp_is_a_whole_read_safe_500() {
    let fixture = fixture().await;
    // 9_007_199_254_740_992 == MAX_SAFE_INTEGER + 1: a value the wire could not
    // carry losslessly. It is the ONLINE viewer's stamp, so at render it would be
    // suppressed to null — only a decode-time check, before liveness, can refuse it.
    let mut db = SqliteConnection::connect(&format!("sqlite:{}", fixture.path.display()))
        .await
        .unwrap();
    sqlx::query("UPDATE users SET last_seen_at = 9007199254740992 WHERE id = 'user-online'")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    fixture
        .timers
        .on_connect(&UserId::parse("user-online").unwrap(), "conn-1");

    let logs = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .without_time()
        .with_target(false)
        .with_writer(logs.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let (status, content_type, body) = get(&app(&fixture)).await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(content_type.starts_with("text/plain"));
    assert_eq!(body, "Internal Server Error");
    // The read was attempted; decode refused the corrupt row before any liveness.
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);

    let errors = logs
        .events()
        .into_iter()
        .filter(|event| event["level"] == "ERROR")
        .collect::<Vec<_>>();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0]["fields"]["message"], "presence cold read failed");
    let chain = errors[0]["fields"]["error_chain"].as_str().unwrap();
    assert!(chain.contains("users.last_seen_at"), "{chain}");
}
