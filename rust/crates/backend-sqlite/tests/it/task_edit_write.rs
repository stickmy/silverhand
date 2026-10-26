//! Real-SQLite gates for the task `edit` write unit-of-work.
//!
//! Drives `SqliteTaskEditWrite::apply_edit`: a real change applies and stamps the
//! `last_editor` triple; a same-value request is a NoOp with zero write; a
//! non-participant is NotFound; and the human editor path stamps its own actor
//! kind. Edit has no
//! assignee gate — participation + read-only + the canonical no-op decision.

use silverhand_backend_core::conversation_messaging::messaging::task::{
    AgentEventNames, ReadViewer, TaskContent, TaskEditWrite, TaskEditWriteOutcome, TaskEditor,
    TaskId, TaskTitle,
};
use silverhand_backend_core::foundation::identity::{AgentId, UserId};
use silverhand_backend_sqlite::conversation_messaging::messaging::task::SqliteTaskEditWrite;
use sqlx::{Connection, Row, SqliteConnection};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

use crate::current_schema;

const TASK: &str = "task-1";
const AGENT: &str = "agent-1";

async fn open_db() -> (TempDir, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("tasks.sqlite");
    let mut connection = current_schema::create_current_schema(&path).await;
    for statement in [
        "INSERT INTO users (id, name, display_name, created_at, updated_at)
         VALUES ('user-1', 'user-1', 'User One', 1, 1)",
        "INSERT INTO hosts (id, name, kind, created_at, user_id)
         VALUES ('host-1', 'host-1', 'remote', 1, 'user-1')",
        "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at,
            display_name, created_by_user_id, session_scope)
         VALUES ('agent-1', 'agent-1', 'codex', 'host-1', 1, 1, 'Agent', 'user-1', 'conversation')",
        "INSERT INTO workspaces (id, name, name_key, created_at, updated_at)
         VALUES ('ws-1', 'ws', 'ws', 1, 1)",
        "INSERT INTO conversations
            (id, workspace_id, placement_kind, roster_policy, title, title_key, status, created_at, updated_at)
         VALUES ('conv-1', 'ws-1', 'workspace', 'open', 'general', 'general', 'active', 1, 1)",
        "INSERT INTO conversation_agents (conversation_id, agent_id, joined_at)
         VALUES ('conv-1', 'agent-1', 1)",
        "INSERT INTO workspace_members (workspace_id, agent_id, joined_at)
         VALUES ('ws-1', 'agent-1', 1)",
        "INSERT INTO workspace_agent_capabilities (workspace_id, agent_id, capability)
         VALUES ('ws-1', 'agent-1', 'write_tasks')",
        "INSERT INTO workspace_agent_capabilities (workspace_id, agent_id, capability)
         VALUES ('ws-1', 'agent-1', 'read_tasks')",
        "INSERT INTO tasks
            (id, workspace_id, task_number, conversation_id, title, content, status,
             creator_kind, creator_agent_id, created_at, board_key)
         VALUES ('task-1', 'ws-1', 1, 'conv-1', 'Old title', 'old body', 'todo',
             'agent', 'agent-1', 1, 'ws:ws-1')",
    ] {
        sqlx::query(statement).execute(&mut connection).await.unwrap();
    }
    connection.close().await.unwrap();
    (directory, path)
}

async fn read_edit(path: &Path) -> (String, String, Option<String>, Option<String>, Option<i64>) {
    let mut connection = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
        .await
        .unwrap();
    let row = sqlx::query(
        "SELECT title, content, last_editor_kind, last_editor_id, last_edited_at
         FROM tasks WHERE id = ?",
    )
    .bind(TASK)
    .fetch_one(&mut connection)
    .await
    .unwrap();
    (
        row.try_get::<String, _>("title").unwrap(),
        row.try_get::<String, _>("content").unwrap(),
        row.try_get::<Option<String>, _>("last_editor_kind")
            .unwrap(),
        row.try_get::<Option<String>, _>("last_editor_id").unwrap(),
        row.try_get::<Option<i64>, _>("last_edited_at").unwrap(),
    )
}

fn task_id() -> TaskId {
    TaskId::parse(TASK).unwrap()
}

fn agent_editor() -> TaskEditor {
    TaskEditor::Agent(AgentId::parse(AGENT).unwrap())
}

/// All messages in the seeded conversation, including unexpected kinds/subtypes.
async fn conversation_messages(path: &Path) -> Vec<(String, String, String)> {
    let mut connection = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
        .await
        .unwrap();
    let rows = sqlx::query(
        "SELECT from_kind, content, metadata FROM messages \
         WHERE conversation_id = 'conv-1' \
         ORDER BY seq",
    )
    .fetch_all(&mut connection)
    .await
    .unwrap();
    rows.into_iter()
        .map(|row| {
            (
                row.try_get::<String, _>("from_kind").unwrap(),
                row.try_get::<String, _>("content").unwrap(),
                row.try_get::<String, _>("metadata").unwrap(),
            )
        })
        .collect()
}

