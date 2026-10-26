//! Real HTTP/SQLite contract for GET /api/workspaces/:id.
//!
//! Literal fixture responses preserve the legacy contract after TS retirement.
//! The real adapter, classifier and DTO enforce existence-before-visibility,
//! ordered home DM channels, and the published face without a mocked read port.

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::Value;
use silverhand_backend_core::foundation::identity::UserId;
use silverhand_backend_server::identity_access::workspace_detail::{self, WORKSPACE_DETAIL_PATH};
use silverhand_backend_sqlite::identity_access::workspace_detail_read::SqliteWorkspaceDetailRead;
use sqlx::{Connection, Executor};
use tower::ServiceExt;

use crate::current_schema;

const SEED_SQL: &str =
    include_str!("../../../../../rust/test-fixtures/fixtures/workspace-detail/seed.sql");
const CASES_JSON: &str =
    include_str!("../../../../../rust/test-fixtures/fixtures/workspace-detail/cases.json");

async fn seed(path: &std::path::Path) {
    let mut db = current_schema::create_current_schema(path).await;
    db.execute(sqlx::raw_sql(SEED_SQL))
        .await
        .expect("shared workspace-detail seed must load against the current schema");
    db.close().await.unwrap();
}

#[tokio::test]
async fn the_real_detail_route_produces_the_shared_expected_responses() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("workspace-detail.sqlite");
    seed(&path).await;

    let read = SqliteWorkspaceDetailRead::open(&path).await.unwrap();
    let app: Router = workspace_detail::router(read);

    let fixture: Value = serde_json::from_str(CASES_JSON).unwrap();
    let cases = fixture["cases"].as_array().unwrap();
    assert!(!cases.is_empty(), "the fixture must carry cases");

    // The route template is `/api/workspaces/{id}`; the served path substitutes the
    // fixture id into that one capture segment.
    let path_prefix = WORKSPACE_DETAIL_PATH.strip_suffix("{id}").unwrap();

    for case in cases {
        let name = case["name"].as_str().unwrap();
        let id = case["id"].as_str().unwrap();
        let expected_status = case["expected"]["status"].as_u64().unwrap() as u16;
        let expected_body = &case["expected"]["body"];

        let request = Request::builder()
            .uri(format!("{path_prefix}{id}"))
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
