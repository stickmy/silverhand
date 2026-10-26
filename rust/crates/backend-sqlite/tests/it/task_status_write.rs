//! Real-SQLite gates for the task `status` write unit-of-work.
//!
//! Drives `SqliteTaskStatusWrite::apply_agent_status` against a database built
//! from the checked-in current schema. Covers actual transitions, close stamps,
//! assignment/participation refusals, a deleted peer, and the human override.
//! Revocations here complete before the write call; lock timing is exercised
//! separately by task_write_clock.rs.

use silverhand_backend_core::conversation_messaging::messaging::task::{
    AgentEventNames, AgentStatusRefusal, AgentStatusWriteOutcome, StatusTransitionRefusal, TaskId,
    TaskStatus, TaskStatusWrite,
};
use silverhand_backend_core::foundation::identity::AgentId;
use silverhand_backend_sqlite::conversation_messaging::messaging::task::SqliteTaskStatusWrite;
use sqlx::{Connection, Row, SqliteConnection};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

use crate::current_schema;

const TASK: &str = "task-1";
const OWNER: &str = "agent-1";
const OTHER: &str = "agent-2";

async fn open_db(seed_status: &str, assignee_agent: Option<&str>) -> (TempDir, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("tasks.sqlite");
    let mut connection = current_schema::create_current_schema(&path).await;
    seed(&mut connection, seed_status, assignee_agent).await;
    connection.close().await.unwrap();
    (directory, path)
}

async fn seed(connection: &mut SqliteConnection, status: &str, assignee_agent: Option<&str>) {
    for statement in [
        "INSERT INTO users (id, name, display_name, created_at, updated_at)
         VALUES ('user-1', 'user-1', 'User One', 1, 1)",
        "INSERT INTO hosts (id, name, kind, created_at, user_id)
         VALUES ('host-1', 'host-1', 'remote', 1, 'user-1')",
        "INSERT INTO workspaces (id, name, name_key, created_at, updated_at)
         VALUES ('ws-1', 'ws', 'ws', 1, 1)",
        "INSERT INTO conversations
            (id, workspace_id, placement_kind, roster_policy, title, title_key, status, created_at, updated_at)
         VALUES ('conv-1', 'ws-1', 'workspace', 'open', 'general', 'general', 'active', 1, 1)",
    ] {
        sqlx::query(statement).execute(&mut *connection).await.unwrap();
    }
    for id in [OWNER, OTHER] {
        sqlx::query(
            "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at,
                display_name, created_by_user_id, session_scope)
             VALUES (?, ?, 'codex', 'host-1', 1, 1, 'Agent', 'user-1', 'conversation')",
        )
        .bind(id)
        .bind(id)
        .execute(&mut *connection)
        .await
        .unwrap();
        // Roster membership of the workspace conversation — participation.
        sqlx::query(
            "INSERT INTO conversation_agents (conversation_id, agent_id, joined_at)
             VALUES ('conv-1', ?, 1)",
        )
        .bind(id)
        .execute(&mut *connection)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO workspace_members (workspace_id, agent_id, joined_at)
             VALUES ('ws-1', ?, 1)",
        )
        .bind(id)
        .execute(&mut *connection)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO workspace_agent_capabilities (workspace_id, agent_id, capability)
             VALUES ('ws-1', ?, 'write_tasks')",
        )
        .bind(id)
        .execute(&mut *connection)
        .await
        .unwrap();
    }
    // An agent-created task; assignee optionally set (else unassigned).
    sqlx::query(
        "INSERT INTO tasks
            (id, workspace_id, task_number, conversation_id, title, status, assignee_agent_id,
             creator_kind, creator_agent_id, created_at, board_key)
         VALUES ('task-1', 'ws-1', 1, 'conv-1', 'Do it', ?, ?, 'agent', 'agent-1', 1, 'ws:ws-1')",
    )
    .bind(status)
    .bind(assignee_agent)
    .execute(&mut *connection)
    .await
    .unwrap();
}

async fn read_status(path: &Path, id: &str) -> (String, Option<i64>) {
    let mut connection = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
        .await
        .unwrap();
    let row = sqlx::query("SELECT status, closed_at FROM tasks WHERE id = ?")
        .bind(id)
        .fetch_one(&mut connection)
        .await
        .unwrap();
    (
        row.try_get::<String, _>("status").unwrap(),
        row.try_get::<Option<i64>, _>("closed_at").unwrap(),
    )
}

