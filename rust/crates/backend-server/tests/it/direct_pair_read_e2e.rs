use std::{
    io::{self, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use silverhand_backend_core::{
    conversation_messaging::direct_pair_read::{DirectPairList, DirectPairReadModel},
    foundation::identity::UserId,
    foundation::persistence::PersistenceFailure,
    identity_access::auth::{HumanSessionAuthenticator, SessionClock, SessionTtl},
};
use silverhand_backend_server::{
    conversation_messaging::direct_pairs::{self, DIRECT_PAIRS_PATH},
    identity_access::auth::{PublicJsonUnauthorized, require_human_session},
};
use silverhand_backend_sqlite::{
    conversation_messaging::direct_pair_read::SqliteDirectPairReadModel,
    identity_access::auth::SqliteSessionStore,
};
use sqlx::{Connection, SqliteConnection};
use tempfile::TempDir;
use tower::ServiceExt;
use tracing_subscriber::fmt::MakeWriter;

use crate::current_schema;

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
struct CountingReadModel {
    inner: SqliteDirectPairReadModel,
    calls: Arc<AtomicUsize>,
}

impl DirectPairReadModel for CountingReadModel {
    async fn list_direct_pairs(
        &self,
        viewer: &UserId,
    ) -> Result<DirectPairList, PersistenceFailure> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.list_direct_pairs(viewer).await
    }
}

struct Fixture {
    _directory: TempDir,
    path: std::path::PathBuf,
    store: SqliteSessionStore,
    read_model: CountingReadModel,
    calls: Arc<AtomicUsize>,
}

async fn fixture() -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("direct-pairs-http.sqlite");
    let mut db = current_schema::create_current_schema(&path).await;
    for statement in [
        "INSERT INTO users (id, name, display_name, created_at, updated_at) VALUES \
         ('user-1', 'viewer', 'Viewer', 1, 1), \
         ('user-2', 'other', 'Other Human', 2, 2), \
         ('user-3', 'foreign', 'Foreign', 3, 3)",
        "INSERT INTO auth_sessions (id, user_id, token_hash, created_at, last_used_at, revoked_at, \
         user_agent, ip_first_seen) VALUES \
         ('session-1', 'user-1', 'OXoqnFv14szsOMJZa2grsb0F_m5OzqbBDPQnVf8iVAM', 1, 9500, NULL, NULL, NULL)",
        "INSERT INTO hosts (id, name, kind, created_at) VALUES ('host-1', 'Host', 'local', 1)",
        "INSERT INTO agents (id, name, display_name, runtime, host_id, created_by_user_id, \
         created_at, updated_at, session_scope) VALUES \
         ('agent-1', 'agent', 'Agent One', 'codex', 'host-1', 'user-1', 1, 11, 'agent')",
        "INSERT INTO conversations (id, placement_kind, roster_policy, title, status, \
         endpoint_a_kind, endpoint_a_id, endpoint_b_kind, endpoint_b_id, created_at, updated_at) \
         VALUES ('pair-agent', 'direct_pair', 'fixed', '', 'open', 'agent', 'agent-1', 'human', \
         'user-1', 100, 100)",
        "INSERT INTO conversations (id, placement_kind, roster_policy, title, status, \
         endpoint_a_kind, endpoint_a_id, endpoint_b_kind, endpoint_b_id, created_at, updated_at) \
         VALUES ('pair-human', 'direct_pair', 'fixed', '', 'open', 'human', 'user-1', 'human', \
         'user-2', 200, 200)",
        "INSERT INTO conversations (id, placement_kind, roster_policy, title, status, \
         endpoint_a_kind, endpoint_a_id, endpoint_b_kind, endpoint_b_id, created_at, updated_at) \
         VALUES ('pair-foreign', 'direct_pair', 'fixed', '', 'open', 'agent', 'agent-1', 'human', \
         'user-3', 50, 50)",
        "INSERT INTO messages (id, conversation_id, from_kind, from_agent_id, content, seq, \
         created_at) VALUES ('message-1', 'pair-agent', 'agent', 'agent-1', 'hello', 1, 1)",
    ] {
        sqlx::query(statement).execute(&mut db).await.unwrap();
    }
    db.close().await.unwrap();
    let store = SqliteSessionStore::open(&path).await.unwrap();
    let inner = SqliteDirectPairReadModel::open(&path).await.unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    Fixture {
        _directory: directory,
        path,
        store,
        read_model: CountingReadModel {
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
        direct_pairs::router(fixture.read_model.clone()),
        authority,
        PublicJsonUnauthorized,
    )
}

#[tokio::test]
async fn valid_session_returns_the_complete_oldest_first_published_shape() {
    let fixture = fixture().await;
    let response = request(&app(&fixture), Some("Bearer valid-token")).await;
    assert_eq!(response.0, StatusCode::OK);
    assert_eq!(response.1, "application/json");
    let contract: Value = serde_json::from_str(include_str!(
        "../../../../../packages/shared/tests/fixtures/direct-pair-list.json"
    ))
    .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&response.2).unwrap(),
        contract["authority_case"]["response"]
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn authentication_refusals_never_enter_the_read_model() {
    for refusal in [
        Refusal::Missing,
        Refusal::Unknown,
        Refusal::Revoked,
        Refusal::Expired,
    ] {
        let fixture = fixture().await;
        match refusal {
            Refusal::Revoked => mutate_session(&fixture.path, refusal).await,
            Refusal::Expired => mutate_session(&fixture.path, refusal).await,
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
            json!({"error": "unauthorized"})
        );
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 0, "{refusal:?}");
    }
}

#[tokio::test]
async fn a_valid_session_can_only_select_its_own_endpoint_transpose() {
    let fixture = fixture().await;
    let mut db = SqliteConnection::connect(&format!("sqlite:{}", fixture.path.display()))
        .await
        .unwrap();
    sqlx::query("UPDATE auth_sessions SET user_id = 'user-3' WHERE id = 'session-1'")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();

    let response = request(&app(&fixture), Some("Bearer valid-token")).await;
    assert_eq!(response.0, StatusCode::OK);
    let body: Value = serde_json::from_str(&response.2).unwrap();
    assert_eq!(
        body["direct_pairs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["conversation_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["pair-foreign"]
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn authentication_store_failure_is_safe_and_never_enters_the_read_model() {
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
    sqlx::query("DROP TABLE conversations")
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
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    assert!(!response.2.contains(TOKEN));
    assert!(!response.2.contains(TOKEN_HASH));
    let errors = logs
        .events()
        .into_iter()
        .filter(|event| event["level"] == "ERROR")
        .collect::<Vec<_>>();
    assert_eq!(errors.len(), 1);
    assert_eq!(
        errors[0]["fields"]["message"],
        "direct-pairs cold read failed"
    );
    let chain = errors[0]["fields"]["error_chain"].as_str().unwrap();
    assert!(chain.contains("no such table: conversations"), "{chain}");
    assert!(!chain.contains(TOKEN));
    assert!(!chain.contains(TOKEN_HASH));
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

async fn request(app: &Router, authorization: Option<&str>) -> (StatusCode, String, String) {
    let mut builder = Request::get(DIRECT_PAIRS_PATH);
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

#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl LogBuffer {
    fn events(&self) -> Vec<Value> {
        String::from_utf8(self.0.lock().unwrap().clone())
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
