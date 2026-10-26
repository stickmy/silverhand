use silverhand_backend_core::{
    conversation_messaging::messaging::{MessageContent, ReferencePositions},
    conversation_messaging::runtime_delivery::{
        ClaimPendingInbox, DeliveryLeaseOwner, InboxSettlementDisposition,
        RuntimeDeliveryFailureReason, RuntimeDispatchReadPort, RuntimeInboxCustodyPort,
    },
    foundation::identity::{AgentId, ConversationId, DaemonInstanceId},
    foundation::persistence::PersistenceFailure,
    host_runtime::agent_presence,
    host_runtime::runtime_credential::{
        ClaimRuntimeCustody, RuntimeCustodyPort, RuntimeEventTime, RuntimeScope,
    },
    host_runtime::runtime_event::{
        CommitRuntimeConsumed, CommitRuntimeExit, CommitRuntimeSessionInit, DaemonEventDurablePort,
        ProjectRuntimeStatus, RuntimeActivityCommitResult, RuntimeActivityFact,
        RuntimeActivityKind, RuntimeActivityPayload, RuntimeEventAdmission, RuntimeExitDisposition,
        RuntimeObservedStatus,
    },
};
use silverhand_backend_sqlite::{
    conversation_messaging::runtime_delivery::SqliteRuntimeDeliveryStore,
    host_runtime::runtime_credential::SqliteRuntimeCustodyStore,
};
use sqlx::{Connection, Row, SqliteConnection, sqlite::SqliteConnectOptions};
use std::ops::Range;
use tempfile::TempDir;

use crate::current_schema;

const AGENT: &str = "agent-delivery";
const CONVERSATION: &str = "conversation0001";

struct NoReferences;

impl ReferencePositions for NoReferences {
    fn autolink_spans(&self, _content: &MessageContent) -> Vec<Range<usize>> {
        Vec::new()
    }
}

struct FixtureReferences;

impl ReferencePositions for FixtureReferences {
    fn autolink_spans(&self, content: &MessageContent) -> Vec<Range<usize>> {
        let body = content.as_str();
        let mut spans = Vec::new();
        let mut offset = 0;
        while let Some(start) = body[offset..].find("<silverhand://") {
            let start = offset + start;
            let Some(end) = body[start..].find('>') else {
                break;
            };
            spans.push(start..start + end + 1);
            offset = start + end + 1;
        }
        spans
    }
}

struct Fixture {
    _directory: TempDir,
    path: std::path::PathBuf,
    custody: SqliteRuntimeCustodyStore,
    delivery: SqliteRuntimeDeliveryStore,
}

async fn fixture() -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("runtime-delivery.sqlite");
    let mut connection = current_schema::create_current_schema(&path).await;
    seed(&mut connection).await;
    connection.close().await.unwrap();
    Fixture {
        _directory: directory,
        path: path.clone(),
        custody: SqliteRuntimeCustodyStore::open(&path).await.unwrap(),
        delivery: SqliteRuntimeDeliveryStore::open(&path).await.unwrap(),
    }
}

async fn seed(connection: &mut SqliteConnection) {
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
         ) VALUES (?, 'agent-delivery', 'codex', 'host-1', 1, 1,
             'Delivery Agent', 'user-1', 'conversation')",
    )
    .bind(AGENT)
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
        "INSERT INTO workspace_members (workspace_id, agent_id, joined_at)
         VALUES ('workspace-1', ?, 1)",
    )
    .bind(AGENT)
    .execute(&mut *connection)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversations (
            id, workspace_id, placement_kind, roster_policy, title, title_key,
            status, created_at, updated_at
         ) VALUES (?, 'workspace-1', 'workspace', 'open', 'Conversation One',
             'conversation one', 'active', 1, 1)",
    )
    .bind(CONVERSATION)
    .execute(&mut *connection)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversation_agents (conversation_id, agent_id, joined_at)
         VALUES (?, ?, 1)",
    )
    .bind(CONVERSATION)
    .bind(AGENT)
    .execute(&mut *connection)
    .await
    .unwrap();
    add_message(connection, "message-1", 1).await;
    add_message(connection, "message-2", 2).await;
    add_message(connection, "message-3", 3).await;
}

async fn add_message(connection: &mut SqliteConnection, id: &str, seq: i64) {
    sqlx::query(
        "INSERT INTO messages (
            id, conversation_id, from_kind, from_user_id, content, seq, created_at
         ) VALUES (?, ?, 'user', 'user-1', ?, ?, ?)",
    )
    .bind(id)
    .bind(CONVERSATION)
    .bind(format!("content-{seq}"))
    .bind(seq)
    .bind(seq)
    .execute(&mut *connection)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversation_inboxes (
            conversation_id, agent_id, message_id, created_at, delivery_kind
         ) VALUES (?, ?, ?, ?, 'turn')",
    )
    .bind(CONVERSATION)
    .bind(AGENT)
    .bind(id)
    .bind(seq)
    .execute(&mut *connection)
    .await
    .unwrap();
}

fn scope() -> RuntimeScope {
    RuntimeScope::ConversationSlot {
        agent: AgentId::parse(AGENT).unwrap(),
        source: ConversationId::parse(CONVERSATION).unwrap(),
    }
}

async fn claim_runtime(
    fixture: &Fixture,
    daemon: &str,
) -> silverhand_backend_core::host_runtime::runtime_credential::RuntimePrincipal {
    fixture
        .custody
        .claim_or_current(ClaimRuntimeCustody::new(
            scope(),
            DaemonInstanceId::parse(daemon).unwrap(),
            RuntimeEventTime::from_unix_millis(10),
        ))
        .await
        .unwrap()
        .unwrap()
        .into_principal()
}

fn owner(raw: &str) -> DeliveryLeaseOwner {
    DeliveryLeaseOwner::parse(raw).unwrap()
}

async fn open(path: &std::path::Path) -> SqliteConnection {
    SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(path)
            .foreign_keys(true),
    )
    .await
    .unwrap()
}

type InboxRow = (i64, Option<i64>, Option<i64>, Option<String>, Option<i64>);

