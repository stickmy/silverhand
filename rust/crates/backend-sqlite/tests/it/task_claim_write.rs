//! Real-SQLite gates for the task `claim` / `unclaim` write unit-of-work.
//!
//! Drives `SqliteTaskClaimWrite`: agent self-claim assigns a free non-terminal
//! task (todo -> in_progress); an already-taken task is AlreadyClaimed; a terminal
//! task is TaskDone/TaskClosed; a non-participant is NotFound. Agent unclaim is a
//! workflow release (clear assignee + rewind to todo), while human unclaim is an
//! assignment-only override; both refuse terminal. Plus the concurrency control: two agents racing to claim a
//! free task settle as exactly one Claimed and one AlreadyClaimed.

use silverhand_backend_core::conversation_messaging::messaging::task::{
    AgentClaimOutcome, AgentEventNames, HumanClaimOutcome, TaskClaimWrite, TaskId, TaskStatus,
    UnclaimOutcome,
};
use silverhand_backend_core::foundation::identity::{AgentId, UserId};
use silverhand_backend_sqlite::conversation_messaging::messaging::task::SqliteTaskClaimWrite;
use sqlx::{Connection, Row, SqliteConnection};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

use crate::current_schema;

const TASK: &str = "task-1";
const OWNER: &str = "agent-1";
const OTHER: &str = "agent-2";
const HUMAN: &str = "user-1";

/// Seed a workspace task with the given status + optional assignee, both agents as
/// participants and a human member.
async fn open_db(status: &str, assignee_agent: Option<&str>) -> (TempDir, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("tasks.sqlite");
    let mut connection = current_schema::create_current_schema(&path).await;
    for statement in [
        "INSERT INTO users (id, name, display_name, created_at, updated_at)
         VALUES ('user-1', 'user-1', 'User One', 1, 1)",
        "INSERT INTO hosts (id, name, kind, created_at, user_id)
         VALUES ('host-1', 'host-1', 'remote', 1, 'user-1')",
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
        sqlx::query(
            "INSERT INTO workspace_members (workspace_id, agent_id, joined_at)
             VALUES ('ws-1', ?, 1)",
        )
        .bind(id)
        .execute(&mut connection)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO workspace_agent_capabilities (workspace_id, agent_id, capability)
             VALUES ('ws-1', ?, 'write_tasks')",
        )
        .bind(id)
        .execute(&mut connection)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO conversation_agents (conversation_id, agent_id, joined_at)
             VALUES ('conv-1', ?, 1)",
        )
        .bind(id)
        .execute(&mut connection)
        .await
        .unwrap();
    }
    // `closed` requires closed_at per the schema CHECK; other statuses keep it null.
    let closed_at: Option<i64> = if status == "closed" { Some(50) } else { None };
    sqlx::query(
        "INSERT INTO tasks
            (id, workspace_id, task_number, conversation_id, title, status, assignee_agent_id,
             creator_kind, creator_agent_id, created_at, closed_at, board_key)
         VALUES ('task-1', 'ws-1', 1, 'conv-1', 'Do it', ?, ?, 'agent', 'agent-1', 1, ?, 'ws:ws-1')",
    )
    .bind(status)
    .bind(assignee_agent)
    .bind(closed_at)
    .execute(&mut connection)
    .await
    .unwrap();
    connection.close().await.unwrap();
    (directory, path)
}

async fn read_assignment(path: &Path) -> (String, Option<String>) {
    let mut connection = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
        .await
        .unwrap();
    let row = sqlx::query("SELECT status, assignee_agent_id FROM tasks WHERE id = ?")
        .bind(TASK)
        .fetch_one(&mut connection)
        .await
        .unwrap();
    (
        row.try_get::<String, _>("status").unwrap(),
        row.try_get::<Option<String>, _>("assignee_agent_id")
            .unwrap(),
    )
}

fn task_id() -> TaskId {
    TaskId::parse(TASK).unwrap()
}
fn owner() -> AgentId {
    AgentId::parse(OWNER).unwrap()
}
fn other() -> AgentId {
    AgentId::parse(OTHER).unwrap()
}

#[tokio::test]
async fn an_agent_claims_a_free_todo_and_it_moves_to_in_progress() {
    let (_dir, path) = open_db("todo", None).await;
    let writer = SqliteTaskClaimWrite::open(&path).await.unwrap();
    let outcome = writer
        .agent_claim(
            &task_id(),
            &owner(),
            &AgentEventNames::new("actor", "Actor"),
        )
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        AgentClaimOutcome::Claimed {
            status: TaskStatus::InProgress,
            ..
        }
    ));
    let (status, assignee) = read_assignment(&path).await;
    assert_eq!(status, "in_progress");
    assert_eq!(assignee.as_deref(), Some(OWNER));
}

