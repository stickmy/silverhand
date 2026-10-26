use std::{error::Error, path::Path};

use silverhand_backend_core::{
    foundation::identity::{AgentId, ConversationId, DaemonInstanceId},
    host_runtime::runtime_credential::{
        ClaimRuntimeCustody, FinalizeRuntimeCustody, RuntimeCustody, RuntimeCustodyPort,
        RuntimeEventTime, RuntimeFinalStatus, RuntimeGeneration, RuntimePrincipal, RuntimeScope,
    },
};
use silverhand_backend_sqlite::host_runtime::runtime_credential::SqliteRuntimeCustodyStore;
use sqlx::{Connection, Row, SqliteConnection, sqlite::SqliteConnectOptions};
use tempfile::TempDir;

use crate::current_schema;

const CLAIMED_AT: RuntimeEventTime = RuntimeEventTime::from_unix_millis(100);
const FINALIZED_AT: RuntimeEventTime = RuntimeEventTime::from_unix_millis(200);

async fn fixture() -> (TempDir, std::path::PathBuf, SqliteRuntimeCustodyStore) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("runtime-custody.sqlite");
    let mut connection = current_schema::create_current_schema(&path).await;
    seed_runtime_slots(&mut connection).await;
    connection.close().await.unwrap();
    let store = SqliteRuntimeCustodyStore::open(&path).await.unwrap();
    (directory, path, store)
}

async fn seed_runtime_slots(connection: &mut SqliteConnection) {
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
    sqlx::query(
        "INSERT INTO agents (
            id, name, runtime, host_id, created_at, updated_at,
            display_name, created_by_user_id, session_scope
         ) VALUES
            ('agent-conversation', 'agent-conversation', 'codex', 'host-1', 1, 1,
             'Conversation Agent', 'user-1', 'conversation'),
            ('agent-global', 'agent-global', 'codex', 'host-1', 1, 1,
             'Global Agent', 'user-1', 'agent')",
    )
    .execute(&mut *connection)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO workspaces (
            id, name, name_key, created_at, updated_at, owner_user_id
         ) VALUES ('workspace-1', 'Workspace One', 'workspace one', 1, 1, 'user-1')",
    )
    .execute(&mut *connection)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversations (
            id, workspace_id, placement_kind, roster_policy, title, title_key,
            status, created_at, updated_at
         ) VALUES (
            'source-1', 'workspace-1', 'workspace', 'open', 'Source One',
            'source one', 'active', 1, 1
         )",
    )
    .execute(&mut *connection)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversation_agents (conversation_id, agent_id, joined_at)
         VALUES ('source-1', 'agent-conversation', 1)",
    )
    .execute(&mut *connection)
    .await
    .unwrap();
}

fn conversation_scope() -> RuntimeScope {
    RuntimeScope::ConversationSlot {
        agent: AgentId::parse("agent-conversation").unwrap(),
        source: ConversationId::parse("source-1").unwrap(),
    }
}

fn agent_scope() -> RuntimeScope {
    RuntimeScope::AgentSlot {
        agent: AgentId::parse("agent-global").unwrap(),
    }
}

fn probe_principal() -> RuntimePrincipal {
    RuntimePrincipal::for_scope(
        conversation_scope(),
        RuntimeCustody::new(
            RuntimeGeneration::new(1).unwrap(),
            DaemonInstanceId::parse("daemon-1").unwrap(),
        ),
    )
}

fn claim(scope: RuntimeScope, daemon: &str, time: RuntimeEventTime) -> ClaimRuntimeCustody {
    ClaimRuntimeCustody::new(scope, DaemonInstanceId::parse(daemon).unwrap(), time)
}

async fn open(path: &Path) -> SqliteConnection {
    SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(path)
            .foreign_keys(true),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn claim_current_and_successor_are_generation_bound_and_evented_once() {
    let (_directory, path, store) = fixture().await;
    let first = store
        .claim_or_current(claim(conversation_scope(), "daemon-1", CLAIMED_AT))
        .await
        .unwrap()
        .unwrap();
    assert!(first.is_newly_claimed());
    assert_eq!(first.principal().custody().generation().as_i64(), 1);
    assert_eq!(first.principal().source().unwrap().as_str(), "source-1");
    assert!(store.is_current(first.principal().clone()).await.unwrap());

    let same = store
        .claim_or_current(claim(
            conversation_scope(),
            "daemon-1",
            RuntimeEventTime::from_unix_millis(101),
        ))
        .await
        .unwrap()
        .unwrap();
    assert!(!same.is_newly_claimed());
    assert_eq!(same.principal(), first.principal());

    let successor = store
        .claim_or_current(claim(
            conversation_scope(),
            "daemon-2",
            RuntimeEventTime::from_unix_millis(102),
        ))
        .await
        .unwrap()
        .unwrap();
    assert!(successor.is_newly_claimed());
    assert_eq!(successor.principal().custody().generation().as_i64(), 2);
    assert!(!store.is_current(first.into_principal()).await.unwrap());
    assert!(store.is_current(successor.into_principal()).await.unwrap());

    let mut connection = open(&path).await;
    let events = sqlx::query(
        "SELECT generation, daemon_instance_id, occurred_at
           FROM agent_runtime_events
          WHERE agent_id = 'agent-conversation' AND event_kind = 'generation_claimed'
          ORDER BY id",
    )
    .fetch_all(&mut connection)
    .await
    .unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].get::<i64, _>("generation"), 1);
    assert_eq!(events[0].get::<String, _>("daemon_instance_id"), "daemon-1");
    assert_eq!(events[0].get::<i64, _>("occurred_at"), 100);
    assert_eq!(events[1].get::<i64, _>("generation"), 2);
    assert_eq!(events[1].get::<String, _>("daemon_instance_id"), "daemon-2");
    assert_eq!(events[1].get::<i64, _>("occurred_at"), 102);
}

