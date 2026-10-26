//! Composed proof for the Rust `GET /api/agents/{id}` agent-detail vertical
//! (#task:2195, batch 3).
//!
//! The per-branch matrix — every ACL arm, each hidden vs. revealed stage, the flag
//! tokens, the source-bind and missing-principal faults — is already pinned by the
//! unit tests of the three layers this composes: `capability::agent_view`,
//! `agent_detail_read`, and the route's own `tests` module against a fake read
//! model. What no unit test can show is that those layers WIRE UP end to end: that
//! the real SQLite `SqliteAgentDetailColdRead` adapter — its agent row and host
//! `LEFT JOIN` plus its two set-shaped ACL arms on one deferred snapshot, its strict
//! decode — feeds the real `identity_access::agent_detail::router` and yields the exact
//! wire bytes.
//!
//! So this is a single LOAD-BEARING composition proof, not a differential and not a
//! matrix: two vectors driven through the real stack against a fresh current-schema
//! database — one admitted creator who must receive the full DTO, and one registered
//! stranger whom the anti-enumeration ACL must collapse to
//! `404 {"error":"agent not found"}`. The expected values were authored against the
//! real TS route's behavior and ratified at authoring; the Rust vertical is now
//! their sole consumer.
//!
//! The route is authenticated, so each request injects the `UserId` extension the
//! composition perimeter (#task:2197) would attach; the auth-refusal body is that
//! perimeter's, not this test's.

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use silverhand_backend_core::foundation::identity::UserId;
use silverhand_backend_server::identity_access::agent_detail::router;
use silverhand_backend_sqlite::identity_access::agent_detail_read::SqliteAgentDetailColdRead;
use sqlx::{Connection, Executor};
use tower::ServiceExt;

use crate::current_schema;

/// Materialize a fresh current-schema database, run `seed_sql` verbatim against it,
/// then drive the real adapter and router for `viewer` asking about `agent_id`.
/// Returns the wire status and parsed JSON body; the `application/json` envelope is
/// asserted here because both the admit and the refuse paths must carry it.
async fn run(seed_sql: &'static str, viewer: &str, agent_id: &str) -> (StatusCode, Value) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("agent-detail-contract.sqlite");
    let mut db = current_schema::create_current_schema(&path).await;
    db.execute(sqlx::raw_sql(seed_sql))
        .await
        .expect("agent-detail seed must load against the current schema");
    db.close().await.unwrap();

    let cold_read = SqliteAgentDetailColdRead::open(&path).await.unwrap();
    let app: Router = router(cold_read);

    // The authenticated principal is the composition perimeter's contribution; this
    // test isolates the production projector, so it injects the viewer fact the auth
    // layer would otherwise attach.
    let request = Request::builder()
        .uri(format!("/api/agents/{agent_id}"))
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
async fn an_admitted_creator_reads_the_full_dto_through_the_real_stack() {
    // Every optional column carries a value, so the projection resolves in full and
    // both derived fields take their non-null arm: `display_label` -> `host_label`,
    // and the avatar key -> a cache-busted `avatar_url` stamped with `updated_at`.
    // `session_scope` is unset in the seed, so the wire value is the schema default —
    // proving that default flows through the real read, not a hand-set literal.
    let seed = "\
        INSERT INTO users (id, name, display_name, created_at, updated_at) \
        VALUES ('u-creator', 'creator', 'Creator', 1000, 1000); \
        INSERT INTO hosts (id, name, kind, created_at, display_label) \
        VALUES ('h1', 'host-one', 'desktop', 1000, 'Prod Box'); \
        INSERT INTO agents (id, name, runtime, model, host_id, description, created_at, updated_at, \
          display_name, reasoning_effort, fast_mode, created_by_user_id, \
          avatar_storage_key, avatar_content_type, materialization_status) \
        VALUES ('a1', 'agent-one', 'qoder', 'opus', 'h1', 'the first agent', 1000, 2000, \
          'Agent One', 'high', 1, 'u-creator', \
          'blob-key', 'image/png', 'active');";

    let (status, body) = run(seed, "u-creator", "a1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({
            "agent": {
                "id": "a1",
                "name": "agent-one",
                "display_name": "Agent One",
                "runtime": "qoder",
                "model": "opus",
                "reasoning_effort": "high",
                "fast_mode": true,
                "host_id": "h1",
                "host_label": "Prod Box",
                "description": "the first agent",
                "session_scope": "conversation",
                "created_by_user_id": "u-creator",
                "avatar_url": "/api/agents/a1/avatar?v=2000",
                "materialization_status": "active",
                "materialization_failed_code": null,
                "materialization_failed_at": null,
                "deletion_started_at": null,
                "created_at": 1000,
                "updated_at": 2000
            }
        })
    );
}

#[tokio::test]
async fn a_registered_stranger_is_the_anti_enumeration_404() {
    // A human in NO arm — not the creator, not a direct-pair endpoint, not a member
    // of any workspace the agent reaches — must be indistinguishable from an absent
    // row: the same byte-identical 404, so the surface is never an existence oracle.
    let seed = "\
        INSERT INTO users (id, name, display_name, created_at, updated_at) \
        VALUES ('u-creator', 'creator', 'Creator', 1000, 1000); \
        INSERT INTO users (id, name, display_name, created_at, updated_at) \
        VALUES ('u-stranger', 'stranger', 'Stranger', 1000, 1000); \
        INSERT INTO hosts (id, name, kind, created_at) \
        VALUES ('h1', 'host-one', 'desktop', 1000); \
        INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, created_by_user_id) \
        VALUES ('a1', 'agent-one', 'qoder', 'h1', 1000, 1000, 'u-creator');";

    let (status, body) = run(seed, "u-stranger", "a1").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, json!({ "error": "agent not found" }));
}