#[tokio::test]
async fn claiming_an_already_taken_task_is_already_claimed() {
    let (_dir, path) = open_db("in_progress", Some(OTHER)).await;
    let writer = SqliteTaskClaimWrite::open(&path).await.unwrap();
    let outcome = writer
        .agent_claim(
            &task_id(),
            &owner(),
            &AgentEventNames::new("actor", "Actor"),
        )
        .await
        .unwrap();
    assert_eq!(outcome, AgentClaimOutcome::AlreadyClaimed);
    assert_eq!(read_assignment(&path).await.1.as_deref(), Some(OTHER));
}

#[tokio::test]
async fn claiming_a_terminal_task_is_task_done_or_closed() {
    for (status, expected) in [
        ("done", AgentClaimOutcome::TaskDone),
        ("closed", AgentClaimOutcome::TaskClosed),
    ] {
        let (_dir, path) = open_db(status, None).await;
        let writer = SqliteTaskClaimWrite::open(&path).await.unwrap();
        let outcome = writer
            .agent_claim(
                &task_id(),
                &owner(),
                &AgentEventNames::new("actor", "Actor"),
            )
            .await
            .unwrap();
        assert_eq!(outcome, expected);
        assert_eq!(read_assignment(&path).await.1, None);
    }
}

#[tokio::test]
async fn a_non_participant_claim_is_notfound() {
    let (_dir, path) = open_db("todo", None).await;
    {
        let mut c = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        sqlx::query(
            "DELETE FROM conversation_agents WHERE conversation_id='conv-1' AND agent_id=?",
        )
        .bind(OWNER)
        .execute(&mut c)
        .await
        .unwrap();
    }
    let writer = SqliteTaskClaimWrite::open(&path).await.unwrap();
    let outcome = writer
        .agent_claim(
            &task_id(),
            &owner(),
            &AgentEventNames::new("actor", "Actor"),
        )
        .await
        .unwrap();
    assert_eq!(outcome, AgentClaimOutcome::NotFound);
    assert_eq!(read_assignment(&path).await.1, None);
}

#[tokio::test]
async fn an_agent_cannot_unclaim_a_task_it_does_not_own() {
    let (_dir, path) = open_db("in_progress", Some(OTHER)).await;
    let writer = SqliteTaskClaimWrite::open(&path).await.unwrap();
    let outcome = writer.agent_unclaim(&task_id(), &owner()).await.unwrap();
    assert_eq!(outcome, UnclaimOutcome::NotAssignee);
    assert_eq!(read_assignment(&path).await.1.as_deref(), Some(OTHER));
}

#[tokio::test]
async fn unclaiming_a_terminal_task_is_refused() {
    // Assigned + done: the agent owns it but the terminal guard refuses.
    let (_dir, path) = open_db("done", Some(OWNER)).await;
    let writer = SqliteTaskClaimWrite::open(&path).await.unwrap();
    let outcome = writer.agent_unclaim(&task_id(), &owner()).await.unwrap();
    assert_eq!(outcome, UnclaimOutcome::TaskDone);
    assert_eq!(read_assignment(&path).await.1.as_deref(), Some(OWNER));
}

#[tokio::test]
async fn agent_and_human_unclaim_keep_their_distinct_workflow_rules() {
    for initial_status in ["in_progress", "in_review"] {
        let (_dir, agent_path) = open_db(initial_status, Some(OWNER)).await;
        let agent_writer = SqliteTaskClaimWrite::open(&agent_path).await.unwrap();
        let agent_outcome = agent_writer
            .agent_unclaim(&task_id(), &owner())
            .await
            .unwrap();
        let UnclaimOutcome::Unclaimed(agent_witness) = agent_outcome else {
            panic!("agent release must commit");
        };
        assert_eq!(agent_witness.status, TaskStatus::Todo);
        assert_eq!(read_assignment(&agent_path).await, ("todo".into(), None));

        let (_dir, human_path) = open_db(initial_status, Some(OTHER)).await;
        let human_writer = SqliteTaskClaimWrite::open(&human_path).await.unwrap();
        let human_outcome = human_writer
            .human_unclaim(&task_id(), &UserId::parse(HUMAN).unwrap())
            .await
            .unwrap();
        let UnclaimOutcome::Unclaimed(human_witness) = human_outcome else {
            panic!("human unassign must commit");
        };
        assert_eq!(human_witness.status.as_str(), initial_status);
        assert_eq!(
            read_assignment(&human_path).await,
            (initial_status.into(), None)
        );
    }
}