#[tokio::test]
async fn agent_slot_has_no_ambient_source_and_uses_its_null_scope_row() {
    let (_directory, _path, store) = fixture().await;
    let claimed = store
        .claim_or_current(claim(agent_scope(), "daemon-agent", CLAIMED_AT))
        .await
        .unwrap()
        .unwrap();

    assert!(claimed.is_newly_claimed());
    assert_eq!(claimed.principal().agent().as_str(), "agent-global");
    assert_eq!(claimed.principal().source(), None);
    assert!(matches!(
        claimed.principal(),
        RuntimePrincipal::AgentSlot { .. }
    ));
    assert!(store.is_current(claimed.into_principal()).await.unwrap());
}

#[tokio::test]
async fn stale_finalize_cannot_clear_a_successor_and_exact_finalize_commits_its_event() {
    let (_directory, path, store) = fixture().await;
    let stale = store
        .claim_or_current(claim(conversation_scope(), "daemon-1", CLAIMED_AT))
        .await
        .unwrap()
        .unwrap()
        .into_principal();
    let successor = store
        .claim_or_current(claim(
            conversation_scope(),
            "daemon-2",
            RuntimeEventTime::from_unix_millis(110),
        ))
        .await
        .unwrap()
        .unwrap()
        .into_principal();

    assert!(
        !store
            .finalize(FinalizeRuntimeCustody::new(
                stale,
                RuntimeFinalStatus::Crashed,
                FINALIZED_AT,
            ))
            .await
            .unwrap()
    );
    assert!(store.is_current(successor.clone()).await.unwrap());
    assert!(
        store
            .finalize(FinalizeRuntimeCustody::new(
                successor.clone(),
                RuntimeFinalStatus::Idle,
                FINALIZED_AT,
            ))
            .await
            .unwrap()
    );
    assert!(!store.is_current(successor).await.unwrap());

    let mut connection = open(&path).await;
    let event = sqlx::query(
        "SELECT generation, daemon_instance_id, observed_status, occurred_at
           FROM agent_runtime_events
          WHERE agent_id = 'agent-conversation' AND event_kind = 'generation_finalized'",
    )
    .fetch_one(&mut connection)
    .await
    .unwrap();
    assert_eq!(event.get::<i64, _>("generation"), 2);
    assert!(
        event
            .get::<Option<String>, _>("daemon_instance_id")
            .is_none()
    );
    assert_eq!(event.get::<String, _>("observed_status"), "idle");
    assert_eq!(event.get::<i64, _>("occurred_at"), 200);
}

#[tokio::test]
async fn claim_and_finalize_roll_back_when_their_event_cannot_append() {
    let (_directory, path, store) = fixture().await;
    let mut connection = open(&path).await;
    sqlx::query(
        "CREATE TRIGGER fail_claim_event
         BEFORE INSERT ON agent_runtime_events
         WHEN NEW.event_kind = 'generation_claimed'
         BEGIN SELECT RAISE(ABORT, 'claim event refused'); END",
    )
    .execute(&mut connection)
    .await
    .unwrap();
    connection.close().await.unwrap();

    let failure = store
        .claim_or_current(claim(conversation_scope(), "daemon-1", CLAIMED_AT))
        .await
        .unwrap_err();
    assert!(error_chain(&failure).contains("claim event refused"));
    assert!(store.current(conversation_scope()).await.unwrap().is_none());

    let mut connection = open(&path).await;
    sqlx::query("DROP TRIGGER fail_claim_event")
        .execute(&mut connection)
        .await
        .unwrap();
    connection.close().await.unwrap();
    let principal = store
        .claim_or_current(claim(conversation_scope(), "daemon-1", CLAIMED_AT))
        .await
        .unwrap()
        .unwrap()
        .into_principal();

    let mut connection = open(&path).await;
    sqlx::query(
        "CREATE TRIGGER fail_finalize_event
         BEFORE INSERT ON agent_runtime_events
         WHEN NEW.event_kind = 'generation_finalized'
         BEGIN SELECT RAISE(ABORT, 'finalize event refused'); END",
    )
    .execute(&mut connection)
    .await
    .unwrap();
    connection.close().await.unwrap();
    let failure = store
        .finalize(FinalizeRuntimeCustody::new(
            principal.clone(),
            RuntimeFinalStatus::Crashed,
            FINALIZED_AT,
        ))
        .await
        .unwrap_err();
    assert!(error_chain(&failure).contains("finalize event refused"));
    assert!(store.is_current(principal).await.unwrap());
}

