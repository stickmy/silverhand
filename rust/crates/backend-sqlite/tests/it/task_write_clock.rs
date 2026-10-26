//! Deterministic linearization tests for the four task writer modules.
//!
//! Each case holds SQLite's reserved writer, starts the mutation with an injected
//! clock, advances that clock while the future is blocked, then releases the lock.
//! The committed row and its atomic event must both use the post-lock instant.

use futures_util::poll;
use silverhand_backend_core::conversation_messaging::messaging::task::{
    AgentClaimOutcome, AgentEventNames, AgentStatusWriteOutcome, BoardKey, CreatePlacement,
    TaskClaimWrite, TaskContent, TaskCreateBatch, TaskCreateOutcome, TaskCreatePlan,
    TaskCreateWrite, TaskCreator, TaskEditWrite, TaskEditWriteOutcome, TaskEditor, TaskId,
    TaskStatus, TaskStatusWrite, TaskSystemActor, TaskTitle, TaskWriteClock, UnclaimOutcome,
};
use silverhand_backend_core::foundation::identity::{AgentId, ConversationId, WorkspaceId};
use silverhand_backend_sqlite::conversation_messaging::messaging::task::{
    SqliteTaskClaimWrite, SqliteTaskCreateWrite, SqliteTaskEditWrite, SqliteTaskStatusWrite,
};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Connection, Row, SqliteConnection, SqlitePool};
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::task::Poll;
use std::time::Duration;
use tempfile::TempDir;

use crate::current_schema;

const ACTOR: &str = "agent-1";
const CONVERSATION: &str = "conv-1";

#[derive(Clone)]
struct MovingClock(Arc<AtomicI64>);