#[tokio::test]
async fn a_human_claim_assigns_a_participant_agent() {
    let (_dir, path) = open_db("todo", None).await;
    let writer = SqliteTaskClaimWrite::open(&path).await.unwrap();
    let outcome = writer
        .human_claim(&task_id(), &UserId::parse(HUMAN).unwrap(), &other())
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        HumanClaimOutcome::Claimed {
            status: TaskStatus::InProgress,
            ..
        }
    ));
    assert_eq!(read_assignment(&path).await.1.as_deref(), Some(OTHER));
}

#[tokio::test]
async fn human_claim_overwrites_agent_and_user_assignments_without_rewinding_workflow() {
    for status in ["in_progress", "in_review"] {
        let (_dir, path) = open_db(status, Some(OWNER)).await;
        let writer = SqliteTaskClaimWrite::open(&path).await.unwrap();
        let outcome = writer
            .human_claim(&task_id(), &UserId::parse(HUMAN).unwrap(), &other())
            .await
            .unwrap();
        let HumanClaimOutcome::Claimed {
            status: committed_status,
            witness,
        } = outcome
        else {
            panic!("human dispatch must overwrite an agent assignment");
        };
        assert_eq!(committed_status.as_str(), status);
        assert_eq!(witness.status.as_str(), status);
        assert_eq!(
            read_assignment(&path).await,
            (status.into(), Some(OTHER.into()))
        );
    }

    let (_dir, path) = open_db("in_progress", None).await;
    let mut connection = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
        .await
        .unwrap();
    sqlx::query("UPDATE tasks SET assignee_user_id=? WHERE id=?")
        .bind(HUMAN)
        .bind(TASK)
        .execute(&mut connection)
        .await
        .unwrap();
    connection.close().await.unwrap();
    let writer = SqliteTaskClaimWrite::open(&path).await.unwrap();
    let outcome = writer
        .human_claim(&task_id(), &UserId::parse(HUMAN).unwrap(), &other())
        .await
        .unwrap();
    assert!(matches!(outcome, HumanClaimOutcome::Claimed { .. }));
    assert_eq!(
        read_assignment(&path).await,
        ("in_progress".into(), Some(OTHER.into()))
    );
}

#[tokio::test]
async fn a_human_claim_to_a_non_participant_agent_is_refused() {
    let (_dir, path) = open_db("todo", None).await;
    // Remove OTHER's membership so it is not a valid assignee.
    {
        let mut c = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        sqlx::query(
            "DELETE FROM conversation_agents WHERE conversation_id='conv-1' AND agent_id=?",
        )
        .bind(OTHER)
        .execute(&mut c)
        .await
        .unwrap();
    }
    let writer = SqliteTaskClaimWrite::open(&path).await.unwrap();
    let outcome = writer
        .human_claim(&task_id(), &UserId::parse(HUMAN).unwrap(), &other())
        .await
        .unwrap();
    assert_eq!(outcome, HumanClaimOutcome::AssigneeNotConversationMember);
    assert_eq!(read_assignment(&path).await.1, None);
}

/// Concurrency control: two agents race to claim the SAME free task. The guarded
/// assign-if-free CAS makes exactly one win; the loser observes AlreadyClaimed, and
/// the task ends assigned to exactly one of them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_agents_racing_to_claim_settle_as_one_claimed_one_already_claimed() {
    let (_dir, path) = open_db("todo", None).await;
    let a = SqliteTaskClaimWrite::open(&path).await.unwrap();
    let b = SqliteTaskClaimWrite::open(&path).await.unwrap();
    let id = task_id();
    let owner_id = owner();
    let other_id = other();
    let names_a = AgentEventNames::new("a", "A");
    let names_b = AgentEventNames::new("b", "B");
    let (ra, rb) = tokio::join!(
        a.agent_claim(&id, &owner_id, &names_a),
        b.agent_claim(&id, &other_id, &names_b)
    );
    let ra = ra.unwrap();
    let rb = rb.unwrap();
    let claimed = [&ra, &rb]
        .iter()
        .filter(|o| matches!(o, AgentClaimOutcome::Claimed { .. }))
        .count();
    let already = [&ra, &rb]
        .iter()
        .filter(|o| matches!(o, AgentClaimOutcome::AlreadyClaimed))
        .count();
    assert_eq!(claimed, 1, "exactly one agent wins the claim");
    assert_eq!(
        already, 1,
        "the loser observes AlreadyClaimed, not a lock error"
    );
    let (status, assignee) = read_assignment(&path).await;
    assert_eq!(status, "in_progress");
    assert!(matches!(assignee.as_deref(), Some(OWNER) | Some(OTHER)));
}