#[tokio::test]
async fn empty_persisted_daemon_fails_current_verify_and_finalize_with_the_same_chain() {
    let (_directory, path, store) = fixture().await;
    let mut connection = open(&path).await;
    sqlx::query(
        "UPDATE agent_runtime_slots
            SET generation = generation + 1,
                claimed_by_daemon_instance_id = ''
          WHERE agent_id = 'agent-conversation' AND conversation_id = 'source-1'",
    )
    .execute(&mut connection)
    .await
    .unwrap();
    connection.close().await.unwrap();

    assert_corruption_crosses_current_verify_and_finalize(
        &store,
        "persisted daemon instance id is invalid: daemon instance id must not be empty",
    )
    .await;
}

#[tokio::test]
async fn invalid_persisted_generation_fails_current_verify_and_finalize_with_the_same_chain() {
    let (_directory, path, store) = fixture().await;
    let mut connection = open(&path).await;
    sqlx::query("DROP TRIGGER agent_runtime_slots_generation_monotonic")
        .execute(&mut connection)
        .await
        .unwrap();
    sqlx::query("DROP TRIGGER agent_runtime_slots_daemon_claim_generation_bound")
        .execute(&mut connection)
        .await
        .unwrap();
    sqlx::query("PRAGMA ignore_check_constraints = ON")
        .execute(&mut connection)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE agent_runtime_slots
            SET generation = -1,
                claimed_by_daemon_instance_id = 'daemon-corrupt'
          WHERE agent_id = 'agent-conversation' AND conversation_id = 'source-1'",
    )
    .execute(&mut connection)
    .await
    .unwrap();
    connection.close().await.unwrap();

    assert_corruption_crosses_current_verify_and_finalize(
        &store,
        "persisted runtime generation is invalid: runtime generation must be nonnegative",
    )
    .await;
}

#[tokio::test]
async fn real_driver_failure_reaches_the_verifier_entrypoint() {
    let (_directory, path, store) = fixture().await;
    let mut connection = open(&path).await;

    sqlx::query("DROP TABLE agent_runtime_slots")
        .execute(&mut connection)
        .await
        .unwrap();
    connection.close().await.unwrap();
    let failure = store.is_current(probe_principal()).await.unwrap_err();
    assert!(error_chain(&failure).contains("no such table: agent_runtime_slots"));
}

async fn assert_corruption_crosses_current_verify_and_finalize(
    store: &SqliteRuntimeCustodyStore,
    expected_chain: &str,
) {
    let current = store.current(conversation_scope()).await.unwrap_err();
    let verification = store.is_current(probe_principal()).await.unwrap_err();
    let finalization = store
        .finalize(FinalizeRuntimeCustody::new(
            probe_principal(),
            RuntimeFinalStatus::Crashed,
            FINALIZED_AT,
        ))
        .await
        .unwrap_err();

    assert_eq!(
        error_chain(&current),
        format!("persistence operation failed: {expected_chain}")
    );
    assert_eq!(error_chain(&verification), error_chain(&current));
    assert_eq!(error_chain(&finalization), error_chain(&current));
}

#[tokio::test]
async fn verify_linearizes_at_the_live_custody_read_not_across_a_product_request() {
    let (_directory, path, store) = fixture().await;
    let principal = store
        .claim_or_current(claim(conversation_scope(), "daemon-1", CLAIMED_AT))
        .await
        .unwrap()
        .unwrap()
        .into_principal();

    let mut connection = open(&path).await;
    let mut finalization = sqlx::Connection::begin_with(&mut connection, "BEGIN IMMEDIATE")
        .await
        .unwrap();
    sqlx::query(
        "UPDATE agent_runtime_slots
            SET claimed_by_daemon_instance_id = NULL,
                observed_status = 'idle',
                observed_pid = NULL
          WHERE agent_id = 'agent-conversation'
            AND conversation_id = 'source-1'
            AND generation = 1
            AND claimed_by_daemon_instance_id = 'daemon-1'",
    )
    .execute(&mut *finalization)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO agent_runtime_events (
            slot_id, agent_id, conversation_id, generation, event_kind,
            daemon_instance_id, observed_status, observed_pid, cli_session_id, occurred_at
         ) SELECT id, agent_id, conversation_id, generation, 'generation_finalized',
                  NULL, 'idle', NULL, cli_session_id, 200
             FROM agent_runtime_slots
            WHERE agent_id = 'agent-conversation' AND conversation_id = 'source-1'",
    )
    .execute(&mut *finalization)
    .await
    .unwrap();

    assert!(store.is_current(principal.clone()).await.unwrap());
    finalization.commit().await.unwrap();
    assert!(!store.is_current(principal).await.unwrap());
}

fn error_chain(error: &(dyn Error + 'static)) -> String {
    let mut messages = vec![error.to_string()];
    let mut current = error.source();
    while let Some(cause) = current {
        messages.push(cause.to_string());
        current = cause.source();
    }
    messages.join(": ")
}