impl TaskWriteClock for MovingClock {
    fn now(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

async fn open_db(with_task: bool) -> (TempDir, std::path::PathBuf, SqliteConnection) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("tasks.sqlite");
    let mut connection = current_schema::create_current_schema(&path).await;
    for statement in [
        "INSERT INTO users (id,name,display_name,created_at,updated_at) VALUES ('user-1','user-1','User One',1,1)",
        "INSERT INTO hosts (id,name,kind,created_at,user_id) VALUES ('host-1','host-1','remote',1,'user-1')",
        "INSERT INTO agents (id,name,runtime,host_id,created_at,updated_at,display_name,created_by_user_id,session_scope) VALUES ('agent-1','agent-1','codex','host-1',1,1,'Agent One','user-1','conversation')",
        "INSERT INTO workspaces (id,name,name_key,created_at,updated_at) VALUES ('ws-1','ws','ws',1,1)",
        "INSERT INTO workspace_members (workspace_id,agent_id,joined_at) VALUES ('ws-1','agent-1',1)",
        "INSERT INTO workspace_agent_capabilities (workspace_id,agent_id,capability) VALUES ('ws-1','agent-1','create_tasks')",
        "INSERT INTO workspace_agent_capabilities (workspace_id,agent_id,capability) VALUES ('ws-1','agent-1','write_tasks')",
        "INSERT INTO workspace_agent_capabilities (workspace_id,agent_id,capability) VALUES ('ws-1','agent-1','read_tasks')",
        "INSERT INTO conversations (id,workspace_id,placement_kind,roster_policy,title,title_key,status,created_at,updated_at) VALUES ('conv-1','ws-1','workspace','open','general','general','active',1,1)",
        "INSERT INTO conversation_agents (conversation_id,agent_id,joined_at) VALUES ('conv-1','agent-1',1)",
        "INSERT INTO conversation_stream_heads (conversation_id,thread_root_id,latest_seq) VALUES ('conv-1',NULL,0)",
    ] {
        sqlx::query(statement)
            .execute(&mut connection)
            .await
            .unwrap();
    }
    if with_task {
        sqlx::query("INSERT INTO tasks (id,workspace_id,task_number,conversation_id,title,content,status,creator_kind,creator_agent_id,assignee_agent_id,created_at,board_key) VALUES ('task-1','ws-1',1,'conv-1','Old','old','todo','agent','agent-1',NULL,1,'ws:ws-1')")
            .execute(&mut connection).await.unwrap();
    }
    sqlx::raw_sql("PRAGMA journal_mode = WAL; BEGIN IMMEDIATE")
        .execute(&mut connection)
        .await
        .unwrap();
    (directory, path, connection)
}

async fn pool(path: &std::path::Path) -> SqlitePool {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(Duration::from_secs(5));
    SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await
        .unwrap()
}

async fn prove_pending<F: std::future::Future>(future: &mut std::pin::Pin<&mut F>) {
    for _ in 0..4 {
        assert!(matches!(poll!(future.as_mut()), Poll::Pending));
        tokio::task::yield_now().await;
    }
}

fn actor() -> AgentId {
    AgentId::parse(ACTOR).unwrap()
}

fn task() -> TaskId {
    TaskId::parse("task-1").unwrap()
}

#[tokio::test]
async fn status_row_and_event_use_the_post_lock_instant() {
    let (_directory, path, mut lock) = open_db(true).await;
    sqlx::query(
        "UPDATE tasks SET status='in_progress', assignee_agent_id='agent-1' WHERE id='task-1'",
    )
    .execute(&mut lock)
    .await
    .unwrap();
    let now = Arc::new(AtomicI64::new(100));
    let writer = SqliteTaskStatusWrite::from_pool_with_clock(
        pool(&path).await.into(),
        MovingClock(now.clone()),
    );
    let task = task();
    let actor = actor();
    let names = AgentEventNames::new("agent-1", "Agent One");
    let mut pending = pin!(writer.apply_agent_status(&task, &actor, &names, TaskStatus::Closed,));
    prove_pending(&mut pending).await;
    now.store(500, Ordering::SeqCst);
    sqlx::raw_sql("COMMIT").execute(&mut lock).await.unwrap();
    let witness = match pending.await.unwrap() {
        AgentStatusWriteOutcome::Applied(witness) => witness,
        other => panic!("expected applied, got {other:?}"),
    };
    assert_eq!(witness.event_message.created_at, 500);
    let mut read = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
        .await
        .unwrap();
    let closed_at: i64 = sqlx::query_scalar("SELECT closed_at FROM tasks WHERE id='task-1'")
        .fetch_one(&mut read)
        .await
        .unwrap();
    assert_eq!(closed_at, 500);
}

#[tokio::test]
async fn edit_row_and_event_use_the_post_lock_instant() {
    let (_directory, path, mut lock) = open_db(true).await;
    let now = Arc::new(AtomicI64::new(100));
    let writer = SqliteTaskEditWrite::from_pool_with_clock(
        pool(&path).await.into(),
        MovingClock(now.clone()),
    );
    let task = task();
    let editor = TaskEditor::Agent(actor());
    let names = AgentEventNames::new("agent-1", "Agent One");
    let mut pending = pin!(writer.apply_edit(
        &task,
        &editor,
        Some(&names),
        Some(TaskTitle::parse("New").unwrap()),
        None,
    ));
    prove_pending(&mut pending).await;
    now.store(500, Ordering::SeqCst);
    sqlx::raw_sql("COMMIT").execute(&mut lock).await.unwrap();
    let success = match pending.await.unwrap() {
        TaskEditWriteOutcome::Applied(success) => success,
        other => panic!("expected applied, got {other:?}"),
    };
    assert_eq!(success.witness.unwrap().event_message.created_at, 500);
    let mut read = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
        .await
        .unwrap();
    let edited_at: i64 = sqlx::query_scalar("SELECT last_edited_at FROM tasks WHERE id='task-1'")
        .fetch_one(&mut read)
        .await
        .unwrap();
    assert_eq!(edited_at, 500);
}

#[tokio::test]
async fn claim_event_uses_the_post_lock_instant() {
    let (_directory, path, mut lock) = open_db(true).await;
    let now = Arc::new(AtomicI64::new(100));
    let writer = SqliteTaskClaimWrite::from_pool_with_clock(
        pool(&path).await.into(),
        MovingClock(now.clone()),
    );
    let task = task();
    let actor = actor();
    let names = AgentEventNames::new("agent-1", "Agent One");
    let mut pending = pin!(writer.agent_claim(&task, &actor, &names));
    prove_pending(&mut pending).await;
    now.store(500, Ordering::SeqCst);
    sqlx::raw_sql("COMMIT").execute(&mut lock).await.unwrap();
    let witness = match pending.await.unwrap() {
        AgentClaimOutcome::Claimed { witness, .. } => witness,
        other => panic!("expected claimed, got {other:?}"),
    };
    assert_eq!(witness.event_message.created_at, 500);
}

#[tokio::test]
async fn create_batch_rows_and_events_share_the_post_lock_instant() {
    let (_directory, path, mut lock) = open_db(false).await;
    let now = Arc::new(AtomicI64::new(100));
    let writer = SqliteTaskCreateWrite::from_pool_with_clock(
        pool(&path).await.into(),
        MovingClock(now.clone()),
    );
    let plans = vec!["A", "B"]
        .into_iter()
        .map(|title| TaskCreatePlan {
            thread_root_id: None,
            title: TaskTitle::parse(title).unwrap(),
            content: TaskContent::empty(),
            claim_by: None,
            source_message_id: None,
            event_actor: TaskSystemActor::Agent {
                id: actor(),
                names: AgentEventNames::new("agent-1", "Agent One"),
            },
            event_assignee: None,
            event_placement: CreatePlacement::Filed,
        })
        .collect();
    let batch = TaskCreateBatch::new(
        ConversationId::parse(CONVERSATION).unwrap(),
        BoardKey::Workspace(WorkspaceId::parse("ws-1").unwrap()),
        TaskCreator::Agent(actor()),
        plans,
    );
    let mut pending = pin!(writer.create_tasks(&batch));
    prove_pending(&mut pending).await;
    now.store(500, Ordering::SeqCst);
    sqlx::raw_sql("COMMIT").execute(&mut lock).await.unwrap();
    let witnesses = match pending.await.unwrap() {
        TaskCreateOutcome::Created(witnesses) => witnesses,
        other => panic!("expected created, got {other:?}"),
    };
    assert_eq!(witnesses.len(), 2);
    assert!(
        witnesses
            .iter()
            .all(|w| w.mutation().event_message.created_at == 500)
    );
    let mut read = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
        .await
        .unwrap();
    let rows = sqlx::query("SELECT created_at FROM tasks ORDER BY task_number")
        .fetch_all(&mut read)
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter()
            .all(|row| row.get::<i64, _>("created_at") == 500)
    );
}

