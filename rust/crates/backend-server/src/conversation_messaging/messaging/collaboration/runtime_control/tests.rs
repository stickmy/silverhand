use std::{
    io::{self, Write},
    sync::{Arc, Mutex},
};

use axum::{
    body::{Body, to_bytes},
    extract::Extension,
    http::{Request, StatusCode},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use silverhand_backend_core::{
    conversation_messaging::messaging::collaboration::{
        ClearSessionOutcome, ClearSessionProviderRefusal, CliSessionId, HardStopRuntimeOutcome,
        HardStopRuntimeProviderRefusal, RuntimeControlPort, RuntimePlacementRefusal,
        RuntimeSeatTarget, RuntimeStartTarget, RuntimeUnavailable, RuntimeUnavailableReason,
        StartRuntimeOutcome, StartRuntimeProviderRefusal,
    },
    foundation::identity::UserId,
};
use silverhand_backend_sqlite::conversation_messaging::messaging::MessagingStore;
use sqlx::{Connection, Executor, SqliteConnection};
use tower::ServiceExt;
use tracing_subscriber::fmt::MakeWriter;

use super::{CLEAR_SESSION_PATH, HARD_STOP_PATH, START_PATH, router};

#[derive(Clone)]
enum ProviderMode {
    Success,
    Placement(RuntimePlacementRefusal),
    StartRefusal(StartRuntimeProviderRefusal),
    ClearRefusal(ClearSessionProviderRefusal),
}

#[derive(Clone)]
struct Provider {
    mode: ProviderMode,
    calls: Arc<Mutex<Vec<&'static str>>>,
}

#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl LogBuffer {
    fn events(&self) -> Vec<Value> {
        let bytes = self.0.lock().unwrap().clone();
        String::from_utf8(bytes)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
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

#[derive(Debug, thiserror::Error)]
#[error("provider driver failed")]
struct ProviderFailure;

impl RuntimeControlPort for Provider {
    type Error = ProviderFailure;

    async fn start(
        &self,
        _target: &RuntimeStartTarget,
    ) -> Result<Result<StartRuntimeOutcome, StartRuntimeProviderRefusal>, Self::Error> {
        self.calls.lock().unwrap().push("start");
        match &self.mode {
            ProviderMode::Placement(refusal) => {
                Ok(Err(StartRuntimeProviderRefusal::Placement(*refusal)))
            }
            ProviderMode::StartRefusal(refusal) => Ok(Err(refusal.clone())),
            _ => Ok(Ok(StartRuntimeOutcome)),
        }
    }

    async fn hard_stop(
        &self,
        _target: &RuntimeSeatTarget,
    ) -> Result<Result<HardStopRuntimeOutcome, HardStopRuntimeProviderRefusal>, Self::Error> {
        self.calls.lock().unwrap().push("hard_stop");
        match self.mode {
            ProviderMode::Placement(refusal) => {
                Ok(Err(HardStopRuntimeProviderRefusal::Placement(refusal)))
            }
            _ => Ok(Ok(HardStopRuntimeOutcome)),
        }
    }

    async fn clear_session(
        &self,
        _target: &RuntimeSeatTarget,
    ) -> Result<Result<ClearSessionOutcome, ClearSessionProviderRefusal>, Self::Error> {
        self.calls.lock().unwrap().push("clear_session");
        match self.mode {
            ProviderMode::Placement(refusal) => {
                Ok(Err(ClearSessionProviderRefusal::Placement(refusal)))
            }
            ProviderMode::ClearRefusal(refusal) => Ok(Err(refusal)),
            _ => Ok(Ok(ClearSessionOutcome::committed(
                Some(CliSessionId::parse("cli-previous").unwrap()),
                true,
            ))),
        }
    }
}

async fn seeded() -> (tempfile::TempDir, MessagingStore) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("runtime-control-server.sqlite");
    let mut db = crate::current_schema::create_current_schema(&path).await;
    db.execute(sqlx::raw_sql(
        concat!(
            "INSERT INTO users (id, name, display_name, created_at, updated_at) VALUES ('user-1', 'owner', 'Owner', 0, 0), ('user-2', 'teammate', 'Teammate', 0, 0); ",
            "INSERT INTO hosts (id, name, kind, created_at) VALUES ('host-1', 'Host', 'local', 0); ",
            seed_agent!(
                id = "agent-1",
                name = "agent",
                runtime = "codex",
                host_id = "host-1",
                created_at = 0,
                updated_at = 0,
                display_name = "Agent",
                created_by_user_id = "user-1",
            ),
            "; ",
            seed_agent!(
                id = "agent-2",
                name = "agent-two",
                runtime = "codex",
                host_id = "host-1",
                created_at = 0,
                updated_at = 0,
                display_name = "Agent Two",
                created_by_user_id = "user-1",
            ),
            "; ",
            "INSERT INTO workspaces (id, name, name_key, owner_user_id, created_at, updated_at) VALUES ('workspace-1', 'Space', 'space', 'user-1', 0, 0); ",
            "INSERT INTO workspace_users (workspace_id, user_id, joined_at, role) VALUES ('workspace-1', 'user-1', 0, 'owner'), ('workspace-1', 'user-2', 0, 'member'); ",
            seed_channel!(
                id = "conversation-1",
                workspace_id = "workspace-1",
                title = "General",
                title_key = "general",
                created_at = 0,
                updated_at = 0,
            ),
            "; ",
            "INSERT INTO conversation_users (conversation_id, user_id, joined_at) VALUES ('conversation-1', 'user-1', 0), ('conversation-1', 'user-2', 0); ",
            "INSERT INTO conversation_agents (conversation_id, agent_id, joined_at) VALUES ('conversation-1', 'agent-1', 0); ",
        ),
    ))
    .await
    .unwrap();
    db.close().await.unwrap();
    let store = MessagingStore::open(&path).await.unwrap();
    (directory, store)
}

fn provider(mode: ProviderMode) -> Provider {
    Provider {
        mode,
        calls: Arc::new(Mutex::new(Vec::new())),
    }
}

#[derive(Deserialize)]
struct WireFixtureCase {
    name: String,
    route: String,
    setup: String,
    provider: String,
    body: Option<Value>,
    expected_status: u16,
    expected_body: Value,
}

fn wire_fixture() -> Vec<WireFixtureCase> {
    serde_json::from_str(include_str!("fixtures/runtime_control_wire.json")).unwrap()
}

fn placement(provider: &str) -> Option<RuntimePlacementRefusal> {
    Some(match provider {
        "placement_host_missing" => RuntimePlacementRefusal::HostMissing,
        "placement_host_not_owned" => RuntimePlacementRefusal::HostNotOwned,
        "placement_host_pending_attach" => RuntimePlacementRefusal::HostPendingAttach,
        "placement_host_runtime_disabled" => RuntimePlacementRefusal::HostRuntimeDisabled,
        "placement_host_revoked" => RuntimePlacementRefusal::HostRevoked,
        "placement_local_host_unbound" => RuntimePlacementRefusal::LocalHostUnbound,
        "placement_local_host_ambiguous" => RuntimePlacementRefusal::LocalHostAmbiguous,
        "placement_host_attach_pending" => RuntimePlacementRefusal::HostAttachPending,
        _ => return None,
    })
}

fn provider_mode(kind: &str) -> ProviderMode {
    if let Some(refusal) = placement(kind) {
        return ProviderMode::Placement(refusal);
    }
    match kind {
        "success" => ProviderMode::Success,
        "start_workspace_not_provisioned" => {
            ProviderMode::StartRefusal(StartRuntimeProviderRefusal::WorkspaceNotProvisioned)
        }
        "start_workspace_bundle_missing" => {
            ProviderMode::StartRefusal(StartRuntimeProviderRefusal::WorkspaceBundleMissing)
        }
        "start_registry_unavailable" => {
            ProviderMode::StartRefusal(StartRuntimeProviderRefusal::RegistryUnavailable)
        }
        "start_shutting_down" => {
            ProviderMode::StartRefusal(StartRuntimeProviderRefusal::ShuttingDown)
        }
        "start_runtime_binary_missing" => ProviderMode::StartRefusal(
            StartRuntimeProviderRefusal::RuntimeUnavailable(RuntimeUnavailable {
                reason: RuntimeUnavailableReason::BinaryMissing,
                message: "Pi is not installed on this computer.".into(),
            }),
        ),
        "start_runtime_version_unsupported" => ProviderMode::StartRefusal(
            StartRuntimeProviderRefusal::RuntimeUnavailable(RuntimeUnavailable {
                reason: RuntimeUnavailableReason::VersionUnsupported,
                message: "Grok Build 1.0.23 is not supported. Update to 1.0.24 or newer.".into(),
            }),
        ),
        "clear_timeout" => ProviderMode::ClearRefusal(ClearSessionProviderRefusal::Timeout),
        "clear_in_progress" => ProviderMode::ClearRefusal(ClearSessionProviderRefusal::InProgress),
        unknown => panic!("fixture names an unknown provider mode: {unknown}"),
    }
}

fn expand_fixture<T: Serialize>(value: &T, conversation: &str, agent: &str) -> Value {
    serde_json::from_str(
        &serde_json::to_string(value)
            .unwrap()
            .replace("{conversation_id}", conversation)
            .replace("{agent_id}", agent),
    )
    .unwrap()
}

fn path(template: &str, conversation: &str, agent: &str) -> String {
    template
        .replace("{id}", conversation)
        .replace("{aid}", agent)
}

async fn request(
    app: axum::Router,
    uri: String,
    actor: &str,
    body: Option<Value>,
) -> (StatusCode, String) {
    let mut builder = Request::builder().method("POST").uri(uri);
    let body = match body {
        Some(value) => {
            builder = builder.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&value).unwrap())
        }
        None => Body::empty(),
    };
    let response = app
        .layer(Extension(UserId::parse(actor).unwrap()))
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

