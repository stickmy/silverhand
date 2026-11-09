//! End-to-end audit-line falsifier for the schedule-management cancel face.
//!
//! The success audit line `agent schedule cancelled by user` must be emitted
//! EXACTLY ONCE on successful cancellation and NEVER on a refusal or a
//! CAS-miss. This wires the real `schedule_cancel_router` (a fake agent-detail
//! gate plus a real `SqliteScheduleCancel` over the checked-in current schema)
//! and counts the line in a captured operator log across three cases: a committed
//! cancel, a terminal CAS-miss, and a gate 404.

#[macro_use]
#[path = "../../backend-sqlite/tests/support/seed_rows.rs"]
mod seed_rows;

use std::sync::OnceLock;

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::Value;
use silverhand_backend_core::{
    foundation::identity::{AgentId, UserId},
    foundation::persistence::PersistenceFailure,
    identity_access::agent_detail_read::{
        AgentDetailFacts, AgentDetailProjection, AgentDetailReadModel,
    },
    identity_access::agent_lifecycle::MaterializationStatus,
    identity_access::agents::runtime::RuntimeKind,
};
use silverhand_backend_server::scheduling::schedule_management::schedule_cancel_router;
use silverhand_backend_sqlite::conversation_messaging::messaging::schedule::SqliteScheduleCancel;
use sqlx::SqliteConnection;
use tower::ServiceExt;

#[path = "../../backend-sqlite/tests/support/current_schema_snapshot.rs"]
mod current_schema;
#[path = "support/log_capture.rs"]
mod log_capture;

const AUDIT: &str = "agent schedule cancelled by user";
const AGENT: &str = "a1";

// ---- a fake gate that admits `viewer-1` as the creator of agent `a1` ----

#[derive(Clone)]
struct FakeDetail {
    present: bool,
}

impl AgentDetailReadModel for FakeDetail {
    async fn read_agent_detail(
        &self,
        _agent: &AgentId,
    ) -> Result<Option<AgentDetailFacts>, PersistenceFailure> {
        if !self.present {
            return Ok(None);
        }
        Ok(Some(AgentDetailFacts::from_source(
            AgentId::parse(AGENT).unwrap(),
            UserId::parse("viewer-1").unwrap(),
            [],
            [],
            MaterializationStatus::Active,
            projection(),
        )))
    }
}

fn projection() -> AgentDetailProjection {
    AgentDetailProjection {
        name: "agent-one".to_owned(),
        display_name: "Base Name".to_owned(),
        runtime: RuntimeKind::Codex,
        model: None,
        reasoning_effort: None,
        fast_mode: None,
        host_id: "host-1".to_owned(),
        host_display_label: None,
        host_name: None,
        description: None,
        session_scope: "agent".to_owned(),
        avatar_storage_key: None,
        materialization_failed_code: None,
        materialization_failed_at: None,
        deletion_started_at: None,
        created_at: 1000,
        updated_at: 2000,
    }
}

// ---- the operator log, installed process-wide (see product_send_e2e) ----

fn operator_log() -> &'static log_capture::LogBuffer {
    static SINK: OnceLock<log_capture::LogBuffer> = OnceLock::new();
    SINK.get_or_init(|| {
        let logs = log_capture::LogBuffer::default();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .without_time()
            .with_target(false)
            .with_writer(logs.clone())
            .finish();
        tracing::subscriber::set_global_default(subscriber)
            .expect("this suite is the only thing installing a subscriber");
        logs
    })
}

/// Count the audit lines naming `schedule_id` emitted while `request` ran. The
/// count is scoped to the schedule id (a per-test unique) because the
/// process-wide sink also holds the OTHER cells' events under parallel runs.
async fn observe(router: Router, request: Request<Body>, schedule_id: &str) -> (StatusCode, usize) {
    let sink = operator_log();
    let before = sink.events().len();
    let response = router.oneshot(request).await.unwrap();
    let status = response.status();
    let _ = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let audit_lines = sink
        .events()
        .into_iter()
        .skip(before)
        .filter(|event| line_is_audit(event, schedule_id))
        .count();
    (status, audit_lines)
}