#[tokio::test]
async fn clearing_content_to_empty_is_a_real_change() {
    let (_dir, path) = open_db().await;
    let writer = SqliteTaskEditWrite::open(&path).await.unwrap();

    let outcome = writer
        .apply_edit(
            &task_id(),
            &agent_editor(),
            Some(&AgentEventNames::new("agent-1", "Agent")),
            None,
            Some(TaskContent::empty()),
        )
        .await
        .unwrap();
    assert!(matches!(outcome, TaskEditWriteOutcome::Applied(_)));
    let (_title, content, _k, _i, _e) = read_edit(&path).await;
    assert_eq!(
        content, "",
        "empty content differs from the old body — a change"
    );
}

#[tokio::test]
async fn a_non_participant_agent_is_notfound() {
    let (_dir, path) = open_db().await;
    // Remove the agent's roster membership before the edit.
    {
        let mut connection = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        sqlx::query(
            "DELETE FROM conversation_agents WHERE conversation_id = 'conv-1' AND agent_id = ?",
        )
        .bind(AGENT)
        .execute(&mut connection)
        .await
        .unwrap();
    }
    let writer = SqliteTaskEditWrite::open(&path).await.unwrap();
    let outcome = writer
        .apply_edit(
            &task_id(),
            &agent_editor(),
            Some(&AgentEventNames::new("agent-1", "Agent")),
            Some(TaskTitle::parse("New title").unwrap()),
            None,
        )
        .await
        .unwrap();
    assert_eq!(outcome, TaskEditWriteOutcome::NotFound);
    assert_eq!(
        read_edit(&path).await.0,
        "Old title",
        "zero write on a participation miss"
    );
}

#[tokio::test]
async fn a_human_editor_shares_the_gate_and_stamps_a_user_triple() {
    // A workspace human participant edits; the last_editor triple is a user triple.
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
        "INSERT INTO tasks
            (id, workspace_id, task_number, conversation_id, title, content, status,
             creator_kind, creator_user_id, created_at, board_key)
         VALUES ('task-1', 'ws-1', 1, 'conv-1', 'Old title', 'old body', 'todo',
             'user', 'user-1', 1, 'ws:ws-1')",
    ] {
        sqlx::query(statement).execute(&mut connection).await.unwrap();
    }
    connection.close().await.unwrap();

    let writer = SqliteTaskEditWrite::open(&path).await.unwrap();
    let outcome = writer
        .apply_edit(
            &task_id(),
            &TaskEditor::Human(UserId::parse("user-1").unwrap()),
            None,
            Some(TaskTitle::parse("Human title").unwrap()),
            None,
        )
        .await
        .unwrap();
    assert!(matches!(outcome, TaskEditWriteOutcome::Applied(_)));
    let (title, _content, kind, id, _e) = read_edit(&path).await;
    assert_eq!(title, "Human title");
    assert_eq!(kind.as_deref(), Some("user"));
    assert_eq!(id.as_deref(), Some("user-1"));
}

#[tokio::test]
async fn a_real_title_change_persists_the_row_editor_and_post_change_event() {
    // The persisted row and notice carry the post-change title and editor.
    let (_dir, path) = open_db().await;
    let writer = SqliteTaskEditWrite::open(&path).await.unwrap();

    let outcome = writer
        .apply_edit(
            &task_id(),
            &agent_editor(),
            Some(&AgentEventNames::new("alice", "Alice")),
            Some(TaskTitle::parse("New title").unwrap()),
            None,
        )
        .await
        .unwrap();
    assert!(matches!(outcome, TaskEditWriteOutcome::Applied(_)));

    let (title, body, kind, id, edited_at) = read_edit(&path).await;
    assert_eq!(title, "New title");
    assert_eq!(body, "old body", "an unset field is left unchanged");
    assert_eq!(kind.as_deref(), Some("agent"));
    assert_eq!(id.as_deref(), Some(AGENT));
    assert!(edited_at.is_some_and(|timestamp| timestamp > 1));

    let events = conversation_messages(&path).await;
    assert_eq!(events.len(), 1, "exactly one task_edit event appended");
    let (from_kind, content, metadata) = &events[0];
    assert_eq!(from_kind, "system");
    // Post-change title + resolved actor label (agent → `@name`), task number 1.
    assert_eq!(content, "@alice edited task #1 \"New title\"");
    let value: serde_json::Value = serde_json::from_str(metadata).unwrap();
    assert_eq!(value["subtype"], "task_edit");
    assert_eq!(value["task_number"], 1);
    assert_eq!(value["task_title"], "New title");
    assert_eq!(value["actor"]["kind"], "agent");
    assert_eq!(value["actor"]["id"], "agent-1");
    assert_eq!(value["actor"]["name"], "Alice");
}