#[tokio::test]
async fn target_projection_and_claim_are_bound_to_live_custody() {
    let fixture = fixture().await;
    let principal = claim_runtime(&fixture, "daemon-1").await;
    let target = fixture
        .delivery
        .resolve_dispatch_target(
            ConversationId::parse(CONVERSATION).unwrap(),
            AgentId::parse(AGENT).unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(target.host_id, "host-1");
    assert_eq!(target.plan.scope, scope());
    assert_eq!(
        target.plan.workspace.as_ref().unwrap().name(),
        "Workspace One"
    );

    let first = fixture
        .delivery
        .claim_pending(
            "host-1".into(),
            principal.clone(),
            ConversationId::parse(CONVERSATION).unwrap(),
            owner("owner-1"),
            100,
            50,
            &NoReferences,
        )
        .await
        .unwrap();
    let ClaimPendingInbox::Claimed(first) = first else {
        panic!("current custody must claim pending rows")
    };
    assert_eq!(first.deliveries().len(), 3);

    assert_eq!(
        fixture
            .delivery
            .claim_pending(
                "host-1".into(),
                principal,
                ConversationId::parse(CONVERSATION).unwrap(),
                owner("owner-2"),
                101,
                50,
                &NoReferences,
            )
            .await
            .unwrap(),
        ClaimPendingInbox::Empty
    );
}

#[tokio::test]
async fn direct_pair_and_thread_coordinates_are_projected_from_persisted_placement() {
    let fixture = fixture().await;
    let mut db = open(&fixture.path).await;
    sqlx::query("DELETE FROM conversation_inboxes WHERE conversation_id = ?")
        .bind(CONVERSATION)
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::query("DELETE FROM messages WHERE conversation_id = ?")
        .bind(CONVERSATION)
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::query("DELETE FROM conversation_agents WHERE conversation_id = ?")
        .bind(CONVERSATION)
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::query("DELETE FROM agent_runtime_slots WHERE conversation_id = ?")
        .bind(CONVERSATION)
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::query("DELETE FROM conversations WHERE id = ?")
        .bind(CONVERSATION)
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO conversations (
            id, workspace_id, placement_kind, roster_policy, title, title_key,
            status, created_at, updated_at, endpoint_a_kind, endpoint_a_id,
            endpoint_b_kind, endpoint_b_id
         ) VALUES (?, NULL, 'direct_pair', 'fixed', 'Direct Pair', NULL,
             'active', 1, 1, 'agent', ?, 'human', 'user-1')",
    )
    .bind(CONVERSATION)
    .bind(AGENT)
    .execute(&mut db)
    .await
    .unwrap();
    add_message(&mut db, "message-1", 1).await;
    add_message(&mut db, "message-2", 2).await;
    add_message(&mut db, "message-3", 3).await;
    sqlx::query(
        "INSERT INTO messages (
            id, conversation_id, from_kind, content, seq, created_at
         ) VALUES ('rootmessage00001', ?, 'system', 'root', 0, 0)",
    )
    .bind(CONVERSATION)
    .execute(&mut db)
    .await
    .unwrap();
    sqlx::query("UPDATE messages SET thread_root_id = 'rootmessage00001' WHERE id = 'message-2'")
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE messages
            SET from_kind = 'agent', from_user_id = NULL, from_agent_id = ?,
                content = '<silverhand://agent/agent-delivery> review'
          WHERE id = 'message-2'",
    )
    .bind(AGENT)
    .execute(&mut db)
    .await
    .unwrap();
    sqlx::query("UPDATE messages SET from_user_id = NULL WHERE id = 'message-1'")
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE messages
            SET from_kind = 'system', from_user_id = NULL, from_agent_id = NULL
          WHERE id = 'message-3'",
    )
    .execute(&mut db)
    .await
    .unwrap();
    db.close().await.unwrap();

    let principal = claim_runtime(&fixture, "daemon-1").await;
    let batch = fixture
        .delivery
        .claim_pending(
            "host-1".into(),
            principal,
            ConversationId::parse(CONVERSATION).unwrap(),
            owner("owner-direct"),
            100,
            50,
            &FixtureReferences,
        )
        .await
        .unwrap();
    let ClaimPendingInbox::Claimed(batch) = batch else {
        panic!("direct-pair inbox rows must be claimable")
    };
    assert!(
        batch.deliveries()[0]
            .presentation()
            .contains("target=dm:@user-1 ")
    );
    assert!(
        batch.deliveries()[0]
            .presentation()
            .contains("sender=@user-1 type=human")
    );
    assert!(
        batch.deliveries()[1]
            .presentation()
            .contains("target=dm:@user-1/thread:rootmessage00001 ")
    );
    assert!(
        batch.deliveries()[1]
            .presentation()
            .contains("sender=@agent-delivery type=agent computer=host-1")
    );
    assert_eq!(
        batch.deliveries()[1].instruction(),
        silverhand_backend_core::conversation_messaging::runtime_delivery::DeliveryInstruction::NonSystemMention
    );
    assert!(
        batch.deliveries()[2]
            .presentation()
            .contains("sender=system type=system")
    );
}

#[tokio::test]
async fn legacy_human_sender_fallback_requires_exactly_one_live_workspace_viewer() {
    let fixture = fixture().await;
    let mut db = open(&fixture.path).await;
    sqlx::query(
        "INSERT INTO users (id, name, display_name, created_at, updated_at)
         VALUES ('user-2', 'user-2', 'User Two', 2, 2)",
    )
    .execute(&mut db)
    .await
    .unwrap();
    for user in ["user-1", "user-2"] {
        sqlx::query(
            "INSERT INTO workspace_users (workspace_id, user_id, joined_at)
             VALUES ('workspace-1', ?, 1)",
        )
        .bind(user)
        .execute(&mut db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO conversation_users (conversation_id, user_id, joined_at)
             VALUES (?, ?, 1)",
        )
        .bind(CONVERSATION)
        .bind(user)
        .execute(&mut db)
        .await
        .unwrap();
    }
    sqlx::query("UPDATE messages SET from_user_id = NULL WHERE id = 'message-1'")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();

    let principal = claim_runtime(&fixture, "daemon-1").await;
    let conversation = ConversationId::parse(CONVERSATION).unwrap();
    let two_viewers = fixture
        .delivery
        .claim_pending(
            "host-1".into(),
            principal.clone(),
            conversation.clone(),
            owner("owner-two-viewers"),
            100,
            50,
            &NoReferences,
        )
        .await
        .unwrap();
    let ClaimPendingInbox::Claimed(two_viewers) = two_viewers else {
        panic!("pending rows must be claimed")
    };
    assert!(
        two_viewers.deliveries()[0]
            .presentation()
            .contains("sender=user type=human")
    );
    assert!(
        !two_viewers.deliveries()[0]
            .presentation()
            .contains("sender=@user-")
    );
    assert_eq!(
        fixture
            .delivery
            .release_owner(owner("owner-two-viewers"))
            .await
            .unwrap(),
        3
    );

    let mut db = open(&fixture.path).await;
    sqlx::query(
        "DELETE FROM workspace_users
          WHERE workspace_id = 'workspace-1' AND user_id = 'user-2'",
    )
    .execute(&mut db)
    .await
    .unwrap();
    db.close().await.unwrap();

    let one_viewer = fixture
        .delivery
        .claim_pending(
            "host-1".into(),
            principal,
            conversation,
            owner("owner-one-viewer"),
            101,
            50,
            &NoReferences,
        )
        .await
        .unwrap();
    let ClaimPendingInbox::Claimed(one_viewer) = one_viewer else {
        panic!("released rows must be claimable again")
    };
    assert!(
        one_viewer.deliveries()[0]
            .presentation()
            .contains("sender=@user-1 type=human")
    );
}

