use std::{
    convert::Infallible,
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll, Waker},
};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use silverhand_backend_core::conversation_messaging::messaging::collaboration::notification::{
    CommittedNotificationClear, NotificationAftermath, NotificationClearFailure,
    NotificationClearPort, NotificationClearSelector, clear_notifications,
};
use silverhand_backend_core::viewer_read_model::presence::LastSeenAt;
use silverhand_backend_core::{
    conversation_messaging::direct_pair_read::DirectPairReadModel,
    conversation_messaging::messaging::{
        AddressedPrincipals, AttachmentId, CapabilityDenied, CommittedMessageView, CreatedMessage,
        DirectLineAuthority, DirectPairOpenedFact, DirectPairOpenedPort, DirectPairParticipants,
        EnvironmentUnavailable, MessageContent, MessageId, NotificationId, OpenDirectPairError,
        ParticipantHandle, PrincipalIdentity, PrincipalKind, ReferencePositions, SendActor,
        SendCapability, SendDestination, SendEntropy, SendMessage, SendMessageDraft,
        SendMessageError, SendMessageOutcome, SendMessageUnitOfWork,
        collaboration::attachment::{
            AttachmentChangedFact, AttachmentChangedPublisher, AttachmentContentType,
            AttachmentContentUpdateCommand, AttachmentContentUpdatedFact, AttachmentCreator,
            AttachmentFilename, AttachmentRecord, AttachmentWriteAuthority,
        },
        collaboration::reaction::{
            ReactionAftermath, ReactionChange, ReactionChangedFact, ReactionCount, ReactionEmoji,
            Reactor,
        },
        collaboration::workspace_rename_messaging::WorkspaceRenamePublicationPort,
        mention_cc::{CommittedMentionCopy, MentionCopyOutcome},
        post_commit::{
            CommittedMentionRow, CommittedMessagePublication, CopyNotification, EffectFailure,
            EffectStep, IdleStart, MainDelivery, MentionIntent, MentionSkipRecord,
            MessagePublication, ReplyDelivery, RuntimeDelivery, RuntimeStart, SendEffects,
            SkippedMention, UnreadPublication, ViewerNotification, announce_send,
        },
        unread::{UnreadAudience, UnreadReadModel, ViewerUnread},
    },
    foundation::blob_store::{BlobStorageKey, StorageKind, StoredBlob},
    foundation::identity::{
        AgentId, ConversationId, DaemonInstanceId, HostId, UserId, WorkspaceId,
    },
    foundation::persistence::PersistenceFailure,
    foundation::scope_event::{AuthorizeEventScope, EventScope},
    host_runtime::agent_availability_read::AgentAvailabilityReadModel,
    host_runtime::runtime_credential::{RuntimeCustody, RuntimeGeneration, RuntimePrincipal},
    host_runtime::{aftermath::AgentAvailabilityAnnouncer, agent_roster::ComputerAgentsReadPort},
    identity_access::access::{WorkspaceHumanAudienceReadPort, WorkspaceRole},
    identity_access::auth::{AuthenticateHumanSession, SessionBearer},
    identity_access::user_identity::UserChangedSink,
    identity_access::workspace_metadata_write::WorkspaceMetadataChangedSink,
    viewer_read_model::presence_grace::{GraceWindow, PresenceFact},
};
use silverhand_backend_sqlite::{
    conversation_messaging::direct_pair_read::SqliteDirectPairReadModel,
    conversation_messaging::messaging::MessagingStore,
    conversation_messaging::runtime_delivery::SqliteRuntimeDeliveryStore,
    viewer_read_model::unread_read::{SqliteUnreadReadModel, UnreadReadModelError},
    viewer_read_model::user_presence_cold_read::SqliteUserPresenceColdRead,
    viewer_read_model::user_presence_last_seen::SqliteLastSeenSink,
};
use sqlx::{Connection, SqliteConnection};
use tokio::sync::Barrier;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message as ClientMessage, client::IntoClientRequest},
};
use tower::ServiceExt;

use crate::conversation_messaging::scope_event_publishers::latest_unread_wire;
use crate::conversation_messaging::scope_event_publishers::*;
use crate::current_schema;
use crate::foundation::scope_event_wire::*;
use crate::host_runtime::host_agent_background::presence_grace_timer::{
    PresenceFactSink, PresenceGraceTimers,
};
use crate::host_runtime::runtime_event_settlement::{
    ParsedEventWire, RuntimeCoordinate, RuntimeEventDirective, RuntimeEventSettlement,
    RuntimeParsedEvent, RuntimeProductEvent, RuntimeScopeWire,
};
use crate::host_runtime::scope_event_publishers::*;
use crate::identity_access::scope_event_publishers::*;
use crate::viewer_read_model::presence::{
    DisconnectClock, HubPresencePublisher, SystemClockFault, compose_presence_vertical,
    duration_to_unix_ms,
};

use super::*;

fn user() -> UserId {
    UserId::parse("user-1").unwrap()
}

fn agent() -> AgentId {
    AgentId::parse("agent-1").unwrap()
}

fn pair_participants() -> DirectPairParticipants {
    DirectPairParticipants::between(
        PrincipalIdentity::Human(user()),
        PrincipalIdentity::Agent(agent()),
    )
    .unwrap()
}

struct FixedEntropy(u8);

impl SendEntropy for FixedEntropy {
    fn fill(&self, buffer: &mut [u8]) -> Result<(), EnvironmentUnavailable> {
        buffer.fill(self.0);
        Ok(())
    }
}

struct OneReference;

fn ready<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    match future.as_mut().poll(&mut context) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("the driver-free authority unexpectedly suspended"),
    }
}

impl ReferencePositions for OneReference {
    fn autolink_spans(&self, content: &MessageContent) -> Vec<std::ops::Range<usize>> {
        let start = content.as_str().find('<').unwrap();
        let end = content.as_str()[start..].find('>').unwrap() + start + 1;
        std::iter::once(start..end).collect()
    }
}

struct GrantsAgent;

impl DirectLineAuthority for GrantsAgent {
    async fn direct_lines(
        &self,
        _author: &UserId,
        addressed: &AddressedPrincipals,
    ) -> Result<Vec<AgentId>, PersistenceFailure> {
        assert!(addressed.iter().any(|reference| {
            reference.kind() == PrincipalKind::Agent && reference.handle().as_str() == "agent"
        }));
        Ok(vec![agent()])
    }
}

fn mention_command() -> SendMessage {
    ready(SendMessage::prepare(
        SendMessageDraft {
            source_conversation: ConversationId::parse("conversation-1").unwrap(),
            target_conversation: ConversationId::parse("conversation-1").unwrap(),
            actor: SendActor::User(user()),
            content: MessageContent::parse("<silverhand://agent/agent> ping").unwrap(),
            destination: SendDestination::MainLine,
            attachment_refs: Vec::new(),
            freshness_bypass: false,
        },
        &OneReference,
        &GrantsAgent,
    ))
    .unwrap()
}

#[derive(Default)]
struct OtherEffects(Mutex<Vec<EffectStep>>);

impl OtherEffects {
    fn record(&self, step: EffectStep) {
        self.0.lock().unwrap().push(step);
    }

    fn calls(&self) -> Vec<EffectStep> {
        self.0.lock().unwrap().clone()
    }
}

impl MessagePublication for OtherEffects {
    async fn publish_copy(&self, _copy: &CommittedMentionCopy) -> Result<(), EffectFailure> {
        self.record(EffectStep::CopyPublished);
        Ok(())
    }
}

impl CopyNotification for OtherEffects {
    async fn notify_author(
        &self,
        _author: &UserId,
        _agent: &AgentId,
        _copy: &CommittedMentionCopy,
    ) -> Result<(), EffectFailure> {
        self.record(EffectStep::AuthorNotified);
        Ok(())
    }
}

impl RuntimeStart for OtherEffects {
    async fn start(
        &self,
        _conversation: &ConversationId,
        _agent: &AgentId,
    ) -> Result<(), EffectFailure> {
        self.record(EffectStep::RuntimeStarted);
        Ok(())
    }
}

impl RuntimeDelivery for OtherEffects {
    async fn deliver(
        &self,
        _agent: &AgentId,
        _copy: &CommittedMentionCopy,
    ) -> Result<(), EffectFailure> {
        self.record(EffectStep::CopyDelivered);
        Ok(())
    }
}

impl MainDelivery for OtherEffects {
    async fn deliver_to(
        &self,
        _recipient: &AgentId,
        _message: &CreatedMessage,
    ) -> Result<(), EffectFailure> {
        self.record(EffectStep::MainDelivered);
        Ok(())
    }
}

impl ReplyDelivery for OtherEffects {
    async fn deliver_to(
        &self,
        _recipient: &AgentId,
        _message: &CreatedMessage,
    ) -> Result<(), EffectFailure> {
        self.record(EffectStep::ReplyDelivered);
        Ok(())
    }
}

impl IdleStart for OtherEffects {
    async fn start_if_idle(
        &self,
        _recipient: &AgentId,
        _conversation: &ConversationId,
    ) -> Result<(), EffectFailure> {
        self.record(EffectStep::IdleStarted);
        Ok(())
    }
}

impl MentionSkipRecord for OtherEffects {
    async fn record_skip(&self, _skipped: &SkippedMention<'_>) -> Result<(), EffectFailure> {
        self.record(EffectStep::SkipRecorded);
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
#[error("announcement unavailable")]
struct AnnouncementUnavailable;

struct FailingOpened;

impl DirectPairOpenedPort for FailingOpened {
    type Error = AnnouncementUnavailable;

    async fn direct_pair_opened(&self, _fact: &DirectPairOpenedFact) -> Result<(), Self::Error> {
        Err(AnnouncementUnavailable)
    }
}

#[derive(Debug, thiserror::Error)]
#[error("capability denied")]
struct LaterCapabilityDenied;

fn reject_later_product_action(_pair: &ConversationId) -> Result<(), LaterCapabilityDenied> {
    Err(LaterCapabilityDenied)
}

async fn direct_pair_database() -> (tempfile::TempDir, std::path::PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("direct-pair.sqlite");
    let mut db = current_schema::create_current_schema(&path).await;
    sqlx::query(
        "INSERT INTO users (id, name, display_name, created_at, updated_at) \
             VALUES ('user-1', 'user', 'User', 1, 1)",
    )
    .execute(&mut db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO hosts (id, name, kind, created_at) \
             VALUES ('host-1', 'host', 'local', 1)",
    )
    .execute(&mut db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, \
             display_name, created_by_user_id, session_scope) VALUES \
             ('agent-1', 'agent', 'claude', 'host-1', 1, 1, 'Agent', 'user-1', 'agent')",
    )
    .execute(&mut db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO workspaces (id, name, name_key, owner_user_id, created_at, updated_at) \
             VALUES ('workspace-1', 'Space', 'space', 'user-1', 1, 1)",
    )
    .execute(&mut db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO workspace_users (workspace_id, user_id, joined_at, role) \
             VALUES ('workspace-1', 'user-1', 1, 'owner')",
    )
    .execute(&mut db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversations (id, workspace_id, placement_kind, roster_policy, title, \
             title_key, visibility, created_at, updated_at) VALUES \
             ('conversation-1', 'workspace-1', 'workspace', 'open', 'Channel', 'channel', \
             'private', 1, 1)",
    )
    .execute(&mut db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversation_users (conversation_id, user_id, joined_at) \
             VALUES ('conversation-1', 'user-1', 1)",
    )
    .execute(&mut db)
    .await
    .unwrap();
    db.close().await.unwrap();
    (directory, path)
}

async fn runtime_event_database() -> (tempfile::TempDir, std::path::PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("runtime-events.sqlite");
    let mut db = current_schema::create_current_schema(&path).await;
    sqlx::raw_sql(
        "INSERT INTO users (id, name, display_name, created_at, updated_at)
             VALUES ('user-1', 'user', 'User', 1, 1);
             INSERT INTO hosts (id, name, kind, created_at)
             VALUES ('host-1', 'host', 'local', 1);
             INSERT INTO workspaces (id, name, name_key, owner_user_id, created_at, updated_at)
             VALUES ('workspace-1', 'Space', 'space', 'user-1', 1, 1);
             INSERT INTO workspace_users (workspace_id, user_id, joined_at, role)
             VALUES ('workspace-1', 'user-1', 1, 'owner');
             INSERT INTO agents (
               id, name, runtime, host_id, created_at, updated_at, display_name,
               created_by_user_id, session_scope
             ) VALUES (
               'agent-1', 'agent', 'claude', 'host-1', 1, 1, 'Agent', 'user-1', 'agent'
             );
             INSERT INTO workspace_members (workspace_id, agent_id, joined_at)
             VALUES ('workspace-1', 'agent-1', 1);
             INSERT INTO conversations (
               id, workspace_id, placement_kind, roster_policy, title, title_key, visibility, created_at, updated_at
             ) VALUES
               ('conversation-1', 'workspace-1', 'workspace', 'fixed', 'First', 'first', 'private', 1, 1),
               ('conversation-2', 'workspace-1', 'workspace', 'fixed', 'Second', 'second', 'private', 1, 1);
             INSERT INTO conversation_users (conversation_id, user_id, joined_at)
             VALUES ('conversation-1', 'user-1', 1), ('conversation-2', 'user-1', 1);
             INSERT INTO conversation_agents (conversation_id, agent_id, joined_at)
             VALUES ('conversation-1', 'agent-1', 1), ('conversation-2', 'agent-1', 1);
             UPDATE agent_runtime_slots
                SET generation = 1, claimed_by_daemon_instance_id = 'daemon-1'
              WHERE agent_id = 'agent-1' AND conversation_id IS NULL;",
    )
    .execute(&mut db)
    .await
    .unwrap();
    db.close().await.unwrap();
    (directory, path)
}

#[tokio::test]
async fn session_init_attempts_every_live_audience_and_still_requests_one_drain() {
    let (_directory, path) = runtime_event_database().await;
    let durable = SqliteRuntimeDeliveryStore::open(&path).await.unwrap();
    let hub = ScopeEventHub::test("epoch-1");
    let first = ConversationId::parse("conversation-1").unwrap();
    let second = ConversationId::parse("conversation-2").unwrap();
    let first_scope = EventScope::ConversationSide(first.clone());
    hub.scopes
        .lock()
        .unwrap()
        .entry(first_scope.clone())
        .or_insert_with(ScopeState::new)
        .seq = TransportSeq(MAX_TRANSPORT_SEQ);
    let owner = RuntimeEventSettlement::new(
        durable,
        Arc::new(hub.runtime_activity_publisher()),
        Arc::new(hub.agent_presence_publisher()),
        Arc::new(hub.runtime_status_publisher()),
    );
    let daemon = DaemonInstanceId::parse("daemon-1").unwrap();
    let directive = owner
        .handle(
            "host-1",
            &daemon,
            "connection-test",
            RuntimeProductEvent::Parsed(RuntimeParsedEvent {
                coordinate: RuntimeCoordinate {
                    runtime_scope: RuntimeScopeWire::Agent {
                        agent_id: "agent-1".to_owned(),
                    },
                    conversation_id: first.as_str().to_owned(),
                    agent_id: "agent-1".to_owned(),
                },
                runtime_generation: 1,
                parsed: ParsedEventWire::SessionInit {
                    session_id: "session-1".to_owned(),
                },
                ts: 10,
            }),
        )
        .await
        .unwrap();

    let RuntimeEventDirective::Drain {
        conversation,
        agent: drained_agent,
        post_commit_failure,
    } = directive
    else {
        panic!("a committed session init must request one drain")
    };
    assert_eq!(conversation, first);
    assert_eq!(drained_agent, agent());
    let post_commit_failure =
        post_commit_failure.expect("the first live projection failure must be retained");
    assert!(matches!(
        Error::source(&post_commit_failure)
            .and_then(|source| source.downcast_ref::<ScopeEventHubError>()),
        Some(ScopeEventHubError::SequenceExhausted)
    ));
    assert_eq!(
        hub.scopes.lock().unwrap()[&first_scope].seq,
        TransportSeq(MAX_TRANSPORT_SEQ)
    );
    let second_events = hub
        .resume(
            &EventScope::ConversationSide(second),
            &TransportEpoch::parse("epoch-1").unwrap(),
            TransportSeq::ZERO,
        )
        .unwrap();
    let Replay::Events(second_events) = second_events.replay else {
        panic!("the second status audience must receive a live projection")
    };
    assert_eq!(second_events.len(), 1);
    assert!(matches!(
        second_events[0].event,
        ProductEvent::AgentStatus(_)
    ));
    let viewer_events = hub
        .resume(
            &EventScope::Viewer(user()),
            &TransportEpoch::parse("epoch-1").unwrap(),
            TransportSeq::ZERO,
        )
        .unwrap();
    let Replay::Events(viewer_events) = viewer_events.replay else {
        panic!("the independent presence audience must still be attempted")
    };
    assert_eq!(viewer_events.len(), 1);
    assert!(matches!(
        viewer_events[0].event,
        ProductEvent::AgentPresenceChanged { .. }
    ));

    let mut db = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
        .await
        .unwrap();
    let status: String = sqlx::query_scalar(
        "SELECT observed_status FROM agent_runtime_slots
              WHERE agent_id = 'agent-1' AND conversation_id IS NULL",
    )
    .fetch_one(&mut db)
    .await
    .unwrap();
    let activities: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM conversation_events
              WHERE conversation_id = 'conversation-1' AND kind = 'session_init'",
    )
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(status, "active");
    assert_eq!(activities, 1);
}

async fn direct_pair_count(path: &std::path::Path) -> i64 {
    let mut db = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
        .await
        .unwrap();
    sqlx::query_scalar("SELECT COUNT(*) FROM conversations WHERE placement_kind = 'direct_pair'")
        .fetch_one(&mut db)
        .await
        .unwrap()
}

struct DenySend(Arc<AtomicUsize>);

impl SendCapability for DenySend {
    type Error = Infallible;

    async fn may_send_messages(
        &self,
        _agent: &AgentId,
        _conversation: &ConversationId,
    ) -> Result<Result<(), CapabilityDenied>, Self::Error> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(Err(CapabilityDenied))
    }
}

struct NoAftermath;

impl crate::conversation_messaging::messaging::aftermath::SendAftermath for NoAftermath {
    async fn announce(&self, _outcome: &SendMessageOutcome) {}
}

struct CountedStore {
    inner: MessagingStore,
    calls: Arc<AtomicUsize>,
}

impl SendMessageUnitOfWork for CountedStore {
    async fn send_message(
        &self,
        command: SendMessage,
    ) -> Result<SendMessageOutcome, SendMessageError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.send_message(command).await
    }
}

fn direct_pair_router(
    store: MessagingStore,
    hub: ScopeEventHub,
    calls: Arc<AtomicUsize>,
    capability_calls: Arc<AtomicUsize>,
) -> axum::Router {
    crate::conversation_messaging::messaging::router::send_router(
        crate::conversation_messaging::messaging::router::SendState {
            channel_access: store.clone(),
            target_address: store.target_address(
                hub.messaging_events(),
                crate::conversation_messaging::messaging::TargetAddressRefusalLog,
            ),
            capability: DenySend(capability_calls),
            authority: store.clone(),
            unit_of_work: CountedStore {
                inner: store,
                calls,
            },
            aftermath: NoAftermath,
        },
    )
    .layer(axum::Extension(RuntimePrincipal::AgentSlot {
        agent: agent(),
        custody: RuntimeCustody::new(
            RuntimeGeneration::new(1).unwrap(),
            silverhand_backend_core::foundation::identity::DaemonInstanceId::parse("daemon-1")
                .unwrap(),
        ),
    }))
}

fn agent_target_request(target: &str) -> axum::http::Request<axum::body::Body> {
    axum::http::Request::post("/internal/agents/agent-1/send-message")
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(
            serde_json::json!({"content":"hello", "target":target}).to_string(),
        ))
        .unwrap()
}

fn viewer_replay_len(hub: &ScopeEventHub) -> usize {
    let resumed = hub
        .resume(
            &EventScope::Viewer(user()),
            &TransportEpoch::parse("epoch-1").unwrap(),
            TransportSeq::ZERO,
        )
        .unwrap();
    let Replay::Events(events) = resumed.replay else {
        panic!("the viewer replay must fit in the retained window")
    };
    events.len()
}

#[tokio::test]
async fn real_resolver_announces_before_capability_refusal_and_never_enters_the_uow() {
    let (_directory, path) = direct_pair_database().await;
    let store = MessagingStore::open(&path).await.unwrap();
    let hub = ScopeEventHub::test("epoch-1");
    let calls = Arc::new(AtomicUsize::new(0));
    let capability_calls = Arc::new(AtomicUsize::new(0));
    let app = direct_pair_router(store, hub.clone(), calls.clone(), capability_calls.clone());

    let response = app.oneshot(agent_target_request("dm:@user")).await.unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::FORBIDDEN);
    assert_eq!(capability_calls.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(direct_pair_count(&path).await, 1);

    assert_eq!(viewer_replay_len(&hub), 1);
}

#[tokio::test]
async fn a_thread_suffix_never_creates_a_missing_pair_but_checks_an_existing_pair() {
    let (_directory, path) = direct_pair_database().await;
    let store = MessagingStore::open(&path).await.unwrap();
    let hub = ScopeEventHub::test("epoch-1");
    let calls = Arc::new(AtomicUsize::new(0));
    let capability_calls = Arc::new(AtomicUsize::new(0));
    let app = direct_pair_router(store, hub.clone(), calls.clone(), capability_calls.clone());
    let missing_root = "root000000000099";

    let missing_pair = app
        .clone()
        .oneshot(agent_target_request(&format!(
            "dm:@user/thread:{missing_root}"
        )))
        .await
        .unwrap();
    assert_eq!(missing_pair.status(), axum::http::StatusCode::NOT_FOUND);
    let body = axum::body::to_bytes(missing_pair.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({
            "error": "target names no stream you can use",
            "code": "target_unresolved",
        })
    );
    assert_eq!(direct_pair_count(&path).await, 0);
    assert_eq!(viewer_replay_len(&hub), 0);
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    let created = app
        .clone()
        .oneshot(agent_target_request("dm:@user"))
        .await
        .unwrap();
    assert_eq!(created.status(), axum::http::StatusCode::FORBIDDEN);
    assert_eq!(capability_calls.load(Ordering::SeqCst), 1);
    assert_eq!(direct_pair_count(&path).await, 1);
    assert_eq!(viewer_replay_len(&hub), 1);

    let existing_pair = app
        .oneshot(agent_target_request(&format!(
            "dm:@user/thread:{missing_root}"
        )))
        .await
        .unwrap();
    assert_eq!(existing_pair.status(), axum::http::StatusCode::NOT_FOUND);
    let body = axum::body::to_bytes(existing_pair.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({
            "error": "target names no reply cluster of that conversation",
            "code": "thread_not_in_target",
        })
    );
    assert_eq!(direct_pair_count(&path).await, 1);
    assert_eq!(viewer_replay_len(&hub), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

fn conversation() -> ConversationId {
    ConversationId::parse("pair-1").unwrap()
}

#[test]
fn transport_coordinates_reject_invalid_values() {
    assert!(TransportEpoch::parse("").is_err());
    assert!(AllowedOrigin::parse("https://example.test/path").is_err());
    assert!(AllowedOrigin::parse("ws://example.test").is_err());
    assert!(EventWebSocketConfig::production([]).is_err());
    assert!(serde_json::from_str::<TransportSeq>("9007199254740992").is_err());
    assert_eq!(
        serde_json::from_str::<TransportSeq>("9007199254740991")
            .unwrap()
            .0,
        MAX_TRANSPORT_SEQ
    );
}

#[test]
fn direct_pair_projection_is_replayable_and_sequence_is_exact_for_javascript() {
    let hub = ScopeEventHub::test("epoch-1");
    hub.publish_direct_pairs_changed(&user(), &conversation())
        .unwrap();
    hub.publish_direct_pairs_changed(&user(), &conversation())
        .unwrap();
    let resumed = hub
        .resume(
            &EventScope::Viewer(user()),
            &TransportEpoch::parse("epoch-1").unwrap(),
            TransportSeq::ZERO,
        )
        .unwrap();
    let Replay::Events(events) = resumed.replay else {
        panic!("same epoch must replay")
    };
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].seq, TransportSeq(1));
    assert_eq!(
        serde_json::to_value(&events[0]).unwrap(),
        serde_json::json!({
            "type": "event", "scope": "viewer:user-1", "seq": 1,
            "epoch": "epoch-1", "kind": "direct_pairs_changed",
            "payload": { "conversation_id": "pair-1" }
        })
    );

    let mut scopes = hub.scopes.lock().unwrap();
    scopes.get_mut(&EventScope::Viewer(user())).unwrap().seq = TransportSeq(MAX_TRANSPORT_SEQ);
    drop(scopes);
    assert!(matches!(
        hub.publish_direct_pairs_changed(&user(), &conversation()),
        Err(ScopeEventHubError::SequenceExhausted)
    ));
}

#[test]
fn user_presence_wire_parity_matches_the_typescript_signed_rows() {
    // TS-first: these fixtures are signed by the REAL `PresenceRegistry`
    // (packages/backend/tests/presence-event-parity.test.ts) before this
    // replay is asked to match them, so the two impls cannot drift on scope,
    // discriminant, the online arm's suppressed `null`, or the offline stamp.
    const ONLINE: &str =
        include_str!("../../../../../rust/test-fixtures/fixtures/presence-event/online.json");
    const OFFLINE: &str =
        include_str!("../../../../../rust/test-fixtures/fixtures/presence-event/offline.json");

    let hub = ScopeEventHub::test("epoch-1");
    // Drive the fixture replay through the PRODUCTION sink, not the hub helper:
    // a no-op `PresenceFactSink` that swallowed the fact would then fail this
    // gate, which is the fake-wiring this phase exists to forbid.
    let sink = HubPresencePublisher::new(hub.clone());
    let viewer = UserId::parse("viewer-1").unwrap();
    PresenceFactSink::publish(
        &sink,
        PresenceFact::Online {
            user_id: viewer.clone(),
        },
    )
    .unwrap();
    PresenceFactSink::publish(
        &sink,
        PresenceFact::Offline {
            user_id: viewer.clone(),
            last_seen_at: LastSeenAt::from_stored(1_234_000).unwrap(),
        },
    )
    .unwrap();

    // Both transitions ride the global `app` scope, in emit order.
    let resumed = hub
        .resume(
            &EventScope::App,
            &TransportEpoch::parse("epoch-1").unwrap(),
            TransportSeq::ZERO,
        )
        .unwrap();
    let Replay::Events(events) = resumed.replay else {
        panic!("same epoch must replay the app-scope presence events")
    };
    assert_eq!(events.len(), 2);

    for (index, (event, fixture_json)) in events.iter().zip([ONLINE, OFFLINE]).enumerate() {
        let fixture: serde_json::Value = serde_json::from_str(fixture_json).unwrap();
        // The EXACT app-scope frame: the transport envelope plus the TS-signed
        // scope/kind/payload the cross-language fixture pins.
        assert_eq!(
            serde_json::to_value(event).unwrap(),
            serde_json::json!({
                "type": "event",
                "scope": fixture["scope"],
                "seq": index + 1,
                "epoch": "epoch-1",
                "kind": fixture["kind"],
                "payload": fixture["payload"],
            })
        );
    }
}

