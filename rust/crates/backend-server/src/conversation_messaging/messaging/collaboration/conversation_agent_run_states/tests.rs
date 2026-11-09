use std::{
    convert::Infallible,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use serde_json::{Number, Value, json};
use silverhand_backend_core::{
    conversation_messaging::messaging::{
        collaboration::{
            channel_directory::{ChannelVisibility, DirectoryRoster},
            channel_lifecycle::{
                ChannelDetailFact, ChannelDetailReadPort, ChannelDetailRefusal, ChannelLifecycleRow,
            },
        },
        conversation_title::CanonicalConversationTitle,
    },
    foundation::identity::{AgentId, ConversationId, UserId, WorkspaceId},
    identity_access::access::WorkspaceRole,
};
use silverhand_backend_sqlite::conversation_messaging::messaging::MessagingStore;
use sqlx::{Connection, Executor, SqliteConnection, sqlite::SqliteConnectOptions};
use tower::ServiceExt;

use super::*;
use crate::host_runtime::runtime_event_settlement::{
    AgentActivitySnippetKind, AgentRunStatus, AgentRunTerminalReason, DaemonRuntimeEventError,
};

#[derive(Clone)]
struct FakeAdmission {
    outcome: Result<ChannelDetailFact, ChannelDetailRefusal>,
    calls: Arc<AtomicUsize>,
}

impl ChannelDetailReadPort for FakeAdmission {
    type Error = Infallible;

    async fn detail(
        &self,
        _: &UserId,
        _: &ConversationId,
    ) -> Result<Result<ChannelDetailFact, ChannelDetailRefusal>, Self::Error> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.outcome.clone())
    }
}

#[derive(Clone)]
struct FakeSnapshot {
    states: Vec<AgentRunStateProjection>,
    calls: Arc<AtomicUsize>,
}

impl ConversationAgentRunStateSource for FakeSnapshot {
    type Error = Infallible;

    async fn snapshot(
        &self,
        _: &ConversationId,
    ) -> Result<Vec<AgentRunStateProjection>, Self::Error> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.states.clone())
    }
}

fn admitted(viewer: &str, conversation: &str) -> ChannelDetailFact {
    ChannelDetailFact::projected(
        UserId::parse(viewer).unwrap(),
        ChannelLifecycleRow::workspace_placed(
            ConversationId::parse(conversation).unwrap(),
            WorkspaceId::parse("w").unwrap(),
            DirectoryRoster::Open,
            CanonicalConversationTitle::parse("General").unwrap(),
            None,
            None,
            0,
            1,
            1,
            ChannelVisibility::Private,
        )
        .unwrap(),
        true,
        Some(WorkspaceRole::Member),
        1,
    )
    .unwrap()
}

fn projection(conversation: &str) -> AgentRunStateProjection {
    let agent = AgentId::parse("agent-1").unwrap();
    let conversation = ConversationId::parse(conversation).unwrap();
    AgentRunStateProjection::from_daemon(DaemonAgentRunStateSnapshot {
        source_agent: agent.clone(),
        source_conversation: conversation.clone(),
        state: DaemonAgentRunState {
            run_id: "run-1".to_owned(),
            agent_id: agent.as_str().to_owned(),
            conversation_id: conversation.as_str().to_owned(),
            status: AgentRunStatus::ToolRunning,
            started_at: Number::from(10),
            updated_at: Number::from(20),
            last_activity_at: Number::from(19),
            terminal_reason: Some(AgentRunTerminalReason::TurnEnded),
            activity_snippet: Some("editing".to_owned()),
            activity_snippet_kind: Some(AgentActivitySnippetKind::Text),
            tool_call_counts: Some(std::collections::BTreeMap::from([(
                "shell".to_owned(),
                Number::from(2),
            )])),
            compacting: Some(true),
            compacting_since: Some(Number::from(18)),
            compaction_stale: Some(false),
            detail: Some("detail".to_owned()),
        },
    })
    .unwrap()
}