#[tokio::test]
async fn lease_expiry_disconnect_and_settlement_are_owner_scoped() {
    let fixture = fixture().await;
    let principal = claim_runtime(&fixture, "daemon-1").await;
    let conversation = ConversationId::parse(CONVERSATION).unwrap();
    let first = fixture
        .delivery
        .claim_pending(
            "host-1".into(),
            principal.clone(),
            conversation.clone(),
            owner("owner-1"),
            100,
            50,
            &NoReferences,
        )
        .await
        .unwrap();
    let ClaimPendingInbox::Claimed(first) = first else {
        panic!("first owner must claim")
    };
    assert_eq!(
        fixture
            .delivery
            .claim_pending(
                "host-1".into(),
                principal.clone(),
                conversation.clone(),
                owner("owner-too-early"),
                149,
                50,
                &NoReferences,
            )
            .await
            .unwrap(),
        ClaimPendingInbox::Empty
    );
    let second = fixture
        .delivery
        .claim_pending(
            "host-1".into(),
            principal.clone(),
            conversation.clone(),
            owner("owner-2"),
            150,
            50,
            &NoReferences,
        )
        .await
        .unwrap();
    let ClaimPendingInbox::Claimed(second) = second else {
        panic!("expired lease must be reclaimed")
    };
    assert_eq!(second.delivery_ids(), first.delivery_ids());
    assert_eq!(
        fixture
            .delivery
            .settle_consumed(
                "host-1".into(),
                principal.clone(),
                conversation.clone(),
                owner("owner-1"),
                first.delivery_ids(),
                160,
            )
            .await
            .unwrap(),
        InboxSettlementDisposition::Stale
    );
    assert_eq!(
        fixture
            .delivery
            .settle_consumed(
                "host-1".into(),
                principal.clone(),
                conversation.clone(),
                owner("owner-2"),
                second.delivery_ids(),
                161,
            )
            .await
            .unwrap(),
        InboxSettlementDisposition::Applied
    );
    assert_eq!(
        fixture
            .delivery
            .settle_consumed(
                "host-1".into(),
                principal,
                conversation,
                owner("owner-2"),
                second.delivery_ids(),
                162,
            )
            .await
            .unwrap(),
        InboxSettlementDisposition::Duplicate
    );
}

#[tokio::test]
async fn retriable_and_terminal_failures_have_distinct_durable_outcomes() {
    let fixture = fixture().await;
    let principal = claim_runtime(&fixture, "daemon-1").await;
    let conversation = ConversationId::parse(CONVERSATION).unwrap();
    let batch = fixture
        .delivery
        .claim_pending(
            "host-1".into(),
            principal.clone(),
            conversation.clone(),
            owner("owner-1"),
            100,
            50,
            &NoReferences,
        )
        .await
        .unwrap();
    let ClaimPendingInbox::Claimed(batch) = batch else {
        panic!("pending rows must be claimed")
    };
    assert_eq!(
        fixture
            .delivery
            .settle_failed(
                "host-1".into(),
                principal.clone(),
                conversation.clone(),
                owner("owner-1"),
                batch.delivery_ids(),
                RuntimeDeliveryFailureReason::ChildExit,
                110,
            )
            .await
            .unwrap(),
        InboxSettlementDisposition::Applied
    );
    let retry = fixture
        .delivery
        .claim_pending(
            "host-1".into(),
            principal.clone(),
            conversation.clone(),
            owner("owner-2"),
            111,
            50,
            &NoReferences,
        )
        .await
        .unwrap();
    let ClaimPendingInbox::Claimed(retry) = retry else {
        panic!("retriable failure must return rows to pending")
    };
    fixture
        .delivery
        .settle_failed(
            "host-1".into(),
            principal,
            conversation,
            owner("owner-2"),
            retry.delivery_ids(),
            RuntimeDeliveryFailureReason::DriverRefused,
            112,
        )
        .await
        .unwrap();
    let mut connection = open(&fixture.path).await;
    let row = sqlx::query(
        "SELECT COUNT(*) AS total, COUNT(failed_at) AS failed,
                COUNT(in_flight_owner) AS leased
           FROM conversation_inboxes",
    )
    .fetch_one(&mut connection)
    .await
    .unwrap();
    assert_eq!(row.get::<i64, _>("total"), 3);
    assert_eq!(row.get::<i64, _>("failed"), 3);
    assert_eq!(row.get::<i64, _>("leased"), 0);
}

#[tokio::test]
async fn successor_custody_rejects_stale_settlement_and_disconnect_releases_exact_owner() {
    let fixture = fixture().await;
    let stale = claim_runtime(&fixture, "daemon-1").await;
    let conversation = ConversationId::parse(CONVERSATION).unwrap();
    let batch = fixture
        .delivery
        .claim_pending(
            "host-1".into(),
            stale.clone(),
            conversation.clone(),
            owner("owner-1"),
            100,
            50,
            &NoReferences,
        )
        .await
        .unwrap();
    let ClaimPendingInbox::Claimed(batch) = batch else {
        panic!("pending rows must be claimed")
    };
    let successor = claim_runtime(&fixture, "daemon-2").await;
    assert_ne!(stale, successor);
    assert_eq!(
        fixture
            .delivery
            .settle_consumed(
                "host-1".into(),
                stale,
                conversation,
                owner("owner-1"),
                batch.delivery_ids(),
                110,
            )
            .await
            .unwrap(),
        InboxSettlementDisposition::Stale
    );
    assert_eq!(
        fixture
            .delivery
            .release_owner(owner("owner-1"))
            .await
            .unwrap(),
        3
    );
}

