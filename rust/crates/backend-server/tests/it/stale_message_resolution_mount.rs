//! Composition-root MOUNT proof for `POST /internal/agents/{aid}/stale-message-resolution`
//! (#task:2197 / #2194).
//!
//! The route's own `tests` module already drives the handler vertical — body bound, zod
//! shape, admission/capability refusals never reaching the observer, and the real
//! `SqliteStaleMessageResolutionAdmission` + `MessagingStore` capability answering `200` — by
//! injecting the `RuntimePrincipal` extension BY HAND, standing in for the perimeter. What no
//! such test can show is that the route is actually MOUNTED behind the product's runtime
//! credential perimeter: that an anonymous caller and a credential minted for another agent
//! are refused BEFORE any product authority is consulted, that a real minted bearer resolves
//! to the agent whose stale draft it resolves, that the PRODUCTION
//! `TracingStaleMessageResolutionObserver` emits exactly one record with the resolution's
//! fields, and that the path answers `POST` only.
//!
//! So this composes the exact production seam — `require_runtime_credential` over the real
//! `RuntimeCredentialAuthority`/`SqliteRuntimeCustodyStore`, wrapping the real authorities and
//! the production observer — and drives it end to end over a real minted credential. The
//! authorities and the observer are wrapped in counting decorators ONLY so "the refused arms
//! touch nothing" is a hard assertion; every admitted call delegates to the real SQLite
//! admission, the real `MessagingStore` capability, and the real `tracing::warn!`.

use std::{
    convert::Infallible,
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering::SeqCst},
    },
};

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::Value;
use silverhand_backend_core::{
    conversation_messaging::messaging::{
        CapabilityDenied, SendCapability,
        collaboration::stale_message_resolution::{
            StaleMessageResolutionAdmissionFact, StaleMessageResolutionAdmissionPort,
            StaleMessageResolutionAdmissionRefusal,
        },
    },
    foundation::identity::{AgentId, ConversationId, DaemonInstanceId},
    foundation::persistence::PersistenceFailure,
    host_runtime::runtime_credential::RuntimeScope,
};
use silverhand_backend_server::{
    conversation_messaging::messaging::collaboration::stale_message_resolution::{
        StaleMessageResolutionObservation, StaleMessageResolutionObserver,
        TracingStaleMessageResolutionObserver, router as stale_message_resolution_router,
    },
    foundation::clock::ProcessClock,
    host_runtime::runtime_credential::{
        RuntimeCredentialAuthority, RuntimeDispatchFailure, RuntimeEntropy, RuntimeStartHandoff,
        RuntimeStartTransport, require_runtime_credential,
    },
};
use silverhand_backend_sqlite::{
    conversation_messaging::messaging::{
        MessagingStore,
        collaboration::stale_message_resolution::SqliteStaleMessageResolutionAdmission,
    },
    host_runtime::runtime_credential::SqliteRuntimeCustodyStore,
};
use sqlx::Connection;
use tower::ServiceExt;

use crate::current_schema;
use crate::log_capture;

use log_capture::LogBuffer;

const AGENT: &str = "agent-1";
const CONVERSATION: &str = "conversation-1";

/// Deterministic bytes so the minted credential is reproducible; the real hashing the
/// authority applies to the token it hands back is what the perimeter re-derives.
#[derive(Clone)]
struct SequenceEntropy(Arc<AtomicU64>);

impl Default for SequenceEntropy {
    fn default() -> Self {
        Self(Arc::new(AtomicU64::new(1)))
    }
}

impl RuntimeEntropy for SequenceEntropy {
    type Error = Infallible;