fn task_id() -> TaskId {
    TaskId::parse(TASK).unwrap()
}

fn owner() -> AgentId {
    AgentId::parse(OWNER).unwrap()
}

#[tokio::test]
async fn the_assignee_advancing_a_legal_transition_applies() {
    let (_dir, path) = open_db("in_progress", Some(OWNER)).await;
    let writer = SqliteTaskStatusWrite::open(&path).await.unwrap();

    let outcome = writer
        .apply_agent_status(
            &task_id(),
            &owner(),
            &AgentEventNames::new("actor", "Actor"),
            TaskStatus::InReview,
        )
        .await
        .unwrap();
    assert!(matches!(outcome, AgentStatusWriteOutcome::Applied(_)));

    let (status, closed_at) = read_status(&path, TASK).await;
    assert_eq!(status, "in_review");
    assert_eq!(closed_at, None);
}

#[tokio::test]
async fn a_close_transition_records_closed_at() {
    let (_dir, path) = open_db("in_progress", Some(OWNER)).await;
    let writer = SqliteTaskStatusWrite::open(&path).await.unwrap();

    let outcome = writer
        .apply_agent_status(
            &task_id(),
            &owner(),
            &AgentEventNames::new("actor", "Actor"),
            TaskStatus::Closed,
        )
        .await
        .unwrap();
    assert!(matches!(outcome, AgentStatusWriteOutcome::Applied(_)));

    let (status, closed_at) = read_status(&path, TASK).await;
    assert_eq!(status, "closed");
    assert!(closed_at.is_some_and(|timestamp| timestamp > 1));
}

#[tokio::test]
async fn the_creator_of_an_unassigned_self_task_may_advance_it() {
    // Unassigned, creator = OWNER: the creator-self-stale branch authorizes.
    let (_dir, path) = open_db("in_progress", None).await;
    let writer = SqliteTaskStatusWrite::open(&path).await.unwrap();

    let outcome = writer
        .apply_agent_status(
            &task_id(),
            &owner(),
            &AgentEventNames::new("actor", "Actor"),
            TaskStatus::Done,
        )
        .await
        .unwrap();
    assert!(matches!(outcome, AgentStatusWriteOutcome::Applied(_)));
    assert_eq!(read_status(&path, TASK).await.0, "done");
}

#[tokio::test]
async fn a_task_assigned_to_another_agent_refuses_its_creator() {
    // Assigned to OTHER; OWNER is neither assignee nor (since assigned) the
    // unassigned-creator: NotAssignee, no write.
    let (_dir, path) = open_db("in_progress", Some(OTHER)).await;
    let writer = SqliteTaskStatusWrite::open(&path).await.unwrap();

    let outcome = writer
        .apply_agent_status(
            &task_id(),
            &owner(),
            &AgentEventNames::new("actor", "Actor"),
            TaskStatus::Done,
        )
        .await
        .unwrap();
    assert_eq!(
        outcome,
        AgentStatusWriteOutcome::Refused(AgentStatusRefusal::NotAssignee)
    );
    assert_eq!(read_status(&path, TASK).await.0, "in_progress");
}

#[tokio::test]
async fn an_illegal_transition_is_refused() {
    // todo -> in_progress is NOT a status-verb edge (that is claim's); refused.
    let (_dir, path) = open_db("todo", Some(OWNER)).await;
    let writer = SqliteTaskStatusWrite::open(&path).await.unwrap();

    let outcome = writer
        .apply_agent_status(
            &task_id(),
            &owner(),
            &AgentEventNames::new("actor", "Actor"),
            TaskStatus::InProgress,
        )
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        AgentStatusWriteOutcome::Refused(AgentStatusRefusal::Transition(
            StatusTransitionRefusal::InvalidTransition { .. }
        ))
    ));
    assert_eq!(read_status(&path, TASK).await.0, "todo");
}

#[tokio::test]
async fn a_missing_task_is_not_found() {
    let (_dir, path) = open_db("in_progress", Some(OWNER)).await;
    let writer = SqliteTaskStatusWrite::open(&path).await.unwrap();
    let ghost = TaskId::parse("task-missing").unwrap();
    let outcome = writer
        .apply_agent_status(
            &ghost,
            &owner(),
            &AgentEventNames::new("actor", "Actor"),
            TaskStatus::Done,
        )
        .await
        .unwrap();
    assert_eq!(outcome, AgentStatusWriteOutcome::NotFound);
}

