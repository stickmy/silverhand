use crate::current_schema;
use crate::log_capture;
use silverhand_backend_core::workspace_data_plane::WorkspaceDataPlane;
use silverhand_backend_server::workspace_data_plane::ProductionWorkspaceDataPlane;

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::{SinkExt, StreamExt};
use log_capture::LogBuffer;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use silverhand_backend_core::{
    application_orchestration::teardown::{
        computer_delete::ComputerCascadeEffects,
        facts::{AgentRuntimeSessionFacts, StoredBlobRef},
    },
    conversation_messaging::messaging::{
        AddressedPrincipals, CreatedMessage, DirectLineAuthority, FromKind, MessageContent,
        MessageId, MessageSeq, PrincipalKind, ReferencePositions, SendActor, SendDestination,
        SendMessage, SendMessageDraft, SendMessageUnitOfWork,
        collaboration::{
            BoundRuntimeControlSeat, ClearSessionCommitReceipt, ClearSessionProviderRefusal,
            ClearSessionRefusal, RuntimeControlAdmission, RuntimeControlAdmissionScope,
            RuntimeControlPort, RuntimeControlSeatPort, RuntimeControlSeatSnapshot,
            RuntimeSeatTarget, RuntimeStartTarget, clear_conversation_session,
            notification::{NotificationListQuery, NotificationReadPort},
        },
        post_commit::{IdleStart, MainDelivery},
    },
    conversation_messaging::runtime_delivery::{
        ClaimPendingInbox, DeliveryLeaseOwner, InboxDeliveryId, InboxSettlementDisposition,
        RuntimeDeliveryFailureReason, RuntimeDispatchReadPort, RuntimeDispatchTarget,
        RuntimeInboxCustodyPort,
    },
    foundation::blob_store::{BlobNamespace, BlobReadOutcome, BlobStore},
    foundation::identity::{AgentId, ConversationId, DaemonInstanceId, HostId, UserId},
    foundation::persistence::PersistenceFailure,
    host_runtime::aftermath::{CustodyEffectFailure, DaemonConnectionControl},
    host_runtime::runtime_credential::{
        ClaimRuntimeCustody, RuntimeCustodyPort, RuntimeEventTime, RuntimeGeneration,
        RuntimePrincipal, RuntimeScope,
    },
    host_runtime::runtime_event::{
        CommitRuntimeConsumed, CommitRuntimeExit, CommitRuntimeSessionInit, DaemonEventDurablePort,
        ProjectRuntimeStatus, RecordRuntimeStartFailure, RefineRuntimeToolCallInput,
        RuntimeActivityCommitResult, RuntimeActivityFact, RuntimeConsumedCommitResult,
        RuntimeEventAdmission, RuntimeExitCommitResult, RuntimeObservedStatus,
        RuntimeSessionInitCommitResult, RuntimeStatusCommitResult,
        RuntimeToolCallInputCommitResult, StampRuntimePresence, UpsertRuntimeTextActivity,
    },
    identity_access::agent_profile_mcp::{
        AgentProfileMcpDataPlane, McpDataPlaneFaultCode, McpProbeCommand,
    },
    identity_access::agents::runtime::RuntimeKind,
    identity_access::auth::{HumanSessionAuthenticator, SessionClock, SessionTtl},
};
use silverhand_backend_filesystem::foundation::blob_store::LocalBlobStore;
use silverhand_backend_server::{
    application_orchestration::daemon_delivery::{
        DaemonDeliveryController, DaemonDeliveryEffects, DaemonDeliveryRuntime,
        HOST_PROTOCOL_VERSION, HOST_WEBSOCKET_PATH, HostDataPlaneCustody,
        OUTBOUND_PROTOCOL_VERSION, OwningHostDataPlane, UPDATE_REQUIRED_CLOSE_REASON,
    },
    foundation::clock::ProcessClock,
    foundation::scope_event_transport::{
        AllowedOrigin, ConnectionLifecycle, DESKTOP_API_EPOCH_HEADER, EVENT_WEBSOCKET_PATH,
        EVENT_WS_SESSION_COOKIE, EventWebSocketConfig, EventWebSocketController, ScopeEventHub,
        router as scope_event_router,
    },
    host_runtime::host_agent_background::agent_deletion_resumer::HostReadyObserver,
    host_runtime::host_control::admission::ProtocolMismatchRefusal,
    host_runtime::host_control::management::{self, update::HostLabelChangeEffect},
    host_runtime::host_control::proxy_config::{
        self, ProxyConfig, ProxyConfigOutcome, ProxyConfigRequest,
    },
    host_runtime::host_control::runtime_catalog::{
        self, CatalogCoordinateMismatch, RuntimeCatalogOutcome, RuntimeCatalogRequest,
    },
    host_runtime::host_control::status_seed::{SeedAttempt, SeedRead},
    host_runtime::host_control::status_snapshot::{
        self, SnapshotCoordinateMismatch, StatusSnapshotOutcome, StatusSnapshotRequest,
    },
    host_runtime::runtime_credential::RuntimeCredentialAuthority,
    host_runtime::{
        cascade_effects::{AgentDeletedAnnouncement, ProductionComputerCascadeEffects},
        detail::ConnectedHostName,
        runtime_control::ProductionRuntimeControl,
    },
    identity_access::agent_profile_mcp_data_plane::ProductionAgentProfileMcpDataPlane,
};
use silverhand_backend_sqlite::{
    conversation_messaging::messaging::MessagingStore,
    conversation_messaging::runtime_delivery::SqliteRuntimeDeliveryStore,
    foundation::{host_agent_store::SqliteHostAgentStore, scope_event::SqliteEventScopeAuthority},
    host_runtime::SqliteHostRuntimeStore,
    host_runtime::runtime_credential::SqliteRuntimeCustodyStore,
    identity_access::auth::SqliteSessionStore,
};
use sqlx::{Connection, Row, SqliteConnection, sqlite::SqliteConnectOptions};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{
        Message,
        client::IntoClientRequest,
        http::header::{AUTHORIZATION, COOKIE, ORIGIN},
        protocol::frame::coding::CloseCode,
    },
};
use tower::ServiceExt;

/// A do-nothing lifecycle for tests that exercise the WS transport alone and do
/// not assert on presence. Production wiring lives in `backend-app`; the crate's
/// only null implementation is test-scoped by design.
struct NullLifecycle;

impl ConnectionLifecycle for NullLifecycle {
    fn on_connect(&self, _viewer: &UserId, _connection_id: &str) {}
    fn on_disconnect(&self, _viewer: &UserId, _connection_id: &str) {}
}

/// A do-nothing host-ready observer for the same reason: these e2e cases drive the WS
/// transport and status/catalog/proxy control planes, never agent deletion. The
/// production resumer is the real observer, wired in `backend-app::prepare`; this
/// transport fixture only needs the router's mandatory argument satisfied.
struct NullHostReady;

impl HostReadyObserver for NullHostReady {
    fn host_ready(&self, _host: &HostId, _connection_id: &str) {}
}

const TOKEN: &str = "host-token";
const TOKEN_2: &str = "host-token-2";
const SESSION_TOKEN: &str = "session-token";
const EVENT_ORIGIN: &str = "http://desktop.test";
const AGENT: &str = "agent-delivery";
const CONVERSATION: &str = "conversation0001";

type Runtime = DaemonDeliveryRuntime<
    SqliteRuntimeCustodyStore,
    CountingRuntimeDeliveryStore,
    SqliteHostRuntimeStore,
>;
type Effects = DaemonDeliveryEffects<
    SqliteRuntimeCustodyStore,
    CountingRuntimeDeliveryStore,
    SqliteHostRuntimeStore,
>;
type RuntimeControl = ProductionRuntimeControl<
    SqliteRuntimeCustodyStore,
    CountingRuntimeDeliveryStore,
    SqliteHostRuntimeStore,
>;
type AgentProfileMcp = ProductionAgentProfileMcpDataPlane<SqliteHostAgentStore>;
type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

struct Fixture {
    _directory: tempfile::TempDir,
    path: std::path::PathBuf,
    address: std::net::SocketAddr,
    effects: Effects,
    runtime_control: RuntimeControl,
    run_states: Router,
    agent_activity: Router,
    agent_profile_mcp: AgentProfileMcp,
    workspace_files: ProductionWorkspaceDataPlane,
    settlement_calls: Arc<AtomicUsize>,
    clear_settlement_calls: Arc<AtomicUsize>,
    controller: DaemonDeliveryController,
    event_controller: EventWebSocketController,
    server: tokio::task::JoinHandle<()>,
}

#[derive(Clone)]
struct FixedSessionClock;

impl SessionClock for FixedSessionClock {
    fn now_unix_millis(&self) -> i64 {
        10_000
    }
}

#[derive(Clone, Copy)]
struct FixedManagementClock;

impl ProcessClock for FixedManagementClock {
    fn now_unix_millis(&self) -> i64 {
        777
    }
}

#[derive(Clone, Default)]
struct RecordingHostLabelChange(Arc<AtomicUsize>);

impl HostLabelChangeEffect for RecordingHostLabelChange {
    async fn agents_changed(
        &self,
        _computer: &HostId,
    ) -> Result<
        (),
        silverhand_backend_server::host_runtime::host_control::management::update::HostLabelChangeEffectFailure,
    >{
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

async fn management_app(fixture: &Fixture, label_change: RecordingHostLabelChange) -> Router {
    let store = SqliteHostRuntimeStore::open(&fixture.path).await.unwrap();
    management::router(
        store.clone(),
        store.clone(),
        store,
        label_change,
        fixture.controller.owning_host_data_plane(),
        fixture.controller.host_status_seeds(),
        fixture.controller.host_admission(),
        fixture.controller.connection_control(),
        FixedManagementClock,
    )
}

async fn management_request(
    app: &Router,
    method: &str,
    uri: &str,
    body: &'static str,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    request
        .extensions_mut()
        .insert(UserId::parse("user-1").unwrap());
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8(bytes.to_vec()).unwrap()))
    };
    (status, body)
}

async fn fixture(name: &str) -> Fixture {
    build_fixture(name, true).await
}

/// A fixture whose conversation exists but holds no inbox row, so the gate that
/// drives the real send door starts from an empty inbox.
async fn fixture_without_seeded_inbox(name: &str) -> Fixture {
    build_fixture(name, false).await
}

async fn build_fixture(name: &str, seed_inbox: bool) -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(name);
    let mut db = current_schema::create_current_schema(&path).await;
    if seed_inbox {
        seed(&mut db).await;
    } else {
        seed_participants(&mut db).await;
    }
    db.close().await.unwrap();
    let custody = SqliteRuntimeCustodyStore::open(&path).await.unwrap();
    let settlement_calls = Arc::new(AtomicUsize::new(0));
    let clear_settlement_calls = Arc::new(AtomicUsize::new(0));
    let data = CountingRuntimeDeliveryStore {
        inner: SqliteRuntimeDeliveryStore::open(&path).await.unwrap(),
        settlement_calls: settlement_calls.clone(),
        clear_settlement_calls: clear_settlement_calls.clone(),
    };
    let hosts = SqliteHostRuntimeStore::open(&path).await.unwrap();
    let event_hub = ScopeEventHub::production().unwrap();
    let runtime = Runtime::production(
        RuntimeCredentialAuthority::production(custody),
        data,
        hosts,
        event_hub.runtime_activity_publisher(),
        event_hub.agent_presence_publisher(),
        event_hub.runtime_status_publisher(),
    )
    .unwrap();
    let runtime_control = runtime.runtime_control();
    let run_states = runtime.agent_run_states_router(
        MessagingStore::open(&path)
            .await
            .unwrap()
            .channel_lifecycle(),
    );
    let agent_activity = silverhand_backend_server::conversation_messaging::messaging::collaboration::conversation_agent_activity::agent_router(MessagingStore::open(&path).await.unwrap().conversation_agent_activity());
    let agent_profile_mcp =
        runtime.agent_profile_mcp_data_plane(SqliteHostAgentStore::open(&path).await.unwrap());
    let workspace_files = runtime.workspace_data_plane();
    let effects = runtime.effects();
    let controller = runtime.controller();
    let daemon_router = runtime.router(Arc::new(NullHostReady));
    let sessions = HumanSessionAuthenticator::new(
        SqliteSessionStore::open(&path).await.unwrap(),
        FixedSessionClock,
        SessionTtl::from_millis(60_000).unwrap(),
    );
    let scope_authority = SqliteEventScopeAuthority::open(&path).await.unwrap();
    let event_config =
        EventWebSocketConfig::production([AllowedOrigin::parse(EVENT_ORIGIN).unwrap()]).unwrap();
    let (event_router, event_controller) = scope_event_router(
        event_hub,
        sessions,
        scope_authority,
        Arc::new(NullLifecycle),
        event_config,
    );
    let router = daemon_router.merge(event_router);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    Fixture {
        _directory: directory,
        path,
        address,
        effects,
        runtime_control,
        run_states,
        agent_activity,
        agent_profile_mcp,
        workspace_files,
        settlement_calls,
        clear_settlement_calls,
        controller,
        event_controller,
        server,
    }
}

#[derive(Clone)]
struct CountingRuntimeDeliveryStore {
    inner: SqliteRuntimeDeliveryStore,
    settlement_calls: Arc<AtomicUsize>,
    clear_settlement_calls: Arc<AtomicUsize>,
}

impl RuntimeDispatchReadPort for CountingRuntimeDeliveryStore {
    async fn resolve_dispatch_target(
        &self,
        conversation: ConversationId,
        agent: AgentId,
    ) -> Result<Option<RuntimeDispatchTarget>, PersistenceFailure> {
        self.inner
            .resolve_dispatch_target(conversation, agent)
            .await
    }

    async fn resolve_start_dispatch(
        &self,
        agent: AgentId,
        candidate_origin: ConversationId,
    ) -> Result<Option<RuntimeDispatchTarget>, PersistenceFailure> {
        self.inner
            .resolve_start_dispatch(agent, candidate_origin)
            .await
    }

    async fn current_custodies_for_host(
        &self,
        host_id: String,
    ) -> Result<Vec<RuntimePrincipal>, PersistenceFailure> {
        self.inner.current_custodies_for_host(host_id).await
    }
}

impl RuntimeControlSeatPort for CountingRuntimeDeliveryStore {
    type Error = PersistenceFailure;

    async fn snapshot(
        &self,
        scope: &RuntimeScope,
    ) -> Result<Option<RuntimeControlSeatSnapshot>, Self::Error> {
        self.inner.snapshot(scope).await
    }

    async fn settle_clear(
        &self,
        target: &RuntimeSeatTarget,
        before: &BoundRuntimeControlSeat,
    ) -> Result<Option<ClearSessionCommitReceipt>, Self::Error> {
        self.clear_settlement_calls.fetch_add(1, Ordering::SeqCst);
        self.inner.settle_clear(target, before).await
    }
}

impl RuntimeInboxCustodyPort for CountingRuntimeDeliveryStore {
    async fn claim_pending<P: ReferencePositions + Sync>(
        &self,
        host_id: String,
        principal: RuntimePrincipal,
        conversation: ConversationId,
        owner: DeliveryLeaseOwner,
        now_ms: i64,
        lease_ttl_ms: i64,
        reference_positions: &P,
    ) -> Result<ClaimPendingInbox, PersistenceFailure> {
        self.inner
            .claim_pending(
                host_id,
                principal,
                conversation,
                owner,
                now_ms,
                lease_ttl_ms,
                reference_positions,
            )
            .await
    }

    async fn settle_consumed(
        &self,
        host_id: String,
        principal: RuntimePrincipal,
        conversation: ConversationId,
        owner: DeliveryLeaseOwner,
        delivery_ids: Vec<InboxDeliveryId>,
        observed_at_ms: i64,
    ) -> Result<InboxSettlementDisposition, PersistenceFailure> {
        self.settlement_calls.fetch_add(1, Ordering::SeqCst);
        self.inner
            .settle_consumed(
                host_id,
                principal,
                conversation,
                owner,
                delivery_ids,
                observed_at_ms,
            )
            .await
    }

    async fn settle_failed(
        &self,
        host_id: String,
        principal: RuntimePrincipal,
        conversation: ConversationId,
        owner: DeliveryLeaseOwner,
        delivery_ids: Vec<InboxDeliveryId>,
        reason: RuntimeDeliveryFailureReason,
        observed_at_ms: i64,
    ) -> Result<InboxSettlementDisposition, PersistenceFailure> {
        self.settlement_calls.fetch_add(1, Ordering::SeqCst);
        self.inner
            .settle_failed(
                host_id,
                principal,
                conversation,
                owner,
                delivery_ids,
                reason,
                observed_at_ms,
            )
            .await
    }

    async fn release_owner(&self, owner: DeliveryLeaseOwner) -> Result<u64, PersistenceFailure> {
        self.inner.release_owner(owner).await
    }
}

impl DaemonEventDurablePort for CountingRuntimeDeliveryStore {
    async fn runtime_activity_targets(
        &self,
        admission: RuntimeEventAdmission,
        origin: ConversationId,
    ) -> Result<
        Option<Vec<silverhand_backend_core::host_runtime::runtime_event::RuntimeStatusTarget>>,
        PersistenceFailure,
    > {
        self.inner.runtime_activity_targets(admission, origin).await
    }

    async fn current_admission(
        &self,
        host_id: String,
        scope: RuntimeScope,
    ) -> Result<Option<RuntimeEventAdmission>, PersistenceFailure> {
        self.inner.current_admission(host_id, scope).await
    }

    async fn has_pending_turn(
        &self,
        admission: RuntimeEventAdmission,
        conversation: ConversationId,
    ) -> Result<bool, PersistenceFailure> {
        self.inner.has_pending_turn(admission, conversation).await
    }

    async fn commit_exit(
        &self,
        event: CommitRuntimeExit,
    ) -> Result<RuntimeExitCommitResult, PersistenceFailure> {
        self.inner.commit_exit(event).await
    }

    async fn commit_consumed(
        &self,
        event: CommitRuntimeConsumed,
    ) -> Result<RuntimeConsumedCommitResult, PersistenceFailure> {
        self.settlement_calls.fetch_add(1, Ordering::SeqCst);
        self.inner.commit_consumed(event).await
    }

    async fn commit_session_init(
        &self,
        event: CommitRuntimeSessionInit,
    ) -> Result<Option<RuntimeSessionInitCommitResult>, PersistenceFailure> {
        self.inner.commit_session_init(event).await
    }

    async fn project_status(
        &self,
        event: ProjectRuntimeStatus,
    ) -> Result<Option<RuntimeStatusCommitResult>, PersistenceFailure> {
        self.inner.project_status(event).await
    }

    async fn record_activity(
        &self,
        admission: RuntimeEventAdmission,
        event: RuntimeActivityFact,
        presence_at: RuntimeEventTime,
    ) -> Result<Option<RuntimeActivityCommitResult>, PersistenceFailure> {
        self.inner
            .record_activity(admission, event, presence_at)
            .await
    }

    async fn upsert_text_activity(
        &self,
        event: UpsertRuntimeTextActivity,
    ) -> Result<Option<RuntimeActivityCommitResult>, PersistenceFailure> {
        self.inner.upsert_text_activity(event).await
    }

    async fn refine_tool_call_input(
        &self,
        event: RefineRuntimeToolCallInput,
    ) -> Result<Option<RuntimeToolCallInputCommitResult>, PersistenceFailure> {
        self.inner.refine_tool_call_input(event).await
    }

    async fn stamp_presence(
        &self,
        event: StampRuntimePresence,
    ) -> Result<Option<Vec<UserId>>, PersistenceFailure> {
        self.inner.stamp_presence(event).await
    }

    async fn record_start_failure(
        &self,
        event: RecordRuntimeStartFailure,
    ) -> Result<Option<UserId>, PersistenceFailure> {
        self.inner.record_start_failure(event).await
    }
}

async fn seed(db: &mut SqliteConnection) {
    seed_participants(db).await;
    add_inbox(db, "message-1", 1).await;
}

/// Everything the delivery fixture needs *except* a pre-seeded inbox row. The
/// gate that closes the production seam sends through the real door and asserts
/// the witness was born of that send's transaction, so it must start from a
/// conversation whose inbox is empty — no hand-planted row to claim.
async fn seed_participants(db: &mut SqliteConnection) {
    sqlx::query(
        "INSERT INTO users (id, name, display_name, created_at, updated_at)
         VALUES ('user-1', 'user-1', 'User One', 1, 1)",
    )
    .execute(&mut *db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auth_sessions (
            id, user_id, token_hash, created_at, last_used_at, revoked_at,
            user_agent, ip_first_seen
         ) VALUES ('session-1', 'user-1', ?, 1, 9_500, NULL, NULL, NULL)",
    )
    .bind(token_hash(SESSION_TOKEN))
    .execute(&mut *db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO hosts (id, name, kind, created_at, user_id)
         VALUES ('host-1', 'host-1', 'remote', 1, 'user-1')",
    )
    .execute(&mut *db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO host_credentials (
            id, host_id, token_hash, kind, expires_at, consumed_at, revoked_at
         ) VALUES ('credential-1', 'host-1', ?, 'long_lived', NULL, NULL, NULL)",
    )
    .bind(token_hash(TOKEN))
    .execute(&mut *db)
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
    .execute(&mut *db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO workspaces (
            id, name, name_key, created_at, updated_at, owner_user_id
         ) VALUES ('workspace-1', 'Workspace One', 'workspace one', 1, 1, 'user-1')",
    )
    .execute(&mut *db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO workspace_members (workspace_id, agent_id, joined_at)
         VALUES ('workspace-1', ?, 1)",
    )
    .bind(AGENT)
    .execute(&mut *db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO workspace_users (workspace_id, user_id, joined_at, role)
         VALUES ('workspace-1', 'user-1', 1, 'owner')",
    )
    .execute(&mut *db)
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
    .execute(&mut *db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversation_agents (conversation_id, agent_id, joined_at)
         VALUES (?, ?, 1)",
    )
    .bind(CONVERSATION)
    .bind(AGENT)
    .execute(&mut *db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversation_users (conversation_id, user_id, joined_at)
         VALUES (?, 'user-1', 1)",
    )
    .bind(CONVERSATION)
    .execute(&mut *db)
    .await
    .unwrap();
}

async fn add_inbox(db: &mut SqliteConnection, message_id: &str, seq: i64) {
    sqlx::query(
        "INSERT INTO messages (
            id, conversation_id, from_kind, from_user_id, content, metadata, seq, created_at
         ) VALUES (?, ?, 'user', 'user-1', 'hello runtime', ?, ?, ?)",
    )
    .bind(message_id)
    .bind(CONVERSATION)
    .bind(
        serde_json::json!({
            "subtype": "task_status",
            "task_number": 2185,
            "task_status": "in_progress",
            "attachments": [{
                "id": "file-1",
                "filename": "runtime notes.txt",
                "size": 7,
                "content_type": "text/plain"
            }]
        })
        .to_string(),
    )
    .bind(seq)
    .bind(seq)
    .execute(&mut *db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversation_inboxes (
            conversation_id, agent_id, message_id, created_at, delivery_kind
         ) VALUES (?, ?, ?, ?, 'turn')",
    )
    .bind(CONVERSATION)
    .bind(AGENT)
    .bind(message_id)
    .bind(seq)
    .execute(&mut *db)
    .await
    .unwrap();
}

fn token_hash(raw: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(raw.as_bytes()))
}

async fn connect(address: std::net::SocketAddr) -> Socket {
    connect_with_token(address, TOKEN).await.unwrap()
}

async fn connect_conversation_side(address: std::net::SocketAddr) -> Socket {
    connect_event_scope(address, &format!("conversation-side:{CONVERSATION}")).await
}

async fn connect_event_scope(address: std::net::SocketAddr, scope: &str) -> Socket {
    connect_event_scope_with_ack(address, scope).await.0
}