#[tokio::test]
async fn consumed_settlement_and_non_replayable_activity_are_one_root_transaction() {
    let fixture = fixture().await;
    let principal = claim_runtime(&fixture, "daemon-1").await;
    let conversation = ConversationId::parse(CONVERSATION).unwrap();
    let lease_owner = owner("owner-combined-consumed");
    let ClaimPendingInbox::Claimed(batch) = fixture
        .delivery
        .claim_pending(
            "host-1".into(),
            principal.clone(),
            conversation.clone(),
            lease_owner.clone(),
            100,
            1_000,
            &NoReferences,
        )
        .await
        .unwrap()
    else {
        panic!("pending rows must be claimed")
    };
    let input = CommitRuntimeConsumed::new(
        RuntimeEventAdmission::new("host-1".into(), principal.clone()),
        conversation.clone(),
        lease_owner.clone(),
        batch.delivery_ids(),
        RuntimeEventTime::from_unix_millis(200),
        RuntimeEventTime::from_unix_millis(201),
    );

    let mut db = open(&fixture.path).await;
    sqlx::raw_sql(
        "CREATE TRIGGER fail_message_received_activity
         BEFORE INSERT ON conversation_events
         WHEN NEW.kind = 'message_received'
         BEGIN SELECT RAISE(ABORT, 'activity-failure'); END;",
    )
    .execute(&mut db)
    .await
    .unwrap();
    assert!(
        fixture
            .delivery
            .commit_consumed(input.clone())
            .await
            .is_err()
    );
    let row = sqlx::query(
        "SELECT
             (SELECT COUNT(delivered_at) FROM conversation_inboxes) AS delivered,
             (SELECT COUNT(in_flight_owner) FROM conversation_inboxes) AS leased,
             (SELECT last_activity_at FROM agents WHERE id = ?) AS last_activity",
    )
    .bind(AGENT)
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(row.get::<i64, _>("delivered"), 0);
    assert_eq!(row.get::<i64, _>("leased"), 3);
    assert!(row.get::<Option<i64>, _>("last_activity").is_none());
    sqlx::raw_sql("DROP TRIGGER fail_message_received_activity;")
        .execute(&mut db)
        .await
        .unwrap();

    let applied = fixture
        .delivery
        .commit_consumed(input.clone())
        .await
        .unwrap();
    assert_eq!(applied.disposition(), InboxSettlementDisposition::Applied);
    assert!(applied.activity_id().is_some());
    let duplicate = fixture.delivery.commit_consumed(input).await.unwrap();
    assert_eq!(
        duplicate.disposition(),
        InboxSettlementDisposition::Duplicate
    );
    assert!(duplicate.activity_id().is_none());

    let row = sqlx::query(
        "SELECT
             (SELECT COUNT(*) FROM conversation_inboxes WHERE delivered_at = 200) AS delivered,
             (SELECT COUNT(*) FROM conversation_events WHERE kind = 'message_received') AS activities,
             (SELECT last_activity_at FROM agents WHERE id = ?) AS last_activity",
    )
    .bind(AGENT)
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(row.get::<i64, _>("delivered"), 3);
    assert_eq!(row.get::<i64, _>("activities"), 1);
    assert_eq!(row.get::<i64, _>("last_activity"), 201);
}

#[tokio::test]
async fn session_init_status_presence_and_activity_are_one_root_transaction() {
    let fixture = fixture().await;
    let principal = claim_runtime(&fixture, "daemon-session-init").await;
    let conversation = ConversationId::parse(CONVERSATION).unwrap();
    let input = CommitRuntimeSessionInit::new(
        RuntimeEventAdmission::new("host-1".into(), principal),
        conversation,
        AgentId::parse(AGENT).unwrap(),
        "cli-session-1".into(),
        RuntimeEventTime::from_unix_millis(200),
        RuntimeEventTime::from_unix_millis(201),
    );
    let mut db = open(&fixture.path).await;
    sqlx::raw_sql(
        "CREATE TRIGGER fail_session_init_activity
         BEFORE INSERT ON conversation_events
         WHEN NEW.kind = 'session_init'
         BEGIN SELECT RAISE(ABORT, 'session-init-activity-failure'); END;",
    )
    .execute(&mut db)
    .await
    .unwrap();
    assert!(
        fixture
            .delivery
            .commit_session_init(input.clone())
            .await
            .is_err()
    );
    let row = sqlx::query(
        "SELECT s.observed_status, s.cli_session_id, a.last_activity_at,
                (SELECT COUNT(*) FROM agent_runtime_events WHERE event_kind = 'projection_updated') AS projections,
                (SELECT COUNT(*) FROM conversation_events WHERE kind = 'session_init') AS activities
           FROM agent_runtime_slots s JOIN agents a ON a.id = s.agent_id
          WHERE s.conversation_id = ? AND s.agent_id = ?",
    )
    .bind(CONVERSATION)
    .bind(AGENT)
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(row.get::<String, _>("observed_status"), "starting");
    assert!(row.get::<Option<String>, _>("cli_session_id").is_none());
    assert!(row.get::<Option<i64>, _>("last_activity_at").is_none());
    assert_eq!(row.get::<i64, _>("projections"), 0);
    assert_eq!(row.get::<i64, _>("activities"), 0);
    sqlx::raw_sql("DROP TRIGGER fail_session_init_activity;")
        .execute(&mut db)
        .await
        .unwrap();

    let committed = fixture
        .delivery
        .commit_session_init(input)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(committed.status_targets().len(), 1);
    let row = sqlx::query(
        "SELECT s.observed_status, s.cli_session_id, a.last_activity_at,
                (SELECT COUNT(*) FROM conversation_events WHERE kind = 'session_init') AS activities
           FROM agent_runtime_slots s JOIN agents a ON a.id = s.agent_id
          WHERE s.conversation_id = ? AND s.agent_id = ?",
    )
    .bind(CONVERSATION)
    .bind(AGENT)
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(row.get::<String, _>("observed_status"), "active");
    assert_eq!(row.get::<String, _>("cli_session_id"), "cli-session-1");
    assert_eq!(row.get::<i64, _>("last_activity_at"), 201);
    assert_eq!(row.get::<i64, _>("activities"), 1);
}