#[test]
fn the_production_presence_sink_surfaces_the_hub_error_and_emits_no_frame() {
    // Failure-preservation through the SAME production path: a transport fault
    // must reach the caller as the exact `ScopeEventHubError` and leave nothing
    // on the scope. Swallowing the error, remapping its cause, or bypassing the
    // sink would each break this gate.
    let hub = ScopeEventHub::test("epoch-1");
    let sink = HubPresencePublisher::new(hub.clone());
    let epoch = TransportEpoch::parse("epoch-1").unwrap();

    // Materialize the app scope, then drive it to the sequence ceiling so the
    // next publish must fail.
    hub.resume(&EventScope::App, &epoch, TransportSeq::ZERO)
        .unwrap();
    {
        let mut scopes = hub.scopes.lock().unwrap();
        scopes.get_mut(&EventScope::App).unwrap().seq = TransportSeq(MAX_TRANSPORT_SEQ);
    }

    let error = PresenceFactSink::publish(
        &sink,
        PresenceFact::Online {
            user_id: UserId::parse("viewer-1").unwrap(),
        },
    )
    .unwrap_err();
    assert!(matches!(error, ScopeEventHubError::SequenceExhausted));

    // Zero frames buffered: the reservation failed before the ring was touched.
    let scopes = hub.scopes.lock().unwrap();
    assert!(scopes.get(&EventScope::App).unwrap().replay.is_empty());
}

// ---- The composed presence vertical, end to end -------------------------
//
// These build the vertical through the ONE constructor the composition root
// builds it with (`compose_presence_vertical`), over the REAL adapters: the
// sqlite last-seen sink writes `users.last_seen_at`, the sqlite cold read loads
// it back for the read face, and the real `HubPresencePublisher` projects the
// transition onto the global `app` scope. Only the disconnect clock (frozen, so
// the sealed instant is a value these tests name) and the grace window (short,
// so a window can expire inside a test) differ from production. A fake sink, a
// second registry behind the read face, or a window that never reached the
// database would each fail here.
//
// Timing is REAL, not virtual. The durable half is genuine I/O, and under a
// paused clock tokio auto-advances whenever the runtime waits on that I/O —
// which would fire the pending window early and make "still online inside the
// window" untestable. Determinism comes from the runtime shape instead: these
// run on the current-thread runtime, so a window's task cannot be polled while
// the test body executes synchronous statements. A disconnect followed with NO
// intervening `await` by a reconnect or a dispose is therefore guaranteed to be
// observed before the window can fire, and every positive effect is awaited by
// polling the authority that records it rather than by guessing a delay.

/// Long enough that a read inside the window cannot race the expiry, short
/// enough that crossing it costs a test a fraction of a second.
const VERTICAL_GRACE_MS: i64 = 500;

/// The instant the frozen disconnect clock seals. The database, the read face
/// and the wire must all carry THIS value; a stamp re-derived at expiry, or
/// fabricated, fails.
const SEALED_DISCONNECT_MS: i64 = 1_700_000_000_123;

/// A disconnect clock frozen at one instant, so the offline stamp under test is
/// a value named here rather than whatever the wall clock happened to read.
struct FrozenDisconnectClock(i64);

impl DisconnectClock for FrozenDisconnectClock {
    type Error = Infallible;

    fn read_unix_ms(&self) -> Result<i64, Self::Error> {
        Ok(self.0)
    }
}

#[test]
fn a_clock_reading_past_the_i64_instant_range_is_a_named_fault_not_a_wrapped_instant() {
    use std::time::Duration;

    // `i64::MAX` ms is the largest instant the `i64` store can hold; it narrows
    // exactly. (The tighter MAX_SAFE_INTEGER wire ceiling is a downstream concern.)
    let boundary = Duration::from_millis(i64::MAX as u64);
    assert_eq!(duration_to_unix_ms(boundary).unwrap(), i64::MAX);

    // One millisecond past it must be refused as a named overflow, NOT wrapped
    // (an unchecked `as i64` here yields `i64::MIN`, a plausible-looking stamp).
    let over = boundary + Duration::from_millis(1);
    match duration_to_unix_ms(over) {
        Err(SystemClockFault::MillisOverflow(millis)) => {
            assert_eq!(millis, i64::MAX as u128 + 1);
        }
        other => panic!("expected MillisOverflow at i64::MAX + 1, got {other:?}"),
    }
}

struct Vertical {
    router: Router,
    feed: Arc<dyn ConnectionLifecycle>,
    timers: PresenceGraceTimers<SqliteLastSeenSink, HubPresencePublisher>,
    hub: ScopeEventHub,
    path: std::path::PathBuf,
    _directory: tempfile::TempDir,
}

/// The vertical the composition root composes, over one seeded viewer whose
/// stored stamp starts NULL so every durable write is observable.
async fn presence_vertical() -> Vertical {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("presence.sqlite");
    let mut db = current_schema::create_current_schema(&path).await;
    sqlx::query(
        "INSERT INTO users (id, name, display_name, created_at, updated_at, last_seen_at) \
             VALUES ('viewer-1', 'viewer', 'Viewer', 1, 1, NULL)",
    )
    .execute(&mut db)
    .await
    .unwrap();
    db.close().await.unwrap();

    let hub = ScopeEventHub::test("epoch-1");
    let (router, feed, timers) = compose_presence_vertical(
        SqliteUserPresenceColdRead::open(&path).await.unwrap(),
        SqliteLastSeenSink::open(&path).await.unwrap(),
        hub.presence_publisher(),
        FrozenDisconnectClock(SEALED_DISCONNECT_MS),
        GraceWindow::from_millis_or_default(Some(VERTICAL_GRACE_MS)),
    );
    Vertical {
        router,
        feed,
        timers,
        hub,
        path,
        _directory: directory,
    }
}

fn viewer() -> UserId {
    UserId::parse("viewer-1").unwrap()
}