async fn connect_event_scope_with_ack(
    address: std::net::SocketAddr,
    scope: &str,
) -> (Socket, Value) {
    let mut request = format!("ws://{address}{EVENT_WEBSOCKET_PATH}")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert(ORIGIN, EVENT_ORIGIN.parse().unwrap());
    request.headers_mut().insert(
        COOKIE,
        format!("{EVENT_WS_SESSION_COOKIE}={SESSION_TOKEN}")
            .parse()
            .unwrap(),
    );
    request
        .headers_mut()
        .insert(DESKTOP_API_EPOCH_HEADER, "4".parse().unwrap());
    let (mut socket, _) = connect_async(request).await.unwrap();
    assert_eq!(next_json(&mut socket).await["type"], "authenticated");
    socket
        .send(Message::Text(
            json!({
                "type": "subscribe",
                "scope": scope,
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
    let subscribed = next_json(&mut socket).await;
    assert_eq!(subscribed["type"], "subscribed");
    (socket, subscribed)
}

async fn connect_with_token(
    address: std::net::SocketAddr,
    token: &str,
) -> Result<Socket, tokio_tungstenite::tungstenite::Error> {
    let mut request = format!("ws://{address}{HOST_WEBSOCKET_PATH}")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert(AUTHORIZATION, format!("Bearer {token}").parse().unwrap());
    connect_async(request).await.map(|(socket, _)| socket)
}

async fn send_ready(socket: &mut Socket, protocol_version: u32) {
    send_ready_with_instance(socket, protocol_version, "instance-test").await;
}

/// A daemon old enough that its `ready` frame predates `instance_id` — the only
/// population the refusal record exists to describe, and one the current strict
/// schema rejects outright. A current frame with the number swapped would pass that
/// schema and never exercise the envelope path at all.
async fn send_old_schema_ready(socket: &mut Socket, protocol_version: u32) {
    socket
        .send(Message::Text(
            json!({
                "direction": "daemon_to_server",
                "type": "event",
                "kind": "ready",
                "protocol_version": protocol_version,
                "daemon_version": "old",
                "daemon_id": "daemon-old",
                "host_descriptor": { "name": "Test host", "kind": "remote" },
                "capabilities": [],
                "daemon_pid": 42,
                "started_at": 100,
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
}

async fn expect_close(socket: &mut Socket, code: CloseCode, reason: &str) {
    let Some(Ok(Message::Close(Some(close)))) = socket.next().await else {
        panic!("expected the transport to be closed with {reason}")
    };
    assert_eq!(close.code, code);
    assert_eq!(close.reason, reason);
}

/// This host's refusal as a composition reads it.
///
/// Every read asks the controller for the ledger again on purpose: the writer is the
/// connection task's own handle, so if the accessor ever built a ledger instead of
/// sharing one, every assertion below would read an empty one.
fn admission_refusal(
    controller: &DaemonDeliveryController,
    host_id: &str,
) -> Option<ProtocolMismatchRefusal> {
    controller
        .host_admission()
        .refusal(&HostId::parse(host_id).unwrap())
        .unwrap()
}

async fn send_ready_with_instance(socket: &mut Socket, protocol_version: u32, instance_id: &str) {
    send_ready_with_name(socket, protocol_version, instance_id, "Test host").await;
}

async fn send_ready_with_name(
    socket: &mut Socket,
    protocol_version: u32,
    instance_id: &str,
    host_name: &str,
) {
    socket
        .send(Message::Text(
            json!({
                "direction": "daemon_to_server",
                "type": "event",
                "kind": "ready",
                "protocol_version": protocol_version,
                "daemon_version": "test",
                "daemon_id": "daemon-test",
                "instance_id": instance_id,
                "host_descriptor": { "name": host_name, "kind": "remote" },
                "capabilities": [],
                "daemon_pid": 42,
                "started_at": 100,
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
}

async fn complete_attachment(socket: &mut Socket, controller: &DaemonDeliveryController) {
    complete_attachment_with_instance(socket, controller, "instance-test").await;
}

async fn complete_attachment_with_instance(
    socket: &mut Socket,
    controller: &DaemonDeliveryController,
    instance_id: &str,
) {
    complete_attachment_until(socket, controller, instance_id, 1).await;
}

async fn complete_attachment_until(
    socket: &mut Socket,
    controller: &DaemonDeliveryController,
    instance_id: &str,
    attached: usize,
) {
    complete_attachment_for_host_until(socket, controller, "host-1", instance_id, attached).await;
}

async fn complete_attachment_for_host_until(
    socket: &mut Socket,
    controller: &DaemonDeliveryController,
    host: &str,
    instance_id: &str,
    attached: usize,
) {
    complete_attachment_transport_until(socket, controller, instance_id, attached).await;
    answer_status_seed(socket, controller, host).await;
}

async fn complete_attachment_transport_until(
    socket: &mut Socket,
    controller: &DaemonDeliveryController,
    instance_id: &str,
    attached: usize,
) {
    send_ready_with_instance(socket, OUTBOUND_PROTOCOL_VERSION, instance_id).await;
    let status = next_json(socket).await;
    assert_eq!(status["kind"], "runtime_status");
    respond(socket, &status, json!({ "runtimes": [] })).await;
    wait_for_attached_count(controller, attached).await;
}

async fn answer_status_seed(
    socket: &mut Socket,
    controller: &DaemonDeliveryController,
    host: &str,
) {
    if matches!(
        controller.host_status_seeds().read(&host_id(host)),
        Ok(SeedRead::Seeded(_))
    ) {
        return;
    }
    let status = next_json(socket).await;
    assert_eq!(status["kind"], "host_status");
    respond(
        socket,
        &status,
        json!({
            "os": "macOS 14.5",
            "os_arch": "arm64",
            "daemon_pid": 4_242,
            "daemon_version": "1.2.3",
            "started_at": 1_700_000_000_000_i64,
            "uptime_ms": 90_000,
            "active_agent_ids": [AGENT],
        }),
    )
    .await;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if matches!(
                controller.host_status_seeds().read(&host_id(host)),
                Ok(SeedRead::Seeded(_))
            ) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

async fn complete_named_attachment_until(
    socket: &mut Socket,
    controller: &DaemonDeliveryController,
    instance_id: &str,
    host_name: &str,
    attached: usize,
) {
    send_ready_with_name(socket, OUTBOUND_PROTOCOL_VERSION, instance_id, host_name).await;
    let status = next_json(socket).await;
    assert_eq!(status["kind"], "runtime_status");
    respond(socket, &status, json!({ "runtimes": [] })).await;
    wait_for_attached_count(controller, attached).await;
    answer_status_seed(socket, controller, "host-1").await;
}

async fn wait_for_attachment(controller: &DaemonDeliveryController) {
    wait_for_attached_count(controller, 1).await;
}

async fn wait_for_attached_count(controller: &DaemonDeliveryController, expected: usize) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while controller.attached_connection_count() != expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

async fn wait_for_free_host_slot(controller: &DaemonDeliveryController) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while controller.active_connection_count() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn self_reported_host_name_exists_only_for_the_live_attachment_generation() {
    let fixture = fixture("connected-host-name.sqlite").await;
    let names = fixture.controller.connected_host_names();
    let host = host_id("host-1");
    assert_eq!(names.connected_host_name(&host), None);

    // Authentication alone is not an attachment and exposes no diagnostic name.
    let mut first = connect(fixture.address).await;
    assert_eq!(names.connected_host_name(&host), None);
    complete_named_attachment_until(
        &mut first,
        &fixture.controller,
        "instance-first",
        "Daemon First",
        1,
    )
    .await;
    assert_eq!(
        names.connected_host_name(&host).as_deref(),
        Some("Daemon First")
    );

    first.close(None).await.unwrap();
    wait_for_free_host_slot(&fixture.controller).await;
    assert_eq!(names.connected_host_name(&host), None);

    let mut replacement = connect(fixture.address).await;
    complete_named_attachment_until(
        &mut replacement,
        &fixture.controller,
        "instance-replacement",
        "Daemon Replacement",
        1,
    )
    .await;
    assert_eq!(
        names.connected_host_name(&host).as_deref(),
        Some("Daemon Replacement")
    );

    replacement.close(None).await.unwrap();
    fixture.controller.close().await;
    fixture.server.abort();
}

#[tokio::test]
async fn per_host_disconnect_is_idempotent_and_never_becomes_global_shutdown() {
    let fixture = fixture("per-host-disconnect.sqlite").await;
    let mut db = open(&fixture.path).await;
    sqlx::query(
        "INSERT INTO hosts (id, name, kind, created_at, user_id)
         VALUES ('host-2', 'host-2', 'remote', 1, 'user-1')",
    )
    .execute(&mut db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO host_credentials (
            id, host_id, token_hash, kind, expires_at, consumed_at, revoked_at
         ) VALUES ('credential-2', 'host-2', ?, 'long_lived', NULL, NULL, NULL)",
    )
    .bind(token_hash(TOKEN_2))
    .execute(&mut db)
    .await
    .unwrap();

    let mut selected = connect(fixture.address).await;
    complete_attachment_until(&mut selected, &fixture.controller, "instance-1", 1).await;
    let mut survivor = connect_with_token(fixture.address, TOKEN_2).await.unwrap();
    complete_attachment_for_host_until(
        &mut survivor,
        &fixture.controller,
        "host-2",
        "instance-2",
        2,
    )
    .await;

    let control = fixture.controller.connection_control();
    control.disconnect(host_id("host-1")).await.unwrap();
    expect_close(
        &mut selected,
        CloseCode::Away,
        HOST_REVOKED_CLOSE_REASON_FOR_TEST,
    )
    .await;
    wait_for_attached_count(&fixture.controller, 1).await;
    assert!(fixture.controller.host_attached("host-2").unwrap());
    assert!(!fixture.controller.host_attached("host-1").unwrap());

    // A second attempt names no live generation and succeeds without touching host-2.
    control.disconnect(host_id("host-1")).await.unwrap();
    survivor
        .send(Message::Text(
            json!({
                "direction": "daemon_to_server",
                "type": "heartbeat",
                "kind": "ping",
                "ts": 77,
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
    let pong = next_json(&mut survivor).await;
    assert_eq!(pong["kind"], "pong");
    assert_eq!(pong["echo_ts"], 77);

    // A later generation attaches normally; the already-delivered close and the old
    // task's generation-compared cleanup cannot remove it.
    let mut replacement = connect(fixture.address).await;
    complete_named_attachment_until(
        &mut replacement,
        &fixture.controller,
        "instance-replacement",
        "Replacement host",
        2,
    )
    .await;
    tokio::task::yield_now().await;
    assert!(fixture.controller.host_attached("host-1").unwrap());
    assert_eq!(
        fixture
            .controller
            .connected_host_names()
            .connected_host_name(&host_id("host-1"))
            .as_deref(),
        Some("Replacement host")
    );

    replacement.close(None).await.unwrap();
    survivor.close(None).await.unwrap();
    fixture.controller.close().await;
    fixture.server.abort();
}

#[tokio::test]
async fn user_disconnect_closes_only_the_captured_generation_with_its_product_reason() {
    let fixture = fixture("user-host-disconnect.sqlite").await;
    let mut selected = connect(fixture.address).await;
    complete_attachment(&mut selected, &fixture.controller).await;

    let control = fixture.controller.connection_control();
    assert!(control.disconnect_user_managed(&host_id("host-1")).unwrap());
    expect_close(
        &mut selected,
        CloseCode::Away,
        HOST_DISCONNECTED_CLOSE_REASON_FOR_TEST,
    )
    .await;
    wait_for_free_host_slot(&fixture.controller).await;
    assert!(!control.disconnect_user_managed(&host_id("host-1")).unwrap());

    let mut replacement = connect(fixture.address).await;
    complete_attachment_with_instance(&mut replacement, &fixture.controller, "instance-new").await;
    replacement
        .send(Message::Text(
            json!({
                "direction": "daemon_to_server",
                "type": "heartbeat",
                "kind": "ping",
                "ts": 91,
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
    assert_eq!(next_json(&mut replacement).await["echo_ts"], 91);

    replacement.close(None).await.unwrap();
    fixture.controller.close().await;
    fixture.server.abort();
}

const HOST_REVOKED_CLOSE_REASON_FOR_TEST: &str = "host_revoked";
const HOST_DISCONNECTED_CLOSE_REASON_FOR_TEST: &str = "host_disconnected";

#[derive(Clone)]
struct RecordingDeletedAnnouncement {
    calls: Arc<AtomicUsize>,
}

impl AgentDeletedAnnouncement for RecordingDeletedAnnouncement {
    async fn agent_deleted(&self, _agent: &AgentId) -> Result<(), CustodyEffectFailure> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn production_cascade_effects_use_exact_daemon_targets_and_the_real_blob_store() {
    let fixture = fixture("cascade-effects.sqlite").await;
    let mut socket = connect(fixture.address).await;
    complete_attachment(&mut socket, &fixture.controller).await;

    let blobs = LocalBlobStore::open(fixture.path.with_extension("blobs"))
        .await
        .unwrap();
    let stored = blobs
        .put(
            &BlobNamespace::from_segments(["attachments", "cascade"]).unwrap(),
            "text/plain",
            b"delete me".to_vec(),
        )
        .await
        .unwrap();
    let stored_ref = StoredBlobRef {
        storage: stored.storage().as_str().to_owned(),
        storage_key: stored.key().as_str().to_owned(),
    };
    let deleted_calls = Arc::new(AtomicUsize::new(0));
    let effects = ProductionComputerCascadeEffects::new(
        RecordingDeletedAnnouncement {
            calls: deleted_calls.clone(),
        },
        fixture.controller.cascade_daemon_effects(),
        blobs.clone(),
    );

    effects
        .agent_deleted(&AgentId::parse(AGENT).unwrap())
        .await
        .unwrap();
    assert_eq!(deleted_calls.load(Ordering::SeqCst), 1);

    let seat = AgentRuntimeSessionFacts {
        scope: RuntimeScope::ConversationSlot {
            agent: AgentId::parse(AGENT).unwrap(),
            source: ConversationId::parse(CONVERSATION).unwrap(),
        },
        status: RuntimeObservedStatus::Active,
        generation: RuntimeGeneration::new(7).unwrap(),
    };
    let stopping_effects = effects.clone();
    let stopping_seat = seat.clone();
    let stopping = tokio::spawn(async move {
        stopping_effects
            .stop_runtime_seat(&host_id("host-1"), &stopping_seat)
            .await
    });
    let stop = next_json(&mut socket).await;
    assert_eq!(stop["kind"], "runtime_stop");
    assert_eq!(
        stop["params"],
        json!({
            "runtime_scope": {
                "kind": "conversation",
                "conversation_id": CONVERSATION,
                "agent_id": AGENT,
            },
            "runtime_generation": 7,
            "mode": "hard",
        })
    );
    respond(
        &mut socket,
        &stop,
        json!({ "mode": "hard", "stopped": true, "exit_code": 0 }),
    )
    .await;
    stopping.await.unwrap().unwrap();
    assert!(
        effects
            .stop_runtime_seat(&host_id("missing-host"), &seat)
            .await
            .is_err()
    );

    let purging_effects = effects.clone();
    let purging = tokio::spawn(async move {
        purging_effects
            .purge_host_local_files(&host_id("host-1"), &AgentId::parse(AGENT).unwrap())
            .await
    });
    let purge = next_json(&mut socket).await;
    assert_eq!(purge["kind"], "agent_files_purge");
    assert_eq!(purge["params"], json!({ "agent_id": AGENT }));
    respond(
        &mut socket,
        &purge,
        json!({ "ok": true, "purged_count": 1, "quiesced_count": 1 }),
    )
    .await;
    purging.await.unwrap().unwrap();
    assert!(
        effects
            .purge_host_local_files(&host_id("missing-host"), &AgentId::parse(AGENT).unwrap(),)
            .await
            .is_err()
    );

    // Command-specific typed refusals are admitted on this one kind and reported as
    // residue; they do not corrupt or close the live connection.
    let retry_effects = effects.clone();
    let retry = tokio::spawn(async move {
        retry_effects
            .purge_host_local_files(&host_id("host-1"), &AgentId::parse(AGENT).unwrap())
            .await
    });
    let refused = next_json(&mut socket).await;
    respond_error(
        &mut socket,
        &refused,
        json!({ "code": "partial_purge", "message": "disk refused removal" }),
    )
    .await;
    assert!(retry.await.unwrap().is_err());
    assert!(fixture.controller.host_attached("host-1").unwrap());

    effects.forget_stored_blob(&stored_ref).await.unwrap();
    assert_eq!(blobs.read(&stored).await.unwrap(), BlobReadOutcome::Missing);
    // Physical absence is idempotent, while an invalid/foreign coordinate is not
    // silently interpreted as a server path.
    effects.forget_stored_blob(&stored_ref).await.unwrap();
    assert!(
        effects
            .forget_stored_blob(&StoredBlobRef {
                storage: "local".to_owned(),
                storage_key: "../outside".to_owned(),
            })
            .await
            .is_err()
    );
    assert!(
        effects
            .forget_stored_blob(&StoredBlobRef {
                storage: "s3".to_owned(),
                storage_key: stored.key().as_str().to_owned(),
            })
            .await
            .is_err()
    );

    socket.close(None).await.unwrap();
    fixture.controller.close().await;
    fixture.server.abort();
}

fn runtime_scope() -> RuntimeScope {
    RuntimeScope::ConversationSlot {
        agent: AgentId::parse(AGENT).unwrap(),
        source: ConversationId::parse(CONVERSATION).unwrap(),
    }
}

fn runtime_control_start_target() -> RuntimeStartTarget {
    RuntimeStartTarget::new(
        UserId::parse("user-1").unwrap(),
        AgentId::parse(AGENT).unwrap(),
        ConversationId::parse(CONVERSATION).unwrap(),
    )
}

/// A deterministic conversation-scope admission over the real fixture row. The
/// seat target is minted only by the core `clear_conversation_session` use case
/// (never a cross-crate `RuntimeSeatTarget::new`), so this e2e drives the same
/// entry as production: full conversation ladder, then the row-owned
/// conversation session scope.
#[derive(Clone)]
struct ClearAdmission;

struct ClearAdmissionScope;

impl RuntimeControlAdmissionScope for ClearAdmissionScope {
    type Error = std::io::Error;

    async fn conversation_exists(&mut self, _: &ConversationId) -> Result<bool, Self::Error> {
        Ok(true)
    }

    async fn agent_exists(&mut self, _: &AgentId) -> Result<bool, Self::Error> {
        Ok(true)
    }

    async fn agent_is_member(
        &mut self,
        _: &ConversationId,
        _: &AgentId,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }

    async fn actor_may_post(
        &mut self,
        _: &ConversationId,
        _: &UserId,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }

    async fn actor_may_edit_agent(&mut self, _: &UserId, _: &AgentId) -> Result<bool, Self::Error> {
        Ok(true)
    }

    async fn agent_session_scope(
        &mut self,
        _: &AgentId,
    ) -> Result<
        Option<silverhand_backend_core::identity_access::agents::session_scope::AgentSessionScope>,
        Self::Error,
    > {
        Ok(Some(
            silverhand_backend_core::identity_access::agents::session_scope::AgentSessionScope::Conversation,
        ))
    }

    async fn finish(self) -> Result<(), Self::Error> {
        Ok(())
    }
}

impl RuntimeControlAdmission for ClearAdmission {
    type Error = std::io::Error;
    type Scope = ClearAdmissionScope;

    async fn begin_snapshot(&self) -> Result<Self::Scope, Self::Error> {
        Ok(ClearAdmissionScope)
    }
}

async fn activate_host_for_runtime_control(path: &std::path::Path) {
    let mut db = open(path).await;
    assert_eq!(
        sqlx::query("UPDATE hosts SET activated_at=1, finalized_at=1 WHERE id='host-1'")
            .execute(&mut db)
            .await
            .unwrap()
            .rows_affected(),
        1
    );
    assert_eq!(
        sqlx::query("UPDATE conversations SET latest_message_seq=1 WHERE id=?")
            .bind(CONVERSATION)
            .execute(&mut db)
            .await
            .unwrap()
            .rows_affected(),
        1,
        "the seeded message and its stream head must describe one snapshot"
    );
}

async fn start_through_runtime_control(
    fixture: &Fixture,
    socket: &mut Socket,
    target: &RuntimeStartTarget,
) -> i64 {
    let control = fixture.runtime_control.clone();
    let target = target.clone();
    let call = tokio::spawn(async move { control.start(&target).await });
    let command = next_json(socket).await;
    assert_eq!(command["kind"], "runtime_start");
    let generation = command["params"]["runtime_generation"].as_i64().unwrap();
    respond(
        socket,
        &command,
        json!({ "started": true, "pid": 7, "started_at": 100 }),
    )
    .await;
    call.await.unwrap().unwrap().unwrap();
    generation
}

async fn wait_for_counter(counter: &AtomicUsize, expected: usize) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while counter.load(Ordering::SeqCst) != expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("counter did not reach its expected value");
}

async fn next_json(socket: &mut Socket) -> Value {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match socket.next().await.unwrap().unwrap() {
                Message::Text(text) => return serde_json::from_str(&text).unwrap(),
                Message::Ping(payload) => socket.send(Message::Pong(payload)).await.unwrap(),
                other => panic!("unexpected daemon frame: {other:?}"),
            }
        }
    })
    .await
    .expect("expected websocket JSON frame")
}

async fn respond(socket: &mut Socket, command: &Value, result: Value) {
    socket
        .send(Message::Text(
            json!({
                "direction": "daemon_to_server",
                "type": "command_response",
                "id": command["id"],
                "kind": command["kind"],
                "result": result,
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
}

async fn respond_error(socket: &mut Socket, command: &Value, error: Value) {
    socket
        .send(Message::Text(
            json!({
                "direction": "daemon_to_server",
                "type": "command_response",
                "id": command["id"],
                "kind": command["kind"],
                "error": error,
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
}

fn created_message() -> CreatedMessage {
    CreatedMessage {
        client_message_id: None,
        id: MessageId::parse("message-1").unwrap(),
        conversation_id: ConversationId::parse(CONVERSATION).unwrap(),
        from_kind: FromKind::User,
        from_agent_id: None,
        from_user_id: Some(UserId::parse("user-1").unwrap()),
        author: None,
        content: MessageContent::parse("hello runtime").unwrap(),
        attachments: Vec::new(),
        seq: MessageSeq::try_from(1).unwrap(),
        created_at_ms: 1,
        thread_root_id: None,
    }
}

/// A body with no autolinks addresses nobody, so the real send door needs a
/// position rule and a direct-line authority that both answer "empty". They exist
/// only so the gate can drive the genuine `SendMessage::prepare` path.
struct NoReferences;

impl ReferencePositions for NoReferences {
    fn autolink_spans(&self, _content: &MessageContent) -> Vec<std::ops::Range<usize>> {
        Vec::new()
    }
}

struct GrantsNothing;

impl DirectLineAuthority for GrantsNothing {
    async fn direct_lines(
        &self,
        _author: &UserId,
        addressed: &AddressedPrincipals,
    ) -> Result<Vec<AgentId>, PersistenceFailure> {
        assert!(
            addressed
                .iter()
                .all(|reference| reference.kind() != PrincipalKind::Agent),
            "the gate's body addresses no agent, so no direct line is asked for"
        );
        Ok(Vec::new())
    }
}

/// P0 gate B — the production seam, end to end. No inbox row is planted: a real
/// user send travels the real `MessagingStore` door, which must commit the
/// recipient's `turn` witness inside its own send transaction. The daemon then
/// claims that exact just-committed row and delivers it. Move the witness write
/// out of the send transaction and the daemon's claim finds `NoPending` — there
/// is nothing to deliver and this gate goes red.
#[tokio::test]
async fn a_real_send_commits_the_witness_the_daemon_then_claims() {
    let fixture = fixture_without_seeded_inbox("real-send-claim.sqlite").await;

    // The real send door. user-1 addresses the channel; agent-delivery is the one
    // participant owed the message. No inbox row is seeded first.
    let store = MessagingStore::open(&fixture.path).await.unwrap();
    let draft = SendMessageDraft {
        source_conversation: ConversationId::parse(CONVERSATION).unwrap(),
        target_conversation: ConversationId::parse(CONVERSATION).unwrap(),
        actor: SendActor::User(UserId::parse("user-1").unwrap()),
        content: MessageContent::parse("a real send").unwrap(),
        destination: SendDestination::MainLine,
        attachment_refs: Vec::new(),
        freshness_bypass: false,
    };
    let command = SendMessage::prepare(draft, &NoReferences, &GrantsNothing)
        .await
        .unwrap();
    let fact = store
        .send_message(command)
        .await
        .unwrap()
        .committed()
        .clone();
    let sent = fact.message().clone();

    // The witness rode the send transaction: exactly one `turn` row for the
    // recipient, its three settlement columns still empty. Nothing was pre-planted.
    let mut db = open(&fixture.path).await;
    let rows = sqlx::query(
        "SELECT id, agent_id, message_id, delivery_kind, delivered_at, failed_at, in_flight_owner
         FROM conversation_inboxes",
    )
    .fetch_all(&mut db)
    .await
    .unwrap();
    assert_eq!(rows.len(), 1, "the send owes exactly one recipient a turn");
    let row = &rows[0];
    let delivery_id: i64 = row.get("id");
    assert_eq!(row.get::<String, _>("agent_id"), AGENT);
    assert_eq!(row.get::<String, _>("message_id"), sent.id.as_str());
    assert_eq!(row.get::<String, _>("delivery_kind"), "turn");
    assert!(row.get::<Option<i64>, _>("delivered_at").is_none());
    assert!(row.get::<Option<i64>, _>("failed_at").is_none());
    assert!(row.get::<Option<String>, _>("in_flight_owner").is_none());

    // The daemon attaches, starts the runtime, and claims that exact row.
    let mut socket = connect(fixture.address).await;
    complete_attachment(&mut socket, &fixture.controller).await;
    let effects = fixture.effects.clone();
    let start = tokio::spawn(async move {
        IdleStart::start_if_idle(
            &effects,
            &AgentId::parse(AGENT).unwrap(),
            &ConversationId::parse(CONVERSATION).unwrap(),
        )
        .await
    });
    let start_command = next_json(&mut socket).await;
    assert_eq!(start_command["kind"], "runtime_start");
    respond(
        &mut socket,
        &start_command,
        json!({ "started": true, "pid": 7, "started_at": 100 }),
    )
    .await;
    start.await.unwrap().unwrap();

    let effects = fixture.effects.clone();
    let sent_for_delivery = sent.clone();
    let deliver = tokio::spawn(async move {
        MainDelivery::deliver_to(
            &effects,
            &AgentId::parse(AGENT).unwrap(),
            &sent_for_delivery,
        )
        .await
    });
    let delivery_command = next_json(&mut socket).await;
    assert_eq!(delivery_command["kind"], "runtime_deliver");
    // The claim resolved to the row the send committed — same delivery id, and the
    // rendered header names that exact message id.
    assert_eq!(
        delivery_command["params"]["delivery_ids"],
        json!([delivery_id])
    );
    let text = delivery_command["params"]["text"].as_str().unwrap();
    assert!(
        text.contains(&format!("[msg_id={} sender=@user-1", sent.id.as_str())),
        "delivered header must name the just-committed message: {text}"
    );
    assert!(text.contains("a real send"));
    respond(
        &mut socket,
        &delivery_command,
        json!({ "disposition": "admitted", "pid": 7, "started_at": 100 }),
    )
    .await;
    deliver.await.unwrap().unwrap();

    socket.close(None).await.unwrap();
    fixture.controller.close().await;
    fixture.server.abort();
}

/// Adjacent negative for the production custody seam: dispatching a message that
/// has no committed inbox witness must stop at `NoPending`. In particular, an
/// attached daemon must not receive a fabricated `runtime_deliver` command.
#[tokio::test]
async fn delivery_without_committed_inbox_custody_emits_no_runtime_deliver() {
    let fixture = fixture_without_seeded_inbox("no-pending-no-deliver.sqlite").await;
    let mut socket = connect(fixture.address).await;
    complete_attachment(&mut socket, &fixture.controller).await;

    let effects = fixture.effects.clone();
    let delivery = tokio::spawn(async move {
        MainDelivery::deliver_to(
            &effects,
            &AgentId::parse(AGENT).unwrap(),
            &created_message(),
        )
        .await
    });

    tokio::time::timeout(std::time::Duration::from_secs(5), delivery)
        .await
        .expect("NoPending delivery must resolve without waiting on the daemon")
        .unwrap()
        .unwrap();

    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(200), socket.next())
            .await
            .is_err(),
        "NoPending must emit no runtime_deliver command"
    );
    let mut db = open(&fixture.path).await;
    let inboxes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM conversation_inboxes")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(inboxes, 0);

    socket.close(None).await.unwrap();
    fixture.controller.close().await;
    fixture.server.abort();
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

#[tokio::test]
async fn compatible_attachment_claims_then_async_consumed_settles_the_inbox() {
    let fixture = fixture("delivery.sqlite").await;
    let mut socket = connect(fixture.address).await;

    let before_ready = IdleStart::start_if_idle(
        &fixture.effects,
        &AgentId::parse(AGENT).unwrap(),
        &ConversationId::parse(CONVERSATION).unwrap(),
    )
    .await;
    assert!(before_ready.is_err());
    let mut db = open(&fixture.path).await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT generation FROM agent_runtime_slots WHERE agent_id = ? AND conversation_id = ?",
        )
        .bind(AGENT)
        .bind(CONVERSATION)
        .fetch_one(&mut db)
        .await
        .unwrap(),
        0
    );

    complete_attachment(&mut socket, &fixture.controller).await;
    let effects = fixture.effects.clone();
    let start = tokio::spawn(async move {
        IdleStart::start_if_idle(
            &effects,
            &AgentId::parse(AGENT).unwrap(),
            &ConversationId::parse(CONVERSATION).unwrap(),
        )
        .await
    });
    let start_command = next_json(&mut socket).await;
    assert_eq!(start_command["kind"], "runtime_start");
    assert_eq!(
        start_command["params"]["options"]["runtime_scope"]["kind"],
        "conversation"
    );
    assert!(
        start_command["params"]["options"]["env_policy"]["silverhand"]["auth_token"]
            .as_str()
            .is_some_and(|token| !token.is_empty() && token != TOKEN)
    );
    respond(
        &mut socket,
        &start_command,
        json!({ "started": true, "pid": 7, "started_at": 100 }),
    )
    .await;
    start.await.unwrap().unwrap();

    let effects = fixture.effects.clone();
    let deliver = tokio::spawn(async move {
        MainDelivery::deliver_to(
            &effects,
            &AgentId::parse(AGENT).unwrap(),
            &created_message(),
        )
        .await
    });
    let delivery_command = next_json(&mut socket).await;
    assert_eq!(delivery_command["kind"], "runtime_deliver");
    assert_eq!(delivery_command["params"]["delivery_ids"], json!([1]));
    assert_eq!(delivery_command["params"]["deliveries"][0]["seq"], 1);
    assert!(
        delivery_command["params"]["text"]
            .as_str()
            .unwrap()
            .contains("hello runtime")
    );
    let text = delivery_command["params"]["text"].as_str().unwrap();
    assert!(text.contains(
        "[msg_id=message-1 sender=@user-1 type=human target=#Workspace%20One/Conversation%20One time="
    ));
    assert!(text.contains(" task=#2185 status=in_progress]\nhello runtime"));
    assert!(text.contains(
        "File #1 — id=file-1 filename=\"runtime notes.txt\" content_type=text/plain size=7"
    ));
    respond(
        &mut socket,
        &delivery_command,
        json!({ "disposition": "admitted", "pid": 7, "started_at": 100 }),
    )
    .await;
    deliver.await.unwrap().unwrap();

    let leased =
        sqlx::query("SELECT delivered_at, in_flight_owner FROM conversation_inboxes WHERE id = 1")
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert!(leased.get::<Option<i64>, _>("delivered_at").is_none());
    assert!(leased.get::<Option<String>, _>("in_flight_owner").is_some());
    let generation: i64 = sqlx::query_scalar(
        "SELECT generation FROM agent_runtime_slots WHERE agent_id = ? AND conversation_id = ?",
    )
    .bind(AGENT)
    .bind(CONVERSATION)
    .fetch_one(&mut db)
    .await
    .unwrap();
    socket
        .send(Message::Text(
            json!({
                "direction": "daemon_to_server",
                "type": "event",
                "kind": "runtime_delivery_consumed",
                "runtime_scope": {
                    "kind": "conversation",
                    "conversation_id": CONVERSATION,
                    "agent_id": AGENT,
                },
                "conversation_id": CONVERSATION,
                "agent_id": AGENT,
                "runtime_generation": generation,
                "delivery_ids": [1],
                "ts": 200,
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let delivered: Option<i64> =
                sqlx::query_scalar("SELECT delivered_at FROM conversation_inboxes WHERE id = 1")
                    .fetch_one(&mut db)
                    .await
                    .unwrap();
            if delivered == Some(200) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    socket.close(None).await.unwrap();
    fixture.controller.close().await;
    fixture.server.abort();
}

#[tokio::test]
async fn host_axis_is_rejected_by_the_outbound_axis_before_attachment_or_claim() {
    assert_ne!(HOST_PROTOCOL_VERSION, OUTBOUND_PROTOCOL_VERSION);
    let fixture = fixture("wrong-axis.sqlite").await;
    let mut socket = connect(fixture.address).await;
    send_ready(&mut socket, HOST_PROTOCOL_VERSION).await;
    let close = socket.next().await.unwrap().unwrap();
    let Message::Close(Some(close)) = close else {
        panic!("wrong outbound version must close the transport")
    };
    assert_eq!(close.code, CloseCode::Protocol);
    assert_eq!(close.reason, UPDATE_REQUIRED_CLOSE_REASON);
    // Released daemon 0.2.0 matches this literal case-sensitively. Pin the
    // actual frame, not just two consumers of the same generated constant.
    assert_eq!(close.reason, "update_required");
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while fixture.controller.active_connection_count() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let mut db = open(&fixture.path).await;
    let generation: i64 = sqlx::query_scalar(
        "SELECT generation FROM agent_runtime_slots WHERE agent_id = ? AND conversation_id = ?",
    )
    .bind(AGENT)
    .bind(CONVERSATION)
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(generation, 0);
    // A version this server can read but does not speak, so the refusal is recorded:
    // the offline Computer can say WHY it will stay offline, whichever axis the
    // number came from.
    let recorded = admission_refusal(&fixture.controller, "host-1")
        .expect("a readable incompatible version is an actionable refusal");
    assert_eq!(recorded.offered_protocol_version, HOST_PROTOCOL_VERSION);
    assert_eq!(
        recorded.expected_protocol_version,
        OUTBOUND_PROTOCOL_VERSION
    );
    fixture.controller.close().await;
    fixture.server.abort();
}

#[tokio::test]
async fn an_old_schema_daemon_is_refused_with_a_reason_the_offline_computer_can_read() {
    let fixture = fixture("old-schema-refusal.sqlite").await;
    assert_eq!(admission_refusal(&fixture.controller, "host-1"), None);

    let mut socket = connect(fixture.address).await;
    send_old_schema_ready(&mut socket, OUTBOUND_PROTOCOL_VERSION - 1).await;
    expect_close(
        &mut socket,
        CloseCode::Protocol,
        UPDATE_REQUIRED_CLOSE_REASON,
    )
    .await;
    wait_for_free_host_slot(&fixture.controller).await;

    // The frame this daemon sent does not satisfy the current strict schema at all,
    // which is the whole point: read at the envelope, the refusal survives the
    // connection and does not have to be timed by a reader.
    let recorded = admission_refusal(&fixture.controller, "host-1")
        .expect("the refusal outlives the connection that caused it");
    assert_eq!(
        recorded.offered_protocol_version,
        OUTBOUND_PROTOCOL_VERSION - 1
    );
    assert_eq!(
        recorded.expected_protocol_version,
        OUTBOUND_PROTOCOL_VERSION
    );
    assert!(recorded.observed_at > 0);
    fixture.controller.close().await;
    fixture.server.abort();
}

#[tokio::test]
async fn a_refusal_is_attributed_to_the_verified_host_and_never_to_the_frame() {
    let fixture = fixture("refusal-attribution.sqlite").await;
    let mut db = open(&fixture.path).await;
    sqlx::query(
        "INSERT INTO hosts (id, name, kind, created_at, user_id)
         VALUES ('host-2', 'host-2', 'remote', 1, 'user-1')",
    )
    .execute(&mut db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO host_credentials (
            id, host_id, token_hash, kind, expires_at, consumed_at, revoked_at
         ) VALUES ('credential-2', 'host-2', ?, 'long_lived', NULL, NULL, NULL)",
    )
    .bind(token_hash(TOKEN_2))
    .execute(&mut db)
    .await
    .unwrap();

    // A dial that never authenticated: no host has been determined, so there is
    // nothing a frame of its could be attributed to. It never gets to send one.
    assert!(
        connect_with_token(fixture.address, "not-a-token")
            .await
            .is_err()
    );
    assert_eq!(admission_refusal(&fixture.controller, "host-1"), None);
    assert_eq!(admission_refusal(&fixture.controller, "host-2"), None);

    let mut socket = connect_with_token(fixture.address, TOKEN_2).await.unwrap();
    send_old_schema_ready(&mut socket, OUTBOUND_PROTOCOL_VERSION - 1).await;
    expect_close(
        &mut socket,
        CloseCode::Protocol,
        UPDATE_REQUIRED_CLOSE_REASON,
    )
    .await;
    wait_for_free_host_slot(&fixture.controller).await;

    // The host comes from the verified credential, so only that host's status changed.
    assert!(admission_refusal(&fixture.controller, "host-2").is_some());
    assert_eq!(admission_refusal(&fixture.controller, "host-1"), None);

    // And an unauthenticated dial resolves nothing either.
    assert!(
        connect_with_token(fixture.address, "not-a-token")
            .await
            .is_err()
    );
    assert!(admission_refusal(&fixture.controller, "host-2").is_some());
    fixture.controller.close().await;
    fixture.server.abort();
}

#[tokio::test]
async fn a_version_this_server_cannot_read_is_refused_without_recording_a_reason() {
    let fixture = fixture("unreadable-version.sqlite").await;

    // Above the envelope's readable bound (100). Still refused — by the strict schema,
    // as before — but not recorded: an unbounded integer from a dialer we have agreed
    // no schema with is a corrupt dialer, not a daemon a human should go upgrade.
    let mut socket = connect(fixture.address).await;
    send_ready(&mut socket, 101).await;
    expect_close(
        &mut socket,
        CloseCode::Protocol,
        UPDATE_REQUIRED_CLOSE_REASON,
    )
    .await;
    wait_for_free_host_slot(&fixture.controller).await;
    assert_eq!(admission_refusal(&fixture.controller, "host-1"), None);

    // Unreadable number AND a schema this server does not know: the pre-existing
    // strict refusal is what answers, and there is still nothing to record.
    let mut socket = connect(fixture.address).await;
    send_old_schema_ready(&mut socket, 101).await;
    expect_close(&mut socket, CloseCode::Protocol, "invalid_daemon_frame").await;
    wait_for_free_host_slot(&fixture.controller).await;
    assert_eq!(admission_refusal(&fixture.controller, "host-1"), None);
    fixture.controller.close().await;
    fixture.server.abort();
}

#[tokio::test]
async fn malformed_runtime_owner_payloads_close_the_attached_connection_as_protocol_faults() {
    let fixture = fixture("malformed-runtime-owner-payloads.sqlite").await;
    let malformed = [
        (
            "consumed",
            json!({
                "direction": "daemon_to_server",
                "type": "event",
                "kind": "runtime_delivery_consumed",
                "runtime_scope": {
                    "kind": "conversation",
                    "conversation_id": CONVERSATION,
                    "agent_id": AGENT,
                },
                "conversation_id": CONVERSATION,
                "agent_id": AGENT,
                "runtime_generation": 1,
                "ts": 200,
            }),
        ),
        (
            "failed",
            json!({
                "direction": "daemon_to_server",
                "type": "event",
                "kind": "runtime_delivery_failed",
                "runtime_scope": {
                    "kind": "conversation",
                    "conversation_id": CONVERSATION,
                    "agent_id": AGENT,
                },
                "conversation_id": CONVERSATION,
                "agent_id": AGENT,
                "runtime_generation": 1,
                "delivery_ids": [1],
                "reason": "not_a_runtime_failure_reason",
                "ts": 201,
            }),
        ),
        (
            "exit",
            json!({
                "direction": "daemon_to_server",
                "type": "event",
                "kind": "runtime_exit",
                "runtime_scope": {
                    "kind": "conversation",
                    "conversation_id": CONVERSATION,
                    "agent_id": AGENT,
                },
                "conversation_id": CONVERSATION,
                "agent_id": AGENT,
                "runtime_generation": 1,
                "code": 1,
                "intentional": "false",
                "ts": 202,
            }),
        ),
    ];

    for (name, frame) in malformed {
        let mut socket = connect(fixture.address).await;
        complete_attachment(&mut socket, &fixture.controller).await;
        socket
            .send(Message::Text(frame.to_string().into()))
            .await
            .unwrap();
        expect_close(&mut socket, CloseCode::Protocol, "invalid_daemon_frame").await;
        wait_for_free_host_slot(&fixture.controller).await;
        assert_eq!(fixture.controller.active_connection_count(), 0, "{name}");
    }

    fixture.controller.close().await;
    fixture.server.abort();
}

#[tokio::test]
async fn a_refusal_is_withheld_by_a_live_dialer_resurfaces_on_its_close_and_only_custody_clears_it()
{
    let fixture = fixture("refusal-sequence.sqlite").await;
    let mut old = connect(fixture.address).await;
    send_old_schema_ready(&mut old, OUTBOUND_PROTOCOL_VERSION - 1).await;
    expect_close(&mut old, CloseCode::Protocol, UPDATE_REQUIRED_CLOSE_REASON).await;
    wait_for_free_host_slot(&fixture.controller).await;
    let recorded = admission_refusal(&fixture.controller, "host-1").unwrap();

    // A new dialer has reserved the slot but has not handshaken. The record describes
    // a socket that is already gone and this one may be the fixed daemon, so it must
    // not be reported as this host's current state.
    let mut replacement = connect(fixture.address).await;
    assert_eq!(fixture.controller.active_connection_count(), 1);
    assert_eq!(fixture.controller.attached_connection_count(), 0);
    assert_eq!(admission_refusal(&fixture.controller, "host-1"), None);

    // A third dialer is refused because the slot is occupied. Being refused is all
    // that happens to it: it neither records nor resolves anything.
    assert!(connect_with_token(fixture.address, TOKEN).await.is_err());
    assert_eq!(admission_refusal(&fixture.controller, "host-1"), None);

    // It closed before proving anything and the registry removed its slot, so the
    // older refusal is the last word again — unchanged, because reading resolved
    // nothing and a close resolves nothing.
    replacement.close(None).await.unwrap();
    wait_for_free_host_slot(&fixture.controller).await;
    assert_eq!(
        admission_refusal(&fixture.controller, "host-1"),
        Some(recorded)
    );

    // Only a daemon that speaks the version AND is promoted to custody clears it.
    let mut current = connect(fixture.address).await;
    complete_attachment(&mut current, &fixture.controller).await;
    assert_eq!(admission_refusal(&fixture.controller, "host-1"), None);
    fixture.controller.close().await;
    fixture.server.abort();
}

#[tokio::test]
async fn a_strict_ready_that_never_becomes_custody_leaves_the_refusal_standing() {
    let fixture = fixture("refusal-unattached.sqlite").await;
    let mut old = connect(fixture.address).await;
    send_old_schema_ready(&mut old, OUTBOUND_PROTOCOL_VERSION - 1).await;
    expect_close(&mut old, CloseCode::Protocol, UPDATE_REQUIRED_CLOSE_REASON).await;
    wait_for_free_host_slot(&fixture.controller).await;
    let recorded = admission_refusal(&fixture.controller, "host-1").unwrap();

    // A current daemon whose strict `ready` passes, but whose hydration fails: it
    // never reaches attached custody, so it has proved nothing about this host and
    // the reason the host is offline is still the one already recorded.
    let mut current = connect(fixture.address).await;
    send_ready(&mut current, OUTBOUND_PROTOCOL_VERSION).await;
    let status = next_json(&mut current).await;
    assert_eq!(status["kind"], "runtime_status");
    respond(&mut current, &status, json!({ "runtimes": "not-an-array" })).await;
    expect_close(&mut current, CloseCode::Error, "runtime_hydration_failed").await;
    wait_for_free_host_slot(&fixture.controller).await;
    assert_eq!(fixture.controller.attached_connection_count(), 0);
    assert_eq!(
        admission_refusal(&fixture.controller, "host-1"),
        Some(recorded)
    );
    fixture.controller.close().await;
    fixture.server.abort();
}

#[tokio::test]
async fn authenticated_transport_is_not_attached_and_cannot_claim_before_ready() {
    let fixture = fixture("not-attached.sqlite").await;
    let mut socket = connect(fixture.address).await;
    assert_eq!(fixture.controller.active_connection_count(), 1);
    assert_eq!(fixture.controller.attached_connection_count(), 0);

    let result = MainDelivery::deliver_to(
        &fixture.effects,
        &AgentId::parse(AGENT).unwrap(),
        &created_message(),
    )
    .await;
    assert!(result.is_err());

    let mut db = open(&fixture.path).await;
    let slot = sqlx::query(
        "SELECT generation, claimed_by_daemon_instance_id
           FROM agent_runtime_slots
          WHERE agent_id = ? AND conversation_id = ?",
    )
    .bind(AGENT)
    .bind(CONVERSATION)
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(slot.get::<i64, _>("generation"), 0);
    assert!(
        slot.get::<Option<String>, _>("claimed_by_daemon_instance_id")
            .is_none()
    );
    let owner: Option<String> =
        sqlx::query_scalar("SELECT in_flight_owner FROM conversation_inboxes WHERE id = 1")
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert!(owner.is_none());

    socket.close(None).await.unwrap();
    fixture.controller.close().await;
    fixture.server.abort();
}

#[tokio::test]
async fn compatible_ready_cannot_finalize_or_settle_before_hydration_attaches() {
    let exit_fixture = fixture("pre-hydration-exit.sqlite").await;
    let custody = SqliteRuntimeCustodyStore::open(&exit_fixture.path)
        .await
        .unwrap();
    custody
        .claim_or_current(ClaimRuntimeCustody::new(
            runtime_scope(),
            DaemonInstanceId::parse("instance-test").unwrap(),
            RuntimeEventTime::from_unix_millis(10),
        ))
        .await
        .unwrap()
        .unwrap();
    let mut exit_socket = connect(exit_fixture.address).await;
    send_ready(&mut exit_socket, OUTBOUND_PROTOCOL_VERSION).await;
    let status = next_json(&mut exit_socket).await;
    assert_eq!(status["kind"], "runtime_status");
    send_runtime_exit(&mut exit_socket, 1, false, 111).await;
    let close = exit_socket.next().await.unwrap().unwrap();
    let Message::Close(Some(close)) = close else {
        panic!("pre-hydration runtime exit must close the connection")
    };
    assert_eq!(close.code, CloseCode::Policy);
    assert_eq!(close.reason, "attachment_not_hydrated");
    let mut exit_db = open(&exit_fixture.path).await;
    let daemon: Option<String> = sqlx::query_scalar(
        "SELECT claimed_by_daemon_instance_id FROM agent_runtime_slots
          WHERE agent_id = ? AND conversation_id = ?",
    )
    .bind(AGENT)
    .bind(CONVERSATION)
    .fetch_one(&mut exit_db)
    .await
    .unwrap();
    assert_eq!(daemon.as_deref(), Some("instance-test"));
    exit_fixture.controller.close().await;
    exit_fixture.server.abort();

    let settle_fixture = fixture("pre-hydration-settle.sqlite").await;
    let mut settle_socket = connect(settle_fixture.address).await;
    send_ready(&mut settle_socket, OUTBOUND_PROTOCOL_VERSION).await;
    let status = next_json(&mut settle_socket).await;
    assert_eq!(status["kind"], "runtime_status");
    send_delivery_failed(&mut settle_socket, 1, "driver_refused", 120).await;
    let close = settle_socket.next().await.unwrap().unwrap();
    let Message::Close(Some(close)) = close else {
        panic!("pre-hydration settlement must close the connection")
    };
    assert_eq!(close.code, CloseCode::Policy);
    assert_eq!(close.reason, "attachment_not_hydrated");
    assert_eq!(settle_fixture.settlement_calls.load(Ordering::SeqCst), 0);
    let mut settle_db = open(&settle_fixture.path).await;
    let inbox = sqlx::query(
        "SELECT delivered_at, failed_at, failed_reason, in_flight_owner
           FROM conversation_inboxes WHERE id = 1",
    )
    .fetch_one(&mut settle_db)
    .await
    .unwrap();
    assert!(inbox.get::<Option<i64>, _>("delivered_at").is_none());
    assert!(inbox.get::<Option<i64>, _>("failed_at").is_none());
    assert!(inbox.get::<Option<String>, _>("failed_reason").is_none());
    assert!(inbox.get::<Option<String>, _>("in_flight_owner").is_none());
    settle_fixture.controller.close().await;
    settle_fixture.server.abort();

    let consumed_fixture = fixture("pre-hydration-consumed.sqlite").await;
    let mut consumed_socket = connect(consumed_fixture.address).await;
    send_ready(&mut consumed_socket, OUTBOUND_PROTOCOL_VERSION).await;
    let status = next_json(&mut consumed_socket).await;
    assert_eq!(status["kind"], "runtime_status");
    send_delivery_consumed(&mut consumed_socket, 1, 121).await;
    let close = consumed_socket.next().await.unwrap().unwrap();
    let Message::Close(Some(close)) = close else {
        panic!("pre-hydration consumed settlement must close the connection")
    };
    assert_eq!(close.code, CloseCode::Policy);
    assert_eq!(close.reason, "attachment_not_hydrated");
    assert_eq!(consumed_fixture.settlement_calls.load(Ordering::SeqCst), 0);
    let mut consumed_db = open(&consumed_fixture.path).await;
    let delivered: Option<i64> =
        sqlx::query_scalar("SELECT delivered_at FROM conversation_inboxes WHERE id = 1")
            .fetch_one(&mut consumed_db)
            .await
            .unwrap();
    assert!(delivered.is_none());
    consumed_fixture.controller.close().await;
    consumed_fixture.server.abort();
}

#[tokio::test]
async fn synchronous_refusal_releases_the_lease_and_a_retry_can_be_admitted() {
    let fixture = fixture("refusal-retry.sqlite").await;
    let mut socket = connect(fixture.address).await;
    complete_attachment(&mut socket, &fixture.controller).await;

    let effects = fixture.effects.clone();
    let first = tokio::spawn(async move {
        MainDelivery::deliver_to(
            &effects,
            &AgentId::parse(AGENT).unwrap(),
            &created_message(),
        )
        .await
    });
    let first_command = next_json(&mut socket).await;
    assert_eq!(first_command["kind"], "runtime_deliver");
    respond(
        &mut socket,
        &first_command,
        json!({ "disposition": "refused", "reason": "startup_pending" }),
    )
    .await;
    assert!(first.await.unwrap().is_err());

    let mut db = open(&fixture.path).await;
    let first_state = sqlx::query(
        "SELECT delivered_at, failed_at, in_flight_owner
           FROM conversation_inboxes WHERE id = 1",
    )
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert!(first_state.get::<Option<i64>, _>("delivered_at").is_none());
    assert!(first_state.get::<Option<i64>, _>("failed_at").is_none());
    assert!(
        first_state
            .get::<Option<String>, _>("in_flight_owner")
            .is_none()
    );

    let effects = fixture.effects.clone();
    let retry = tokio::spawn(async move {
        MainDelivery::deliver_to(
            &effects,
            &AgentId::parse(AGENT).unwrap(),
            &created_message(),
        )
        .await
    });
    let retry_command = next_json(&mut socket).await;
    assert_eq!(retry_command["kind"], "runtime_deliver");
    assert_eq!(retry_command["params"]["delivery_ids"], json!([1]));
    respond(
        &mut socket,
        &retry_command,
        json!({ "disposition": "admitted", "pid": 7, "started_at": 100 }),
    )
    .await;
    retry.await.unwrap().unwrap();

    let owner: Option<String> =
        sqlx::query_scalar("SELECT in_flight_owner FROM conversation_inboxes WHERE id = 1")
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert!(owner.is_some());

    socket.close(None).await.unwrap();
    fixture.controller.close().await;
    fixture.server.abort();
}

/// Both automatic start arms (`runtime_deliver` spawn and idle `runtime_start`) share
/// one refusal reading: the batch stays pending, the owner gets one unread notice
/// carrying the daemon's text, and the connection stays usable.
#[tokio::test]
async fn runtime_unavailable_refusals_keep_delivery_pending_and_notify_the_owner_once() {
    let cases = [
        (
            "runtime_deliver",
            "runtime_binary_missing",
            "Codex CLI is not installed on this computer.",
        ),
        (
            "runtime_start",
            "runtime_version_unsupported",
            "Codex CLI 0.1.0 is not supported. Update to 0.2.0 or newer.",
        ),
    ];
    for (kind, code, message) in cases {
        let fixture = fixture(&format!("runtime-unavailable-{code}.sqlite")).await;
        let mut viewer = connect_event_scope(fixture.address, "viewer:user-1").await;
        let mut socket = connect(fixture.address).await;
        complete_attachment(&mut socket, &fixture.controller).await;
        assert_eq!(
            next_json(&mut viewer).await["kind"],
            "host_connection_changed"
        );

        for attempt in 0..2 {
            let effects = fixture.effects.clone();
            let call = tokio::spawn(async move {
                let agent = AgentId::parse(AGENT).unwrap();
                if kind == "runtime_deliver" {
                    MainDelivery::deliver_to(&effects, &agent, &created_message()).await
                } else {
                    IdleStart::start_if_idle(
                        &effects,
                        &agent,
                        &ConversationId::parse(CONVERSATION).unwrap(),
                    )
                    .await
                }
            });
            let command = next_json(&mut socket).await;
            assert_eq!(command["kind"], kind);
            respond_error(
                &mut socket,
                &command,
                json!({ "code": code, "message": message }),
            )
            .await;
            assert!(
                call.await.unwrap().is_err(),
                "{code} is a failure, attempt {attempt}"
            );
        }

        let notice = next_json(&mut viewer).await;
        assert_eq!(notice["kind"], "notification", "{notice:#}");
        assert_eq!(notice["payload"], json!({ "kind": "agent_start_failed" }));

        let mut db = open(&fixture.path).await;
        let inbox = sqlx::query(
            "SELECT delivered_at, failed_at, in_flight_owner FROM conversation_inboxes WHERE id = 1",
        )
        .fetch_one(&mut db)
        .await
        .unwrap();
        assert!(inbox.get::<Option<i64>, _>("delivered_at").is_none());
        assert!(inbox.get::<Option<i64>, _>("failed_at").is_none());
        assert!(inbox.get::<Option<String>, _>("in_flight_owner").is_none());
        let rows = sqlx::query(
            "SELECT user_id, agent_id, workspace_id, conversation_id, message_id, payload
               FROM notifications WHERE kind = 'agent_start_failed'",
        )
        .fetch_all(&mut db)
        .await
        .unwrap();
        assert_eq!(rows.len(), 1, "a repeated refusal must not stack notices");
        let row = &rows[0];
        assert_eq!(row.get::<String, _>("user_id"), "user-1");
        assert_eq!(row.get::<String, _>("agent_id"), AGENT);
        assert_eq!(row.get::<String, _>("workspace_id"), "workspace-1");
        assert_eq!(row.get::<String, _>("conversation_id"), CONVERSATION);
        assert!(row.get::<Option<String>, _>("message_id").is_none());
        assert_eq!(
            serde_json::from_str::<Value>(&row.get::<String, _>("payload")).unwrap(),
            json!({ "reason": code, "error": message, "agent_name": "Delivery Agent" })
        );

        socket.close(None).await.unwrap();
        fixture.controller.close().await;
        fixture.server.abort();
    }
}

/// An owner who cannot read the source conversation still gets the notice, without
/// its coordinates, and a newer reason refreshes the one unread row in place.
#[tokio::test]
async fn a_start_failure_notice_for_an_owner_outside_the_conversation_hides_its_coordinates() {
    let fixture = fixture("runtime-unavailable-outsider.sqlite").await;
    let mut db = open(&fixture.path).await;
    sqlx::query("DELETE FROM conversation_users WHERE user_id = 'user-1'")
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::query("DELETE FROM workspace_users WHERE user_id = 'user-1'")
        .execute(&mut db)
        .await
        .unwrap();
    let mut socket = connect(fixture.address).await;
    complete_attachment(&mut socket, &fixture.controller).await;

    for (code, message) in [
        (
            "runtime_binary_missing",
            "Codex CLI is not installed on this computer.",
        ),
        (
            "runtime_version_unsupported",
            "Codex CLI 0.1.0 is not supported. Update to 0.2.0 or newer.",
        ),
    ] {
        let effects = fixture.effects.clone();
        let call = tokio::spawn(async move {
            MainDelivery::deliver_to(
                &effects,
                &AgentId::parse(AGENT).unwrap(),
                &created_message(),
            )
            .await
        });
        let command = next_json(&mut socket).await;
        respond_error(
            &mut socket,
            &command,
            json!({ "code": code, "message": message }),
        )
        .await;
        assert!(call.await.unwrap().is_err());
    }

    let rows = sqlx::query(
        "SELECT user_id, workspace_id, conversation_id, payload, read_at
           FROM notifications WHERE kind = 'agent_start_failed'",
    )
    .fetch_all(&mut db)
    .await
    .unwrap();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.get::<String, _>("user_id"), "user-1");
    assert!(row.get::<Option<String>, _>("workspace_id").is_none());
    assert!(row.get::<Option<String>, _>("conversation_id").is_none());
    assert!(row.get::<Option<i64>, _>("read_at").is_none());
    assert_eq!(
        serde_json::from_str::<Value>(&row.get::<String, _>("payload")).unwrap()["reason"],
        "runtime_version_unsupported",
        "the unread row carries the latest reason"
    );

    // Coming back online, the owner's unread list (the toast surface's first load)
    // carries the notice.
    let listed = MessagingStore::open(&fixture.path)
        .await
        .unwrap()
        .notifications()
        .list(
            &UserId::parse("user-1").unwrap(),
            NotificationListQuery {
                unread_only: true,
                limit: 100,
            },
        )
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].notification.kind.as_str(), "agent_start_failed");

    socket.close(None).await.unwrap();
    fixture.controller.close().await;
    fixture.server.abort();
}

/// An unknown internal failure keeps the generic path: no notice, nothing consumed.
#[tokio::test]
async fn an_internal_start_failure_neither_notifies_nor_consumes() {
    let fixture = fixture("runtime-internal-error.sqlite").await;
    let mut socket = connect(fixture.address).await;
    complete_attachment(&mut socket, &fixture.controller).await;
    let effects = fixture.effects.clone();
    let call = tokio::spawn(async move {
        MainDelivery::deliver_to(
            &effects,
            &AgentId::parse(AGENT).unwrap(),
            &created_message(),
        )
        .await
    });
    let command = next_json(&mut socket).await;
    respond_error(
        &mut socket,
        &command,
        json!({ "code": "internal_error", "message": "boom at /Users/someone/.secret" }),
    )
    .await;
    assert!(call.await.unwrap().is_err());

    let mut db = open(&fixture.path).await;
    let notices: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM notifications")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(notices, 0);
    let delivered: Option<i64> =
        sqlx::query_scalar("SELECT delivered_at FROM conversation_inboxes WHERE id = 1")
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert!(delivered.is_none());

    socket.close(None).await.unwrap();
    fixture.controller.close().await;
    fixture.server.abort();
}

#[tokio::test]
async fn ambiguous_disconnect_releases_the_lease_but_preserves_custody_for_hydration() {
    let fixture = fixture("ambiguous.sqlite").await;
    let mut socket = connect(fixture.address).await;
    complete_attachment(&mut socket, &fixture.controller).await;

    let effects = fixture.effects.clone();
    let delivery = tokio::spawn(async move {
        MainDelivery::deliver_to(
            &effects,
            &AgentId::parse(AGENT).unwrap(),
            &created_message(),
        )
        .await
    });
    let command = next_json(&mut socket).await;
    assert_eq!(command["kind"], "runtime_deliver");
    socket.close(None).await.unwrap();
    assert!(delivery.await.unwrap().is_err());

    let mut db = open(&fixture.path).await;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let owner: Option<String> =
                sqlx::query_scalar("SELECT in_flight_owner FROM conversation_inboxes WHERE id = 1")
                    .fetch_one(&mut db)
                    .await
                    .unwrap();
            if owner.is_none() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let slot = sqlx::query(
        "SELECT generation, claimed_by_daemon_instance_id
           FROM agent_runtime_slots
          WHERE agent_id = ? AND conversation_id = ?",
    )
    .bind(AGENT)
    .bind(CONVERSATION)
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(slot.get::<i64, _>("generation"), 1);
    assert_eq!(
        slot.get::<Option<String>, _>("claimed_by_daemon_instance_id")
            .as_deref(),
        Some("instance-test")
    );

    fixture.controller.close().await;
    fixture.server.abort();
}

#[tokio::test]
async fn restart_hydration_stops_the_new_process_and_finalizes_old_daemon_custody() {
    let fixture = fixture("hydrate.sqlite").await;
    let custody = SqliteRuntimeCustodyStore::open(&fixture.path)
        .await
        .unwrap();
    let principal = custody
        .claim_or_current(ClaimRuntimeCustody::new(
            runtime_scope(),
            DaemonInstanceId::parse("previous-daemon").unwrap(),
            RuntimeEventTime::from_unix_millis(10),
        ))
        .await
        .unwrap()
        .unwrap()
        .into_principal();
    assert_eq!(principal.custody().generation().as_i64(), 1);

    let mut socket = connect(fixture.address).await;
    send_ready(&mut socket, OUTBOUND_PROTOCOL_VERSION).await;
    let status = next_json(&mut socket).await;
    assert_eq!(status["kind"], "runtime_status");
    respond(
        &mut socket,
        &status,
        json!({
            "runtimes": [{
                "runtime_scope": {
                    "kind": "conversation",
                    "conversation_id": CONVERSATION,
                    "agent_id": AGENT,
                },
                "conversation_id": CONVERSATION,
                "agent_id": AGENT,
                "runtime_generation": 1,
                "pid": 7,
                "phase": "ready",
                "run_state": null,
                "started_at": 100,
            }]
        }),
    )
    .await;
    let stop = next_json(&mut socket).await;
    assert_eq!(stop["kind"], "runtime_stop");
    assert_eq!(stop["params"]["runtime_generation"], 1);
    respond(
        &mut socket,
        &stop,
        json!({ "mode": "hard", "stopped": true, "exit_code": 0 }),
    )
    .await;
    wait_for_attachment(&fixture.controller).await;

    let mut db = open(&fixture.path).await;
    let slot = sqlx::query(
        "SELECT generation, claimed_by_daemon_instance_id
           FROM agent_runtime_slots
          WHERE agent_id = ? AND conversation_id = ?",
    )
    .bind(AGENT)
    .bind(CONVERSATION)
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(slot.get::<i64, _>("generation"), 1);
    assert!(
        slot.get::<Option<String>, _>("claimed_by_daemon_instance_id")
            .is_none()
    );

    socket.close(None).await.unwrap();
    fixture.controller.close().await;
    fixture.server.abort();
}

fn host_id(raw: &str) -> HostId {
    HostId::parse(raw).unwrap()
}

fn daemon_instance(raw: &str) -> DaemonInstanceId {
    DaemonInstanceId::parse(raw).unwrap()
}

fn custody(dispatch: &OwningHostDataPlane, host: &str) -> HostDataPlaneCustody {
    dispatch
        .resolve(&host_id(host))
        .expect("the registry is readable")
        .expect("the host is attached")
}

fn catalog_request(custody: HostDataPlaneCustody, force: bool) -> RuntimeCatalogRequest {
    RuntimeCatalogRequest::new(custody, force)
}

fn probe_in_background(
    dispatch: &OwningHostDataPlane,
    request: RuntimeCatalogRequest,
) -> tokio::task::JoinHandle<RuntimeCatalogOutcome> {
    let dispatch = dispatch.clone();
    tokio::spawn(async move { runtime_catalog::probe(&dispatch, request).await })
}

fn status_request(custody: HostDataPlaneCustody) -> StatusSnapshotRequest {
    StatusSnapshotRequest::new(custody)
}

fn status_probe_in_background(
    dispatch: &OwningHostDataPlane,
    request: StatusSnapshotRequest,
) -> tokio::task::JoinHandle<StatusSnapshotOutcome> {
    let dispatch = dispatch.clone();
    tokio::spawn(async move { status_snapshot::probe(&dispatch, request).await })
}

async fn assert_no_daemon_frame(socket: &mut Socket) {
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(200), socket.next())
            .await
            .is_err(),
        "the transport must not have put a frame on the wire"
    );
}

fn proxy_get_in_background(
    dispatch: &OwningHostDataPlane,
    request: ProxyConfigRequest,
) -> tokio::task::JoinHandle<ProxyConfigOutcome> {
    let dispatch = dispatch.clone();
    tokio::spawn(async move { proxy_config::get(&dispatch, request).await })
}

fn proxy_set_in_background(
    dispatch: &OwningHostDataPlane,
    request: ProxyConfigRequest,
    config: ProxyConfig,
) -> tokio::task::JoinHandle<ProxyConfigOutcome> {
    let dispatch = dispatch.clone();
    tokio::spawn(async move { proxy_config::set(&dispatch, request, config).await })
}

#[tokio::test]
async fn an_unknown_host_is_absent_without_putting_anything_on_another_hosts_wire() {
    let fixture = fixture("host-command-instance-gate.sqlite").await;
    let mut socket = connect(fixture.address).await;
    complete_attachment(&mut socket, &fixture.controller).await;
    let dispatch = fixture.controller.owning_host_data_plane();

    assert!(dispatch.resolve(&host_id("host-2")).unwrap().is_none());
    assert_no_daemon_frame(&mut socket).await;

    let probing = probe_in_background(
        &dispatch,
        catalog_request(custody(&dispatch, "host-1"), true),
    );
    let command = next_json(&mut socket).await;
    assert_eq!(command["kind"], "host_runtime_catalog");
    assert_eq!(command["params"], json!({ "force": true }));
    respond(
        &mut socket,
        &command,
        json!({ "runtimes": [], "detected_at": 5 }),
    )
    .await;
    let RuntimeCatalogOutcome::Detected(catalog) = probing.await.unwrap() else {
        panic!("the custody that is actually attached must answer")
    };
    let view = catalog
        .rows_for(&custody(&dispatch, "host-1"))
        .expect("the fact opens for the coordinate that asked for it");
    assert!(view.runtimes.is_empty());
    assert_eq!(view.detected_at, 5);

    socket.close(None).await.unwrap();
    fixture.controller.close().await;
    fixture.server.abort();
}

#[tokio::test]
async fn proxy_config_get_and_set_share_the_real_generation_pinned_correlated_transport() {
    let fixture = fixture("host-proxy-config-commands.sqlite").await;
    let mut socket = connect(fixture.address).await;
    complete_attachment(&mut socket, &fixture.controller).await;
    let dispatch = fixture.controller.owning_host_data_plane();

    let reading = proxy_get_in_background(
        &dispatch,
        ProxyConfigRequest::new(custody(&dispatch, "host-1")),
    );
    let get = next_json(&mut socket).await;
    assert_eq!(get["kind"], "host_proxy_config_get");
    assert_eq!(get["params"], json!({}));
    respond(
        &mut socket,
        &get,
        json!({
            "config": {
                "proxy_url": null,
                "proxy_runtimes": ["codex"]
            }
        }),
    )
    .await;
    let ProxyConfigOutcome::Answered(config) = reading.await.unwrap() else {
        panic!("proxy-config GET must decode its correlated answer")
    };
    assert_eq!(config.proxy_url, None);
    assert_eq!(config.proxy_runtimes, vec![RuntimeKind::Codex]);

    let writing = proxy_set_in_background(
        &dispatch,
        ProxyConfigRequest::new(custody(&dispatch, "host-1")),
        ProxyConfig {
            proxy_url: Some("http://127.0.0.1:7890".to_owned()),
            proxy_runtimes: vec![RuntimeKind::Codex, RuntimeKind::Qoder],
        },
    );
    let set = next_json(&mut socket).await;
    assert_eq!(set["kind"], "host_proxy_config_set");
    assert_eq!(
        set["params"],
        json!({
            "proxy_url": "http://127.0.0.1:7890",
            "proxy_runtimes": ["codex", "qoder"]
        })
    );
    respond(
        &mut socket,
        &set,
        json!({
            "config": {
                "proxy_url": "http://127.0.0.1:7890",
                "proxy_runtimes": ["codex", "qoder"]
            }
        }),
    )
    .await;
    let ProxyConfigOutcome::Answered(config) = writing.await.unwrap() else {
        panic!("proxy-config SET must decode its correlated answer")
    };
    assert_eq!(config.proxy_url.as_deref(), Some("http://127.0.0.1:7890"));
    assert_eq!(
        config.proxy_runtimes,
        vec![RuntimeKind::Codex, RuntimeKind::Qoder]
    );

    socket.close(None).await.unwrap();
    fixture.controller.close().await;
    fixture.server.abort();
}

#[tokio::test]
async fn the_unmounted_host_management_router_composes_sqlite_live_transport_and_http_wires() {
    let fixture = fixture("host-management-router.sqlite").await;
    let mut db = open(&fixture.path).await;
    sqlx::query(
        "UPDATE hosts
            SET activated_at = 2,
                finalized_at = NULL,
                provisioning_source = 'user_invite',
                last_seen_at = 3
          WHERE id = 'host-1'",
    )
    .execute(&mut db)
    .await
    .unwrap();

    let announcements = RecordingHostLabelChange::default();
    let app = management_app(&fixture, announcements.clone()).await;

    // Shared GET/PUT path proves its authenticated method table rather than relying
    // on an outer unauthenticated 401 to hide an accidental handler.
    assert_eq!(
        management_request(&app, "POST", "/api/hosts/host-1/proxy-config", "{}")
            .await
            .0,
        StatusCode::METHOD_NOT_ALLOWED
    );

    let (status, body) = management_request(
        &app,
        "PATCH",
        "/api/hosts/host-1",
        r#"{"display_label":" Studio "}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["host"]["display_label"], "Studio");
    assert_eq!(body["host"]["finalized_at"], 777);
    assert_eq!(announcements.0.load(Ordering::SeqCst), 1);

    // The host is active but offline. PUT parses before transport resolution just
    // like TS: malformed input is a 400, while a valid GET reaches the typed offline
    // data-plane outcome.
    assert_eq!(
        management_request(&app, "PUT", "/api/hosts/host-1/proxy-config", "{}")
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    let (status, body) =
        management_request(&app, "GET", "/api/hosts/host-1/proxy-config", "").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error_code"], "computer_offline");

    let (status, body) = management_request(&app, "GET", "/api/hosts/host-1/status", "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["availability"], "disconnected");
    assert_eq!(body["agents"][0]["online"], "offline");

    let (status, body) = management_request(&app, "POST", "/api/hosts/host-1/disconnect", "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["disconnected"], false);

    let mut daemon = connect(fixture.address).await;
    complete_attachment(&mut daemon, &fixture.controller).await;

    let status_app = app.clone();
    let (status, body) =
        management_request(&status_app, "GET", "/api/hosts/host-1/status", "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["availability"], "available");
    assert_eq!(body["snapshot"]["daemon_pid"], 4_242);
    assert_eq!(body["agents"][0]["online"], "offline");

    let proxy_app = app.clone();
    let proxy_read = tokio::spawn(async move {
        management_request(&proxy_app, "GET", "/api/hosts/host-1/proxy-config", "").await
    });
    let command = next_json(&mut daemon).await;
    assert_eq!(command["kind"], "host_proxy_config_get");
    respond(
        &mut daemon,
        &command,
        json!({ "config": { "proxy_url": null, "proxy_runtimes": ["codex"] } }),
    )
    .await;
    let (status, body) = proxy_read.await.unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["config"]["proxy_runtimes"], json!(["codex"]));

    let catalog_app = app.clone();
    let catalog_read = tokio::spawn(async move {
        management_request(
            &catalog_app,
            "GET",
            "/api/hosts/host-1/runtime-catalog?force=1",
            "",
        )
        .await
    });
    let command = next_json(&mut daemon).await;
    assert_eq!(command["kind"], "host_runtime_catalog");
    assert_eq!(command["params"], json!({ "force": true }));
    respond(
        &mut daemon,
        &command,
        json!({ "runtimes": [], "detected_at": 5 }),
    )
    .await;
    let (status, body) = catalog_read.await.unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "runtimes": [], "detected_at": 5 }));

    let (status, body) = management_request(&app, "POST", "/api/hosts/host-1/disconnect", "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["disconnected"], true);
    expect_close(
        &mut daemon,
        CloseCode::Away,
        HOST_DISCONNECTED_CLOSE_REASON_FOR_TEST,
    )
    .await;

    fixture.controller.close().await;
    fixture.server.abort();
}

#[tokio::test]
async fn a_status_snapshot_rides_the_wire_the_deployed_daemon_answers_and_binds_its_facts() {
    let fixture = fixture("host-command-status-snapshot.sqlite").await;
    let mut socket = connect(fixture.address).await;
    complete_attachment(&mut socket, &fixture.controller).await;
    let dispatch = fixture.controller.owning_host_data_plane();

    let probing =
        status_probe_in_background(&dispatch, status_request(custody(&dispatch, "host-1")));
    let command = next_json(&mut socket).await;
    // The literal pair the deployed daemon dispatches on. A rename on either side is
    // an unknown intent, not a decode failure.
    assert_eq!(command["kind"], "host_status");
    assert_eq!(command["params"], json!({}));
    respond(
        &mut socket,
        &command,
        json!({
            "os": "macOS 14.5",
            "os_arch": "arm64",
            "daemon_pid": 4_242,
            "daemon_version": "1.2.3",
            "started_at": 1_700_000_000_000_i64,
            "uptime_ms": 90_000,
            "active_agent_ids": ["agent-1", "agent-2"],
        }),
    )
    .await;
    let StatusSnapshotOutcome::Answered(snapshot) = probing.await.unwrap() else {
        panic!("the custody that is actually attached must answer")
    };
    let view = snapshot
        .facts_for(&host_id("host-1"), &daemon_instance("instance-test"))
        .expect("the fact opens for the coordinate that asked for it");
    assert_eq!(view.os, "macOS 14.5");
    assert_eq!(view.os_arch, Some("arm64"));
    assert_eq!(view.daemon_pid, 4_242);
    assert_eq!(view.daemon_version, "1.2.3");
    assert_eq!(view.started_at, 1_700_000_000_000);
    assert_eq!(view.uptime_ms_at_answer, 90_000);
    let mut active = view
        .active_agents
        .iter()
        .map(|agent| agent.as_str())
        .collect::<Vec<_>>();
    active.sort_unstable();
    assert_eq!(active, vec!["agent-1", "agent-2"]);

    // The facts this machine answered stay unreadable under any other machine's key.
    assert!(matches!(
        snapshot.facts_for(&host_id("host-2"), &daemon_instance("instance-test")),
        Err(SnapshotCoordinateMismatch::Host)
    ));

    socket.close(None).await.unwrap();
    fixture.controller.close().await;
    fixture.server.abort();
}

#[tokio::test]
async fn the_seed_store_asks_the_attached_custody_and_every_handle_reads_what_it_kept() {
    let fixture = fixture("host-status-seed.sqlite").await;
    let mut viewer_events = connect_event_scope(fixture.address, "viewer:user-1").await;
    // Two handles from the one runtime. Everything below seeds through the first and
    // reads through the second: a store built a second time would answer this host
    // from facts it never saw.
    let seeds = fixture.controller.host_status_seeds();
    let reader = fixture.controller.host_status_seeds();
    let mut socket = connect(fixture.address).await;

    // The socket has reserved this host's slot and proven nothing. The admission
    // ledger's occupancy port would already name it; this store reads the ATTACHED
    // custody, so the host is absent and no frame is worth sending to it.
    assert_eq!(
        reader.read(&host_id("host-1")).unwrap(),
        SeedRead::Disconnected
    );
    assert_eq!(
        seeds
            .seed_on_attach(&host_id("host-1"), &daemon_instance("instance-test"))
            .await,
        Ok(SeedAttempt::Superseded)
    );
    assert_no_daemon_frame(&mut socket).await;

    complete_attachment_transport_until(&mut socket, &fixture.controller, "instance-test", 1).await;
    // Attached, and its facts have not landed: not a fault, and not a value.
    assert_eq!(
        reader.read(&host_id("host-1")).unwrap(),
        SeedRead::NotSeeded
    );

    let command = next_json(&mut socket).await;
    assert_eq!(command["kind"], "host_status");
    respond(
        &mut socket,
        &command,
        json!({
            "os": "macOS 14.5",
            "os_arch": "arm64",
            "daemon_pid": 4_242,
            "daemon_version": "1.2.3",
            "started_at": 1_700_000_000_000_i64,
            "uptime_ms": 90_000,
            "active_agent_ids": ["agent-1"],
        }),
    )
    .await;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !matches!(reader.read(&host_id("host-1")), Ok(SeedRead::Seeded(_))) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    let SeedRead::Seeded(status) = reader.read(&host_id("host-1")).unwrap() else {
        panic!("the handle that did not seed must read what the store kept")
    };
    assert_eq!(status.daemon_instance, daemon_instance("instance-test"));
    assert_eq!(status.os, "macOS 14.5");
    assert_eq!(status.os_arch.as_deref(), Some("arm64"));
    assert_eq!(status.daemon_pid, 4_242);
    assert_eq!(status.daemon_version, "1.2.3");
    assert_eq!(status.started_at, 1_700_000_000_000);
    assert!(status.uptime_ms >= 90_000);
    // Hydration completed with an empty runtime set, so the event-fed owner
    // overrides the pre-hydrate active-agent ids carried by the status seed.
    assert!(status.active_agents.is_empty());

    let seeded = next_json(&mut viewer_events).await;
    assert_eq!(seeded["scope"], "viewer:user-1");
    assert_eq!(seeded["kind"], "host_connection_changed");
    assert_eq!(seeded["payload"], json!({ "host_id": "host-1" }));

    // Its process did not restart, so it is not asked again — and the read surface
    // never asks at all.
    assert_eq!(
        seeds
            .seed_on_attach(&host_id("host-1"), &daemon_instance("instance-test"))
            .await,
        Ok(SeedAttempt::AlreadySealed)
    );
    assert_no_daemon_frame(&mut socket).await;

    // The daemon leaves. The registry is the authority for that, so the read that
    // observes it reports an absent machine and keeps nothing to serve.
    socket.close(None).await.unwrap();
    wait_for_attached_count(&fixture.controller, 0).await;
    assert_eq!(
        reader.read(&host_id("host-1")).unwrap(),
        SeedRead::Disconnected
    );

    fixture.controller.close().await;
    viewer_events.close(None).await.unwrap();
    fixture.event_controller.close().await;
    fixture.server.abort();
}

#[tokio::test]
async fn same_daemon_instance_reconnect_requires_a_fresh_connection_generation_seed() {
    let fixture = fixture("host-status-same-instance-reconnect.sqlite").await;
    let seeds = fixture.controller.host_status_seeds();
    let mut first = connect(fixture.address).await;
    complete_attachment(&mut first, &fixture.controller).await;
    assert!(matches!(
        seeds.read(&host_id("host-1")),
        Ok(SeedRead::Seeded(_))
    ));
    first.close(None).await.unwrap();
    wait_for_free_host_slot(&fixture.controller).await;

    let mut replacement = connect(fixture.address).await;
    complete_attachment_transport_until(&mut replacement, &fixture.controller, "instance-test", 1)
        .await;
    assert_eq!(seeds.read(&host_id("host-1")).unwrap(), SeedRead::NotSeeded);
    let command = next_json(&mut replacement).await;
    assert_eq!(command["kind"], "host_status");
    respond(
        &mut replacement,
        &command,
        json!({
            "os": "generation-b",
            "os_arch": "arm64",
            "daemon_pid": 4_243,
            "daemon_version": "1.2.4",
            "started_at": 1_700_000_000_100_i64,
            "uptime_ms": 1,
            "active_agent_ids": [],
        }),
    )
    .await;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Ok(SeedRead::Seeded(status)) = seeds.read(&host_id("host-1"))
                && status.os == "generation-b"
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    replacement.close(None).await.unwrap();
    fixture.controller.close().await;
    fixture.server.abort();
}

#[tokio::test]
async fn status_seed_timeout_is_driven_through_the_one_second_retry_to_seal() {
    let fixture = fixture("host-status-production-retry.sqlite").await;
    let mut socket = connect(fixture.address).await;
    complete_attachment_transport_until(&mut socket, &fixture.controller, "instance-test", 1).await;
    let first = next_json(&mut socket).await;
    assert_eq!(first["kind"], "host_status");
    let started = tokio::time::Instant::now();

    let retry = tokio::time::timeout(std::time::Duration::from_secs(13), async {
        loop {
            match socket.next().await.unwrap().unwrap() {
                Message::Text(text) => {
                    let frame: Value = serde_json::from_str(&text).unwrap();
                    if frame["kind"] == "host_status" {
                        break frame;
                    }
                }
                Message::Ping(payload) => socket.send(Message::Pong(payload)).await.unwrap(),
                other => panic!("unexpected daemon frame: {other:?}"),
            }
        }
    })
    .await
    .expect("the production retry driver must wake after timeout + 1 second");
    assert!(started.elapsed() >= std::time::Duration::from_secs(11));
    respond(
        &mut socket,
        &retry,
        json!({
            "os": "retried",
            "daemon_pid": 42,
            "daemon_version": "test",
            "started_at": 100,
            "uptime_ms": 11_000,
            "active_agent_ids": [],
        }),
    )
    .await;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !matches!(
            fixture
                .controller
                .host_status_seeds()
                .read(&host_id("host-1")),
            Ok(SeedRead::Seeded(_))
        ) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "retry response did not seal: {:?}",
            fixture
                .controller
                .host_status_seeds()
                .read(&host_id("host-1"))
        )
    });

    socket.close(None).await.unwrap();
    fixture.controller.close().await;
    fixture.server.abort();
}

#[tokio::test]
async fn a_replacement_daemon_moves_the_expected_custody_and_strands_the_old_command() {
    let fixture = fixture("host-command-replacement.sqlite").await;
    let mut first = connect(fixture.address).await;
    complete_attachment_with_instance(&mut first, &fixture.controller, "instance-a").await;
    let dispatch = fixture.controller.owning_host_data_plane();
    let first_custody = custody(&dispatch, "host-1");

    // The connection dies with the command on it, so whether the machine ran it is
    // unknowable: this operation may only report that the machine did not answer.
    let in_flight = probe_in_background(&dispatch, catalog_request(first_custody.clone(), true));
    let command = next_json(&mut first).await;
    assert_eq!(command["kind"], "host_runtime_catalog");
    first.close(None).await.unwrap();
    assert!(matches!(
        in_flight.await.unwrap(),
        RuntimeCatalogOutcome::DaemonInternalError
    ));
    wait_for_free_host_slot(&fixture.controller).await;

    assert!(matches!(
        runtime_catalog::probe(&dispatch, catalog_request(first_custody.clone(), true)).await,
        RuntimeCatalogOutcome::AttachPending
    ));

    let mut second = connect(fixture.address).await;
    // The daemon process identity is deliberately unchanged. Only the registry's
    // connection generation moves, so this proves the expectedConnectionId-equivalent
    // guard rather than an easier daemon-instance mismatch.
    complete_attachment_with_instance(&mut second, &fixture.controller, "instance-a").await;
    assert!(matches!(
        runtime_catalog::probe(&dispatch, catalog_request(first_custody.clone(), true)).await,
        RuntimeCatalogOutcome::AttachPending
    ));
    assert_no_daemon_frame(&mut second).await;

    let second_custody = custody(&dispatch, "host-1");
    let answered = probe_in_background(&dispatch, catalog_request(second_custody.clone(), true));
    let command = next_json(&mut second).await;
    respond(
        &mut second,
        &command,
        json!({ "runtimes": [], "detected_at": 9 }),
    )
    .await;
    let RuntimeCatalogOutcome::Detected(catalog) = answered.await.unwrap() else {
        panic!("the replacement custody must answer for itself")
    };
    assert_eq!(
        catalog
            .rows_for(&second_custody)
            .expect("the fact opens for the custody that produced it")
            .detected_at,
        9
    );
    // The stranded connection never produced these rows, even though the replacement
    // daemon spells the same process instance.
    assert!(matches!(
        catalog.rows_for(&first_custody),
        Err(CatalogCoordinateMismatch::ConnectionGeneration)
    ));

    second.close(None).await.unwrap();
    fixture.controller.close().await;
    fixture.server.abort();
}

#[tokio::test]
async fn an_answer_for_an_unknown_id_cannot_settle_the_command_in_flight() {
    let fixture = fixture("host-command-stray-answer.sqlite").await;
    let mut socket = connect(fixture.address).await;
    complete_attachment(&mut socket, &fixture.controller).await;
    let dispatch = fixture.controller.owning_host_data_plane();

    let answered = probe_in_background(
        &dispatch,
        catalog_request(custody(&dispatch, "host-1"), true),
    );
    let live = next_json(&mut socket).await;

    // Nothing is waiting for this id, so the frame is dropped: it may not settle the
    // command that is genuinely in flight, and it may not fail the connection
    // either. The live answer arriving behind it on the same wire proves both.
    let mut stray = live.clone();
    stray["id"] = json!("rust-daemon-4242");
    respond(
        &mut socket,
        &stray,
        json!({ "runtimes": [], "detected_at": 1 }),
    )
    .await;
    respond(
        &mut socket,
        &live,
        json!({ "runtimes": [], "detected_at": 2 }),
    )
    .await;

    let RuntimeCatalogOutcome::Detected(catalog) = answered.await.unwrap() else {
        panic!("the live command must be settled by its own answer")
    };
    assert_eq!(
        catalog
            .rows_for(&custody(&dispatch, "host-1"))
            .expect("the fact opens for the coordinate that asked for it")
            .detected_at,
        2
    );

    socket.close(None).await.unwrap();
    fixture.controller.close().await;
    fixture.server.abort();
}

/// Every frame the daemon side received, in order, so the assertions below read a
/// record instead of guessing when something should have arrived. Heartbeats are
/// answered here because nothing in these tests is about liveness checking, and a
/// close is recorded rather than swallowed: it is the shape a mis-settlement takes.
async fn next_daemon_command(socket: &mut Socket, observed: &mut Vec<Value>) -> Value {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match socket.next().await.unwrap().unwrap() {
                Message::Text(text) => {
                    let frame: Value = serde_json::from_str(&text).unwrap();
                    observed.push(frame.clone());
                    if frame["type"] == "heartbeat" {
                        socket
                            .send(Message::Text(
                                json!({
                                    "direction": "daemon_to_server",
                                    "type": "heartbeat",
                                    "kind": "pong",
                                    "ts": frame["ts"],
                                    "echo_ts": frame["ts"],
                                })
                                .to_string()
                                .into(),
                            ))
                            .await
                            .unwrap();
                        continue;
                    }
                    return frame;
                }
                Message::Ping(payload) => socket.send(Message::Pong(payload)).await.unwrap(),
                Message::Close(frame) => {
                    observed.push(json!({ "type": "close", "close": format!("{frame:?}") }));
                    panic!(
                        "the connection was closed instead of carrying a command: {observed:#?}"
                    );
                }
                other => panic!("unexpected daemon frame {other:?} after {observed:#?}"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("expected a command frame after {observed:#?}"))
}

#[tokio::test]
async fn a_timed_out_host_command_is_deregistered_before_its_late_answer_arrives() {
    let fixture = fixture("host-command-timeout-cleanup.sqlite").await;
    let mut socket = connect(fixture.address).await;
    complete_attachment(&mut socket, &fixture.controller).await;
    let dispatch = fixture.controller.owning_host_data_plane();
    let mut observed = Vec::new();

    let abandoned = probe_in_background(
        &dispatch,
        catalog_request(custody(&dispatch, "host-1"), true),
    );
    let stale = next_daemon_command(&mut socket, &mut observed).await;
    assert_eq!(stale["kind"], "host_runtime_catalog");

    // The clock is paused around the jump and nothing else: with a real socket a
    // paused clock auto-advances whenever the runtime parks on I/O, so awaiting a
    // frame while paused could cross the pong deadline this jump arms and close the
    // connection for a reason this test is not about. The budget elapsed inside the
    // paused window, so the timeout fires on the next park either way.
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(46)).await;
    tokio::time::resume();
    assert!(
        matches!(abandoned.await.unwrap(), RuntimeCatalogOutcome::Timeout),
        "the budget elapsed, so this operation may only report that nobody answered"
    );

    let answered = probe_in_background(
        &dispatch,
        catalog_request(custody(&dispatch, "host-1"), true),
    );
    let live = next_daemon_command(&mut socket, &mut observed).await;
    assert_ne!(live["id"], stale["id"]);

    // A code outside the closed refusal set is envelope-valid and fatal at settlement
    // (`only_the_closed_refusal_set_settles_a_host_command_at_all`), which makes it a
    // tripwire for "is anything still registered under this id". Nothing may be: the
    // timeout had to remove its own entry, so the frame is dropped. An entry the
    // timeout settled but left behind, or another command's entry reached because this
    // id was not found, would both close the connection instead.
    respond_error(
        &mut socket,
        &stale,
        json!({ "code": "banana", "message": "late" }),
    )
    .await;
    respond(
        &mut socket,
        &live,
        json!({ "runtimes": [], "detected_at": 7 }),
    )
    .await;

    let RuntimeCatalogOutcome::Detected(catalog) = answered.await.unwrap() else {
        panic!("the command still in flight must be settled by its own answer: {observed:#?}")
    };
    assert_eq!(
        catalog
            .rows_for(&custody(&dispatch, "host-1"))
            .expect("the fact opens for the coordinate that asked for it")
            .detected_at,
        7
    );

    // The connection is still usable, proven by another round trip on it rather than
    // by the absence of something within some window.
    let after = probe_in_background(
        &dispatch,
        catalog_request(custody(&dispatch, "host-1"), true),
    );
    let next = next_daemon_command(&mut socket, &mut observed).await;
    respond(
        &mut socket,
        &next,
        json!({ "runtimes": [], "detected_at": 11 }),
    )
    .await;
    let RuntimeCatalogOutcome::Detected(catalog) = after.await.unwrap() else {
        panic!("the connection outlived the late answer: {observed:#?}")
    };
    assert_eq!(
        catalog
            .rows_for(&custody(&dispatch, "host-1"))
            .expect("the fact opens for the coordinate that asked for it")
            .detected_at,
        11
    );

    let types: Vec<&str> = observed
        .iter()
        .filter_map(|frame| frame["type"].as_str())
        .collect();
    assert!(!types.contains(&"close"), "{observed:#?}");
    assert_eq!(
        types.iter().filter(|frame| **frame == "command").count(),
        3,
        "{observed:#?}"
    );

    socket.close(None).await.unwrap();
    fixture.controller.close().await;
    fixture.server.abort();
}

#[tokio::test]
async fn profile_mcp_timeout_removes_correlation_and_late_answer_cannot_poison_probe() {
    let fixture = fixture("profile-mcp-timeout-cleanup.sqlite").await;
    let mut socket = connect(fixture.address).await;
    complete_attachment(&mut socket, &fixture.controller).await;
    let agent = AgentId::parse(AGENT).unwrap();
    let mut observed = Vec::new();

    let timed_adapter = fixture.agent_profile_mcp.clone();
    let timed_agent = agent.clone();
    let timed = tokio::spawn(async move { timed_adapter.write_config(&timed_agent, "{}").await });
    let stale = next_daemon_command(&mut socket, &mut observed).await;
    assert_eq!(stale["kind"], "agent_profile_mcp_write");
    assert_eq!(stale["params"], json!({ "agent_id": AGENT, "raw": "{}" }));

    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(31)).await;
    tokio::time::resume();
    assert_eq!(
        timed.await.unwrap().unwrap_err().code,
        McpDataPlaneFaultCode::DaemonTimeout
    );

    let live_adapter = fixture.agent_profile_mcp.clone();
    let live_agent = agent.clone();
    let live = tokio::spawn(async move {
        live_adapter
            .probe(
                &live_agent,
                &McpProbeCommand {
                    server_name: "sky".to_owned(),
                    include_tools: false,
                },
            )
            .await
    });
    let command = next_daemon_command(&mut socket, &mut observed).await;
    assert_eq!(command["kind"], "mcp_probe");
    assert_ne!(command["id"], stale["id"]);

    // An unknown refusal would be fatal if the stale id still owned a pending
    // settlement. It must instead be ignored, leaving the live probe to settle
    // from its own result and the connection open.
    respond_error(
        &mut socket,
        &stale,
        json!({ "code": "banana", "message": "late raw message" }),
    )
    .await;
    respond(
        &mut socket,
        &command,
        json!({ "ok": true, "phase": "initialize" }),
    )
    .await;
    let outcome = live.await.unwrap().unwrap();
    assert!(outcome.ok);

    // A third command proves the socket outlived the late answer.
    let next_adapter = fixture.agent_profile_mcp.clone();
    let next = tokio::spawn(async move { next_adapter.read_config(&agent).await });
    let command = next_daemon_command(&mut socket, &mut observed).await;
    assert_eq!(command["kind"], "agent_profile_mcp_read");
    respond(&mut socket, &command, json!({ "ok": true, "raw": "raw" })).await;
    assert_eq!(next.await.unwrap().unwrap().as_deref(), Some("raw"));

    assert!(
        observed.iter().all(|frame| frame["type"] != "close"),
        "{observed:#?}"
    );
    socket.close(None).await.unwrap();
    fixture.controller.close().await;
    fixture.server.abort();
}

#[tokio::test]
async fn only_the_closed_refusal_set_settles_a_host_command_at_all() {
    let fixture = fixture("host-command-refusals.sqlite").await;
    let mut socket = connect(fixture.address).await;
    complete_attachment(&mut socket, &fixture.controller).await;
    let dispatch = fixture.controller.owning_host_data_plane();

    // Every admitted code means one thing to this operation — the machine cannot say
    // what it has installed — so what a socket can still prove is that each one
    // settles the caller and leaves the connection usable. Which code arrived stays
    // visible at the transport, and the admission table itself is asserted per kind
    // by `refusal_codes_are_admitted_per_kind_and_never_invented`.
    for code in [
        "unknown_intent",
        "invalid_params",
        "internal_error",
        "shutting_down",
        "request_too_large",
    ] {
        let refused = probe_in_background(
            &dispatch,
            catalog_request(custody(&dispatch, "host-1"), true),
        );
        let command = next_json(&mut socket).await;
        respond_error(
            &mut socket,
            &command,
            json!({ "code": code, "message": "refused" }),
        )
        .await;
        assert!(
            matches!(
                refused.await.unwrap(),
                RuntimeCatalogOutcome::DaemonInternalError
            ),
            "{code} is a refusal every command kind may be answered with"
        );
    }

    let surviving = probe_in_background(
        &dispatch,
        catalog_request(custody(&dispatch, "host-1"), true),
    );
    let command = next_json(&mut socket).await;
    respond(
        &mut socket,
        &command,
        json!({ "runtimes": [], "detected_at": 3 }),
    )
    .await;
    assert!(matches!(
        surviving.await.unwrap(),
        RuntimeCatalogOutcome::Detected(_)
    ));

    // A code outside the closed set, and a code only a runtime command may be refused
    // with, are the same thing to a host-control command: an answer it has no arm for.
    // Neither may surface as a refusal, and each fails the connection it arrived on.
    for error in [
        json!({ "code": "banana", "message": "invented" }),
        json!({ "code": "agent_deleting", "message": "wrong vocabulary" }),
    ] {
        let corrupt = probe_in_background(
            &dispatch,
            catalog_request(custody(&dispatch, "host-1"), true),
        );
        let command = next_json(&mut socket).await;
        respond_error(&mut socket, &command, error).await;
        assert!(matches!(
            corrupt.await.unwrap(),
            RuntimeCatalogOutcome::DaemonInternalError
        ));
        let Message::Close(Some(close)) = socket.next().await.unwrap().unwrap() else {
            panic!("a corrupt command response must close the transport")
        };
        assert_eq!(close.code, CloseCode::Protocol);
        assert_eq!(close.reason, "invalid_command_response");
        wait_for_free_host_slot(&fixture.controller).await;
        socket = connect(fixture.address).await;
        complete_attachment(&mut socket, &fixture.controller).await;
    }

    socket.close(None).await.unwrap();
    fixture.controller.close().await;
    fixture.server.abort();
}

#[tokio::test]
async fn the_runtime_catalog_probe_binds_its_answer_to_the_custody_that_produced_it() {
    let fixture = fixture("runtime-catalog-probe.sqlite").await;
    let mut socket = connect(fixture.address).await;
    complete_attachment(&mut socket, &fixture.controller).await;
    let dispatch = fixture.controller.owning_host_data_plane();

    let probing = probe_in_background(
        &dispatch,
        catalog_request(custody(&dispatch, "host-1"), false),
    );
    let command = next_json(&mut socket).await;
    assert_eq!(command["kind"], "host_runtime_catalog");
    assert_eq!(command["params"], json!({ "force": false }));
    respond(
        &mut socket,
        &command,
        json!({
            "runtimes": [{
                "kind": "codex",
                "state": "present",
                "reason": "resolved",
                "installed": true,
                "binary": "/opt/codex",
                "version": "2.0.0",
            }],
            "detected_at": 1_700_000_000_000_i64,
        }),
    )
    .await;
    let RuntimeCatalogOutcome::Detected(catalog) = probing.await.unwrap() else {
        panic!("a well-formed answer must be reported as a catalog")
    };
    let view = catalog
        .rows_for(&custody(&dispatch, "host-1"))
        .expect("the fact opens for the coordinate that asked for it");
    assert_eq!(view.detected_at, 1_700_000_000_000);
    assert_eq!(view.runtimes.len(), 1);
    assert_eq!(view.runtimes[0].kind.as_str(), "codex");
    assert_eq!(view.runtimes[0].binary.as_deref(), Some("/opt/codex"));

    socket.close(None).await.unwrap();
    fixture.controller.close().await;
    fixture.server.abort();
}

#[tokio::test]
async fn two_hosts_spelling_one_daemon_instance_cannot_read_each_others_catalog() {
    let fixture = fixture("host-command-instance-collision.sqlite").await;
    let mut db = open(&fixture.path).await;
    sqlx::query(
        "INSERT INTO hosts (id, name, kind, created_at, user_id)
         VALUES ('host-2', 'host-2', 'remote', 1, 'user-1')",
    )
    .execute(&mut db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO host_credentials (
            id, host_id, token_hash, kind, expires_at, consumed_at, revoked_at
         ) VALUES ('credential-2', 'host-2', ?, 'long_lived', NULL, NULL, NULL)",
    )
    .bind(token_hash(TOKEN_2))
    .execute(&mut db)
    .await
    .unwrap();

    // Both machines are genuinely attached and both spell their custody the same way,
    // so the daemon instance alone cannot tell one host's answer from the other's.
    let mut owner = connect(fixture.address).await;
    complete_attachment_with_instance(&mut owner, &fixture.controller, "instance-shared").await;
    let mut other = connect_with_token(fixture.address, TOKEN_2).await.unwrap();
    complete_attachment_for_host_until(
        &mut other,
        &fixture.controller,
        "host-2",
        "instance-shared",
        2,
    )
    .await;
    let dispatch = fixture.controller.owning_host_data_plane();

    let owner_probe = probe_in_background(
        &dispatch,
        catalog_request(custody(&dispatch, "host-1"), true),
    );
    let owner_command = next_json(&mut owner).await;
    respond(
        &mut owner,
        &owner_command,
        json!({ "runtimes": [], "detected_at": 1 }),
    )
    .await;
    let RuntimeCatalogOutcome::Detected(owner_catalog) = owner_probe.await.unwrap() else {
        panic!("the owner's machine answered on its own wire")
    };

    let other_probe = probe_in_background(
        &dispatch,
        catalog_request(custody(&dispatch, "host-2"), true),
    );
    let other_command = next_json(&mut other).await;
    respond(
        &mut other,
        &other_command,
        json!({ "runtimes": [], "detected_at": 2 }),
    )
    .await;
    let RuntimeCatalogOutcome::Detected(other_catalog) = other_probe.await.unwrap() else {
        panic!("the other machine answered on its own wire")
    };

    assert_eq!(
        owner_catalog
            .rows_for(&custody(&dispatch, "host-1"))
            .expect("each fact opens for the machine that produced it")
            .detected_at,
        1
    );
    assert_eq!(
        other_catalog
            .rows_for(&custody(&dispatch, "host-2"))
            .expect("each fact opens for the machine that produced it")
            .detected_at,
        2
    );
    // A bind on the custody alone would let either machine's answer be filed under
    // the other's key, which is exactly what a seed keyed by (host, instance) would
    // then serve to the wrong host.
    assert!(matches!(
        owner_catalog.rows_for(&custody(&dispatch, "host-2")),
        Err(CatalogCoordinateMismatch::Host)
    ));
    assert!(matches!(
        other_catalog.rows_for(&custody(&dispatch, "host-1")),
        Err(CatalogCoordinateMismatch::Host)
    ));

    owner.close(None).await.unwrap();
    other.close(None).await.unwrap();
    fixture.controller.close().await;
    fixture.server.abort();
}

#[tokio::test]
async fn authenticated_host_fences_colliding_daemon_instances_from_foreign_custody_and_cache() {
    let fixture = fixture("host-bound-daemon-events.sqlite").await;
    let mut db = open(&fixture.path).await;
    sqlx::query(
        "INSERT INTO hosts (id, name, kind, created_at, user_id)
         VALUES ('host-2', 'host-2', 'remote', 1, 'user-1')",
    )
    .execute(&mut db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO host_credentials (
            id, host_id, token_hash, kind, expires_at, consumed_at, revoked_at
         ) VALUES ('credential-2', 'host-2', ?, 'long_lived', NULL, NULL, NULL)",
    )
    .bind(token_hash(TOKEN_2))
    .execute(&mut db)
    .await
    .unwrap();

    let mut live = connect_conversation_side(fixture.address).await;
    let mut owner = connect(fixture.address).await;
    complete_attachment(&mut owner, &fixture.controller).await;
    let effects = fixture.effects.clone();
    let start = tokio::spawn(async move {
        IdleStart::start_if_idle(
            &effects,
            &AgentId::parse(AGENT).unwrap(),
            &ConversationId::parse(CONVERSATION).unwrap(),
        )
        .await
    });
    let start_command = next_json(&mut owner).await;
    let generation = start_command["params"]["runtime_generation"]
        .as_i64()
        .unwrap();
    respond(
        &mut owner,
        &start_command,
        json!({ "started": true, "pid": 7, "started_at": 100 }),
    )
    .await;
    start.await.unwrap().unwrap();
    send_product(
        &mut owner,
        product_frame(
            "runtime_state",
            Some(generation),
            json!({
                "state": {
                    "runId": "owner-run",
                    "agentId": AGENT,
                    "conversationId": CONVERSATION,
                    "status": "thinking",
                    "startedAt": 10,
                    "updatedAt": 11,
                    "lastActivityAt": 11,
                },
                "ts": 11,
            }),
        ),
    )
    .await;
    assert_eq!(next_json(&mut live).await["payload"]["runId"], "owner-run");

    let mut foreign = connect_with_token(fixture.address, TOKEN_2).await.unwrap();
    send_ready_with_instance(&mut foreign, OUTBOUND_PROTOCOL_VERSION, "instance-test").await;
    let status = next_json(&mut foreign).await;
    respond(
        &mut foreign,
        &status,
        json!({
            "runtimes": [{
                "runtime_scope": {
                    "kind": "conversation",
                    "conversation_id": CONVERSATION,
                    "agent_id": AGENT,
                },
                "conversation_id": CONVERSATION,
                "agent_id": AGENT,
                "runtime_generation": generation,
                "pid": 9,
                "phase": "ready",
                "run_state": {
                    "runId": "forged-run",
                    "agentId": AGENT,
                    "conversationId": CONVERSATION,
                    "status": "waiting",
                    "startedAt": 20,
                    "updatedAt": 21,
                    "lastActivityAt": 21,
                },
                "started_at": 200,
            }]
        }),
    )
    .await;
    let stop = next_json(&mut foreign).await;
    assert_eq!(stop["kind"], "runtime_stop");
    respond(
        &mut foreign,
        &stop,
        json!({ "mode": "hard", "stopped": true, "exit_code": 0 }),
    )
    .await;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while fixture.controller.attached_connection_count() != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    answer_status_seed(&mut foreign, &fixture.controller, "host-2").await;

    // The foreign host may describe the colliding daemon/scope, but its exit
    // CAS is stale. A following heartbeat is a deterministic processing
    // barrier before the persisted custody assertion.
    send_runtime_exit(&mut foreign, generation, false, 30).await;
    send_product(
        &mut foreign,
        json!({
            "direction": "daemon_to_server",
            "type": "heartbeat",
            "kind": "ping",
            "ts": 31,
        }),
    )
    .await;
    let pong = next_json(&mut foreign).await;
    assert_eq!(pong["kind"], "pong");
    assert_eq!(pong["echo_ts"], 31);
    let claimed: Option<String> = sqlx::query_scalar(
        "SELECT claimed_by_daemon_instance_id
           FROM agent_runtime_slots
          WHERE agent_id = ? AND conversation_id = ?",
    )
    .bind(AGENT)
    .bind(CONVERSATION)
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(claimed.as_deref(), Some("instance-test"));

    send_product(
        &mut foreign,
        product_frame(
            "runtime_state",
            Some(generation),
            json!({
                "state": {
                    "runId": "foreign-write",
                    "agentId": AGENT,
                    "conversationId": CONVERSATION,
                    "status": "thinking",
                    "startedAt": 30,
                    "updatedAt": 31,
                    "lastActivityAt": 31,
                },
                "ts": 31,
            }),
        ),
    )
    .await;
    send_product(
        &mut foreign,
        json!({
            "direction": "daemon_to_server",
            "type": "heartbeat",
            "kind": "ping",
            "ts": 32,
        }),
    )
    .await;
    assert_eq!(next_json(&mut foreign).await["echo_ts"], 32);
    foreign.close(None).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while fixture.controller.attached_connection_count() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    // Detaching the foreign host's colliding instance cannot clear the real
    // host's process cache. Exact owner exit still carries owner-run.
    send_runtime_exit(&mut owner, generation, true, 40).await;
    let exit = next_json(&mut live).await;
    assert_eq!(exit["kind"], "agent_status");
    assert_eq!(exit["payload"]["run_state"]["runId"], "owner-run");
    let claimed: Option<String> = sqlx::query_scalar(
        "SELECT claimed_by_daemon_instance_id
           FROM agent_runtime_slots
          WHERE agent_id = ? AND conversation_id = ?",
    )
    .bind(AGENT)
    .bind(CONVERSATION)
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert!(claimed.is_none());

    live.close(None).await.unwrap();
    owner.close(None).await.unwrap();
    fixture.event_controller.close().await;
    fixture.controller.close().await;
    fixture.server.abort();
}

#[tokio::test]
async fn unknown_host_bearer_is_rejected_before_connection_reservation() {
    let fixture = fixture("unknown-host.sqlite").await;
    let error = connect_with_token(fixture.address, "unknown-host-token")
        .await
        .unwrap_err();
    let tokio_tungstenite::tungstenite::Error::Http(response) = error else {
        panic!("unknown bearer must fail during the HTTP upgrade")
    };
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(fixture.controller.active_connection_count(), 0);
    assert_eq!(fixture.controller.attached_connection_count(), 0);

    fixture.controller.close().await;
    fixture.server.abort();
}

#[tokio::test]
async fn runtime_exit_finalizes_only_the_exact_generation_and_never_its_successor() {
    let fixture = fixture("runtime-exit.sqlite").await;
    let mut socket = connect(fixture.address).await;
    complete_attachment(&mut socket, &fixture.controller).await;

    let effects = fixture.effects.clone();
    let first = tokio::spawn(async move {
        IdleStart::start_if_idle(
            &effects,
            &AgentId::parse(AGENT).unwrap(),
            &ConversationId::parse(CONVERSATION).unwrap(),
        )
        .await
    });
    let first_start = next_json(&mut socket).await;
    assert_eq!(first_start["kind"], "runtime_start");
    assert_eq!(first_start["params"]["runtime_generation"], 1);
    respond(
        &mut socket,
        &first_start,
        json!({ "started": true, "pid": 7, "started_at": 100 }),
    )
    .await;
    first.await.unwrap().unwrap();

    // Keep this fixture focused on exact-generation finalization. A crash exit
    // now owns an automatic pending-inbox respawn effect, so use an intentional
    // exit here before explicitly claiming the successor below.
    send_runtime_exit(&mut socket, 1, true, 111).await;
    let mut db = open(&fixture.path).await;
    wait_for_slot_daemon(&mut db, None).await;

    let effects = fixture.effects.clone();
    let successor = tokio::spawn(async move {
        IdleStart::start_if_idle(
            &effects,
            &AgentId::parse(AGENT).unwrap(),
            &ConversationId::parse(CONVERSATION).unwrap(),
        )
        .await
    });
    let second_start = next_json(&mut socket).await;
    assert_eq!(second_start["kind"], "runtime_start");
    assert_eq!(second_start["params"]["runtime_generation"], 2);
    respond(
        &mut socket,
        &second_start,
        json!({ "started": true, "pid": 8, "started_at": 200 }),
    )
    .await;
    successor.await.unwrap().unwrap();

    send_runtime_exit(&mut socket, 1, true, 222).await;
    tokio::task::yield_now().await;
    let slot = sqlx::query(
        "SELECT generation, claimed_by_daemon_instance_id
           FROM agent_runtime_slots
          WHERE agent_id = ? AND conversation_id = ?",
    )
    .bind(AGENT)
    .bind(CONVERSATION)
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(slot.get::<i64, _>("generation"), 2);
    assert_eq!(
        slot.get::<Option<String>, _>("claimed_by_daemon_instance_id")
            .as_deref(),
        Some("instance-test")
    );

    socket.close(None).await.unwrap();
    fixture.controller.close().await;
    fixture.server.abort();
}

#[tokio::test]
async fn clear_session_waiter_settles_after_the_exact_runtime_exit_frame_commits() {
    let fixture = fixture("clear-runtime-exit.sqlite").await;
    activate_host_for_runtime_control(&fixture.path).await;
    let mut socket = connect(fixture.address).await;
    complete_attachment(&mut socket, &fixture.controller).await;

    let target = runtime_control_start_target();
    let generation = start_through_runtime_control(&fixture, &mut socket, &target).await;
    let control = fixture.runtime_control.clone();
    let clear = tokio::spawn(async move {
        clear_conversation_session(
            &ClearAdmission,
            &control,
            UserId::parse("user-1").unwrap(),
            AgentId::parse(AGENT).unwrap(),
            ConversationId::parse(CONVERSATION).unwrap(),
        )
        .await
    });
    let command = next_json(&mut socket).await;
    assert_eq!(command["kind"], "runtime_clear_session");
    assert_eq!(command["params"]["runtime_generation"], generation);

    // The exit deliberately wins the response race. The production frame router
    // must durably commit it and latch the waiter that was registered before send.
    send_runtime_exit(&mut socket, generation, true, 111).await;
    respond(&mut socket, &command, json!({ "cleared": true })).await;
    let outcome = clear.await.unwrap().unwrap().unwrap();
    assert!(outcome.was_running());
    assert_eq!(outcome.previous_cli_session(), None);
    assert_eq!(
        fixture.clear_settlement_calls.load(Ordering::SeqCst),
        1,
        "one durable exit releases exactly one clear settlement"
    );

    let mut db = open(&fixture.path).await;
    let slot = sqlx::query(
        "SELECT claimed_by_daemon_instance_id, cli_session_id
           FROM agent_runtime_slots
          WHERE agent_id=? AND conversation_id=?",
    )
    .bind(AGENT)
    .bind(CONVERSATION)
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert!(
        slot.get::<Option<String>, _>("claimed_by_daemon_instance_id")
            .is_none()
    );
    assert!(slot.get::<Option<String>, _>("cli_session_id").is_none());
    let audits: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM messages
          WHERE conversation_id=? AND from_kind='system'
            AND json_extract(metadata, '$.subtype')='session_cleared'",
    )
    .bind(CONVERSATION)
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(audits, 1);

    socket.close(None).await.unwrap();
    fixture.controller.close().await;
    fixture.server.abort();
}

#[tokio::test]
async fn delivery_failed_frame_cannot_release_a_clear_session_waiter() {
    let fixture = fixture("clear-delivery-failed.sqlite").await;
    activate_host_for_runtime_control(&fixture.path).await;
    let mut socket = connect(fixture.address).await;
    complete_attachment(&mut socket, &fixture.controller).await;

    let target = runtime_control_start_target();
    let generation = start_through_runtime_control(&fixture, &mut socket, &target).await;
    let control = fixture.runtime_control.clone();
    let clear = tokio::spawn(async move {
        clear_conversation_session(
            &ClearAdmission,
            &control,
            UserId::parse("user-1").unwrap(),
            AgentId::parse(AGENT).unwrap(),
            ConversationId::parse(CONVERSATION).unwrap(),
        )
        .await
    });
    let command = next_json(&mut socket).await;
    assert_eq!(command["kind"], "runtime_clear_session");
    respond(&mut socket, &command, json!({ "cleared": true })).await;

    send_delivery_failed(&mut socket, generation, "driver_refused", 120).await;
    wait_for_counter(&fixture.settlement_calls, 1).await;
    tokio::task::yield_now().await;
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(11)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        clear.await.unwrap().unwrap(),
        Err(ClearSessionRefusal::Provider(
            ClearSessionProviderRefusal::Timeout
        ))
    );
    assert_eq!(
        fixture.clear_settlement_calls.load(Ordering::SeqCst),
        0,
        "delivery settlement is not runtime-exit evidence"
    );

    let mut db = open(&fixture.path).await;
    let claimed: Option<String> = sqlx::query_scalar(
        "SELECT claimed_by_daemon_instance_id
           FROM agent_runtime_slots
          WHERE agent_id=? AND conversation_id=?",
    )
    .bind(AGENT)
    .bind(CONVERSATION)
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(claimed.as_deref(), Some("instance-test"));
    let audits: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM messages
          WHERE conversation_id=? AND from_kind='system'
            AND json_extract(metadata, '$.subtype')='session_cleared'",
    )
    .bind(CONVERSATION)
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(audits, 0);

    socket.close(None).await.unwrap();
    fixture.controller.close().await;
    fixture.server.abort();
}

#[tokio::test]
async fn async_failed_events_distinguish_retryable_and_terminal_settlement() {
    let fixture = fixture("async-failed.sqlite").await;
    let mut socket = connect(fixture.address).await;
    complete_attachment(&mut socket, &fixture.controller).await;

    let effects = fixture.effects.clone();
    let first = tokio::spawn(async move {
        MainDelivery::deliver_to(
            &effects,
            &AgentId::parse(AGENT).unwrap(),
            &created_message(),
        )
        .await
    });
    let first_command = next_json(&mut socket).await;
    respond(
        &mut socket,
        &first_command,
        json!({ "disposition": "admitted", "pid": 7, "started_at": 100 }),
    )
    .await;
    first.await.unwrap().unwrap();
    let generation = first_command["params"]["runtime_generation"]
        .as_i64()
        .unwrap();
    send_delivery_failed(&mut socket, generation, "child_exit", 120).await;

    let mut db = open(&fixture.path).await;
    wait_for_inbox_state(&mut db, None, None).await;
    let effects = fixture.effects.clone();
    let retry = tokio::spawn(async move {
        MainDelivery::deliver_to(
            &effects,
            &AgentId::parse(AGENT).unwrap(),
            &created_message(),
        )
        .await
    });
    let retry_command = next_json(&mut socket).await;
    assert_eq!(retry_command["params"]["delivery_ids"], json!([1]));
    respond(
        &mut socket,
        &retry_command,
        json!({ "disposition": "admitted", "pid": 7, "started_at": 100 }),
    )
    .await;
    retry.await.unwrap().unwrap();
    send_delivery_failed(&mut socket, generation, "driver_refused", 130).await;
    wait_for_inbox_state(&mut db, Some(130), Some("driver_refused")).await;

    socket.close(None).await.unwrap();
    fixture.controller.close().await;
    fixture.server.abort();
}

async fn send_runtime_exit(socket: &mut Socket, generation: i64, intentional: bool, ts: i64) {
    socket
        .send(Message::Text(
            json!({
                "direction": "daemon_to_server",
                "type": "event",
                "kind": "runtime_exit",
                "runtime_scope": {
                    "kind": "conversation",
                    "conversation_id": CONVERSATION,
                    "agent_id": AGENT,
                },
                "conversation_id": CONVERSATION,
                "agent_id": AGENT,
                "runtime_generation": generation,
                "code": if intentional { Some(0) } else { Some(1) },
                "intentional": intentional,
                "ts": ts,
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
}

async fn send_delivery_consumed(socket: &mut Socket, generation: i64, ts: i64) {
    socket
        .send(Message::Text(
            json!({
                "direction": "daemon_to_server",
                "type": "event",
                "kind": "runtime_delivery_consumed",
                "runtime_scope": {
                    "kind": "conversation",
                    "conversation_id": CONVERSATION,
                    "agent_id": AGENT,
                },
                "conversation_id": CONVERSATION,
                "agent_id": AGENT,
                "runtime_generation": generation,
                "delivery_ids": [1],
                "ts": ts,
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
}

async fn send_delivery_failed(socket: &mut Socket, generation: i64, reason: &str, ts: i64) {
    socket
        .send(Message::Text(
            json!({
                "direction": "daemon_to_server",
                "type": "event",
                "kind": "runtime_delivery_failed",
                "runtime_scope": {
                    "kind": "conversation",
                    "conversation_id": CONVERSATION,
                    "agent_id": AGENT,
                },
                "conversation_id": CONVERSATION,
                "agent_id": AGENT,
                "runtime_generation": generation,
                "delivery_ids": [1],
                "reason": reason,
                "ts": ts,
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
}

async fn wait_for_slot_daemon(db: &mut SqliteConnection, expected: Option<&str>) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let daemon: Option<String> = sqlx::query_scalar(
                "SELECT claimed_by_daemon_instance_id
                   FROM agent_runtime_slots
                  WHERE agent_id = ? AND conversation_id = ?",
            )
            .bind(AGENT)
            .bind(CONVERSATION)
            .fetch_one(&mut *db)
            .await
            .unwrap();
            if daemon.as_deref() == expected {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

async fn wait_for_inbox_state(
    db: &mut SqliteConnection,
    failed_at: Option<i64>,
    failed_reason: Option<&str>,
) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let row = sqlx::query(
                "SELECT failed_at, failed_reason, in_flight_owner
                   FROM conversation_inboxes WHERE id = 1",
            )
            .fetch_one(&mut *db)
            .await
            .unwrap();
            if row.get::<Option<i64>, _>("failed_at") == failed_at
                && row.get::<Option<String>, _>("failed_reason").as_deref() == failed_reason
                && row.get::<Option<String>, _>("in_flight_owner").is_none()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

fn product_frame(kind: &str, generation: Option<i64>, fields: Value) -> Value {
    product_frame_for(
        kind,
        generation,
        fields,
        AGENT,
        CONVERSATION,
        "conversation",
    )
}

fn product_frame_for(
    kind: &str,
    generation: Option<i64>,
    fields: Value,
    agent: &str,
    conversation: &str,
    scope_kind: &str,
) -> Value {
    let runtime_scope = if scope_kind == "agent" {
        json!({ "kind": "agent", "agent_id": agent })
    } else {
        json!({ "kind": "conversation", "conversation_id": conversation, "agent_id": agent })
    };
    let mut frame = serde_json::Map::from_iter([
        ("direction".to_owned(), json!("daemon_to_server")),
        ("type".to_owned(), json!("event")),
        ("kind".to_owned(), json!(kind)),
        ("runtime_scope".to_owned(), runtime_scope),
        ("conversation_id".to_owned(), json!(conversation)),
        ("agent_id".to_owned(), json!(agent)),
    ]);
    if let Some(generation) = generation {
        frame.insert("runtime_generation".to_owned(), json!(generation));
    }
    frame.extend(fields.as_object().unwrap().clone());
    Value::Object(frame)
}

async fn send_product(socket: &mut Socket, frame: Value) {
    socket
        .send(Message::Text(frame.to_string().into()))
        .await
        .unwrap();
}

#[tokio::test]
async fn hydrated_product_frames_reach_durable_live_and_coordinator_owners_once() {
    let fixture = fixture("daemon-product-events.sqlite").await;
    let mut live = connect_conversation_side(fixture.address).await;
    let mut presence = connect_event_scope(fixture.address, "viewer:user-1").await;
    let mut daemon = connect(fixture.address).await;
    complete_attachment(&mut daemon, &fixture.controller).await;
    let seed_landed = next_json(&mut presence).await;
    assert_eq!(seed_landed["scope"], "viewer:user-1");
    assert_eq!(seed_landed["kind"], "host_connection_changed");
    assert_eq!(seed_landed["payload"], json!({ "host_id": "host-1" }));

    let effects = fixture.effects.clone();
    let start = tokio::spawn(async move {
        IdleStart::start_if_idle(
            &effects,
            &AgentId::parse(AGENT).unwrap(),
            &ConversationId::parse(CONVERSATION).unwrap(),
        )
        .await
    });
    let start_command = next_json(&mut daemon).await;
    let generation = start_command["params"]["runtime_generation"]
        .as_i64()
        .unwrap();
    respond(
        &mut daemon,
        &start_command,
        json!({ "started": true, "pid": 7, "started_at": 100 }),
    )
    .await;
    start.await.unwrap().unwrap();

    let run_state = json!({
        "runId": "run-1",
        "agentId": AGENT,
        "conversationId": CONVERSATION,
        "status": "thinking",
        "startedAt": 10,
        "updatedAt": 11,
        "lastActivityAt": 11,
    });
    send_product(
        &mut daemon,
        product_frame(
            "runtime_state",
            Some(generation),
            json!({ "state": run_state, "ts": 11 }),
        ),
    )
    .await;
    let state_live = next_json(&mut live).await;
    assert_eq!(state_live["kind"], "agent_activity");
    assert_eq!(state_live["payload"]["runId"], "run-1");

    send_product(
        &mut daemon,
        product_frame(
            "runtime_init_timeout",
            Some(generation),
            json!({
                "timeout_reason": "silence",
                "elapsed_ms": 10,
                "timeout_ms": 20,
                "retrying": true,
                "retry_attempt": 0,
                "max_retries": 2,
                "cooldown_until": 30,
                "ts": 12,
            }),
        ),
    )
    .await;
    assert_eq!(next_json(&mut live).await["kind"], "agent_init_timeout");
    send_product(
        &mut daemon,
        product_frame(
            "runtime_init_retrying",
            Some(generation),
            json!({
                "retry_attempt": 1,
                "max_retries": 2,
                "delay_ms": 5,
                "ts": 13,
            }),
        ),
    )
    .await;
    let retry_live = next_json(&mut live).await;
    assert_eq!(retry_live["kind"], "agent_status");
    assert_eq!(retry_live["payload"]["status"], "starting");
    send_product(
        &mut daemon,
        product_frame(
            "runtime_init_cooldown",
            Some(generation),
            json!({
                "cooldown_until": 30,
                "attempts_exhausted": 2,
                "ts": 13,
            }),
        ),
    )
    .await;
    let cooldown_live = next_json(&mut live).await;
    assert_eq!(cooldown_live["kind"], "agent_status");
    assert_eq!(cooldown_live["payload"]["status"], "crashed");
    assert_eq!(cooldown_live["payload"]["reason"], "init_cooldown");

    // A later daemon run-state snapshot is the positive recovery gate: it
    // clears the exhausted-init marker, so the final unintentional exit below
    // is again eligible for the pending-inbox respawn.
    send_product(
        &mut daemon,
        product_frame(
            "runtime_state",
            Some(generation),
            json!({ "state": run_state, "ts": 13 }),
        ),
    )
    .await;
    assert_eq!(next_json(&mut live).await["kind"], "agent_activity");

    for (kind, parsed, ts) in [
        (
            "runtime_parsed_event",
            json!({ "kind": "thinking", "text": "first " }),
            14,
        ),
        (
            "runtime_parsed_event",
            json!({ "kind": "thinking", "text": "second" }),
            15,
        ),
    ] {
        send_product(
            &mut daemon,
            product_frame(
                kind,
                Some(generation),
                json!({ "parsed": parsed, "ts": ts }),
            ),
        )
        .await;
        if ts == 14 {
            let presence_frame = next_json(&mut presence).await;
            assert_eq!(presence_frame["kind"], "agent_presence_changed");
            assert_eq!(presence_frame["payload"], json!({ "agent_id": AGENT }));
            assert_eq!(presence_frame["scope"], "viewer:user-1");
        }
        assert_eq!(next_json(&mut live).await["kind"], "agent_event");
        assert_eq!(next_json(&mut live).await["kind"], "conversation_activity");
    }
    let mut db = open(&fixture.path).await;
    let thinking = sqlx::query(
        "SELECT id, payload FROM conversation_events
          WHERE conversation_id = ? AND agent_id = ? AND kind = 'thinking'",
    )
    .bind(CONVERSATION)
    .bind(AGENT)
    .fetch_all(&mut db)
    .await
    .unwrap();
    assert_eq!(thinking.len(), 1);
    assert_eq!(
        serde_json::from_str::<Value>(thinking[0].get("payload")).unwrap()["text"],
        "first second"
    );

    let oversized = "x".repeat(700);
    send_product(
        &mut daemon,
        product_frame(
            "runtime_parsed_event",
            Some(generation),
            json!({
                "parsed": {
                    "kind": "tool_call",
                    "name": "Bash",
                    "input": { "command": oversized },
                    "toolCallId": "tool-1",
                },
                "ts": 16,
            }),
        ),
    )
    .await;
    let tool_live = next_json(&mut live).await;
    assert_eq!(tool_live["kind"], "agent_event");
    assert!(
        tool_live["payload"]["event"]["input"]["command"]
            .as_str()
            .unwrap()
            .contains("<truncated 200 chars>")
    );
    let tool_activity = next_json(&mut live).await;
    assert_eq!(tool_activity["kind"], "conversation_activity");
    let tool_id = tool_activity["payload"]["id"].as_i64().unwrap();
    sqlx::query("UPDATE agents SET last_activity_at = NULL WHERE id = ?")
        .bind(AGENT)
        .execute(&mut db)
        .await
        .unwrap();
    send_product(
        &mut daemon,
        product_frame(
            "runtime_parsed_event",
            Some(generation),
            json!({
                "parsed": {
                    "kind": "tool_call_input",
                    "toolCallId": "tool-1",
                    "input": { "command": "refined" },
                },
                "ts": 17,
            }),
        ),
    )
    .await;
    assert_eq!(
        next_json(&mut presence).await["kind"],
        "agent_presence_changed"
    );
    assert_eq!(next_json(&mut live).await["kind"], "agent_event");
    let refined = next_json(&mut live).await;
    assert_eq!(refined["kind"], "conversation_activity");
    assert_eq!(refined["payload"]["id"], tool_id);
    assert_eq!(refined["payload"]["payload"]["input"]["command"], "refined");

    send_product(
        &mut daemon,
        product_frame(
            "runtime_parsed_event",
            Some(generation),
            json!({
                "parsed": {
                    "kind": "input_lifecycle",
                    "commandUuid": "command-1",
                    "state": "started",
                },
                "ts": 18,
            }),
        ),
    )
    .await;
    sqlx::query("UPDATE agents SET last_activity_at = NULL WHERE id = ?")
        .bind(AGENT)
        .execute(&mut db)
        .await
        .unwrap();
    send_product(
        &mut daemon,
        product_frame(
            "runtime_parsed_event",
            Some(generation),
            json!({
                "parsed": { "kind": "unknown", "raw": "future event" },
                "ts": 19,
            }),
        ),
    )
    .await;
    assert_eq!(
        next_json(&mut presence).await["kind"],
        "agent_presence_changed"
    );
    assert_eq!(next_json(&mut live).await["kind"], "agent_event");
    send_product(
        &mut daemon,
        product_frame(
            "runtime_message_stale",
            Some(generation),
            json!({ "ts": 20 }),
        ),
    )
    .await;
    let after_diagnostic = next_json(&mut live).await;
    assert_eq!(after_diagnostic["kind"], "conversation_activity");
    assert_eq!(after_diagnostic["payload"]["kind"], "message_stale");

    send_product(
        &mut daemon,
        product_frame(
            "runtime_ready_for_drain",
            Some(generation),
            json!({ "ts": 21 }),
        ),
    )
    .await;
    let ready_live = next_json(&mut live).await;
    assert_eq!(ready_live["kind"], "agent_status");
    assert_eq!(ready_live["payload"]["status"], "active");
    let drain = next_json(&mut daemon).await;
    assert_eq!(drain["kind"], "runtime_deliver");
    respond(
        &mut daemon,
        &drain,
        json!({ "disposition": "admitted", "pid": 7, "started_at": 100 }),
    )
    .await;

    send_runtime_exit(&mut daemon, generation, false, 22).await;
    let exit_live = next_json(&mut live).await;
    assert_eq!(exit_live["kind"], "agent_status");
    assert_eq!(exit_live["payload"]["status"], "crashed");
    assert_eq!(exit_live["payload"]["run_state"]["runId"], "run-1");
    let respawn = next_json(&mut daemon).await;
    assert_eq!(respawn["kind"], "runtime_deliver");
    assert_eq!(respawn["params"]["runtime_generation"], generation + 1);
    respond(
        &mut daemon,
        &respawn,
        json!({ "disposition": "admitted", "pid": 8, "started_at": 200 }),
    )
    .await;

    let statuses =
        sqlx::query_scalar::<_, String>("SELECT event_kind FROM agent_runtime_events ORDER BY id")
            .fetch_all(&mut db)
            .await
            .unwrap();
    assert!(statuses.iter().any(|kind| kind == "projection_updated"));
    assert!(statuses.iter().any(|kind| kind == "generation_finalized"));

    live.close(None).await.unwrap();
    presence.close(None).await.unwrap();
    daemon.close(None).await.unwrap();
    fixture.event_controller.close().await;
    fixture.controller.close().await;
    fixture.server.abort();
}

#[tokio::test]
async fn hydrated_run_state_is_the_cold_authority_and_session_init_drains_once() {
    let fixture = fixture("daemon-hydrated-run-state.sqlite").await;
    let mut first_daemon = connect(fixture.address).await;
    complete_attachment(&mut first_daemon, &fixture.controller).await;
    let effects = fixture.effects.clone();
    let start = tokio::spawn(async move {
        IdleStart::start_if_idle(
            &effects,
            &AgentId::parse(AGENT).unwrap(),
            &ConversationId::parse(CONVERSATION).unwrap(),
        )
        .await
    });
    let start_command = next_json(&mut first_daemon).await;
    let generation = start_command["params"]["runtime_generation"]
        .as_i64()
        .unwrap();
    respond(
        &mut first_daemon,
        &start_command,
        json!({ "started": true, "pid": 7, "started_at": 100 }),
    )
    .await;
    start.await.unwrap().unwrap();
    first_daemon.close(None).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while fixture.controller.active_connection_count() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    let wire_fixture: Value = serde_json::from_str(include_str!(
        "../../../../../packages/host/tests/fixtures/outbound-daemon-events.json"
    ))
    .unwrap();
    let run_state_case = wire_fixture["runState"]["valid"]
        .as_array()
        .unwrap()
        .iter()
        .find(|test_case| test_case["name"] == "authenticated_hydrate_and_egress")
        .unwrap();
    let hydrated_run_state = run_state_case["state"].clone();
    let expected_live_run_state = run_state_case["canonical"].clone();
    let mut live = connect_conversation_side(fixture.address).await;
    let mut daemon = connect(fixture.address).await;
    send_ready(&mut daemon, OUTBOUND_PROTOCOL_VERSION).await;
    let status = next_json(&mut daemon).await;
    assert_eq!(status["kind"], "runtime_status");
    respond(
        &mut daemon,
        &status,
        json!({
            "runtimes": [{
                "runtime_scope": {
                    "kind": "conversation",
                    "conversation_id": CONVERSATION,
                    "agent_id": AGENT,
                },
                "conversation_id": CONVERSATION,
                "agent_id": AGENT,
                "runtime_generation": generation,
                "pid": 7,
                "phase": "ready",
                "run_state": hydrated_run_state,
                "started_at": 100,
            }]
        }),
    )
    .await;
    wait_for_attachment(&fixture.controller).await;
    answer_status_seed(&mut daemon, &fixture.controller, "host-1").await;

    send_product(
        &mut daemon,
        product_frame(
            "runtime_parsed_event",
            Some(generation),
            json!({
                "parsed": { "kind": "session_init", "sessionId": "cli-session-1" },
                "ts": 20,
            }),
        ),
    )
    .await;
    assert_eq!(next_json(&mut live).await["kind"], "agent_event");
    let activity = next_json(&mut live).await;
    assert_eq!(activity["kind"], "conversation_activity");
    assert_eq!(activity["payload"]["kind"], "session_init");
    let projected = next_json(&mut live).await;
    assert_eq!(projected["kind"], "agent_status");
    assert_eq!(projected["payload"]["status"], "active");
    assert_eq!(projected["payload"]["cli_session_id"], "cli-session-1");

    let drain = next_json(&mut daemon).await;
    assert_eq!(drain["kind"], "runtime_deliver");
    assert_eq!(drain["params"]["runtime_generation"], generation);
    respond(
        &mut daemon,
        &drain,
        json!({ "disposition": "admitted", "pid": 7, "started_at": 100 }),
    )
    .await;

    send_runtime_exit(&mut daemon, generation, true, 30).await;
    let exit = next_json(&mut live).await;
    assert_eq!(exit["kind"], "agent_status");
    assert_eq!(exit["payload"]["status"], "idle");
    assert_eq!(exit["payload"]["run_state"], expected_live_run_state);

    let mut db = open(&fixture.path).await;
    let projections = sqlx::query(
        "SELECT event_kind, cli_session_id
           FROM agent_runtime_events
          ORDER BY id",
    )
    .fetch_all(&mut db)
    .await
    .unwrap();
    assert!(projections.iter().any(|row| {
        row.get::<String, _>("event_kind") == "projection_updated"
            && row.get::<Option<String>, _>("cli_session_id").as_deref() == Some("cli-session-1")
    }));
    assert!(
        projections
            .iter()
            .any(|row| row.get::<String, _>("event_kind") == "generation_finalized")
    );

    live.close(None).await.unwrap();
    daemon.close(None).await.unwrap();
    fixture.event_controller.close().await;
    fixture.controller.close().await;
    fixture.server.abort();
}

#[tokio::test]
async fn session_init_activity_failure_rolls_back_status_presence_and_live_drain() {
    let fixture = fixture("daemon-session-init-root-uow.sqlite").await;
    let mut daemon = connect(fixture.address).await;
    complete_attachment(&mut daemon, &fixture.controller).await;
    let effects = fixture.effects.clone();
    let start = tokio::spawn(async move {
        IdleStart::start_if_idle(
            &effects,
            &AgentId::parse(AGENT).unwrap(),
            &ConversationId::parse(CONVERSATION).unwrap(),
        )
        .await
    });
    let start_command = next_json(&mut daemon).await;
    let generation = start_command["params"]["runtime_generation"]
        .as_i64()
        .unwrap();
    respond(
        &mut daemon,
        &start_command,
        json!({ "started": true, "pid": 7, "started_at": 100 }),
    )
    .await;
    start.await.unwrap().unwrap();

    let (mut before, before_snapshot) = connect_event_scope_with_ack(
        fixture.address,
        &format!("conversation-side:{CONVERSATION}"),
    )
    .await;
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
    send_product(
        &mut daemon,
        product_frame(
            "runtime_parsed_event",
            Some(generation),
            json!({
                "parsed": { "kind": "session_init", "sessionId": "must-rollback" },
                "ts": 20,
            }),
        ),
    )
    .await;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while fixture.controller.active_connection_count() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
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
    assert_eq!(row.get::<String, _>("observed_status"), "starting");
    assert!(row.get::<Option<String>, _>("cli_session_id").is_none());
    assert!(row.get::<Option<i64>, _>("last_activity_at").is_none());
    assert_eq!(row.get::<i64, _>("activities"), 0);
    let (mut after, after_snapshot) = connect_event_scope_with_ack(
        fixture.address,
        &format!("conversation-side:{CONVERSATION}"),
    )
    .await;
    assert_eq!(after_snapshot["seq"], before_snapshot["seq"]);

    before.close(None).await.unwrap();
    after.close(None).await.unwrap();
    fixture.event_controller.close().await;
    fixture.controller.close().await;
    fixture.server.abort();
}

#[tokio::test]
async fn stale_resume_fatal_clears_cli_session_before_successor_start() {
    let fixture = fixture("daemon-stale-resume-clear.sqlite").await;
    let mut live = connect_conversation_side(fixture.address).await;
    let mut daemon = connect(fixture.address).await;
    complete_attachment(&mut daemon, &fixture.controller).await;
    let effects = fixture.effects.clone();
    let start = tokio::spawn(async move {
        IdleStart::start_if_idle(
            &effects,
            &AgentId::parse(AGENT).unwrap(),
            &ConversationId::parse(CONVERSATION).unwrap(),
        )
        .await
    });
    let start_command = next_json(&mut daemon).await;
    let generation = start_command["params"]["runtime_generation"]
        .as_i64()
        .unwrap();
    respond(
        &mut daemon,
        &start_command,
        json!({ "started": true, "pid": 7, "started_at": 100 }),
    )
    .await;
    start.await.unwrap().unwrap();
    send_product(
        &mut daemon,
        product_frame(
            "runtime_parsed_event",
            Some(generation),
            json!({
                "parsed": { "kind": "session_init", "sessionId": "stale-cli-session" },
                "ts": 20,
            }),
        ),
    )
    .await;
    for expected in ["agent_event", "conversation_activity", "agent_status"] {
        assert_eq!(next_json(&mut live).await["kind"], expected);
    }
    let drain = next_json(&mut daemon).await;
    respond(
        &mut daemon,
        &drain,
        json!({ "disposition": "admitted", "pid": 7, "started_at": 100 }),
    )
    .await;
    send_product(
        &mut daemon,
        product_frame(
            "runtime_parsed_event",
            Some(generation),
            json!({
                "parsed": {
                    "kind": "error",
                    "message": "resume target vanished",
                    "code": "thread_not_found",
                    "fatal": true
                },
                "ts": 21,
            }),
        ),
    )
    .await;
    assert_eq!(next_json(&mut live).await["kind"], "agent_event");
    assert_eq!(next_json(&mut live).await["kind"], "conversation_activity");
    send_runtime_exit(&mut daemon, generation, false, 22).await;
    let exited = next_json(&mut live).await;
    assert_eq!(exited["kind"], "agent_status");
    assert_eq!(exited["payload"]["status"], "crashed");
    assert!(exited["payload"]["cli_session_id"].is_null());
    let mut db = open(&fixture.path).await;
    assert!(
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT cli_session_id FROM agent_runtime_slots WHERE conversation_id = ? AND agent_id = ?",
        )
        .bind(CONVERSATION)
        .bind(AGENT)
        .fetch_one(&mut db)
        .await
        .unwrap()
        .is_none()
    );
    add_inbox(&mut db, "message-successor", 50).await;
    let effects = fixture.effects.clone();
    let successor = tokio::spawn(async move {
        IdleStart::start_if_idle(
            &effects,
            &AgentId::parse(AGENT).unwrap(),
            &ConversationId::parse(CONVERSATION).unwrap(),
        )
        .await
    });
    let successor_command = next_json(&mut daemon).await;
    assert_eq!(successor_command["kind"], "runtime_start");
    // `runtime_start` nests the spawn options under `options`; the `spawn_options` key
    // belongs to `runtime_deliver`. Asserting the runtime word FIRST keeps the
    // cli-session assertion below honest — a path that does not resolve reads as JSON
    // null, which `is_null()` accepts without having read anything.
    //
    // The plan carries the TYPED runtime now, so this word is the domain's `as_str()`
    // rather than the stored column: it must still be the seeded token, byte for byte.
    assert_eq!(
        successor_command["params"]["options"]["driver"]["kind"],
        "codex"
    );
    assert!(successor_command["params"]["options"]["driver"]["cli_session_id"].is_null());
    respond(
        &mut daemon,
        &successor_command,
        json!({ "started": true, "pid": 8, "started_at": 200 }),
    )
    .await;
    successor.await.unwrap().unwrap();

    live.close(None).await.unwrap();
    daemon.close(None).await.unwrap();
    fixture.event_controller.close().await;
    fixture.controller.close().await;
    fixture.server.abort();
}

#[tokio::test]
async fn agent_slot_activity_is_shared_across_channels_and_dm_without_source_content() {
    const AGGREGATE_AGENT: &str = "agent-aggregate";
    const SECOND: &str = "conversation0002";
    const PAIR: &str = "activity-dm";
    const SECRET: &str = "secret-origin";
    let fixture = fixture("daemon-agent-slot-status.sqlite").await;
    let mut db = open(&fixture.path).await;
    sqlx::raw_sql(
        "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, display_name, created_by_user_id, session_scope)
         VALUES ('agent-aggregate', 'agent-aggregate', 'codex', 'host-1', 1, 1, 'Aggregate Agent', 'user-1', 'agent');
         INSERT INTO workspace_members (workspace_id, agent_id, joined_at) VALUES ('workspace-1', 'agent-aggregate', 1);
         INSERT INTO conversation_agents (conversation_id, agent_id, joined_at) VALUES ('conversation0001', 'agent-aggregate', 1);
         INSERT INTO conversations (id, workspace_id, placement_kind, roster_policy, title, title_key, status, created_at, updated_at)
         VALUES ('conversation0002', 'workspace-1', 'workspace', 'fixed', 'Second', 'second', 'active', 1, 1);
         INSERT INTO conversation_agents (conversation_id, agent_id, joined_at) VALUES ('conversation0002', 'agent-aggregate', 1);
         INSERT INTO conversation_users (conversation_id, user_id, joined_at) VALUES ('conversation0002', 'user-1', 1);
         INSERT INTO conversations(id,placement_kind,roster_policy,title,endpoint_a_kind,endpoint_a_id,endpoint_b_kind,endpoint_b_id,status,visibility,created_at,updated_at)
         VALUES('activity-dm','direct_pair','fixed','','agent','agent-aggregate','human','user-1','idle','private',1,1);
         INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,status,visibility,created_at,updated_at)
         VALUES('secret-origin','workspace-1','workspace','open','Secret','secret','idle','private',1,1);
         INSERT INTO conversation_agents(conversation_id,agent_id,joined_at) VALUES('secret-origin','agent-aggregate',1);
         INSERT INTO messages (id, conversation_id, from_kind, from_user_id, content, seq, created_at)
         VALUES ('aggregate-message', 'conversation0001', 'user', 'user-1', 'wake aggregate', 2, 2);
         INSERT INTO conversation_inboxes (conversation_id, agent_id, message_id, created_at, delivery_kind)
         VALUES ('conversation0001', 'agent-aggregate', 'aggregate-message', 2, 'turn');",
    )
    .execute(&mut db)
    .await
    .unwrap();
    let mut origin = connect_event_scope(
        fixture.address,
        &format!("conversation-side:{CONVERSATION}"),
    )
    .await;
    let mut second =
        connect_event_scope(fixture.address, &format!("conversation-side:{SECOND}")).await;
    let mut direct =
        connect_event_scope(fixture.address, &format!("conversation-side:{PAIR}")).await;
    let mut daemon = connect(fixture.address).await;
    complete_attachment(&mut daemon, &fixture.controller).await;
    let effects = fixture.effects.clone();
    let start = tokio::spawn(async move {
        IdleStart::start_if_idle(
            &effects,
            &AgentId::parse(AGGREGATE_AGENT).unwrap(),
            &ConversationId::parse(CONVERSATION).unwrap(),
        )
        .await
    });
    let start_command = next_json(&mut daemon).await;
    let generation = start_command["params"]["runtime_generation"]
        .as_i64()
        .unwrap();
    respond(
        &mut daemon,
        &start_command,
        json!({ "started": true, "pid": 8, "started_at": 100 }),
    )
    .await;
    start.await.unwrap().unwrap();
    send_product(
        &mut daemon,
        product_frame_for(
            "runtime_state",
            Some(generation),
            json!({
                "state": {
                    "runId": "aggregate-run",
                    "agentId": AGGREGATE_AGENT,
                    "conversationId": CONVERSATION,
                    "status": "thinking",
                    "startedAt": 10,
                    "updatedAt": 11,
                    "lastActivityAt": 11,
                    "activitySnippet": "private source text",
                    "activitySnippetKind": "thinking",
                    "detail": "private source location",
                    "toolCallCounts": {"shell": 2}
                },
                "ts": 11
            }),
            AGGREGATE_AGENT,
            CONVERSATION,
            "agent",
        ),
    )
    .await;
    for (socket, target) in [
        (&mut origin, CONVERSATION),
        (&mut second, SECOND),
        (&mut direct, PAIR),
    ] {
        let event = next_json(socket).await;
        assert_eq!(event["kind"], "agent_activity");
        assert_eq!(event["payload"]["runId"], "aggregate-run");
        assert_eq!(event["payload"]["conversationId"], target);
        assert_eq!(event["payload"]["toolCallCounts"]["shell"], 2);
        assert!(event["payload"].get("activitySnippet").is_none());
        assert!(event["payload"].get("detail").is_none());
        let (status, page) = management_request(
            &fixture.run_states,
            "GET",
            &format!("/api/channels/{target}/agent-run-states"),
            "",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{page}");
        assert_eq!(page["states"], json!([event["payload"].clone()]));
    }
    let (denied, _) = management_request(
        &fixture.run_states,
        "GET",
        &format!("/api/channels/{SECRET}/agent-run-states"),
        "",
    )
    .await;
    assert_eq!(denied, StatusCode::FORBIDDEN);
    // A focus switch to a source this viewer cannot read keeps one public run.
    send_product(&mut daemon, product_frame_for("runtime_state", Some(generation), json!({
        "state": {"runId":"aggregate-run","agentId":AGGREGATE_AGENT,"conversationId":SECRET,
          "status":"tool_running","startedAt":10,"updatedAt":12,"lastActivityAt":12,
          "activitySnippet":"hidden tool input", "activitySnippetKind":"text", "detail":"secret-origin"}, "ts":12
    }), AGGREGATE_AGENT, SECRET, "agent")).await;
    for (socket, target) in [
        (&mut origin, CONVERSATION),
        (&mut second, SECOND),
        (&mut direct, PAIR),
    ] {
        let event = next_json(socket).await;
        assert_eq!(event["kind"], "agent_activity");
        assert_eq!(event["payload"]["status"], "tool_running");
        assert_eq!(event["payload"]["runId"], "aggregate-run");
        assert!(!event.to_string().contains(SECRET));
        assert!(!event.to_string().contains("hidden tool input"));
        let (status, page) = management_request(
            &fixture.run_states,
            "GET",
            &format!("/api/channels/{target}/agent-run-states"),
            "",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(page["states"], json!([event["payload"].clone()]));
    }
    // An old generation event neither overwrites the snapshot nor emits a frame.
    send_product(&mut daemon, product_frame_for("runtime_state", Some(generation + 1), json!({
        "state":{"runId":"wrong-generation","agentId":AGGREGATE_AGENT,"conversationId":CONVERSATION,
        "status":"thinking","startedAt":1,"updatedAt":99,"lastActivityAt":99},"ts":99
    }), AGGREGATE_AGENT, CONVERSATION, "agent")).await;

    send_product(
        &mut daemon,
        product_frame_for(
            "runtime_exit",
            Some(generation),
            json!({ "code": 0, "intentional": true, "ts": 12 }),
            AGGREGATE_AGENT,
            CONVERSATION,
            "agent",
        ),
    )
    .await;
    for socket in [&mut origin, &mut second, &mut direct] {
        let terminal = next_json(socket).await;
        assert_eq!(terminal["kind"], "agent_activity");
        assert_eq!(terminal["payload"]["terminalReason"], "stopped");
        assert_eq!(terminal["payload"]["status"], "idle");
        assert!(terminal["payload"].get("detail").is_none());
    }
    let origin_status = next_json(&mut origin).await;
    let second_status = next_json(&mut second).await;
    assert_eq!(origin_status["kind"], "agent_status");
    assert_eq!(second_status["kind"], "agent_status");
    assert_eq!(
        origin_status["payload"]["run_state"]["runId"],
        "aggregate-run"
    );
    assert!(second_status["payload"].get("run_state").is_none());
    assert!(!origin_status.to_string().contains(SECRET));
    assert!(!origin_status.to_string().contains("hidden tool input"));
    assert_eq!(
        origin_status["payload"]["run_state"]["terminalReason"],
        "stopped"
    );

    assert_eq!(next_json(&mut direct).await["kind"], "agent_status");
    for target in [CONVERSATION, SECOND, PAIR] {
        let (status, page) = management_request(
            &fixture.run_states,
            "GET",
            &format!("/api/channels/{target}/agent-run-states"),
            "",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            page["states"].as_array().unwrap().is_empty(),
            "terminal run removed from shared owner"
        );
    }
    let effects = fixture.effects.clone();
    let restart = tokio::spawn(async move {
        IdleStart::start_if_idle(
            &effects,
            &AgentId::parse(AGGREGATE_AGENT).unwrap(),
            &ConversationId::parse(CONVERSATION).unwrap(),
        )
        .await
    });
    let restart_command = next_json(&mut daemon).await;
    let successor = restart_command["params"]["runtime_generation"]
        .as_i64()
        .unwrap();
    assert_eq!(successor, generation + 1);
    respond(
        &mut daemon,
        &restart_command,
        json!({"started":true,"pid":9,"started_at":200}),
    )
    .await;
    restart.await.unwrap().unwrap();
    for (wire_generation, run_id) in [(generation, "old-run"), (successor, "successor-run")] {
        send_product(&mut daemon, product_frame_for("runtime_state", Some(wire_generation), json!({
            "state":{"runId":run_id,"agentId":AGGREGATE_AGENT,"conversationId":CONVERSATION,
            "status":"thinking","startedAt":200,"updatedAt":201,"lastActivityAt":201},"ts":201
        }), AGGREGATE_AGENT, CONVERSATION, "agent")).await;
    }
    for (socket, target) in [
        (&mut origin, CONVERSATION),
        (&mut second, SECOND),
        (&mut direct, PAIR),
    ] {
        let current = next_json(socket).await;
        assert_eq!(current["kind"], "agent_activity");
        assert_eq!(
            current["payload"]["runId"], "successor-run",
            "old generation has no projection"
        );
        let (_, page) = management_request(
            &fixture.run_states,
            "GET",
            &format!("/api/channels/{target}/agent-run-states"),
            "",
        )
        .await;
        assert_eq!(page["states"], json!([current["payload"].clone()]));
    }
    sqlx::query("DELETE FROM conversation_agents WHERE conversation_id=? AND agent_id=?")
        .bind(SECOND)
        .bind(AGGREGATE_AGENT)
        .execute(&mut db)
        .await
        .unwrap();
    let (_, page) = management_request(
        &fixture.run_states,
        "GET",
        &format!("/api/channels/{SECOND}/agent-run-states"),
        "",
    )
    .await;
    assert!(
        page["states"].as_array().unwrap().is_empty(),
        "removed participation cannot read a cached shared run"
    );
    send_product(&mut daemon, product_frame_for("runtime_state", Some(successor), json!({
        "state":{"runId":"successor-run","agentId":AGGREGATE_AGENT,"conversationId":CONVERSATION,
        "status":"waiting","startedAt":200,"updatedAt":202,"lastActivityAt":202},"ts":202
    }), AGGREGATE_AGENT, CONVERSATION, "agent")).await;
    assert_eq!(next_json(&mut origin).await["payload"]["status"], "waiting");
    assert_eq!(next_json(&mut direct).await["payload"]["status"], "waiting");
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), second.next())
            .await
            .is_err(),
        "removed target receives no shared event"
    );
    direct.close(None).await.unwrap();
    origin.close(None).await.unwrap();
    second.close(None).await.unwrap();
    daemon.close(None).await.unwrap();
    fixture.event_controller.close().await;
    fixture.controller.close().await;
    fixture.server.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn output_arms_are_typed_diagnostics_and_never_business_projection() {
    let fixture = fixture("daemon-output-diagnostics.sqlite").await;
    let mut daemon = connect(fixture.address).await;
    complete_attachment(&mut daemon, &fixture.controller).await;
    let effects = fixture.effects.clone();
    let start = tokio::spawn(async move {
        IdleStart::start_if_idle(
            &effects,
            &AgentId::parse(AGENT).unwrap(),
            &ConversationId::parse(CONVERSATION).unwrap(),
        )
        .await
    });
    let start_command = next_json(&mut daemon).await;
    respond(
        &mut daemon,
        &start_command,
        json!({ "started": true, "pid": 7, "started_at": 100 }),
    )
    .await;
    start.await.unwrap().unwrap();

    let logs = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .without_time()
        .with_target(false)
        .with_writer(logs.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    for frame in [
        product_frame(
            "runtime_output",
            None,
            json!({ "stream": "stdout", "chunk": "hello", "ts": 30 }),
        ),
        product_frame(
            "runtime_output_dropped",
            None,
            json!({ "dropped_count": 2, "bytes_dropped": 100, "since_ts": 30 }),
        ),
        product_frame("runtime_output_paused", None, json!({})),
        product_frame("runtime_output_resumed", None, json!({})),
    ] {
        send_product(&mut daemon, frame).await;
    }
    daemon
        .send(Message::Text(
            json!({
                "direction": "daemon_to_server",
                "type": "heartbeat",
                "kind": "ping",
                "ts": 31,
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
    assert_eq!(next_json(&mut daemon).await["kind"], "pong");

    let diagnostics = logs
        .events()
        .into_iter()
        .filter_map(|event| event["fields"]["runtime_event"].as_str().map(str::to_owned))
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(
        diagnostics,
        std::collections::HashSet::from([
            "runtime_output".to_owned(),
            "runtime_output_dropped".to_owned(),
            "runtime_output_paused".to_owned(),
            "runtime_output_resumed".to_owned(),
        ])
    );
    let mut db = open(&fixture.path).await;
    let activity_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM conversation_events")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(activity_count, 0);
    let status: String = sqlx::query_scalar(
        "SELECT observed_status FROM agent_runtime_slots
          WHERE conversation_id = ? AND agent_id = ?",
    )
    .bind(CONVERSATION)
    .bind(AGENT)
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(status, "starting");

    daemon.close(None).await.unwrap();
    fixture.event_controller.close().await;
    fixture.controller.close().await;
    fixture.server.abort();
}

#[tokio::test]
async fn stale_generation_events_cannot_change_or_publish_successor_state() {
    let fixture = fixture("daemon-stale-successor.sqlite").await;
    let mut live = connect_conversation_side(fixture.address).await;
    let mut daemon = connect(fixture.address).await;
    complete_attachment(&mut daemon, &fixture.controller).await;

    let effects = fixture.effects.clone();
    let first = tokio::spawn(async move {
        IdleStart::start_if_idle(
            &effects,
            &AgentId::parse(AGENT).unwrap(),
            &ConversationId::parse(CONVERSATION).unwrap(),
        )
        .await
    });
    let first_command = next_json(&mut daemon).await;
    respond(
        &mut daemon,
        &first_command,
        json!({ "started": true, "pid": 7, "started_at": 100 }),
    )
    .await;
    first.await.unwrap().unwrap();
    send_runtime_exit(&mut daemon, 1, true, 40).await;
    assert_eq!(next_json(&mut live).await["kind"], "agent_status");
    let mut db = open(&fixture.path).await;
    wait_for_slot_daemon(&mut db, None).await;

    let effects = fixture.effects.clone();
    let successor = tokio::spawn(async move {
        IdleStart::start_if_idle(
            &effects,
            &AgentId::parse(AGENT).unwrap(),
            &ConversationId::parse(CONVERSATION).unwrap(),
        )
        .await
    });
    let successor_command = next_json(&mut daemon).await;
    assert_eq!(successor_command["params"]["runtime_generation"], 2);
    respond(
        &mut daemon,
        &successor_command,
        json!({ "started": true, "pid": 8, "started_at": 200 }),
    )
    .await;
    successor.await.unwrap().unwrap();

    for (generation, run_id, ts) in [(1, "stale-run", 41), (2, "successor-run", 42)] {
        send_product(
            &mut daemon,
            product_frame(
                "runtime_state",
                Some(generation),
                json!({
                    "state": {
                        "runId": run_id,
                        "agentId": AGENT,
                        "conversationId": CONVERSATION,
                        "status": "thinking",
                        "startedAt": ts,
                        "updatedAt": ts,
                        "lastActivityAt": ts,
                    },
                    "ts": ts,
                }),
            ),
        )
        .await;
    }
    let only_state = next_json(&mut live).await;
    assert_eq!(only_state["kind"], "agent_activity");
    assert_eq!(only_state["payload"]["runId"], "successor-run");
    let SeedRead::Seeded(status) = fixture
        .controller
        .host_status_seeds()
        .read(&host_id("host-1"))
        .unwrap()
    else {
        panic!("status seed is available")
    };
    assert_eq!(
        status.active_agents,
        std::collections::HashSet::from([AgentId::parse(AGENT).unwrap()])
    );
    assert_no_daemon_frame(&mut daemon).await;

    for (generation, retry_attempt, ts) in [(1, 1, 43), (2, 2, 44)] {
        send_product(
            &mut daemon,
            product_frame(
                "runtime_init_retrying",
                Some(generation),
                json!({
                    "retry_attempt": retry_attempt,
                    "max_retries": 3,
                    "delay_ms": 5,
                    "ts": ts,
                }),
            ),
        )
        .await;
    }
    let only_status = next_json(&mut live).await;
    assert_eq!(only_status["kind"], "agent_status");
    assert_eq!(only_status["payload"]["retry_attempt"], 2);
    let projected_generation: i64 = sqlx::query_scalar(
        "SELECT generation FROM agent_runtime_slots
          WHERE conversation_id = ? AND agent_id = ?",
    )
    .bind(CONVERSATION)
    .bind(AGENT)
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(projected_generation, 2);

    send_runtime_exit(&mut daemon, 2, true, 45).await;
    assert_eq!(next_json(&mut live).await["kind"], "agent_status");
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Ok(SeedRead::Seeded(status)) = fixture
                .controller
                .host_status_seeds()
                .read(&host_id("host-1"))
                && status.active_agents.is_empty()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_no_daemon_frame(&mut daemon).await;

    live.close(None).await.unwrap();
    daemon.close(None).await.unwrap();
    fixture.event_controller.close().await;
    fixture.controller.close().await;
    fixture.server.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn durable_owner_failure_closes_once_with_a_safe_wire_and_full_operator_chain() {
    let fixture = fixture("daemon-owner-failure.sqlite").await;
    let mut daemon = connect(fixture.address).await;
    complete_attachment(&mut daemon, &fixture.controller).await;
    let effects = fixture.effects.clone();
    let start = tokio::spawn(async move {
        IdleStart::start_if_idle(
            &effects,
            &AgentId::parse(AGENT).unwrap(),
            &ConversationId::parse(CONVERSATION).unwrap(),
        )
        .await
    });
    let command = next_json(&mut daemon).await;
    let generation = command["params"]["runtime_generation"].as_i64().unwrap();
    respond(
        &mut daemon,
        &command,
        json!({ "started": true, "pid": 7, "started_at": 100 }),
    )
    .await;
    start.await.unwrap().unwrap();

    let mut db = open(&fixture.path).await;
    sqlx::raw_sql("DROP TABLE agent_runtime_events")
        .execute(&mut db)
        .await
        .unwrap();
    let logs = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .without_time()
        .with_target(false)
        .with_writer(logs.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    send_product(
        &mut daemon,
        product_frame(
            "runtime_init_retrying",
            Some(generation),
            json!({
                "retry_attempt": 1,
                "max_retries": 2,
                "delay_ms": 5,
                "ts": 300,
            }),
        ),
    )
    .await;
    let close = daemon.next().await.unwrap().unwrap();
    let Message::Close(Some(close)) = close else {
        panic!("durable owner failure must close the daemon connection")
    };
    assert_eq!(close.code, CloseCode::Error);
    assert_eq!(close.reason, "runtime_event_owner_failed");

    let owner_failures = logs
        .events()
        .into_iter()
        .filter(|event| event["fields"]["message"] == "daemon runtime event owner failed")
        .collect::<Vec<_>>();
    assert_eq!(owner_failures.len(), 1);
    let chain = owner_failures[0]["fields"]["error_chain"].as_str().unwrap();
    assert!(chain.contains("daemon runtime event persistence failed"));
    assert!(chain.contains("no such table: agent_runtime_events"));
    assert!(!chain.contains(TOKEN));

    fixture.event_controller.close().await;
    fixture.controller.close().await;
    fixture.server.abort();
}

#[tokio::test]
async fn daemon_shutdown_clears_only_its_process_projection_before_reattach() {
    let fixture = fixture("daemon-shutdown-projection.sqlite").await;
    let mut live = connect_conversation_side(fixture.address).await;
    let mut first_daemon = connect(fixture.address).await;
    complete_attachment(&mut first_daemon, &fixture.controller).await;
    let effects = fixture.effects.clone();
    let start = tokio::spawn(async move {
        IdleStart::start_if_idle(
            &effects,
            &AgentId::parse(AGENT).unwrap(),
            &ConversationId::parse(CONVERSATION).unwrap(),
        )
        .await
    });
    let command = next_json(&mut first_daemon).await;
    respond(
        &mut first_daemon,
        &command,
        json!({ "started": true, "pid": 7, "started_at": 100 }),
    )
    .await;
    start.await.unwrap().unwrap();
    send_product(
        &mut first_daemon,
        product_frame(
            "runtime_state",
            Some(1),
            json!({
                "state": {
                    "runId": "old-run",
                    "agentId": AGENT,
                    "conversationId": CONVERSATION,
                    "status": "thinking",
                    "startedAt": 50,
                    "updatedAt": 50,
                    "lastActivityAt": 50,
                },
                "ts": 50,
            }),
        ),
    )
    .await;
    assert_eq!(next_json(&mut live).await["payload"]["runId"], "old-run");
    first_daemon
        .send(Message::Text(
            json!({
                "direction": "daemon_to_server",
                "type": "event",
                "kind": "shutting_down",
                "reason": "requested",
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
    assert!(matches!(
        first_daemon.next().await.unwrap().unwrap(),
        Message::Close(_)
    ));
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while fixture.controller.active_connection_count() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    let mut second_daemon = connect(fixture.address).await;
    complete_attachment(&mut second_daemon, &fixture.controller).await;
    let mut db = open(&fixture.path).await;
    wait_for_slot_daemon(&mut db, None).await;
    let effects = fixture.effects.clone();
    let successor = tokio::spawn(async move {
        IdleStart::start_if_idle(
            &effects,
            &AgentId::parse(AGENT).unwrap(),
            &ConversationId::parse(CONVERSATION).unwrap(),
        )
        .await
    });
    let successor_command = next_json(&mut second_daemon).await;
    assert_eq!(successor_command["params"]["runtime_generation"], 2);
    respond(
        &mut second_daemon,
        &successor_command,
        json!({ "started": true, "pid": 8, "started_at": 200 }),
    )
    .await;
    successor.await.unwrap().unwrap();
    send_runtime_exit(&mut second_daemon, 2, true, 60).await;
    let exit = next_json(&mut live).await;
    assert_eq!(exit["kind"], "agent_status");
    assert!(exit["payload"]["run_state"].is_null());

    live.close(None).await.unwrap();
    second_daemon.close(None).await.unwrap();
    fixture.event_controller.close().await;
    fixture.controller.close().await;
    fixture.server.abort();
}

#[tokio::test]
async fn workspace_large_result_crosses_real_ws_only_for_its_pending_command() {
    let fixture = fixture("workspace-frame-budget.sqlite").await;
    let mut socket = connect(fixture.address).await;
    complete_attachment(&mut socket, &fixture.controller).await;
    let plane = fixture.workspace_files.clone();
    let reading = tokio::spawn(async move {
        plane
            .read_file(
                &AgentId::parse(AGENT).unwrap(),
                &host_id("host-1"),
                None,
                "MEMORY.md",
            )
            .await
    });
    let command = next_json(&mut socket).await;
    assert_eq!(command["kind"], "agent_workspace_file_read");
    let content = "\u{1}".repeat(512 * 1024);
    let result = json!({"ok":true,"file":{"path":"MEMORY.md","content":content,"size":512*1024,"mtime":1700000000123.456,"truncated":false}});
    respond(&mut socket, &command, result.clone()).await;
    let file = reading.await.unwrap().unwrap();
    assert_eq!(file.content, content);
    // A duplicate has no pending command and must not gain the large allowance.
    respond(&mut socket, &command, result).await;
    expect_close(&mut socket, CloseCode::Size, "frame_too_large").await;
    fixture.controller.close().await;
    fixture.server.abort();
}

#[tokio::test]
async fn a_forged_workspace_kind_cannot_enlarge_an_ordinary_pending_response() {
    let fixture = fixture("forged-workspace-budget.sqlite").await;
    let mut socket = connect(fixture.address).await;
    complete_attachment(&mut socket, &fixture.controller).await;
    let dispatch = fixture.controller.owning_host_data_plane();
    let probing = probe_in_background(
        &dispatch,
        catalog_request(custody(&dispatch, "host-1"), true),
    );
    let mut command = next_json(&mut socket).await;
    command["kind"] = json!("agent_workspace_file_read");
    respond(
        &mut socket,
        &command,
        json!({"content":"x".repeat(1024*1024)}),
    )
    .await;
    expect_close(&mut socket, CloseCode::Size, "frame_too_large").await;
    assert!(!matches!(
        probing.await.unwrap(),
        RuntimeCatalogOutcome::Detected(_)
    ));
    fixture.controller.close().await;
    fixture.server.abort();
}

#[tokio::test]
async fn agent_full_activity_has_one_http_ws_stream_across_focus_and_revocation() {
    const AGGREGATE_AGENT: &str = "agent-aggregate";
    const SECOND: &str = "conversation0002";
    const PAIR: &str = "activity-dm";
    const SECRET: &str = "secret-origin";
    let fixture = fixture("daemon-agent-slot-status.sqlite").await;
    let mut db = open(&fixture.path).await;
    sqlx::raw_sql(
        "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES('stream-peer','stream-peer','Stream Peer',1,1);
         INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, display_name, created_by_user_id, session_scope)
         VALUES ('agent-aggregate', 'agent-aggregate', 'codex', 'host-1', 1, 1, 'Aggregate Agent', 'stream-peer', 'agent');
         INSERT INTO workspace_members (workspace_id, agent_id, joined_at) VALUES ('workspace-1', 'agent-aggregate', 1);
         INSERT INTO conversation_agents (conversation_id, agent_id, joined_at) VALUES ('conversation0001', 'agent-aggregate', 1);
         INSERT INTO conversations (id, workspace_id, placement_kind, roster_policy, title, title_key, status, created_at, updated_at)
         VALUES ('conversation0002', 'workspace-1', 'workspace', 'fixed', 'Second', 'second', 'active', 1, 1);
         INSERT INTO conversation_agents (conversation_id, agent_id, joined_at) VALUES ('conversation0002', 'agent-aggregate', 1);
         INSERT INTO conversation_users (conversation_id, user_id, joined_at) VALUES ('conversation0002', 'user-1', 1);
         INSERT INTO conversations(id,placement_kind,roster_policy,title,endpoint_a_kind,endpoint_a_id,endpoint_b_kind,endpoint_b_id,status,visibility,created_at,updated_at)
         VALUES('activity-dm','direct_pair','fixed','','agent','agent-aggregate','human','stream-peer','idle','private',1,1);
         INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,status,visibility,created_at,updated_at)
         VALUES('secret-origin','workspace-1','workspace','open','Secret','secret','idle','private',1,1);
         INSERT INTO conversation_agents(conversation_id,agent_id,joined_at) VALUES('secret-origin','agent-aggregate',1);
         INSERT INTO messages (id, conversation_id, from_kind, from_user_id, content, seq, created_at)
         VALUES ('aggregate-message', 'conversation0001', 'user', 'user-1', 'wake aggregate', 2, 2);
         INSERT INTO conversation_inboxes (conversation_id, agent_id, message_id, created_at, delivery_kind)
         VALUES ('conversation0001', 'agent-aggregate', 'aggregate-message', 2, 'turn');",
    )
    .execute(&mut db)
    .await
    .unwrap();
    let scope = format!("agent:{AGGREGATE_AGENT}");
    let (mut stream, baseline) = connect_event_scope_with_ack(fixture.address, &scope).await;
    let mut daemon = connect(fixture.address).await;
    complete_attachment(&mut daemon, &fixture.controller).await;
    let effects = fixture.effects.clone();
    let start = tokio::spawn(async move {
        IdleStart::start_if_idle(
            &effects,
            &AgentId::parse(AGGREGATE_AGENT).unwrap(),
            &ConversationId::parse(CONVERSATION).unwrap(),
        )
        .await
    });
    let start_command = next_json(&mut daemon).await;
    let generation = start_command["params"]["runtime_generation"]
        .as_i64()
        .unwrap();
    respond(
        &mut daemon,
        &start_command,
        json!({ "started": true, "pid": 8, "started_at": 100 }),
    )
    .await;
    start.await.unwrap().unwrap();

    // Shared workspace alone suffices, even with no creator, DM or channel relation.
    sqlx::raw_sql("DELETE FROM conversation_users WHERE user_id='user-1';")
        .execute(&mut db)
        .await
        .unwrap();
    let mut activities = Vec::new();
    for (source, parsed) in [
        (
            CONVERSATION,
            json!({"kind":"thinking","text":"one shared thought"}),
        ),
        (
            SECOND,
            json!({"kind":"tool_call","name":"shell","toolCallId":"stream-tool","input":{}}),
        ),
        (
            PAIR,
            json!({"kind":"tool_call_input","toolCallId":"stream-tool","input":{"command":"complete input"}}),
        ),
        (
            CONVERSATION,
            json!({"kind":"tool_output","name":"shell","toolCallId":"stream-tool","outcome":"success"}),
        ),
    ] {
        send_product(
            &mut daemon,
            product_frame_for(
                "runtime_parsed_event",
                Some(generation),
                json!({"parsed":parsed,"ts":20}),
                AGGREGATE_AGENT,
                source,
                "agent",
            ),
        )
        .await;
        loop {
            let frame = next_json(&mut stream).await;
            if frame["kind"] == "agent_stream_activity" {
                activities.push(frame);
                break;
            }
        }
    }
    assert_eq!(
        activities[1]["payload"]["id"],
        activities[2]["payload"]["id"]
    );
    assert_eq!(
        activities[2]["payload"]["payload"]["input"]["command"],
        "complete input"
    );
    assert!(
        activities
            .iter()
            .all(|e| e["scope"] == scope && e["payload"].get("conversation_id").is_none())
    );
    assert_eq!(
        activities[3]["payload"]["payload"],
        json!({"name":"shell", "tool_call_id":"stream-tool", "outcome":"success"})
    );
    let path = format!("/api/agents/{AGGREGATE_AGENT}/activity");
    let unauthenticated = fixture
        .agent_activity
        .clone()
        .oneshot(Request::builder().uri(&path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        management_request(
            &fixture.agent_activity,
            "GET",
            &format!("/api/agents/{AGENT}/activity"),
            ""
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    for query in ["limit=0", "limit=abc", "before_id=0"] {
        assert_eq!(
            management_request(
                &fixture.agent_activity,
                "GET",
                &format!("{path}?{query}"),
                ""
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    let (status, page) = management_request(&fixture.agent_activity, "GET", &path, "").await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(
        page["events"],
        json!([
            activities[0]["payload"],
            activities[2]["payload"],
            activities[3]["payload"]
        ])
    );
    let (status, tail) = management_request(
        &fixture.agent_activity,
        "GET",
        &format!("{path}?limit=2"),
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let before = tail["next_before_id"].as_i64().unwrap();
    let (_, head) = management_request(
        &fixture.agent_activity,
        "GET",
        &format!("{path}?before_id={before}"),
        "",
    )
    .await;
    assert_eq!(head["events"], json!([activities[0]["payload"]]));
    // Resume intentionally replays duplicates with the same transport/event identity.
    stream.send(Message::Text(json!({"type":"resume","scopes":{scope.clone():{"epoch":baseline["epoch"],"lastSeq":baseline["seq"]}}}).to_string().into())).await.unwrap();
    for original in &activities {
        loop {
            let replay = next_json(&mut stream).await;
            if replay["kind"] == "agent_stream_activity" {
                assert_eq!(&replay, original);
                break;
            }
        }
    }
    send_product(
        &mut daemon,
        product_frame_for(
            "runtime_parsed_event",
            Some(generation + 1),
            json!({"parsed":{"kind":"text","text":"stale must not enter"},"ts":21}),
            AGGREGATE_AGENT,
            SECRET,
            "agent",
        ),
    )
    .await;
    assert_no_daemon_frame(&mut stream).await;
    let (_, unchanged) = management_request(&fixture.agent_activity, "GET", &path, "").await;
    assert_eq!(unchanged, page);
    sqlx::query(
        "DELETE FROM workspace_users WHERE workspace_id='workspace-1' AND user_id='user-1'",
    )
    .execute(&mut db)
    .await
    .unwrap();
    // Revocation must cut the already-open live subscription, not merely a new read.
    send_product(
        &mut daemon,
        product_frame_for(
            "runtime_parsed_event",
            Some(generation),
            json!({"parsed":{"kind":"thinking","text":"after revocation"},"ts":22}),
            AGGREGATE_AGENT,
            SECOND,
            "agent",
        ),
    )
    .await;
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let count: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM conversation_events WHERE agent_id='agent-aggregate'",
            )
            .fetch_one(&mut db)
            .await
            .unwrap();
            if count == 4 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let revoked = next_json(&mut stream).await;
    assert_eq!(revoked["code"], "forbidden");
    assert_eq!(revoked["scope"], scope);
    assert_no_daemon_frame(&mut stream).await;
    assert_eq!(
        management_request(&fixture.agent_activity, "GET", &path, "")
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    stream.send(Message::Text(json!({"type":"resume","scopes":{scope:{"epoch":baseline["epoch"],"lastSeq":baseline["seq"]}}}).to_string().into())).await.unwrap();
    let refused = next_json(&mut stream).await;
    assert_eq!(refused["code"], "forbidden", "{refused}");
    assert_eq!(refused["scope"], format!("agent:{AGGREGATE_AGENT}"));
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM conversation_events WHERE agent_id='agent-aggregate'",
    )
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(count, 4);
    daemon.close(None).await.unwrap();
    stream.close(None).await.unwrap();
    fixture.controller.close().await;
    fixture.server.abort();
}

#[tokio::test]
async fn initial_and_refined_tool_inputs_share_ts_limits_in_both_activity_scopes() {
    const INPUT_AGENT: &str = "input-agent";
    let cases: Vec<Value> = serde_json::from_str(include_str!(
        "../../../../../rust/test-fixtures/fixtures/activity-input-sanitizer.json"
    ))
    .unwrap();
    for scope_kind in ["conversation", "agent"] {
        let fixture = fixture("input-limits.sqlite").await;
        let mut db = open(&fixture.path).await;
        sqlx::query("INSERT INTO agents(id,name,runtime,host_id,created_at,updated_at,display_name,created_by_user_id,session_scope) VALUES('input-agent','input-agent','codex','host-1',1,1,'Input Agent','user-1',?)")
            .bind(scope_kind).execute(&mut db).await.unwrap();
        sqlx::raw_sql("INSERT INTO workspace_members(workspace_id,agent_id,joined_at) VALUES('workspace-1','input-agent',1);
            INSERT INTO conversation_agents(conversation_id,agent_id,joined_at) VALUES('conversation0001','input-agent',1);
            INSERT INTO messages(id,conversation_id,from_kind,from_user_id,content,seq,created_at) VALUES('input-wake','conversation0001','user','user-1','wake',2,2);
            INSERT INTO conversation_inboxes(conversation_id,agent_id,message_id,created_at,delivery_kind) VALUES('conversation0001','input-agent','input-wake',2,'turn');")
            .execute(&mut db).await.unwrap();
        let historical =
            json!({"name":"test", "tool_call_id":"call-0", "input":"historic".repeat(200)})
                .to_string();
        let legacy_id = sqlx::query("INSERT INTO conversation_events(conversation_id,agent_id,kind,payload,created_at) VALUES(?,?,'tool_call',?,1)")
            .bind(CONVERSATION).bind(INPUT_AGENT).bind(&historical).execute(&mut db).await.unwrap().last_insert_rowid();
        let scope = if scope_kind == "agent" {
            format!("agent:{INPUT_AGENT}")
        } else {
            format!("conversation-side:{CONVERSATION}")
        };
        let (mut live, _) = connect_event_scope_with_ack(fixture.address, &scope).await;
        let mut daemon = connect(fixture.address).await;
        complete_attachment(&mut daemon, &fixture.controller).await;
        let effects = fixture.effects.clone();
        let start = tokio::spawn(async move {
            IdleStart::start_if_idle(
                &effects,
                &AgentId::parse(INPUT_AGENT).unwrap(),
                &ConversationId::parse(CONVERSATION).unwrap(),
            )
            .await
        });
        let command = next_json(&mut daemon).await;
        let generation = command["params"]["runtime_generation"].as_i64().unwrap();
        respond(
            &mut daemon,
            &command,
            json!({"started":true,"pid":8,"started_at":100}),
        )
        .await;
        start.await.unwrap().unwrap();
        let reads = MessagingStore::open(&fixture.path)
            .await
            .unwrap()
            .conversation_agent_activity();
        let (http, path) = if scope_kind == "agent" {
            (silverhand_backend_server::conversation_messaging::messaging::collaboration::conversation_agent_activity::agent_router(reads), format!("/api/agents/{INPUT_AGENT}/activity"))
        } else {
            (silverhand_backend_server::conversation_messaging::messaging::collaboration::conversation_agent_activity::router(reads), format!("/api/conversations/{CONVERSATION}/agents/{INPUT_AGENT}/activity"))
        };
        for index in 0..cases.len() {
            let mut original_id = None;
            for (kind, case) in [
                ("tool_call", &cases[index]),
                ("tool_call_input", &cases[1 - index]),
            ] {
                let mut parsed =
                    json!({"kind":kind,"toolCallId":format!("call-{index}"),"input":case["input"]});
                if kind == "tool_call" {
                    parsed["name"] = json!("test");
                }
                send_product(
                    &mut daemon,
                    product_frame_for(
                        "runtime_parsed_event",
                        Some(generation),
                        json!({"parsed":parsed,"ts":20}),
                        INPUT_AGENT,
                        CONVERSATION,
                        scope_kind,
                    ),
                )
                .await;
                let activity = loop {
                    let frame = next_json(&mut live).await;
                    if frame["kind"] == "agent_event" {
                        assert_eq!(frame["payload"]["event"]["input"], case["expected"]);
                    }
                    if frame["kind"] == "conversation_activity"
                        || frame["kind"] == "agent_stream_activity"
                    {
                        break frame;
                    }
                };
                let id = activity["payload"]["id"].as_i64().unwrap();
                let stored: String =
                    sqlx::query_scalar("SELECT payload FROM conversation_events WHERE id=?")
                        .bind(id)
                        .fetch_one(&mut db)
                        .await
                        .unwrap();
                assert_eq!(
                    serde_json::from_str::<Value>(&stored).unwrap()["input"],
                    case["expected"],
                    "{scope_kind} {kind} {} first durable payload",
                    case["name"]
                );
                let (status, page) = management_request(&http, "GET", &path, "").await;
                assert_eq!(status, StatusCode::OK, "{page}");
                let row = page["events"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|row| row["id"] == id)
                    .unwrap();
                assert_eq!(row["payload"]["input"], case["expected"]);
                assert_eq!(activity["payload"]["payload"]["input"], case["expected"]);
                if let Some(original) = original_id {
                    assert_eq!(id, original);
                } else {
                    original_id = Some(id);
                }
            }
        }
        let retained: String =
            sqlx::query_scalar("SELECT payload FROM conversation_events WHERE id=?")
                .bind(legacy_id)
                .fetch_one(&mut db)
                .await
                .unwrap();
        assert_eq!(retained, historical);
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM conversation_events WHERE agent_id=? AND kind='tool_call'",
        )
        .bind(INPUT_AGENT)
        .fetch_one(&mut db)
        .await
        .unwrap();
        assert_eq!(
            count, 3,
            "one legacy row plus two calls; refinements keep identity"
        );
        fixture.event_controller.close().await;
        fixture.controller.close().await;
        fixture.server.abort();
    }
}

/// Runs the installed logged-out Grok through the actual host supervisor, then
/// transports its pre-session error through the real daemon WS and activity reads.
#[tokio::test]
#[ignore = "requires GROK_TEST_BINARY pointing to an explicitly authorized logged-out installation"]
async fn grok_pre_session_auth_error_reaches_durable_http_and_live_activity() {
    use std::{process::Stdio, time::Duration};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    const AUTH_AGENT: &str = "grok-auth-agent";
    assert!(std::env::var_os("GROK_TEST_BINARY").is_some());
    for scope_kind in ["conversation", "agent"] {
        let fixture = fixture("grok-auth.sqlite").await;
        let mut db = open(&fixture.path).await;
        seed_grok_auth_agent(&mut db, scope_kind).await;
        let scope = if scope_kind == "agent" {
            format!("agent:{AUTH_AGENT}")
        } else {
            format!("conversation-side:{CONVERSATION}")
        };
        let (mut live, _) = connect_event_scope_with_ack(fixture.address, &scope).await;
        let mut daemon = connect(fixture.address).await;
        complete_attachment(&mut daemon, &fixture.controller).await;

        let effects = fixture.effects.clone();
        let start = tokio::spawn(async move {
            IdleStart::start_if_idle(
                &effects,
                &AgentId::parse(AUTH_AGENT).unwrap(),
                &ConversationId::parse(CONVERSATION).unwrap(),
            )
            .await
        });
        let command = next_json(&mut daemon).await;
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
        let mut host =
            tokio::process::Command::new(root.join("packages/host/node_modules/.bin/tsx"))
                .arg(root.join("packages/host/tests/fixtures/grok/auth-host.mts"))
                .current_dir(&root)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
        let mut stdin = host.stdin.take().unwrap();
        stdin
            .write_all(format!("{}\n", command["params"]).as_bytes())
            .await
            .unwrap();
        let mut output = BufReader::new(host.stdout.take().unwrap()).lines();
        let mut error_event = None;
        let mut started = false;
        while error_event.is_none() || !started {
            let line = tokio::time::timeout(Duration::from_secs(20), output.next_line())
                .await
                .unwrap()
                .unwrap()
                .expect("host closed before auth error");
            let frame: Value = serde_json::from_str(&line).unwrap();
            assert!(frame.get("bridge_error").is_none(), "{frame}");
            if frame.get("result").is_some() {
                respond(&mut daemon, &command, frame["result"].clone()).await;
                started = true;
            }
            if frame["event"]["parsed"]["kind"] == "error" {
                error_event = Some(frame["event"].clone());
            } else if frame.get("event").is_some() {
                assert_ne!(frame["event"]["parsed"]["kind"], "session_init");
            }
        }
        start.await.unwrap().unwrap();
        let event = error_event.unwrap();
        assert_eq!(event["agent_id"], AUTH_AGENT);
        assert_eq!(event["conversation_id"], CONVERSATION);
        assert_eq!(
            event["runtime_generation"],
            command["params"]["runtime_generation"]
        );
        assert_eq!(event["parsed"]["message"], "Authentication required");
        let generation = event["runtime_generation"].as_i64().unwrap();
        send_product(
            &mut daemon,
            product_frame_for(
                "runtime_parsed_event",
                Some(generation),
                json!({ "parsed": event["parsed"], "ts": event["ts"] }),
                AUTH_AGENT,
                CONVERSATION,
                scope_kind,
            ),
        )
        .await;

        let activity = loop {
            let frame = next_json(&mut live).await;
            if frame["kind"] == "conversation_activity" || frame["kind"] == "agent_stream_activity"
            {
                break frame;
            }
        };
        assert_eq!(
            activity["payload"]["payload"]["message"],
            "Authentication required"
        );
        let id = activity["payload"]["id"].as_i64().unwrap();
        assert_grok_auth_activity_reads(&fixture.path, &mut db, scope_kind, id).await;

        stdin.shutdown().await.unwrap();
        drop(stdin);
        assert!(
            tokio::time::timeout(Duration::from_secs(5), host.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
        fixture.event_controller.close().await;
        fixture.controller.close().await;
        fixture.server.abort();
    }
}

async fn seed_grok_auth_agent(db: &mut SqliteConnection, scope_kind: &str) {
    sqlx::query(
        "INSERT INTO agents(id,name,runtime,host_id,created_at,updated_at,
         display_name,created_by_user_id,session_scope)
         VALUES('grok-auth-agent','grok-auth-agent','grok','host-1',1,1,
         'Grok Auth','user-1',?)",
    )
    .bind(scope_kind)
    .execute(&mut *db)
    .await
    .unwrap();

    sqlx::raw_sql(
        "INSERT INTO workspace_members(workspace_id,agent_id,joined_at)
         VALUES('workspace-1','grok-auth-agent',1);
         INSERT INTO conversation_agents(conversation_id,agent_id,joined_at)
         VALUES('conversation0001','grok-auth-agent',1);
         INSERT INTO messages(id,conversation_id,from_kind,from_user_id,content,seq,created_at)
         VALUES('grok-auth-wake','conversation0001','user','user-1','wake',2,2);
         INSERT INTO conversation_inboxes(conversation_id,agent_id,message_id,created_at,delivery_kind)
         VALUES('conversation0001','grok-auth-agent','grok-auth-wake',2,'turn');",
    )
    .execute(&mut *db)
    .await
    .unwrap();
}

async fn assert_grok_auth_activity_reads(
    path: &std::path::Path,
    db: &mut SqliteConnection,
    scope_kind: &str,
    id: i64,
) {
    use silverhand_backend_server::conversation_messaging::messaging::collaboration::conversation_agent_activity as activity_reads;

    let payload: String = sqlx::query_scalar("SELECT payload FROM conversation_events WHERE id=?")
        .bind(id)
        .fetch_one(&mut *db)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&payload).unwrap()["message"],
        "Authentication required"
    );
    let reads = MessagingStore::open(path)
        .await
        .unwrap()
        .conversation_agent_activity();
    let (http, path) = if scope_kind == "agent" {
        (
            activity_reads::agent_router(reads),
            "/api/agents/grok-auth-agent/activity".to_owned(),
        )
    } else {
        (
            activity_reads::router(reads),
            format!("/api/conversations/{CONVERSATION}/agents/grok-auth-agent/activity"),
        )
    };
    let (status, page) = management_request(&http, "GET", &path, "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        page["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["id"] == id && row["payload"]["message"] == "Authentication required")
    );
}

#[tokio::test]
async fn a_turn_end_drains_queued_turns_but_not_observe_only_context() {
    let fixture = fixture("daemon-turn-end-drain.sqlite").await;
    let mut daemon = connect(fixture.address).await;
    complete_attachment(&mut daemon, &fixture.controller).await;
    let effects = fixture.effects.clone();
    let start = tokio::spawn(async move {
        IdleStart::start_if_idle(
            &effects,
            &AgentId::parse(AGENT).unwrap(),
            &ConversationId::parse(CONVERSATION).unwrap(),
        )
        .await
    });
    let start_command = next_json(&mut daemon).await;
    let generation = start_command["params"]["runtime_generation"]
        .as_i64()
        .unwrap();
    respond(
        &mut daemon,
        &start_command,
        json!({ "started": true, "pid": 7, "started_at": 100 }),
    )
    .await;
    start.await.unwrap().unwrap();

    let turn_ended = |ts: i64| {
        product_frame(
            "runtime_state",
            Some(generation),
            json!({
                "state": {
                    "runId": format!("run-{ts}"),
                    "agentId": AGENT,
                    "conversationId": CONVERSATION,
                    "status": "idle",
                    "terminalReason": "turn_ended",
                    "startedAt": 10,
                    "updatedAt": ts,
                    "lastActivityAt": ts,
                },
                "ts": ts,
            }),
        )
    };

    // A queued turn behind the batch the run took is drained when the turn ends.
    let mut db = open(&fixture.path).await;
    add_inbox(&mut db, "message-2", 2).await;
    sqlx::query("UPDATE conversation_inboxes SET delivered_at = 15 WHERE message_id = 'message-1'")
        .execute(&mut db)
        .await
        .unwrap();
    send_product(&mut daemon, turn_ended(20)).await;
    let drain = next_json(&mut daemon).await;
    assert_eq!(drain["kind"], "runtime_deliver");
    assert_eq!(drain["params"]["deliveries"][0]["seq"], 2);
    respond(
        &mut daemon,
        &drain,
        json!({ "disposition": "admitted", "pid": 7, "started_at": 100 }),
    )
    .await;

    // Only observe context left: the turn end wakes nobody.
    sqlx::query("UPDATE conversation_inboxes SET in_flight_owner = NULL, in_flight_at = NULL, delivered_at = 30")
        .execute(&mut db)
        .await
        .unwrap();
    add_inbox(&mut db, "message-3", 3).await;
    sqlx::query(
        "UPDATE conversation_inboxes SET delivery_kind = 'observe' WHERE message_id = 'message-3'",
    )
    .execute(&mut db)
    .await
    .unwrap();
    send_product(&mut daemon, turn_ended(40)).await;
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(300), daemon.next())
            .await
            .is_err(),
        "observe-only pending context must not be drained"
    );

    daemon.close(None).await.unwrap();
    fixture.controller.close().await;
    fixture.server.abort();
}
