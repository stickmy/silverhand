//! Real-SQLite gates for the task `create` write unit-of-work.
//!
//! Drives `SqliteTaskCreateWrite::create_tasks`: a create persists its row and
//! `task_create` message anchor; duplicate source is SourceConflict and read-only
//! destination is ConversationReadonly. Concurrent creates get consecutive numbers.
//! A duplicate source inside one batch exercises rollback of its earlier task/event;
//! this suite does not inject a message-insert or backfill failure.

use silverhand_backend_core::conversation_messaging::messaging::task::{
    BoardKey, PairKey, TaskContent, TaskCreateBatch, TaskCreateOutcome, TaskCreatePlan,
    TaskCreateWrite, TaskCreator, TaskTitle,
};
use silverhand_backend_core::foundation::identity::{AgentId, ConversationId, WorkspaceId};
use silverhand_backend_sqlite::conversation_messaging::messaging::task::SqliteTaskCreateWrite;
use sqlx::{Connection, Row, SqliteConnection};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

use crate::current_schema;

const CONV: &str = "conv-1";
const CREATOR: &str = "agent-1";

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
        "INSERT INTO workspace_members (workspace_id, agent_id, joined_at)
         VALUES ('ws-1', 'agent-1', 1)",
        "INSERT INTO workspace_agent_capabilities (workspace_id, agent_id, capability)
         VALUES ('ws-1', 'agent-1', 'create_tasks')",
        "INSERT INTO conversations
            (id, workspace_id, placement_kind, roster_policy, title, title_key, status, created_at, updated_at)
         VALUES ('conv-1', 'ws-1', 'workspace', 'open', 'general', 'general', 'active', 1, 1)",
        "INSERT INTO conversation_agents (conversation_id, agent_id, joined_at)
         VALUES ('conv-1', 'agent-1', 1)",
        // The conversation needs a stream-head row for the seq reservation to advance.
        "INSERT INTO conversation_stream_heads (conversation_id, thread_root_id, latest_seq)
         VALUES ('conv-1', NULL, 0)",
    ] {
        sqlx::query(statement).execute(&mut connection).await.unwrap();
    }
    connection.close().await.unwrap();
    (directory, path)
}

fn plan(source: Option<&str>) -> TaskCreatePlan {
    TaskCreatePlan {
        thread_root_id: None,
        title: TaskTitle::parse("Do it").unwrap(),
        content: TaskContent::parse("body").unwrap(),
        claim_by: None,
        source_message_id: source
            .map(|s| {
                silverhand_backend_core::conversation_messaging::messaging::task::SourceMessageId::parse(s)
                    .unwrap()
            }),
        event_actor:
            silverhand_backend_core::conversation_messaging::messaging::task::TaskSystemActor::Agent {
                id: AgentId::parse(CREATOR).unwrap(),
                names: silverhand_backend_core::conversation_messaging::messaging::task::AgentEventNames::new("agent", "Agent"),
            },
        event_assignee: None,
        event_placement:
            silverhand_backend_core::conversation_messaging::messaging::task::CreatePlacement::Filed,
    }
}

fn batch(items: Vec<TaskCreatePlan>) -> TaskCreateBatch {
    TaskCreateBatch::new(
        ConversationId::parse(CONV).unwrap(),
        BoardKey::Workspace(WorkspaceId::parse("ws-1").unwrap()),
        TaskCreator::Agent(AgentId::parse(CREATOR).unwrap()),
        items,
    )
}

async fn conn(path: &Path) -> SqliteConnection {
    SqliteConnection::connect(&format!("sqlite://{}", path.display()))
        .await
        .unwrap()
}

