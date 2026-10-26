//! Composed proof for the Rust `GET /api/agents` list vertical (#task:2195, batch 5).
//!
//! This suite owns wire DTOs, creation order, ownership filtering and empty results.
//! The SQLite visibility matrix covers ACL/stage combinations; route unit tests
//! retain query parsing and injected source/principal faults. What no unit test can show
//! is that those layers WIRE UP end to end: that the real SQLite
//! `SqliteAgentListColdRead` — its `creator UNION relation` candidate SQL on one
//! deferred snapshot, its strict per-row decode, its shared detail projection — feeds
//! the real `identity_access::agent_list::router` and yields the exact wire bytes.
//!
//! Three vectors driven through the real stack against a fresh current-schema
//! database: the default view (own + workspace-reached agents, in `created_at` order,
//! as full DTOs), the `?scope=owned` narrowing (only the caller's own), and a
//! registered stranger the ACL collapses to an EMPTY array — a list face has no 404,
//! a hidden agent is simply absent.
//!
//! The route is authenticated, so each request injects the `UserId` extension the
//! composition perimeter (#task:2197) would attach.

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use silverhand_backend_core::foundation::identity::UserId;
use silverhand_backend_server::identity_access::agent_list::router;
use silverhand_backend_sqlite::identity_access::agent_list_read::SqliteAgentListColdRead;
use sqlx::{Connection, Executor};
use tower::ServiceExt;

use crate::current_schema;

/// A viewer who creates `a1` and reaches `a2` through workspace `w1`; `a2` is created
/// by someone else, so `?scope=owned` must drop it. `a1` and `a2` sit on different
/// hosts to exercise both `host_label` arms across rows.
const SEED: &str = "\
    INSERT INTO users (id, name, display_name, created_at, updated_at) \
    VALUES ('u-viewer', 'viewer', 'Viewer', 1, 1); \
    INSERT INTO users (id, name, display_name, created_at, updated_at) \
    VALUES ('u-other', 'other', 'Other', 1, 1); \
    INSERT INTO users (id, name, display_name, created_at, updated_at) \
    VALUES ('u-stranger', 'stranger', 'Stranger', 1, 1); \
    INSERT INTO hosts (id, name, kind, created_at, display_label) \
    VALUES ('h1', 'host-one', 'desktop', 1, 'Prod Box'); \
    INSERT INTO hosts (id, name, kind, created_at) \
    VALUES ('h2', 'host-two', 'desktop', 1); \
    INSERT INTO agents (id, name, runtime, model, host_id, description, created_at, updated_at, \
      display_name, reasoning_effort, fast_mode, created_by_user_id, \
      avatar_storage_key, avatar_content_type, materialization_status) \
    VALUES ('a1', 'agent-one', 'qoder', 'opus', 'h1', 'the first agent', 3000, 4000, \
      'Agent One', 'high', 1, 'u-viewer', 'blob-key', 'image/png', 'active'); \
    INSERT INTO agents (id, name, runtime, model, host_id, description, created_at, updated_at, \
      display_name, reasoning_effort, fast_mode, created_by_user_id, session_scope, \
      materialization_status) \
    VALUES ('a2', 'agent-two', 'codex', 'sonnet', 'h2', 'the second agent', 2000, 2100, \
      'Agent Two', 'low', 0, 'u-other', 'agent', 'active'); \
    INSERT INTO workspaces \
      (id, name, name_key, description, owner_agent_id, archived_at, created_at, updated_at) \
    VALUES ('w1', 'Workspace One', 'workspace-one', NULL, NULL, NULL, 1, 1); \
    INSERT INTO workspace_members (workspace_id, agent_id, joined_at) VALUES ('w1', 'a2', 1); \
    INSERT INTO workspace_users (workspace_id, user_id, joined_at) VALUES ('w1', 'u-viewer', 1);";

/// Materialize a fresh current-schema database, seed it, then drive the real adapter
/// and router for `viewer` with the given query string. The `application/json`
/// envelope is asserted here — every list answer is a JSON body.
async fn run(viewer: &str, query: &str) -> (StatusCode, Value) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("agent-list-contract.sqlite");
    let mut db = current_schema::create_current_schema(&path).await;
    db.execute(sqlx::raw_sql(SEED))
        .await
        .expect("agent-list seed must load against the current schema");
    db.close().await.unwrap();

    let cold_read = SqliteAgentListColdRead::open(&path).await.unwrap();
    let app: Router = router(cold_read);

    let request = Request::builder()
        .uri(format!("/api/agents{query}"))
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

fn agent_one() -> Value {
    json!({
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
        "created_by_user_id": "u-viewer",
        "avatar_url": "/api/agents/a1/avatar?v=4000",
        "materialization_status": "active",
        "materialization_failed_code": null,
        "materialization_failed_at": null,
        "deletion_started_at": null,
        "created_at": 3000,
        "updated_at": 4000
    })
}

fn agent_two() -> Value {
    json!({
        "id": "a2",
        "name": "agent-two",
        "display_name": "Agent Two",
        "runtime": "codex",
        "model": "sonnet",
        "reasoning_effort": "low",
        "fast_mode": false,
        "host_id": "h2",
        // No `display_label` on h2, so the label falls back to the host name.
        "host_label": "host-two",
        "description": "the second agent",
        "session_scope": "agent",
        "created_by_user_id": "u-other",
        "avatar_url": null,
        "materialization_status": "active",
        "materialization_failed_code": null,
        "materialization_failed_at": null,
        "deletion_started_at": null,
        "created_at": 2000,
        "updated_at": 2100
    })
}

#[tokio::test]
async fn the_default_view_lists_reachable_agents_in_created_at_order() {
    // a1 is the viewer's own (created_at 3000); a2 is reached through the shared
    // workspace (created_at 2000). Creation order opposes insertion and id order: a2 must come first.
    let (status, body) = run("u-viewer", "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "agents": [agent_two(), agent_one()] }));
}

#[tokio::test]
async fn scope_owned_narrows_to_the_callers_own_agents() {
    // The workspace-reached a2 is created by someone else, so `?scope=owned` drops it;
    // only a1 remains. The ACL still admits both — the narrowing is the scope filter's.
    let (status, body) = run("u-viewer", "?scope=owned").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "agents": [agent_one()] }));
}

#[tokio::test]
async fn a_registered_stranger_gets_an_empty_array() {
    // A human in no arm sees nothing — and a list face has no existence oracle to
    // protect, so the answer is a plain empty array, never a 404.
    let (status, body) = run("u-stranger", "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "agents": [] }));
}