#[tokio::test]
async fn agent_slot_status_targets_every_placement_participation_and_only_origin_carries_detail() {
    let fixture = fixture().await;
    let mut db = open(&fixture.path).await;
    sqlx::raw_sql(
        "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, display_name, created_by_user_id, session_scope)
         VALUES ('agent-aggregate', 'agent-aggregate', 'codex', 'host-1', 1, 1, 'Aggregate Agent', 'user-1', 'agent');
         INSERT INTO workspace_members (workspace_id, agent_id, joined_at) VALUES ('workspace-1', 'agent-aggregate', 1);
         INSERT INTO conversation_agents (conversation_id, agent_id, joined_at) VALUES ('conversation0001', 'agent-aggregate', 1);
         INSERT INTO conversations (id, workspace_id, placement_kind, roster_policy, title, title_key, status, created_at, updated_at)
         VALUES ('conversation0002', 'workspace-1', 'workspace', 'fixed', 'Second', 'second', 'active', 1, 1),
                ('conversation0003', 'workspace-1', 'workspace', 'open', 'Not Seated', 'not seated', 'active', 1, 1);
         INSERT INTO conversation_agents (conversation_id, agent_id, joined_at)
         VALUES ('conversation0002', 'agent-aggregate', 1);
         INSERT INTO conversations (id, workspace_id, placement_kind, roster_policy, title, title_key, status, created_at, updated_at,
                                    endpoint_a_kind, endpoint_a_id, endpoint_b_kind, endpoint_b_id)
         VALUES ('conversation0004', NULL, 'direct_pair', 'fixed', 'DM', NULL, 'active', 1, 1,
                 'agent', 'agent-aggregate', 'human', 'user-1');",
    )
    .execute(&mut db)
    .await
    .unwrap();
    let scope = RuntimeScope::AgentSlot {
        agent: AgentId::parse("agent-aggregate").unwrap(),
    };
    let principal = fixture
        .custody
        .claim_or_current(ClaimRuntimeCustody::new(
            scope,
            DaemonInstanceId::parse("daemon-agent-slot").unwrap(),
            RuntimeEventTime::from_unix_millis(10),
        ))
        .await
        .unwrap()
        .unwrap()
        .into_principal();
    let result = fixture
        .delivery
        .project_status(ProjectRuntimeStatus::new(
            RuntimeEventAdmission::new("host-1".into(), principal),
            ConversationId::parse(CONVERSATION).unwrap(),
            RuntimeObservedStatus::Active,
            None,
            RuntimeEventTime::from_unix_millis(20),
        ))
        .await
        .unwrap()
        .unwrap();
    let targets = result
        .targets()
        .iter()
        .map(|target| {
            (
                target.conversation().as_str(),
                target.carries_origin_detail(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        targets,
        vec![
            ("conversation0001", true),
            ("conversation0002", false),
            ("conversation0004", false),
        ]
    );
}

#[tokio::test]
async fn a_stale_exit_cannot_clear_the_successors_cli_session() {
    let fixture = fixture().await;
    let conversation = ConversationId::parse(CONVERSATION).unwrap();
    let old = claim_runtime(&fixture, "daemon-old").await;
    fixture
        .delivery
        .project_status(ProjectRuntimeStatus::new(
            RuntimeEventAdmission::new("host-1".into(), old.clone()),
            conversation.clone(),
            RuntimeObservedStatus::Active,
            Some("old-cli".into()),
            RuntimeEventTime::from_unix_millis(20),
        ))
        .await
        .unwrap()
        .unwrap();
    fixture
        .delivery
        .commit_exit(CommitRuntimeExit::new(
            RuntimeEventAdmission::new("host-1".into(), old.clone()),
            conversation.clone(),
            RuntimeObservedStatus::Crashed,
            false,
            RuntimeEventTime::from_unix_millis(21),
            None,
        ))
        .await
        .unwrap();
    let successor = claim_runtime(&fixture, "daemon-successor").await;
    fixture
        .delivery
        .project_status(ProjectRuntimeStatus::new(
            RuntimeEventAdmission::new("host-1".into(), successor),
            conversation.clone(),
            RuntimeObservedStatus::Active,
            Some("successor-cli".into()),
            RuntimeEventTime::from_unix_millis(22),
        ))
        .await
        .unwrap()
        .unwrap();
    let stale = fixture
        .delivery
        .commit_exit(CommitRuntimeExit::new(
            RuntimeEventAdmission::new("host-1".into(), old),
            conversation,
            RuntimeObservedStatus::Crashed,
            true,
            RuntimeEventTime::from_unix_millis(23),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(stale.disposition(), RuntimeExitDisposition::Stale);
    let mut db = open(&fixture.path).await;
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT cli_session_id FROM agent_runtime_slots WHERE conversation_id = ? AND agent_id = ?",
        )
        .bind(CONVERSATION)
        .bind(AGENT)
        .fetch_one(&mut db)
        .await
        .unwrap(),
        "successor-cli"
    );
}

#[tokio::test]
async fn exit_finalization_status_activity_and_lease_cleanup_are_one_root_transaction() {
    let fixture = fixture().await;
    let principal = claim_runtime(&fixture, "daemon-exit").await;
    let conversation = ConversationId::parse(CONVERSATION).unwrap();
    let lease_owner = owner("owner-exit");
    let ClaimPendingInbox::Claimed(_) = fixture
        .delivery
        .claim_pending(
            "host-1".into(),
            principal.clone(),
            conversation.clone(),
            lease_owner.clone(),
            100,
            1_000,
            &NoReferences,
        )
        .await
        .unwrap()
    else {
        panic!("pending rows must be claimed")
    };
    let pending = RuntimeActivityFact::new(
        conversation.clone(),
        AgentId::parse(AGENT).unwrap(),
        RuntimeActivityKind::Text,
        RuntimeActivityPayload::Text {
            text: "last fragment".into(),
        },
        RuntimeEventTime::from_unix_millis(300),
    );
    let input = CommitRuntimeExit::new(
        RuntimeEventAdmission::new("host-1".into(), principal.clone()),
        conversation,
        RuntimeObservedStatus::Crashed,
        false,
        RuntimeEventTime::from_unix_millis(300),
        Some(pending),
    );
    let mut db = open(&fixture.path).await;
    sqlx::raw_sql(
        "CREATE TRIGGER fail_runtime_finalize_event
         BEFORE INSERT ON agent_runtime_events
         WHEN NEW.event_kind = 'generation_finalized'
         BEGIN SELECT RAISE(ABORT, 'finalize-failure'); END;",
    )
    .execute(&mut db)
    .await
    .unwrap();
    assert!(fixture.delivery.commit_exit(input.clone()).await.is_err());
    let row = sqlx::query(
        "SELECT
             (SELECT claimed_by_daemon_instance_id FROM agent_runtime_slots
               WHERE conversation_id = ? AND agent_id = ?) AS daemon,
             (SELECT COUNT(in_flight_owner) FROM conversation_inboxes) AS leased,
             (SELECT COUNT(*) FROM conversation_events WHERE kind = 'text') AS activities",
    )
    .bind(CONVERSATION)
    .bind(AGENT)
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(row.get::<String, _>("daemon"), "daemon-exit");
    assert_eq!(row.get::<i64, _>("leased"), 3);
    assert_eq!(row.get::<i64, _>("activities"), 0);
    sqlx::raw_sql("DROP TRIGGER fail_runtime_finalize_event;")
        .execute(&mut db)
        .await
        .unwrap();

    let applied = fixture.delivery.commit_exit(input.clone()).await.unwrap();
    assert_eq!(applied.disposition(), RuntimeExitDisposition::Applied);
    assert!(applied.activity_id().is_some());
    let stale = fixture.delivery.commit_exit(input).await.unwrap();
    assert_eq!(stale.disposition(), RuntimeExitDisposition::Stale);
    let row = sqlx::query(
        "SELECT
             (SELECT claimed_by_daemon_instance_id FROM agent_runtime_slots
               WHERE conversation_id = ? AND agent_id = ?) AS daemon,
             (SELECT observed_status FROM agent_runtime_slots
               WHERE conversation_id = ? AND agent_id = ?) AS status,
             (SELECT COUNT(in_flight_owner) FROM conversation_inboxes) AS leased,
             (SELECT COUNT(*) FROM conversation_events WHERE kind = 'text') AS activities,
             (SELECT COUNT(*) FROM agent_runtime_events
               WHERE event_kind = 'generation_finalized') AS finalized",
    )
    .bind(CONVERSATION)
    .bind(AGENT)
    .bind(CONVERSATION)
    .bind(AGENT)
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert!(
        row.try_get::<Option<String>, _>("daemon")
            .unwrap()
            .is_none()
    );
    assert_eq!(row.get::<String, _>("status"), "crashed");
    assert_eq!(row.get::<i64, _>("leased"), 0);
    assert_eq!(row.get::<i64, _>("activities"), 1);
    assert_eq!(row.get::<i64, _>("finalized"), 1);
}

#[tokio::test]
async fn presence_audience_is_emitted_only_on_deterministic_away_to_online_edges() {
    let fixture = fixture().await;
    let principal = claim_runtime(&fixture, "daemon-presence").await;
    let fact = |created_at| {
        RuntimeActivityFact::new(
            ConversationId::parse(CONVERSATION).unwrap(),
            AgentId::parse(AGENT).unwrap(),
            RuntimeActivityKind::Text,
            RuntimeActivityPayload::Text {
                text: format!("presence-{created_at}"),
            },
            RuntimeEventTime::from_unix_millis(created_at),
        )
    };

    let first = fixture
        .delivery
        .record_activity(
            RuntimeEventAdmission::new("host-1".into(), principal.clone()),
            fact(1_000),
            RuntimeEventTime::from_unix_millis(1_000),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        first
            .presence_viewers()
            .iter()
            .map(|viewer| viewer.as_str())
            .collect::<Vec<_>>(),
        ["user-1"]
    );

    let continuous = fixture
        .delivery
        .record_activity(
            RuntimeEventAdmission::new("host-1".into(), principal.clone()),
            fact(2_000),
            RuntimeEventTime::from_unix_millis(2_000),
        )
        .await
        .unwrap()
        .unwrap();
    assert!(continuous.presence_viewers().is_empty());

    let recovered_at = 2_000 + agent_presence::AWAY_TTL_MS;
    let recovered = fixture
        .delivery
        .record_activity(
            RuntimeEventAdmission::new("host-1".into(), principal),
            fact(recovered_at),
            RuntimeEventTime::from_unix_millis(recovered_at),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        recovered
            .presence_viewers()
            .iter()
            .map(|viewer| viewer.as_str())
            .collect::<Vec<_>>(),
        ["user-1"]
    );
}

/// Walk the failure's source chain and return the offending value IF a link is the
/// named `ActivityInstantOutOfRange`. A falsifier uses this to prove the fault is the
/// shared authority's refusal — carrying the exact rejected value — rather than an
/// unrelated driver error that would fake-green a bare `is_err()`.
fn activity_out_of_range(error: &PersistenceFailure) -> Option<i64> {
    let mut current = std::error::Error::source(error);
    while let Some(link) = current {
        if let Some(out_of_range) = link.downcast_ref::<agent_presence::ActivityInstantOutOfRange>()
        {
            return Some(out_of_range.0);
        }
        current = link.source();
    }
    None
}

/// Drive `record_activity` for the fixture agent, using `observed` as BOTH the fact time
/// and the presence stamp — one knob per call. The fact time only matters on the success
/// path; the fault path aborts before the activity is inserted.
async fn record_presence(
    fixture: &Fixture,
    principal: &silverhand_backend_core::host_runtime::runtime_credential::RuntimePrincipal,
    observed: i64,
) -> Result<Option<RuntimeActivityCommitResult>, PersistenceFailure> {
    fixture
        .delivery
        .record_activity(
            RuntimeEventAdmission::new("host-1".into(), principal.clone()),
            RuntimeActivityFact::new(
                ConversationId::parse(CONVERSATION).unwrap(),
                AgentId::parse(AGENT).unwrap(),
                RuntimeActivityKind::Text,
                RuntimeActivityPayload::Text {
                    text: "wake".into(),
                },
                RuntimeEventTime::from_unix_millis(observed),
            ),
            RuntimeEventTime::from_unix_millis(observed),
        )
        .await
}

/// Read `agent-delivery`'s committed `last_activity_at` on a fresh connection.
async fn stored_stamp(fixture: &Fixture) -> Option<i64> {
    let mut db = open(&fixture.path).await;
    let stored =
        sqlx::query_scalar::<_, Option<i64>>("SELECT last_activity_at FROM agents WHERE id = ?")
            .bind(AGENT)
            .fetch_one(&mut db)
            .await
            .unwrap();
    db.close().await.unwrap();
    stored
}

/// Overwrite `agent-delivery`'s committed `last_activity_at` in place.
async fn set_stamp(fixture: &Fixture, stamp: i64) {
    let mut db = open(&fixture.path).await;
    sqlx::query("UPDATE agents SET last_activity_at = ? WHERE id = ?")
        .bind(stamp)
        .bind(AGENT)
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
}

#[tokio::test]
async fn a_corrupt_previous_activity_stamp_faults_instead_of_being_judged_online() {
    // The saturation the fix removes would have washed a corrupt committed stamp into
    // i64::MAX and judged the agent "still online", silently suppressing the away→online
    // presence edge. Seed exactly such a value — a raw stamp inside the JS-safe range
    // whose away deadline overflows it — and prove the writer now FAULTS on it through
    // the SAME authority the read uses: the NAMED refusal (not any driver error) fires,
    // the transaction does not commit, and a subsequent LEGAL write still succeeds so the
    // fault was the corruption, not an unconditional failure faking green.
    let fixture = fixture().await;
    let principal = claim_runtime(&fixture, "daemon-presence").await;

    let corrupt = agent_presence::MAX_SAFE_INTEGER;
    set_stamp(&fixture, corrupt).await;

    let error = record_presence(&fixture, &principal, 1_000)
        .await
        .expect_err("a corrupt previous stamp must fault, not be judged still-online");
    assert_eq!(
        activity_out_of_range(&error),
        Some(corrupt),
        "the fault must be the NAMED ActivityInstantOutOfRange carrying the corrupt value"
    );
    assert_eq!(
        stored_stamp(&fixture).await,
        Some(corrupt),
        "the faulting write must not commit, leaving the committed row untouched"
    );

    // Repair positive control: with a legal previous stamp the SAME call path commits.
    set_stamp(&fixture, 1_000).await;
    record_presence(&fixture, &principal, 2_000)
        .await
        .expect("a legal previous stamp must let the write commit")
        .expect("the principal is current, so the activity is recorded");
    assert_eq!(
        stored_stamp(&fixture).await,
        Some(2_000),
        "the repaired write must advance the committed stamp"
    );
}

#[tokio::test]
async fn an_invalid_observed_stamp_faults_before_it_can_be_written() {
    // The load-bearing guard: the writer must refuse to WRITE a stamp the read side is
    // bound to reject. With a perfectly legal previous stamp, an observation one past the
    // accepted domain (its away deadline would overflow the wire ceiling) must fault
    // through the NAMED authority BEFORE the UPDATE — nothing committed, no audience — and
    // the SAME fixture must accept a legal observation right after. Were the writer's
    // `ActivityInstant::new(observed_at)` guard dropped, this observation would be written
    // and this test would go red.
    let fixture = fixture().await;
    let principal = claim_runtime(&fixture, "daemon-presence").await;
    set_stamp(&fixture, 1_000).await;

    let invalid = agent_presence::ActivityInstant::MAX + 1;
    let error = record_presence(&fixture, &principal, invalid)
        .await
        .expect_err("an out-of-domain observation must fault before it is written");
    assert_eq!(
        activity_out_of_range(&error),
        Some(invalid),
        "the fault must be the NAMED ActivityInstantOutOfRange carrying the rejected \
         observation"
    );
    assert_eq!(
        stored_stamp(&fixture).await,
        Some(1_000),
        "the rejected observation must not be written: the legal previous stamp stands"
    );

    // Repair: a legal observation on the SAME fixture commits, proving the fault was the
    // observation's value and not an unconditional failure.
    record_presence(&fixture, &principal, 2_000)
        .await
        .expect("a legal observation must let the write commit")
        .expect("the principal is current, so the activity is recorded");
    assert_eq!(
        stored_stamp(&fixture).await,
        Some(2_000),
        "the repaired observation must advance the committed stamp"
    );
}

/// Whether any link of the failure chain is the adapter's unsupported-runtime fault.
/// The fault type is private to the adapter, so its rendering is what a consumer can
/// observe; a bare `is_err()` would be satisfied by any unrelated driver error.
fn unsupported_runtime(error: &PersistenceFailure) -> bool {
    let mut current = std::error::Error::source(error);
    while let Some(link) = current {
        if link.to_string() == "persisted runtime kind is unsupported" {
            return true;
        }
        current = link.source();
    }
    false
}

async fn resolve_target(
    fixture: &Fixture,
) -> Result<
    Option<
        silverhand_backend_core::conversation_messaging::runtime_delivery::RuntimeDispatchTarget,
    >,
    PersistenceFailure,
> {
    fixture
        .delivery
        .resolve_dispatch_target(
            ConversationId::parse(CONVERSATION).unwrap(),
            AgentId::parse(AGENT).unwrap(),
        )
        .await
}

/// `agents.runtime` carries no CHECK, so a token outside the closed set is storable,
/// and it reaches this adapter at TWO independent decode sites: the dispatch agent's
/// own spec, and every agent in the session roster. The spec decodes first, so one
/// corrupt row cannot tell them apart — a corrupt DISPATCH row proves the first site,
/// and a corrupt ROSTER PEER behind a legal dispatch row proves the second, where
/// only the member decode can be the fault. The repair in between is what rules out
/// an unconditional failure.
#[tokio::test]
async fn a_stored_runtime_outside_the_closed_set_fails_both_decode_sites() {
    let fixture = fixture().await;

    let mut db = open(&fixture.path).await;
    sqlx::query("UPDATE agents SET runtime = 'aider' WHERE id = ?")
        .bind(AGENT)
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    let error = resolve_target(&fixture)
        .await
        .expect_err("a dispatch agent whose stored runtime is outside the set is corruption");
    assert!(
        unsupported_runtime(&error),
        "the spec decode must fault through the named unsupported-runtime cause"
    );

    let mut db = open(&fixture.path).await;
    sqlx::query("UPDATE agents SET runtime = 'codex' WHERE id = ?")
        .bind(AGENT)
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    assert!(
        resolve_target(&fixture)
            .await
            .expect("the repaired row decodes")
            .is_some(),
        "the fault must be the stored value, not the read itself"
    );

    let mut db = open(&fixture.path).await;
    sqlx::query(
        "INSERT INTO agents (
            id, name, runtime, host_id, created_at, updated_at,
            display_name, created_by_user_id, session_scope
         ) VALUES ('agent-peer', 'agent-peer', 'aider', 'host-1', 1, 1,
             'Peer Agent', 'user-1', 'conversation')",
    )
    .execute(&mut db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversation_agents (conversation_id, agent_id, joined_at)
         VALUES (?, 'agent-peer', 1)",
    )
    .bind(CONVERSATION)
    .execute(&mut db)
    .await
    .unwrap();
    db.close().await.unwrap();
    let error = resolve_target(&fixture)
        .await
        .expect_err("a roster peer outside the set is corruption too");
    assert!(
        unsupported_runtime(&error),
        "the member decode must fault through the same named cause, with the dispatch \
         agent's own runtime legal"
    );
}

#[tokio::test]
async fn tool_refinement_never_rewrites_previous_generation_or_legacy_rows() {
    use silverhand_backend_core::host_runtime::{
        runtime_credential::{FinalizeRuntimeCustody, RuntimeFinalStatus},
        runtime_event::RefineRuntimeToolCallInput,
    };
    let fixture = fixture().await;
    let old = claim_runtime(&fixture, "daemon-old-tool").await;
    let record = |principal| {
        fixture.delivery.record_activity(
            RuntimeEventAdmission::new("host-1".into(), principal),
            RuntimeActivityFact::new(
                ConversationId::parse(CONVERSATION).unwrap(),
                AgentId::parse(AGENT).unwrap(),
                RuntimeActivityKind::ToolCall,
                RuntimeActivityPayload::ToolCall {
                    name: "shell".into(),
                    input_json: "{}".into(),
                    tool_call_id: Some("reused-tool-id".into()),
                },
                RuntimeEventTime::from_unix_millis(20),
            ),
            RuntimeEventTime::from_unix_millis(20),
        )
    };
    let old_id = record(old.clone()).await.unwrap().unwrap().id();
    fixture
        .custody
        .finalize(FinalizeRuntimeCustody::new(
            old.clone(),
            RuntimeFinalStatus::Idle,
            RuntimeEventTime::from_unix_millis(21),
        ))
        .await
        .unwrap();
    let current = claim_runtime(&fixture, "daemon-current-tool").await;
    assert!(current.custody().generation().as_i64() > old.custody().generation().as_i64());
    let refine = |principal| {
        fixture
            .delivery
            .refine_tool_call_input(RefineRuntimeToolCallInput::new(
                RuntimeEventAdmission::new("host-1".into(), principal),
                ConversationId::parse(CONVERSATION).unwrap(),
                AgentId::parse(AGENT).unwrap(),
                "reused-tool-id".into(),
                "{\"command\":\"current only\"}".into(),
                RuntimeEventTime::from_unix_millis(30),
            ))
    };
    assert!(
        refine(current.clone())
            .await
            .unwrap()
            .unwrap()
            .refined()
            .is_none()
    );
    let current_id = record(current.clone()).await.unwrap().unwrap().id();
    let mut db = open(&fixture.path).await;
    // A newer legacy row sharing the tool id must not hide the current call.
    let legacy_id:i64=sqlx::query_scalar("INSERT INTO conversation_events(conversation_id,agent_id,kind,payload,created_at) VALUES(?,?,'tool_call','{\"name\":\"shell\",\"tool_call_id\":\"reused-tool-id\",\"input\":{}}',40) RETURNING id")
        .bind(CONVERSATION).bind(AGENT).fetch_one(&mut db).await.unwrap();
    for _ in 0..2 {
        assert_eq!(
            refine(current.clone())
                .await
                .unwrap()
                .unwrap()
                .refined()
                .unwrap()
                .id(),
            current_id
        );
    }
    let current_payload: String =
        sqlx::query_scalar("SELECT payload FROM conversation_events WHERE id=?")
            .bind(current_id)
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&current_payload).unwrap()["input"],
        serde_json::json!({ "command": "current only" })
    );
    assert!(refine(old.clone()).await.unwrap().is_none());
    assert!(record(old).await.unwrap().is_none());
    for id in [old_id, legacy_id] {
        let payload: String =
            sqlx::query_scalar("SELECT payload FROM conversation_events WHERE id=?")
                .bind(id)
                .fetch_one(&mut db)
                .await
                .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&payload).unwrap()["input"],
            serde_json::json!({})
        );
    }
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM conversation_events WHERE agent_id=?")
            .bind(AGENT)
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(count, 3);
}