#[tokio::test]
async fn all_four_agent_writers_replay_actor_existence_before_any_write() {
    let (_directory, path, mut connection) = open_db(true).await;
    sqlx::query("UPDATE tasks SET creator_kind='system', creator_agent_id=NULL, assignee_agent_id=NULL WHERE id='task-1'")
        .execute(&mut connection).await.unwrap();
    for statement in [
        "DELETE FROM conversation_agents WHERE agent_id='agent-1'",
        "DELETE FROM workspace_agent_capabilities WHERE agent_id='agent-1'",
        "DELETE FROM workspace_members WHERE agent_id='agent-1'",
    ] {
        sqlx::query(statement)
            .execute(&mut connection)
            .await
            .unwrap();
    }
    sqlx::query("DELETE FROM agents WHERE id='agent-1'")
        .execute(&mut connection)
        .await
        .unwrap();
    sqlx::raw_sql("COMMIT")
        .execute(&mut connection)
        .await
        .unwrap();

    let names = AgentEventNames::new("agent-1", "Agent One");
    let actor = actor();
    let task = task();
    let status = SqliteTaskStatusWrite::open(&path)
        .await
        .unwrap()
        .apply_agent_status(&task, &actor, &names, TaskStatus::InProgress)
        .await
        .unwrap();
    assert!(matches!(status, AgentStatusWriteOutcome::AgentNotFound));

    let edit = SqliteTaskEditWrite::open(&path)
        .await
        .unwrap()
        .apply_edit(
            &task,
            &TaskEditor::Agent(actor.clone()),
            Some(&names),
            Some(TaskTitle::parse("New").unwrap()),
            None,
        )
        .await
        .unwrap();
    assert!(matches!(edit, TaskEditWriteOutcome::AgentNotFound));

    let claim = SqliteTaskClaimWrite::open(&path)
        .await
        .unwrap()
        .agent_claim(&task, &actor, &names)
        .await
        .unwrap();
    assert!(matches!(claim, AgentClaimOutcome::AgentNotFound));

    let create_plan = TaskCreatePlan {
        thread_root_id: None,
        title: TaskTitle::parse("New task").unwrap(),
        content: TaskContent::empty(),
        claim_by: None,
        source_message_id: None,
        event_actor: TaskSystemActor::Agent {
            id: actor.clone(),
            names,
        },
        event_assignee: None,
        event_placement: CreatePlacement::Filed,
    };
    let create_batch = TaskCreateBatch::new(
        ConversationId::parse(CONVERSATION).unwrap(),
        BoardKey::Workspace(WorkspaceId::parse("ws-1").unwrap()),
        TaskCreator::Agent(actor),
        vec![create_plan],
    );
    let create = SqliteTaskCreateWrite::open(&path)
        .await
        .unwrap()
        .create_tasks(&create_batch)
        .await
        .unwrap();
    assert!(matches!(create, TaskCreateOutcome::AgentNotFound));

    let mut read = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
        .await
        .unwrap();
    let task_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tasks")
        .fetch_one(&mut read)
        .await
        .unwrap();
    let event_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE from_kind='system'")
            .fetch_one(&mut read)
            .await
            .unwrap();
    assert_eq!(task_count, 1, "no create row");
    assert_eq!(event_count, 0, "no mutation event");
}

