//! Task-list router composed with real runtime credentials and SQLite reads.
//! Unlike `task_list_e2e`, the auth cases mint a bearer and exercise the credential
//! perimeter instead of injecting a principal. CountingFetch only observes whether
//! the real read was reached. App registry tests separately own whether the product
//! mounts this route; this test assembles the router itself.
//!
//! The historical-title case also exercises actual startup repair and HTTP reads,
//! keeping both strict rejection before migration and readable canonical data after it.

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
use serde_json::{Value, json};
use silverhand_backend_core::{
    conversation_messaging::messaging::task::{
        TaskCandidateFetch, TaskCandidateQuery, TaskListCandidate,
    },
    foundation::identity::{AgentId, DaemonInstanceId},
    foundation::persistence::PersistenceFailure,
    host_runtime::runtime_credential::RuntimeScope,
};
use silverhand_backend_server::{
    conversation_messaging::messaging::{
        TargetAddressRefusalLog,
        task::{TaskListState, task_list_router},
    },
    foundation::clock::ProcessClock,
    host_runtime::runtime_credential::{
        RuntimeCredentialAuthority, RuntimeDispatchFailure, RuntimeEntropy, RuntimeStartHandoff,
        RuntimeStartTransport, require_runtime_credential,
    },
};
use silverhand_backend_sqlite::{
    conversation_messaging::messaging::{MessagingStore, task::SqliteTaskStore},
    host_runtime::runtime_credential::SqliteRuntimeCustodyStore,
};
use sqlx::Connection;
use tower::ServiceExt;

use crate::current_schema;

const AGENT: &str = "agent-1";

struct NoOpened;

impl silverhand_backend_core::conversation_messaging::messaging::DirectPairOpenedPort for NoOpened {
    type Error = std::convert::Infallible;

