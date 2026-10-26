use std::{
    convert::Infallible,
    error::Error,
    future::Future,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{
    Extension, Json, Router,
    body::{Body, to_bytes},
    extract::Query,
    http::{Request, StatusCode},
    routing::get,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use silverhand_backend_core::{
    foundation::identity::{AgentId, ConversationId, DaemonInstanceId},
    host_runtime::runtime_credential::{
        AgentSlotPresentationFocusPort, RuntimeFinalStatus, RuntimePrincipal, RuntimeScope,
    },
    host_runtime::runtime_transition::{BeginTransition, begin_runtime_transition},
    identity_access::agents::runtime::RuntimeKind,
};
use silverhand_backend_server::{
    foundation::clock::ProcessClock,
    host_runtime::runtime_credential::{
        HydrateCredentialDisposition, PrepareRuntimeCredentialError, RuntimeCredentialAuthority,
        RuntimeDispatchFailure, RuntimeEntropy, RuntimeStartFailure, RuntimeStartHandoff,
        RuntimeStartTransport, RuntimeStopTransport, require_runtime_credential,
    },
};
use silverhand_backend_sqlite::{
    foundation::host_agent_store::SqliteHostAgentStore,
    host_runtime::runtime_credential::SqliteRuntimeCustodyStore,
};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use tempfile::TempDir;
use thiserror::Error;
use tokio::{sync::Semaphore, time::timeout};
use tower::ServiceExt;
use tracing_subscriber::fmt::MakeWriter;

use crate::current_schema;

const CONVERSATION_AGENT: &str = "agent-conversation";
const GLOBAL_AGENT: &str = "agent-global";
const SOURCE: &str = "source-1";
const HANG_LIMIT: Duration = Duration::from_secs(5);

type Authority = RuntimeCredentialAuthority<SqliteRuntimeCustodyStore, SequenceEntropy, FixedClock>;

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
        let seed = self.0.fetch_add(1, Ordering::SeqCst);
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

struct Fixture {
    _directory: TempDir,
    path: PathBuf,
    store: SqliteRuntimeCustodyStore,
    authority: Authority,
}

async fn fixture() -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("runtime-server.sqlite");
    let mut connection = current_schema::create_current_schema(&path).await;
    seed_runtime_slots(&mut connection).await;
    connection.close().await.unwrap();
    let store = SqliteRuntimeCustodyStore::open(&path).await.unwrap();
    let authority =
        RuntimeCredentialAuthority::new(store.clone(), SequenceEntropy::default(), FixedClock);
    Fixture {
        _directory: directory,
        path,
        store,
        authority,
    }
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
         ) VALUES
            ('source-1', 'workspace-1', 'workspace', 'open', 'Source One',
             'source one', 'active', 1, 1),
            ('target-2', 'workspace-1', 'workspace', 'open', 'Target Two',
             'target two', 'active', 1, 1)",
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
        agent: AgentId::parse(CONVERSATION_AGENT).unwrap(),
        source: ConversationId::parse(SOURCE).unwrap(),
    }
}

fn agent_scope() -> RuntimeScope {
    RuntimeScope::AgentSlot {
        agent: AgentId::parse(GLOBAL_AGENT).unwrap(),
    }
}

fn daemon(id: &str) -> DaemonInstanceId {
    DaemonInstanceId::parse(id).unwrap()
}

#[derive(Clone, Copy)]
enum StartOutcome {
    Accepted,
    ProvenNotDispatched,
    Ambiguous,
}

#[derive(Clone)]
struct StartTransport {
    outcome: StartOutcome,
    records: Arc<Mutex<Vec<StartRecord>>>,
}

#[derive(Clone)]
struct StartRecord {
    principal: RuntimePrincipal,
    bearer: String,
}