/// Removed conversation membership is a uniform NotFound with no status write.
#[tokio::test]
async fn an_agent_without_conversation_membership_is_notfound_with_zero_status_write() {
    let (_dir, path) = open_db("in_progress", Some(OWNER)).await;

    // Remove the OWNER's conversation roster row before calling the writer.
    {
        let mut connection = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        sqlx::query(
            "DELETE FROM conversation_agents WHERE conversation_id = 'conv-1' AND agent_id = ?",
        )
        .bind(OWNER)
        .execute(&mut connection)
        .await
        .unwrap();
    }

    let writer = SqliteTaskStatusWrite::open(&path).await.unwrap();
    let outcome = writer
        .apply_agent_status(
            &task_id(),
            &owner(),
            &AgentEventNames::new("actor", "Actor"),
            TaskStatus::InReview,
        )
        .await
        .unwrap();
    assert_eq!(
        outcome,
        AgentStatusWriteOutcome::NotFound,
        "a non-participant (membership since removed) is the uniform NotFound"
    );
    assert_eq!(
        read_status(&path, TASK).await.0,
        "in_progress",
        "zero status write on the participation miss"
    );
}

/// A participant still sees the deleted peer as read-only, not a membership miss.
#[tokio::test]
async fn a_deleted_direct_pair_peer_is_conversation_readonly_with_zero_status_write() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("tasks.sqlite");
    let mut connection = current_schema::create_current_schema(&path).await;
    // Base rows: user, host, both agents.
    for statement in [
        "INSERT INTO users (id, name, display_name, created_at, updated_at)
         VALUES ('user-1', 'user-1', 'User One', 1, 1)",
        "INSERT INTO hosts (id, name, kind, created_at, user_id)
         VALUES ('host-1', 'host-1', 'remote', 1, 'user-1')",
    ] {
        sqlx::query(statement)
            .execute(&mut connection)
            .await
            .unwrap();
    }
    for id in [OWNER, OTHER] {
        sqlx::query(
            "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at,
                display_name, created_by_user_id, session_scope)
             VALUES (?, ?, 'codex', 'host-1', 1, 1, 'Agent', 'user-1', 'conversation')",
        )
        .bind(id)
        .bind(id)
        .execute(&mut connection)
        .await
        .unwrap();
    }
    // A direct-pair conversation between OWNER and OTHER (no workspace, fixed roster).
    sqlx::query(
        "INSERT INTO conversations
            (id, placement_kind, roster_policy, title, status, created_at, updated_at,
             endpoint_a_kind, endpoint_a_id, endpoint_b_kind, endpoint_b_id)
         VALUES ('pair-1', 'direct_pair', 'fixed', '', 'active', 1, 1,
             'agent', ?, 'agent', ?)",
    )
    .bind(OWNER)
    .bind(OTHER)
    .execute(&mut connection)
    .await
    .unwrap();
    // The pair board key is `pair:` + the generated `pair_key` column; read it back
    // rather than recompute the generation grammar by hand.
    let pair_key: String =
        sqlx::query_scalar("SELECT pair_key FROM conversations WHERE id = 'pair-1'")
            .fetch_one(&mut connection)
            .await
            .unwrap();
    let board_key = format!("pair:{pair_key}");
    // A task on the pair, assigned to OWNER.
    sqlx::query(
        "INSERT INTO tasks
            (id, task_number, conversation_id, title, status, assignee_agent_id,
             creator_kind, creator_agent_id, created_at, board_key)
         VALUES ('task-1', 1, 'pair-1', 'Do it', 'in_progress', ?, 'agent', ?, 1, ?)",
    )
    .bind(OWNER)
    .bind(OWNER)
    .bind(&board_key)
    .execute(&mut connection)
    .await
    .unwrap();
    connection.close().await.unwrap();

    // Delete the PEER (OTHER) principal before calling the writer.
    {
        let mut connection = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        sqlx::query("DELETE FROM agents WHERE id = ?")
            .bind(OTHER)
            .execute(&mut connection)
            .await
            .unwrap();
    }

    let writer = SqliteTaskStatusWrite::open(&path).await.unwrap();
    let outcome = writer
        .apply_agent_status(
            &task_id(),
            &owner(),
            &AgentEventNames::new("actor", "Actor"),
            TaskStatus::InReview,
        )
        .await
        .unwrap();
    assert_eq!(
        outcome,
        AgentStatusWriteOutcome::ConversationReadonly,
        "a direct pair whose peer principal is gone is read-only"
    );
    assert_eq!(
        read_status(&path, TASK).await.0,
        "in_progress",
        "zero status write on the read-only refusal"
    );
}