async fn insert_source(path: &Path, id: &str) {
    let mut c = conn(path).await;
    sqlx::query("INSERT INTO messages (id, conversation_id, from_kind, from_user_id, content, seq, created_at) VALUES (?, ?, 'user', 'user-1', 'source', 1, 1)")
        .bind(id).bind(CONV).execute(&mut c).await.unwrap();
    sqlx::query("UPDATE conversation_stream_heads SET latest_seq = 1 WHERE conversation_id = ? AND thread_root_id IS NULL")
        .bind(CONV).execute(&mut c).await.unwrap();
    sqlx::query("UPDATE conversations SET latest_message_seq = 1 WHERE id = ?")
        .bind(CONV)
        .execute(&mut c)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_create_persists_its_row_number_and_system_message_anchor() {
    let (_dir, path) = open_db().await;
    let writer = SqliteTaskCreateWrite::open(&path).await.unwrap();

    let outcome = writer.create_tasks(&batch(vec![plan(None)])).await.unwrap();
    let witness = match outcome {
        TaskCreateOutcome::Created(mut w) => w.remove(0),
        other => panic!("expected Created, got {other:?}"),
    };
    assert_eq!(witness.task_number().get(), 1);
    assert_eq!(
        witness.mutation().board.workspace_id().unwrap().as_str(),
        "ws-1"
    );

    let mut c = conn(&path).await;
    // The task row carries the created_event_message_id, and that message exists
    // as a `task_create` system message — a 1:1 anchor.
    let row = sqlx::query(
        "SELECT t.created_event_message_id AS mid, m.from_kind, m.metadata
         FROM tasks t JOIN messages m ON m.id = t.created_event_message_id
         WHERE t.id = ?",
    )
    .bind(witness.task_id().as_str())
    .fetch_one(&mut c)
    .await
    .unwrap();
    let mid: String = row.try_get("mid").unwrap();
    assert_eq!(mid, witness.mutation().event_message.id.as_str());
    assert_eq!(row.try_get::<String, _>("from_kind").unwrap(), "system");
    let metadata: serde_json::Value =
        serde_json::from_str(&row.try_get::<String, _>("metadata").unwrap()).unwrap();
    assert_eq!(metadata["subtype"], "task_create");
}

#[tokio::test]
async fn a_duplicate_source_message_is_a_source_conflict_with_zero_write() {
    let (_dir, path) = open_db().await;
    insert_source(&path, "msg-99").await;
    let writer = SqliteTaskCreateWrite::open(&path).await.unwrap();

    // First create binds the source.
    let first = writer
        .create_tasks(&batch(vec![plan(Some("msg-99"))]))
        .await
        .unwrap();
    assert!(matches!(first, TaskCreateOutcome::Created(_)));

    // Second create with the SAME source is a conflict, nothing new written.
    let second = writer
        .create_tasks(&batch(vec![plan(Some("msg-99"))]))
        .await
        .unwrap();
    match second {
        TaskCreateOutcome::SourceConflict {
            existing_task: Some(existing),
        } => {
            assert_eq!(existing.task_number.get(), 1);
            assert_eq!(existing.status.as_str(), "todo");
        }
        other => panic!("expected source conflict with winner, got {other:?}"),
    }

    let mut c = conn(&path).await;
    let task_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tasks")
        .fetch_one(&mut c)
        .await
        .unwrap();
    let msg_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages")
        .fetch_one(&mut c)
        .await
        .unwrap();
    assert_eq!(task_count, 1, "the conflicting create wrote no second task");
    assert_eq!(
        msg_count, 2,
        "source plus one task_create message; no second event"
    );
}

#[tokio::test]
async fn a_missing_source_is_refused_inside_the_batch_writer_with_zero_task_and_event() {
    let (_dir, path) = open_db().await;
    let writer = SqliteTaskCreateWrite::open(&path).await.unwrap();
    let outcome = writer
        .create_tasks(&batch(vec![plan(Some("missing-source")), plan(None)]))
        .await
        .unwrap();
    assert_eq!(outcome, TaskCreateOutcome::SourceMessageNotFound);

    let mut c = conn(&path).await;
    let tasks: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tasks")
        .fetch_one(&mut c)
        .await
        .unwrap();
    let events: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM messages WHERE json_extract(metadata, '$.subtype') = 'task_create'",
    )
    .fetch_one(&mut c)
    .await
    .unwrap();
    assert_eq!((tasks, events), (0, 0));
}

#[tokio::test]
async fn a_source_from_another_conversation_refuses_the_whole_batch_before_any_write() {
    let (_dir, path) = open_db().await;
    let mut c = conn(&path).await;
    sqlx::query(
        "INSERT INTO conversations
            (id, workspace_id, placement_kind, roster_policy, title, title_key, status, created_at, updated_at)
         VALUES ('conv-other', 'ws-1', 'workspace', 'open', 'other', 'other', 'active', 1, 1)",
    )
    .execute(&mut c)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO messages
            (id, conversation_id, from_kind, from_user_id, content, seq, created_at)
         VALUES ('msg-other', 'conv-other', 'user', 'user-1', 'source', 1, 1)",
    )
    .execute(&mut c)
    .await
    .unwrap();
    c.close().await.unwrap();

    let writer = SqliteTaskCreateWrite::open(&path).await.unwrap();
    let outcome = writer
        .create_tasks(&batch(vec![plan(Some("msg-other")), plan(None)]))
        .await
        .unwrap();
    assert_eq!(outcome, TaskCreateOutcome::SourceConversationMismatch);

    let mut c = conn(&path).await;
    let tasks: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tasks")
        .fetch_one(&mut c)
        .await
        .unwrap();
    let events: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM messages WHERE json_extract(metadata, '$.subtype') = 'task_create'",
    )
    .fetch_one(&mut c)
    .await
    .unwrap();
    assert_eq!((tasks, events), (0, 0));
}