#[tokio::test]
async fn the_ts_signed_wire_fixture_crosses_the_real_rust_router() {
    for case in wire_fixture() {
        let (_directory, store) = seeded().await;

        let provider = provider(provider_mode(&case.provider));
        let calls = Arc::clone(&provider.calls);
        let app = router(store.runtime_control_admission(), provider);
        let conversation = if case.setup == "missing_conversation" {
            "missing"
        } else {
            "conversation-1"
        };
        let agent = match case.setup.as_str() {
            "missing_agent" => "missing",
            "unseated_agent" => "agent-2",
            _ => "agent-1",
        };
        let actor = if case.setup == "non_creator" {
            "user-2"
        } else {
            "user-1"
        };
        let route = match case.route.as_str() {
            "start" => START_PATH,
            "hard_stop" => HARD_STOP_PATH,
            "clear_session" => CLEAR_SESSION_PATH,
            unknown => panic!("fixture names an unknown route: {unknown}"),
        };
        let (status, body) = request(
            app,
            path(route, conversation, agent),
            actor,
            case.body.clone(),
        )
        .await;
        assert_eq!(status.as_u16(), case.expected_status, "{}", case.name);
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap(),
            expand_fixture(&case.expected_body, conversation, agent),
            "{}",
            case.name
        );
        let should_reach_provider = case.setup == "normal" && case.name != "legacy_soft_stop";
        let expected_calls = if should_reach_provider {
            vec![case.route.as_str()]
        } else {
            vec![]
        };
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            expected_calls,
            "{}",
            case.name
        );
    }
}

