//! The runtime closed set is ONE vocabulary on both agent wire faces (#task:2207).
//!
//! `agents.runtime` carries no CHECK, so a token outside the set CAN be committed.
//! The dispatch, roster and direct-pair readers already refuse such a row. This pins
//! that the two PUBLIC runtime faces — `GET /api/agents` and `GET /api/agents/{id}` —
//! refuse it too, so one committed row cannot yield two runtime domains depending on
//! which door was used.
//!
//! Both faces are driven over the SAME seeded database through the real adapters and
//! the real routers, because that is the only place the claim can be observed: the
//! projection they share is decoded in `decode_agent_row`, and a per-face test would
//! prove nothing about the other face.
//!
//! The two seeds differ in the `runtime` column ALONE. Repairing that column is what
//! turns both faces green, so the gate cannot pass by refusing everything.

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use silverhand_backend_core::foundation::identity::UserId;
use silverhand_backend_server::identity_access::{agent_detail, agent_list};
use silverhand_backend_sqlite::{
    identity_access::agent_detail_read::SqliteAgentDetailColdRead,
    identity_access::agent_list_read::SqliteAgentListColdRead,
};
use sqlx::{Connection, Executor};
use tower::ServiceExt;

use crate::current_schema;

/// The viewer creates `a1`, so the ACL admits it on both faces and any refusal is the
/// decode's, never the gate's.
const PRINCIPALS: &str = "\
    INSERT INTO users (id, name, display_name, created_at, updated_at) \
    VALUES ('u-creator', 'creator', 'Creator', 1000, 1000); \
    INSERT INTO hosts (id, name, kind, created_at) \
    VALUES ('h1', 'host-one', 'desktop', 1000);";

/// A committed row whose runtime is outside the closed set. Nothing in the schema
/// bars it, which is why it is a read-side invariant.
const AGENT_WITH_UNKNOWN_RUNTIME: &str = "\
    INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, created_by_user_id, \
      display_name, materialization_status) \
    VALUES ('a1', 'agent-one', 'future-runtime', 'h1', 1000, 2000, 'u-creator', \
      'Agent One', 'active');";

/// The same row with the runtime column — and only it — repaired.
const AGENT_WITH_KNOWN_RUNTIME: &str = "\
    INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, created_by_user_id, \
      display_name, materialization_status) \
    VALUES ('a1', 'agent-one', 'qoder', 'h1', 1000, 2000, 'u-creator', \
      'Agent One', 'active');";

/// One fresh current-schema database seeded with `agent_row`, then asked the same
/// question through BOTH real faces: `(list, detail)` as `(status, body text)`.
///
/// The body is kept as text, not parsed, because a refusal is the framework's bare
/// `text/plain` `Internal Server Error` while an answer is JSON — and because the
/// assertion that the offending token never reached the wire has to see the bytes.
async fn both_faces(agent_row: &'static str) -> ((StatusCode, String), (StatusCode, String)) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("agent-runtime-vocabulary.sqlite");
    let mut db = current_schema::create_current_schema(&path).await;
    db.execute(sqlx::raw_sql(PRINCIPALS))
        .await
        .expect("principals must load against the current schema");
    db.execute(sqlx::raw_sql(agent_row))
        .await
        .expect("the agent row must COMMIT — an out-of-set runtime is not barred by the schema");
    db.close().await.unwrap();

    let list: Router = agent_list::router(SqliteAgentListColdRead::open(&path).await.unwrap());
    let detail: Router =
        agent_detail::router(SqliteAgentDetailColdRead::open(&path).await.unwrap());

    (
        ask(list, "/api/agents").await,
        ask(detail, "/api/agents/a1").await,
    )
}

/// Drive one face for the admitted creator. The authenticated principal is the
/// composition perimeter's contribution, injected here as the auth layer would.
async fn ask(app: Router, uri: &str) -> (StatusCode, String) {
    let request = Request::builder()
        .uri(uri)
        .extension(UserId::parse("u-creator").unwrap())
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (
        status,
        String::from_utf8(body.to_vec()).expect("utf-8 body"),
    )
}

#[tokio::test]
async fn an_out_of_set_runtime_refuses_on_both_faces_instead_of_reaching_the_wire() {
    let ((list_status, list_body), (detail_status, detail_body)) =
        both_faces(AGENT_WITH_UNKNOWN_RUNTIME).await;

    // A corrupt committed row is a safe 500 on BOTH faces: the shared decode faults
    // before any DTO exists, so neither face can answer 200 with a token the dispatch
    // and roster readers refuse. Asserted as ONE pair, so a mutation reports what both
    // faces did rather than stopping at the first.
    assert_eq!(
        (list_status, detail_status),
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::INTERNAL_SERVER_ERROR
        ),
        "list answered {list_status} with {list_body}; detail answered {detail_status} with {detail_body}"
    );
    // And the offending value is nowhere on the wire — not in a DTO, not in an error
    // envelope. The value belongs in the log the adapter's error carries.
    assert!(!list_body.contains("future-runtime"));
    assert!(!detail_body.contains("future-runtime"));
}

#[tokio::test]
async fn repairing_the_runtime_column_alone_makes_both_faces_answer_the_same_token() {
    let ((list_status, list_body), (detail_status, detail_body)) =
        both_faces(AGENT_WITH_KNOWN_RUNTIME).await;

    assert_eq!(list_status, StatusCode::OK);
    assert_eq!(detail_status, StatusCode::OK);

    let list: Value = serde_json::from_str(&list_body).unwrap();
    let detail: Value = serde_json::from_str(&detail_body).unwrap();
    // One row, one runtime: the value-bearing assertion, so a wrong JSON path could
    // not make this vacuous.
    assert_eq!(list["agents"][0]["runtime"], json!("qoder"));
    assert_eq!(detail["agent"]["runtime"], json!("qoder"));
    assert_eq!(list["agents"][0], detail["agent"]);
}