#[tokio::test]
async fn a_same_value_request_preserves_the_row_and_appends_no_message() {
    // A same-value request writes nothing — no UPDATE, no last_editor stamp, and
    // crucially NO event (a notice would claim a change that did not happen).
    let (_dir, path) = open_db().await;
    let writer = SqliteTaskEditWrite::open(&path).await.unwrap();

    let outcome = writer
        .apply_edit(
            &task_id(),
            &agent_editor(),
            Some(&AgentEventNames::new("alice", "Alice")),
            Some(TaskTitle::parse("Old title").unwrap()),
            None,
        )
        .await
        .unwrap();
    assert!(matches!(outcome, TaskEditWriteOutcome::NoOp(_)));
    assert!(
        conversation_messages(&path).await.is_empty(),
        "a no-op appends no message of any subtype"
    );
    let (title, content, kind, id, edited_at) = read_edit(&path).await;
    assert_eq!(title, "Old title");
    assert_eq!(content, "old body");
    assert_eq!(kind, None, "no last_editor stamp on a no-op");
    assert_eq!(id, None);
    assert_eq!(edited_at, None);
}

#[tokio::test]
async fn applied_and_noop_return_the_authorized_committed_post_image_without_a_live_reread() {
    for human in [false, true] {
        for (requested, expected_title, expects_event) in [
            ("New title", "New title", true),
            ("Old title", "Old title", false),
        ] {
            let (_dir, path) = open_db().await;
            let (editor, names, viewer) = if human {
                let mut connection =
                    SqliteConnection::connect(&format!("sqlite://{}", path.display()))
                        .await
                        .unwrap();
                sqlx::query("INSERT INTO workspace_users (workspace_id,user_id,role,joined_at) VALUES ('ws-1','user-1','member',1)")
                    .execute(&mut connection).await.unwrap();
                sqlx::query("INSERT INTO conversation_users (conversation_id,user_id,joined_at) VALUES ('conv-1','user-1',1)")
                    .execute(&mut connection).await.unwrap();
                connection.close().await.unwrap();
                (
                    TaskEditor::Human(UserId::parse("user-1").unwrap()),
                    None,
                    ReadViewer::User(UserId::parse("user-1").unwrap()),
                )
            } else {
                (
                    agent_editor(),
                    Some(AgentEventNames::new("alice", "Alice")),
                    ReadViewer::Agent(AgentId::parse(AGENT).unwrap()),
                )
            };
            let writer = SqliteTaskEditWrite::open(&path).await.unwrap();
            let outcome = writer
                .apply_edit(
                    &task_id(),
                    &editor,
                    names.as_ref(),
                    Some(TaskTitle::parse(requested).unwrap()),
                    None,
                )
                .await
                .unwrap();
            let success = match outcome {
                TaskEditWriteOutcome::Applied(success) if expects_event => success,
                TaskEditWriteOutcome::NoOp(success) if !expects_event => success,
                other => panic!("unexpected edit outcome: {other:?}"),
            };

            // Deterministic post-commit latch: revoke mutable authorization before
            // the response consumes the write result. The already committed success
            // remains self-contained and viewer-bound for both route faces.
            let mut connection = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
                .await
                .unwrap();
            if human {
                sqlx::query(
                    "DELETE FROM workspace_users WHERE workspace_id='ws-1' AND user_id='user-1'",
                )
                .execute(&mut connection)
                .await
                .unwrap();
                sqlx::query("DELETE FROM conversation_users WHERE conversation_id='conv-1' AND user_id='user-1'")
                    .execute(&mut connection).await.unwrap();
            } else {
                sqlx::query("DELETE FROM workspace_agent_capabilities WHERE workspace_id='ws-1' AND agent_id='agent-1'")
                    .execute(&mut connection).await.unwrap();
                sqlx::query("DELETE FROM conversation_agents WHERE conversation_id='conv-1' AND agent_id='agent-1'")
                    .execute(&mut connection).await.unwrap();
            }
            connection.close().await.unwrap();

            let source = success
                .detail
                .for_viewer(&viewer)
                .expect("the committed post-image remains available to its sealed viewer");
            assert_eq!(source.summary.task.title().as_str(), expected_title);
            assert_eq!(success.witness.is_some(), expects_event);
        }
    }
}