#[tokio::test]
async fn archive_blocks_existing_queue_without_changing_its_rows() {
    let fixture = fixture().await;
    let principal = claim_runtime(&fixture, "daemon-1").await;
    let mut connection = open(&fixture.path).await;
    let snapshot_sql = "SELECT id, delivered_at, failed_at, in_flight_owner, in_flight_at FROM conversation_inboxes ORDER BY id";
    let before: Vec<InboxRow> = sqlx::query_as(snapshot_sql)
        .fetch_all(&mut connection)
        .await
        .unwrap();
    assert!(!before.is_empty());

    // A caller may already hold a live dispatch projection when archive commits.
    assert!(
        fixture
            .delivery
            .resolve_dispatch_target(
                ConversationId::parse(CONVERSATION).unwrap(),
                AgentId::parse(AGENT).unwrap(),
            )
            .await
            .unwrap()
            .is_some()
    );

    sqlx::query("UPDATE conversations SET archived_at = 100 WHERE id = ?")
        .bind(CONVERSATION)
        .execute(&mut connection)
        .await
        .unwrap();
    assert!(
        fixture
            .delivery
            .resolve_dispatch_target(
                ConversationId::parse(CONVERSATION).unwrap(),
                AgentId::parse(AGENT).unwrap(),
            )
            .await
            .unwrap()
            .is_none()
    );

    let outcome = fixture
        .delivery
        .claim_pending(
            "host-1".into(),
            principal,
            ConversationId::parse(CONVERSATION).unwrap(),
            owner("owner-1"),
            200,
            50,
            &NoReferences,
        )
        .await
        .unwrap();
    assert_eq!(outcome, ClaimPendingInbox::Archived);
    let after: Vec<InboxRow> = sqlx::query_as(snapshot_sql)
        .fetch_all(&mut connection)
        .await
        .unwrap();
    assert_eq!(after, before);
}

