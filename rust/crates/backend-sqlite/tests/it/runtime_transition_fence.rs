use std::ops::Range;

use silverhand_backend_core::{
    conversation_messaging::messaging::{MessageContent, ReferencePositions},
    conversation_messaging::runtime_delivery::{
        ClaimPendingInbox, DeliveryLeaseOwner, RuntimeInboxCustodyPort,
    },
    foundation::identity::{AgentId, ConversationId, DaemonInstanceId},
    host_runtime::runtime_credential::{
        ClaimRuntimeCustody, RuntimeCustodyPort, RuntimeEventTime, RuntimeScope,
    },
    host_runtime::runtime_event::RuntimeObservedStatus,
    host_runtime::runtime_transition::{
        AbortTransition, BeginTransition, ReadRuntimeTransitionSeats,
        ReclaimRuntimeTransitionInboxes, RuntimeTransitionWrite, abort_runtime_transition,
        begin_runtime_transition, read_runtime_transition_seats,
        reclaim_runtime_transition_inboxes,
    },
    identity_access::agents::runtime::RuntimeKind,
};
use silverhand_backend_sqlite::{
    conversation_messaging::runtime_delivery::SqliteRuntimeDeliveryStore,
    foundation::host_agent_store::SqliteHostAgentStore,
    host_runtime::runtime_credential::SqliteRuntimeCustodyStore,
};
use sqlx::{Connection, Executor, Row, SqliteConnection};

use crate::current_schema;

const AGENT: &str = "transition-agent";
const C1: &str = "conversation0001";
const C2: &str = "conversation0002";

struct NoReferences;

impl ReferencePositions for NoReferences {
    fn autolink_spans(&self, _content: &MessageContent) -> Vec<Range<usize>> {
        Vec::new()
    }
}

struct Fixture {
    _directory: tempfile::TempDir,
    path: std::path::PathBuf,
    custody: SqliteRuntimeCustodyStore,
    delivery: SqliteRuntimeDeliveryStore,
    transition: SqliteHostAgentStore,
}

fn agent() -> AgentId {
    AgentId::parse(AGENT).unwrap()
}

fn conversation(raw: &str) -> ConversationId {
    ConversationId::parse(raw).unwrap()
}

fn scope() -> RuntimeScope {
    RuntimeScope::ConversationSlot {
        agent: agent(),
        source: conversation(C1),
    }
}

async fn fixture() -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("runtime-transition-fence.sqlite");
    let mut db = current_schema::create_current_schema(&path).await;
    db.execute(sqlx::raw_sql(
        "INSERT INTO users(id,name,display_name,created_at,updated_at) \
             VALUES('user-1','owner','Owner',0,0); \
         INSERT INTO hosts(id,name,kind,created_at,user_id) \
             VALUES('host-1','Host','remote',0,'user-1'); \
         INSERT INTO agents(id,name,runtime,host_id,created_at,updated_at,display_name, \
             created_by_user_id,materialization_status,session_scope) \
             VALUES('transition-agent','agent','codex','host-1',0,0,'Agent', \
                    'user-1','active','conversation'); \
         INSERT INTO workspaces(id,name,name_key,owner_user_id,created_at,updated_at) \
             VALUES('workspace-1','Space','space','user-1',0,0); \
         INSERT INTO workspace_members(workspace_id,agent_id,joined_at) \
             VALUES('workspace-1','transition-agent',0); \
         INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key, \
             status,visibility,created_at,updated_at,latest_message_seq) VALUES \
             ('conversation0001','workspace-1','workspace','open','One','one', \
              'open','private',0,0,1), \
             ('conversation0002','workspace-1','workspace','open','Two','two', \
              'open','private',0,0,1); \
         INSERT INTO conversation_agents(conversation_id,agent_id,joined_at) VALUES \
             ('conversation0001','transition-agent',0), \
             ('conversation0002','transition-agent',0); \
         INSERT INTO messages(id,conversation_id,from_kind,from_user_id,content,seq,created_at) VALUES \
             ('message-1','conversation0001','user','user-1','one',1,1), \
             ('message-2','conversation0002','user','user-1','two',1,1); \
         INSERT INTO conversation_inboxes(conversation_id,agent_id,message_id,created_at,delivery_kind, \
             in_flight_owner,in_flight_at) VALUES \
             ('conversation0001','transition-agent','message-1',1,'turn','old-owner',1), \
             ('conversation0002','transition-agent','message-2',1,'turn','old-owner',1);",
    ))
    .await
    .unwrap();
    db.close().await.unwrap();
    let fixture = Fixture {
        _directory: directory,
        path: path.clone(),
        custody: SqliteRuntimeCustodyStore::open(&path).await.unwrap(),
        delivery: SqliteRuntimeDeliveryStore::open(&path).await.unwrap(),
        transition: SqliteHostAgentStore::open(&path).await.unwrap(),
    };
    fixture
        .custody
        .claim_or_current(ClaimRuntimeCustody::new(
            scope(),
            DaemonInstanceId::parse("daemon-a").unwrap(),
            RuntimeEventTime::from_unix_millis(7),
        ))
        .await
        .unwrap()
        .unwrap();
    let mut db = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    sqlx::query(
        "UPDATE agent_runtime_slots SET observed_status='active', cli_session_id='cli-session' \
         WHERE agent_id=? AND conversation_id=?",
    )
    .bind(AGENT)
    .bind(C1)
    .execute(&mut db)
    .await
    .unwrap();
    db.close().await.unwrap();
    fixture
}