#[tokio::test]
async fn all_existing_task_writers_replay_workspace_capabilities_before_any_write() {
    let (_directory, path, mut connection) = open_db(true).await;
    sqlx::query(
        "DELETE FROM workspace_agent_capabilities \
         WHERE workspace_id='ws-1' AND agent_id='agent-1' AND capability='write_tasks'",
    )
    .execute(&mut connection)
    .await
    .unwrap();
    sqlx::raw_sql("COMMIT")
        .execute(&mut connection)
        .await
        .unwrap();

    let names = AgentEventNames::new("agent-1", "Agent One");
    let actor = actor();
    let task = task();
    let status = SqliteTaskStatusWrite::open(&path)
        .await
        .unwrap()
        .apply_agent_status(&task, &actor, &names, TaskStatus::InProgress)
        .await
        .unwrap();
    assert!(matches!(status, AgentStatusWriteOutcome::NotFound));

    let claim_writer = SqliteTaskClaimWrite::open(&path).await.unwrap();
    let claim = claim_writer
        .agent_claim(&task, &actor, &names)
        .await
        .unwrap();
    assert!(matches!(claim, AgentClaimOutcome::NotFound));
    let unclaim = claim_writer.agent_unclaim(&task, &actor).await.unwrap();
    assert!(matches!(unclaim, UnclaimOutcome::NotFound));

    let edit = SqliteTaskEditWrite::open(&path)
        .await
        .unwrap()
        .apply_edit(
            &task,
            &TaskEditor::Agent(actor.clone()),
            Some(&names),
            Some(TaskTitle::parse("New").unwrap()),
            None,
        )
        .await
        .unwrap();
    assert!(matches!(edit, TaskEditWriteOutcome::NotFound));

    let mut connection = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO workspace_agent_capabilities (workspace_id,agent_id,capability) \
         VALUES ('ws-1','agent-1','write_tasks')",
    )
    .execute(&mut connection)
    .await
    .unwrap();
    sqlx::query(
        "DELETE FROM workspace_agent_capabilities \
         WHERE workspace_id='ws-1' AND agent_id='agent-1' AND capability='read_tasks'",
    )
    .execute(&mut connection)
    .await
    .unwrap();
    let edit = SqliteTaskEditWrite::open(&path)
        .await
        .unwrap()
        .apply_edit(
            &task,
            &TaskEditor::Agent(actor),
            Some(&names),
            Some(TaskTitle::parse("New").unwrap()),
            None,
        )
        .await
        .unwrap();
    assert!(matches!(edit, TaskEditWriteOutcome::NotFound));

    let row = sqlx::query("SELECT status,title,assignee_agent_id FROM tasks WHERE id='task-1'")
        .fetch_one(&mut connection)
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>("status"), "todo");
    assert_eq!(row.get::<String, _>("title"), "Old");
    assert_eq!(row.get::<Option<String>, _>("assignee_agent_id"), None);
    let event_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE from_kind='system'")
            .fetch_one(&mut connection)
            .await
            .unwrap();
    assert_eq!(event_count, 0, "capability refusals append no event");
}