/// The oldest backfilled reply names the gap before it, whether or not the root is
/// in this batch (already delivered, or authored by the recipient).
#[tokio::test]
async fn the_oldest_backfilled_reply_names_the_replies_omitted_before_it() {
    for root_case in ["observe", "delivered", "self"] {
        let fixture = fixture().await;
        let mut db = open(&fixture.path).await;
        sqlx::query("DELETE FROM conversation_inboxes")
            .execute(&mut db)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE conversations SET title = 'it''s #1', title_key = 'it''s #1' WHERE id = ?",
        )
        .bind(CONVERSATION)
        .execute(&mut db)
        .await
        .unwrap();
        let (from_kind, from_user, from_agent) = if root_case == "self" {
            ("agent", None, Some(AGENT))
        } else {
            ("user", Some("user-1"), None)
        };
        sqlx::query(
            "INSERT INTO messages (id, conversation_id, from_kind, from_user_id, from_agent_id,
                 content, seq, created_at)
             VALUES ('rootmessage00001', ?, ?, ?, ?, 'root', 10, 10)",
        )
        .bind(CONVERSATION)
        .bind(from_kind)
        .bind(from_user)
        .bind(from_agent)
        .execute(&mut db)
        .await
        .unwrap();
        for seq in 11..=15 {
            sqlx::query(
                "INSERT INTO messages (id, conversation_id, from_kind, from_user_id, content, seq,
                     created_at, thread_root_id)
                 VALUES (?, ?, 'user', 'user-1', 'reply', ?, ?, 'rootmessage00001')",
            )
            .bind(format!("reply-{seq}"))
            .bind(CONVERSATION)
            .bind(seq)
            .bind(seq)
            .execute(&mut db)
            .await
            .unwrap();
        }
        let mut backfilled = vec![("reply-14", None), ("reply-15", None)];
        match root_case {
            "observe" => backfilled.insert(0, ("rootmessage00001", None)),
            "delivered" => backfilled.insert(0, ("rootmessage00001", Some(5_i64))),
            _ => {}
        }
        for (id, delivered_at) in backfilled {
            sqlx::query(
                "INSERT INTO conversation_inboxes (conversation_id, agent_id, message_id,
                     delivered_at, created_at, delivery_kind) VALUES (?, ?, ?, ?, 20, 'observe')",
            )
            .bind(CONVERSATION)
            .bind(AGENT)
            .bind(id)
            .bind(delivered_at)
            .execute(&mut db)
            .await
            .unwrap();
        }
        db.close().await.unwrap();

        let principal = claim_runtime(&fixture, "daemon-1").await;
        let ClaimPendingInbox::Claimed(batch) = fixture
            .delivery
            .claim_pending(
                "host-1".into(),
                principal,
                ConversationId::parse(CONVERSATION).unwrap(),
                owner("owner-omitted"),
                100,
                50,
                &NoReferences,
            )
            .await
            .unwrap()
        else {
            panic!("backfilled rows must be claimable")
        };
        let notes: Vec<bool> = batch
            .deliveries()
            .iter()
            .map(|delivery| delivery.presentation().contains("not included"))
            .collect();
        let first_reply = if root_case == "observe" { 1 } else { 0 };
        assert_eq!(notes.iter().filter(|note| **note).count(), 1, "{root_case}");
        let note = batch.deliveries()[first_reply].presentation();
        assert!(
            note.ends_with(
                "reply\n\n[3 earlier replies in this thread are not included. Read them with `silverhand message history --target '#Workspace%20One/it'\\''s%20#1/thread:rootmessage00001' --before-seq 14`.]"
            ),
            "{root_case}: {note}"
        );
    }
}