    async fn direct_pair_opened(
        &self,
        _fact: &silverhand_backend_core::conversation_messaging::messaging::DirectPairOpenedFact,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

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

/// Wraps the real candidate fetch so the anonymous arm can assert the port was NEVER
/// touched; every admitted read delegates to the real `SqliteTaskStore` SQL.
#[derive(Clone)]
struct CountingFetch {
    inner: SqliteTaskStore,
    fetches: Arc<AtomicUsize>,
}

impl TaskCandidateFetch for CountingFetch {
    async fn fetch_list_candidates(
        &self,
        query: &TaskCandidateQuery,
    ) -> Result<Vec<TaskListCandidate>, PersistenceFailure> {
        self.fetches.fetch_add(1, SeqCst);
        self.inner.fetch_list_candidates(query).await
    }
}

/// The mounted vertical: the real authorities+router guarded by the real runtime credential
/// perimeter, plus the shared fetch counter, the real minted bearer, and the `TempDir` the
/// pools outlive.
struct Mounted {
    app: Router,
    fetches: Arc<AtomicUsize>,
    bearer: String,
    _directory: tempfile::TempDir,
}

/// `agent-1` is a member of `workspace-1`, holds its `read_tasks` capability and is on the
/// `conv-1` roster — the union arm resolves that one board and keeps its one task. The same
/// rows back the runtime credential the authority mints for the `agent` slot.
const SEED: &[&str] = &[
    "INSERT INTO hosts (id, name, kind, created_at) VALUES ('host-1', 'host', 'local', 0)",
    "INSERT INTO users (id, name, display_name, created_at, updated_at) \
     VALUES ('user-1', 'user-1', 'User One', 0, 0)",
    "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, display_name, \
     created_by_user_id, materialization_status, session_scope) \
     VALUES ('agent-1', 'lio', 'claude', 'host-1', 0, 0, 'Lio', 'user-1', 'active', 'agent')",
    "INSERT INTO workspaces (id, name, name_key, owner_user_id, created_at, updated_at) \
     VALUES ('workspace-1', 'Product Space', 'product space', 'user-1', 0, 0)",
    "INSERT INTO workspace_members (workspace_id, agent_id, joined_at) \
     VALUES ('workspace-1', 'agent-1', 0)",
    "INSERT INTO workspace_agent_capabilities (workspace_id, agent_id, capability) \
     VALUES ('workspace-1', 'agent-1', 'read_tasks')",
    "INSERT INTO conversations (id, workspace_id, placement_kind, roster_policy, title, \
     title_key, status, visibility, created_at, updated_at, latest_message_seq) \
     VALUES ('conv000000000001', 'workspace-1', 'workspace', 'open', 'General Chat', \
     'general chat', 'open', 'private', 0, 0, 0)",
    "INSERT INTO conversation_agents (conversation_id, agent_id, joined_at) \
     VALUES ('conv000000000001', 'agent-1', 0)",
    "INSERT INTO tasks (id, workspace_id, task_number, conversation_id, title, status, \
     creator_kind, created_at, board_key) \
     VALUES ('task-1', 'workspace-1', 1, 'conv000000000001', 'first', 'todo', 'system', 100, \
     'ws:workspace-1')",
];

async fn mounted() -> Mounted {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("task-list-mount.sqlite");

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
            silverhand_backend_core::foundation::identity::ConversationId::parse(
                "conversation0001",
            )
            .unwrap(),
            &transport,
        )
        .await
        .unwrap();
    let bearer = transport.0.lock().unwrap().clone().unwrap();

    let fetches = Arc::new(AtomicUsize::new(0));
    let boards = MessagingStore::open(&path).await.unwrap();
    let state = TaskListState {
        targets: boards.target_address(NoOpened, TargetAddressRefusalLog),
        boards,
        fetch: CountingFetch {
            inner: SqliteTaskStore::open(&path).await.unwrap(),
            fetches: fetches.clone(),
        },
    };

    let app = require_runtime_credential(task_list_router(state), authority);
    Mounted {
        app,
        fetches,
        bearer,
        _directory: directory,
    }
}

/// Drives the mounted app against `AGENT`'s own path. `bearer` of `None` omits the
/// `Authorization` header entirely — a genuinely anonymous request, not a malformed token.
async fn call(app: &Router, method: &str, bearer: Option<&str>) -> (StatusCode, Value) {
    call_path(app, method, AGENT, bearer).await
}

/// As `call`, but the URL names `path_aid` — which may differ from the agent the `bearer`
/// was minted for, so the perimeter's path/principal binding can be falsified directly.
async fn call_path(
    app: &Router,
    method: &str,
    path_aid: &str,
    bearer: Option<&str>,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(format!("/internal/agents/{path_aid}/tasks"));
    if let Some(token) = bearer {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
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
async fn an_anonymous_caller_is_refused_before_the_fetch_port_is_touched() {
    let mounted = mounted().await;
    let (status, _) = call(&mounted.app, "GET", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // The perimeter rejected before dispatch: the product read never ran.
    assert_eq!(mounted.fetches.load(SeqCst), 0);
}

#[tokio::test]
async fn a_minted_credential_resolves_to_the_agent_the_body_reads_for() {
    let mounted = mounted().await;
    let (status, body) = call(&mounted.app, "GET", Some(&mounted.bearer)).await;
    assert_eq!(status, StatusCode::OK);
    // The authenticated agent's own board, projected to the exact wire summary.
    assert_eq!(
        body,
        json!({
            "tasks": [{
                "id": "task-1",
                "task_number": 1,
                "place": {
                    "kind": "workspace",
                    "conversation_id": "conv000000000001",
                    "workspace_id": "workspace-1",
                    "channel_name": "General Chat",
                },
                "thread_root_id": null,
                "status": "todo",
                "title": "first",
                "assignee": null,
                "creator": { "type": "system", "id": null, "name": "system" },
                "created_at": 100,
                "closed_at": null,
            }]
        })
    );
    assert_eq!(mounted.fetches.load(SeqCst), 1);
}

#[tokio::test]
async fn a_credential_bound_to_one_agent_cannot_read_another_agents_path() {
    let mounted = mounted().await;
    // `agent-1`'s real minted bearer, but the URL names `agent-2`. The perimeter binds the
    // path aid to the authenticated principal (`principal.agent() != route_agent`), so this
    // is refused BEFORE the handler — proving the route rides the path/principal binding and
    // not a token-only wrapper. `agent-2` need not exist: the mismatch is caught at the
    // perimeter, ahead of any board read.
    let (status, _) = call_path(&mounted.app, "GET", "agent-2", Some(&mounted.bearer)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(mounted.fetches.load(SeqCst), 0);
}

#[tokio::test]
async fn the_mounted_path_answers_get_only() {
    let mounted = mounted().await;
    // Authenticated, so a 405 is the ROUTE's method surface, not the perimeter's 401.
    let (status, _) = call(&mounted.app, "POST", Some(&mounted.bearer)).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
}

#[derive(serde::Deserialize)]
struct MigrationTitleCase {
    name: String,
    raw: String,
    canonical: String,
}

fn valid_migration_titles() -> Vec<MigrationTitleCase> {
    let cases: Vec<Value> = serde_json::from_str(include_str!(
        "../../../../../rust/test-fixtures/fixtures/task-title-migration/cases.json"
    ))
    .unwrap();
    cases
        .into_iter()
        .filter(|case| case["canonical"].is_string())
        .map(|case| serde_json::from_value(case).unwrap())
        .collect()
}

const UNDO_STEP_100: &str = "DROP TRIGGER users_avatar_design_follows_image; \
     DROP TRIGGER users_avatar_design_release; DROP TRIGGER users_avatar_design_delete; \
     DROP TRIGGER agents_avatar_design_follows_image; DROP TRIGGER agents_avatar_design_release; \
     DROP TRIGGER agents_avatar_design_delete; DROP INDEX idx_users_avatar_design; \
     DROP INDEX idx_agents_avatar_design; ALTER TABLE users DROP COLUMN avatar_design_id; \
     ALTER TABLE agents DROP COLUMN avatar_design_id; DROP TABLE avatar_designs; \
     DROP TABLE password_recovery_state;";

async fn seed_step95_titles(path: &std::path::Path, cases: &[MigrationTitleCase]) {
    use sqlx::Executor;

    let mut db = current_schema::create_current_schema(path).await;
    // The snapshot already carries steps 100-101; undo them so the replay from 96 is real.
    db.execute(sqlx::raw_sql(UNDO_STEP_100)).await.unwrap();
    for statement in SEED {
        db.execute(*statement).await.unwrap();
    }
    db.execute("DELETE FROM tasks").await.unwrap();

    for (index, case) in cases.iter().enumerate() {
        let closed = index % 2 == 0;
        sqlx::query(
            "INSERT INTO tasks (id, workspace_id, task_number, conversation_id, title, \
             status, creator_kind, created_at, closed_at, board_key, content) \
             VALUES (?, 'workspace-1', ?, 'conv000000000001', ?, ?, 'system', \
             100, ?, 'ws:workspace-1', 'untouched body')",
        )
        .bind(format!("task-{index}"))
        .bind(index as i64 + 1)
        .bind(&case.raw)
        .bind(if closed { "closed" } else { "done" })
        .bind(closed.then_some(200_i64))
        .execute(&mut db)
        .await
        .unwrap();
    }

    db.execute(
        "CREATE TABLE _migrations (version INTEGER PRIMARY KEY, \
         name TEXT NOT NULL, applied_at INTEGER NOT NULL)",
    )
    .await
    .unwrap();
    let plan: Value = serde_json::from_str(include_str!(
        "../../../../../packages/shared/src/db/rust-migrations.generated.json"
    ))
    .unwrap();
    for step in plan["steps"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|step| step["version"].as_i64().unwrap() <= 95)
    {
        sqlx::query("INSERT INTO _migrations VALUES (?, ?, 1)")
            .bind(step["version"].as_i64().unwrap())
            .bind(step["name"].as_str().unwrap())
            .execute(&mut db)
            .await
            .unwrap();
    }
    db.close().await.unwrap();
}

async fn upgrade_title_copy(old: &std::path::Path, upgraded: &std::path::Path) {
    use silverhand_backend_sqlite::foundation::database_startup::prepare_database;

    std::fs::copy(old, upgraded).unwrap();
    let first = prepare_database(upgraded, true).await.unwrap();
    assert_eq!(first.applied_steps.first(), Some(&96));

    let second = prepare_database(upgraded, true).await.unwrap();
    assert!(second.applied_steps.is_empty());
}

#[derive(Clone)]
struct NoopMigrationAudit;

impl silverhand_backend_core::foundation::cli_invoke_audit::CliInvokeAuditSink
    for NoopMigrationAudit
{
    fn record(&self, _: silverhand_backend_core::foundation::cli_invoke_audit::CliInvoke) {}
}

async fn title_read_router(path: &std::path::Path) -> Router {
    use axum::Extension;
    use silverhand_backend_core::host_runtime::runtime_credential::{
        RuntimeCustody, RuntimeGeneration, RuntimePrincipal,
    };
    use silverhand_backend_server::{
        conversation_messaging::messaging::task::{TaskDetailState, task_detail_router},
        foundation::cli_invoke_observer::audit_cli_invocations,
    };

    let boards = MessagingStore::open(path).await.unwrap();
    let tasks = SqliteTaskStore::open(path).await.unwrap();
    let routes = task_list_router(TaskListState {
        boards: boards.clone(),
        fetch: tasks.clone(),
        targets: boards.target_address(NoOpened, TargetAddressRefusalLog),
    })
    .merge(task_detail_router(TaskDetailState {
        boards,
        read: tasks,
    }));

    let principal = RuntimePrincipal::for_scope(
        RuntimeScope::AgentSlot {
            agent: AgentId::parse(AGENT).unwrap(),
        },
        RuntimeCustody::new(
            RuntimeGeneration::new(1).unwrap(),
            DaemonInstanceId::parse("daemon-1").unwrap(),
        ),
    );
    audit_cli_invocations(routes, NoopMigrationAudit).layer(Extension(principal))
}

async fn title_get(app: &Router, suffix: &str) -> axum::response::Response {
    let request = Request::builder()
        .uri(format!("/internal/agents/{AGENT}/{suffix}"))
        .body(Body::empty())
        .unwrap();
    app.clone().oneshot(request).await.unwrap()
}

async fn title_json(response: axum::response::Response) -> Value {
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn assert_title_list(app: &Router, repaired: bool, count: usize) {
    let response = title_get(app, "tasks?limit=100&workspace_id=workspace-1").await;
    let expected = if repaired {
        StatusCode::OK
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    assert_eq!(
        response.status(),
        expected,
        "title list: repaired={repaired}"
    );

    if repaired {
        let body = title_json(response).await;
        assert_eq!(body["tasks"].as_array().unwrap().len(), count);
    }
}

async fn assert_title_details(app: &Router, repaired: bool, cases: &[MigrationTitleCase]) {
    for (index, case) in cases.iter().enumerate() {
        let response = title_get(app, &format!("tasks/detail?task_id=task-{index}")).await;
        let expected = if repaired || case.raw == case.canonical {
            StatusCode::OK
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        };
        assert_eq!(
            response.status(),
            expected,
            "{}: repaired={repaired}",
            case.name
        );

        if repaired {
            let body = title_json(response).await;
            assert_eq!(body["task"]["title"], case.canonical, "{}", case.name);
            assert_eq!(body["task"]["content"], "untouched body");
        }
    }
}

fn assert_title_diagnostics(logs: &log_capture::LogBuffer) {
    let events = logs.events();
    assert!(
        events
            .iter()
            .any(|event| event["fields"]["field"] == "tasks.title:non_canonical")
    );
    assert!(
        events
            .iter()
            .any(|event| event["fields"]["stage"] == "fetch_candidates")
    );
    assert!(
        events
            .iter()
            .any(|event| event["fields"]["stage"] == "read_point")
    );

    for event in events
        .iter()
        .filter(|event| event["fields"]["stage"].is_string())
    {
        assert!(
            event["span"]["request_id"]
                .as_str()
                .unwrap()
                .starts_with("rust-")
        );
        let serialized = event.to_string();
        assert!(!serialized.contains("untouched body"));
        assert!(!serialized.contains("把  system"));
        assert!(!serialized.contains("task-0"));
    }
}

/// Compare a synthetic step95 database with its upgraded copy through real HTTP
/// handlers. The untouched database proves strict reads remain strict.
#[tokio::test]
async fn historical_titles_fail_strict_reads_then_upgrade_restores_list_and_detail() {
    use tracing::instrument::WithSubscriber;

    // Setup and migration use only isolated synthetic data.
    let cases = valid_migration_titles();
    let directory = tempfile::tempdir().unwrap();
    let old = directory.path().join("old.sqlite");
    let upgraded = directory.path().join("upgrade.sqlite");
    seed_step95_titles(&old, &cases).await;
    upgrade_title_copy(&old, &upgraded).await;

    // Capture HTTP failures at the production default level.
    let logs = log_capture::LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::ERROR)
        .with_writer(logs.clone())
        .finish();
    async {
        for (path, repaired) in [(&old, false), (&upgraded, true)] {
            let app = title_read_router(path).await;
            assert_title_list(&app, repaired, cases.len()).await;
            assert_title_details(&app, repaired, &cases).await;
        }
    }
    .with_subscriber(subscriber)
    .await;

    // Diagnostics expose stage/field and correlation, never persisted content.
    assert_title_diagnostics(&logs);
}

use crate::log_capture;