/// The read face, through the composed router — the cold last-seen facts joined
/// with the live registry the same composition handed the route.
async fn presence_read(router: &Router) -> Value {
    let response = router
        .clone()
        .oneshot(
            axum::http::Request::get(crate::viewer_read_model::user_presence_http::PRESENCE_PATH)
                .extension(viewer())
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// The durable half, read straight from the column the production sink writes.
async fn stored_last_seen(path: &std::path::Path) -> Option<i64> {
    let mut db = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    let stored: Option<i64> =
        sqlx::query_scalar("SELECT last_seen_at FROM users WHERE id = 'viewer-1'")
            .fetch_one(&mut db)
            .await
            .unwrap();
    db.close().await.unwrap();
    stored
}

/// Every frame the global `app` scope holds, in emit order — the wire the real
/// `HubPresencePublisher` published to.
fn app_scope_frames(hub: &ScopeEventHub) -> Vec<Value> {
    let resumed = hub
        .resume(
            &EventScope::App,
            &TransportEpoch::parse("epoch-1").unwrap(),
            TransportSeq::ZERO,
        )
        .unwrap();
    let Replay::Events(events) = resumed.replay else {
        panic!("same epoch must replay the app-scope presence frames")
    };
    events
        .iter()
        .map(|event| serde_json::to_value(event).unwrap())
        .collect()
}

fn presence_frame(seq: u64, presence: &str, last_seen_at: Value) -> Value {
    json!({
        "type": "event",
        "scope": "app",
        "seq": seq,
        "epoch": "epoch-1",
        "kind": "user_presence",
        "payload": {
            "user_id": "viewer-1",
            "presence": presence,
            "last_seen_at": last_seen_at,
        },
    })
}

fn presence_body(presence: &str, last_seen_at: Value) -> Value {
    json!({
        "users": [{
            "user_id": "viewer-1",
            "presence": presence,
            "last_seen_at": last_seen_at,
        }],
    })
}

/// The expiry is real elapsed time and the write it triggers is real I/O, so the
/// durable effect is AWAITED: the column the production sink writes is polled
/// until it holds the sealed instant.
async fn await_sealed_last_seen(path: &std::path::Path) {
    for _ in 0..200 {
        if stored_last_seen(path).await == Some(SEALED_DISCONNECT_MS) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the expiring window never persisted the sealed disconnect instant");
}

/// The wire half is awaited the same way. The publish follows the durable write,
/// so an assertion on the frame list would otherwise race it.
async fn await_frames(hub: &ScopeEventHub, expected: usize) -> Vec<Value> {
    for _ in 0..200 {
        let frames = app_scope_frames(hub);
        if frames.len() >= expected {
            return frames;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the app scope never carried the expected presence frames");
}

/// Passes the pending window's whole deadline, so a negative assertion states
/// "the deadline came and went with no effect" rather than "the test did not
/// wait long enough".
async fn outlast_the_window() {
    tokio::time::sleep(Duration::from_millis(2 * VERTICAL_GRACE_MS as u64)).await;
}

#[tokio::test]
async fn the_composed_vertical_seals_one_disconnect_instant_into_the_database_the_read_face_and_the_wire()
 {
    let v = presence_vertical().await;

    // Connecting announces Online through the SAME sink the wire reads, and the
    // read face joins the live registry the composition handed the route.
    v.feed.on_connect(&viewer(), "c1");
    assert_eq!(
        presence_read(&v.router).await,
        presence_body("online", Value::Null)
    );
    assert_eq!(
        app_scope_frames(&v.hub),
        vec![presence_frame(1, "online", Value::Null)]
    );

    // Inside the window the viewer is STILL online and nothing is durable yet:
    // the offline instant is decided at disconnect but not effected until the
    // window expires.
    v.feed.on_disconnect(&viewer(), "c1");
    assert_eq!(
        presence_read(&v.router).await,
        presence_body("online", Value::Null)
    );
    assert_eq!(stored_last_seen(&v.path).await, None);
    assert_eq!(
        app_scope_frames(&v.hub),
        vec![presence_frame(1, "online", Value::Null)]
    );

    // The window expires: ONE instant — the one the frozen clock sealed at
    // disconnect, not the expiry — lands in the database, the read face and the
    // wire.
    await_sealed_last_seen(&v.path).await;
    let frames = await_frames(&v.hub, 2).await;
    assert_eq!(
        frames,
        vec![
            presence_frame(1, "online", Value::Null),
            presence_frame(2, "offline", json!(SEALED_DISCONNECT_MS)),
        ]
    );
    assert_eq!(
        presence_read(&v.router).await,
        presence_body("offline", json!(SEALED_DISCONNECT_MS))
    );
    assert!(!v.timers.is_online(&viewer()));

    // Another whole window passes with no second offline frame and the stored
    // instant remains unchanged (this read does not count repeated writes).
    outlast_the_window().await;
    assert_eq!(app_scope_frames(&v.hub).len(), 2);
    assert_eq!(stored_last_seen(&v.path).await, Some(SEALED_DISCONNECT_MS));

    // A later connection transitions back to Online through the same sink.
    v.feed.on_connect(&viewer(), "c2");
    assert_eq!(
        app_scope_frames(&v.hub),
        vec![
            presence_frame(1, "online", Value::Null),
            presence_frame(2, "offline", json!(SEALED_DISCONNECT_MS)),
            presence_frame(3, "online", Value::Null),
        ]
    );
    // Coming back does not clear the stored stamp — the live half suppresses it,
    // which is what makes the read face the joined answer rather than either
    // authority alone.
    assert_eq!(
        presence_read(&v.router).await,
        presence_body("online", Value::Null)
    );
}

#[tokio::test]
async fn a_reconnect_inside_the_composed_window_cancels_every_effect_and_a_later_disconnect_seals_once()
 {
    let v = presence_vertical().await;

    // Disconnect then reconnect with NO await between them: on the current-thread
    // runtime the window's task cannot be polled until this body yields, so the
    // reconnect provably lands inside the window.
    v.feed.on_connect(&viewer(), "c1");
    v.feed.on_disconnect(&viewer(), "c1");
    v.feed.on_connect(&viewer(), "c2");

    // The original deadline comes and goes: the superseded window stays silent,
    // so no stale offline crosses it — nothing durable, nothing on the wire — and
    // the reconnect does not re-announce the Online it never left.
    outlast_the_window().await;
    assert_eq!(stored_last_seen(&v.path).await, None);
    assert_eq!(
        app_scope_frames(&v.hub),
        vec![presence_frame(1, "online", Value::Null)]
    );
    assert_eq!(
        presence_read(&v.router).await,
        presence_body("online", Value::Null)
    );

    // The real disconnect that follows still seals exactly once.
    v.feed.on_disconnect(&viewer(), "c2");
    await_sealed_last_seen(&v.path).await;
    let frames = await_frames(&v.hub, 2).await;
    assert_eq!(
        frames,
        vec![
            presence_frame(1, "online", Value::Null),
            presence_frame(2, "offline", json!(SEALED_DISCONNECT_MS)),
        ]
    );
    outlast_the_window().await;
    assert_eq!(app_scope_frames(&v.hub).len(), 2);
}

#[tokio::test]
async fn disposing_the_composed_vertical_drops_the_pending_window_with_no_effect() {
    let v = presence_vertical().await;

    // `dispose` is called on the handle the composition root retains and, at
    // shutdown, disposes AFTER the connections are closed. Disposing while a
    // window is still pending IS that shutdown, and it must not leave an offline
    // transition to fire against a torn-down process.
    v.feed.on_connect(&viewer(), "c1");
    v.feed.on_disconnect(&viewer(), "c1");
    v.timers.dispose();
    assert!(!v.timers.is_online(&viewer()));

    outlast_the_window().await;
    assert_eq!(stored_last_seen(&v.path).await, None);
    assert_eq!(
        app_scope_frames(&v.hub),
        vec![presence_frame(1, "online", Value::Null)]
    );
}

#[tokio::test]
async fn concurrent_pair_mints_publish_only_the_insert_winner() {
    let (_directory, path) = direct_pair_database().await;
    let first = MessagingStore::open_with(&path, FixedEntropy(1))
        .await
        .unwrap();
    let second = MessagingStore::open_with(&path, FixedEntropy(2))
        .await
        .unwrap();
    let hub = ScopeEventHub::test("epoch-1");
    let barrier = Arc::new(Barrier::new(3));

    let first_task = tokio::spawn({
        let barrier = Arc::clone(&barrier);
        let hub = hub.clone();
        async move {
            barrier.wait().await;
            first
                .open_direct_pair(&hub.messaging_events(), pair_participants(), 10)
                .await
                .unwrap()
        }
    });
    let second_task = tokio::spawn({
        let barrier = Arc::clone(&barrier);
        let hub = hub.clone();
        async move {
            barrier.wait().await;
            second
                .open_direct_pair(&hub.messaging_events(), pair_participants(), 11)
                .await
                .unwrap()
        }
    });
    barrier.wait().await;

    let first_pair = first_task.await.unwrap();
    let second_pair = second_task.await.unwrap();
    assert_eq!(first_pair, second_pair);
    assert_eq!(direct_pair_count(&path).await, 1);

    let resumed = hub
        .resume(
            &EventScope::Viewer(user()),
            &TransportEpoch::parse("epoch-1").unwrap(),
            TransportSeq::ZERO,
        )
        .unwrap();
    let Replay::Events(events) = resumed.replay else {
        panic!("the insert winner must be replayable")
    };
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].event,
        ProductEvent::DirectPairsChanged {
            conversation_id: first_pair.as_str().to_owned(),
        }
    );
}

/// The viewer event is only an identity hint. Arrival, unread movement, and
/// clearing all use that same carrier; every resulting value comes from a fresh
/// cold read over committed SQLite state, never from replay sequence arithmetic.
#[tokio::test]
async fn direct_pair_invalidation_and_the_cold_authority_are_symmetric_both_ways() {
    let (_directory, path) = direct_pair_database().await;
    let store = MessagingStore::open_with(&path, FixedEntropy(7))
        .await
        .unwrap();
    let hub = ScopeEventHub::test("epoch-1");
    let cold = SqliteDirectPairReadModel::open(&path).await.unwrap();

    assert!(
        cold.list_direct_pairs(&user())
            .await
            .unwrap()
            .items()
            .is_empty()
    );
    let pair = store
        .open_direct_pair(&hub.messaging_events(), pair_participants(), 10)
        .await
        .unwrap();
    let arrived = cold.list_direct_pairs(&user()).await.unwrap();
    assert_eq!(arrived.items().len(), 1);
    assert_eq!(arrived.items()[0].conversation(), &pair);
    assert_eq!(arrived.items()[0].unread_count().get(), 0);

    let mut db = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO messages (id, conversation_id, from_kind, from_agent_id, content, seq, \
             created_at) VALUES ('message-1', ?, 'agent', 'agent-1', 'hello', 1, 11)",
    )
    .bind(pair.as_str())
    .execute(&mut db)
    .await
    .unwrap();
    db.close().await.unwrap();
    let unread = HubUnreadPublisher::new(
        hub.clone(),
        SqliteUnreadReadModel::open(&path).await.unwrap(),
    );
    UnreadPublication::publish_unread(&unread, &pair)
        .await
        .unwrap();
    assert_eq!(
        cold.list_direct_pairs(&user()).await.unwrap().items()[0]
            .unread_count()
            .get(),
        1
    );

    let mut db = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO conversation_reads (user_id, conversation_id, thread_root_id, \
             last_read_seq, read_at) VALUES ('user-1', ?, NULL, 1, 12)",
    )
    .bind(pair.as_str())
    .execute(&mut db)
    .await
    .unwrap();
    db.close().await.unwrap();
    UnreadPublication::publish_unread(&unread, &pair)
        .await
        .unwrap();
    assert_eq!(
        cold.list_direct_pairs(&user()).await.unwrap().items()[0]
            .unread_count()
            .get(),
        0
    );

    let events = scope_events(&hub, EventScope::Viewer(user()));
    assert_eq!(events.len(), 3);
    assert!(events.iter().all(|event| event
        == &ProductEvent::DirectPairsChanged {
            conversation_id: pair.as_str().to_owned(),
        }));
}

#[tokio::test]
async fn a_committed_mention_copy_publishes_once_through_the_production_viewer_sink() {
    let (_directory, path) = direct_pair_database().await;
    let store = MessagingStore::open(&path).await.unwrap();
    let outcome = store.send_message(mention_command()).await.unwrap();
    assert!(matches!(
        outcome.committed().mention_copies(),
        [silverhand_backend_core::conversation_messaging::messaging::mention_cc::MentionCopyOutcome::Copied { .. }]
    ));
    let hub = ScopeEventHub::test("epoch-1");
    let effects = OtherEffects::default();
    let publisher = hub.messaging_events();

    let report = announce_send(
        outcome.committed(),
        SendEffects {
            direct_lines: &publisher,
            publication: &effects,
            notification: &effects,
            start: &effects,
            delivery: &effects,
            skips: &effects,
        },
    )
    .await;

    assert!(report.failed.is_empty());
    assert_eq!(direct_pair_count(&path).await, 1);
    let resumed = hub
        .resume(
            &EventScope::Viewer(user()),
            &TransportEpoch::parse("epoch-1").unwrap(),
            TransportSeq::ZERO,
        )
        .unwrap();
    let Replay::Events(events) = resumed.replay else {
        panic!("the committed mention copy must reach viewer replay")
    };
    // This outcome contains both the committed copy and the transaction's
    // opened-line coverage. Without announce_send's conversation dedupe the
    // same viewer projector would append two events here. The independent
    // no-copy fallback is an orchestration invariant tested in backend-core.
    assert_eq!(events.len(), 1);
    assert!(matches!(
        events[0].event,
        ProductEvent::DirectPairsChanged { .. }
    ));
}

#[tokio::test]
async fn mention_copy_sink_failure_keeps_its_chain_and_does_not_stop_later_effects() {
    let (_directory, path) = direct_pair_database().await;
    let store = MessagingStore::open(&path).await.unwrap();
    let outcome = store.send_message(mention_command()).await.unwrap();
    let hub = ScopeEventHub::test("epoch-1");
    let registry = Arc::clone(&hub.scopes);
    let _ = std::thread::spawn(move || {
        let _guard = registry.lock().unwrap();
        panic!("poison the test registry");
    })
    .join();
    let effects = OtherEffects::default();
    let publisher = hub.messaging_events();

    let report = announce_send(
        outcome.committed(),
        SendEffects {
            direct_lines: &publisher,
            publication: &effects,
            notification: &effects,
            start: &effects,
            delivery: &effects,
            skips: &effects,
        },
    )
    .await;

    assert_eq!(report.failed.len(), 1);
    assert_eq!(
        report
            .failed
            .iter()
            .map(|(_, step, _)| *step)
            .collect::<Vec<_>>(),
        vec![EffectStep::DirectLineInvalidation]
    );
    assert_eq!(
        report.failed[0].2.source().unwrap().to_string(),
        "scope event registry lock was poisoned"
    );
    assert_eq!(
        effects.calls(),
        vec![
            EffectStep::CopyPublished,
            EffectStep::AuthorNotified,
            EffectStep::RuntimeStarted,
            EffectStep::CopyDelivered,
        ]
    );
    assert_eq!(direct_pair_count(&path).await, 1);
}

#[tokio::test]
async fn announcement_failure_does_not_rollback_the_autocommitted_pair() {
    let (_directory, path) = direct_pair_database().await;
    let store = MessagingStore::open_with(&path, FixedEntropy(3))
        .await
        .unwrap();

    let error = store
        .open_direct_pair(&FailingOpened, pair_participants(), 10)
        .await
        .unwrap_err();
    assert!(matches!(error, OpenDirectPairError::Announcement(_)));
    assert_eq!(
        error.source().unwrap().to_string(),
        "announcement unavailable"
    );
    assert_eq!(direct_pair_count(&path).await, 1);
}

#[tokio::test]
async fn later_product_refusal_neither_compensates_nor_reannounces_the_pair() {
    let (_directory, path) = direct_pair_database().await;
    let store = MessagingStore::open_with(&path, FixedEntropy(4))
        .await
        .unwrap();
    let hub = ScopeEventHub::test("epoch-1");

    let pair = store
        .open_direct_pair(&hub.messaging_events(), pair_participants(), 10)
        .await
        .unwrap();
    assert_eq!(
        reject_later_product_action(&pair).unwrap_err().to_string(),
        "capability denied"
    );

    let existing = store
        .open_direct_pair(&hub.messaging_events(), pair_participants(), 11)
        .await
        .unwrap();
    assert_eq!(existing, pair);
    assert_eq!(direct_pair_count(&path).await, 1);
    let resumed = hub
        .resume(
            &EventScope::Viewer(user()),
            &TransportEpoch::parse("epoch-1").unwrap(),
            TransportSeq::ZERO,
        )
        .unwrap();
    let Replay::Events(events) = resumed.replay else {
        panic!("the original announcement must remain replayable")
    };
    assert_eq!(events.len(), 1);
}

#[tokio::test]
async fn resume_registers_live_before_releasing_snapshot() {
    let hub = ScopeEventHub::test("epoch-1");
    hub.publish_direct_pairs_changed(&user(), &conversation())
        .unwrap();
    let resumed = hub
        .resume(
            &EventScope::Viewer(user()),
            &TransportEpoch::parse("epoch-1").unwrap(),
            TransportSeq::ZERO,
        )
        .unwrap();
    let mut live = resumed.subscription.unwrap().receiver;
    hub.publish_direct_pairs_changed(&user(), &conversation())
        .unwrap();
    assert_eq!(live.recv().await.unwrap().seq, TransportSeq(2));
}

#[test]
fn heartbeat_requires_a_pong_before_the_deadline() {
    let mut heartbeat = Heartbeat::default();
    assert!(matches!(heartbeat.on_ping_due(), HeartbeatAction::Ping));
    assert!(matches!(heartbeat.on_ping_due(), HeartbeatAction::Close));
    heartbeat.on_pong();
    assert!(matches!(heartbeat.on_ping_due(), HeartbeatAction::Ping));
}

#[derive(Clone)]
struct Allows;

impl AuthorizeEventScope for Allows {
    async fn authorize_event_scope(
        &self,
        _viewer: &UserId,
        _scope: &EventScope,
    ) -> Result<bool, PersistenceFailure> {
        Ok(true)
    }
}

#[derive(Clone)]
struct Sessions;

impl AuthenticateHumanSession for Sessions {
    async fn recheck_session(
        &self,
        bearer: SessionBearer,
    ) -> Result<Option<UserId>, PersistenceFailure> {
        self.authenticate_session(bearer).await
    }
    async fn authenticate_session(
        &self,
        _bearer: SessionBearer,
    ) -> Result<Option<UserId>, PersistenceFailure> {
        Ok(Some(user()))
    }
}

/// One recorded lifecycle call: (kind, viewer id, connection id).
type LifecycleEntry = (&'static str, String, String);

/// Records every lifecycle call so a test can prove the connect/disconnect
/// pairing. Cloning shares the underlying log, so the copy handed to `router`
/// and the copy the test inspects are the same ledger.
#[derive(Clone, Default)]
struct LifecycleLog {
    entries: Arc<Mutex<Vec<LifecycleEntry>>>,
}

impl LifecycleLog {
    fn of_kind(&self, kind: &str) -> Vec<(String, String)> {
        self.entries
            .lock()
            .unwrap()
            .iter()
            .filter(|(entry_kind, _, _)| *entry_kind == kind)
            .map(|(_, viewer, id)| (viewer.clone(), id.clone()))
            .collect()
    }
}

impl ConnectionLifecycle for LifecycleLog {
    fn on_connect(&self, viewer: &UserId, connection_id: &str) {
        self.entries.lock().unwrap().push((
            "connect",
            viewer.as_str().to_owned(),
            connection_id.to_owned(),
        ));
    }

    fn on_disconnect(&self, viewer: &UserId, connection_id: &str) {
        self.entries.lock().unwrap().push((
            "disconnect",
            viewer.as_str().to_owned(),
            connection_id.to_owned(),
        ));
    }
}

async fn wait_until_drained(controller: &EventWebSocketController) {
    for _ in 0..200 {
        if controller.active_connection_count() == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the connection never drained after termination");
}

#[test]
fn the_lifetime_bracket_pairs_one_connect_with_exactly_one_drop_disconnect() {
    let log = LifecycleLog::default();
    let viewer = user().as_str().to_owned();
    {
        let _lifetime = ConnectionLifetime::begin(Arc::new(log.clone()), user(), "42".to_owned());
        assert_eq!(
            log.of_kind("connect"),
            vec![(viewer.clone(), "42".to_owned())],
            "on_connect fires once, at admission"
        );
        assert!(
            log.of_kind("disconnect").is_empty(),
            "no disconnect while the bracket is live"
        );
    }
    // The bracket left scope: Drop fired the matching disconnect.
    assert_eq!(
        log.of_kind("disconnect"),
        vec![(viewer, "42".to_owned())],
        "Drop fires the matching disconnect exactly once"
    );
    assert_eq!(log.of_kind("connect").len(), 1);
}

#[tokio::test]
async fn a_client_close_fires_exactly_one_disconnect_for_the_admitted_connection() {
    let hub = ScopeEventHub::test("epoch-1");
    let origin = "http://127.0.0.1";
    let log = LifecycleLog::default();
    let (app, controller) = router(
        hub,
        Sessions,
        Allows,
        Arc::new(log.clone()),
        EventWebSocketConfig::test(origin),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let mut socket = connect(address, origin).await;
    assert_eq!(next_json(&mut socket).await["type"], "authenticated");
    socket.close(None).await.unwrap();
    wait_until_drained(&controller).await;

    let connects = log.of_kind("connect");
    let disconnects = log.of_kind("disconnect");
    assert_eq!(connects.len(), 1, "admission connects exactly once");
    assert_eq!(
        disconnects.len(),
        1,
        "the sole termination disconnects exactly once — never zero, never twice"
    );
    assert_eq!(
        connects, disconnects,
        "connect and disconnect name the same admitted viewer and connection id"
    );
    assert_eq!(connects[0].0, user().as_str());

    server.abort();
}

#[tokio::test]
async fn a_server_shutdown_fires_exactly_one_disconnect_for_a_live_connection() {
    let hub = ScopeEventHub::test("epoch-1");
    let origin = "http://127.0.0.1";
    let log = LifecycleLog::default();
    let (app, controller) = router(
        hub,
        Sessions,
        Allows,
        Arc::new(log.clone()),
        EventWebSocketConfig::test(origin),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let mut socket = connect(address, origin).await;
    assert_eq!(next_json(&mut socket).await["type"], "authenticated");
    assert_eq!(controller.active_connection_count(), 1);

    // Server-driven teardown: the shutdown signal (or, past the grace window,
    // a task abort) must still run the bracket's Drop exactly once.
    controller.close().await;
    wait_until_drained(&controller).await;

    assert_eq!(log.of_kind("connect").len(), 1);
    assert_eq!(
        log.of_kind("disconnect").len(),
        1,
        "a server shutdown disconnects the live connection exactly once"
    );

    server.abort();
    let _ = socket.close(None).await;
}

#[derive(Clone)]
struct RejectsSession;

impl AuthenticateHumanSession for RejectsSession {
    async fn recheck_session(
        &self,
        bearer: SessionBearer,
    ) -> Result<Option<UserId>, PersistenceFailure> {
        self.authenticate_session(bearer).await
    }
    async fn authenticate_session(
        &self,
        _bearer: SessionBearer,
    ) -> Result<Option<UserId>, PersistenceFailure> {
        Ok(None)
    }
}

#[tokio::test]
async fn a_rejected_handshake_never_connects_and_never_ghosts_online() {
    let hub = ScopeEventHub::test("epoch-1");
    let origin = "http://127.0.0.1";
    let log = LifecycleLog::default();
    let (app, controller) = router(
        hub,
        RejectsSession,
        Allows,
        Arc::new(log.clone()),
        EventWebSocketConfig::test(origin),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    // The upgrade is refused (401) before any connection is accepted, so the
    // client handshake fails outright.
    let mut request = format!("ws://{address}{EVENT_WEBSOCKET_PATH}")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert(ORIGIN, origin.parse().unwrap());
    request.headers_mut().insert(
        COOKIE,
        format!("{EVENT_WS_SESSION_COOKIE}=token").parse().unwrap(),
    );
    request
        .headers_mut()
        .insert(DESKTOP_API_EPOCH_HEADER, MINIMUM_CLIENT_API_EPOCH.into());
    connect_async(request)
        .await
        .expect_err("an unauthenticated upgrade must be refused, not accepted");

    assert_eq!(controller.active_connection_count(), 0);
    assert!(
        log.of_kind("connect").is_empty(),
        "a rejected handshake must never admit a connection"
    );
    assert!(
        log.of_kind("disconnect").is_empty(),
        "no admission means no disconnect — a refused handshake never ghosts online"
    );

    server.abort();
}

#[tokio::test]
async fn subscriptions_are_independent_for_the_complete_scope_key() {
    let hub = ScopeEventHub::test("epoch-1");
    let (output, mut received) = mpsc::channel(8);
    let viewer_scope = EventScope::Viewer(user());
    let conversation_scope = EventScope::Conversation(conversation());
    let viewer = relay(
        hub.subscribe(viewer_scope.clone()).unwrap(),
        hub.clone(),
        viewer_scope,
        output.clone(),
    );
    let conversation = relay(
        hub.subscribe(conversation_scope.clone()).unwrap(),
        hub.clone(),
        conversation_scope.clone(),
        output,
    );

    viewer.task.abort();
    hub.publish(
        conversation_scope,
        ProductEvent::Test {
            value: "still-live".to_owned(),
        },
    )
    .unwrap();
    let QueuedScopeEvent::Event(event) = received.recv().await.unwrap().event else {
        panic!("expected event")
    };
    assert_eq!(event.scope, "conversation:pair-1");
    conversation.task.abort();
}

#[tokio::test]
async fn a_poisoned_registry_never_becomes_a_zero_snapshot_baseline() {
    let hub = ScopeEventHub::test("epoch-1");
    let scope = EventScope::Viewer(user());
    let subscription = hub.subscribe(scope.clone()).unwrap();
    let sender = hub.scopes.lock().unwrap().get(&scope).unwrap().live.clone();
    for seq in 1..=(LIVE_CAPACITY * 2) {
        sender
            .send(ScopedEvent {
                published_at: 0,
                frame_type: "event",
                scope: scope_wire(&scope),
                seq: TransportSeq(seq as u64),
                epoch: "epoch-1".to_owned(),
                event: ProductEvent::Test {
                    value: "lag".to_owned(),
                },
            })
            .unwrap();
    }
    let registry = Arc::clone(&hub.scopes);
    let _ = std::thread::spawn(move || {
        let _guard = registry.lock().unwrap();
        panic!("poison the test registry");
    })
    .join();

    let (output, mut received) = mpsc::channel(1);
    let task = relay(subscription, hub, scope, output);
    let QueuedScopeEvent::Failure(cause) = received.recv().await.unwrap().event else {
        panic!("registry failure must close, never emit a seq-zero snapshot")
    };
    assert_eq!(cause.to_string(), "scope event registry lock was poisoned");
    task.task.await.unwrap();
}

#[tokio::test]
async fn one_socket_keeps_multiple_scopes_and_controller_closes_it() {
    let hub = ScopeEventHub::test("epoch-1");
    let origin = "http://127.0.0.1";
    let (app, controller) = router(
        hub.clone(),
        Sessions,
        Allows,
        Arc::new(NoopConnectionLifecycle),
        EventWebSocketConfig::test(origin),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut socket = connect(address, origin).await;
    assert_eq!(next_json(&mut socket).await["type"], "authenticated");

    for scope in ["viewer:self", "conversation:pair-1"] {
        socket
            .send(ClientMessage::Text(
                serde_json::json!({ "type": "subscribe", "scope": scope })
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        assert_eq!(next_json(&mut socket).await["type"], "subscribed");
    }
    assert_eq!(controller.active_connection_count(), 1);

    hub.publish(
        EventScope::Viewer(user()),
        ProductEvent::Test {
            value: "viewer".to_owned(),
        },
    )
    .unwrap();
    hub.publish(
        EventScope::Conversation(conversation()),
        ProductEvent::Test {
            value: "conversation".to_owned(),
        },
    )
    .unwrap();
    let mut scopes = [
        next_json(&mut socket).await["scope"]
            .as_str()
            .unwrap()
            .to_owned(),
        next_json(&mut socket).await["scope"]
            .as_str()
            .unwrap()
            .to_owned(),
    ];
    scopes.sort();
    assert_eq!(scopes, ["conversation:pair-1", "viewer:user-1"]);

    socket
        .send(ClientMessage::Text(
            serde_json::json!({ "type": "unsubscribe", "scope": "viewer:self" })
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    socket
        .send(ClientMessage::Text(
            serde_json::json!({ "type": "subscribe", "scope": "app" })
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    assert_eq!(next_json(&mut socket).await["scope"], "app");
    hub.publish(
        EventScope::Viewer(user()),
        ProductEvent::Test {
            value: "must-not-arrive".to_owned(),
        },
    )
    .unwrap();
    hub.publish(
        EventScope::Conversation(conversation()),
        ProductEvent::Test {
            value: "still-live".to_owned(),
        },
    )
    .unwrap();
    let frame = next_json(&mut socket).await;
    assert_eq!(frame["scope"], "conversation:pair-1");
    assert_eq!(frame["payload"]["value"], "still-live");

    controller.close().await;
    let close = socket.next().await.unwrap().unwrap();
    let ClientMessage::Close(Some(close)) = close else {
        panic!("controller must close the socket")
    };
    assert_eq!(u16::from(close.code), 1001);
    assert_eq!(close.reason, "server_shutdown");
    assert_eq!(controller.active_connection_count(), 0);
    server.abort();
}

#[tokio::test]
async fn missing_json_pong_closes_on_the_heartbeat_deadline() {
    let hub = ScopeEventHub::test("epoch-1");
    let origin = "http://127.0.0.1";
    let mut config = EventWebSocketConfig::test(origin);
    config.ping_interval = Duration::from_millis(20);
    config.pong_timeout = Duration::from_millis(20);
    let (app, controller) = router(
        hub,
        Sessions,
        Allows,
        Arc::new(NoopConnectionLifecycle),
        config,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut socket = connect(address, origin).await;
    assert_eq!(next_json(&mut socket).await["type"], "authenticated");
    assert_eq!(next_json(&mut socket).await["type"], "ping");
    let close = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let ClientMessage::Close(Some(close)) = close else {
        panic!("missing pong must close the socket")
    };
    assert_eq!(u16::from(close.code), 1008);
    assert_eq!(close.reason, "pong_timeout");
    controller.close().await;
    server.abort();
}

#[tokio::test]
async fn control_pong_does_not_satisfy_the_json_heartbeat() {
    let hub = ScopeEventHub::test("epoch-1");
    let origin = "http://127.0.0.1";
    let mut config = EventWebSocketConfig::test(origin);
    config.ping_interval = Duration::from_millis(20);
    config.pong_timeout = Duration::from_millis(20);
    let (app, controller) = router(
        hub,
        Sessions,
        Allows,
        Arc::new(NoopConnectionLifecycle),
        config,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut socket = connect(address, origin).await;
    assert_eq!(next_json(&mut socket).await["type"], "authenticated");
    assert_eq!(next_json(&mut socket).await["type"], "ping");
    socket
        .send(ClientMessage::Pong(Vec::new().into()))
        .await
        .unwrap();
    let close = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let ClientMessage::Close(Some(close)) = close else {
        panic!("control pong must not clear the application heartbeat")
    };
    assert_eq!(u16::from(close.code), 1008);
    assert_eq!(close.reason, "pong_timeout");
    controller.close().await;
    server.abort();
}

#[tokio::test]
async fn json_pong_clears_the_application_heartbeat() {
    let hub = ScopeEventHub::test("epoch-1");
    let origin = "http://127.0.0.1";
    let mut config = EventWebSocketConfig::test(origin);
    config.ping_interval = Duration::from_millis(20);
    config.pong_timeout = Duration::from_millis(20);
    let (app, controller) = router(
        hub,
        Sessions,
        Allows,
        Arc::new(NoopConnectionLifecycle),
        config,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut socket = connect(address, origin).await;
    assert_eq!(next_json(&mut socket).await["type"], "authenticated");
    assert_eq!(next_json(&mut socket).await["type"], "ping");
    socket
        .send(ClientMessage::Text(
            serde_json::json!({ "type": "pong" }).to_string().into(),
        ))
        .await
        .unwrap();
    assert_eq!(next_json(&mut socket).await["type"], "ping");
    socket
        .send(ClientMessage::Text(
            serde_json::json!({ "type": "pong" }).to_string().into(),
        ))
        .await
        .unwrap();
    controller.close().await;
    let close = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let ClientMessage::Close(Some(close)) = close else {
        panic!("controller must close a healthy socket")
    };
    assert_eq!(u16::from(close.code), 1001);
    assert_eq!(close.reason, "server_shutdown");
    server.abort();
}

#[tokio::test]
async fn payload_over_sixty_four_kib_is_closed_before_json_dispatch() {
    let hub = ScopeEventHub::test("epoch-1");
    let origin = "http://127.0.0.1";
    let (app, controller) = router(
        hub,
        Sessions,
        Allows,
        Arc::new(NoopConnectionLifecycle),
        EventWebSocketConfig::test(origin),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut socket = connect(address, origin).await;
    assert_eq!(next_json(&mut socket).await["type"], "authenticated");
    socket
        .send(ClientMessage::Text(
            format!("\"{}\"", "x".repeat(MAX_INBOUND_BYTES)).into(),
        ))
        .await
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .unwrap();
    match result {
        Some(Ok(ClientMessage::Close(Some(close)))) => {
            assert_eq!(u16::from(close.code), 1009);
        }
        Some(Err(tokio_tungstenite::tungstenite::Error::Protocol(_)))
        | Some(Err(tokio_tungstenite::tungstenite::Error::Io(_)))
        | None => {}
        other => panic!("oversized payload must close before dispatch: {other:?}"),
    }
    controller.close().await;
    server.abort();
}

// ------------------------------------------------------------------
// Terminal multi-port consistency gate.
//
// The five product-event effect ports are NOT five independent audience
// decisions. One committed send drives several of them at once through the
// shared placement/audience projection (the read model's participant query and
// the one committed-message authority). These cells prove that sharing causes no
// duplicate message frame, no duplicate or missing unread/badge co-emit, and no
// widening of a copy-only viewer into the conversation's audience — driven by the
// real `announce_*` orchestration over the real SQLite read model, never a
// hand-rolled audience. Delivery/idle are the only doubles: those effects belong
// to #2185's daemon lane, not to this projection.
// ------------------------------------------------------------------

/// Every autolink span in the body, so one send can name a human and an agent at once.
struct AllReferences;

impl ReferencePositions for AllReferences {
    fn autolink_spans(&self, content: &MessageContent) -> Vec<std::ops::Range<usize>> {
        let text = content.as_str();
        let mut spans = Vec::new();
        let mut cursor = 0;
        while let Some(relative) = text[cursor..].find('<') {
            let start = cursor + relative;
            let end = start + text[start..].find('>').expect("a closing bracket") + 1;
            spans.push(start..end);
            cursor = end;
        }
        spans
    }
}

/// The one agent handle this world can reach privately maps to the seeded agent —
/// a name resolved to an id, never a handle parsed as one.
struct DirectLinesToExternal;

impl DirectLineAuthority for DirectLinesToExternal {
    async fn direct_lines(
        &self,
        _author: &UserId,
        addressed: &AddressedPrincipals,
    ) -> Result<Vec<AgentId>, PersistenceFailure> {
        Ok(addressed
            .iter()
            .filter(|reference| reference.kind() == PrincipalKind::Agent)
            .filter(|reference| reference.handle().as_str() == "external")
            .map(|_| AgentId::parse("agent-1").unwrap())
            .collect())
    }
}

fn mentioned() -> UserId {
    UserId::parse("user-2").unwrap()
}

fn channel() -> ConversationId {
    ConversationId::parse("conversation-1").unwrap()
}

/// A workspace channel with two seated humans — the author and a reader who will be
/// named — plus a workspace member who is NOT seated (the no-widening witness) and an
/// agent reachable only over a direct line (the copy-only viewer).
async fn seed_fanout(path: &std::path::Path) {
    let mut db = current_schema::create_current_schema(path).await;
    for statement in [
        "INSERT INTO users (id, name, display_name, created_at, updated_at) \
             VALUES ('user-1', 'sender', 'Sender', 1, 1)",
        "INSERT INTO users (id, name, display_name, created_at, updated_at) \
             VALUES ('user-2', 'mentioned', 'Mentioned', 2, 2)",
        "INSERT INTO users (id, name, display_name, created_at, updated_at) \
             VALUES ('user-3', 'bystander', 'Bystander', 3, 3)",
        "INSERT INTO hosts (id, name, kind, created_at) VALUES ('host-1', 'host', 'local', 1)",
        "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, \
             display_name, created_by_user_id) VALUES \
             ('agent-1', 'external', 'claude', 'host-1', 1, 1, 'External', 'user-1')",
        "INSERT INTO workspaces (id, name, name_key, owner_user_id, created_at, updated_at) \
             VALUES ('workspace-1', 'Space', 'space', 'user-1', 1, 1)",
        "INSERT INTO workspace_users (workspace_id, user_id, joined_at, role) \
             VALUES ('workspace-1', 'user-1', 1, 'owner')",
        "INSERT INTO workspace_users (workspace_id, user_id, joined_at, role) \
             VALUES ('workspace-1', 'user-2', 1, 'member')",
        "INSERT INTO workspace_users (workspace_id, user_id, joined_at, role) \
             VALUES ('workspace-1', 'user-3', 1, 'member')",
        "INSERT INTO conversations (id, workspace_id, placement_kind, roster_policy, title, \
             title_key, visibility, created_at, updated_at) VALUES \
             ('conversation-1', 'workspace-1', 'workspace', 'open', 'Channel', 'channel', \
             'private', 1, 1)",
        "INSERT INTO conversation_users (conversation_id, user_id, joined_at) \
             VALUES ('conversation-1', 'user-1', 1)",
        "INSERT INTO conversation_users (conversation_id, user_id, joined_at) \
             VALUES ('conversation-1', 'user-2', 1)",
    ] {
        sqlx::query(statement).execute(&mut db).await.unwrap();
    }
    db.close().await.unwrap();
}

/// A direct line between two humans, its own placement with no workspace tier.
async fn seed_direct_pair(path: &std::path::Path) {
    let mut db = current_schema::create_current_schema(path).await;
    for statement in [
        "INSERT INTO users (id, name, display_name, created_at, updated_at) \
             VALUES ('user-1', 'sender', 'Sender', 1, 1)",
        "INSERT INTO users (id, name, display_name, created_at, updated_at) \
             VALUES ('user-2', 'mentioned', 'Mentioned', 2, 2)",
        "INSERT INTO conversations (id, endpoint_a_kind, endpoint_a_id, endpoint_b_kind, \
             endpoint_b_id, placement_kind, roster_policy, title, visibility, \
             created_at, updated_at) VALUES \
             ('conversation-2', 'human', 'user-1', 'human', 'user-2', 'direct_pair', 'fixed', \
             'Direct', 'private', 1, 1)",
        "INSERT INTO conversation_users (conversation_id, user_id, joined_at) \
             VALUES ('conversation-2', 'user-1', 1)",
        "INSERT INTO conversation_users (conversation_id, user_id, joined_at) \
             VALUES ('conversation-2', 'user-2', 1)",
    ] {
        sqlx::query(statement).execute(&mut db).await.unwrap();
    }
    db.close().await.unwrap();
}

/// A person's send that names a seated reader (a `mention_self`) and an agent that is
/// only reachable off-channel (a mention-CC copy) — one committed send, several ports.
fn fanout_command() -> SendMessage {
    ready(SendMessage::prepare(
        SendMessageDraft {
            source_conversation: channel(),
            target_conversation: channel(),
            actor: SendActor::User(user()),
            content: MessageContent::parse(
                "<silverhand://human/mentioned> <silverhand://agent/external> ping",
            )
            .unwrap(),
            destination: SendDestination::MainLine,
            attachment_refs: Vec::new(),
            freshness_bypass: false,
        },
        &AllReferences,
        &DirectLinesToExternal,
    ))
    .unwrap()
}

/// A person's send that names no one — enough to make a reader's unread move.
fn plain_command() -> SendMessage {
    ready(SendMessage::prepare(
        SendMessageDraft {
            source_conversation: channel(),
            target_conversation: channel(),
            actor: SendActor::User(user()),
            content: MessageContent::parse("hello").unwrap(),
            destination: SendDestination::MainLine,
            attachment_refs: Vec::new(),
            freshness_bypass: false,
        },
        &AllReferences,
        &DirectLinesToExternal,
    ))
    .unwrap()
}

/// A person's send that names a workspace member who is NOT seated in the target
/// channel — the bystander. The name resolves to a real user but reaches nobody in
/// this conversation, so the outcome carries a sender-only `unresolved_references`
/// that the broadcast frame must never mirror.
fn names_a_bystander_command() -> SendMessage {
    ready(SendMessage::prepare(
        SendMessageDraft {
            source_conversation: channel(),
            target_conversation: channel(),
            actor: SendActor::User(user()),
            content: MessageContent::parse("<silverhand://human/bystander> ping").unwrap(),
            destination: SendDestination::MainLine,
            attachment_refs: Vec::new(),
            freshness_bypass: false,
        },
        &AllReferences,
        &DirectLinesToExternal,
    ))
    .unwrap()
}

/// A reply to one message — the arm whose frame carries a `projections` sidecar.
fn reply_command(target: &MessageId) -> SendMessage {
    ready(SendMessage::prepare(
        SendMessageDraft {
            source_conversation: channel(),
            target_conversation: channel(),
            actor: SendActor::User(user()),
            content: MessageContent::parse("a reply").unwrap(),
            destination: SendDestination::Reply(target.clone()),
            attachment_refs: Vec::new(),
            freshness_bypass: false,
        },
        &AllReferences,
        &DirectLinesToExternal,
    ))
    .unwrap()
}

/// The ONE `message` frame the production door emits to the channel for a committed
/// send — captured off the real hub, not re-derived. `Message` is the only frame this
/// helper asks for; a caller that seeds mentions/unread would see those on other scopes.
async fn emitted_channel_message(view: CommittedMessageView<'_>) -> Value {
    let hub = ScopeEventHub::test("epoch-1");
    CommittedMessagePublication::publish_message(&hub.messaging_events(), view)
        .await
        .unwrap();
    match scope_events(&hub, EventScope::Conversation(channel())).as_slice() {
        [ProductEvent::Message(payload)] => payload.clone(),
        other => panic!("the door must publish exactly one message frame, got {other:?}"),
    }
}

/// The ACK is the conversation frame PLUS a sender-only diagnostic — never a second
/// shape, and the frame never carries the diagnostic.
///
/// `send_ack` and the broadcast door both project the shared `{message, projections?}`
/// from the SAME committed outcome; the only difference the ACK may carry is
/// `unresolved_references`, whose audience is one. This asserts the whole JSON: strip
/// that one member from the ACK and it must equal the frame byte for byte, and the frame
/// must never have carried it.
fn assert_ack_is_frame_plus_unresolved(
    frame: &Value,
    outcome: &SendMessageOutcome,
    expects_unresolved: bool,
) {
    assert!(
        frame.get("unresolved_references").is_none(),
        "the broadcast frame must never carry the sender-only diagnostic: {frame}"
    );
    let mut ack = crate::conversation_messaging::messaging::ack::send_ack(outcome);
    let removed = ack
        .as_object_mut()
        .expect("the ACK is an object")
        .remove("unresolved_references");
    assert_eq!(
        removed.is_some(),
        expects_unresolved,
        "unresolved_references presence disagreed with the outcome"
    );
    assert_eq!(
        &ack, frame,
        "the ACK's shared half must equal the conversation frame whole"
    );
}

/// The sender's ACK and everyone's `message` frame are ONE projection from ONE
/// committed send, on BOTH arms. The main-line arm names an unseated bystander, so its
/// ACK carries `unresolved_references` the frame must not; the reply arm carries the
/// `projections` sidecar on both. If `send_ack` and the broadcast door ever read
/// different reply sources, or the sender-only member leaks onto the wire, the whole-JSON
/// equality breaks.
#[tokio::test]
async fn the_ack_is_the_conversation_frame_plus_a_sender_only_diagnostic() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("ack-parity.sqlite");
    seed_fanout(&path).await;
    let store = MessagingStore::open(&path).await.unwrap();

    // MAIN-LINE arm: no projections, and a bystander reference that reaches nobody.
    let main = store
        .send_message(names_a_bystander_command())
        .await
        .unwrap();
    let main_view = main
        .committed()
        .main_line()
        .expect("a main-line send")
        .published();
    assert!(
        main_view.reply_projection().is_none(),
        "a main-line frame carries no projections sidecar"
    );
    let main_frame = emitted_channel_message(main_view).await;
    assert_ack_is_frame_plus_unresolved(&main_frame, &main, true);

    // REPLY arm: the root, then a reply to it — the frame gains a projections sidecar
    // and the equality must still hold arm-for-arm.
    let root = store.send_message(plain_command()).await.unwrap();
    let root_id = root.committed().message().id.clone();
    let reply = store.send_message(reply_command(&root_id)).await.unwrap();
    let reply_view = reply
        .committed()
        .reply_cluster()
        .expect("a reply send")
        .published();
    assert!(
        reply_view.reply_projection().is_some(),
        "a reply frame carries the projections sidecar"
    );
    let reply_frame = emitted_channel_message(reply_view).await;
    assert!(
        reply_frame.get("projections").is_some(),
        "the reply frame must carry projections: {reply_frame}"
    );
    assert_ack_is_frame_plus_unresolved(&reply_frame, &reply, false);
}

/// One viewer's replay, as the product events alone — the retained transport window,
/// used as an ordering witness (NOT a cold read of committed state).
fn scope_events(hub: &ScopeEventHub, scope: EventScope) -> Vec<ProductEvent> {
    let resumed = hub
        .resume(
            &scope,
            &TransportEpoch::parse("epoch-1").unwrap(),
            TransportSeq::ZERO,
        )
        .unwrap();
    let Replay::Events(events) = resumed.replay else {
        panic!("the retained window must hold this scope's whole replay")
    };
    events.into_iter().map(|event| event.event).collect()
}

/// One viewer's unread state as a FRESH read-model computation over the committed
/// database — the cold authority a page load would run, independent of any frame the
/// transport happens to be holding. The live co-emit must equal this field for field.
async fn workspace_cold_viewer(
    path: &std::path::Path,
    conversation: &ConversationId,
    viewer: &UserId,
) -> ViewerUnread {
    let model = SqliteUnreadReadModel::open(path).await.unwrap();
    let UnreadAudience::Workspace { viewers, .. } = model
        .unread_audience(conversation)
        .await
        .unwrap()
        .expect("the conversation is present")
    else {
        panic!("a workspace conversation must yield a workspace audience");
    };
    viewers
        .into_iter()
        .find(|candidate| &candidate.viewer == viewer)
        .expect("the viewer is owed an unread update")
}

/// Asserts a live co-emit carries exactly the cold read-model's unread state for a
/// viewer — the projection is a faithful carrier of committed state, not a second
/// opinion computed on the effect path.
fn assert_co_emit_matches_cold(unread: &ProductEvent, badge: &ProductEvent, cold: &ViewerUnread) {
    let (ProductEvent::UnreadUpdated(unread), ProductEvent::AttentionBadgeUpdated(badge)) =
        (unread, badge)
    else {
        panic!("expected an unread+badge co-emit, got {unread:?} then {badge:?}");
    };
    assert_eq!(
        unread.conversation_unread_count, cold.conversation_unread_count,
        "conversation unread diverged from the cold read model"
    );
    assert_eq!(
        unread.workspace_unread_count, cold.workspace_unread_count,
        "workspace unread diverged from the cold read model"
    );
    // WHOLE-VALUE, not just presence: a mis-projected anchor coordinate
    // (conversation_id/message_id/seq/thread_root_id) must fail here, so the live
    // anchor is compared field for field to the cold read-model row projected through
    // the SAME `latest_unread_wire` the production port uses.
    assert_eq!(
        unread.latest_unread,
        cold.latest_unread.as_ref().map(latest_unread_wire),
        "the latest-unread anchor diverged from the cold read model"
    );
    assert_eq!(
        badge.badge_count, cold.badge_count,
        "the attention badge diverged from the cold read model"
    );
}

/// Advances a reader's main-line cursor past everything in a conversation.
async fn mark_all_read(path: &std::path::Path, viewer: &str, conversation: &str) {
    let mut db = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
        .await
        .unwrap();
    let latest: i64 =
        sqlx::query_scalar("SELECT COALESCE(MAX(seq), 0) FROM messages WHERE conversation_id = ?")
            .bind(conversation)
            .fetch_one(&mut db)
            .await
            .unwrap();
    sqlx::query(
        "INSERT INTO conversation_reads (user_id, conversation_id, thread_root_id, \
             last_read_seq, read_at) VALUES (?, ?, NULL, ?, 2)",
    )
    .bind(viewer)
    .bind(conversation)
    .bind(latest)
    .execute(&mut db)
    .await
    .unwrap();
    db.close().await.unwrap();
}

/// Enrolls a human as a follower of one thread, so its replies count toward their
/// unread — the precondition for a THREADED latest-unread anchor.
async fn follow_thread(path: &std::path::Path, viewer: &str, conversation: &str, root: &str) {
    let mut db = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO thread_followers (thread_root_id, conversation_id, principal_kind, \
             principal_id, followed_at, source) VALUES (?, ?, 'human', ?, 2, 'manual')",
    )
    .bind(root)
    .bind(conversation)
    .bind(viewer)
    .execute(&mut db)
    .await
    .unwrap();
    db.close().await.unwrap();
}

/// Advances a reader's PER-THREAD cursor past everything in a conversation — the
/// clearing a main-line `mark_all_read` cannot express, since the two cursors are
/// independent rows.
async fn mark_thread_read(path: &std::path::Path, viewer: &str, conversation: &str, root: &str) {
    let mut db = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
        .await
        .unwrap();
    let latest: i64 =
        sqlx::query_scalar("SELECT COALESCE(MAX(seq), 0) FROM messages WHERE conversation_id = ?")
            .bind(conversation)
            .fetch_one(&mut db)
            .await
            .unwrap();
    sqlx::query(
        "INSERT INTO conversation_reads (user_id, conversation_id, thread_root_id, \
             last_read_seq, read_at) VALUES (?, ?, ?, ?, 2)",
    )
    .bind(viewer)
    .bind(conversation)
    .bind(root)
    .bind(latest)
    .execute(&mut db)
    .await
    .unwrap();
    db.close().await.unwrap();
}

#[tokio::test]
async fn a_workspace_send_fans_out_across_ports_without_duplicate_or_widening() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("fanout.sqlite");
    seed_fanout(&path).await;
    let store = MessagingStore::open(&path).await.unwrap();
    let outcome = store.send_message(fanout_command()).await.unwrap();

    // One committed send, one mention-CC copy, on a line that is NOT the channel.
    let copy_line = match outcome.committed().mention_copies() {
        [MentionCopyOutcome::Copied { copy, .. }] => copy.conversation_id.clone(),
        other => panic!("expected exactly one copied mention, got {other:?}"),
    };
    assert_ne!(copy_line.as_str(), "conversation-1");

    let hub = ScopeEventHub::test("epoch-1");
    let aftermath = hub.messaging_aftermath(
        OtherEffects::default(),
        store.clone(),
        SqliteUnreadReadModel::open(&path).await.unwrap(),
    );

    // The ONE production orchestration consumes the committed outcome. It runs the
    // copy layer and exactly one path layer (main here), so the assertions below
    // would expose either a missing layer or a second invocation as missing/duplicate
    // frames and durable mention rows.
    crate::conversation_messaging::messaging::aftermath::SendAftermath::announce(
        &aftermath, &outcome,
    )
    .await;

    // The channel carries EXACTLY ONE message frame: the copy did not widen onto it,
    // and no path published the message twice.
    let channel_frames = scope_events(&hub, EventScope::Conversation(channel()));
    assert_eq!(channel_frames.len(), 1);
    assert!(matches!(channel_frames[0], ProductEvent::Message(_)));

    // The copy rides its OWN line's conversation scope — the distinct audience a
    // copy-only viewer watches, never the channel's.
    let copy_frames = scope_events(&hub, EventScope::Conversation(copy_line));
    assert_eq!(copy_frames.len(), 1);
    assert!(matches!(copy_frames[0], ProductEvent::Message(_)));

    // The seated reader who was named: one mention_self notice, then one unread+badge
    // co-emit — never the notice twice, never a badge without its unread.
    let mut reader = scope_events(&hub, EventScope::Viewer(mentioned()));
    assert!(matches!(
        reader.pop(),
        Some(ProductEvent::ConversationLatestChanged(_))
    ));
    assert!(
        matches!(
            reader.as_slice(),
            [
                ProductEvent::Notification(notice),
                ProductEvent::UnreadUpdated(unread),
                ProductEvent::AttentionBadgeUpdated(badge),
            ] if notice.kind == NotificationKind::MentionSelf
                && unread.conversation_unread_count == 1
                && unread.workspace_unread_count == 1
                && unread.latest_unread.is_some()
                && badge.badge_count == 1
        ),
        "the named reader's scope was {reader:?}"
    );

    // The author's own scope: their line invalidation, their mention-left-the-room
    // notice, and their own co-emit — their own message is never their unread (zero),
    // yet the badge still rides beside it.
    let author = scope_events(&hub, EventScope::Viewer(user()));
    assert_eq!(
        author
            .iter()
            .filter(|event| matches!(event, ProductEvent::DirectPairsChanged { .. }))
            .count(),
        1
    );
    assert_eq!(
        author
            .iter()
            .filter(|event| matches!(
                event,
                ProductEvent::Notification(notice)
                    if notice.kind == NotificationKind::MentionExternal
            ))
            .count(),
        1
    );
    assert!(
        author.windows(2).any(|pair| matches!(
            pair,
            [ProductEvent::UnreadUpdated(unread), ProductEvent::AttentionBadgeUpdated(badge)]
                if unread.conversation_unread_count == 0 && badge.badge_count == 0
        )),
        "the author's unread+badge co-emit was not adjacent: {author:?}"
    );

    // No widening: a workspace member NOT seated in the channel is owed nothing. The
    // unread audience is the conversation's participants, not the workspace's roster.
    let bystander = scope_events(&hub, EventScope::Viewer(UserId::parse("user-3").unwrap()));
    assert!(
        bystander.is_empty(),
        "an unseated member received {bystander:?}"
    );
}

// ============ directed-notification-clear aftermath, on the REAL hub ============

/// A fake clear port that seals a committed witness with a fixed `marked` count —
/// the shape the SQLite authority returns (target result only, no badge value). It
/// lets the hub proof drive the ONE production `NotificationAftermath` over a real
/// transport without a database.
struct SealedClear {
    marked: u64,
}
impl NotificationClearPort for SealedClear {
    type Error = std::convert::Infallible;
    async fn clear(
        &self,
        viewer: UserId,
        _: NotificationClearSelector,
    ) -> Result<CommittedNotificationClear, Self::Error> {
        Ok(match std::num::NonZeroU64::new(self.marked) {
            Some(marked) => CommittedNotificationClear::changed(viewer, marked),
            None => CommittedNotificationClear::unchanged(viewer),
        })
    }
}

#[tokio::test]
async fn a_committed_notification_clear_emits_exactly_one_thin_refetch_signal() {
    let hub = ScopeEventHub::test("epoch-1");
    let viewer = UserId::parse("viewer-1").unwrap();
    let port = SealedClear { marked: 2 };

    // The ONE production aftermath, derived from THIS hub. No fake sink.
    let result = clear_notifications(
        &port,
        &hub.notification_aftermath(),
        viewer.clone(),
        NotificationClearSelector::All,
    )
    .await
    .unwrap();
    assert_eq!(result.committed().marked(), 2);
    assert!(result.first_failure().is_none());

    // The acting viewer's OWN scope carries EXACTLY one thin `unread_updated`
    // frame — a refetch signal. No conversation-derived co-emit, and crucially NO
    // `attention_badge_updated` frame at all: the notification-clear path publishes
    // no absolute badge value, so an out-of-order delivery cannot write back a
    // stale count.
    let frames = scope_events(&hub, EventScope::Viewer(viewer.clone()));
    assert!(
        matches!(
            frames.as_slice(),
            [ProductEvent::NotificationUnreadCleared { marked: 2 }]
        ),
        "the viewer scope carried {frames:?}"
    );
    assert!(
        !frames
            .iter()
            .any(|f| matches!(f, ProductEvent::AttentionBadgeUpdated(_))),
        "notification-clear must publish no absolute badge frame: {frames:?}"
    );

    // The exact wire JSON: kind `unread_updated`, payload `{marked}`, and NO
    // `conversation_id` (the renderer's sole discriminator between the enriched and
    // the thin refetch-signal variants).
    assert_eq!(
        serde_json::to_value(&frames[0]).unwrap(),
        json!({ "kind": "unread_updated", "payload": { "marked": 2 } })
    );

    // Confinement by SEPARATION: another viewer's scope is untouched.
    let other = scope_events(&hub, EventScope::Viewer(UserId::parse("viewer-2").unwrap()));
    assert!(other.is_empty(), "a foreign viewer received {other:?}");
}

#[tokio::test]
async fn a_zero_marked_notification_clear_emits_no_frames() {
    // Both the unchanged witness and a `marked == 0` change publish nothing: the
    // core guard on `marked > 0` means no unread signal is emitted and the viewer
    // scope stays empty.
    let hub = ScopeEventHub::test("epoch-1");
    let viewer = UserId::parse("viewer-1").unwrap();
    let port = SealedClear { marked: 0 };
    let result = clear_notifications(
        &port,
        &hub.notification_aftermath(),
        viewer.clone(),
        NotificationClearSelector::All,
    )
    .await
    .unwrap();
    assert_eq!(result.committed().marked(), 0);
    let frames = scope_events(&hub, EventScope::Viewer(viewer));
    assert!(frames.is_empty(), "a no-op clear emitted {frames:?}");
}

/// The concurrency falsifier. Two clears for the SAME viewer are
/// committed and their effects published in DELIBERATELY REVERSED order: the first
/// clear's thin event is emitted only AFTER the second clear's, exactly the
/// interleaving that would let an earlier commit's stale ABSOLUTE badge count win
/// under the client's last-write-wins consumer.
///
/// Because the notification-clear path publishes NO absolute badge value — only a
/// thin refetch signal — no frame on the viewer scope carries a badge count, in ANY
/// order. The client therefore re-reads the authoritative roll-up on whichever
/// signal arrives last and converges on the current committed state; there is no
/// stale value to write back. This test asserts the structural guarantee that makes
/// that safe: across a reversed-order emission, the replay holds only thin
/// `unread_updated` refetch signals and ZERO `attention_badge_updated` frames from
/// this path. The pre-fix implementation (which sealed an absolute count and emitted
/// a standalone badge) would fail this assertion.
#[tokio::test]
async fn out_of_order_notification_clears_publish_no_absolute_badge_to_reconcile() {
    let hub = ScopeEventHub::test("epoch-1");
    let viewer = UserId::parse("viewer-1").unwrap();

    // Clear A commits first (fact: a larger residual), clear B commits second (fact:
    // fewer). We then emit B's effect BEFORE A's — the reversed order.
    let clear_a = clear_notifications(
        &SealedClear { marked: 3 },
        &NoopAftermath, // A: commit only, effect deferred
        viewer.clone(),
        NotificationClearSelector::All,
    )
    .await
    .unwrap();
    let clear_b = clear_notifications(
        &SealedClear { marked: 1 },
        &NoopAftermath, // B: commit only, effect deferred
        viewer.clone(),
        NotificationClearSelector::All,
    )
    .await
    .unwrap();

    // Emit the effects in REVERSED commit order through the real production
    // aftermath: B (later commit) first, then A (earlier commit).
    let aftermath = hub.notification_aftermath();
    aftermath.unread_changed(clear_b.committed()).await.unwrap();
    aftermath.unread_changed(clear_a.committed()).await.unwrap();

    let frames = scope_events(&hub, EventScope::Viewer(viewer));
    // Two thin refetch signals landed (order does not matter — each just re-triggers
    // the authoritative read), and NOT ONE absolute badge frame exists for the
    // client to reconcile out of order.
    assert_eq!(
        frames
            .iter()
            .filter(|f| matches!(f, ProductEvent::NotificationUnreadCleared { .. }))
            .count(),
        2,
        "both clears' refetch signals must land: {frames:?}"
    );
    assert!(
        !frames
            .iter()
            .any(|f| matches!(f, ProductEvent::AttentionBadgeUpdated(_))),
        "no absolute badge frame may exist to be reconciled out of order: {frames:?}"
    );
}

/// A commit-only aftermath: it participates in `clear_notifications` (so the witness
/// is produced) but publishes nothing, letting the falsifier above sequence the real
/// hub effects by hand in a chosen order.
struct NoopAftermath;
impl NotificationAftermath for NoopAftermath {
    async fn unread_changed(
        &self,
        _: &CommittedNotificationClear,
    ) -> Result<(), NotificationClearFailure> {
        Ok(())
    }
}

#[tokio::test]
async fn workspace_unread_arrival_and_clearing_ride_one_co_emit_live_and_cold() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("workspace-unread.sqlite");
    seed_fanout(&path).await;
    let store = MessagingStore::open(&path).await.unwrap();
    // A main-line root the reader catches up on, then a reply they follow: the arrival
    // anchor is a THREADED row, so the co-emit must carry its `thread_root`, not merely
    // prove some anchor exists.
    let root = store.send_message(plain_command()).await.unwrap();
    let root_id = root.committed().message().id.clone();
    mark_all_read(&path, "user-2", "conversation-1").await;
    follow_thread(&path, "user-2", "conversation-1", root_id.as_str()).await;
    store.send_message(reply_command(&root_id)).await.unwrap();

    let hub = ScopeEventHub::test("epoch-1");
    let unread = HubUnreadPublisher::new(
        hub.clone(),
        SqliteUnreadReadModel::open(&path).await.unwrap(),
    );

    // Live BEFORE the publish: the arrival must be observed on the wire, not only in
    // replay.
    let mut live = hub
        .subscribe(EventScope::Viewer(mentioned()))
        .unwrap()
        .receiver;
    unread.publish_unread(&channel()).await.unwrap();

    let arrival_unread = live.recv().await.unwrap().event;
    let arrival_badge = live.recv().await.unwrap().event;
    assert!(
        matches!(
            arrival_unread,
            ProductEvent::UnreadUpdated(ref unread)
                if unread.conversation_unread_count == 1
                    && matches!(
                        unread.latest_unread,
                        Some(ref anchor) if anchor.thread_root_id.as_deref() == Some(root_id.as_str())
                    )
        ),
        "the arrival anchor must be the THREADED reply, carrying its thread_root: {arrival_unread:?}"
    );
    assert!(matches!(
        arrival_badge,
        ProductEvent::AttentionBadgeUpdated(ref badge) if badge.badge_count == 1
    ));
    // COLD authority: the live co-emit equals a fresh read-model computation over the
    // committed database, field for field — not the transport's own replay buffer.
    let cold_arrival = workspace_cold_viewer(&path, &channel(), &mentioned()).await;
    assert_eq!(cold_arrival.conversation_unread_count, 1);
    assert_co_emit_matches_cold(&arrival_unread, &arrival_badge, &cold_arrival);

    // The reader catches up on the FOLLOWED THREAD (a per-thread cursor, independent of
    // the main-line one), and the same conversation is republished.
    mark_thread_read(&path, "user-2", "conversation-1", root_id.as_str()).await;
    unread.publish_unread(&channel()).await.unwrap();

    let clearing_unread = live.recv().await.unwrap().event;
    let clearing_badge = live.recv().await.unwrap().event;
    // The SAME two frame kinds carry the clearing — a settled unread, not a withheld
    // one — and the anchor is dropped, never left stale.
    assert!(matches!(
        clearing_unread,
        ProductEvent::UnreadUpdated(ref unread)
            if unread.conversation_unread_count == 0 && unread.latest_unread.is_none()
    ));
    assert!(matches!(
        clearing_badge,
        ProductEvent::AttentionBadgeUpdated(ref badge) if badge.badge_count == 0
    ));
    // COLD authority again after the cursor moved: the cleared co-emit still equals a
    // fresh read-model computation, so clearing is settled state, not a lucky frame.
    let cold_cleared = workspace_cold_viewer(&path, &channel(), &mentioned()).await;
    assert_eq!(cold_cleared.conversation_unread_count, 0);
    assert_co_emit_matches_cold(&clearing_unread, &clearing_badge, &cold_cleared);

    // The retained transport window (replay, NOT a cold read) holds all four in order —
    // arrival then clearing, each a co-emit — so a resuming client catches up on the
    // whole ordered pair, never a lone half.
    let replay = scope_events(&hub, EventScope::Viewer(mentioned()));
    assert!(matches!(
        replay.as_slice(),
        [
            ProductEvent::UnreadUpdated(_),
            ProductEvent::AttentionBadgeUpdated(_),
            ProductEvent::UnreadUpdated(_),
            ProductEvent::AttentionBadgeUpdated(_),
        ]
    ));
}

/// A viewer's unread+badge, with a moved count, so a half-emit would be observable.
fn viewer_unread_of(viewer: &str, badge: i64) -> ViewerUnread {
    ViewerUnread {
        viewer: UserId::parse(viewer).unwrap(),
        conversation_unread_count: 1,
        workspace_unread_count: 1,
        latest_unread: None,
        badge_count: badge,
    }
}

/// The co-emit is BOTH-OR-NEITHER at the sequence ceiling, not first-wins.
///
/// This is the discriminating case the two-publish shape got wrong: seeded one below
/// the ceiling, a lone `publish` would land the unread at `MAX` and only the badge's
/// `checked_next` would fail — a persisted half-pair, exactly what the door swears a
/// client can never see. The batch reserves both sequence numbers before touching the
/// ring, so the second reservation overflowing leaves ZERO frames, and the door either
/// moves the unread and the badge together or moves neither.
#[test]
fn an_unread_co_emit_is_both_or_neither_at_the_sequence_ceiling() {
    let hub = ScopeEventHub::test("epoch-1");
    let workspace = WorkspaceId::parse("workspace-1").unwrap();
    let conversation = channel();
    let scope = EventScope::Viewer(mentioned());

    // One below the ceiling: a 2-event co-emit needs seq MAX and MAX+1; the second
    // overflows, so the whole co-emit must fail with no frame persisted.
    hub.scopes
        .lock()
        .unwrap()
        .entry(scope.clone())
        .or_insert_with(ScopeState::new)
        .seq = TransportSeq(MAX_TRANSPORT_SEQ - 1);
    assert!(matches!(
        hub.publish_unread_updated_to_viewer(
            &workspace,
            &conversation,
            &viewer_unread_of("user-2", 1),
        ),
        Err(ScopeEventHubError::SequenceExhausted)
    ));
    // Read the replay ring directly: a resume cannot describe zero frames (an empty ring
    // behind a non-zero seq is a SnapshotRequired gap), and zero frames is exactly the
    // claim — the exhausted co-emit persisted no lone unread.
    assert!(
        hub.scopes
            .lock()
            .unwrap()
            .get(&scope)
            .unwrap()
            .replay
            .is_empty(),
        "an exhausted co-emit must leave zero frames, never a lone unread"
    );

    // Two below the ceiling: MAX-1 and MAX both fit, so the co-emit lands whole.
    hub.scopes.lock().unwrap().get_mut(&scope).unwrap().seq = TransportSeq(MAX_TRANSPORT_SEQ - 2);
    hub.publish_unread_updated_to_viewer(&workspace, &conversation, &viewer_unread_of("user-2", 1))
        .unwrap();
    let landed: Vec<ProductEvent> = hub
        .scopes
        .lock()
        .unwrap()
        .get(&scope)
        .unwrap()
        .replay
        .iter()
        .map(|frame| frame.event.clone())
        .collect();
    assert!(matches!(
        landed.as_slice(),
        [
            ProductEvent::UnreadUpdated(_),
            ProductEvent::AttentionBadgeUpdated(_),
        ]
    ));
}

/// A workspace viewer's exhausted transport is local to that viewer. The read model
/// has already computed the complete audience, so the publisher retains the first
/// failure while still delivering the atomic unread+badge pair to later viewers.
#[tokio::test]
async fn a_workspace_viewer_failure_does_not_drop_the_remaining_real_audience() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("workspace-isolation.sqlite");
    seed_fanout(&path).await;
    let hub = ScopeEventHub::test("epoch-1");
    let failed_scope = EventScope::Viewer(user());
    hub.scopes
        .lock()
        .unwrap()
        .entry(failed_scope.clone())
        .or_insert_with(ScopeState::new)
        .seq = TransportSeq(MAX_TRANSPORT_SEQ);
    let publisher = HubUnreadPublisher::new(
        hub.clone(),
        SqliteUnreadReadModel::open(&path).await.unwrap(),
    );

    let failure = publisher
        .publish_unread(&channel())
        .await
        .expect_err("the first viewer's exhausted sequence must still surface");
    assert!(matches!(
        Error::source(&failure).and_then(|source| source.downcast_ref()),
        Some(ScopeEventHubError::SequenceExhausted)
    ));
    assert!(
        hub.scopes.lock().unwrap()[&failed_scope].replay.is_empty(),
        "the failed viewer must receive neither half of the co-emit"
    );
    assert!(matches!(
        scope_events(&hub, EventScope::Viewer(mentioned())).as_slice(),
        [
            ProductEvent::UnreadUpdated(_),
            ProductEvent::AttentionBadgeUpdated(_)
        ]
    ));
}

/// Direct-pair invalidation has the same audience isolation: one endpoint's
/// exhausted scope cannot suppress the hint owed to the other endpoint.
#[tokio::test]
async fn a_direct_pair_viewer_failure_does_not_drop_the_remaining_real_audience() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("direct-pair-isolation.sqlite");
    seed_direct_pair(&path).await;
    let hub = ScopeEventHub::test("epoch-1");
    let failed_scope = EventScope::Viewer(user());
    hub.scopes
        .lock()
        .unwrap()
        .entry(failed_scope.clone())
        .or_insert_with(ScopeState::new)
        .seq = TransportSeq(MAX_TRANSPORT_SEQ);
    let publisher = HubUnreadPublisher::new(
        hub.clone(),
        SqliteUnreadReadModel::open(&path).await.unwrap(),
    );

    let failure = publisher
        .publish_unread(&ConversationId::parse("conversation-2").unwrap())
        .await
        .expect_err("the first endpoint's exhausted sequence must still surface");
    assert!(matches!(
        Error::source(&failure).and_then(|source| source.downcast_ref()),
        Some(ScopeEventHubError::SequenceExhausted)
    ));
    assert!(hub.scopes.lock().unwrap()[&failed_scope].replay.is_empty());
    assert!(matches!(
        scope_events(&hub, EventScope::Viewer(mentioned())).as_slice(),
        [ProductEvent::DirectPairsChanged { conversation_id }]
            if conversation_id == "conversation-2"
    ));
}

#[tokio::test]
async fn a_direct_pair_unread_takes_the_distinct_axis_for_both_endpoints() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("direct-unread.sqlite");
    seed_direct_pair(&path).await;

    let hub = ScopeEventHub::test("epoch-1");
    let unread = HubUnreadPublisher::new(
        hub.clone(),
        SqliteUnreadReadModel::open(&path).await.unwrap(),
    );
    let line = ConversationId::parse("conversation-2").unwrap();

    let mut live_a = hub.subscribe(EventScope::Viewer(user())).unwrap().receiver;
    let mut live_b = hub
        .subscribe(EventScope::Viewer(mentioned()))
        .unwrap()
        .receiver;

    // A direct line has no workspace unread surface: each human endpoint's list is
    // invalidated instead — never an UnreadUpdated/AttentionBadge co-emit.
    unread.publish_unread(&line).await.unwrap();
    assert!(matches!(
        live_a.recv().await.unwrap().event,
        ProductEvent::DirectPairsChanged { .. }
    ));
    assert!(matches!(
        live_b.recv().await.unwrap().event,
        ProductEvent::DirectPairsChanged { .. }
    ));

    // The same carrier rides the other direction, so arrival and clearing are symmetric
    // on this axis too.
    unread.publish_unread(&line).await.unwrap();
    assert!(matches!(
        live_a.recv().await.unwrap().event,
        ProductEvent::DirectPairsChanged { .. }
    ));
    assert!(matches!(
        live_b.recv().await.unwrap().event,
        ProductEvent::DirectPairsChanged { .. }
    ));

    // This gate proves ONLY the LIVE INVALIDATION CARRIER for a direct pair: both
    // endpoints are told their list is stale, and no workspace unread/badge frame ever
    // rides this placement. It is NOT a cold read — the retained window below is the
    // transport's own buffer, not a fresh direct-pair list computed off the committed
    // database. There is no Rust direct-pair cold face in this tree (no list read model,
    // no `GET /api/direct-pairs`), so cold/live PARITY for the DM surface is out of scope
    // here and is delivered by #task:2187, which consumes this ticket's reviewed hash.
    for scope in [EventScope::Viewer(user()), EventScope::Viewer(mentioned())] {
        let events = scope_events(&hub, scope);
        assert_eq!(events.len(), 2);
        assert!(
            events
                .iter()
                .all(|event| matches!(event, ProductEvent::DirectPairsChanged { .. }))
        );
    }
}

/// The `user_changed` identity hint, over a REAL subscription: it rides the
/// global `app` scope and carries only `{user_id}`, exactly as the TypeScript
/// `publishToScope('app', 'user_changed', { user_id })`. Driven through the
/// production `UserChangedSink` adapter, not the raw producer, so the adapter's
/// own routing is what this pins.
#[tokio::test]
async fn user_changed_rides_the_app_scope_with_the_id_only_payload() {
    let hub = ScopeEventHub::test("epoch-1");
    let mut app = hub.subscribe(EventScope::App).unwrap().receiver;

    UserChangedSink::user_changed(&hub.user_changed_sink(), &user())
        .await
        .unwrap();

    let frame = app.recv().await.unwrap();
    assert_eq!(frame.scope, "app");
    assert_eq!(
        serde_json::to_value(&frame.event).unwrap(),
        json!({ "kind": "user_changed", "payload": { "user_id": "user-1" } })
    );
}

/// Factories from distinct capability owners must retain the hub they were derived
/// from. Their frames share that hub's sequence and routing state in call order; a
/// second hub stays untouched. Because every adapter constructor is crate-private,
/// this is also the composition proof that public callers cannot splice a factory
/// onto another transport.
#[tokio::test]
async fn typed_factories_share_the_originating_hub_scope_and_order() {
    let hub = ScopeEventHub::test("epoch-1");
    let other = ScopeEventHub::test("epoch-2");
    let mut app = hub.subscribe(EventScope::App).unwrap().receiver;
    let mut viewer = hub.subscribe(EventScope::Viewer(user())).unwrap().receiver;

    UserChangedSink::user_changed(&hub.user_changed_sink(), &user())
        .await
        .unwrap();
    PresenceFactSink::publish(
        &hub.presence_publisher(),
        PresenceFact::Online { user_id: user() },
    )
    .unwrap();
    crate::application_orchestration::agent_create::DirectPairsChanged::direct_pairs_changed(
        &hub.direct_pairs_changed_sink(),
        &user(),
        &channel(),
    )
    .await
    .unwrap();

    let first = app.recv().await.unwrap();
    let second = app.recv().await.unwrap();
    let scoped = viewer.recv().await.unwrap();
    assert_eq!((first.seq, second.seq), (TransportSeq(1), TransportSeq(2)));
    assert!(matches!(first.event, ProductEvent::UserChanged { .. }));
    assert!(matches!(second.event, ProductEvent::UserPresence { .. }));
    assert_eq!(scoped.seq, TransportSeq(1));
    assert!(matches!(
        scoped.event,
        ProductEvent::DirectPairsChanged { .. }
    ));
    assert_eq!(
        other.current_seq(&EventScope::App).unwrap(),
        TransportSeq::ZERO
    );
    assert_eq!(
        other.current_seq(&EventScope::Viewer(user())).unwrap(),
        TransportSeq::ZERO
    );
}

/// The `agent_changed` identity hint (the SAME seam agent deletion reuses):
/// global `app` scope, `{agent_id}` only, mirroring the TypeScript
/// `publishToScope('app', 'agent_changed', { agent_id })`. Its own typed frame,
/// never a generic app-scope announcer folded together with `user_changed`.
#[tokio::test]
async fn agent_changed_rides_the_app_scope_with_the_id_only_payload() {
    let hub = ScopeEventHub::test("epoch-1");
    let mut app = hub.subscribe(EventScope::App).unwrap().receiver;

    hub.publish_agent_changed(&agent()).unwrap();

    let frame = app.recv().await.unwrap();
    assert_eq!(frame.scope, "app");
    assert_eq!(
        serde_json::to_value(&frame.event).unwrap(),
        json!({ "kind": "agent_changed", "payload": { "agent_id": "agent-1" } })
    );
}

/// The production [`HubAgentChangedSink`] — the tuning PATCH's post-commit
/// announce adapter — lands the exact `{agent_id}`-only `agent_changed` frame on a
/// REAL subscription when driven through the `backend-core` `AgentChangedSink`
/// trait it satisfies. This is the real-hub discrimination gate: it proves the
/// production sink is a live producer over the accepted transport, not a trait +
/// test fake, and that it reuses the ONE `agent_changed` frame rather than a
/// second event.
#[tokio::test]
async fn hub_agent_changed_sink_lands_the_agent_changed_frame_on_a_real_subscription() {
    use silverhand_backend_core::identity_access::agent_profile_write::AgentChangedSink as _;

    let hub = ScopeEventHub::test("epoch-1");
    let mut app = hub.subscribe(EventScope::App).unwrap().receiver;

    hub.agent_changed_sink()
        .agent_changed(&agent())
        .await
        .unwrap();

    let frame = app.recv().await.unwrap();
    assert_eq!(frame.scope, "app");
    assert_eq!(
        serde_json::to_value(&frame.event).unwrap(),
        json!({ "kind": "agent_changed", "payload": { "agent_id": "agent-1" } })
    );
}

/// committed computer cascade calls for each deleted agent — routes to the SAME
/// app-scope `agent_changed` frame as the raw producer. Per the ratified A/B decision
/// the terminal deletion reuses `agent_changed` rather than minting a distinct
/// `agent_deleted` wire event, so this proves the adapter names no new event and folds
/// nothing together: driven through the `AgentDeletedAnnouncement` trait it satisfies,
/// it lands the exact `{agent_id}`-only frame on a REAL subscription.
#[tokio::test]
async fn hub_agent_deleted_announcer_reuses_the_agent_changed_frame() {
    use crate::host_runtime::cascade_effects::AgentDeletedAnnouncement;

    let hub = ScopeEventHub::test("epoch-1");
    let mut app = hub.subscribe(EventScope::App).unwrap().receiver;

    hub.agent_deleted_announcer()
        .agent_deleted(&agent())
        .await
        .unwrap();

    let frame = app.recv().await.unwrap();
    assert_eq!(frame.scope, "app");
    assert_eq!(
        serde_json::to_value(&frame.event).unwrap(),
        json!({ "kind": "agent_changed", "payload": { "agent_id": "agent-1" } })
    );
}

/// The `metadata_changed` viewer hint rides the addressed viewer's OWN scope and
/// carries NO role — a rename moved the label, not the membership. Exactly the
/// TypeScript `publishToScope('viewer:<id>', 'viewer_workspaces_changed',
/// { change: 'metadata_changed', workspace_id })`. The absence of a `role` key is
/// the discriminator against the `added` arm and is asserted by exact-equality.
#[tokio::test]
async fn workspace_metadata_changed_rides_the_viewer_scope_without_a_role() {
    let hub = ScopeEventHub::test("epoch-1");
    let workspace = WorkspaceId::parse("ws-1").unwrap();
    let mut viewer = hub.subscribe(EventScope::Viewer(user())).unwrap().receiver;

    hub.publish_viewer_workspace_metadata_changed(&user(), &workspace)
        .unwrap();

    let frame = viewer.recv().await.unwrap();
    assert_eq!(frame.scope, "viewer:user-1");
    assert_eq!(
        serde_json::to_value(&frame.event).unwrap(),
        json!({
            "kind": "viewer_workspaces_changed",
            "payload": { "change": "metadata_changed", "workspace_id": "ws-1" },
        })
    );
}

/// The rename fan-out's PUBLICATION leg, over a REAL subscription: a committed
/// rename rides the conversation's OWN `message` frame, projected by the one wire
/// authority, exactly as the TypeScript `events.publish(channel.id, {kind:'message',
/// payload:{message}})`. It is never a thread reply, so the payload is `{message}`
/// alone — no `projections` sidecar. Driven through the production
/// `WorkspaceRenamePublicationPort` adapter so the port's routing (scope + kind) is
/// what this pins; the exact body is the same closed-key-set `rename_dto` the wire
/// module's own gate locks.
#[tokio::test]
async fn workspace_rename_publication_rides_the_conversation_message_frame() {
    use silverhand_backend_core::conversation_messaging::messaging::MessageSeq;
    use silverhand_backend_core::conversation_messaging::messaging::collaboration::workspace_rename_messaging::{
        CommittedWorkspaceRenameMessage, WorkspaceRenameMessageCreatedAt,
        WorkspaceRenameMessageDraft,
    };

    let hub = ScopeEventHub::test("epoch-1");
    let mut subscriber = hub
        .subscribe(EventScope::Conversation(conversation()))
        .unwrap()
        .receiver;

    let draft = WorkspaceRenameMessageDraft::for_names(
        WorkspaceId::parse("ws-1").unwrap(),
        conversation(),
        "Old Workspace",
        "New Workspace",
    );
    let bound = CommittedWorkspaceRenameMessage::committed(
        draft.clone(),
        MessageId::parse("msg-1").unwrap(),
        MessageSeq::try_from(7).unwrap(),
        WorkspaceRenameMessageCreatedAt::new(77).unwrap(),
    )
    .bind(&draft)
    .unwrap();

    // The publication scope is derived from the SEALED message
    // (`draft().conversation()`), not a separately passed conversation
    // coordinate. There is no second parameter that could splice the frame onto a
    // conversation other than the one the rename was committed for.
    WorkspaceRenamePublicationPort::publish(&hub.workspace_rename_publisher(), &bound)
        .await
        .unwrap();

    let frame = subscriber.recv().await.unwrap();
    assert_eq!(frame.scope, "conversation:pair-1");
    assert_eq!(
        serde_json::to_value(&frame.event).unwrap(),
        json!({
            "kind": "message",
            "payload": {
                "message": {
                    "id": "msg-1",
                    "conversation_id": "pair-1",
                    "from_kind": "system",
                    "from_agent_id": Value::Null,
                    "from_user_id": Value::Null,
                    "author": Value::Null,
                    "content": draft.content(),
                    "metadata": {
                        "subtype": "rename",
                        "old_name": "Old Workspace",
                        "new_name": "New Workspace",
                        "scope": "workspace",
                    },
                    "attachments": Value::Null,
                    "reactions": Value::Null,
                    "seq": 7,
                    "created_at": 77,
                    "thread_root_id": Value::Null,
                    "linked_task": Value::Null,
                }
            },
        })
    );
}

#[derive(Clone)]
struct StubHumanAudience(Vec<UserId>);

impl WorkspaceHumanAudienceReadPort for StubHumanAudience {
    async fn human_member_ids(
        &self,
        _workspace_id: &WorkspaceId,
    ) -> Result<Vec<UserId>, silverhand_backend_core::foundation::persistence::PersistenceFailure>
    {
        Ok(self.0.clone())
    }
}

#[tokio::test]
async fn metadata_changed_sink_fans_out_one_viewer_frame_per_human_member() {
    // The discriminator against a scope-addressed impl: the sink is handed ONE
    // workspace id and must emit an INDEPENDENT frame to each member's own viewer
    // scope, not a single workspace-scoped frame. Two members => two distinct
    // viewer subscriptions each see exactly one metadata_changed hint.
    let hub = ScopeEventHub::test("epoch-1");
    let workspace = WorkspaceId::parse("ws-1").unwrap();
    let member_a = UserId::parse("user-1").unwrap();
    let member_b = UserId::parse("user-2").unwrap();
    let mut sub_a = hub
        .subscribe(EventScope::Viewer(member_a.clone()))
        .unwrap()
        .receiver;
    let mut sub_b = hub
        .subscribe(EventScope::Viewer(member_b.clone()))
        .unwrap()
        .receiver;

    let publisher = HubWorkspaceMetadataPublisher::new(
        hub.clone(),
        StubHumanAudience(vec![member_a.clone(), member_b.clone()]),
    );
    publisher
        .workspace_metadata_changed(&workspace)
        .await
        .unwrap();

    let expected = json!({
        "kind": "viewer_workspaces_changed",
        "payload": { "change": "metadata_changed", "workspace_id": "ws-1" },
    });
    let frame_a = sub_a.recv().await.unwrap();
    assert_eq!(frame_a.scope, "viewer:user-1");
    assert_eq!(serde_json::to_value(&frame_a.event).unwrap(), expected);
    let frame_b = sub_b.recv().await.unwrap();
    assert_eq!(frame_b.scope, "viewer:user-2");
    assert_eq!(serde_json::to_value(&frame_b.event).unwrap(), expected);
}

#[tokio::test]
async fn metadata_changed_sink_owes_nothing_when_the_workspace_has_no_human_members() {
    let hub = ScopeEventHub::test("epoch-1");
    let publisher = HubWorkspaceMetadataPublisher::new(hub.clone(), StubHumanAudience(vec![]));
    // An empty audience is a true "owed to nobody" answer, not a failure.
    publisher
        .workspace_metadata_changed(&WorkspaceId::parse("ws-1").unwrap())
        .await
        .unwrap();
}

#[derive(Clone)]
struct StubComputerAgents(Vec<AgentId>);

impl ComputerAgentsReadPort for StubComputerAgents {
    async fn agent_ids_on_computer(
        &self,
        _computer: &HostId,
    ) -> Result<Vec<AgentId>, PersistenceFailure> {
        Ok(self.0.clone())
    }
}

#[derive(Clone)]
struct StubAgentView(
    std::collections::HashMap<
        String,
        silverhand_backend_core::host_runtime::agent_availability_read::AgentViewFacts,
    >,
);

impl AgentAvailabilityReadModel for StubAgentView {
    async fn read_agent_view(
        &self,
        agent: &AgentId,
    ) -> Result<
        Option<silverhand_backend_core::host_runtime::agent_availability_read::AgentViewFacts>,
        PersistenceFailure,
    > {
        Ok(self.0.get(agent.as_str()).cloned())
    }

    async fn read_host_reachability(
        &self,
        _host: &HostId,
    ) -> Result<
        Option<silverhand_backend_core::host_runtime::availability::HostReachabilityFacts>,
        PersistenceFailure,
    > {
        Ok(None)
    }
}

/// An agent-view fact bundle for `source` on `host-1` whose ACL admits the
/// given viewers through the three arms, at the given materialization stage.
fn availability_view(
    source: &str,
    creator: Option<&str>,
    direct_pairs: &[&str],
    workspaces: &[&str],
    status: silverhand_backend_core::identity_access::agent_lifecycle::MaterializationStatus,
) -> silverhand_backend_core::host_runtime::agent_availability_read::AgentViewFacts {
    use silverhand_backend_core::identity_access::capability::agent_view::{
        AgentViewerArms, AgentVisibility,
    };
    silverhand_backend_core::host_runtime::agent_availability_read::AgentViewFacts::from_source(
        AgentId::parse(source).unwrap(),
        AgentVisibility {
            viewers: AgentViewerArms::new(
                creator.map(|c| UserId::parse(c).unwrap()),
                direct_pairs.iter().map(|u| UserId::parse(*u).unwrap()),
                workspaces.iter().map(|u| UserId::parse(*u).unwrap()),
            ),
            materialization_status: status,
        },
        HostId::parse("host-1").unwrap(),
        None,
    )
}

fn availability_agent_ids(frames: &[ScopedEvent]) -> Vec<String> {
    let mut ids: Vec<String> = frames
        .iter()
        .map(|frame| {
            let value = serde_json::to_value(&frame.event).unwrap();
            assert_eq!(value["kind"], "agent_availability_changed");
            value["payload"]["agent_id"].as_str().unwrap().to_owned()
        })
        .collect();
    ids.sort();
    ids
}

#[tokio::test]
async fn availability_announcer_fans_out_per_agent_to_each_agents_own_viewer_audience() {
    use silverhand_backend_core::identity_access::agent_lifecycle::MaterializationStatus;
    // Computer host-1 carries three agents. Two are active with different
    // audiences; the third is a non-active row that a viewer would still be
    // able to see if it were active. The announcer is handed ONLY the computer
    // id and must, per agent, reach exactly that agent's viewer audience (the
    // dual of the view ACL) with the active-only widener applied.
    let hub = ScopeEventHub::test("epoch-1");
    let viewer_1 = UserId::parse("viewer-1").unwrap();
    let viewer_2 = UserId::parse("viewer-2").unwrap();
    let mut sub_1 = hub
        .subscribe(EventScope::Viewer(viewer_1.clone()))
        .unwrap()
        .receiver;
    let mut sub_2 = hub
        .subscribe(EventScope::Viewer(viewer_2.clone()))
        .unwrap()
        .receiver;

    let mut views = std::collections::HashMap::new();
    // agent-a: active, seen by viewer-1 (creator) AND viewer-2 (workspace).
    views.insert(
        "agent-a".to_owned(),
        availability_view(
            "agent-a",
            Some("viewer-1"),
            &[],
            &["viewer-2"],
            MaterializationStatus::Active,
        ),
    );
    // agent-b: active, seen by viewer-1 (direct pair) only.
    views.insert(
        "agent-b".to_owned(),
        availability_view(
            "agent-b",
            None,
            &["viewer-1"],
            &[],
            MaterializationStatus::Active,
        ),
    );
    // agent-c: on the computer and would admit viewer-1, but NON-active — its
    // audience is empty, so no viewer is told. Falsifier for the active gate.
    views.insert(
        "agent-c".to_owned(),
        availability_view(
            "agent-c",
            Some("viewer-1"),
            &[],
            &[],
            MaterializationStatus::Failed,
        ),
    );

    let announcer = HubAgentAvailabilityAnnouncer::new(
        hub.clone(),
        StubComputerAgents(vec![
            AgentId::parse("agent-a").unwrap(),
            AgentId::parse("agent-b").unwrap(),
            AgentId::parse("agent-c").unwrap(),
        ]),
        StubAgentView(views),
    );
    announcer
        .agents_availability_changed(HostId::parse("host-1").unwrap())
        .await
        .unwrap();

    // viewer-1 sees agent-a and agent-b, NOT agent-c (non-active gate).
    let one = vec![sub_1.recv().await.unwrap(), sub_1.recv().await.unwrap()];
    assert_eq!(one[0].scope, "viewer:viewer-1");
    assert_eq!(availability_agent_ids(&one), vec!["agent-a", "agent-b"]);
    assert!(matches!(
        sub_1.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));

    // viewer-2 sees ONLY agent-a — not agent-b (outside its ACL) and not
    // agent-c (non-active). This is the viewer-audience discriminator: the
    // fan-out consults each agent's own admitted set, never a blanket blast.
    let two = vec![sub_2.recv().await.unwrap()];
    assert_eq!(two[0].scope, "viewer:viewer-2");
    assert_eq!(availability_agent_ids(&two), vec!["agent-a"]);
    assert!(matches!(
        sub_2.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
}

#[tokio::test]
async fn availability_view_resolved_for_the_wrong_agent_is_a_failure_that_reaches_no_viewer() {
    use silverhand_backend_core::identity_access::agent_lifecycle::MaterializationStatus;
    // The splice this guards against: the roster asks for agent-a, but the view
    // model resolves facts whose decoded `source()` is agent-b. Its ACL admits
    // viewer-99. Without the source==request bind, agent-a's `agent_id` payload
    // would be blasted to agent-b's viewer (viewer-99) — a leak AND a misdelivery
    // (viewer-99 is not agent-a's audience). The bind must instead: (1) send
    // viewer-99 ZERO frames, (2) still attempt the remaining roster (agent-c), and
    // (3) surface the mismatch as the aggregate production failure.
    let hub = ScopeEventHub::test("epoch-1");
    let viewer_99 = UserId::parse("viewer-99").unwrap();
    let viewer_1 = UserId::parse("viewer-1").unwrap();
    let mut sub_99 = hub
        .subscribe(EventScope::Viewer(viewer_99.clone()))
        .unwrap()
        .receiver;
    let mut sub_1 = hub
        .subscribe(EventScope::Viewer(viewer_1.clone()))
        .unwrap()
        .receiver;

    let mut views = std::collections::HashMap::new();
    // agent-a's lookup returns facts whose source is agent-b (the splice). agent-b's
    // ACL would admit viewer-99 — the very viewer we prove gets nothing.
    views.insert(
        "agent-a".to_owned(),
        availability_view(
            "agent-b",
            Some("viewer-99"),
            &[],
            &[],
            MaterializationStatus::Active,
        ),
    );
    // agent-c resolves honestly and admits viewer-1: it must still be reached even
    // though agent-a failed the bind earlier in the roster.
    views.insert(
        "agent-c".to_owned(),
        availability_view(
            "agent-c",
            Some("viewer-1"),
            &[],
            &[],
            MaterializationStatus::Active,
        ),
    );

    let announcer = HubAgentAvailabilityAnnouncer::new(
        hub.clone(),
        StubComputerAgents(vec![
            AgentId::parse("agent-a").unwrap(),
            AgentId::parse("agent-c").unwrap(),
        ]),
        StubAgentView(views),
    );

    // The mismatch is retained and returned as the aggregate production failure.
    let failure = announcer
        .agents_availability_changed(HostId::parse("host-1").unwrap())
        .await
        .unwrap_err();
    let source = std::error::Error::source(&failure).expect("a retained source cause");
    assert!(
        source.to_string().contains("resolved for the wrong agent"),
        "{source}"
    );

    // agent-c was still attempted: viewer-1 sees exactly agent-c.
    let one = sub_1.recv().await.unwrap();
    assert_eq!(one.scope, "viewer:viewer-1");
    assert_eq!(availability_agent_ids(&[one]), vec!["agent-c"]);
    assert!(matches!(
        sub_1.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));

    // The spliced agent-a reached NO viewer — viewer-99 (agent-b's audience) is
    // empty, so agent-a's payload never leaked onto another agent's viewers.
    assert!(matches!(
        sub_99.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
}

/// A person's plain send into an arbitrary conversation — enough to move a seated
/// reader's unread, whichever placement the channel has.
fn plain_send(conversation: ConversationId) -> SendMessage {
    ready(SendMessage::prepare(
        SendMessageDraft {
            source_conversation: conversation.clone(),
            target_conversation: conversation,
            actor: SendActor::User(user()),
            content: MessageContent::parse("hello").unwrap(),
            destination: SendDestination::MainLine,
            attachment_refs: Vec::new(),
            freshness_bypass: false,
        },
        &AllReferences,
        &DirectLinesToExternal,
    ))
    .unwrap()
}

/// One workspace holding a FIXED-roster and an OPEN-roster channel, each with its own
/// seated reader, and the workspace muted for BOTH readers. The mute is what makes the
/// two `loud_positional` arms discriminating: a fixed channel enters the badge on its
/// roster policy alone, an open one only by ambiently entering — which the mute blocks.
async fn seed_placement_matrix(path: &std::path::Path) {
    let mut db = current_schema::create_current_schema(path).await;
    for statement in [
        "INSERT INTO users (id, name, display_name, created_at, updated_at) \
             VALUES ('user-1', 'sender', 'Sender', 1, 1)",
        "INSERT INTO users (id, name, display_name, created_at, updated_at) \
             VALUES ('user-2', 'on-fixed', 'OnFixed', 2, 2)",
        "INSERT INTO users (id, name, display_name, created_at, updated_at) \
             VALUES ('user-3', 'on-open', 'OnOpen', 3, 3)",
        "INSERT INTO users (id, name, display_name, created_at, updated_at) \
             VALUES ('user-4', 'sees-public', 'SeesPublic', 4, 4)",
        "INSERT INTO workspaces (id, name, name_key, owner_user_id, created_at, updated_at) \
             VALUES ('workspace-1', 'Space', 'space', 'user-1', 1, 1)",
        "INSERT INTO workspace_users (workspace_id, user_id, joined_at, role) \
             VALUES ('workspace-1', 'user-1', 1, 'owner')",
        "INSERT INTO workspace_users (workspace_id, user_id, joined_at, role) \
             VALUES ('workspace-1', 'user-2', 1, 'member')",
        "INSERT INTO workspace_users (workspace_id, user_id, joined_at, role) \
             VALUES ('workspace-1', 'user-3', 1, 'member')",
        "INSERT INTO workspace_users (workspace_id, user_id, joined_at, role) \
             VALUES ('workspace-1', 'user-4', 1, 'member')",
        "INSERT INTO conversations (id, workspace_id, placement_kind, roster_policy, title, \
             title_key, visibility, created_at, updated_at) VALUES \
             ('conversation-fixed', 'workspace-1', 'workspace', 'fixed', 'Fixed', 'fixed', 'private', 1, 1)",
        "INSERT INTO conversations (id, workspace_id, placement_kind, roster_policy, title, \
             title_key, visibility, created_at, updated_at) VALUES \
             ('conversation-open', 'workspace-1', 'workspace', 'open', 'Open', 'open', 'private', 1, 1)",
        // A PUBLIC channel: visibility 'public' (open roster, as the schema requires).
        // user-4 is a workspace member who could read/subscribe by visibility, but is NOT
        // seated — the unread push audience must still exclude them.
        "INSERT INTO conversations (id, workspace_id, placement_kind, roster_policy, title, \
             title_key, visibility, created_at, updated_at) VALUES \
             ('conversation-public', 'workspace-1', 'workspace', 'open', 'Public', 'public', 'public', 1, 1)",
        "INSERT INTO conversation_users (conversation_id, user_id, joined_at) \
             VALUES ('conversation-fixed', 'user-1', 1)",
        "INSERT INTO conversation_users (conversation_id, user_id, joined_at) \
             VALUES ('conversation-fixed', 'user-2', 1)",
        "INSERT INTO conversation_users (conversation_id, user_id, joined_at) \
             VALUES ('conversation-open', 'user-1', 1)",
        "INSERT INTO conversation_users (conversation_id, user_id, joined_at) \
             VALUES ('conversation-open', 'user-3', 1)",
        "INSERT INTO conversation_users (conversation_id, user_id, joined_at) \
             VALUES ('conversation-public', 'user-1', 1)",
        "INSERT INTO workspace_attention_preferences (user_id, workspace_id, muted, updated_at) \
             VALUES ('user-2', 'workspace-1', 1, 1)",
        "INSERT INTO workspace_attention_preferences (user_id, workspace_id, muted, updated_at) \
             VALUES ('user-3', 'workspace-1', 1, 1)",
    ] {
        sqlx::query(statement).execute(&mut db).await.unwrap();
    }
    db.close().await.unwrap();
}

/// Seats or unseats a user in a conversation, so the participant set the audience is
/// computed from moves under a fresh read.
async fn set_conversation_membership(
    path: &std::path::Path,
    conversation: &str,
    user: &str,
    seated: bool,
) {
    let mut db = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
        .await
        .unwrap();
    let statement = if seated {
        "INSERT INTO conversation_users (conversation_id, user_id, joined_at) VALUES (?, ?, 1)"
    } else {
        "DELETE FROM conversation_users WHERE conversation_id = ? AND user_id = ?"
    };
    sqlx::query(statement)
        .bind(conversation)
        .bind(user)
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
}

/// The sorted viewer ids of a workspace audience — the set that decides who is owed an
/// unread update.
fn workspace_audience_ids(audience: &UnreadAudience) -> Vec<String> {
    let UnreadAudience::Workspace { viewers, .. } = audience else {
        panic!("a workspace conversation must yield a workspace audience: {audience:?}");
    };
    let mut ids: Vec<String> = viewers
        .iter()
        .map(|viewer| viewer.viewer.as_str().to_owned())
        .collect();
    ids.sort();
    ids
}

/// The unread count is placement-independent, but the BADGE enters on the arm the
/// roster policy names: a fixed channel positionally regardless of a workspace mute, an
/// open one only when it ambiently enters — which the mute forecloses.
///
/// Same committed unread, opposite badge: the discriminating case that proves the two
/// arms of `loud_positional` are genuinely distinct rather than one arm doing all the
/// work. Read cold off the committed database — the read model is the badge authority.
#[tokio::test]
async fn the_unread_badge_enters_on_the_arm_the_roster_policy_names() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("placement-matrix.sqlite");
    seed_placement_matrix(&path).await;
    let store = MessagingStore::open(&path).await.unwrap();
    let fixed = ConversationId::parse("conversation-fixed").unwrap();
    let open = ConversationId::parse("conversation-open").unwrap();
    store.send_message(plain_send(fixed.clone())).await.unwrap();
    store.send_message(plain_send(open.clone())).await.unwrap();

    // Each reader has exactly one unread message — unread itself does not care about the
    // roster policy.
    let on_fixed = workspace_cold_viewer(&path, &fixed, &mentioned()).await;
    let on_open = workspace_cold_viewer(&path, &open, &UserId::parse("user-3").unwrap()).await;
    assert_eq!(on_fixed.conversation_unread_count, 1);
    assert_eq!(on_open.conversation_unread_count, 1);

    // The badge does. The muted fixed channel still enters; the muted open channel does
    // not — the whole point of the two arms.
    assert_eq!(
        on_fixed.badge_count, 1,
        "a fixed-roster channel must enter the badge despite the workspace mute"
    );
    assert_eq!(
        on_open.badge_count, 0,
        "a muted open channel must not enter the badge positionally"
    );
}

/// The unread audience is exactly the conversation's seated participants, and it tracks
/// membership: seat a workspace member and they join the audience, unseat a reader and
/// they leave it. A fresh read each time, because the audience is a read off the
/// committed roster, not a cached set.
#[tokio::test]
async fn the_unread_audience_expands_and_contracts_with_conversation_membership() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("membership.sqlite");
    // seed_fanout seats user-1 and user-2; user-3 is a workspace member but NOT seated.
    seed_fanout(&path).await;
    let store = MessagingStore::open(&path).await.unwrap();
    store.send_message(plain_command()).await.unwrap();

    let audience = |path: std::path::PathBuf| async move {
        SqliteUnreadReadModel::open(&path)
            .await
            .unwrap()
            .unread_audience(&channel())
            .await
            .unwrap()
            .expect("the conversation is present")
    };

    // Only the seated participants — the unseated workspace member is owed nothing.
    assert_eq!(
        workspace_audience_ids(&audience(path.clone()).await),
        ["user-1", "user-2"]
    );

    // Seat the bystander: the audience expands to include them.
    set_conversation_membership(&path, "conversation-1", "user-3", true).await;
    assert_eq!(
        workspace_audience_ids(&audience(path.clone()).await),
        ["user-1", "user-2", "user-3"]
    );

    // Unseat a reader: the audience contracts to drop them.
    set_conversation_membership(&path, "conversation-1", "user-2", false).await;
    assert_eq!(
        workspace_audience_ids(&audience(path.clone()).await),
        ["user-1", "user-3"]
    );
}

/// A PUBLIC conversation pushes unread only to its SEATS, never to everyone its
/// visibility lets read it.
///
/// This closes the `workspace/public/fixed/direct-pair` matrix the task requires:
/// user-4 is a workspace member of a PUBLIC channel, so visibility would authorize them
/// to cold-read or subscribe — a different axis, decided by the scope authority, not this
/// port. The unread PUSH audience is the seated roster alone: user-4 is owed nothing until
/// a real seat enters, and is dropped again when it leaves. Being able to see a channel is
/// not being owed an unread update for it.
#[tokio::test]
async fn a_public_conversation_pushes_unread_only_to_its_seats() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("public-audience.sqlite");
    seed_placement_matrix(&path).await;

    let public = ConversationId::parse("conversation-public").unwrap();
    let audience = |path: std::path::PathBuf, conversation: ConversationId| async move {
        SqliteUnreadReadModel::open(&path)
            .await
            .unwrap()
            .unread_audience(&conversation)
            .await
            .unwrap()
            .expect("the conversation is present")
    };

    // user-4 can SEE the public channel (workspace member + public visibility) yet is not
    // seated — the push audience is the seat, user-1, alone.
    assert_eq!(
        workspace_audience_ids(&audience(path.clone(), public.clone()).await),
        ["user-1"]
    );

    // A real seat enters: now user-4 is owed the update.
    set_conversation_membership(&path, "conversation-public", "user-4", true).await;
    assert_eq!(
        workspace_audience_ids(&audience(path.clone(), public.clone()).await),
        ["user-1", "user-4"]
    );

    // The seat leaves: back to the seated roster, though visibility never changed.
    set_conversation_membership(&path, "conversation-public", "user-4", false).await;
    assert_eq!(
        workspace_audience_ids(&audience(path.clone(), public.clone()).await),
        ["user-1"]
    );
}

/// Poisons a hub's registry, so every subsequent publish path fails with
/// [`ScopeEventHubError::RegistryPoisoned`] — a deterministic driver-level failure with
/// no DB or timing dependency.
fn poison_registry(hub: &ScopeEventHub) {
    let registry = Arc::clone(&hub.scopes);
    let _ = std::thread::spawn(move || {
        let _guard = registry.lock().unwrap();
        panic!("poison the test registry");
    })
    .join();
}

/// A committed mention row, hand-built: the failure gate below cares only that the port
/// maps a hub failure to a source-preserving [`EffectFailure`], and that mapping does
/// not read the row's contents.
fn a_committed_mention_row() -> CommittedMentionRow {
    CommittedMentionRow::committed(
        NotificationId::parse("row-1").unwrap(),
        11,
        mentioned(),
        ParticipantHandle::parse("mentioned").unwrap(),
        MentionIntent::committed(
            vec![ParticipantHandle::parse("mentioned").unwrap()],
            Some(user()),
            "Sender".to_owned(),
            WorkspaceId::parse("workspace-1").unwrap(),
            channel(),
            "Channel".to_owned(),
            MessageId::parse("source-message").unwrap(),
            "ping".to_owned(),
        ),
    )
}

/// A read model that hands back a fixed audience without touching a database — lets the
/// batch co-emit be driven against a poisoned hub with a real workspace audience.
struct StubWorkspaceAudience(UnreadAudience);

impl UnreadReadModel for StubWorkspaceAudience {
    async fn unread_audience(
        &self,
        _conversation: &ConversationId,
    ) -> Result<Option<UnreadAudience>, PersistenceFailure> {
        Ok(Some(self.0.clone()))
    }
}

/// A read model that fails, carrying a distinctive driver cause the port must preserve.
struct FailingReadModel;

#[derive(Debug)]
struct ReadModelBoom;

impl fmt::Display for ReadModelBoom {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("read model boom")
    }
}

impl Error for ReadModelBoom {}

impl UnreadReadModel for FailingReadModel {
    async fn unread_audience(
        &self,
        _conversation: &ConversationId,
    ) -> Result<Option<UnreadAudience>, PersistenceFailure> {
        Err(PersistenceFailure::from_source(ReadModelBoom))
    }
}

/// Every direct-publish port surfaces a hub failure as an [`EffectFailure`] whose source
/// is the ORIGINAL [`ScopeEventHubError`] — never a new opaque error that has discarded
/// the cause a retry or telemetry seam needs.
///
/// Table-driven over all four ports off ONE real committed send, against ONE poisoned
/// hub: a `map_err` that flattened the source, or a port that swallowed the failure,
/// fails here for that port specifically.
#[tokio::test]
async fn a_hub_failure_reaches_every_direct_port_as_a_source_preserving_effect_failure() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("port-failures.sqlite");
    seed_fanout(&path).await;
    let store = MessagingStore::open(&path).await.unwrap();
    let outcome = store.send_message(fanout_command()).await.unwrap();
    let committed = outcome.committed();
    let copy = match committed.mention_copies() {
        [MentionCopyOutcome::Copied { copy, .. }] => copy,
        other => panic!("expected exactly one copied mention, got {other:?}"),
    };
    let view = committed.main_line().expect("a main-line send").published();
    let mention = a_committed_mention_row();

    let hub = ScopeEventHub::test("epoch-1");
    poison_registry(&hub);
    let publisher = hub.messaging_events();

    let failures: Vec<(&str, EffectFailure)> = vec![
        (
            "publish_copy",
            MessagePublication::publish_copy(&publisher, copy)
                .await
                .expect_err("a poisoned hub must fail the copy publication"),
        ),
        (
            "publish_message",
            CommittedMessagePublication::publish_message(&publisher, view)
                .await
                .expect_err("a poisoned hub must fail the message publication"),
        ),
        (
            "notify_author",
            CopyNotification::notify_author(&publisher, &user(), &agent(), copy)
                .await
                .expect_err("a poisoned hub must fail the author notice"),
        ),
        (
            "notify_viewer",
            ViewerNotification::notify_viewer(&publisher, &mention)
                .await
                .expect_err("a poisoned hub must fail the viewer notice"),
        ),
    ];

    for (port, failure) in &failures {
        let source = Error::source(failure)
            .unwrap_or_else(|| panic!("{port} dropped the failure source entirely"));
        assert!(
            matches!(
                source.downcast_ref::<ScopeEventHubError>(),
                Some(ScopeEventHubError::RegistryPoisoned)
            ),
            "{port} did not preserve the ScopeEventHubError source: {source}"
        );
    }
}

/// The unread port has TWO distinct failure origins, and both reach the effect chain as
/// a source-preserving [`EffectFailure`]: the read model itself failing, and the batch
/// co-emit hitting a hub failure after the read model succeeded.
///
/// A NARROW MAPPING UNIT over stubs: it pins that `HubUnreadPublisher`'s own `map_err`
/// preserves whichever source failed, holding the read model and the hub each still in
/// turn. It does NOT stand in for the real adapter's failure modes — those are proven end
/// to end against `SqliteUnreadReadModel` in
/// `the_real_read_model_carries_its_driver_and_decode_failures_through_the_unread_port`.
#[tokio::test]
async fn the_unread_port_surfaces_both_its_failure_origins_with_the_source_intact() {
    // (a) The read model fails: its PersistenceFailure is the preserved source, and its
    // own driver cause survives one level deeper.
    let publisher = HubUnreadPublisher::new(ScopeEventHub::test("epoch-1"), FailingReadModel);
    let failure = publisher
        .publish_unread(&channel())
        .await
        .expect_err("the read-model failure must surface");
    let source = Error::source(&failure).expect("the read-model failure kept its source");
    assert!(
        source.downcast_ref::<PersistenceFailure>().is_some(),
        "the read-model source was not a PersistenceFailure: {source}"
    );
    let inner = Error::source(source).expect("the PersistenceFailure kept its driver cause");
    assert!(
        inner.downcast_ref::<ReadModelBoom>().is_some(),
        "the read-model driver cause was flattened away: {inner}"
    );

    // (b) The read model SUCCEEDS with a real workspace audience, but the batch co-emit
    // hits a poisoned hub: the SAME EffectFailure seam now carries the hub's cause.
    let hub = ScopeEventHub::test("epoch-1");
    poison_registry(&hub);
    let audience = UnreadAudience::Workspace {
        workspace: WorkspaceId::parse("workspace-1").unwrap(),
        viewers: vec![viewer_unread_of("user-2", 1)],
    };
    let publisher = HubUnreadPublisher::new(hub, StubWorkspaceAudience(audience));
    let failure = publisher
        .publish_unread(&channel())
        .await
        .expect_err("the poisoned batch must surface");
    let source = Error::source(&failure).expect("the hub failure kept its source");
    assert!(
        matches!(
            source.downcast_ref::<ScopeEventHubError>(),
            Some(ScopeEventHubError::RegistryPoisoned)
        ),
        "the hub cause was not preserved: {source}"
    );
}

/// The REAL SQLite read model carries both of its failure classes — a driver fault and a
/// decode fault — through `HubUnreadPublisher<SqliteUnreadReadModel>` to a
/// source-preserving [`EffectFailure`], and emits ZERO frames when it does.
///
/// The stub test above proves only the publisher's mapping; a fake can never fail the way
/// the driver does. Here the two classes the read model can actually hit are forced
/// against a fresh committed schema: (a) a dropped `messages` table — the driver cause,
/// surfacing as `UnreadReadModelError::Sql` with the underlying `sqlx::Error` one level
/// deeper; (b) a workspace-placed conversation whose stored `workspace_id` is present but
/// unusable (the empty string) — the decode cause, surfacing as
/// `UnreadReadModelError::MalformedIdentity`. Both
/// fail during audience resolution, before any sequence is reserved, so no viewer scope
/// receives a frame.
#[tokio::test]
async fn the_real_read_model_carries_its_driver_and_decode_failures_through_the_unread_port() {
    // (a) DRIVER cause: the read model's count query hits a table that is gone.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("driver-failure.sqlite");
    seed_fanout(&path).await;
    let model = SqliteUnreadReadModel::open(&path).await.unwrap();
    drop_messages_table(&path).await;

    let hub = ScopeEventHub::test("epoch-1");
    // Subscribe the audience's scopes up front, so "zero frames" is a positive read of an
    // existing, empty scope rather than the absence of one.
    let _seated = hub.subscribe(EventScope::Viewer(user())).unwrap();
    let _named = hub.subscribe(EventScope::Viewer(mentioned())).unwrap();
    let publisher = HubUnreadPublisher::new(hub.clone(), model);

    let failure = publisher
        .publish_unread(&channel())
        .await
        .expect_err("a dropped table must fail audience resolution");
    let persistence =
        Error::source(&failure).expect("the driver failure kept its PersistenceFailure source");
    let read_error = Error::source(persistence)
        .expect("the PersistenceFailure kept the read-model error")
        .downcast_ref::<UnreadReadModelError>()
        .expect("the read-model error survived as its own type");
    assert!(
        matches!(read_error, UnreadReadModelError::Sql(_)),
        "the driver failure was not the read model's Sql variant: {read_error}"
    );
    assert!(
        Error::source(read_error).is_some(),
        "the underlying sqlx driver cause was flattened away"
    );
    assert!(
        scope_events(&hub, EventScope::Viewer(user())).is_empty()
            && scope_events(&hub, EventScope::Viewer(mentioned())).is_empty(),
        "a failed read model must publish no frame"
    );

    // (b) DECODE/placement cause: a workspace-placed conversation whose stored workspace
    // id is present-but-unusable (the empty string), which no identity parse accepts.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("decode-failure.sqlite");
    seed_workspace_conversation_with_malformed_workspace(&path).await;
    let model = SqliteUnreadReadModel::open(&path).await.unwrap();

    let hub = ScopeEventHub::test("epoch-1");
    let _seated = hub.subscribe(EventScope::Viewer(user())).unwrap();
    let _named = hub.subscribe(EventScope::Viewer(mentioned())).unwrap();
    let publisher = HubUnreadPublisher::new(hub.clone(), model);

    let failure = publisher
        .publish_unread(&channel())
        .await
        .expect_err("a workspace-placed row with an unusable workspace id must fail");
    let persistence =
        Error::source(&failure).expect("the decode failure kept its PersistenceFailure source");
    let read_error = Error::source(persistence)
        .expect("the PersistenceFailure kept the read-model error")
        .downcast_ref::<UnreadReadModelError>()
        .expect("the read-model error survived as its own type");
    assert!(
        matches!(read_error, UnreadReadModelError::MalformedIdentity(_)),
        "the decode failure was not MalformedIdentity: {read_error}"
    );
    assert!(
        scope_events(&hub, EventScope::Viewer(user())).is_empty()
            && scope_events(&hub, EventScope::Viewer(mentioned())).is_empty(),
        "a malformed placement must publish no frame"
    );
}

/// Drops the `messages` table, so the read model's count query against it fails with a
/// driver error.
async fn drop_messages_table(path: &std::path::Path) {
    let mut db = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
        .await
        .unwrap();
    sqlx::query("DROP TABLE messages")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
}

/// Seeds a workspace-placed conversation whose stored `workspace_id` is the empty
/// string — schema-legal (the placement CHECK demands only `NOT NULL`, and the empty
/// string is not null), but no usable identity: `WorkspaceId::parse` rejects it, so the
/// read model must fail with `MalformedIdentity` rather than trust the row.
async fn seed_workspace_conversation_with_malformed_workspace(path: &std::path::Path) {
    let mut db = current_schema::create_current_schema(path).await;
    for statement in [
        "INSERT INTO users (id, name, display_name, created_at, updated_at) \
             VALUES ('user-1', 'sender', 'Sender', 1, 1)",
        "INSERT INTO workspaces (id, name, name_key, owner_user_id, created_at, updated_at) \
             VALUES ('', 'Orphan', 'orphan', 'user-1', 1, 1)",
        "INSERT INTO conversations (id, workspace_id, placement_kind, roster_policy, title, \
             title_key, visibility, created_at, updated_at) VALUES \
             ('conversation-1', '', 'workspace', 'fixed', 'Orphan', 'orphan', \
             'private', 1, 1)",
    ] {
        sqlx::query(statement).execute(&mut db).await.unwrap();
    }
    db.close().await.unwrap();
}

fn committed_attachment_change() -> AttachmentChangedFact {
    let viewer = UserId::parse("user-1").unwrap();
    let conversation = ConversationId::parse("conversation-1").unwrap();
    let attachment = AttachmentId::parse("attachment-1").unwrap();
    let content_type = AttachmentContentType::parse("text/plain").unwrap();
    let before = AttachmentRecord::projected(
        attachment.clone(),
        conversation.clone(),
        Some(conversation.clone()),
        AttachmentFilename::parse("note.txt").unwrap(),
        4,
        content_type.clone(),
        StoredBlob::new(
            StorageKind::Local,
            BlobStorageKey::parse("attachments/attachment-1/revision-1.txt").unwrap(),
        ),
        1,
        AttachmentCreator::Human(viewer.clone()),
        1,
    )
    .unwrap();
    let replacement = StoredBlob::new(
        StorageKind::Local,
        BlobStorageKey::parse("attachments/attachment-1/revision-2.txt").unwrap(),
    );
    let command = AttachmentContentUpdateCommand::new(
        viewer.clone(),
        attachment.clone(),
        content_type.clone(),
        5,
        replacement.clone(),
    )
    .unwrap();
    let after = AttachmentRecord::projected(
        attachment.clone(),
        conversation.clone(),
        Some(conversation.clone()),
        AttachmentFilename::parse("note.txt").unwrap(),
        5,
        content_type,
        replacement,
        2,
        AttachmentCreator::Human(viewer.clone()),
        1,
    )
    .unwrap();
    let authority =
        AttachmentWriteAuthority::workspace(conversation, viewer.clone(), viewer.clone()).unwrap();
    AttachmentContentUpdatedFact::committed(&command, &authority, before, after)
        .unwrap()
        .bind(&viewer, &attachment)
        .unwrap()
        .changed()
}

#[tokio::test]
async fn attachment_changed_is_exactly_one_cursor_inert_conversation_side_frame() {
    let hub = ScopeEventHub::test("epoch-1");
    let fact = committed_attachment_change();

    AttachmentChangedPublisher::publish(&hub.attachment_publisher(), &fact)
        .await
        .unwrap();

    let side = scope_events(
        &hub,
        EventScope::ConversationSide(fact.conversation().clone()),
    );
    assert_eq!(side.len(), 1);
    assert_eq!(
        serde_json::to_value(&side[0]).unwrap(),
        json!({
            "kind": "attachment_changed",
            "payload": {
                "attachment_id": "attachment-1",
                "content_revision": 2,
            },
        })
    );

    assert!(
        scope_events(&hub, EventScope::Conversation(fact.conversation().clone())).is_empty(),
        "an attachment invalidation must not advance the message scope"
    );
    assert!(scope_events(&hub, EventScope::Viewer(user())).is_empty());
    assert!(scope_events(&hub, EventScope::App).is_empty());
    assert!(
        scope_events(
            &hub,
            EventScope::ConversationSide(ConversationId::parse("conversation-foreign").unwrap())
        )
        .is_empty(),
        "a sealed fact must not leak to a foreign conversation"
    );
}

#[tokio::test]
async fn attachment_changed_exposes_the_concrete_hub_failure() {
    let hub = ScopeEventHub::test("epoch-1");
    poison_registry(&hub);

    let error = AttachmentChangedPublisher::publish(
        &hub.attachment_publisher(),
        &committed_attachment_change(),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, ScopeEventHubError::RegistryPoisoned));
}

fn committed_reaction_change() -> ReactionChangedFact {
    ReactionChangedFact::committed(
        MessageId::parse("message-1").unwrap(),
        ConversationId::parse("conversation-1").unwrap(),
        Some(MessageId::parse("thread-1").unwrap()),
        ReactionEmoji::parse("👍").unwrap(),
        ReactionChange::Add,
        Reactor::Human(UserId::parse("user-1").unwrap()),
        ReactionCount::new(2).unwrap(),
    )
}

#[tokio::test]
async fn reaction_aftermath_is_exactly_one_canonical_conversation_frame() {
    let hub = ScopeEventHub::test("epoch-1");
    let fact = committed_reaction_change();

    ReactionAftermath::announce(&hub.reaction_publisher(), &fact)
        .await
        .unwrap();

    let frames = scope_events(&hub, EventScope::Conversation(fact.conversation().clone()));
    assert_eq!(frames.len(), 1);
    assert_eq!(
        serde_json::to_value(&frames[0]).unwrap(),
        json!({
            "kind": "message_reaction_changed",
            "payload": {
                "message_id": "message-1",
                "conversation_id": "conversation-1",
                "thread_root_id": "thread-1",
                "emoji": "👍",
                "op": "add",
                "reactor_kind": "user",
                "reactor_id": "user-1",
                "count": 2,
            },
        })
    );
    assert!(
        scope_events(
            &hub,
            EventScope::ConversationSide(fact.conversation().clone())
        )
        .is_empty(),
        "reaction deltas belong to the canonical message stream, not the side channel"
    );
    assert!(scope_events(&hub, EventScope::Viewer(user())).is_empty());
    assert!(scope_events(&hub, EventScope::App).is_empty());
}

#[test]
fn converged_task_changed_is_exactly_one_canonical_conversation_frame() {
    let hub = ScopeEventHub::test("epoch-1");
    let conversation = conversation();
    let payload = json!({
        "kind": "edit",
        "conversation_id": conversation.as_str(),
        "task_id": "task-1",
        "title": "committed title",
    });

    hub.publish_task_changed_to_conversation(&conversation, payload.clone())
        .unwrap();

    let frames = scope_events(&hub, EventScope::Conversation(conversation.clone()));
    assert_eq!(frames.len(), 1);
    assert_eq!(
        serde_json::to_value(&frames[0]).unwrap(),
        json!({"kind":"task_changed","payload":payload})
    );
    assert!(
        scope_events(&hub, EventScope::ConversationSide(conversation)).is_empty(),
        "task_changed advances the canonical message stream, never the cursor-inert side channel"
    );
}

/// The closed set of product-event wire `kind`s this transport is allowed to emit.
/// A `&str` set precisely because `kind` is a string at the wire boundary: nothing but
/// an explicit enumeration can tell a renamed or newly-added kind from a supported one.
const SUPPORTED_EVENT_KINDS: &[&str] = &[
    "direct_pairs_changed",
    "host_connection_changed",
    "agent_activity",
    "agent_status",
    "agent_init_timeout",
    "compaction_stale",
    "agent_event",
    "conversation_activity",
    "agent_stream_activity",
    "agent_presence_changed",
    "agent_availability_changed",
    "agent_changed",
    "user_presence",
    "user_changed",
    "message",
    "workspace_channels_changed",
    "workspace_members_changed",
    "conversation_members_changed",
    "attention_pref_changed",
    "task_changed",
    "notification",
    "unread_updated",
    "attention_badge_updated",
    "viewer_workspaces_changed",
    "sidebar_sort_changed",
    "saved_changed",
    "message_reaction_changed",
    "attachment_changed",
    "conversation_latest_changed",
];

/// The wire `kind` a product event MUST serialize as. Exhaustive by construction: a new
/// [`ProductEvent`] variant fails to COMPILE here rather than reaching the wire
/// undocumented — the enforcement an open `_ =>` arm or a field probe cannot give.
fn wire_kind(event: &ProductEvent) -> &'static str {
    match event {
        ProductEvent::DirectPairsChanged { .. } => "direct_pairs_changed",
        ProductEvent::HostConnectionChanged { .. } => "host_connection_changed",
        ProductEvent::AgentActivity(_) => "agent_activity",
        ProductEvent::AgentStatus(_) => "agent_status",
        ProductEvent::AgentInitTimeout(_) => "agent_init_timeout",
        ProductEvent::CompactionStale(_) => "compaction_stale",
        ProductEvent::AgentEvent(_) => "agent_event",
        ProductEvent::ConversationActivity(_) => "conversation_activity",
        ProductEvent::AgentStreamActivity(_) => "agent_stream_activity",
        ProductEvent::AgentPresenceChanged { .. } => "agent_presence_changed",
        ProductEvent::AgentAvailabilityChanged { .. } => "agent_availability_changed",
        ProductEvent::AgentChanged { .. } => "agent_changed",
        ProductEvent::UserPresence { .. } => "user_presence",
        ProductEvent::UserChanged { .. } => "user_changed",
        ProductEvent::Message(_) => "message",
        ProductEvent::WorkspaceMembersChanged { .. } => "workspace_members_changed",
        ProductEvent::WorkspaceChannelsChanged { .. } => "workspace_channels_changed",
        ProductEvent::ConversationMembersChanged { .. } => "conversation_members_changed",
        ProductEvent::AttentionPrefChanged { .. } => "attention_pref_changed",
        ProductEvent::TaskChanged(_) => "task_changed",
        ProductEvent::Notification(_) => "notification",
        ProductEvent::UnreadUpdated(_) => "unread_updated",
        ProductEvent::NotificationUnreadCleared { .. } => "unread_updated",
        ProductEvent::NotificationDismissed { .. } => "unread_updated",
        ProductEvent::AttentionBadgeUpdated(_) => "attention_badge_updated",
        ProductEvent::ViewerWorkspacesChanged { .. } => "viewer_workspaces_changed",
        ProductEvent::SidebarSortChanged {} => "sidebar_sort_changed",
        ProductEvent::SavedChanged(_) => "saved_changed",
        ProductEvent::MessageReactionChanged(_) => "message_reaction_changed",
        ProductEvent::AttachmentChanged(_) => "attachment_changed",
        ProductEvent::ConversationLatestChanged(_) => "conversation_latest_changed",
        ProductEvent::Test { .. } => "test",
    }
}

/// The supported `kind` set is CLOSED and NAMED, and every member has a real producer.
///
/// Breaks three ways on drift, which is the whole point of naming the set rather than
/// probing a field: a renamed `#[serde(rename_all)]` tag fails the serialize assertion;
/// a NEW `ProductEvent` variant fails the exhaustive [`wire_kind`] match at compile time;
/// a kind dropped from [`SUPPORTED_EVENT_KINDS`] fails membership or the coverage count.
#[test]
fn the_supported_wire_kinds_are_a_closed_named_set() {
    let samples = [
        ProductEvent::DirectPairsChanged {
            conversation_id: "c-1".to_owned(),
        },
        ProductEvent::HostConnectionChanged {
            host_id: "host-1".to_owned(),
        },
        ProductEvent::AgentActivity(json!({ "message_id": "m-1" })),
        ProductEvent::AgentStatus(json!({ "status": "active" })),
        ProductEvent::AgentInitTimeout(json!({ "session_id": "s-1" })),
        ProductEvent::CompactionStale(json!({ "message_id": "m-1" })),
        ProductEvent::AgentEvent(json!({ "event": "tool_use" })),
        ProductEvent::ConversationActivity(json!({ "message_id": "m-1" })),
        ProductEvent::AgentStreamActivity(json!({ "id": 1, "agent_id": "a" })),
        ProductEvent::AgentPresenceChanged {
            agent_id: "agent-1".to_owned(),
        },
        ProductEvent::AgentAvailabilityChanged {
            agent_id: "agent-1".to_owned(),
        },
        ProductEvent::AgentChanged {
            agent_id: "agent-1".to_owned(),
        },
        ProductEvent::UserPresence {
            user_id: "user-1".to_owned(),
            presence: "offline",
            last_seen_at: Some(1_234),
        },
        ProductEvent::UserChanged {
            user_id: "user-1".to_owned(),
        },
        ProductEvent::Message(json!({ "message": { "id": "m-1" } })),
        ProductEvent::WorkspaceMembersChanged {
            workspace_id: "w-1".to_owned(),
        },
        ProductEvent::WorkspaceChannelsChanged {
            workspace_id: "w-1".to_owned(),
            channel_id: "c-1".to_owned(),
        },
        ProductEvent::ConversationMembersChanged {
            conversation_id: "c-1".to_owned(),
            workspace_id: "w-1".to_owned(),
        },
        ProductEvent::AttentionPrefChanged {
            workspace_id: "w-1".to_owned(),
            conversation_id: Some("c-1".to_owned()),
        },
        ProductEvent::TaskChanged(json!({ "task_id": "task-1" })),
        ProductEvent::Notification(NotificationEvent {
            kind: NotificationKind::MentionSelf,
            conversation_id: Some("c-1".to_owned()),
            message_id: Some("m-1".to_owned()),
        }),
        ProductEvent::UnreadUpdated(UnreadUpdated {
            conversation_id: "c-1".to_owned(),
            workspace_id: "w-1".to_owned(),
            conversation_unread_count: 1,
            workspace_unread_count: 1,
            latest_unread: None,
        }),
        // The directed-notification-clear badge variant shares the `unread_updated`
        // wire kind with the enriched frame above but carries a different, thinner
        // payload; the serde-tag assertion below proves both map to the same kind.
        ProductEvent::NotificationUnreadCleared { marked: 3 },
        ProductEvent::NotificationDismissed {
            dismissed_notification_ids: vec!["notification-1".to_owned()],
        },
        ProductEvent::AttentionBadgeUpdated(AttentionBadge {
            workspace_id: "w-1".to_owned(),
            badge_count: 1,
        }),
        ProductEvent::ViewerWorkspacesChanged {
            change: ViewerWorkspaceChange::Added,
            workspace_id: "w-1".to_owned(),
            role: Some(WorkspaceRole::Member.as_str().to_owned()),
        },
        ProductEvent::ViewerWorkspacesChanged {
            change: ViewerWorkspaceChange::MetadataChanged,
            workspace_id: "w-1".to_owned(),
            role: None,
        },
        ProductEvent::SidebarSortChanged {},
        // A representative saved-set toggle. Its inner `Value` is illustrative, exactly
        // as the other opaque-payload samples above are (`Message`, `AgentActivity`, …):
        // this gate pins the variant→`kind` mapping, the serde tag, and set membership,
        // NOT payload fidelity. `SavedChanged`'s payload is projected in production only
        // by `saved::changed_event`, and its exact `{message_id, action}` shape is pinned
        // against that sole authority over a real subscription by the backend-app test
        // `the_composition_publishes_saved_changed_to_the_writer_on_the_real_hub`. The
        // authority cannot be exercised here: `SavedChangeFact` is deliberately
        // un-constructable outside `backend-core` (private `committed`/`fact`).
        ProductEvent::SavedChanged(json!({ "message_id": "m-1", "action": "save" })),
        ProductEvent::MessageReactionChanged(json!({
            "message_id": "m-1",
            "conversation_id": "c-1",
            "thread_root_id": null,
            "emoji": "👍",
            "op": "add",
            "reactor_kind": "user",
            "reactor_id": "user-1",
            "count": 1
        })),
        ProductEvent::AttachmentChanged(
            json!({ "attachment_id": "attachment-1", "content_revision": 2 }),
        ),
        ProductEvent::ConversationLatestChanged(json!({ "conversation_id": "c-1" })),
    ];
    let mut produced = std::collections::BTreeSet::new();
    for event in &samples {
        let kind = wire_kind(event);
        assert!(
            SUPPORTED_EVENT_KINDS.contains(&kind),
            "{kind} is not in the closed supported set"
        );
        // The serialized tag must equal wire_kind — a serde rename drift is caught here.
        assert_eq!(
            serde_json::to_value(event).unwrap()["kind"].as_str(),
            Some(kind),
            "the serde tag drifted from wire_kind for {kind}"
        );
        produced.insert(kind);
    }
    // Every supported kind was produced by a sampled variant: the set carries no member
    // without a real producer.
    assert_eq!(produced.len(), SUPPORTED_EVENT_KINDS.len());
    for kind in SUPPORTED_EVENT_KINDS {
        assert!(produced.contains(kind), "{kind} has no producing variant");
    }
}

/// Every one of the four product frames crosses a REAL authenticated subscription as
/// exact JSON, and confinement is proven by SEPARATION, not by reading a self-declared
/// `scope` field.
///
/// Two independent authenticated sockets: one subscribes ONLY `viewer:self`, the other
/// ONLY `conversation:pair-1`. Frames are published interleaved across the two axes, and
/// each socket is read in order — the viewer socket must see exactly its three viewer
/// frames (notification, unread, badge) and the conversation socket exactly its two
/// message frames, each as a whole envelope. A single earlier test that subscribed BOTH
/// scopes on one socket could not tell a confined stream from a crossed one: that socket
/// was entitled to every frame regardless of routing, so checking the producer-written
/// `scope` field proved nothing. Here a leak reorders a socket's stream and the very next
/// whole-JSON equality fails — a positive observation, never a timeout.
#[tokio::test]
async fn each_authenticated_scope_receives_only_its_own_frames_as_exact_json() {
    let hub = ScopeEventHub::test("epoch-1");
    let origin = "http://127.0.0.1";
    let (app, controller) = router(
        hub.clone(),
        Sessions,
        Allows,
        Arc::new(NoopConnectionLifecycle),
        EventWebSocketConfig::test(origin),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    // TWO sockets, each subscribing exactly ONE scope — the separation that makes a
    // cross-stream leak observable rather than merely mislabelled.
    let mut viewer_socket = connect(address, origin).await;
    assert_eq!(next_json(&mut viewer_socket).await["type"], "authenticated");
    subscribe_scope(&mut viewer_socket, "viewer:self").await;

    let mut conversation_socket = connect(address, origin).await;
    assert_eq!(
        next_json(&mut conversation_socket).await["type"],
        "authenticated"
    );
    subscribe_scope(&mut conversation_socket, "conversation:pair-1").await;

    // Published through the real hub — the only producer — INTERLEAVED across the axes,
    // so a router that crossed streams would land a frame on the wrong socket.
    hub.publish(
        EventScope::Viewer(user()),
        ProductEvent::Notification(NotificationEvent {
            kind: NotificationKind::MentionSelf,
            conversation_id: Some("pair-1".to_owned()),
            message_id: Some("m-1".to_owned()),
        }),
    )
    .unwrap();
    hub.publish(
        EventScope::Conversation(conversation()),
        ProductEvent::Message(json!({ "message": { "id": "m-1" } })),
    )
    .unwrap();
    hub.publish(
        EventScope::Viewer(user()),
        ProductEvent::UnreadUpdated(UnreadUpdated {
            conversation_id: "pair-1".to_owned(),
            workspace_id: "workspace-1".to_owned(),
            conversation_unread_count: 2,
            workspace_unread_count: 5,
            latest_unread: Some(LatestUnread {
                conversation_id: "pair-1".to_owned(),
                message_id: "m-1".to_owned(),
                seq: 9,
                thread_root_id: None,
            }),
        }),
    )
    .unwrap();
    hub.publish(
        EventScope::Conversation(conversation()),
        ProductEvent::Message(json!({ "message": { "id": "m-2" } })),
    )
    .unwrap();
    hub.publish(
        EventScope::Viewer(user()),
        ProductEvent::AttentionBadgeUpdated(AttentionBadge {
            workspace_id: "workspace-1".to_owned(),
            badge_count: 7,
        }),
    )
    .unwrap();

    // The viewer socket sees ONLY its three viewer frames, in publish order. A leaked
    // conversation frame would appear here and fail the next equality.
    assert_eq!(
        next_json(&mut viewer_socket).await,
        json!({
            "type": "event", "scope": "viewer:user-1", "seq": 1, "epoch": "epoch-1",
            "kind": "notification",
            "payload": { "kind": "mention_self", "conversation_id": "pair-1", "message_id": "m-1" },
        })
    );
    assert_eq!(
        next_json(&mut viewer_socket).await,
        json!({
            "type": "event", "scope": "viewer:user-1", "seq": 2, "epoch": "epoch-1",
            "kind": "unread_updated",
            "payload": {
                "conversation_id": "pair-1", "workspace_id": "workspace-1",
                "conversation_unread_count": 2, "workspace_unread_count": 5,
                "latest_unread": {
                    "conversation_id": "pair-1", "message_id": "m-1", "seq": 9,
                    "thread_root_id": null,
                },
            },
        })
    );
    assert_eq!(
        next_json(&mut viewer_socket).await,
        json!({
            "type": "event", "scope": "viewer:user-1", "seq": 3, "epoch": "epoch-1",
            "kind": "attention_badge_updated",
            "payload": { "workspace_id": "workspace-1", "badge_count": 7 },
        })
    );

    // The conversation socket sees ONLY its two message frames, in publish order.
    assert_eq!(
        next_json(&mut conversation_socket).await,
        json!({
            "type": "event", "scope": "conversation:pair-1", "seq": 1, "epoch": "epoch-1",
            "kind": "message", "payload": { "message": { "id": "m-1" } },
        })
    );
    assert_eq!(
        next_json(&mut conversation_socket).await,
        json!({
            "type": "event", "scope": "conversation:pair-1", "seq": 2, "epoch": "epoch-1",
            "kind": "message", "payload": { "message": { "id": "m-2" } },
        })
    );

    controller.close().await;
    server.abort();
}

/// The enrollment announce, end to end: one committed membership must leave
/// BOTH a durable row a fresh connection can read AND a live
/// `viewer_workspaces_changed` frame on the enrolled viewer's own socket.
///
/// This is the dual evidence the seam owes. The two proofs are independent on
/// purpose: the durable row shows the write actually committed (not merely that
/// a frame was emitted), and the live frame shows the post-commit announce
/// reached the real transport for THIS viewer (not merely that a row exists).
/// A regression that emitted the frame without committing, or committed without
/// announcing, fails exactly one half.
///
/// Scope of the "authenticated" claim: the socket traverses the REAL perimeter
/// SEAM — the router's authenticate-then-authorize wiring, the real transport,
/// and the real hub as the enrollment's sink. But `Sessions`/`Allows` here are
/// test DOUBLES standing in for the session and scope authorities, so this pins
/// the seam and the frame path, NOT the real human-session authority. The
/// composed proof over the real `SqliteSessionStore` + `SqliteEventScopeAuthority`
/// is owed by the #2190 prerequisite, not claimed here.
///
/// A note on the cold half: the TS contract's *cold* evidence is a `['workspaces']`
/// list refetch, and there is no workspaces read model in this Rust tree yet —
/// that projection is the same #2190 lane. The durable committed row is the honest
/// cold evidence available here: it is what any future list read model would read.
#[tokio::test]
async fn one_enrollment_leaves_a_durable_row_and_a_live_frame_through_the_authenticated_perimeter_seam()
 {
    #[derive(Clone, Copy)]
    struct FixedClock(i64);
    impl silverhand_backend_core::identity_access::access::EnrollmentClock for FixedClock {
        fn now_unix_millis(&self) -> i64 {
            self.0
        }
    }

    const JOIN_MS: i64 = 1_700_000_000_555;

    // The socket authenticates as `user()` (= "user-1"); enroll THAT person, so
    // the viewer who gains the membership is the one listening on `viewer:self`.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("enroll.db");
    {
        let mut db = current_schema::create_current_schema(&path).await;
        for statement in [
            "INSERT INTO users (id, name, display_name, created_at, updated_at) \
                 VALUES ('admin-1', 'Admin', 'Admin', 1, 1)",
            "INSERT INTO users (id, name, display_name, created_at, updated_at) \
                 VALUES ('user-1', 'Invitee', 'Invitee', 1, 1)",
            "INSERT INTO workspaces (id, name, name_key, created_at, updated_at) \
                 VALUES ('workspace-1', 'Space', 'space', 1, 1)",
            "INSERT INTO workspace_users (workspace_id, user_id, joined_at, role) \
                 VALUES ('workspace-1', 'admin-1', 1, 'admin')",
        ] {
            sqlx::query(statement).execute(&mut db).await.unwrap();
        }
        db.close().await.unwrap();
    }

    let store = silverhand_backend_sqlite::identity_access::access::SqliteAccessStore::open(&path)
        .await
        .unwrap();

    let hub = ScopeEventHub::test("epoch-1");
    let origin = "http://127.0.0.1";
    let (app, controller) = router(
        hub.clone(),
        Sessions,
        Allows,
        Arc::new(NoopConnectionLifecycle),
        EventWebSocketConfig::test(origin),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let mut viewer_socket = connect(address, origin).await;
    assert_eq!(next_json(&mut viewer_socket).await["type"], "authenticated");
    subscribe_scope(&mut viewer_socket, "viewer:self").await;

    // The production use case, driven through the REAL store and the REAL hub as
    // its sink — the same path the mounted surface will call.
    let (_membership, announce) =
        silverhand_backend_core::identity_access::access::add_workspace_human_member(
            &store,
            &hub.workspace_membership_publisher(),
            &FixedClock(JOIN_MS),
            &silverhand_backend_core::foundation::identity::Principal::Human {
                user_id: UserId::parse("admin-1").unwrap(),
            },
            WorkspaceId::parse("workspace-1").unwrap(),
            user(),
        )
        .await
        .unwrap()
        .into_parts();
    assert!(
        matches!(
            announce,
            silverhand_backend_core::identity_access::access::ViewerAnnounce::Delivered
        ),
        "the hub accepted the announce"
    );

    // Evidence 1 — durable: a FRESH connection reads the committed row.
    let mut probe = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
        .await
        .unwrap();
    let row: Option<(i64, String)> = sqlx::query_as(
        "SELECT joined_at, role FROM workspace_users WHERE workspace_id = ? AND user_id = ?",
    )
    .bind("workspace-1")
    .bind("user-1")
    .fetch_optional(&mut probe)
    .await
    .unwrap();
    probe.close().await.unwrap();
    assert_eq!(
        row,
        Some((JOIN_MS, "member".to_owned())),
        "the enrollment committed at the clock join time and the default member role"
    );

    // Evidence 2 — live: the enrolled viewer's own socket receives the frame,
    // as exact JSON, on the viewer scope (not the workspace scope: the person is
    // not yet subscribed to the workspace they just gained).
    assert_eq!(
        next_json(&mut viewer_socket).await,
        json!({
            "type": "event", "scope": "viewer:user-1", "seq": 1, "epoch": "epoch-1",
            "kind": "viewer_workspaces_changed",
            "payload": { "change": "added", "workspace_id": "workspace-1", "role": "member" },
        })
    );

    controller.close().await;
    server.abort();
}

/// Subscribes one scope and consumes the `subscribed` acknowledgement.
async fn subscribe_scope<S>(socket: &mut tokio_tungstenite::WebSocketStream<S>, scope: &str)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    socket
        .send(ClientMessage::Text(
            serde_json::json!({ "type": "subscribe", "scope": scope })
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    assert_eq!(next_json(socket).await["type"], "subscribed");
}

async fn connect(
    address: std::net::SocketAddr,
    origin: &str,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    // The scope-transport tests here front the socket with the `Sessions` double,
    // which resolves any bearer to the same viewer, so the cookie value is
    // immaterial to them. The causal enrollment test drives a REAL session
    // authority and passes its seeded bearer through `connect_with_session`.
    connect_with_session(address, origin, "token").await
}

async fn connect_with_session(
    address: std::net::SocketAddr,
    origin: &str,
    session: &str,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let mut request = format!("ws://{address}{EVENT_WEBSOCKET_PATH}")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert(ORIGIN, origin.parse().unwrap());
    request.headers_mut().insert(
        COOKIE,
        format!("{EVENT_WS_SESSION_COOKIE}={session}")
            .parse()
            .unwrap(),
    );
    request
        .headers_mut()
        .insert(DESKTOP_API_EPOCH_HEADER, MINIMUM_CLIENT_API_EPOCH.into());
    connect_async(request).await.unwrap().0
}

async fn next_json<S>(socket: &mut tokio_tungstenite::WebSocketStream<S>) -> serde_json::Value
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let ClientMessage::Text(frame) = socket.next().await.unwrap().unwrap() else {
        panic!("expected a text frame")
    };
    serde_json::from_str(&frame).unwrap()
}

/// The enrollment→refetch contract, proven CAUSALLY end to end (#task:2190 P0-4).
///
/// A juxtaposition proof — a hand-written `INSERT workspace_users` next to a
/// hand-written `publish_viewer_added_to_workspace` — would stay green even if
/// the real enrollment use case never emitted at all. That is exactly the
/// regression this task must catch, so this test refuses that shortcut and
/// drives the PRODUCTION path instead: an admin's authenticated
/// `POST /api/workspaces/{id}/members` runs `add_workspace_human_member`, whose
/// single post-commit sink call is the ONLY producer of the hint. The SAME
/// `ScopeEventHub` is both the access route's sink and the WS perimeter's state,
/// so the frame the enrollment emits is the frame the target's live `viewer:self`
/// subscription receives — and the SAME served cold read (`GET /api/workspaces`)
/// is the authority that grows on refetch.
///
/// It goes RED if ANY link in the chain breaks:
///   * delete the sink call, or move it before the commit — no frame, so the
///     live socket assertion and the `current_seq == 1` assertion both fail;
///   * swap the real hub for a no-op sink — the frame never reaches the socket;
///   * misfire the idempotent re-enroll — the second POST would advance the
///     stream or add a second row, and both are asserted unchanged;
///   * let the frame's `(workspace_id, role)` disagree with the committed row
///     the cold read returns — the exact-JSON frame, the durable row, and the
///     newly-appearing cold-read id are cross-checked to be the same coordinate.
///
/// Composition notes. The enrollment and cold-read HTTP surfaces are driven
/// in-process with `oneshot` (this test tree has no HTTP client); only the WS
/// perimeter, which needs a real socket, is served on a real listener. All three
/// share one hub, so the causal chain still crosses the real transport. Every
/// principal on this test is REAL: the HTTP surfaces authenticate through a
/// `HumanSessionAuthenticator` over `SqliteSessionStore`, and the WS perimeter
/// resolves the target viewer through the SAME real session authority from the
/// seeded `EVENT_WS_SESSION_COOKIE` bearer — so deleting the
/// `SqliteSessionStore → WS principal` wiring turns this test RED rather than
/// leaving a fake cookie to stand in. WS scope authorization likewise runs
/// through the real `SqliteEventScopeAuthority`.
#[tokio::test]
async fn an_authenticated_enrollment_emits_the_hint_its_own_cold_read_then_honors() {
    assert_membership_hint_reaches_viewer(false).await;
}

#[tokio::test]
async fn workspace_creation_initial_members_receive_live_hint_and_cold_workspace() {
    assert_membership_hint_reaches_viewer(true).await;
}

async fn assert_membership_hint_reaches_viewer(create_workspace: bool) {
    use sqlx::Executor;

    #[derive(Clone, Copy)]
    struct AtClock(i64);
    impl silverhand_backend_core::identity_access::auth::SessionClock for AtClock {
        fn now_unix_millis(&self) -> i64 {
            self.0
        }
    }
    impl silverhand_backend_core::identity_access::access::EnrollmentClock for AtClock {
        fn now_unix_millis(&self) -> i64 {
            self.0
        }
    }

    impl silverhand_backend_core::identity_access::workspace_creation::WorkspaceProvisionClock
        for AtClock
    {
        fn now_ms(&self) -> i64 {
            self.0
        }
    }

    struct NoAgents;
    impl crate::identity_access::workspace_creation::WorkspaceCreationAftermath for NoAgents {
        async fn deliver(
            &self,
            _: &silverhand_backend_core::identity_access::workspace_creation::WorkspaceOnboardingTarget,
            _: &silverhand_backend_core::conversation_messaging::messaging::MessageId,
        ) -> Result<(), crate::identity_access::workspace_creation::OnboardingDeliverFailed>
        {
            panic!("human-only creation must not deliver to agents")
        }
        fn start(
            &self,
            _: &silverhand_backend_core::identity_access::workspace_creation::WorkspaceOnboardingTarget,
        ) {
            panic!("human-only creation must not start agents")
        }
    }

    /// Drives one authenticated request through a router in-process and decodes
    /// the JSON body (or `Null` for an empty one).
    async fn call(
        app: &Router,
        method: &str,
        uri: &str,
        bearer: &str,
        body: axum::body::Body,
    ) -> (StatusCode, Value) {
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method(method)
                    .uri(uri)
                    .header("authorization", format!("Bearer {bearer}"))
                    .header("content-type", "application/json")
                    .body(body)
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, value)
    }

    fn workspace_ids(envelope: &Value) -> Vec<String> {
        envelope["workspaces"]
            .as_array()
            .expect("the envelope carries a workspaces array")
            .iter()
            .map(|workspace| workspace["id"].as_str().unwrap().to_owned())
            .collect()
    }

    const NOW_MS: i64 = 10_000;
    const JOIN_MS: i64 = 1_700_000_000_555;
    // sha256 → URL_SAFE_NO_PAD of the raw bearers (the same hashing the session
    // authority applies); the admin actor and the target viewer are told apart
    // purely by which seeded session their token resolves to.
    const TARGET_TOKEN: &str = "valid-token";
    const TARGET_HASH: &str = "OXoqnFv14szsOMJZa2grsb0F_m5OzqbBDPQnVf8iVAM";
    const ADMIN_TOKEN: &str = "admin-token";
    const ADMIN_HASH: &str = "EKTHyfxSBtbzbcaUSoG7b0o8sOJQFK47EubD5ScSKSo";
    const SEED_SQL: &str =
        include_str!("../../../../../rust/test-fixtures/fixtures/workspace-cold-read/seed.sql");

    // The shared cold-read seed makes the target (user-1) a member of three
    // workspaces and NOT of `ws-foreign`. Add the enrolling admin, make it an
    // admin of `ws-foreign`, and seed the two live sessions (valid under NOW_MS:
    // last_used_at 9_500, TTL 1_000ms).
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("enroll-causal.sqlite");
    {
        let mut db = current_schema::create_current_schema(&path).await;
        db.execute(sqlx::raw_sql(SEED_SQL))
            .await
            .expect("shared cold-read seed must load against the current schema");
        for statement in [
            "INSERT INTO users (id, name, display_name, created_at, updated_at) \
                 VALUES ('admin-1', 'Admin', 'Admin', 1, 1)",
            "INSERT INTO workspace_users (workspace_id, user_id, joined_at, role) \
                 VALUES ('ws-foreign', 'admin-1', 1, 'admin')",
        ] {
            sqlx::query(statement).execute(&mut db).await.unwrap();
        }
        // The token hashes are dynamic data, so they ride bind parameters rather
        // than being interpolated into the SQL text.
        for (id, user, hash) in [
            ("session-target", "user-1", TARGET_HASH),
            ("session-admin", "admin-1", ADMIN_HASH),
        ] {
            sqlx::query(
                "INSERT INTO auth_sessions (id, user_id, token_hash, created_at, \
                     last_used_at, revoked_at, user_agent, ip_first_seen) \
                     VALUES (?, ?, ?, 1, 9500, NULL, NULL, NULL)",
            )
            .bind(id)
            .bind(user)
            .bind(hash)
            .execute(&mut db)
            .await
            .unwrap();
        }
        db.close().await.unwrap();
    }

    // One hub, shared as the enrollment sink AND the WS perimeter state.
    let hub = ScopeEventHub::test("epoch-1");
    let origin = "http://127.0.0.1";

    // The real session authority, shared by every principal on this test: the two
    // HTTP surfaces AND the WS perimeter resolve their caller through it.
    let session_store =
        silverhand_backend_sqlite::identity_access::auth::SqliteSessionStore::open(&path)
            .await
            .unwrap();
    let ttl =
        silverhand_backend_core::identity_access::auth::SessionTtl::from_millis(1_000).unwrap();
    let authenticator = || {
        silverhand_backend_core::identity_access::auth::HumanSessionAuthenticator::new(
            session_store.clone(),
            AtClock(NOW_MS),
            ttl,
        )
    };

    // The WS perimeter on a real listener: real scope authority, and its principal
    // comes from the SAME real session authority via the seeded cookie bearer.
    let scope_authority =
        silverhand_backend_sqlite::foundation::scope_event::SqliteEventScopeAuthority::open(&path)
            .await
            .unwrap();
    let (ws_app, controller) = router(
        hub.clone(),
        authenticator(),
        scope_authority,
        Arc::new(NoopConnectionLifecycle),
        EventWebSocketConfig::test(origin),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, ws_app).await.unwrap() });

    // The two HTTP surfaces, each behind the REAL human-session perimeter over the
    // seeded sessions. The access route's sink is the shared hub; the workspaces
    // route reads the same database's cold authority.
    let store = silverhand_backend_sqlite::identity_access::access::SqliteAccessStore::open(&path)
        .await
        .unwrap();
    let cold_read =
        silverhand_backend_sqlite::identity_access::workspace_cold_read::SqliteWorkspaceColdRead::open(&path)
            .await
            .unwrap();
    let enroll_app = crate::identity_access::auth::require_human_session(
        crate::identity_access::access::router(
            store.clone(),
            store,
            hub.workspace_membership_publisher(),
            AtClock(JOIN_MS),
        ),
        authenticator(),
        crate::identity_access::auth::PublicJsonUnauthorized,
    );
    let read_app = crate::identity_access::auth::require_human_session(
        crate::identity_access::workspaces::router(cold_read),
        authenticator(),
        crate::identity_access::auth::PublicJsonUnauthorized,
    );

    // The page-load authority BEFORE the change, read through the SAME served cold
    // endpoint the client would call: `ws-foreign` is absent.
    let (status, before) = call(
        &read_app,
        "GET",
        "/api/workspaces",
        TARGET_TOKEN,
        axum::body::Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let before_ids = workspace_ids(&before);
    assert_eq!(before_ids, ["ws-archived", "ws-regular", "ws-agent"]);

    // The target subscribes its own viewer stream on the real transport, its WS
    // principal resolved from the seeded target bearer by the real session
    // authority; its stream sits at the origin until something is emitted.
    let mut target_socket = connect_with_session(address, origin, TARGET_TOKEN).await;
    assert_eq!(next_json(&mut target_socket).await["type"], "authenticated");
    subscribe_scope(&mut target_socket, "viewer:self").await;
    let scope = EventScope::Viewer(user());
    assert_eq!(hub.current_seq(&scope).unwrap(), TransportSeq::ZERO);

    // Both initial membership and subsequent enrollment must use the same viewer hint.
    let workspace_id = if create_workspace {
        use crate::identity_access::workspace_creation::{
            WorkspaceCreateState, workspace_create_router,
        };
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                sqlx::sqlite::SqliteConnectOptions::new()
                    .filename(&path)
                    .foreign_keys(true),
            )
            .await
            .unwrap();
        use silverhand_backend_sqlite::identity_access::workspace_creation::SqliteWorkspaceCreation;

        let writer = SqliteWorkspaceCreation::from_pool(pool);
        let create_app = crate::identity_access::auth::require_human_session(
            workspace_create_router(WorkspaceCreateState {
                writer,
                entropy:
                    silverhand_backend_sqlite::conversation_messaging::messaging::HostEnvironment,
                clock: AtClock(JOIN_MS),
                aftermath: NoAgents,
                membership_events: hub.workspace_membership_publisher(),
            }),
            authenticator(),
            crate::identity_access::auth::PublicJsonUnauthorized,
        );
        let input = json!({
            "name": "InitialMembers",
            "member_user_ids": ["user-1", "user-1", "admin-1", "unknown-user"],
        });
        let (status, body) = call(
            &create_app,
            "POST",
            "/api/workspaces",
            ADMIN_TOKEN,
            axum::body::Body::from(input.to_string()),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(
            hub.current_seq(&EventScope::Viewer(UserId::parse("unknown-user").unwrap()))
                .unwrap(),
            TransportSeq::ZERO
        );
        let id = body["workspace"]["id"].as_str().unwrap().to_owned();
        assert_eq!(
            scope_events(&hub, EventScope::Viewer(UserId::parse("admin-1").unwrap())),
            vec![ProductEvent::ViewerWorkspacesChanged {
                change: ViewerWorkspaceChange::Added,
                workspace_id: id.clone(),
                role: Some("owner".into()),
            }]
        );
        // A rejected duplicate create cannot publish a phantom membership.
        let (status, _) = call(
            &create_app,
            "POST",
            "/api/workspaces",
            ADMIN_TOKEN,
            axum::body::Body::from(input.to_string()),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(hub.current_seq(&scope).unwrap(), TransportSeq(1));

        id
    } else {
        let (status, body) = call(
            &enroll_app,
            "POST",
            "/api/workspaces/ws-foreign/members",
            ADMIN_TOKEN,
            axum::body::Body::from(r#"{"kind":"human","user_id":"user-1"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        "ws-foreign".to_owned()
    };

    // EFFECT 1 — the hub's own high-water advanced by EXACTLY ONE, and the retained
    // replay is that single hint. This is checked FIRST because it is synchronous
    // with the (awaited) sink call: a deleted or no-op sink leaves the stream at the
    // origin and fails HERE, before the blocking socket read is ever reached.
    assert_eq!(hub.current_seq(&scope).unwrap(), TransportSeq(1));
    assert_eq!(
        scope_events(&hub, scope.clone()).as_slice(),
        [ProductEvent::ViewerWorkspacesChanged {
            change: ViewerWorkspaceChange::Added,
            workspace_id: workspace_id.clone(),
            role: Some("member".to_owned()),
        }]
    );

    // EFFECT 2 — the exact same hint crosses the REAL transport to the target's live
    // `viewer:self` subscription: seq 1, carrying only the coordinate + role, no list.
    assert_eq!(
        next_json(&mut target_socket).await,
        json!({
            "type": "event", "scope": "viewer:user-1", "seq": 1, "epoch": "epoch-1",
            "kind": "viewer_workspaces_changed",
            "payload": { "change": "added", "workspace_id": workspace_id, "role": "member" },
        })
    );

    // EFFECT 3 — the durable committed row, at the clock's join time and the default
    // member role. This is the coordinate the frame must agree with.
    let mut probe = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    let rows: Vec<(i64, String)> = sqlx::query_as(
        "SELECT joined_at, role FROM workspace_users WHERE workspace_id = ? AND user_id = ?",
    )
    .bind(&workspace_id)
    .bind("user-1")
    .fetch_all(&mut probe)
    .await
    .unwrap();
    assert_eq!(rows, [(JOIN_MS, "member".to_owned())]);

    // EFFECT 4 — the SAME served cold read now honors the hint: `ws-foreign`
    // (updated_at 500) leads the regular group, and the ONLY id it gained is exactly
    // the workspace the frame named. Frame coordinate == durable row == cold delta.
    let (status, after) = call(
        &read_app,
        "GET",
        "/api/workspaces",
        TARGET_TOKEN,
        axum::body::Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        workspace_ids(&after),
        [
            workspace_id.as_str(),
            "ws-archived",
            "ws-regular",
            "ws-agent"
        ]
    );
    let gained: Vec<String> = workspace_ids(&after)
        .into_iter()
        .filter(|id| !before_ids.contains(id))
        .collect();
    assert_eq!(gained, std::slice::from_ref(&workspace_id));

    // Idempotent re-enroll: a second identical POST still answers 201 but owes no
    // announce. The membership stays a single row and the viewer stream does NOT
    // advance — proven by the hub's high-water, never by waiting for a frame.
    let seq_before_reenroll = hub.current_seq(&scope).unwrap();
    let (status, _) = call(
        &enroll_app,
        "POST",
        &format!("/api/workspaces/{workspace_id}/members"),
        ADMIN_TOKEN,
        axum::body::Body::from(r#"{"kind":"human","user_id":"user-1"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let membership_rows: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM workspace_users WHERE workspace_id = ? AND user_id = ?",
    )
    .bind(&workspace_id)
    .bind("user-1")
    .fetch_one(&mut probe)
    .await
    .unwrap();
    assert_eq!(
        membership_rows.0, 1,
        "re-enroll must not add a second membership"
    );
    assert_eq!(
        hub.current_seq(&scope).unwrap(),
        seq_before_reenroll,
        "an idempotent re-enroll owes no announce, so the viewer stream must not advance"
    );
    probe.close().await.unwrap();

    controller.close().await;
    server.abort();
}

/// A deterministic authority boundary for a real socket: pause one selected
/// authorization before reading its live decision, without timing a DB/network race.
#[derive(Clone)]
struct RevocableScopeGate {
    allowed: Arc<std::sync::atomic::AtomicBool>,
    remaining: Arc<Mutex<Option<usize>>>,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}
impl AuthorizeEventScope for RevocableScopeGate {
    async fn authorize_event_scope(
        &self,
        _viewer: &UserId,
        scope: &EventScope,
    ) -> Result<bool, PersistenceFailure> {
        if scope != &EventScope::Agent(AgentId::parse("agent-revoked").unwrap()) {
            return Ok(true);
        }
        let pause = {
            let mut remaining = self.remaining.lock().unwrap();
            match *remaining {
                Some(0) => {
                    *remaining = None;
                    true
                }
                Some(n) => {
                    *remaining = Some(n - 1);
                    false
                }
                None => false,
            }
        };
        if pause {
            self.entered.notify_one();
            self.release.notified().await;
        }
        Ok(self.allowed.load(Ordering::SeqCst))
    }
}

#[tokio::test]
async fn scoped_forbidden_cancels_live_queue_and_mid_replay_without_harming_other_scope() {
    for during_replay in [false, true] {
        tokio::time::timeout(Duration::from_secs(5), async {
            let hub = ScopeEventHub::test("epoch-1");
            let scope = EventScope::Agent(AgentId::parse("agent-revoked").unwrap());
            let wire = scope_wire(&scope);
            let gate = RevocableScopeGate {
                allowed: Arc::new(std::sync::atomic::AtomicBool::new(true)),
                remaining: Arc::new(Mutex::new(None)),
                entered: Arc::new(tokio::sync::Notify::new()),
                release: Arc::new(tokio::sync::Notify::new()),
            };
            let origin = "http://127.0.0.1";
            let (app, controller) = router(hub.clone(), Sessions, gate.clone(), Arc::new(NoopConnectionLifecycle), EventWebSocketConfig::test(origin));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let mut socket = connect(address, origin).await;
            assert_eq!(next_json(&mut socket).await["type"], "authenticated");
            for selected in [&wire, "app"] {
                socket.send(ClientMessage::Text(json!({"type":"subscribe","scope":selected}).to_string().into())).await.unwrap();
                assert_eq!(next_json(&mut socket).await["scope"], selected);
            }
            if during_replay {
                for n in 0..3 {
                    hub.publish(scope.clone(), ProductEvent::Test { value: format!("history-{n}") }).unwrap();
                    assert_eq!(next_json(&mut socket).await["scope"], wire);
                }
                // Initial resume admission + first event pass; second replay event pauses.
                *gate.remaining.lock().unwrap() = Some(2);
                socket.send(ClientMessage::Text(json!({"type":"resume","scopes":{wire.clone():{"epoch":"epoch-1","lastSeq":0}}}).to_string().into())).await.unwrap();
                gate.entered.notified().await;
                assert_eq!(next_json(&mut socket).await["seq"], 1);
            } else {
                *gate.remaining.lock().unwrap() = Some(0);
                hub.publish(scope.clone(), ProductEvent::Test { value: "at-writer-boundary".into() }).unwrap();
                gate.entered.notified().await;
            }
            // While the sole writer is paused, old subscription work accumulates.
            for _ in 0..20 { hub.publish(scope.clone(), ProductEvent::Test { value: "queued-before-revocation".into() }).unwrap(); }
            tokio::task::yield_now().await;
            gate.allowed.store(false, Ordering::SeqCst);
            gate.release.notify_one();
            let denied = next_json(&mut socket).await;
            assert_eq!(denied["type"], "error");
            assert_eq!(denied["code"], "forbidden");
            assert_eq!(denied["scope"], wire);
            hub.publish(EventScope::App, ProductEvent::Test { value: "other-still-live".into() }).unwrap();
            assert_eq!(next_json(&mut socket).await["scope"], "app");
            // A fresh subscribe denial also carries the exact canonical scope.
            socket.send(ClientMessage::Text(json!({"type":"subscribe","scope":wire}).to_string().into())).await.unwrap();
            assert_eq!(next_json(&mut socket).await, denied);
            // Regaining permission cannot resurrect the canceled relay or its queue.
            gate.allowed.store(true, Ordering::SeqCst);
            hub.publish(scope.clone(), ProductEvent::Test { value: "no-implicit-resubscribe".into() }).unwrap();
            hub.publish(EventScope::App, ProductEvent::Test { value: "still-only-app".into() }).unwrap();
            assert_eq!(next_json(&mut socket).await["scope"], "app");
            socket.send(ClientMessage::Text(json!({"type":"subscribe","scope":wire}).to_string().into())).await.unwrap();
            assert_eq!(next_json(&mut socket).await["type"], "subscribed");
            hub.publish(scope.clone(), ProductEvent::Test { value: "explicit-new-subscription".into() }).unwrap();
            assert_eq!(next_json(&mut socket).await["payload"]["value"], "explicit-new-subscription");
            controller.close().await;
            server.abort();
        }).await.expect("bounded real socket refusal scenario");
    }
}

#[tokio::test]
async fn thread_reply_announces_unread_through_production_aftermath() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("thread-live.sqlite");
    seed_fanout(&path).await;
    let mut db = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
        .await
        .unwrap();
    sqlx::raw_sql("INSERT INTO workspace_members(workspace_id,agent_id,joined_at) VALUES('workspace-1','agent-1',1); INSERT INTO conversation_agents(conversation_id,agent_id,joined_at) VALUES('conversation-1','agent-1',1);").execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    let store = MessagingStore::open(&path).await.unwrap();
    let root = store.send_message(plain_command()).await.unwrap();
    let root_id = root.committed().message().id.clone();
    mark_all_read(&path, "user-1", "conversation-1").await;
    follow_thread(&path, "user-1", "conversation-1", root_id.as_str()).await;
    let hub = ScopeEventHub::test("epoch-1");
    let aftermath = hub.messaging_aftermath(
        OtherEffects::default(),
        store.clone(),
        SqliteUnreadReadModel::open(&path).await.unwrap(),
    );
    let origin = "http://127.0.0.1";
    let (app, controller) = router(
        hub.clone(),
        Sessions,
        silverhand_backend_sqlite::foundation::scope_event::SqliteEventScopeAuthority::open(&path)
            .await
            .unwrap(),
        Arc::new(NoopConnectionLifecycle),
        EventWebSocketConfig::test(origin),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut socket = connect(address, origin).await;
    assert_eq!(next_json(&mut socket).await["type"], "authenticated");
    subscribe_scope(&mut socket, "viewer:self").await;
    let cold = crate::conversation_messaging::messaging::collaboration::conversation_unread::router(
        store.conversation_unread(),
    );
    // No conversation/thread subscription is needed for the closed-thread badge.
    for (expected, actor) in [
        (1, SendActor::User(mentioned())),
        (2, SendActor::User(mentioned())),
        (1, SendActor::User(mentioned())),
        (2, SendActor::Agent(agent())),
    ] {
        if expected == 1 && !scope_events(&hub, EventScope::Viewer(user())).is_empty() {
            mark_thread_read(&path, "user-1", "conversation-1", root_id.as_str()).await;
        }
        let command = SendMessage::prepare(
            SendMessageDraft {
                source_conversation: channel(),
                target_conversation: channel(),
                actor,
                content: MessageContent::parse("fresh reply").unwrap(),
                destination: SendDestination::Reply(root_id.clone()),
                attachment_refs: Vec::new(),
                freshness_bypass: true,
            },
            &AllReferences,
            &DirectLinesToExternal,
        )
        .await
        .unwrap();
        let outcome = store.send_message(command).await.unwrap();
        crate::conversation_messaging::messaging::aftermath::SendAftermath::announce(
            &aftermath, &outcome,
        )
        .await;
        let frame = next_json(&mut socket).await;
        assert_eq!(frame["kind"], "unread_updated", "{frame}");
        assert_eq!(
            frame["payload"]["conversation_unread_count"], expected,
            "{frame}"
        );
        assert_eq!(
            next_json(&mut socket).await["kind"],
            "attention_badge_updated"
        );
        assert_eq!(
            next_json(&mut socket).await["kind"],
            "conversation_latest_changed"
        );
        let probe = silverhand_backend_core::conversation_messaging::messaging::collaboration::conversation_unread::ConversationUnreadReadPort::read(&store.conversation_unread(), &user(), &channel()).await;
        assert!(probe.is_ok(), "read failure: {probe:?}");
        let mut request = axum::http::Request::builder()
            .uri("/api/conversations/conversation-1/unread")
            .body(axum::body::Body::empty())
            .unwrap();
        request.extensions_mut().insert(user());
        let response = cold.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let page: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            page["threads"][root_id.as_str()]["unread_count"],
            expected,
            "{page}"
        );
        assert_eq!(page["followed_thread_unread"], true);
    }
    socket.close(None).await.unwrap();
    wait_until_drained(&controller).await;
    server.abort();
}

/// DM replies owe a viewer invalidation even when no thread content is open.
/// Waiting and a later mainline send must not be prerequisites for the cold unread fact.
#[tokio::test]
async fn agent_dm_thread_unread_is_immediate_before_wait_or_mainline_send() {
    for send_mainline in [false, true] {
        let (_directory, path) = direct_pair_database().await;
        let store = MessagingStore::open(&path).await.unwrap();
        let hub = ScopeEventHub::test("epoch-1");
        let pair = store
            .open_direct_pair(&hub.messaging_events(), pair_participants(), 10)
            .await
            .unwrap();
        let aftermath = hub.messaging_aftermath(
            OtherEffects::default(),
            store.clone(),
            SqliteUnreadReadModel::open(&path).await.unwrap(),
        );
        let make_command = |actor, destination| SendMessageDraft {
            source_conversation: pair.clone(),
            target_conversation: pair.clone(),
            actor,
            content: MessageContent::parse("isolated DM message").unwrap(),
            destination,
            attachment_refs: Vec::new(),
            freshness_bypass: true,
        };
        let root = store
            .send_message(
                SendMessage::prepare(
                    make_command(SendActor::User(user()), SendDestination::MainLine),
                    &AllReferences,
                    &DirectLinesToExternal,
                )
                .await
                .unwrap(),
            )
            .await
            .unwrap();
        let root_id = root.committed().message().id.clone();
        let origin = "http://127.0.0.1";
        let (app, controller) = router(
            hub.clone(),
            Sessions,
            silverhand_backend_sqlite::foundation::scope_event::SqliteEventScopeAuthority::open(
                &path,
            )
            .await
            .unwrap(),
            Arc::new(NoopConnectionLifecycle),
            EventWebSocketConfig::test(origin),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut socket = connect(address, origin).await;
        assert_eq!(next_json(&mut socket).await["type"], "authenticated");
        subscribe_scope(&mut socket, "viewer:self").await;
        let cold =
            crate::conversation_messaging::messaging::collaboration::conversation_unread::router(
                store.conversation_unread(),
            );
        let read = || async {
            let mut request = axum::http::Request::builder()
                .uri(format!("/api/conversations/{}/unread", pair.as_str()))
                .body(axum::body::Body::empty())
                .unwrap();
            request.extensions_mut().insert(user());
            let response = cold.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), axum::http::StatusCode::OK);
            serde_json::from_slice::<Value>(
                &axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap(),
            )
            .unwrap()
        };
        assert_eq!(read().await["threads"], json!({}));
        let reply = store
            .send_message(
                SendMessage::prepare(
                    make_command(
                        SendActor::Agent(agent()),
                        SendDestination::Reply(root_id.clone()),
                    ),
                    &AllReferences,
                    &DirectLinesToExternal,
                )
                .await
                .unwrap(),
            )
            .await
            .unwrap();
        crate::conversation_messaging::messaging::aftermath::SendAftermath::announce(
            &aftermath, &reply,
        )
        .await;
        let frame = next_json(&mut socket).await;
        assert_eq!(frame["kind"], "direct_pairs_changed", "{frame}");
        assert_eq!(frame["scope"], "viewer:user-1");
        assert_eq!(
            next_json(&mut socket).await["kind"],
            "conversation_latest_changed"
        );
        assert_eq!(frame["payload"], json!({"conversation_id":pair.as_str()}));
        let immediate = read().await;
        assert_eq!(
            immediate["threads"][root_id.as_str()]["unread_count"],
            1,
            "{immediate}"
        );
        if send_mainline {
            let sent = store
                .send_message(
                    SendMessage::prepare(
                        make_command(SendActor::User(user()), SendDestination::MainLine),
                        &AllReferences,
                        &DirectLinesToExternal,
                    )
                    .await
                    .unwrap(),
                )
                .await
                .unwrap();
            crate::conversation_messaging::messaging::aftermath::SendAftermath::announce(
                &aftermath, &sent,
            )
            .await;
            assert_eq!(next_json(&mut socket).await["kind"], "direct_pairs_changed");
            assert_eq!(
                next_json(&mut socket).await["kind"],
                "conversation_latest_changed"
            );
        } else {
            // A bounded idle comparison; no read/write/observe or synthetic refresh.
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(read().await, immediate);
        mark_thread_read(&path, "user-1", pair.as_str(), root_id.as_str()).await;
        assert_eq!(read().await["threads"], json!({}));
        let reply = store
            .send_message(
                SendMessage::prepare(
                    make_command(
                        SendActor::Agent(agent()),
                        SendDestination::Reply(root_id.clone()),
                    ),
                    &AllReferences,
                    &DirectLinesToExternal,
                )
                .await
                .unwrap(),
            )
            .await
            .unwrap();
        crate::conversation_messaging::messaging::aftermath::SendAftermath::announce(
            &aftermath, &reply,
        )
        .await;
        assert_eq!(next_json(&mut socket).await["kind"], "direct_pairs_changed");
        assert_eq!(
            next_json(&mut socket).await["kind"],
            "conversation_latest_changed"
        );
        assert_eq!(read().await["threads"][root_id.as_str()]["unread_count"], 1);
        let other_root = store
            .send_message(
                SendMessage::prepare(
                    make_command(SendActor::User(user()), SendDestination::MainLine),
                    &AllReferences,
                    &DirectLinesToExternal,
                )
                .await
                .unwrap(),
            )
            .await
            .unwrap();
        let other_id = other_root.committed().message().id.clone();
        let other_reply = store
            .send_message(
                SendMessage::prepare(
                    make_command(
                        SendActor::Agent(agent()),
                        SendDestination::Reply(other_id.clone()),
                    ),
                    &AllReferences,
                    &DirectLinesToExternal,
                )
                .await
                .unwrap(),
            )
            .await
            .unwrap();
        crate::conversation_messaging::messaging::aftermath::SendAftermath::announce(
            &aftermath,
            &other_reply,
        )
        .await;
        assert_eq!(next_json(&mut socket).await["kind"], "direct_pairs_changed");
        assert_eq!(
            next_json(&mut socket).await["kind"],
            "conversation_latest_changed"
        );
        let two_threads = read().await;
        assert_eq!(two_threads["threads"][root_id.as_str()]["unread_count"], 1);
        assert_eq!(two_threads["threads"][other_id.as_str()]["unread_count"], 1);
        assert_eq!(two_threads["mainline"]["unread_count"], 0);
        let mut request = axum::http::Request::builder()
            .uri(format!("/api/conversations/{}/unread", pair.as_str()))
            .body(axum::body::Body::empty())
            .unwrap();
        request
            .extensions_mut()
            .insert(UserId::parse("unrelated-user").unwrap());
        assert_eq!(
            cold.clone().oneshot(request).await.unwrap().status(),
            axum::http::StatusCode::FORBIDDEN
        );
        assert!(
            scope_events(
                &hub,
                EventScope::Viewer(UserId::parse("unrelated-user").unwrap())
            )
            .is_empty()
        );
        assert!(
            scope_events(&hub, EventScope::Viewer(user()))
                .iter()
                .all(|event| matches!(
                    event,
                    ProductEvent::DirectPairsChanged { .. }
                        | ProductEvent::ConversationLatestChanged(_)
                ))
        );
        socket.close(None).await.unwrap();
        wait_until_drained(&controller).await;
        server.abort();
    }
}

#[test]
fn retention_replay_holes_require_snapshot_without_resetting_scope_sequence() {
    let hub = ScopeEventHub::test("retention-epoch");
    let scope = EventScope::Agent(AgentId::parse("retention-agent").unwrap());
    hub.publish(
        scope.clone(),
        ProductEvent::AgentStreamActivity(json!({"id":1,"created_at":100})),
    )
    .unwrap();
    hub.publish(
        scope.clone(),
        ProductEvent::AgentStreamActivity(json!({"id":2,"created_at":99})),
    )
    .unwrap();
    hub.publish(
        scope.clone(),
        ProductEvent::AgentStreamActivity(json!({"id":3,"created_at":101})),
    )
    .unwrap();
    hub.prune_activity_before(100).unwrap();
    let epoch = TransportEpoch::parse("retention-epoch").unwrap();
    assert!(matches!(
        hub.resume(&scope, &epoch, TransportSeq(0)).unwrap().replay,
        Replay::SnapshotRequired {
            reason: "gap_too_large",
            ..
        }
    ));
    match hub.resume(&scope, &epoch, TransportSeq(2)).unwrap().replay {
        Replay::Events(events) => {
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].seq, TransportSeq(3));
        }
        _ => panic!("retained tail should resume"),
    }
    hub.prune_activity_before(102).unwrap();
    assert!(matches!(
        hub.resume(&scope, &epoch, TransportSeq(2)).unwrap().replay,
        Replay::SnapshotRequired { .. }
    ));
    assert!(
        matches!(hub.resume(&scope,&epoch,TransportSeq(3)).unwrap().replay, Replay::Events(events) if events.is_empty())
    );
    hub.publish(
        scope.clone(),
        ProductEvent::AgentStreamActivity(json!({"id":4,"created_at":103})),
    )
    .unwrap();
    assert_eq!(hub.current_seq(&scope).unwrap(), TransportSeq(4));
}

#[tokio::test]
async fn retention_expired_ws_cursor_requires_cold_read_and_retained_tail_resumes() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let hub = ScopeEventHub::test("epoch-1");
        let scope = EventScope::Agent(AgentId::parse("retention-agent").unwrap());
        let wire = scope_wire(&scope);
        hub.publish(
            scope.clone(),
            ProductEvent::AgentStreamActivity(
                json!({"id":1,"created_at":99,"payload":{"text":"expired"}}),
            ),
        )
        .unwrap();
        hub.publish(
            scope.clone(),
            ProductEvent::AgentStreamActivity(
                json!({"id":2,"created_at":100,"payload":{"text":"retained"}}),
            ),
        )
        .unwrap();
        hub.publish(
            EventScope::App,
            ProductEvent::Test {
                value: "other-history".into(),
            },
        )
        .unwrap();
        hub.prune_activity_before(100).unwrap();
        let origin = "http://127.0.0.1";
        let (app, controller) = router(
            hub.clone(),
            Sessions,
            Allows,
            Arc::new(NoopConnectionLifecycle),
            EventWebSocketConfig::test(origin),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut socket = connect(address, origin).await;
        assert_eq!(next_json(&mut socket).await["type"], "authenticated");
        socket
            .send(ClientMessage::Text(
                json!({"type":"resume","scopes":{wire.clone():{"epoch":"epoch-1","lastSeq":0}}})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        let frame = next_json(&mut socket).await;
        assert_eq!(frame["type"], "snapshot_required");
        assert_eq!(frame["scope"], wire);
        assert_eq!(frame["reason"], "gap_too_large");
        socket
            .send(ClientMessage::Text(
                json!({"type":"resume","scopes":{wire.clone():{"epoch":"epoch-1","lastSeq":1}}})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        let frame = next_json(&mut socket).await;
        assert_eq!(frame["payload"]["id"], 2);
        assert_eq!(frame["seq"], 2);
        socket
            .send(ClientMessage::Text(
                json!({"type":"resume","scopes":{"app":{"epoch":"epoch-1","lastSeq":0}}})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        assert_eq!(next_json(&mut socket).await["scope"], "app");
        hub.publish(
            scope,
            ProductEvent::AgentStreamActivity(json!({"id":3,"created_at":101})),
        )
        .unwrap();
        let live = next_json(&mut socket).await;
        assert_eq!(live["seq"], 3);
        assert_eq!(live["payload"]["id"], 3);
        controller.close().await;
        server.abort();
    })
    .await
    .unwrap();
}

#[test]
fn retention_covers_conversation_and_diagnostic_copies_but_not_chat_events() {
    let hub = ScopeEventHub::test("retention-epoch");
    let scope = EventScope::ConversationSide(conversation());
    hub.publish(
        scope.clone(),
        ProductEvent::ConversationActivity(json!({"created_at":1})),
    )
    .unwrap();
    hub.publish(
        scope.clone(),
        ProductEvent::AgentEvent(json!({"event":{"text":"diagnostic"}})),
    )
    .unwrap();
    hub.publish(
        scope.clone(),
        ProductEvent::Message(json!({"created_at":1,"content":"chat"})),
    )
    .unwrap();
    let mut scopes = hub.scopes.lock().unwrap();
    let state = scopes.get_mut(&scope).unwrap();
    state.replay[1].published_at = 2;
    assert!(
        serde_json::to_value(&state.replay[1])
            .unwrap()
            .get("published_at")
            .is_none()
    );
    drop(scopes);
    hub.prune_activity_before(3).unwrap();
    let scopes = hub.scopes.lock().unwrap();
    let state = scopes.get(&scope).unwrap();
    assert_eq!(state.replay.len(), 1);
    assert!(matches!(&state.replay[0].event, ProductEvent::Message(_)));
    assert_eq!(state.seq, TransportSeq(3));
}
