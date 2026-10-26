//! Real-SQLite gates for the product schedule LIST read.
//!
//! Drives `SqliteScheduleListRead` against a database built from the checked-in
//! current-schema snapshot: the owner scope, the status-family filter, the
//! `next_fire_at ASC, created_at ASC` ordering, the `1..=200` limit, the
//! empty-family short-circuit, and the `target_label` LEFT JOIN (resolved board
//! vs deleted target) over real rows.

use silverhand_backend_core::conversation_messaging::messaging::schedule::{
    ScheduleListLimit, ScheduleListQuery, ScheduleListReadModel, ScheduleStatus, ScheduleView,
};
use silverhand_backend_core::foundation::identity::AgentId;
use silverhand_backend_sqlite::conversation_messaging::messaging::schedule::SqliteScheduleListRead;
use sqlx::{Connection, SqliteConnection};
use std::path::PathBuf;
use tempfile::TempDir;

use crate::current_schema;

const OWNER: &str = "agent-sched";
const OTHER: &str = "agent-other";
const SPEC_ONCE: &str = r#"{"kind":"once","fire_at":100000}"#;

async fn open_db() -> (TempDir, PathBuf, SqliteConnection) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("schedules.sqlite");
    let mut connection = current_schema::create_current_schema(&path).await;
    seed_agents(&mut connection).await;
    (directory, path, connection)
}

async fn seed_agents(connection: &mut SqliteConnection) {
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
    for id in [OWNER, OTHER] {
        sqlx::query(
            "INSERT INTO agents (
                id, name, runtime, host_id, created_at, updated_at,
                display_name, created_by_user_id, session_scope
             ) VALUES (?, ?, 'codex', 'host-1', 1, 1, 'Agent', 'user-1', 'conversation')",
        )
        .bind(id)
        .bind(id)
        .execute(&mut *connection)
        .await
        .unwrap();
    }
}

/// A schedule row with a chosen owner, target, status, `next_fire_at`, and
/// `created_at` — the columns the filter and ordering read.
#[allow(clippy::too_many_arguments)]
async fn seed_schedule(
    connection: &mut SqliteConnection,
    id: &str,
    owner: &str,
    target: &str,
    status: &str,
    next_fire_at: i64,
    created_at: i64,
) {
    sqlx::query(
        "INSERT INTO schedules (
            id, owner_agent_id, target_conversation_id, title, message, spec_json,
            next_fire_at, status, fired_count, max_occurrences, until_at,
            consecutive_failure_count, created_at, updated_at
         ) VALUES (?, ?, ?, 'digest', 'hi', ?, ?, ?, 0, NULL, NULL, 0, ?, ?)",
    )
    .bind(id)
    .bind(owner)
    .bind(target)
    .bind(SPEC_ONCE)
    .bind(next_fire_at)
    .bind(status)
    .bind(created_at)
    .bind(created_at)
    .execute(&mut *connection)
    .await
    .unwrap();
}

fn owner_agent() -> AgentId {
    AgentId::parse(OWNER).unwrap()
}

fn ids(views: &[ScheduleView]) -> Vec<String> {
    views.iter().map(|v| v.id.as_str().to_owned()).collect()
}

async fn list_reader(path: &std::path::Path) -> SqliteScheduleListRead {
    SqliteScheduleListRead::open(path).await.unwrap()
}

fn query(statuses: Option<Vec<ScheduleStatus>>, limit: ScheduleListLimit) -> ScheduleListQuery {
    ScheduleListQuery {
        owner_agent_id: owner_agent(),
        statuses,
        limit,
    }
}

#[tokio::test]
async fn only_the_owner_rows_are_listed_ordered_by_fire_then_created() {
    let (_dir, path, mut connection) = open_db().await;
    // Out-of-insert-order fire instants; a tie on next_fire_at breaks by created_at.
    seed_schedule(&mut connection, "s-late", OWNER, "gone", "pending", 300, 1).await;
    seed_schedule(&mut connection, "s-early", OWNER, "gone", "pending", 100, 1).await;
    seed_schedule(
        &mut connection,
        "s-tie-b",
        OWNER,
        "gone",
        "pending",
        200,
        20,
    )
    .await;
    seed_schedule(
        &mut connection,
        "s-tie-a",
        OWNER,
        "gone",
        "pending",
        200,
        10,
    )
    .await;
    // A different owner's row must never appear.
    seed_schedule(&mut connection, "s-other", OTHER, "gone", "pending", 50, 1).await;
    connection.close().await.unwrap();

    let reader = list_reader(&path).await;
    let views = reader
        .list_owned_schedules(&query(None, ScheduleListLimit::DEFAULT))
        .await
        .unwrap();
    assert_eq!(ids(&views), ["s-early", "s-tie-a", "s-tie-b", "s-late"]);
}