#[tokio::test]
async fn workspace_capabilities_are_replayed_under_the_batch_writer_with_whole_batch_zero_write() {
    let (_dir, path) = open_db().await;
    let writer = SqliteTaskCreateWrite::open(&path).await.unwrap();
    let mut c = conn(&path).await;
    sqlx::query("DELETE FROM workspace_agent_capabilities WHERE workspace_id='ws-1' AND agent_id='agent-1' AND capability='create_tasks'")
        .execute(&mut c).await.unwrap();
    drop(c);
    let outcome = writer
        .create_tasks(&batch(vec![plan(None), plan(None)]))
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        TaskCreateOutcome::CapabilityDenied(
            silverhand_backend_core::conversation_messaging::messaging::task::TaskWriteCapability::Create
        )
    ));

    let mut c = conn(&path).await;
    let task_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tasks")
        .fetch_one(&mut c)
        .await
        .unwrap();
    let event_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE from_kind='system'")
            .fetch_one(&mut c)
            .await
            .unwrap();
    assert_eq!((task_count, event_count), (0, 0));

    sqlx::query("INSERT INTO workspace_agent_capabilities (workspace_id,agent_id,capability) VALUES ('ws-1','agent-1','create_tasks')")
        .execute(&mut c).await.unwrap();
    drop(c);
    let mut claimed = plan(None);
    claimed.claim_by = Some(AgentId::parse(CREATOR).unwrap());
    claimed.event_assignee = Some(
        silverhand_backend_core::conversation_messaging::messaging::task::TaskEventAssignee::Agent {
            id: AgentId::parse(CREATOR).unwrap(),
            names: silverhand_backend_core::conversation_messaging::messaging::task::AgentEventNames::new("agent", "Agent"),
        },
    );
    let outcome = writer
        .create_tasks(&batch(vec![claimed, plan(None)]))
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        TaskCreateOutcome::CapabilityDenied(
            silverhand_backend_core::conversation_messaging::messaging::task::TaskWriteCapability::Write
        )
    ));
    let mut c = conn(&path).await;
    let task_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tasks")
        .fetch_one(&mut c)
        .await
        .unwrap();
    let event_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE from_kind='system'")
            .fetch_one(&mut c)
            .await
            .unwrap();
    assert_eq!((task_count, event_count), (0, 0));
}

#[tokio::test]
async fn a_duplicate_source_inside_one_batch_rolls_back_every_task_and_event() {
    let (_dir, path) = open_db().await;
    insert_source(&path, "msg-dup").await;
    let writer = SqliteTaskCreateWrite::open(&path).await.unwrap();
    let outcome = writer
        .create_tasks(&batch(vec![plan(Some("msg-dup")), plan(Some("msg-dup"))]))
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        TaskCreateOutcome::SourceConflict {
            existing_task: None
        }
    ));

    let mut c = conn(&path).await;
    let tasks: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tasks")
        .fetch_one(&mut c)
        .await
        .unwrap();
    let events: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM messages WHERE json_extract(metadata, '$.subtype') = 'task_create'",
    )
    .fetch_one(&mut c)
    .await
    .unwrap();
    assert_eq!((tasks, events), (0, 0));
}