/// The JSON-formatted event carries the log message and the structured fields
/// under `fields`; the audit line is the one whose message is [`AUDIT`] and
/// whose `schedule_id` field is this cell's schedule.
fn line_is_audit(event: &Value, schedule_id: &str) -> bool {
    event["fields"]["message"] == AUDIT && event["fields"]["schedule_id"] == schedule_id
}

// ---- database seed ----

async fn seed_agent(connection: &mut SqliteConnection) {
    sqlx::query(
        "INSERT INTO users (id, name, display_name, created_at, updated_at)
         VALUES ('user-1', 'user-1', 'User One', 1, 1)",
    )
    .execute(&mut *connection)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO hosts (id, name, kind, created_at, user_id)
         VALUES ('host-1', 'host-1', 'remote', 1, 'user-1')",
    )
    .execute(&mut *connection)
    .await
    .unwrap();
    sqlx::query(seed_agent!(
        id = ?,
        name = "a1",
        runtime = "codex",
        host_id = "host-1",
        display_name = "Agent",
        created_by_user_id = "user-1",
    ))
    .bind(AGENT)
    .execute(&mut *connection)
    .await
    .unwrap();
}

async fn seed_schedule(connection: &mut SqliteConnection, id: &str, status: &str) {
    sqlx::query(
        "INSERT INTO schedules (
            id, owner_agent_id, target_conversation_id, title, message, spec_json,
            next_fire_at, status, fired_count, max_occurrences, until_at,
            consecutive_failure_count, created_at, updated_at
         ) VALUES (?, ?, 'gone', 'digest', 'hi', ?, 100000, ?, 0, NULL, NULL, 0, 1, 1)",
    )
    .bind(id)
    .bind(AGENT)
    .bind(r#"{"kind":"once","fire_at":100000}"#)
    .bind(status)
    .execute(&mut *connection)
    .await
    .unwrap();
}

async fn cancel_router_for(
    schedules: &[(&str, &str)],
    present_agent: bool,
) -> (tempfile::TempDir, Router) {
    let directory = tempfile::tempdir().unwrap();
    // The caller retains the directory until the request and its pool are done.
    let path = directory.path().join("schedules.sqlite");
    let mut connection = current_schema::create_current_schema(&path).await;
    seed_agent(&mut connection).await;
    for (id, status) in schedules {
        seed_schedule(&mut connection, id, status).await;
    }
    sqlx::Connection::close(connection).await.unwrap();
    let cancel = SqliteScheduleCancel::open(&path).await.unwrap();
    let router = schedule_cancel_router(
        FakeDetail {
            present: present_agent,
        },
        cancel,
    );
    (directory, router)
}

fn cancel_request(schedule_id: &str) -> Request<Body> {
    Request::post(format!(
        "/api/agents/{AGENT}/schedules/{schedule_id}/cancel"
    ))
    .extension(UserId::parse("viewer-1").unwrap())
    .body(Body::empty())
    .unwrap()
}

#[tokio::test]
async fn a_committed_cancel_emits_the_audit_line_exactly_once() {
    let (_directory, router) = cancel_router_for(&[("s-live-ok", "pending")], true).await;
    let (status, audit_lines) = observe(router, cancel_request("s-live-ok"), "s-live-ok").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(audit_lines, 1, "a 200 cancel must audit exactly once");
}

#[tokio::test]
async fn a_terminal_cas_miss_emits_no_audit_line() {
    let (_directory, router) = cancel_router_for(&[("s-done", "done")], true).await;
    let (status, audit_lines) = observe(router, cancel_request("s-done"), "s-done").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(audit_lines, 0, "a CAS-miss must never audit a cancellation");
}

#[tokio::test]
async fn a_gate_refusal_emits_no_audit_line() {
    // The agent is invisible → the anti-enum 404 before the unit of work runs.
    let (_directory, router) = cancel_router_for(&[], false).await;
    let (status, audit_lines) = observe(router, cancel_request("s-ghost"), "s-ghost").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(audit_lines, 0, "a refused request must never audit");
}
