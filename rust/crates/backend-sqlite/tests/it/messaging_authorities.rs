use std::{
    convert::Infallible,
    error::Error,
    ops::Range,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use silverhand_backend_core::conversation_messaging::messaging::task::{
    AgentBoardResolver, AgentReadableBoards, BoardKey, BoardResolution, BoardScopeRequest,
    BoardScopeSource, PairKey, ResolvedBoard, TaskParticipationReads, TaskReadCapability,
};
use silverhand_backend_core::{
    conversation_messaging::messaging::{
        AddressRefusal, ChannelAccess, ChannelAccessRefusal, DeliveryIntent, DirectPairAllocation,
        DirectPairOpenedFact, DirectPairOpenedPort, EnvironmentUnavailable, MessageContent,
        ReferencePositions, SendActor, SendCapability, SendClock, SendDestination, SendEntropy,
        SendMessage, SendMessageDraft, SendMessageUnitOfWork, TargetAddress,
        TargetAddressOperatorReason, TargetAddressRefusalPort, TargetAddressRefusalReport,
    },
    foundation::identity::{AgentId, ConversationId, UserId, WorkspaceId},
};
use silverhand_backend_sqlite::conversation_messaging::messaging::MessagingStore;
use sqlx::SqliteConnection;
use tempfile::TempDir;

use crate::current_schema;

const USER: &str = "user-1";
const OTHER_USER: &str = "user-2";
const AGENT: &str = "agent-1";
const PEER: &str = "agent-2";
const HIDDEN: &str = "agent-3";
const WORKSPACE: &str = "workspace-1";
const CHANNEL: &str = "conv000000000001";
const ROOT: &str = "root000000000001";

struct Fixture {
    _temp: TempDir,
    path: PathBuf,
    db: SqliteConnection,
}

impl Fixture {
    async fn new() -> Self {
        Self::with_conversation_agent(None).await
    }

    async fn with_conversation_agent(scoped: Option<&str>) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("authority.sqlite");
        let mut db = current_schema::create_current_schema(&path).await;
        seed(&mut db, scoped).await;
        Self {
            _temp: temp,
            path,
            db,
        }
    }

    async fn store(&self) -> MessagingStore {
        MessagingStore::open(&self.path).await.unwrap()
    }

    async fn store_with<E>(&self, environment: E) -> MessagingStore<E> {
        MessagingStore::open_with(&self.path, environment)
            .await
            .unwrap()
    }
}

#[derive(Clone, Default)]
struct CountingEnvironment {
    clock_calls: Arc<AtomicUsize>,
    entropy_calls: Arc<AtomicUsize>,
}

impl SendClock for CountingEnvironment {
    fn now_unix_ms(&self) -> Result<i64, EnvironmentUnavailable> {
        self.clock_calls.fetch_add(1, Ordering::SeqCst);
        Ok(1)
    }
}

impl SendEntropy for CountingEnvironment {
    fn fill(&self, buffer: &mut [u8]) -> Result<(), EnvironmentUnavailable> {
        self.entropy_calls.fetch_add(1, Ordering::SeqCst);
        buffer.fill(7);
        Ok(())
    }
}

#[derive(Clone, Default)]
struct Opened(Arc<Mutex<Vec<DirectPairOpenedFact>>>);

impl DirectPairOpenedPort for Opened {
    type Error = Infallible;