#[tokio::test]
async fn closed_and_archived_destinations_refuse_without_task_or_message() {
    for column in ["closed_at", "archived_at"] {
        let (_dir, path) = open_db().await;
        let mut c = conn(&path).await;
        let update = if column == "closed_at" {
            "UPDATE conversations SET closed_at = 9 WHERE id = ?"
        } else {
            "UPDATE conversations SET archived_at = 9 WHERE id = ?"
        };
        sqlx::query(update)
            .bind(CONV)
            .execute(&mut c)
            .await
            .unwrap();
        c.close().await.unwrap();

        let writer = SqliteTaskCreateWrite::open(&path).await.unwrap();
        let outcome = writer.create_tasks(&batch(vec![plan(None)])).await.unwrap();
        assert!(matches!(
            (column, outcome),
            ("closed_at", TaskCreateOutcome::ConversationClosed)
                | ("archived_at", TaskCreateOutcome::ConversationArchived)
        ));

        let mut c = conn(&path).await;
        let tasks: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tasks")
            .fetch_one(&mut c)
            .await
            .unwrap();
        let events: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages")
            .fetch_one(&mut c)
            .await
            .unwrap();
        assert_eq!((tasks, events), (0, 0), "{column}");
    }
}

#[tokio::test]
async fn a_readonly_direct_pair_destination_is_conversation_readonly() {
    // A direct pair whose peer principal is gone accepts no writer.
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
        // The peer exists at pair-insert time (a trigger requires it), then is
        // deleted below to make the pair read-only.
        "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at,
            display_name, created_by_user_id, session_scope)
         VALUES ('agent-gone', 'agent-gone', 'codex', 'host-1', 1, 1, 'Agent', 'user-1', 'conversation')",
        // pair between agent-1 and the (soon-gone) peer.
        "INSERT INTO conversations
            (id, placement_kind, roster_policy, title, status, created_at, updated_at,
             endpoint_a_kind, endpoint_a_id, endpoint_b_kind, endpoint_b_id)
         VALUES ('pair-1', 'direct_pair', 'fixed', '', 'active', 1, 1,
             'agent', 'agent-1', 'agent', 'agent-gone')",
        "INSERT INTO conversation_stream_heads (conversation_id, thread_root_id, latest_seq)
         VALUES ('pair-1', NULL, 0)",
    ] {
        sqlx::query(statement).execute(&mut connection).await.unwrap();
    }
    // Delete the peer principal → the pair now accepts no posts (read-only).
    sqlx::query("DELETE FROM agents WHERE id = 'agent-gone'")
        .execute(&mut connection)
        .await
        .unwrap();
    let pair_key: String =
        sqlx::query_scalar("SELECT pair_key FROM conversations WHERE id='pair-1'")
            .fetch_one(&mut connection)
            .await
            .unwrap();
    connection.close().await.unwrap();

    let writer = SqliteTaskCreateWrite::open(&path).await.unwrap();
    let p = TaskCreateBatch::new(
        ConversationId::parse("pair-1").unwrap(),
        BoardKey::Pair(PairKey::parse(pair_key).unwrap()),
        TaskCreator::Agent(AgentId::parse(CREATOR).unwrap()),
        vec![plan(None)],
    );
    let outcome = writer.create_tasks(&p).await.unwrap();
    assert_eq!(outcome, TaskCreateOutcome::ConversationReadonly);

    let mut c = conn(&path).await;
    let task_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tasks")
        .fetch_one(&mut c)
        .await
        .unwrap();
    assert_eq!(task_count, 0, "a read-only destination writes no task");
}

/// Concurrency control: two creates on the same board get DISTINCT CONSECUTIVE
/// numbers (1 and 2), never a collision — the IMMEDIATE writer serializes the
/// MAX+1 allocation and the UNIQUE(board_key, task_number) index backstops.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_creates_on_one_board_get_distinct_consecutive_numbers() {
    let (_dir, path) = open_db().await;
    let a = SqliteTaskCreateWrite::open(&path).await.unwrap();
    let b = SqliteTaskCreateWrite::open(&path).await.unwrap();
    let pa = batch(vec![plan(None)]);
    let pb = batch(vec![plan(None)]);
    let (ra, rb) = tokio::join!(a.create_tasks(&pa), b.create_tasks(&pb));
    let na = match ra.unwrap() {
        TaskCreateOutcome::Created(w) => w[0].task_number().get(),
        other => panic!("a: {other:?}"),
    };
    let nb = match rb.unwrap() {
        TaskCreateOutcome::Created(w) => w[0].task_number().get(),
        other => panic!("b: {other:?}"),
    };
    let mut nums = [na, nb];
    nums.sort_unstable();
    assert_eq!(nums, [1, 2], "two creates get distinct consecutive numbers");

    let mut c = conn(&path).await;
    let task_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tasks")
        .fetch_one(&mut c)
        .await
        .unwrap();
    assert_eq!(task_count, 2);
}