#[tokio::test]
async fn pending_turn_is_reported_only_for_unclaimed_turn_rows_of_current_custody() {
    let fixture = fixture().await;
    let principal = claim_runtime(&fixture, "daemon-1").await;
    let admission = RuntimeEventAdmission::new("host-1".into(), principal.clone());
    let conversation = ConversationId::parse(CONVERSATION).unwrap();
    assert!(
        fixture
            .delivery
            .has_pending_turn(admission.clone(), conversation.clone())
            .await
            .unwrap()
    );
    let mut db = open(&fixture.path).await;
    sqlx::query(
        "UPDATE conversation_inboxes SET delivery_kind = 'observe' WHERE message_id = 'message-3'",
    )
    .execute(&mut db)
    .await
    .unwrap();
    sqlx::query("UPDATE conversation_inboxes SET in_flight_owner = 'other', in_flight_at = 100 WHERE message_id = 'message-1'")
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE conversation_inboxes SET delivered_at = 100 WHERE message_id = 'message-2'",
    )
    .execute(&mut db)
    .await
    .unwrap();
    db.close().await.unwrap();
    assert!(
        !fixture
            .delivery
            .has_pending_turn(admission, conversation)
            .await
            .unwrap(),
        "observe-only, in-flight and delivered rows owe no run"
    );
}