async fn current_principal(
    fixture: &Fixture,
) -> silverhand_backend_core::host_runtime::runtime_credential::RuntimePrincipal {
    fixture.custody.current(scope()).await.unwrap().unwrap()
}

async fn lease_state(fixture: &Fixture) -> Vec<(Option<String>, Option<i64>)> {
    let mut db = SqliteConnection::connect(&format!("sqlite:{}", fixture.path.display()))
        .await
        .unwrap();
    let rows = sqlx::query(
        "SELECT in_flight_owner, in_flight_at FROM conversation_inboxes \
         WHERE agent_id=? ORDER BY conversation_id",
    )
    .bind(AGENT)
    .fetch_all(&mut db)
    .await
    .unwrap();
    rows.into_iter()
        .map(|row| (row.get("in_flight_owner"), row.get("in_flight_at")))
        .collect()
}

#[tokio::test]
async fn held_marker_fences_custody_and_claim_then_exact_reclaim_stays_stable() {
    let fixture = fixture().await;
    let principal = current_principal(&fixture).await;
    let BeginTransition::Started(lock) =
        begin_runtime_transition(&fixture.transition, &agent(), RuntimeKind::Claude, 100)
            .await
            .unwrap()
    else {
        panic!("transition lock");
    };

    // Both same-daemon reuse and successor generation minting are fenced before the
    // slot/event mutation line while any marker is held.
    for daemon in ["daemon-a", "daemon-b"] {
        let claim = fixture
            .custody
            .claim_or_current(ClaimRuntimeCustody::new(
                scope(),
                DaemonInstanceId::parse(daemon).unwrap(),
                RuntimeEventTime::from_unix_millis(101),
            ))
            .await
            .unwrap();
        assert!(claim.is_none());
    }
    assert_eq!(current_principal(&fixture).await, principal);

    let snapshot = read_runtime_transition_seats(&fixture.transition, &lock)
        .await
        .unwrap();
    let ReadRuntimeTransitionSeats::Snapshot(snapshot) = snapshot else {
        panic!("seat snapshot");
    };
    assert_eq!(snapshot.agent_id(), &agent());
    assert_eq!(snapshot.seats().len(), 2);
    let live = snapshot
        .seats()
        .iter()
        .find(|seat| seat.scope == scope())
        .unwrap();
    assert_eq!(live.observed_status, RuntimeObservedStatus::Active);
    assert_eq!(live.cli_session_id.as_deref(), Some("cli-session"));

    let reclaimed = reclaim_runtime_transition_inboxes(&fixture.transition, &lock)
        .await
        .unwrap();
    let ReclaimRuntimeTransitionInboxes::Reclaimed(reclaimed) = reclaimed else {
        panic!("reclaimed");
    };
    assert_eq!(
        reclaimed.conversations(),
        &[conversation(C1), conversation(C2)]
    );
    assert_eq!(
        lease_state(&fixture).await,
        vec![(None, None), (None, None)]
    );

    let claim = fixture
        .delivery
        .claim_pending(
            "host-1".to_owned(),
            principal.clone(),
            conversation(C1),
            DeliveryLeaseOwner::parse("new-owner").unwrap(),
            200,
            100,
            &NoReferences,
        )
        .await
        .unwrap();
    assert!(matches!(claim, ClaimPendingInbox::Transitioning));
    assert_eq!(
        lease_state(&fixture).await,
        vec![(None, None), (None, None)]
    );

    assert_eq!(
        abort_runtime_transition(&fixture.transition, &lock)
            .await
            .unwrap(),
        AbortTransition::Released
    );
    let claim = fixture
        .delivery
        .claim_pending(
            "host-1".to_owned(),
            principal,
            conversation(C1),
            DeliveryLeaseOwner::parse("new-owner").unwrap(),
            200,
            100,
            &NoReferences,
        )
        .await
        .unwrap();
    assert!(matches!(claim, ClaimPendingInbox::Claimed(_)));
}

#[tokio::test]
async fn stale_lock_reclaim_is_lost_and_leaves_every_lease_untouched() {
    let fixture = fixture().await;
    let BeginTransition::Started(stale) =
        begin_runtime_transition(&fixture.transition, &agent(), RuntimeKind::Claude, 100)
            .await
            .unwrap()
    else {
        panic!("transition lock");
    };
    assert_eq!(
        fixture.transition.abort(&stale).await.unwrap(),
        AbortTransition::Released
    );
    let BeginTransition::Started(_successor) =
        begin_runtime_transition(&fixture.transition, &agent(), RuntimeKind::Claude, 101)
            .await
            .unwrap()
    else {
        panic!("successor lock");
    };

    assert_eq!(
        reclaim_runtime_transition_inboxes(&fixture.transition, &stale)
            .await
            .unwrap(),
        ReclaimRuntimeTransitionInboxes::Lost
    );
    assert_eq!(
        lease_state(&fixture).await,
        vec![
            (Some("old-owner".to_owned()), Some(1)),
            (Some("old-owner".to_owned()), Some(1)),
        ]
    );
}