    fn fill(&self, bytes: &mut [u8]) -> Result<(), Self::Error> {
        let seed = self.0.fetch_add(1, SeqCst);
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = seed.wrapping_add(index as u64) as u8;
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct FixedClock;

impl ProcessClock for FixedClock {
    fn now_unix_millis(&self) -> i64 {
        1_000
    }
}

/// Captures the daemon auth token the authority mints so the test can present it as the
/// bearer a real daemon would; the start is always accepted.
#[derive(Clone, Default)]
struct CapturingStart(Arc<Mutex<Option<String>>>);

impl RuntimeStartTransport for CapturingStart {
    type Error = Infallible;

    fn dispatch(
        &self,
        handoff: &RuntimeStartHandoff,
    ) -> impl Future<Output = Result<(), RuntimeDispatchFailure<Self::Error>>> + Send {
        *self.0.lock().unwrap() = Some(handoff.daemon_auth_token().to_owned());
        std::future::ready(Ok(()))
    }
}

/// Wraps the real SQLite admission so the refused arms can assert the post authority was
/// NEVER consulted; every admitted call delegates to the real one-snapshot admission and
/// returns its canonical fact unchanged — the adapter answers the core port itself, so no
/// translation happens anywhere.
#[derive(Clone)]
struct CountingAdmission {
    inner: SqliteStaleMessageResolutionAdmission,
    calls: Arc<AtomicUsize>,
}

impl StaleMessageResolutionAdmissionPort for CountingAdmission {
    type Error = PersistenceFailure;

    async fn admit(
        &self,
        agent: &AgentId,
        conversation: &ConversationId,
    ) -> Result<
        Result<StaleMessageResolutionAdmissionFact, StaleMessageResolutionAdmissionRefusal>,
        Self::Error,
    > {
        self.calls.fetch_add(1, SeqCst);
        self.inner.admit(agent, conversation).await
    }
}

/// Wraps the real `MessagingStore` `send_messages` capability so the refused arms can assert
/// it was never consulted; every admitted call delegates to the real capability read.
#[derive(Clone)]
struct CountingCapability {
    inner: MessagingStore,
    calls: Arc<AtomicUsize>,
}

impl SendCapability for CountingCapability {
    type Error = PersistenceFailure;

    async fn may_send_messages(
        &self,
        agent: &AgentId,
        conversation: &ConversationId,
    ) -> Result<Result<(), CapabilityDenied>, Self::Error> {
        self.calls.fetch_add(1, SeqCst);
        self.inner.may_send_messages(agent, conversation).await
    }
}

/// Wraps the PRODUCTION observer so the refused arms can assert nothing was recorded, while an
/// admitted request still fires the real `tracing::warn!` the `LogBuffer` captures.
#[derive(Clone, Default)]
struct CountingObserver {
    inner: TracingStaleMessageResolutionObserver,
    calls: Arc<AtomicUsize>,
}

impl StaleMessageResolutionObserver for CountingObserver {
    fn stale_message_resolved(&self, observation: &StaleMessageResolutionObservation) {
        self.calls.fetch_add(1, SeqCst);
        self.inner.stale_message_resolved(observation);
    }
}

/// The mounted vertical: the real authorities + production observer guarded by the real
/// runtime credential perimeter, the three call counters, the real minted bearer, and the
/// `TempDir` the pools outlive.
struct Mounted {
    app: Router,
    admission_calls: Arc<AtomicUsize>,
    capability_calls: Arc<AtomicUsize>,
    observer_calls: Arc<AtomicUsize>,
    bearer: String,
    _directory: tempfile::TempDir,
}

/// `agent-1` is a member of `workspace`, holds its `send_messages` capability and is on the
/// `conversation-1` roster of an idle (post-accepting) channel — the admission resolves that
/// participation and the capability grant. The same rows back the runtime credential the
/// authority mints for the `agent` slot.
const SEED: &[&str] = &[
    "INSERT INTO hosts (id, name, kind, created_at) VALUES ('host-1', 'host', 'local', 0)",
    "INSERT INTO users (id, name, display_name, created_at, updated_at) \
     VALUES ('user-1', 'user-1', 'User One', 0, 0)",
    "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, display_name, \
     created_by_user_id, materialization_status, session_scope) \
     VALUES ('agent-1', 'lio', 'claude', 'host-1', 0, 0, 'Lio', 'user-1', 'active', 'agent')",
    "INSERT INTO workspaces (id, name, name_key, owner_user_id, created_at, updated_at) \
     VALUES ('workspace', 'Product Space', 'product space', 'user-1', 0, 0)",
    "INSERT INTO workspace_members (workspace_id, agent_id, joined_at) \
     VALUES ('workspace', 'agent-1', 0)",
    "INSERT INTO workspace_agent_capabilities (workspace_id, agent_id, capability) \
     VALUES ('workspace', 'agent-1', 'send_messages')",
    "INSERT INTO conversations (id, workspace_id, placement_kind, roster_policy, title, \
     title_key, status, visibility, created_at, updated_at, latest_message_seq) \
     VALUES ('conversation-1', 'workspace', 'workspace', 'open', 'General Chat', \
     'general chat', 'idle', 'private', 0, 0, 0)",
    "INSERT INTO conversation_agents (conversation_id, agent_id, joined_at) \
     VALUES ('conversation-1', 'agent-1', 0)",
];

async fn mounted() -> Mounted {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("stale-resolution-mount.sqlite");

    let mut db = current_schema::create_current_schema(&path).await;
    for statement in SEED {
        sqlx::query(*statement).execute(&mut db).await.unwrap();
    }
    db.close().await.unwrap();

    // Mint a real credential for the `agent` slot through the same authority that will guard
    // the route; the bearer is the token a daemon would present.
    let authority = RuntimeCredentialAuthority::new(
        SqliteRuntimeCustodyStore::open(&path).await.unwrap(),
        SequenceEntropy::default(),
        FixedClock,
    );
    let transport = CapturingStart::default();
    authority
        .dispatch_start(
            RuntimeScope::AgentSlot {
                agent: AgentId::parse(AGENT).unwrap(),
            },
            DaemonInstanceId::parse("daemon-1").unwrap(),
            ConversationId::parse(CONVERSATION).unwrap(),
            &transport,
        )
        .await
        .unwrap();
    let bearer = transport.0.lock().unwrap().clone().unwrap();

    let store = MessagingStore::open(&path).await.unwrap();
    let admission_calls = Arc::new(AtomicUsize::new(0));
    let capability_calls = Arc::new(AtomicUsize::new(0));
    let observer_calls = Arc::new(AtomicUsize::new(0));

    let router = stale_message_resolution_router(
        CountingAdmission {
            inner: store.stale_message_resolution_admission(),
            calls: admission_calls.clone(),
        },
        CountingCapability {
            inner: store,
            calls: capability_calls.clone(),
        },
        CountingObserver {
            inner: TracingStaleMessageResolutionObserver,
            calls: observer_calls.clone(),
        },
    );
    let app = require_runtime_credential(router, authority);
    Mounted {
        app,
        admission_calls,
        capability_calls,
        observer_calls,
        bearer,
        _directory: directory,
    }
}

fn body() -> Body {
    Body::from(
        r#"{"conversation_id":"conversation-1","stale_event_id":"evt-1","resolution_reason":"obsolete"}"#,
    )
}

/// Drives the mounted app. `bearer` of `None` omits the `Authorization` header entirely — a
/// genuinely anonymous request, not a malformed token — and `path_aid` names the URL agent,
/// which may differ from the agent the `bearer` was minted for so the perimeter's
/// path/principal binding can be falsified directly.
async fn call(
    app: &Router,
    method: &str,
    path_aid: &str,
    bearer: Option<&str>,
    body: Body,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(format!(
        "/internal/agents/{path_aid}/stale-message-resolution"
    ));
    if let Some(token) = bearer {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = app
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, value)
}

#[tokio::test]
async fn an_anonymous_caller_is_refused_before_any_product_authority() {
    let mounted = mounted().await;
    let (status, _) = call(&mounted.app, "POST", AGENT, None, body()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // The perimeter rejected before the handler: no admission, capability or record ran.
    assert_eq!(mounted.admission_calls.load(SeqCst), 0);
    assert_eq!(mounted.capability_calls.load(SeqCst), 0);
    assert_eq!(mounted.observer_calls.load(SeqCst), 0);
}

#[tokio::test]
async fn a_credential_bound_to_one_agent_cannot_resolve_on_another_agents_path() {
    let mounted = mounted().await;
    // `agent-1`'s real minted bearer, but the URL names `agent-2`. The perimeter binds the
    // path aid to the authenticated principal, so this is refused at the perimeter — proving
    // the route rides the path/principal binding and not a token-only wrapper, ahead of any
    // product authority.
    let (status, _) = call(
        &mounted.app,
        "POST",
        "agent-2",
        Some(&mounted.bearer),
        body(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(mounted.admission_calls.load(SeqCst), 0);
    assert_eq!(mounted.capability_calls.load(SeqCst), 0);
    assert_eq!(mounted.observer_calls.load(SeqCst), 0);
}

#[tokio::test]
async fn the_mounted_path_answers_post_only() {
    let mounted = mounted().await;
    // Authenticated, so a 405 is the ROUTE's method surface, not the perimeter's 401.
    let (status, _) = call(
        &mounted.app,
        "GET",
        AGENT,
        Some(&mounted.bearer),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test(flavor = "current_thread")]
async fn a_minted_credential_resolves_its_own_stale_draft_and_emits_one_record() {
    let logs = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::WARN)
        .with_writer(logs.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let mounted = mounted().await;
    let (status, body_json) =
        call(&mounted.app, "POST", AGENT, Some(&mounted.bearer), body()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body_json, serde_json::json!({"ok": true}));

    // The real one-snapshot admission and the real `send_messages` capability both ran once.
    assert_eq!(mounted.admission_calls.load(SeqCst), 1);
    assert_eq!(mounted.capability_calls.load(SeqCst), 1);
    assert_eq!(mounted.observer_calls.load(SeqCst), 1);

    // The PRODUCTION observer emitted exactly one warning carrying the resolution's fields —
    // the record a no-op observer could never fake. The stale draft never became a message, so
    // the TS durable record's null `messageId`/`messageSeq` are projected as field ABSENCE
    // here (tracing skips a `None`); assert they are absent so no future `"None"`/`"null"`
    // string can masquerade as a typed null.
    let events = logs.events();
    let records: Vec<&Value> = events
        .iter()
        .filter(|event| event["fields"]["message"] == "stale message resolution recorded")
        .collect();
    assert_eq!(records.len(), 1);
    let fields = &records[0]["fields"];
    assert_eq!(fields["agent"], AGENT);
    assert_eq!(fields["source_conversation"], CONVERSATION);
    assert_eq!(fields["target_conversation"], CONVERSATION);
    assert_eq!(fields["stale_event_id"], "evt-1");
    assert_eq!(fields["reason"], "obsolete");
    assert_eq!(fields["outcome"], "obsolete");
    assert!(fields.get("message_id").is_none());
    assert!(fields.get("message_seq").is_none());
}