async fn response(
    principal: bool,
    admission: FakeAdmission,
    snapshot: FakeSnapshot,
) -> (u16, Value) {
    let mut request = Request::builder()
        .uri("/api/channels/c/agent-run-states")
        .body(Body::empty())
        .unwrap();
    if principal {
        request
            .extensions_mut()
            .insert(UserId::parse("viewer").unwrap());
    }
    let response = router(admission, snapshot).oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

#[tokio::test]
async fn missing_principal_fails_closed_without_product_or_owner_calls() {
    let admission_calls = Arc::new(AtomicUsize::new(0));
    let snapshot_calls = Arc::new(AtomicUsize::new(0));
    let (status, body) = response(
        false,
        FakeAdmission {
            outcome: Ok(admitted("viewer", "c")),
            calls: admission_calls.clone(),
        },
        FakeSnapshot {
            states: vec![projection("c")],
            calls: snapshot_calls.clone(),
        },
    )
    .await;

    assert_eq!(status, 500);
    assert_eq!(body, json!({"error":"internal error"}));
    assert_eq!(admission_calls.load(Ordering::SeqCst), 0);
    assert_eq!(snapshot_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn admission_refusals_never_touch_the_live_owner() {
    for (refusal, expected_status, expected_error) in [
        (ChannelDetailRefusal::NotFound, 404, "channel not found"),
        (ChannelDetailRefusal::Forbidden, 403, "forbidden"),
    ] {
        let snapshot_calls = Arc::new(AtomicUsize::new(0));
        let (status, body) = response(
            true,
            FakeAdmission {
                outcome: Err(refusal),
                calls: Arc::new(AtomicUsize::new(0)),
            },
            FakeSnapshot {
                states: vec![projection("c")],
                calls: snapshot_calls.clone(),
            },
        )
        .await;
        assert_eq!(status, expected_status);
        assert_eq!(body, json!({"error":expected_error}));
        assert_eq!(snapshot_calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn admission_and_each_run_state_are_bound_before_wire() {
    let snapshot_calls = Arc::new(AtomicUsize::new(0));
    let (status, body) = response(
        true,
        FakeAdmission {
            outcome: Ok(admitted("viewer", "other")),
            calls: Arc::new(AtomicUsize::new(0)),
        },
        FakeSnapshot {
            states: vec![projection("c")],
            calls: snapshot_calls.clone(),
        },
    )
    .await;
    assert_eq!(status, 500);
    assert_eq!(body, json!({"error":"internal error"}));
    assert_eq!(snapshot_calls.load(Ordering::SeqCst), 0);

    let (status, body) = response(
        true,
        FakeAdmission {
            outcome: Ok(admitted("viewer", "c")),
            calls: Arc::new(AtomicUsize::new(0)),
        },
        FakeSnapshot {
            states: vec![projection("foreign")],
            calls: snapshot_calls.clone(),
        },
    )
    .await;
    assert_eq!(status, 500);
    assert_eq!(body, json!({"error":"internal error"}));
    assert_eq!(snapshot_calls.load(Ordering::SeqCst), 1);

    let mut spliced_agent = projection("c");
    spliced_agent.source_agent = AgentId::parse("outer-agent").unwrap();
    let (status, body) = response(
        true,
        FakeAdmission {
            outcome: Ok(admitted("viewer", "c")),
            calls: Arc::new(AtomicUsize::new(0)),
        },
        FakeSnapshot {
            states: vec![spliced_agent],
            calls: snapshot_calls.clone(),
        },
    )
    .await;
    assert_eq!(status, 500);
    assert_eq!(body, json!({"error":"internal error"}));
    assert_eq!(snapshot_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn success_preserves_the_complete_optional_wire() {
    let (status, body) = response(
        true,
        FakeAdmission {
            outcome: Ok(admitted("viewer", "c")),
            calls: Arc::new(AtomicUsize::new(0)),
        },
        FakeSnapshot {
            states: vec![projection("c")],
            calls: Arc::new(AtomicUsize::new(0)),
        },
    )
    .await;

    assert_eq!(status, 200);
    assert_eq!(
        body,
        json!({"states":[{
            "runId":"run-1",
            "agentId":"agent-1",
            "conversationId":"c",
            "status":"tool_running",
            "startedAt":10,
            "updatedAt":20,
            "lastActivityAt":19,
            "terminalReason":"turn_ended",
            "activitySnippet":"editing",
            "activitySnippetKind":"text",
            "toolCallCounts":{"shell":2},
            "compacting":true,
            "compactingSince":18,
            "compactionStale":false,
            "detail":"detail"
        }]})
    );
}

async fn sqlite_admission(
    visibility: &str,
    archived_at: Option<i64>,
    conversation_member: bool,
) -> (tempfile::TempDir, MessagingStore) {
    const SCHEMA: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../rust/test-fixtures/migration-baseline/schema-current.sql"
    ));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agent-run-states-admission.sqlite");
    let options = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true)
        .foreign_keys(true);
    let mut db = SqliteConnection::connect_with(&options).await.unwrap();
    db.execute(sqlx::raw_sql(SCHEMA)).await.unwrap();
    db.execute("INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES('viewer','run-states-viewer','Viewer',1,1)")
        .await
        .unwrap();
    db.execute("INSERT INTO workspaces(id,name,name_key,created_at,updated_at) VALUES('w','Workspace','run-states-workspace',1,1)")
        .await
        .unwrap();
    db.execute(
        "INSERT INTO workspace_users(workspace_id,user_id,joined_at) VALUES('w','viewer',1)",
    )
    .await
    .unwrap();
    sqlx::query("INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,visibility,archived_at,created_at,updated_at) VALUES('c','w','workspace','open','General','general',?,?,1,1)")
        .bind(visibility)
        .bind(archived_at)
        .execute(&mut db)
        .await
        .unwrap();
    if conversation_member {
        db.execute("INSERT INTO conversation_users(conversation_id,user_id,joined_at) VALUES('c','viewer',1)")
            .await
            .unwrap();
    }
    db.close().await.unwrap();
    let store = MessagingStore::open(path).await.unwrap();
    (dir, store)
}

#[tokio::test]
async fn production_visibility_authority_gates_the_live_owner() {
    for (visibility, archived_at, member, expected_status, expected_calls) in [
        ("public", None, false, 200, 1),
        ("private", None, false, 403, 0),
        ("public", Some(9), false, 403, 0),
        ("private", None, true, 200, 1),
    ] {
        let (_dir, store) = sqlite_admission(visibility, archived_at, member).await;
        let calls = Arc::new(AtomicUsize::new(0));
        let mut request = Request::builder()
            .uri("/api/channels/c/agent-run-states")
            .body(Body::empty())
            .unwrap();
        request
            .extensions_mut()
            .insert(UserId::parse("viewer").unwrap());
        let response = router(
            store.channel_lifecycle(),
            FakeSnapshot {
                states: vec![projection("c")],
                calls: calls.clone(),
            },
        )
        .oneshot(request)
        .await
        .unwrap();
        assert_eq!(response.status().as_u16(), expected_status);
        assert_eq!(calls.load(Ordering::SeqCst), expected_calls);
    }
}

#[test]
fn owner_snapshot_failure_preserves_the_concrete_daemon_cause_chain() {
    let failure = RunStateSnapshotFailure::owner(DaemonRuntimeEventError::StatePoisoned);
    assert_eq!(
        crate::foundation::error::describe(&failure),
        "live agent run-state owner snapshot failed: daemon runtime event state lock was poisoned"
    );
}