// ── Human status override ────────────────────────────────────────────────────
// The human door is an override: no transition table, no assignee gate; only
// participation → read-only → same-status no-op.

use silverhand_backend_core::conversation_messaging::messaging::task::{
    HumanStatusWriteOutcome, HumanTaskStatusWrite,
};
use silverhand_backend_core::foundation::identity::UserId;

const HUMAN: &str = "user-1";

/// Seed a workspace task with a human participant (workspace_users + conversation_users).
async fn open_db_human(seed_status: &str) -> (TempDir, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("tasks.sqlite");
    let mut connection = current_schema::create_current_schema(&path).await;
    for statement in [
        "INSERT INTO users (id, name, display_name, created_at, updated_at)
         VALUES ('user-1', 'user-1', 'User One', 1, 1)",
        "INSERT INTO workspaces (id, name, name_key, created_at, updated_at)
         VALUES ('ws-1', 'ws', 'ws', 1, 1)",
        "INSERT INTO workspace_users (workspace_id, user_id, role, joined_at)
         VALUES ('ws-1', 'user-1', 'member', 1)",
        "INSERT INTO conversations
            (id, workspace_id, placement_kind, roster_policy, title, title_key, status, created_at, updated_at)
         VALUES ('conv-1', 'ws-1', 'workspace', 'open', 'general', 'general', 'active', 1, 1)",
        "INSERT INTO conversation_users (conversation_id, user_id, joined_at)
         VALUES ('conv-1', 'user-1', 1)",
    ] {
        sqlx::query(statement).execute(&mut connection).await.unwrap();
    }
    sqlx::query(
        "INSERT INTO tasks
            (id, workspace_id, task_number, conversation_id, title, status, creator_kind,
             creator_user_id, created_at, board_key)
         VALUES ('task-1', 'ws-1', 1, 'conv-1', 'Do it', ?, 'user', 'user-1', 1, 'ws:ws-1')",
    )
    .bind(seed_status)
    .execute(&mut connection)
    .await
    .unwrap();
    connection.close().await.unwrap();
    (directory, path)
}

fn human() -> UserId {
    UserId::parse(HUMAN).unwrap()
}

#[tokio::test]
async fn a_human_override_ignores_the_agent_transition_table() {
    // todo -> done is NOT a legal AGENT status edge, but the human override permits
    // any change; the participant human applies it.
    let (_dir, path) = open_db_human("todo").await;
    let writer = SqliteTaskStatusWrite::open(&path).await.unwrap();
    let outcome = writer
        .apply_human_status(&task_id(), &human(), TaskStatus::Done)
        .await
        .unwrap();
    assert!(matches!(outcome, HumanStatusWriteOutcome::Applied(_)));
    assert_eq!(read_status(&path, TASK).await.0, "done");
}

#[tokio::test]
async fn a_human_setting_the_same_status_is_already_at_status() {
    let (_dir, path) = open_db_human("in_progress").await;
    let writer = SqliteTaskStatusWrite::open(&path).await.unwrap();
    let outcome = writer
        .apply_human_status(&task_id(), &human(), TaskStatus::InProgress)
        .await
        .unwrap();
    assert_eq!(outcome, HumanStatusWriteOutcome::AlreadyAtStatus);
    assert_eq!(read_status(&path, TASK).await.0, "in_progress");
}

#[tokio::test]
async fn a_non_participant_human_is_notfound() {
    // Remove the human's conversation membership before the write.
    let (_dir, path) = open_db_human("in_progress").await;
    {
        let mut connection = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        sqlx::query("DELETE FROM conversation_users WHERE conversation_id = 'conv-1' AND user_id = 'user-1'")
            .execute(&mut connection)
            .await
            .unwrap();
    }
    let writer = SqliteTaskStatusWrite::open(&path).await.unwrap();
    let outcome = writer
        .apply_human_status(&task_id(), &human(), TaskStatus::Done)
        .await
        .unwrap();
    assert_eq!(outcome, HumanStatusWriteOutcome::NotFound);
    assert_eq!(read_status(&path, TASK).await.0, "in_progress");
}