impl StartTransport {
    fn new(outcome: StartOutcome) -> Self {
        Self {
            outcome,
            records: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn last(&self) -> StartRecord {
        self.records.lock().unwrap().last().unwrap().clone()
    }

    fn call_count(&self) -> usize {
        self.records.lock().unwrap().len()
    }
}

#[derive(Debug, Error)]
#[error("transport refused the start")]
struct TransportError;

impl RuntimeStartTransport for StartTransport {
    type Error = TransportError;

    fn dispatch(
        &self,
        handoff: &RuntimeStartHandoff,
    ) -> impl Future<Output = Result<(), RuntimeDispatchFailure<Self::Error>>> + Send {
        let outcome = self.outcome;
        self.records.lock().unwrap().push(StartRecord {
            principal: handoff.principal().clone(),
            bearer: handoff.daemon_auth_token().to_owned(),
        });
        async move {
            match outcome {
                StartOutcome::Accepted => Ok(()),
                StartOutcome::ProvenNotDispatched => {
                    Err(RuntimeDispatchFailure::ProvenNotDispatched(TransportError))
                }
                StartOutcome::Ambiguous => Err(RuntimeDispatchFailure::Ambiguous(TransportError)),
            }
        }
    }
}

#[derive(Clone)]
struct BlockingStartTransport {
    outcome: StartOutcome,
    records: Arc<Mutex<Vec<StartRecord>>>,
    started: Arc<Semaphore>,
    release: Arc<Semaphore>,
}

impl BlockingStartTransport {
    fn new(outcome: StartOutcome) -> Self {
        Self {
            outcome,
            records: Arc::new(Mutex::new(Vec::new())),
            started: Arc::new(Semaphore::new(0)),
            release: Arc::new(Semaphore::new(0)),
        }
    }

    async fn wait_until_started(&self) {
        self.started.acquire().await.unwrap().forget();
    }

    fn record(&self) -> StartRecord {
        self.records.lock().unwrap().last().unwrap().clone()
    }

    fn finish(&self) {
        self.release.add_permits(1);
    }
}

impl RuntimeStartTransport for BlockingStartTransport {
    type Error = TransportError;

    fn dispatch(
        &self,
        handoff: &RuntimeStartHandoff,
    ) -> impl Future<Output = Result<(), RuntimeDispatchFailure<Self::Error>>> + Send {
        let outcome = self.outcome;
        let release = self.release.clone();
        self.records.lock().unwrap().push(StartRecord {
            principal: handoff.principal().clone(),
            bearer: handoff.daemon_auth_token().to_owned(),
        });
        self.started.add_permits(1);
        async move {
            release.acquire().await.unwrap().forget();
            match outcome {
                StartOutcome::Accepted => Ok(()),
                StartOutcome::ProvenNotDispatched => {
                    Err(RuntimeDispatchFailure::ProvenNotDispatched(TransportError))
                }
                StartOutcome::Ambiguous => Err(RuntimeDispatchFailure::Ambiguous(TransportError)),
            }
        }
    }
}

#[derive(Clone, Default)]
struct StopTransport(Arc<Mutex<Vec<RuntimePrincipal>>>);

impl RuntimeStopTransport for StopTransport {
    type Error = Infallible;

    fn stop_exact(
        &self,
        principal: &RuntimePrincipal,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        self.0.lock().unwrap().push(principal.clone());
        std::future::ready(Ok(()))
    }
}

async fn issue(authority: &Authority, scope: RuntimeScope, daemon_instance: &str) -> StartRecord {
    let transport = StartTransport::new(StartOutcome::Accepted);
    let principal = authority
        .dispatch_start(scope, daemon(daemon_instance), origin(), &transport)
        .await
        .unwrap();
    let record = transport.last();
    assert_eq!(record.principal, principal);
    record
}

fn origin() -> ConversationId {
    ConversationId::parse(SOURCE).unwrap()
}

#[tokio::test]
async fn held_transition_marker_refuses_before_custody_mutation_or_transport() {
    let fixture = fixture().await;
    let transitions = SqliteHostAgentStore::open(&fixture.path).await.unwrap();
    let BeginTransition::Started(_lock) = begin_runtime_transition(
        &transitions,
        &AgentId::parse(CONVERSATION_AGENT).unwrap(),
        RuntimeKind::Claude,
        50,
    )
    .await
    .unwrap() else {
        panic!("transition lock");
    };
    let mut db = open(&fixture.path).await;
    let before: (i64, Option<String>, String, i64) = sqlx::query_as(
        "SELECT generation, claimed_by_daemon_instance_id, observed_status, \
                (SELECT count(*) FROM agent_runtime_events WHERE agent_id=?) \
           FROM agent_runtime_slots WHERE agent_id=? AND conversation_id=?",
    )
    .bind(CONVERSATION_AGENT)
    .bind(CONVERSATION_AGENT)
    .bind(SOURCE)
    .fetch_one(&mut db)
    .await
    .unwrap();
    let transport = StartTransport::new(StartOutcome::Accepted);
    let result = fixture
        .authority
        .dispatch_start(
            conversation_scope(),
            daemon("daemon-1"),
            origin(),
            &transport,
        )
        .await;
    assert!(matches!(result, Err(RuntimeStartFailure::MissingSlot)));
    assert_eq!(transport.call_count(), 0);
    let after: (i64, Option<String>, String, i64) = sqlx::query_as(
        "SELECT generation, claimed_by_daemon_instance_id, observed_status, \
                (SELECT count(*) FROM agent_runtime_events WHERE agent_id=?) \
           FROM agent_runtime_slots WHERE agent_id=? AND conversation_id=?",
    )
    .bind(CONVERSATION_AGENT)
    .bind(CONVERSATION_AGENT)
    .bind(SOURCE)
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(after, before);
}

#[tokio::test]
async fn a_real_current_custody_started_retry_installs_and_updates_agent_slot_focus() {
    let fixture = fixture().await;
    let ambiguous = StartTransport::new(StartOutcome::Ambiguous);
    let result = fixture
        .authority
        .dispatch_start(agent_scope(), daemon("daemon-agent"), origin(), &ambiguous)
        .await;
    assert!(matches!(result, Err(RuntimeStartFailure::Ambiguous(_))));
    let principal = ambiguous.last().principal;
    assert_eq!(
        fixture
            .authority
            .agent_slot_presentation_focus(principal.clone())
            .await
            .unwrap(),
        None
    );

    let retry_origin = ConversationId::parse("target-2").unwrap();
    let retry = StartTransport::new(StartOutcome::Accepted);
    let retried = fixture
        .authority
        .dispatch_start(
            agent_scope(),
            daemon("daemon-agent"),
            retry_origin.clone(),
            &retry,
        )
        .await
        .unwrap();
    assert_eq!(retried, principal);
    assert_eq!(retry.last().principal, principal);
    assert_eq!(
        fixture
            .authority
            .agent_slot_presentation_focus(principal.clone())
            .await
            .unwrap(),
        Some(retry_origin)
    );

    let latest_origin = ConversationId::parse("focus-latest").unwrap();
    let repeat = StartTransport::new(StartOutcome::Accepted);
    assert_eq!(
        fixture
            .authority
            .dispatch_start(
                agent_scope(),
                daemon("daemon-agent"),
                latest_origin.clone(),
                &repeat,
            )
            .await
            .unwrap(),
        principal
    );
    assert_eq!(
        fixture
            .authority
            .agent_slot_presentation_focus(principal)
            .await
            .unwrap(),
        Some(latest_origin)
    );
}

#[derive(Clone)]
struct Calls(Arc<AtomicUsize>);

#[derive(Deserialize)]
struct ProbeQuery {
    target: Option<String>,
}

async fn probe(
    Extension(principal): Extension<RuntimePrincipal>,
    Extension(calls): Extension<Calls>,
    Query(query): Query<ProbeQuery>,
) -> Json<Value> {
    calls.0.fetch_add(1, Ordering::SeqCst);
    Json(json!({
        "agent": principal.agent().as_str(),
        "source": principal.source().map(|source| source.as_str()),
        "target": query.target,
        "generation": principal.custody().generation().as_i64(),
        "daemon": principal.custody().daemon_instance().as_str(),
    }))
}

fn app(authority: Authority, calls: Calls) -> Router {
    let product = Router::new()
        .route("/internal/agents/{aid}/probe", get(probe))
        .layer(Extension(calls));
    require_runtime_credential(product, authority)
}

#[tokio::test]
async fn strict_middleware_injects_the_exact_typed_principal_before_product_code() {
    let fixture = fixture().await;
    let conversation = issue(&fixture.authority, conversation_scope(), "daemon-1").await;
    let agent = issue(&fixture.authority, agent_scope(), "daemon-agent").await;
    let calls = Calls(Arc::new(AtomicUsize::new(0)));
    let app = app(fixture.authority.clone(), calls.clone());

    let response = request(
        &app,
        CONVERSATION_AGENT,
        Some(&format!("bEaReR\t {}", conversation.bearer)),
        None,
        Some("target-2"),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(
        response.json(),
        json!({
            "agent": CONVERSATION_AGENT,
            "source": SOURCE,
            "target": "target-2",
            "generation": 1,
            "daemon": "daemon-1",
        })
    );

    let response = request(
        &app,
        GLOBAL_AGENT,
        Some(&format!("Bearer {}", agent.bearer)),
        None,
        None,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(
        response.json(),
        json!({
            "agent": GLOBAL_AGENT,
            "source": null,
            "target": null,
            "generation": 1,
            "daemon": "daemon-agent",
        })
    );

    for (aid, authorization, legacy, expected) in [
        (
            GLOBAL_AGENT,
            Some(format!("Bearer {}", conversation.bearer)),
            None,
            StatusCode::UNAUTHORIZED,
        ),
        (
            CONVERSATION_AGENT,
            Some(conversation.bearer.clone()),
            None,
            StatusCode::UNAUTHORIZED,
        ),
        (
            CONVERSATION_AGENT,
            Some(format!("Basic {}", conversation.bearer)),
            None,
            StatusCode::UNAUTHORIZED,
        ),
        (
            CONVERSATION_AGENT,
            Some("Bearer unknown".to_owned()),
            None,
            StatusCode::UNAUTHORIZED,
        ),
        (CONVERSATION_AGENT, None, None, StatusCode::UNAUTHORIZED),
        (
            CONVERSATION_AGENT,
            Some(format!("Bearer {}", conversation.bearer)),
            Some("forged"),
            StatusCode::BAD_REQUEST,
        ),
    ] {
        let response = request(&app, aid, authorization.as_deref(), legacy, None).await;
        assert_eq!(response.status, expected, "authorization={authorization:?}");
    }
    assert_eq!(calls.0.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn same_generation_has_multiple_tokens_and_exact_finalize_revokes_them_all() {
    let fixture = fixture().await;
    let first = issue(&fixture.authority, conversation_scope(), "daemon-1").await;
    let second = issue(&fixture.authority, conversation_scope(), "daemon-1").await;
    assert_eq!(first.principal, second.principal);
    assert_ne!(first.bearer, second.bearer);

    let calls = Calls(Arc::new(AtomicUsize::new(0)));
    let app = app(fixture.authority.clone(), calls.clone());
    for bearer in [&first.bearer, &second.bearer] {
        assert_eq!(
            request(
                &app,
                CONVERSATION_AGENT,
                Some(&format!("Bearer {bearer}")),
                None,
                None,
            )
            .await
            .status,
            StatusCode::OK
        );
    }

    assert!(
        fixture
            .authority
            .finalize(first.principal.clone(), RuntimeFinalStatus::Idle)
            .await
            .unwrap()
    );
    for bearer in [&first.bearer, &second.bearer] {
        assert_eq!(
            request(
                &app,
                CONVERSATION_AGENT,
                Some(&format!("Bearer {bearer}")),
                None,
                None,
            )
            .await
            .status,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(calls.0.load(Ordering::SeqCst), 2);
    assert_secret_absent_from_database(&fixture.path, [&first.bearer, &second.bearer]);
}

#[tokio::test]
async fn stale_finalizer_does_not_revoke_the_successor() {
    let fixture = fixture().await;
    let stale = issue(&fixture.authority, conversation_scope(), "daemon-1").await;
    let successor = issue(&fixture.authority, conversation_scope(), "daemon-2").await;
    assert_eq!(stale.principal.custody().generation().as_i64(), 1);
    assert_eq!(successor.principal.custody().generation().as_i64(), 2);

    assert!(
        !fixture
            .authority
            .finalize(stale.principal, RuntimeFinalStatus::Crashed)
            .await
            .unwrap()
    );
    let calls = Calls(Arc::new(AtomicUsize::new(0)));
    let app = app(fixture.authority, calls.clone());
    assert_eq!(
        request(
            &app,
            GLOBAL_AGENT,
            Some(&format!("Bearer {}", successor.bearer)),
            None,
            None,
        )
        .await
        .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(
            &app,
            CONVERSATION_AGENT,
            Some(&format!("Bearer {}", successor.bearer)),
            None,
            None,
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(calls.0.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn proven_non_dispatch_compensates_but_ambiguous_dispatch_remains_live() {
    let fixture = fixture().await;
    let proven = StartTransport::new(StartOutcome::ProvenNotDispatched);
    let failure = fixture
        .authority
        .dispatch_start(conversation_scope(), daemon("daemon-1"), origin(), &proven)
        .await
        .unwrap_err();
    assert!(matches!(
        failure,
        RuntimeStartFailure::ProvenNotDispatched(_)
    ));
    let proven_record = proven.last();
    assert!(
        fixture
            .authority
            .current(conversation_scope())
            .await
            .unwrap()
            .is_none()
    );

    let ambiguous = StartTransport::new(StartOutcome::Ambiguous);
    let failure = fixture
        .authority
        .dispatch_start(agent_scope(), daemon("daemon-agent"), origin(), &ambiguous)
        .await
        .unwrap_err();
    assert!(matches!(failure, RuntimeStartFailure::Ambiguous(_)));
    let ambiguous_record = ambiguous.last();
    assert_eq!(
        fixture.authority.current(agent_scope()).await.unwrap(),
        Some(ambiguous_record.principal.clone())
    );

    let calls = Calls(Arc::new(AtomicUsize::new(0)));
    let app = app(fixture.authority.clone(), calls.clone());
    assert_eq!(
        request(
            &app,
            CONVERSATION_AGENT,
            Some(&format!("Bearer {}", proven_record.bearer)),
            None,
            None,
        )
        .await
        .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(
            &app,
            GLOBAL_AGENT,
            Some(&format!("Bearer {}", ambiguous_record.bearer)),
            None,
            None,
        )
        .await
        .status,
        StatusCode::OK
    );

    let established = issue(&fixture.authority, conversation_scope(), "daemon-2").await;
    let retry = StartTransport::new(StartOutcome::ProvenNotDispatched);
    let _ = fixture
        .authority
        .dispatch_start(conversation_scope(), daemon("daemon-2"), origin(), &retry)
        .await
        .unwrap_err();
    assert_eq!(
        request(
            &app,
            CONVERSATION_AGENT,
            Some(&format!("Bearer {}", established.bearer)),
            None,
            None,
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(
        request(
            &app,
            CONVERSATION_AGENT,
            Some(&format!("Bearer {}", retry.last().bearer)),
            None,
            None,
        )
        .await
        .status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn same_scope_lifecycle_is_serial_but_different_scopes_progress_independently() {
    let fixture = fixture().await;
    let blocked = BlockingStartTransport::new(StartOutcome::ProvenNotDispatched);
    let blocked_task = {
        let authority = fixture.authority.clone();
        let transport = blocked.clone();
        tokio::spawn(async move {
            authority
                .dispatch_start(
                    conversation_scope(),
                    daemon("daemon-1"),
                    origin(),
                    &transport,
                )
                .await
        })
    };
    blocked.wait_until_started().await;

    let independent = StartTransport::new(StartOutcome::Accepted);
    let independent_principal = timeout(
        HANG_LIMIT,
        fixture.authority.dispatch_start(
            agent_scope(),
            daemon("daemon-agent"),
            origin(),
            &independent,
        ),
    )
    .await
    .expect("a different scope must not wait for the blocked conversation scope")
    .unwrap();
    assert!(matches!(
        independent_principal,
        RuntimePrincipal::AgentSlot { .. }
    ));

    let successor = StartTransport::new(StartOutcome::Accepted);
    let successor_task = {
        let authority = fixture.authority.clone();
        let transport = successor.clone();
        tokio::spawn(async move {
            authority
                .dispatch_start(
                    conversation_scope(),
                    daemon("daemon-1"),
                    origin(),
                    &transport,
                )
                .await
        })
    };

    blocked.finish();
    assert!(matches!(
        blocked_task.await.unwrap().unwrap_err(),
        RuntimeStartFailure::ProvenNotDispatched(_)
    ));
    let successor_principal = successor_task.await.unwrap().unwrap();
    assert_eq!(successor_principal.custody().generation().as_i64(), 2);
    assert_eq!(successor.call_count(), 1);

    let calls = Calls(Arc::new(AtomicUsize::new(0)));
    let app = app(fixture.authority, calls.clone());
    assert_eq!(
        request(
            &app,
            CONVERSATION_AGENT,
            Some(&format!("Bearer {}", successor.last().bearer)),
            None,
            None,
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(calls.0.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn hydrate_reconcile_cannot_finalize_a_same_scope_handoff_in_flight() {
    let fixture = fixture().await;
    let blocked = BlockingStartTransport::new(StartOutcome::Accepted);
    let dispatch_task = {
        let authority = fixture.authority.clone();
        let transport = blocked.clone();
        tokio::spawn(async move {
            authority
                .dispatch_start(
                    conversation_scope(),
                    daemon("daemon-1"),
                    origin(),
                    &transport,
                )
                .await
        })
    };
    blocked.wait_until_started().await;
    let principal = blocked.record().principal;

    let stop = StopTransport::default();
    let reconcile_task = {
        let authority = fixture.authority.clone();
        let principal = principal.clone();
        let stop = stop.clone();
        tokio::spawn(async move { authority.reconcile_hydrated(principal, &stop).await })
    };

    blocked.finish();
    assert_eq!(dispatch_task.await.unwrap().unwrap(), principal);
    assert_eq!(
        reconcile_task.await.unwrap().unwrap(),
        HydrateCredentialDisposition::CredentialPresent
    );
    assert!(stop.0.lock().unwrap().is_empty());
    assert_eq!(
        fixture
            .authority
            .current(conversation_scope())
            .await
            .unwrap(),
        Some(principal)
    );
}

#[tokio::test]
async fn restart_refuses_the_old_token_then_stops_and_finalizes_before_successor_claim() {
    let fixture = fixture().await;
    let old = issue(&fixture.authority, conversation_scope(), "daemon-old").await;
    let restarted = RuntimeCredentialAuthority::new(
        fixture.store.clone(),
        SequenceEntropy::default(),
        FixedClock,
    );
    let calls = Calls(Arc::new(AtomicUsize::new(0)));
    let restarted_app = app(restarted.clone(), calls.clone());
    assert_eq!(
        request(
            &restarted_app,
            CONVERSATION_AGENT,
            Some(&format!("Bearer {}", old.bearer)),
            None,
            None,
        )
        .await
        .status,
        StatusCode::UNAUTHORIZED
    );

    let hydrated = restarted
        .current(conversation_scope())
        .await
        .unwrap()
        .unwrap();
    let stop = StopTransport::default();
    assert_eq!(
        restarted
            .reconcile_hydrated(hydrated.clone(), &stop)
            .await
            .unwrap(),
        HydrateCredentialDisposition::Finalized
    );
    assert_eq!(
        stop.0.lock().unwrap().as_slice(),
        std::slice::from_ref(&hydrated)
    );
    assert!(
        restarted
            .current(conversation_scope())
            .await
            .unwrap()
            .is_none()
    );

    let successor = issue(&restarted, conversation_scope(), "daemon-next").await;
    assert_eq!(successor.principal.custody().generation().as_i64(), 2);
    assert_eq!(
        restarted.reconcile_hydrated(hydrated, &stop).await.unwrap(),
        HydrateCredentialDisposition::Superseded
    );
    assert_eq!(stop.0.lock().unwrap().len(), 1);
    assert_eq!(
        request(
            &restarted_app,
            CONVERSATION_AGENT,
            Some(&format!("Bearer {}", successor.bearer)),
            None,
            None,
        )
        .await
        .status,
        StatusCode::OK
    );
}

#[tokio::test(flavor = "current_thread")]
async fn unknown_token_skips_custody_but_known_token_surfaces_one_safe_driver_error() {
    let fixture = fixture().await;
    let issued = issue(&fixture.authority, conversation_scope(), "daemon-1").await;
    let mut connection = open(&fixture.path).await;
    sqlx::query("DROP TABLE agent_runtime_slots")
        .execute(&mut connection)
        .await
        .unwrap();
    connection.close().await.unwrap();

    let logs = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .without_time()
        .with_target(false)
        .with_writer(logs.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let calls = Calls(Arc::new(AtomicUsize::new(0)));
    let app = app(fixture.authority, calls.clone());

    let malformed_aid = request(
        &app,
        "%FF",
        Some(&format!("Bearer {}", issued.bearer)),
        None,
        None,
    )
    .await;
    assert_eq!(malformed_aid.status, StatusCode::UNAUTHORIZED);
    assert!(logs.error_events().is_empty());

    let unknown = request(
        &app,
        CONVERSATION_AGENT,
        Some("Bearer never-issued"),
        None,
        None,
    )
    .await;
    assert_eq!(unknown.status, StatusCode::UNAUTHORIZED);
    assert!(logs.error_events().is_empty());

    let wrong_aid = request(
        &app,
        GLOBAL_AGENT,
        Some(&format!("Bearer {}", issued.bearer)),
        None,
        None,
    )
    .await;
    assert_eq!(wrong_aid.status, StatusCode::UNAUTHORIZED);
    assert!(logs.error_events().is_empty());

    let failed = request(
        &app,
        CONVERSATION_AGENT,
        Some(&format!("Bearer {}", issued.bearer)),
        None,
        None,
    )
    .await;
    assert_eq!(failed.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(failed.content_type.starts_with("text/plain"));
    assert_eq!(failed.body, "Internal Server Error");
    assert_eq!(calls.0.load(Ordering::SeqCst), 0);
    assert!(!failed.body.contains(&issued.bearer));

    let errors = logs.error_events();
    assert_eq!(errors.len(), 1);
    assert_eq!(
        errors[0]["fields"]["message"],
        "runtime credential authentication failed"
    );
    let chain = errors[0]["fields"]["error_chain"].as_str().unwrap();
    assert!(chain.contains("no such table: agent_runtime_slots"));
    assert!(!chain.contains(&issued.bearer));
    assert!(!chain.contains(&token_hash_text(&issued.bearer)));
}

#[derive(Clone)]
struct FailingEntropy;

#[derive(Debug, Error)]
#[error("entropy source offline")]
struct EntropyFailure;

impl RuntimeEntropy for FailingEntropy {
    type Error = EntropyFailure;

    fn fill(&self, _bytes: &mut [u8]) -> Result<(), Self::Error> {
        Err(EntropyFailure)
    }
}

#[tokio::test]
async fn entropy_failure_happens_before_a_custody_claim() {
    let fixture = fixture().await;
    let authority = RuntimeCredentialAuthority::new(fixture.store, FailingEntropy, FixedClock);
    let transport = StartTransport::new(StartOutcome::Accepted);
    let failure = authority
        .dispatch_start(
            conversation_scope(),
            daemon("daemon-1"),
            origin(),
            &transport,
        )
        .await
        .unwrap_err();
    assert!(matches!(
        failure,
        RuntimeStartFailure::Prepare(PrepareRuntimeCredentialError::Entropy(_))
    ));
    assert!(error_chain(&failure).contains("entropy source offline"));
    assert_eq!(transport.call_count(), 0);
    assert!(
        authority
            .current(conversation_scope())
            .await
            .unwrap()
            .is_none()
    );
}

struct HttpResponse {
    status: StatusCode,
    content_type: String,
    body: String,
}

impl HttpResponse {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap()
    }
}

async fn request(
    app: &Router,
    aid: &str,
    authorization: Option<&str>,
    legacy_principal: Option<&str>,
    target: Option<&str>,
) -> HttpResponse {
    let query = target.map_or_else(String::new, |target| format!("?target={target}"));
    let mut builder = Request::get(format!("/internal/agents/{aid}/probe{query}"));
    if let Some(authorization) = authorization {
        builder = builder.header("authorization", authorization);
    }
    if let Some(legacy_principal) = legacy_principal {
        builder = builder.header("x-silverhand-principal", legacy_principal);
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    HttpResponse {
        status,
        content_type,
        body: String::from_utf8(body.to_vec()).unwrap(),
    }
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

fn assert_secret_absent_from_database<'a>(
    path: &Path,
    bearers: impl IntoIterator<Item = &'a String>,
) {
    let mut bytes = std::fs::read(path).unwrap();
    let wal = PathBuf::from(format!("{}-wal", path.display()));
    if wal.exists() {
        bytes.extend(std::fs::read(wal).unwrap());
    }
    for bearer in bearers {
        assert!(!contains_bytes(&bytes, bearer.as_bytes()));
        assert!(!contains_bytes(&bytes, token_hash_text(bearer).as_bytes()));
    }
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn token_hash_text(bearer: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(bearer.as_bytes()))
}

fn error_chain(error: &(dyn Error + 'static)) -> String {
    let mut messages = vec![error.to_string()];
    let mut source = error.source();
    while let Some(cause) = source {
        messages.push(cause.to_string());
        source = cause.source();
    }
    messages.join(": ")
}

#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl LogBuffer {
    fn error_events(&self) -> Vec<Value> {
        let bytes = self.0.lock().unwrap().clone();
        String::from_utf8(bytes)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .filter(|event: &Value| event["level"] == "ERROR")
            .collect()
    }
}

struct LogWriter(LogBuffer);

impl Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for LogBuffer {
    type Writer = LogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        LogWriter(self.clone())
    }
}