#[tokio::test]
async fn the_status_family_filters_the_rows() {
    let (_dir, path, mut connection) = open_db().await;
    seed_schedule(
        &mut connection,
        "s-pending",
        OWNER,
        "gone",
        "pending",
        100,
        1,
    )
    .await;
    seed_schedule(&mut connection, "s-firing", OWNER, "gone", "firing", 200, 1).await;
    seed_schedule(&mut connection, "s-done", OWNER, "gone", "done", 300, 1).await;
    seed_schedule(
        &mut connection,
        "s-cancelled",
        OWNER,
        "gone",
        "cancelled",
        400,
        1,
    )
    .await;
    connection.close().await.unwrap();

    let reader = list_reader(&path).await;

    // `active` = pending|firing.
    let active = reader
        .list_owned_schedules(&query(
            Some(vec![ScheduleStatus::Pending, ScheduleStatus::Firing]),
            ScheduleListLimit::DEFAULT,
        ))
        .await
        .unwrap();
    assert_eq!(ids(&active), ["s-pending", "s-firing"]);

    // A single concrete status.
    let done = reader
        .list_owned_schedules(&query(
            Some(vec![ScheduleStatus::Done]),
            ScheduleListLimit::DEFAULT,
        ))
        .await
        .unwrap();
    assert_eq!(ids(&done), ["s-done"]);
}

#[tokio::test]
async fn an_empty_status_family_short_circuits_to_no_rows() {
    let (_dir, path, mut connection) = open_db().await;
    seed_schedule(&mut connection, "s1", OWNER, "gone", "pending", 100, 1).await;
    connection.close().await.unwrap();

    let reader = list_reader(&path).await;
    // A `Some(empty)` family constrains to nothing — the read must not run an
    // `IN ()` that would error, nor drop the constraint and return everything.
    let views = reader
        .list_owned_schedules(&query(Some(vec![]), ScheduleListLimit::DEFAULT))
        .await
        .unwrap();
    assert!(views.is_empty());
}

#[tokio::test]
async fn the_limit_caps_the_row_count() {
    let (_dir, path, mut connection) = open_db().await;
    for index in 0..5 {
        seed_schedule(
            &mut connection,
            &format!("s{index}"),
            OWNER,
            "gone",
            "pending",
            100 + index,
            1,
        )
        .await;
    }
    connection.close().await.unwrap();

    let reader = list_reader(&path).await;
    let views = reader
        .list_owned_schedules(&query(None, ScheduleListLimit::parse(2).unwrap()))
        .await
        .unwrap();
    assert_eq!(ids(&views), ["s0", "s1"]);
}

#[tokio::test]
async fn the_target_label_reflects_the_joined_board_or_falls_back() {
    let (_dir, path, mut connection) = open_db().await;
    // A real workspace + open board so the LEFT JOIN resolves title/roster.
    sqlx::query(
        "INSERT INTO workspaces (id, name, name_key, owner_user_id, created_at, updated_at)
         VALUES ('ws-1', 'Team', 'ws-1', 'user-1', 1, 1)",
    )
    .execute(&mut connection)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversations
            (id, workspace_id, placement_kind, roster_policy, title, title_key, status,
             created_at, updated_at)
         VALUES ('c-open', 'ws-1', 'workspace', 'open', 'ops', 'ops', 'active', 1, 1)",
    )
    .execute(&mut connection)
    .await
    .unwrap();
    seed_schedule(
        &mut connection,
        "s-resolved",
        OWNER,
        "c-open",
        "pending",
        100,
        1,
    )
    .await;
    seed_schedule(
        &mut connection,
        "s-orphan",
        OWNER,
        "deleted-target",
        "pending",
        200,
        1,
    )
    .await;
    connection.close().await.unwrap();

    let reader = list_reader(&path).await;
    let views = reader
        .list_owned_schedules(&query(None, ScheduleListLimit::DEFAULT))
        .await
        .unwrap();
    let by_id = |id: &str| views.iter().find(|v| v.id.as_str() == id).unwrap();
    assert_eq!(by_id("s-resolved").target_label, "#ops");
    assert_eq!(by_id("s-orphan").target_label, "Unavailable conversation");
}