    async fn direct_pair_opened(&self, fact: &DirectPairOpenedFact) -> Result<(), Self::Error> {
        self.0.lock().unwrap().push(fact.clone());
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RefusalRecord {
    refusal: AddressRefusal,
    reason: TargetAddressOperatorReason,
    address: String,
    principal: String,
    intent: DeliveryIntent,
}

#[derive(Clone, Default)]
struct Refusals(Arc<Mutex<Vec<RefusalRecord>>>);

impl TargetAddressRefusalPort for Refusals {
    type Error = Infallible;

    async fn refused(&self, report: &TargetAddressRefusalReport) -> Result<(), Self::Error> {
        self.0.lock().unwrap().push(RefusalRecord {
            refusal: report.refusal(),
            reason: report.reason(),
            address: report.address().to_owned(),
            principal: report.principal().as_str().to_owned(),
            intent: report.intent(),
        });
        Ok(())
    }
}

fn user() -> UserId {
    UserId::parse(USER).unwrap()
}

fn agent() -> AgentId {
    AgentId::parse(AGENT).unwrap()
}

fn channel() -> ConversationId {
    ConversationId::parse(CHANNEL).unwrap()
}

#[tokio::test]
async fn target_refusal_observation_preserves_explicit_read_intent_and_forbidden_allocation() {
    let fixture = Fixture::new().await;
    let store = fixture.store().await;
    let opened = Opened::default();
    let refusals = Refusals::default();
    let target = store.target_address(opened.clone(), refusals.clone());

    assert_eq!(
        target
            .resolve(
                &agent(),
                "dm:@hidden",
                DirectPairAllocation::Forbidden,
                DeliveryIntent::Read,
            )
            .await
            .unwrap(),
        Err(AddressRefusal::Unresolved)
    );
    assert!(opened.0.lock().unwrap().is_empty());
    assert_eq!(
        refusals.0.lock().unwrap().as_slice(),
        &[RefusalRecord {
            refusal: AddressRefusal::Unresolved,
            reason: TargetAddressOperatorReason::DirectPairNotFound,
            address: "dm:@hidden".to_owned(),
            principal: AGENT.to_owned(),
            intent: DeliveryIntent::Read,
        }]
    );
}

async fn seed(db: &mut SqliteConnection, scoped: Option<&str>) {
    sqlx::query(
        "INSERT INTO hosts (id, name, kind, created_at) VALUES ('host-1', 'host', 'local', 0)",
    )
    .execute(&mut *db)
    .await
    .unwrap();
    for (id, name) in [(USER, "human"), (OTHER_USER, "other")] {
        sqlx::query("INSERT INTO users (id, name, display_name, created_at, updated_at) VALUES (?, ?, ?, 0, 0)")
            .bind(id).bind(name).bind(name).execute(&mut *db).await.unwrap();
    }
    for (id, name, creator, scope) in [
        (AGENT, "actor", USER, "agent"),
        (PEER, "peer", USER, "agent"),
        (HIDDEN, "hidden", OTHER_USER, "agent"),
    ] {
        sqlx::query("INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, display_name, created_by_user_id, materialization_status, session_scope) VALUES (?, ?, 'codex', 'host-1', 0, 0, ?, ?, 'active', ?)")
            .bind(id).bind(name).bind(name).bind(creator).bind(if scoped == Some(id) { "conversation" } else { scope }).execute(&mut *db).await.unwrap();
    }
    sqlx::query("INSERT INTO workspaces (id, name, name_key, owner_user_id, created_at, updated_at) VALUES (?, 'Product Space', 'product space', ?, 0, 0)")
        .bind(WORKSPACE).bind(USER).execute(&mut *db).await.unwrap();
    sqlx::query("INSERT INTO workspace_users (workspace_id, user_id, joined_at, role) VALUES (?, ?, 0, 'owner')")
        .bind(WORKSPACE).bind(USER).execute(&mut *db).await.unwrap();
    for id in [AGENT, PEER] {
        sqlx::query(
            "INSERT INTO workspace_members (workspace_id, agent_id, joined_at) VALUES (?, ?, 0)",
        )
        .bind(WORKSPACE)
        .bind(id)
        .execute(&mut *db)
        .await
        .unwrap();
    }
    sqlx::query("INSERT INTO conversations (id, workspace_id, placement_kind, roster_policy, title, title_key, status, visibility, created_at, updated_at, latest_message_seq) VALUES (?, ?, 'workspace', 'open', 'General Chat', 'general chat', 'open', 'private', 0, 0, 1)")
        .bind(CHANNEL).bind(WORKSPACE).execute(&mut *db).await.unwrap();
    sqlx::query(
        "INSERT INTO conversation_users (conversation_id, user_id, joined_at) VALUES (?, ?, 0)",
    )
    .bind(CHANNEL)
    .bind(USER)
    .execute(&mut *db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversation_agents (conversation_id, agent_id, joined_at) VALUES (?, ?, 0)",
    )
    .bind(CHANNEL)
    .bind(AGENT)
    .execute(&mut *db)
    .await
    .unwrap();
    sqlx::query("INSERT INTO workspace_agent_capabilities (workspace_id, agent_id, capability) VALUES (?, ?, 'send_messages')")
        .bind(WORKSPACE).bind(AGENT).execute(&mut *db).await.unwrap();
    sqlx::query("INSERT INTO messages (id, conversation_id, from_kind, from_user_id, content, seq, created_at) VALUES (?, ?, 'user', ?, 'root', 1, 0)")
        .bind(ROOT).bind(CHANNEL).bind(USER).execute(&mut *db).await.unwrap();
}

#[tokio::test]
async fn human_channel_access_distinguishes_refusal_from_driver_failure() {
    let mut fixture = Fixture::new().await;
    let store = fixture.store().await;

    assert_eq!(store.may_post(&channel(), &user()).await.unwrap(), Ok(()));
    sqlx::query("DELETE FROM conversation_users WHERE conversation_id = ? AND user_id = ?")
        .bind(CHANNEL)
        .bind(USER)
        .execute(&mut fixture.db)
        .await
        .unwrap();
    assert_eq!(
        store.may_post(&channel(), &user()).await.unwrap(),
        Err(ChannelAccessRefusal::Forbidden)
    );
    assert_eq!(
        store
            .may_post(&ConversationId::parse("missing").unwrap(), &user())
            .await
            .unwrap(),
        Err(ChannelAccessRefusal::NotFound)
    );

    sqlx::query("DROP TABLE conversation_users")
        .execute(&mut fixture.db)
        .await
        .unwrap();
    let error = store.may_post(&channel(), &user()).await.unwrap_err();
    assert!(error.to_string().contains("persistence operation failed"));
    assert!(error.source().is_some());
}

#[tokio::test]
async fn target_resolution_covers_all_grammar_arms_and_required_pair_announcement() {
    let mut fixture = Fixture::new().await;
    let store = fixture.store().await;
    let opened = Opened::default();
    let refusals = Refusals::default();
    let target = store.target_address(opened.clone(), refusals.clone());

    let by_id = target
        .resolve(
            &agent(),
            "#conv:conv000000000001/thread:root000000000001",
            DirectPairAllocation::Permitted,
            DeliveryIntent::Write,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(by_id.conversation.as_str(), CHANNEL);
    assert_eq!(by_id.thread_root.unwrap().as_str(), ROOT);

    let by_name = target
        .resolve(
            &agent(),
            "#Product%20Space/General%20Chat",
            DirectPairAllocation::Permitted,
            DeliveryIntent::Write,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(by_name.conversation.as_str(), CHANNEL);

    sqlx::query("UPDATE workspaces SET name = 'Straße', name_key = 'strasse' WHERE id = ?")
        .bind(WORKSPACE)
        .execute(&mut fixture.db)
        .await
        .unwrap();
    sqlx::query("UPDATE conversations SET title = 'Kelvin', title_key = 'kelvin' WHERE id = ?")
        .bind(CHANNEL)
        .execute(&mut fixture.db)
        .await
        .unwrap();
    let folded = target
        .resolve(
            &agent(),
            "#STRASSE/kelvin",
            DirectPairAllocation::Permitted,
            DeliveryIntent::Write,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(folded.conversation.as_str(), CHANNEL);

    let first = target
        .resolve(
            &agent(),
            "dm:@peer",
            DirectPairAllocation::Permitted,
            DeliveryIntent::Write,
        )
        .await
        .unwrap()
        .unwrap();
    let second = target
        .resolve(
            &agent(),
            "dm:@peer",
            DirectPairAllocation::Permitted,
            DeliveryIntent::Write,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.conversation, second.conversation);
    assert_eq!(opened.0.lock().unwrap().len(), 1);

    assert_eq!(
        target
            .resolve(
                &agent(),
                "dm:@hidden",
                DirectPairAllocation::Forbidden,
                DeliveryIntent::Write
            )
            .await
            .unwrap(),
        Err(AddressRefusal::Unresolved)
    );
    assert_eq!(
        target
            .resolve(
                &agent(),
                "not-an-address",
                DirectPairAllocation::Permitted,
                DeliveryIntent::Write
            )
            .await
            .unwrap(),
        Err(AddressRefusal::Malformed)
    );
    assert_eq!(
        *refusals.0.lock().unwrap(),
        [
            RefusalRecord {
                refusal: AddressRefusal::Unresolved,
                reason: TargetAddressOperatorReason::DirectPairNotFound,
                address: "dm:@hidden".to_owned(),
                principal: AGENT.to_owned(),
                intent: DeliveryIntent::Write,
            },
            RefusalRecord {
                refusal: AddressRefusal::Malformed,
                reason: TargetAddressOperatorReason::Grammar,
                address: "not-an-address".to_owned(),
                principal: AGENT.to_owned(),
                intent: DeliveryIntent::Write,
            },
        ]
    );
}

#[tokio::test]
async fn a_thread_suffix_cannot_create_a_missing_direct_pair() {
    let mut fixture = Fixture::new().await;
    let environment = CountingEnvironment::default();
    let store = fixture.store_with(environment.clone()).await;
    let opened = Opened::default();
    let refusals = Refusals::default();
    let target = store.target_address(opened.clone(), refusals.clone());
    let address = "dm:@peer/thread:root000000000099";

    assert_eq!(
        target
            .resolve(
                &agent(),
                address,
                DirectPairAllocation::Permitted,
                DeliveryIntent::Write
            )
            .await
            .unwrap(),
        Err(AddressRefusal::Unresolved)
    );
    assert_eq!(environment.clock_calls.load(Ordering::SeqCst), 0);
    assert_eq!(environment.entropy_calls.load(Ordering::SeqCst), 0);
    assert!(opened.0.lock().unwrap().is_empty());
    let pair_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM conversations WHERE placement_kind = 'direct_pair'",
    )
    .fetch_one(&mut fixture.db)
    .await
    .unwrap();
    assert_eq!(pair_count, 0);
    assert_eq!(
        *refusals.0.lock().unwrap(),
        [RefusalRecord {
            refusal: AddressRefusal::Unresolved,
            reason: TargetAddressOperatorReason::DirectPairNotFound,
            address: address.to_owned(),
            principal: AGENT.to_owned(),
            intent: DeliveryIntent::Write,
        }]
    );
}

#[tokio::test]
async fn an_existing_direct_pair_still_validates_its_thread_root() {
    let fixture = Fixture::new().await;
    let store = fixture.store().await;
    let opened = Opened::default();
    let refusals = Refusals::default();
    let target = store.target_address(opened.clone(), refusals.clone());

    target
        .resolve(
            &agent(),
            "dm:@peer",
            DirectPairAllocation::Permitted,
            DeliveryIntent::Write,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        target
            .resolve(
                &agent(),
                "dm:@peer/thread:root000000000099",
                DirectPairAllocation::Permitted,
                DeliveryIntent::Write
            )
            .await
            .unwrap(),
        Err(AddressRefusal::ThreadNotInTarget)
    );
    assert_eq!(opened.0.lock().unwrap().len(), 1);
    assert_eq!(
        refusals.0.lock().unwrap()[0].reason,
        TargetAddressOperatorReason::ThreadRootNotFound
    );
}

#[tokio::test]
async fn named_resolution_refuses_an_ambiguous_root_before_looking_at_titles() {
    let mut fixture = Fixture::new().await;
    sqlx::query("DROP INDEX idx_workspaces_name_key")
        .execute(&mut fixture.db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO workspaces (id, name, name_key, owner_user_id, created_at, updated_at) \
         VALUES ('workspace-2', 'Product Space', 'product space', ?, 0, 0)",
    )
    .bind(USER)
    .execute(&mut fixture.db)
    .await
    .unwrap();
    let store = fixture.store().await;
    let refusals = Refusals::default();
    let target = store.target_address(Opened::default(), refusals.clone());

    assert_eq!(
        target
            .resolve(
                &agent(),
                "#Product%20Space/General%20Chat",
                DirectPairAllocation::Permitted,
                DeliveryIntent::Write
            )
            .await
            .unwrap(),
        Err(AddressRefusal::Unresolved)
    );
    assert_eq!(
        refusals.0.lock().unwrap()[0].reason,
        TargetAddressOperatorReason::WorkspaceAmbiguous
    );
}

#[tokio::test]
async fn store_derived_conversation_ids_must_be_addressable() {
    let mut named = Fixture::new().await;
    sqlx::query(
        "INSERT INTO conversations (id, workspace_id, placement_kind, roster_policy, title, \
         title_key, status, visibility, created_at, updated_at) VALUES \
         ('bad/id', ?, 'workspace', 'open', 'Broken', 'broken', 'open', 'private', 0, 0)",
    )
    .bind(WORKSPACE)
    .execute(&mut named.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversation_agents (conversation_id, agent_id, joined_at) \
         VALUES ('bad/id', ?, 0)",
    )
    .bind(AGENT)
    .execute(&mut named.db)
    .await
    .unwrap();
    let target = named
        .store()
        .await
        .target_address(Opened::default(), Refusals::default());
    let error = target
        .resolve(
            &agent(),
            "#Product%20Space/Broken",
            DirectPairAllocation::Permitted,
            DeliveryIntent::Write,
        )
        .await
        .unwrap_err();
    assert!(format_source_chain(&error).contains("conversations.id"));

    let mut pair = Fixture::new().await;
    sqlx::query(
        "INSERT INTO conversations (id, placement_kind, roster_policy, endpoint_a_kind, \
         endpoint_a_id, endpoint_b_kind, endpoint_b_id, title, status, visibility, created_at, \
         updated_at) VALUES ('bad/id', 'direct_pair', 'fixed', 'agent', ?, 'agent', ?, '', \
         'idle', 'private', 0, 0)",
    )
    .bind(AGENT)
    .bind(PEER)
    .execute(&mut pair.db)
    .await
    .unwrap();
    let target = pair
        .store()
        .await
        .target_address(Opened::default(), Refusals::default());
    let error = target
        .resolve(
            &agent(),
            "dm:@peer",
            DirectPairAllocation::Permitted,
            DeliveryIntent::Write,
        )
        .await
        .unwrap_err();
    assert!(format_source_chain(&error).contains("conversations.id"));
}

fn format_source_chain(error: &(dyn Error + 'static)) -> String {
    let mut chain = vec![error.to_string()];
    let mut source = error.source();
    while let Some(next) = source {
        chain.push(next.to_string());
        source = next.source();
    }
    chain.join(": ")
}

#[tokio::test]
async fn send_capability_uses_placement_and_reports_corruption_as_our_failure() {
    let mut fixture = Fixture::new().await;
    let store = fixture.store().await;
    assert_eq!(
        store.may_send_messages(&agent(), &channel()).await.unwrap(),
        Ok(())
    );

    sqlx::query("DELETE FROM workspace_agent_capabilities WHERE workspace_id = ? AND agent_id = ?")
        .bind(WORKSPACE)
        .bind(AGENT)
        .execute(&mut fixture.db)
        .await
        .unwrap();
    assert!(
        store
            .may_send_messages(&agent(), &channel())
            .await
            .unwrap()
            .is_err()
    );

    sqlx::query("DROP TABLE workspace_agent_capabilities")
        .execute(&mut fixture.db)
        .await
        .unwrap();
    let error = store
        .may_send_messages(&agent(), &channel())
        .await
        .unwrap_err();
    assert!(error.source().is_some());
}

#[tokio::test]
async fn direct_line_authority_is_active_agent_scoped_and_uses_the_real_viewer_acl() {
    let mut fixture = Fixture::new().await;
    let store = fixture.store().await;
    struct Autolinks;
    impl ReferencePositions for Autolinks {
        fn autolink_spans(&self, content: &MessageContent) -> Vec<Range<usize>> {
            let body = content.as_str();
            let mut spans = Vec::new();
            let mut cursor = 0;
            while let Some(open) = body[cursor..].find('<') {
                let open = cursor + open;
                let close = open + body[open..].find('>').unwrap();
                spans.push(open..close + 1);
                cursor = close + 1;
            }
            spans
        }
    }
    let command = SendMessage::prepare(
        SendMessageDraft {
            source_conversation: channel(),
            target_conversation: channel(),
            actor: SendActor::User(user()),
            content: MessageContent::parse("<silverhand://agent/peer> <silverhand://agent/hidden>")
                .unwrap(),
            destination: SendDestination::MainLine,
            attachment_refs: Vec::new(),
            freshness_bypass: false,
        },
        &Autolinks,
        &store,
    )
    .await
    .unwrap();
    assert!(
        command
            .direct_lines()
            .contains(&AgentId::parse(PEER).unwrap())
    );
    assert!(
        !command
            .direct_lines()
            .contains(&AgentId::parse(HIDDEN).unwrap())
    );

    let drifted = SendMessage::prepare(
        SendMessageDraft {
            source_conversation: channel(),
            target_conversation: channel(),
            actor: SendActor::User(user()),
            content: MessageContent::parse("<silverhand://agent/peer>").unwrap(),
            destination: SendDestination::MainLine,
            attachment_refs: Vec::new(),
            freshness_bypass: false,
        },
        &Autolinks,
        &store,
    )
    .await
    .unwrap();
    sqlx::query("UPDATE agents SET name = 'renamed' WHERE id = ?")
        .bind(PEER)
        .execute(&mut fixture.db)
        .await
        .unwrap();
    sqlx::query("UPDATE agents SET name = 'peer' WHERE id = ?")
        .bind(HIDDEN)
        .execute(&mut fixture.db)
        .await
        .unwrap();
    store.send_message(drifted).await.unwrap();
    let copied: i64 =
        sqlx::query_scalar("SELECT count(*) FROM messages WHERE cc_source_message_id IS NOT NULL")
            .fetch_one(&mut fixture.db)
            .await
            .unwrap();
    assert_eq!(
        copied, 0,
        "an ACL grant must not follow a reassigned handle"
    );
}

#[tokio::test]
async fn persisted_placement_corruption_is_never_a_capability_denial() {
    for mutation in [
        "UPDATE conversations SET placement_kind = 'broken' WHERE id = 'conv000000000001'",
        "UPDATE conversations SET workspace_id = NULL WHERE id = 'conv000000000001'",
        "UPDATE conversations SET placement_kind = 'direct_pair', workspace_id = NULL, \
         endpoint_a_kind = 'agent', endpoint_a_id = 'agent-1', endpoint_b_kind = NULL, \
         endpoint_b_id = NULL, title_key = NULL, roster_policy = 'fixed' \
         WHERE id = 'conv000000000001'",
    ] {
        let mut fixture = Fixture::new().await;
        sqlx::query("PRAGMA ignore_check_constraints = ON")
            .execute(&mut fixture.db)
            .await
            .unwrap();
        sqlx::query("DROP TRIGGER conversations_placement_immutable")
            .execute(&mut fixture.db)
            .await
            .unwrap();
        sqlx::query("DROP TRIGGER trg_conversations_endpoints_immutable")
            .execute(&mut fixture.db)
            .await
            .unwrap();
        sqlx::query(mutation)
            .execute(&mut fixture.db)
            .await
            .unwrap();
        let store = fixture.store().await;
        let error = store
            .may_send_messages(&agent(), &channel())
            .await
            .unwrap_err();
        assert!(error.source().is_some());
    }
}

#[tokio::test]
async fn a_deleting_but_present_direct_pair_endpoint_remains_addressable_until_delete() {
    struct NoReferences;
    impl ReferencePositions for NoReferences {
        fn autolink_spans(&self, _content: &MessageContent) -> Vec<Range<usize>> {
            Vec::new()
        }
    }

    const PAIR: &str = "pair000000000001";
    let mut fixture = Fixture::new().await;
    sqlx::query(
        "INSERT INTO conversations (id, placement_kind, roster_policy, endpoint_a_kind, \
         endpoint_a_id, endpoint_b_kind, endpoint_b_id, title, status, visibility, created_at, \
         updated_at) VALUES (?, 'direct_pair', 'fixed', 'agent', ?, 'human', ?, '', 'idle', \
         'private', 0, 0)",
    )
    .bind(PAIR)
    .bind(PEER)
    .bind(USER)
    .execute(&mut fixture.db)
    .await
    .unwrap();
    sqlx::query("UPDATE agents SET deletion_started_at = 1 WHERE id = ?")
        .bind(PEER)
        .execute(&mut fixture.db)
        .await
        .unwrap();
    let store = fixture.store().await;
    let pair = ConversationId::parse(PAIR).unwrap();

    assert_eq!(store.may_post(&pair, &user()).await.unwrap(), Ok(()));
    let command = SendMessage::prepare(
        SendMessageDraft {
            source_conversation: pair.clone(),
            target_conversation: pair.clone(),
            actor: SendActor::User(user()),
            content: MessageContent::parse("delete is not complete").unwrap(),
            destination: SendDestination::MainLine,
            attachment_refs: Vec::new(),
            freshness_bypass: false,
        },
        &NoReferences,
        &store,
    )
    .await
    .unwrap();
    store.send_message(command).await.unwrap();
    let written: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM messages WHERE conversation_id = ? AND content = ?",
    )
    .bind(PAIR)
    .bind("delete is not complete")
    .fetch_one(&mut fixture.db)
    .await
    .unwrap();
    assert_eq!(written, 1);

    sqlx::query("DELETE FROM agents WHERE id = ?")
        .bind(PEER)
        .execute(&mut fixture.db)
        .await
        .unwrap();
    assert_eq!(
        store.may_post(&pair, &user()).await.unwrap(),
        Err(ChannelAccessRefusal::Forbidden)
    );
}

#[tokio::test]
async fn a_shaping_read_never_survives_revocation_into_the_write_unit() {
    struct NoReferences;
    impl ReferencePositions for NoReferences {
        fn autolink_spans(&self, _content: &MessageContent) -> Vec<Range<usize>> {
            Vec::new()
        }
    }

    let mut fixture = Fixture::new().await;
    let store = fixture.store().await;
    assert_eq!(store.may_post(&channel(), &user()).await.unwrap(), Ok(()));
    let command = SendMessage::prepare(
        SendMessageDraft {
            source_conversation: channel(),
            target_conversation: channel(),
            actor: SendActor::User(user()),
            content: MessageContent::parse("will be revoked").unwrap(),
            destination: SendDestination::MainLine,
            attachment_refs: Vec::new(),
            freshness_bypass: false,
        },
        &NoReferences,
        &store,
    )
    .await
    .unwrap();

    sqlx::query("DELETE FROM conversation_users WHERE conversation_id = ? AND user_id = ?")
        .bind(CHANNEL)
        .bind(USER)
        .execute(&mut fixture.db)
        .await
        .unwrap();
    assert!(store.send_message(command).await.is_err());
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE content = 'will be revoked'")
            .fetch_one(&mut fixture.db)
            .await
            .unwrap();
    assert_eq!(count, 0);
}

// --- Task-read authorities --------------------------------------------------
//
// The three consumer ports the task read composes (`may_read_tasks`,
// `is_{agent,user}_participant`, `resolve_board`) are answered from the same
// roster/workspace tables this fixture already seeds for the send gate. The
// fixture is deliberately reused: AGENT holds `send_messages` but NOT
// `read_tasks`, PEER is a workspace member absent from the channel roster, and
// workspace-1 is user-owned (not an agent-home) — exactly the shapes that pin
// token specificity, participation-vs-membership, and the owner arm.

fn an_agent(id: &str) -> AgentId {
    AgentId::parse(id).unwrap()
}

fn a_workspace(id: &str) -> WorkspaceId {
    WorkspaceId::parse(id).unwrap()
}

fn a_conversation(id: &str) -> ConversationId {
    ConversationId::parse(id).unwrap()
}

fn a_user(id: &str) -> UserId {
    UserId::parse(id).unwrap()
}

/// The capability is `read_tasks`, not merely "some capability". AGENT holds
/// `send_messages` in this workspace and nothing else, so a gate that asked the
/// wrong token — or any token — would wrongly admit it. Granting `read_tasks`
/// (AGENT is already a member, which the capability FK requires) flips it.
#[tokio::test]
async fn may_read_tasks_is_specific_to_the_read_tasks_token() {
    let mut fixture = Fixture::new().await;
    let mut store = fixture.store().await;

    assert!(
        !store
            .may_read_tasks(&an_agent(AGENT), &a_workspace(WORKSPACE))
            .await
            .unwrap(),
        "send_messages must not satisfy read_tasks"
    );

    sqlx::query(
        "INSERT INTO workspace_agent_capabilities (workspace_id, agent_id, capability) \
         VALUES (?, ?, 'read_tasks')",
    )
    .bind(WORKSPACE)
    .bind(AGENT)
    .execute(&mut fixture.db)
    .await
    .unwrap();

    assert!(
        store
            .may_read_tasks(&an_agent(AGENT), &a_workspace(WORKSPACE))
            .await
            .unwrap(),
        "read_tasks capability must satisfy read_tasks"
    );
}

/// An agent-home workspace admits its owner through the `owner_agent_id` arm with
/// no capability row at all; a non-owner is not admitted by that arm and — since
/// an agent-home workspace's membership is owner-only — has no members-table
/// fallback either. This is the capability's owner arm, distinct from membership.
#[tokio::test]
async fn may_read_tasks_owner_arm_admits_the_agent_home_owner_only() {
    let mut fixture = Fixture::new().await;
    sqlx::query(
        "INSERT INTO workspaces (id, name, owner_agent_id, created_at, updated_at) \
         VALUES ('ws-home', 'Home', ?, 0, 0)",
    )
    .bind(PEER)
    .execute(&mut fixture.db)
    .await
    .unwrap();
    let mut store = fixture.store().await;

    assert!(
        store
            .may_read_tasks(&an_agent(PEER), &a_workspace("ws-home"))
            .await
            .unwrap(),
        "the agent-home owner reads through the owner arm without a capability row"
    );
    assert!(
        !store
            .may_read_tasks(&an_agent(AGENT), &a_workspace("ws-home"))
            .await
            .unwrap(),
        "a non-owner is not admitted to an agent-home workspace"
    );
}

/// Participation is the placement roster, NOT workspace membership: PEER is a
/// member of workspace-1 but is not in the channel's `conversation_agents`, so it
/// does not participate; AGENT, which is in the roster, does. HIDDEN is neither.
#[tokio::test]
async fn agent_participation_is_the_roster_not_workspace_membership() {
    let fixture = Fixture::new().await;
    let mut store = fixture.store().await;
    let channel = a_conversation(CHANNEL);

    assert!(
        store
            .is_agent_participant(&channel, &an_agent(AGENT))
            .await
            .unwrap()
    );
    assert!(
        !store
            .is_agent_participant(&channel, &an_agent(PEER))
            .await
            .unwrap(),
        "a workspace member absent from the channel roster does not participate"
    );
    assert!(
        !store
            .is_agent_participant(&channel, &an_agent(HIDDEN))
            .await
            .unwrap()
    );
}

/// The set-shaped capability read folds a MIXED set in one call: it admits the
/// workspaces the agent may read, drops a member workspace with no `read_tasks`, and
/// drops an id that names no workspace — all in a single `json_each`-driven read. Proves
/// the fan-out is a real set fold, not the singular method behind a per-item loop.
#[tokio::test]
async fn readable_task_workspaces_folds_a_mixed_set_in_one_read() {
    let mut fixture = Fixture::new().await;
    // AGENT gains read_tasks on workspace-1; it is a member of ws-2 but holds no
    // capability there; "ws-missing" is not a workspace at all.
    sqlx::query("INSERT INTO workspace_agent_capabilities (workspace_id, agent_id, capability) VALUES (?, ?, 'read_tasks')")
        .bind(WORKSPACE).bind(AGENT).execute(&mut fixture.db).await.unwrap();
    sqlx::query("INSERT INTO workspaces (id, name, name_key, owner_user_id, created_at, updated_at) VALUES ('ws-2', 'Second', 'second', ?, 0, 0)")
        .bind(USER).execute(&mut fixture.db).await.unwrap();
    sqlx::query(
        "INSERT INTO workspace_members (workspace_id, agent_id, joined_at) VALUES ('ws-2', ?, 0)",
    )
    .bind(AGENT)
    .execute(&mut fixture.db)
    .await
    .unwrap();
    let mut store = fixture.store().await;

    let readable = store
        .readable_task_workspaces(
            &an_agent(AGENT),
            &[
                a_workspace(WORKSPACE),
                a_workspace("ws-2"),
                a_workspace("ws-missing"),
            ],
        )
        .await
        .unwrap();
    assert!(readable.contains(&a_workspace(WORKSPACE)));
    assert!(
        !readable.contains(&a_workspace("ws-2")),
        "a member workspace with no read_tasks is not admitted"
    );
    assert!(
        !readable.contains(&a_workspace("ws-missing")),
        "an id that names no workspace is not admitted"
    );
    assert_eq!(readable.len(), 1);
}

/// The set-shaped participation read folds a MIXED set in one call: the roster channel
/// is admitted, a workspace channel the agent is absent from is dropped, and an id that
/// names no conversation is dropped — a single `json_each`-driven read, not a per-row loop.
#[tokio::test]
async fn agent_participating_conversations_folds_a_mixed_set_in_one_read() {
    let mut fixture = Fixture::new().await;
    // A second workspace channel AGENT is NOT rostered into.
    sqlx::query("INSERT INTO conversations (id, workspace_id, placement_kind, roster_policy, title, title_key, status, visibility, created_at, updated_at, latest_message_seq) VALUES ('chan-2', ?, 'workspace', 'open', 'Other', 'other', 'open', 'private', 0, 0, 1)")
        .bind(WORKSPACE).execute(&mut fixture.db).await.unwrap();
    let mut store = fixture.store().await;

    let participating = store
        .agent_participating_conversations(
            &an_agent(AGENT),
            &[
                a_conversation(CHANNEL),
                a_conversation("chan-2"),
                a_conversation("conv-missing"),
            ],
        )
        .await
        .unwrap();
    assert!(participating.contains(&a_conversation(CHANNEL)));
    assert!(
        !participating.contains(&a_conversation("chan-2")),
        "a workspace channel the agent is absent from is not a participation"
    );
    assert!(
        !participating.contains(&a_conversation("conv-missing")),
        "an id that names no conversation is not a participation"
    );
    assert_eq!(participating.len(), 1);
}

/// The human arm requires BOTH facts — a conversation seat AND workspace
/// membership. USER has both. A seat granted to OTHER_USER without workspace
/// membership is not a participant: a seat that outlived (or preceded) membership
/// admits nobody.
#[tokio::test]
async fn user_participation_requires_both_the_seat_and_workspace_membership() {
    let mut fixture = Fixture::new().await;
    let channel = a_conversation(CHANNEL);

    // A bare seat, no workspace membership.
    sqlx::query(
        "INSERT INTO conversation_users (conversation_id, user_id, joined_at) VALUES (?, ?, 0)",
    )
    .bind(CHANNEL)
    .bind(OTHER_USER)
    .execute(&mut fixture.db)
    .await
    .unwrap();
    let mut store = fixture.store().await;

    assert!(
        store
            .is_user_participant(&channel, &a_user(USER))
            .await
            .unwrap()
    );
    assert!(
        !store
            .is_user_participant(&channel, &a_user(OTHER_USER))
            .await
            .unwrap(),
        "a seat without workspace membership is not a participant"
    );
}

/// The board a resolution carries, or `None` for either refusal — for tests that
/// only assert the resolved board and treat both refusals alike.
fn resolved_board(resolution: BoardResolution) -> Option<BoardKey> {
    match resolution {
        BoardResolution::Resolved(resolved) => Some(resolved.board),
        BoardResolution::NoSuchWorkspace | BoardResolution::Unauthorized => None,
    }
}

/// A `--workspace` selector resolves to that workspace's board only for a member,
/// and the resolved fact echoes the exact subject (agent + source) so the core
/// minter can bind it to the request. A non-member of an EXISTING workspace resolves
/// to `Unauthorized` (a 403); a selector naming NO SUCH workspace to `NoSuchWorkspace`
/// (a 404) — the two are distinguished at the resolver, not collapsed.
#[tokio::test]
async fn selected_workspace_resolves_for_a_member_and_echoes_the_subject() {
    let fixture = Fixture::new().await;
    let mut store = fixture.store().await;
    let source = BoardScopeSource::SelectedWorkspace(a_workspace(WORKSPACE));

    let resolved = store
        .resolve_board(&BoardScopeRequest {
            agent: an_agent(AGENT),
            source: source.clone(),
        })
        .await
        .unwrap();
    assert_eq!(
        resolved,
        BoardResolution::Resolved(ResolvedBoard {
            agent: an_agent(AGENT),
            source,
            board: BoardKey::Workspace(a_workspace(WORKSPACE)),
        })
    );

    let stranger = store
        .resolve_board(&BoardScopeRequest {
            agent: an_agent(HIDDEN),
            source: BoardScopeSource::SelectedWorkspace(a_workspace(WORKSPACE)),
        })
        .await
        .unwrap();
    assert_eq!(
        stranger,
        BoardResolution::Unauthorized,
        "a non-member of an existing workspace is 403, not 404"
    );

    let unknown = store
        .resolve_board(&BoardScopeRequest {
            agent: an_agent(AGENT),
            source: BoardScopeSource::SelectedWorkspace(a_workspace("ws-does-not-exist")),
        })
        .await
        .unwrap();
    assert_eq!(
        unknown,
        BoardResolution::NoSuchWorkspace,
        "a selector naming no such workspace is 404"
    );
}

/// An agent-home workspace's membership is owner-only, so the selector resolves
/// for its owner and denies a non-owner — the members table is never consulted.
#[tokio::test]
async fn selected_agent_home_workspace_resolves_for_its_owner_only() {
    let mut fixture = Fixture::new().await;
    sqlx::query(
        "INSERT INTO workspaces (id, name, owner_agent_id, created_at, updated_at) \
         VALUES ('ws-home', 'Home', ?, 0, 0)",
    )
    .bind(PEER)
    .execute(&mut fixture.db)
    .await
    .unwrap();
    let mut store = fixture.store().await;

    let owner = store
        .resolve_board(&BoardScopeRequest {
            agent: an_agent(PEER),
            source: BoardScopeSource::SelectedWorkspace(a_workspace("ws-home")),
        })
        .await
        .unwrap();
    assert_eq!(
        resolved_board(owner),
        Some(BoardKey::Workspace(a_workspace("ws-home")))
    );

    let non_owner = store
        .resolve_board(&BoardScopeRequest {
            agent: an_agent(AGENT),
            source: BoardScopeSource::SelectedWorkspace(a_workspace("ws-home")),
        })
        .await
        .unwrap();
    assert_eq!(
        non_owner,
        BoardResolution::Unauthorized,
        "the home workspace exists, so a non-owner is 403 not 404"
    );
}

/// A conversation frame resolves to the frame's OWN board, derived from its
/// placement: a workspace channel yields its workspace board.
#[tokio::test]
async fn frame_conversation_resolves_a_workspace_board_from_its_placement() {
    let fixture = Fixture::new().await;
    let mut store = fixture.store().await;

    let resolved = store
        .resolve_board(&BoardScopeRequest {
            agent: an_agent(AGENT),
            source: BoardScopeSource::FrameConversation(a_conversation(CHANNEL)),
        })
        .await
        .unwrap();
    assert_eq!(
        resolved_board(resolved),
        Some(BoardKey::Workspace(a_workspace(WORKSPACE)))
    );
}

/// A direct-pair frame yields its pair board, derived from the stored (generated)
/// `pair_key` — never synthesized. An ABSENT conversation derives no board and
/// honestly denies (`None`); a PRESENT-but-corrupt row is a safe-500, proved by
/// the two repair tests below.
#[tokio::test]
async fn frame_conversation_resolves_a_pair_board_and_denies_the_unknown() {
    let mut fixture = Fixture::new().await;
    const PAIR: &str = "pair000000000009";
    sqlx::query(
        "INSERT INTO conversations (id, placement_kind, roster_policy, endpoint_a_kind, \
         endpoint_a_id, endpoint_b_kind, endpoint_b_id, title, status, visibility, created_at, \
         updated_at) VALUES (?, 'direct_pair', 'fixed', 'agent', ?, 'human', ?, '', 'idle', \
         'private', 0, 0)",
    )
    .bind(PAIR)
    .bind(AGENT)
    .bind(USER)
    .execute(&mut fixture.db)
    .await
    .unwrap();
    let pair_key: String = sqlx::query_scalar("SELECT pair_key FROM conversations WHERE id = ?")
        .bind(PAIR)
        .fetch_one(&mut fixture.db)
        .await
        .unwrap();
    let mut store = fixture.store().await;

    let resolved = store
        .resolve_board(&BoardScopeRequest {
            agent: an_agent(AGENT),
            source: BoardScopeSource::FrameConversation(a_conversation(PAIR)),
        })
        .await
        .unwrap();
    assert_eq!(
        resolved_board(resolved),
        Some(BoardKey::Pair(PairKey::parse(pair_key).unwrap()))
    );

    let unknown = store
        .resolve_board(&BoardScopeRequest {
            agent: an_agent(AGENT),
            source: BoardScopeSource::FrameConversation(a_conversation("conv000000000404")),
        })
        .await
        .unwrap();
    assert_eq!(
        unknown,
        BoardResolution::Unauthorized,
        "an unknown frame conversation derives no board — a 403, never a workspace 404"
    );
}

/// A PRESENT workspace-placed frame whose own `workspace_id` coordinate is NULL is
/// schema corruption, not a scope denial: the placement shape obliges the
/// coordinate, so its absence is a source-bearing safe-500 naming the offending
/// column. Restoring only that column makes the SAME frame resolve cleanly through
/// the SAME open store, attributing the failure to the coordinate, not to a missing
/// row (which stays `None`).
#[tokio::test]
async fn frame_workspace_coordinate_corruption_is_a_safe_500_repaired_in_place() {
    let mut fixture = Fixture::new().await;
    // The placement is trigger-immutable and the coordinate is CHECK-guarded, so
    // drop the trigger and suspend the check to inject the below-the-write-path
    // corruption this gate exists to catch.
    sqlx::query("DROP TRIGGER conversations_placement_immutable")
        .execute(&mut fixture.db)
        .await
        .unwrap();
    sqlx::query("PRAGMA ignore_check_constraints = ON")
        .execute(&mut fixture.db)
        .await
        .unwrap();
    sqlx::query("UPDATE conversations SET workspace_id = NULL WHERE id = ?")
        .bind(CHANNEL)
        .execute(&mut fixture.db)
        .await
        .unwrap();
    let mut store = fixture.store().await;

    let error = store
        .resolve_board(&BoardScopeRequest {
            agent: an_agent(AGENT),
            source: BoardScopeSource::FrameConversation(a_conversation(CHANNEL)),
        })
        .await
        .unwrap_err();
    assert!(
        format_source_chain(&error).contains("conversations.workspace_id"),
        "a NULL workspace coordinate names its own column: {}",
        format_source_chain(&error)
    );

    sqlx::query("UPDATE conversations SET workspace_id = ? WHERE id = ?")
        .bind(WORKSPACE)
        .bind(CHANNEL)
        .execute(&mut fixture.db)
        .await
        .unwrap();
    let repaired = store
        .resolve_board(&BoardScopeRequest {
            agent: an_agent(AGENT),
            source: BoardScopeSource::FrameConversation(a_conversation(CHANNEL)),
        })
        .await
        .unwrap();
    assert_eq!(
        resolved_board(repaired),
        Some(BoardKey::Workspace(a_workspace(WORKSPACE)))
    );
}

/// A PRESENT frame whose `placement_kind` is outside the v76 closed set
/// (`workspace|direct_pair`) is schema corruption, not a scope denial: the unknown
/// arm is a source-bearing safe-500 naming the placement column. Restoring the
/// placement to a known shape makes the SAME frame resolve cleanly through the SAME
/// open store.
#[tokio::test]
async fn frame_unknown_placement_is_a_safe_500_repaired_in_place() {
    let mut fixture = Fixture::new().await;
    sqlx::query("DROP TRIGGER conversations_placement_immutable")
        .execute(&mut fixture.db)
        .await
        .unwrap();
    sqlx::query("PRAGMA ignore_check_constraints = ON")
        .execute(&mut fixture.db)
        .await
        .unwrap();
    sqlx::query("UPDATE conversations SET placement_kind = 'broken' WHERE id = ?")
        .bind(CHANNEL)
        .execute(&mut fixture.db)
        .await
        .unwrap();
    let mut store = fixture.store().await;

    let error = store
        .resolve_board(&BoardScopeRequest {
            agent: an_agent(AGENT),
            source: BoardScopeSource::FrameConversation(a_conversation(CHANNEL)),
        })
        .await
        .unwrap_err();
    assert!(
        format_source_chain(&error).contains("conversations.placement_kind"),
        "an unknown placement names the placement column: {}",
        format_source_chain(&error)
    );

    sqlx::query("UPDATE conversations SET placement_kind = 'workspace' WHERE id = ?")
        .bind(CHANNEL)
        .execute(&mut fixture.db)
        .await
        .unwrap();
    let repaired = store
        .resolve_board(&BoardScopeRequest {
            agent: an_agent(AGENT),
            source: BoardScopeSource::FrameConversation(a_conversation(CHANNEL)),
        })
        .await
        .unwrap();
    assert_eq!(
        resolved_board(repaired),
        Some(BoardKey::Workspace(a_workspace(WORKSPACE)))
    );
}

/// The union list arm's enumeration: an agent's readable boards are its member and
/// owner-home workspace boards UNIONED with the direct-pair boards it is an endpoint
/// of — never a pair it is not in. Each entry is bound to the enumerated agent, the
/// subject the core minter checks. `read_tasks` is NOT applied here (the minter folds
/// it downstream), so a member workspace enters on membership alone.
#[tokio::test]
async fn readable_boards_unions_member_workspaces_owner_home_and_endpoint_pairs() {
    let mut fixture = Fixture::new().await;
    // An agent-home workspace AGENT owns (the owner arm of the workspace union) and a
    // member workspace PEER owns that AGENT is NOT in (must be excluded).
    sqlx::query(
        "INSERT INTO workspaces (id, name, owner_agent_id, created_at, updated_at) \
         VALUES ('ws-home', 'Home', ?, 0, 0)",
    )
    .bind(AGENT)
    .execute(&mut fixture.db)
    .await
    .unwrap();
    // A direct pair AGENT is an endpoint of, and one it is NOT (PEER↔USER) — the second
    // proves the endpoint predicate, not "every pair".
    for (id, a_kind, a_id) in [("conv-pair", "agent", AGENT), ("conv-other", "agent", PEER)] {
        sqlx::query(
            "INSERT INTO conversations (id, placement_kind, roster_policy, title, status, \
             endpoint_a_kind, endpoint_a_id, endpoint_b_kind, endpoint_b_id, created_at, \
             updated_at) VALUES (?, 'direct_pair', 'fixed', '', 'open', ?, ?, 'human', ?, 0, 0)",
        )
        .bind(id)
        .bind(a_kind)
        .bind(a_id)
        .bind(USER)
        .execute(&mut fixture.db)
        .await
        .unwrap();
    }
    let my_pair: String =
        sqlx::query_scalar("SELECT pair_key FROM conversations WHERE id = 'conv-pair'")
            .fetch_one(&mut fixture.db)
            .await
            .unwrap();
    let other_pair: String =
        sqlx::query_scalar("SELECT pair_key FROM conversations WHERE id = 'conv-other'")
            .fetch_one(&mut fixture.db)
            .await
            .unwrap();
    let mut store = fixture.store().await;

    let readable = store.readable_boards(&an_agent(AGENT)).await.unwrap();
    assert!(
        readable.iter().all(|entry| entry.agent == an_agent(AGENT)),
        "every enumerated entry is bound to the requested agent"
    );
    let boards: Vec<BoardKey> = readable.into_iter().map(|entry| entry.board).collect();
    assert!(
        boards.contains(&BoardKey::Workspace(a_workspace(WORKSPACE))),
        "the member workspace board is readable"
    );
    assert!(
        boards.contains(&BoardKey::Workspace(a_workspace("ws-home"))),
        "the owned agent-home workspace board is readable via the owner arm"
    );
    assert!(
        boards.contains(&BoardKey::Pair(PairKey::parse(&my_pair).unwrap())),
        "the pair the agent is an endpoint of is readable"
    );
    assert!(
        !boards.contains(&BoardKey::Pair(PairKey::parse(&other_pair).unwrap())),
        "a pair the agent is NOT an endpoint of is excluded"
    );
    assert_eq!(
        boards.len(),
        3,
        "exactly the two workspaces and the one pair"
    );
}

#[tokio::test]
async fn conversation_scoped_endpoints_cannot_allocate_a_direct_pair() {
    for scoped in [AGENT, PEER] {
        let mut fixture = Fixture::with_conversation_agent(Some(scoped)).await;
        let store = fixture.store().await;
        let target = store.target_address(Opened::default(), Refusals::default());
        assert_eq!(
            target
                .resolve(
                    &agent(),
                    "dm:@peer",
                    DirectPairAllocation::Permitted,
                    DeliveryIntent::Write
                )
                .await
                .unwrap(),
            Err(AddressRefusal::Unresolved)
        );
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM conversations WHERE placement_kind = 'direct_pair'",
        )
        .fetch_one(&mut fixture.db)
        .await
        .unwrap();
        assert_eq!(count, 0);
    }
}

#[tokio::test]
async fn historical_pair_remains_resolvable_for_reads_but_loses_send_capability() {
    let mut fixture = Fixture::with_conversation_agent(Some(PEER)).await;
    let store = fixture.store().await;
    let target = store.target_address(Opened::default(), Refusals::default());
    sqlx::query("INSERT INTO conversations (id, placement_kind, roster_policy, endpoint_a_kind, endpoint_a_id, endpoint_b_kind, endpoint_b_id, title, status, created_at, updated_at) VALUES ('pair000000000001', 'direct_pair', 'fixed', 'agent', ?, 'agent', ?, '', 'open', 0, 0)")
        .bind(AGENT).bind(PEER).execute(&mut fixture.db).await.unwrap();
    let read = target
        .resolve(
            &agent(),
            "dm:@peer",
            DirectPairAllocation::Forbidden,
            DeliveryIntent::Read,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(read.conversation.as_str(), "pair000000000001");
    assert!(
        store
            .may_send_messages(&agent(), &read.conversation)
            .await
            .unwrap()
            .is_err()
    );
}

#[tokio::test]
async fn dm_scope_hint_requires_the_existing_peer_read_horizon_and_capability() {
    for scoped in [AGENT, PEER, HIDDEN] {
        let mut fixture = Fixture::with_conversation_agent(Some(scoped)).await;
        let store = fixture.store().await;
        let address = store.target_address(Opened::default(), Refusals::default());
        sqlx::query("INSERT INTO workspace_agent_capabilities(workspace_id,agent_id,capability) VALUES(?,?,'read_messages')")
            .bind(WORKSPACE).bind(AGENT).execute(&mut fixture.db).await.unwrap();
        let handle = if scoped == HIDDEN {
            "dm:@hidden"
        } else {
            "dm:@peer"
        };
        for allocation in [
            DirectPairAllocation::Permitted,
            DirectPairAllocation::Forbidden,
        ] {
            let refused = address
                .resolve(&agent(), handle, allocation, DeliveryIntent::Write)
                .await
                .unwrap();
            assert_eq!(
                refused,
                Err(if scoped == HIDDEN {
                    AddressRefusal::Unresolved
                } else {
                    AddressRefusal::DirectMessageScope
                })
            );
        }
        assert_eq!(
            address
                .resolve(
                    &agent(),
                    "dm:@unknown",
                    DirectPairAllocation::Permitted,
                    DeliveryIntent::Write
                )
                .await
                .unwrap(),
            Err(AddressRefusal::Unresolved)
        );
        assert_eq!(
            address
                .resolve(
                    &agent(),
                    handle,
                    DirectPairAllocation::Forbidden,
                    DeliveryIntent::Read
                )
                .await
                .unwrap(),
            Err(AddressRefusal::Unresolved)
        );
        // Membership alone cannot disclose a profile without read_messages.
        sqlx::query("DELETE FROM workspace_agent_capabilities WHERE agent_id=? AND capability='read_messages'").bind(AGENT).execute(&mut fixture.db).await.unwrap();
        assert_eq!(
            address
                .resolve(
                    &agent(),
                    handle,
                    DirectPairAllocation::Permitted,
                    DeliveryIntent::Write
                )
                .await
                .unwrap(),
            Err(AddressRefusal::Unresolved)
        );
        // Nor may a capability without the caller's workspace horizon disclose it.
        sqlx::query("INSERT INTO workspace_agent_capabilities(workspace_id,agent_id,capability) VALUES(?,?,'read_messages')").bind(WORKSPACE).bind(AGENT).execute(&mut fixture.db).await.unwrap();
        sqlx::query("DELETE FROM workspace_members WHERE workspace_id=? AND agent_id=?")
            .bind(WORKSPACE)
            .bind(AGENT)
            .execute(&mut fixture.db)
            .await
            .unwrap();
        assert_eq!(
            address
                .resolve(
                    &agent(),
                    handle,
                    DirectPairAllocation::Permitted,
                    DeliveryIntent::Write
                )
                .await
                .unwrap(),
            Err(AddressRefusal::Unresolved)
        );
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM conversations WHERE placement_kind='direct_pair'",
        )
        .fetch_one(&mut fixture.db)
        .await
        .unwrap();
        assert_eq!(count, 0);
    }
}
