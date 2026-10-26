//! Preserved name-availability wire contract over the real HTTP/SQLite stack.
//! The checked-in seed and literal expected verdicts originated in TS parity;
//! the TS backend is retired. This runner remains the owner of available/taken/
//! invalid projection, Unicode query transport and regular-only namespace scope.
//! It does not claim to reserve a name or prove a concurrent create succeeds.

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::Value;
use silverhand_backend_core::foundation::identity::UserId;
use silverhand_backend_server::identity_access::workspace_name_availability::{
    self, WORKSPACE_NAME_AVAILABILITY_PATH,
};
use silverhand_backend_sqlite::identity_access::workspace_name_availability::SqliteRegularWorkspaceNamespace;
use sqlx::{Connection, Executor};
use tower::ServiceExt;

use crate::current_schema;

const SEED_SQL: &str =
    include_str!("../../../../../rust/test-fixtures/fixtures/workspace-name-availability/seed.sql");
const CASES_JSON: &str = include_str!(
    "../../../../../rust/test-fixtures/fixtures/workspace-name-availability/cases.json"
);

async fn seed(path: &std::path::Path) {
    let mut db = current_schema::create_current_schema(path).await;
    db.execute(sqlx::raw_sql(SEED_SQL))
        .await
        .expect("shared name-availability seed must load against the current schema");
    db.close().await.unwrap();
}

/// Percent-encode a raw name into the `name=` query value. Every byte outside the
/// unreserved set is `%XX`-escaped, so multi-byte scalars (the ZWJ in the
/// cross-length cases) round-trip through `serde_urlencoded` exactly.
fn encode_query_value(raw: &str) -> String {
    let mut out = String::new();
    for byte in raw.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char);
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[tokio::test]
async fn the_real_probe_produces_the_shared_expected_verdicts() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("name-availability.sqlite");
    seed(&path).await;

    let namespace = SqliteRegularWorkspaceNamespace::open(&path).await.unwrap();
    let app: Router = workspace_name_availability::router(namespace);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(WORKSPACE_NAME_AVAILABILITY_PATH)
                .extension(UserId::parse("user-1").unwrap())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap(),
        serde_json::json!({"available": false, "reason": "invalid"}),
        "an omitted query parameter defaults to an invalid empty name"
    );

    let fixture: Value = serde_json::from_str(CASES_JSON).unwrap();
    let expected_status = fixture["status"].as_u64().unwrap() as u16;
    let cases = fixture["cases"].as_array().unwrap();
    assert!(!cases.is_empty(), "the fixture must carry cases");

    for case in cases {
        let name = case["name"].as_str().unwrap();
        let input = case["input"].as_str().unwrap();
        let expected_body = &case["expected"];

        let uri = format!(
            "{WORKSPACE_NAME_AVAILABILITY_PATH}?name={}",
            encode_query_value(input)
        );
        let request = Request::builder()
            .uri(uri)
            .extension(UserId::parse("user-1").unwrap())
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();

        assert_eq!(
            response.status(),
            StatusCode::from_u16(expected_status).unwrap(),
            "{name}"
        );
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let actual: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(&actual, expected_body, "{name}");
    }
}
