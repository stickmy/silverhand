//! Composed proof for the Rust `GET /api/hosts/{id}/resources` vertical
//! (#task:2198, batch 1).
//!
//! The per-branch matrix is already pinned by the unit tests of the three layers
//! this composes: `capability::computer_access` owns the ownership arms,
//! `host_resources_read` owns the coordinate bind and the refusal fold, the SQLite
//! adapter owns the ordering, the per-computer scoping and the strict decode, and
//! the route's own `tests` module owns the status/body mapping against a fake read
//! model. What no unit test can show is that those layers WIRE UP end to end: that
//! the real `SqliteHostResourcesColdRead` — its two statements on one deferred
//! snapshot, both bound to the same id — feeds the real `host_resources::router`
//! and yields the expected payload and JSON envelope.
//!
//! So this is a single LOAD-BEARING composition proof, not a differential and not a
//! matrix: two vectors driven through the real stack against a fresh current-schema
//! database — one admitted owner who must receive the full list, and one registered
//! stranger who receives the owner-scoped 404 payload. Byte equivalence across
//! refusal cases is tested by the route and mounted composition keepers. The
//! expected values are the TS route's (`routes/hosts.ts`, `read-model/
//! host-resources.ts`), read at authoring; the Rust vertical is now their sole
//! consumer.
//!
//! The route is authenticated, so each request injects the `UserId` extension the
//! composition perimeter (#task:2197) would attach; the auth-refusal body is that
//! perimeter's, not this test's. The router built here is unmounted by design.

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use silverhand_backend_core::foundation::identity::UserId;
use silverhand_backend_server::host_runtime::host_resources::router;
use silverhand_backend_sqlite::host_runtime::host_resources_read::SqliteHostResourcesColdRead;
use sqlx::{Connection, Executor};
use tower::ServiceExt;

use crate::current_schema;

/// Two users, and one computer owned by the first. Shared by both vectors so the
/// admitted and refused answers differ only in WHO is asking.
const SEED: &str = "\
    INSERT INTO users (id, name, display_name, created_at, updated_at) \
    VALUES ('u-owner', 'owner', 'Owner', 1000, 1000), \
           ('u-stranger', 'stranger', 'Stranger', 1000, 1000); \
    INSERT INTO hosts (id, name, kind, created_at, user_id) \
    VALUES ('h1', 'host-one', 'desktop', 1000, 'u-owner'); \
    INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, display_name, \
      created_by_user_id, materialization_status) \
    VALUES ('a-late', 'late', 'codex', 'h1', 3000, 3000, 'Late One', 'u-owner', 'failed'), \
           ('a-early', 'early', 'qoder', 'h1', 1000, 1000, 'Early One', 'u-owner', 'active');";

/// Materialize a fresh current-schema database, load [`SEED`], then drive the real
/// adapter and router for `viewer` asking about `host_id`. The `application/json`
/// envelope is asserted here because both the admit and the refuse path carry it.
async fn run(viewer: &str, host_id: &str) -> (StatusCode, Value) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("host-resources-contract.sqlite");
    let mut db = current_schema::create_current_schema(&path).await;
    db.execute(sqlx::raw_sql(SEED))
        .await
        .expect("host-resources seed must load against the current schema");
    db.close().await.unwrap();

    let cold_read = SqliteHostResourcesColdRead::open(&path).await.unwrap();
    let app: Router = router(cold_read);

    let request = Request::builder()
        .uri(format!("/api/hosts/{host_id}/resources"))
        .extension(UserId::parse(viewer).unwrap())
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();

    let status = response.status();
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("application/json"),
    );
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value: Value = serde_json::from_slice(&body).unwrap();
    (status, value)
}

#[tokio::test]
async fn the_owner_reads_every_agent_rooted_here_through_the_real_stack() {
    // The seed inserts `a-late` FIRST and gives it the later `created_at`, so a list
    // that came back in insertion or rowid order would fail here: the adapter's
    // `ORDER BY created_at ASC` is what puts `a-early` in front. `a-late` also sits
    // at the `failed` stage — this is a registration view, so no stage predicate may
    // hide it, unlike the agent-detail surface.
    let (status, body) = run("u-owner", "h1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({
            "agents": [
                {
                    "id": "a-early",
                    "name": "early",
                    "display_name": "Early One",
                    "runtime": "qoder"
                },
                {
                    "id": "a-late",
                    "name": "late",
                    "display_name": "Late One",
                    "runtime": "codex"
                }
            ]
        })
    );
}

#[tokio::test]
async fn a_registered_stranger_is_the_owner_scoped_404() {
    // A real, authenticated user who simply does not own this computer must be
    // indistinguishable from one asking about a computer that does not exist, so the
    // surface is never an existence oracle for host ids.
    let (status, body) = run("u-stranger", "h1").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        body,
        json!({ "error": "host not found", "error_code": "host_not_found" })
    );
}