#[tokio::test]
async fn legacy_directive_presence_is_rejected_even_when_null() {
    let (_directory, store) = seeded().await;
    let provider = provider(ProviderMode::Success);
    let calls = Arc::clone(&provider.calls);
    let app = router(store.runtime_control_admission(), provider);

    let (status, body) = request(
        app,
        path(HARD_STOP_PATH, "conversation-1", "agent-1"),
        "user-1",
        Some(json!({ "directive": null })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap(),
        json!({ "error": "soft stop was removed; only hard stop is supported" })
    );
    assert!(calls.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn a_real_sqlite_driver_failure_is_safe_and_never_reaches_the_provider() {
    let (directory, store) = seeded().await;
    let provider = provider(ProviderMode::Success);
    let calls = Arc::clone(&provider.calls);
    let app = router(store.runtime_control_admission(), provider);

    let path_to_db = directory.path().join("runtime-control-server.sqlite");
    let mut breaker = SqliteConnection::connect(&format!("sqlite:{}", path_to_db.display()))
        .await
        .unwrap();
    breaker.execute("PRAGMA foreign_keys = OFF").await.unwrap();
    breaker.execute("DROP TABLE agents").await.unwrap();
    breaker.close().await.unwrap();

    let logs = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .without_time()
        .with_target(false)
        .with_writer(logs.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let (status, body) = request(
        app,
        path(START_PATH, "conversation-1", "agent-1"),
        "user-1",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body, "Internal Server Error");
    assert!(calls.lock().unwrap().is_empty());
    let errors = logs
        .events()
        .into_iter()
        .filter(|event| event["level"] == "ERROR")
        .collect::<Vec<_>>();
    assert_eq!(errors.len(), 1);
    assert_eq!(
        errors[0]["fields"]["message"],
        "conversation runtime start failed"
    );
    let chain = errors[0]["fields"]["error_chain"].as_str().unwrap();
    assert!(chain.contains("database operation failed"), "{chain}");
    assert!(chain.contains("no such table: agents"), "{chain}");
}
