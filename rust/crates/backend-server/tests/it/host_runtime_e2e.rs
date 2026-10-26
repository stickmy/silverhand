use std::{
    collections::VecDeque,
    convert::Infallible,
    io::{self, Write},
    sync::{Arc, Mutex},
};

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::Value;
use silverhand_backend_core::{
    foundation::identity::UserId,
    host_runtime::{
        environment::{Clock, EntropySource},
        invite::CreateHostInvite,
    },
};
use silverhand_backend_server::host_runtime::{self, HOSTS_PATH};
use silverhand_backend_sqlite::host_runtime::SqliteHostRuntimeStore;
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use tempfile::TempDir;
use tower::ServiceExt;
use tracing_subscriber::fmt::MakeWriter;

use crate::current_schema;

#[derive(Clone)]
struct FixedEntropy(Arc<Mutex<VecDeque<u8>>>);

impl FixedEntropy {
    fn sequence() -> Self {
        Self(Arc::new(Mutex::new((0_u8..=u8::MAX).collect())))
    }
}

impl EntropySource for FixedEntropy {
    type Error = Infallible;

    fn fill(&self, destination: &mut [u8]) -> Result<(), Self::Error> {
        let mut source = self.0.lock().unwrap();
        for byte in destination {
            *byte = source.pop_front().unwrap();
        }
        Ok(())
    }
}

async fn fixture(name: &str) -> (TempDir, std::path::PathBuf, SqliteHostRuntimeStore) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(name);
    let mut connection = current_schema::create_current_schema(&path).await;
    sqlx::query(
        "INSERT INTO users (id, name, display_name, created_at, updated_at)
         VALUES ('user-1', 'user-1', 'User One', 1, 1)",
    )
    .execute(&mut connection)
    .await
    .unwrap();
    connection.close().await.unwrap();
    let store = SqliteHostRuntimeStore::open(&path).await.unwrap();
    (directory, path, store)
}

fn app(store: SqliteHostRuntimeStore) -> Router {
    host_runtime::router(
        CreateHostInvite::new(store, FixedEntropy::sequence(), FixedClock),
        None,
    )
}

#[derive(Clone, Copy)]
struct FixedClock;

impl Clock for FixedClock {
    type Error = Infallible;

    fn unix_time_ms(&self) -> Result<i64, Self::Error> {
        Ok(1_700_000_000_123)
    }
}

#[tokio::test]
async fn real_sqlite_store_drives_the_public_host_create_wire() {
    let (_directory, path, store) = fixture("success.sqlite").await;
    let (status, body) = request(&app(store), "{}").await;
    assert_eq!(status, StatusCode::OK);
    let body: Value = serde_json::from_str(&body).unwrap();
    let fixture: Value = serde_json::from_str(include_str!(
        "../../src/host_runtime/fixtures/create_host_invite_parity.json"
    ))
    .unwrap();
    let mut expected = fixture["success"]["body"].clone();
    // Substitute independent expected values, never normalize actual response fields.
    expected["host_id"] = "AAECAwQFBgcICQoL".into();
    expected["host"]["id"] = "AAECAwQFBgcICQoL".into();
    expected["host"]["name"] = "computer-0c0d0e0f".into();
    expected["host"]["created_at"] = 1_700_000_000_123_i64.into();
    expected["host"]["creator_user_id"] = "user-1".into();
    expected["connect_token"] = "HB0eHyAhIiMkJSYnKCkqKywtLi8wMTIzNDU2Nzg5Ojs".into();
    assert_eq!(body, expected);

    let options = SqliteConnectOptions::new()
        .filename(path)
        .foreign_keys(true);
    let mut connection = SqliteConnection::connect_with(&options).await.unwrap();
    let stored_hash: String = sqlx::query_scalar("SELECT token_hash FROM host_credentials")
        .fetch_one(&mut connection)
        .await
        .unwrap();
    // Independent Node SHA-256/base64url vector for the expected returned secret.
    assert_eq!(stored_hash, "NqQOYYO_N23JO0dqiftLMauI6EQzdxH2jHyMjWFcvsM");
    let counts: (i64, i64) = (
        sqlx::query_scalar("SELECT count(*) FROM hosts")
            .fetch_one(&mut connection)
            .await
            .unwrap(),
        sqlx::query_scalar("SELECT count(*) FROM host_credentials")
            .fetch_one(&mut connection)
            .await
            .unwrap(),
    );
    assert_eq!(counts, (1, 1));
}

#[tokio::test(flavor = "current_thread")]
async fn real_credential_failure_rolls_back_and_logs_once_without_the_secret() {
    let (_directory, path, store) = fixture("failure.sqlite").await;
    let options = SqliteConnectOptions::new()
        .filename(&path)
        .foreign_keys(true);
    let mut independent = SqliteConnection::connect_with(&options).await.unwrap();
    sqlx::query(
        "CREATE TRIGGER reject_host_credential BEFORE INSERT ON host_credentials
         BEGIN SELECT RAISE(ABORT, 'forced host credential failure'); END",
    )
    .execute(&mut independent)
    .await
    .unwrap();

    let logs = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .without_time()
        .with_target(false)
        .with_writer(logs.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let (status, body) = request(&app(store), "{}").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body, "Internal Server Error");
    let known_generated_token = "HB0eHyAhIiMkJSYnKCkqKywtLi8wMTIzNDU2Nzg5Ojs";

    let host_count: i64 = sqlx::query_scalar("SELECT count(*) FROM hosts")
        .fetch_one(&mut independent)
        .await
        .unwrap();
    let credential_count: i64 = sqlx::query_scalar("SELECT count(*) FROM host_credentials")
        .fetch_one(&mut independent)
        .await
        .unwrap();
    assert_eq!((host_count, credential_count), (0, 0));

    let events = logs.events();
    let errors = events
        .iter()
        .filter(|event| event["level"] == "ERROR")
        .collect::<Vec<_>>();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0]["fields"]["message"], "create host invite failed");
    let chain = errors[0]["fields"]["error_chain"].as_str().unwrap();
    assert!(chain.contains("forced host credential failure"));
    assert!(!chain.contains(known_generated_token));
}

async fn request(app: &Router, body: &'static str) -> (StatusCode, String) {
    let mut request = Request::post(HOSTS_PATH)
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    request
        .extensions_mut()
        .insert(UserId::parse("user-1").unwrap());
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, String::from_utf8(body.to_vec()).unwrap())
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
