use std::{
    convert::Infallible,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use silverhand_backend_core::conversation_messaging::messaging::collaboration::conversation_agent_activity::{
    ActivityCreatedAt, ConversationActivityEvent, ConversationActivityKind,
    ConversationAgentActivityPage,
};
use sqlx::{Connection, Executor, SqliteConnection, sqlite::SqliteConnectOptions};
use tower::ServiceExt;

#[derive(Clone)]
struct FakeRead {
    admission: Result<(), ConversationAgentActivityRefusal>,
    read_refusal: Option<ConversationAgentActivityRefusal>,
    projected_viewer: &'static str,
    projected_conversation: &'static str,
    projected_agent: &'static str,
    payload: &'static str,
    admit_calls: Arc<AtomicUsize>,
    read_calls: Arc<AtomicUsize>,
    pages: Arc<Mutex<Vec<ActivityPageRequest>>>,
}

impl FakeRead {
    fn successful() -> Self {
        Self {
            admission: Ok(()),
            read_refusal: None,
            projected_viewer: "viewer",
            projected_conversation: "c",
            projected_agent: "a",
            payload: "{\"text\":\"hello\"}",
            admit_calls: Arc::new(AtomicUsize::new(0)),
            read_calls: Arc::new(AtomicUsize::new(0)),
            pages: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl ConversationAgentActivityReadPort for FakeRead {
    type Error = Infallible;

    async fn admit(
        &self,
        _: &UserId,
        _: &ConversationId,
        _: &AgentId,
    ) -> Result<Result<(), ConversationAgentActivityRefusal>, Self::Error> {
        self.admit_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.admission)
    }

    async fn read(
        &self,
        _: &UserId,
        _: &ConversationId,
        _: &AgentId,
        page: ActivityPageRequest,
    ) -> Result<Result<ConversationAgentActivityPage, ConversationAgentActivityRefusal>, Self::Error>
    {
        self.read_calls.fetch_add(1, Ordering::SeqCst);
        self.pages.lock().unwrap().push(page);
        if let Some(refusal) = self.read_refusal {
            return Ok(Err(refusal));
        }
        let conversation = ConversationId::parse(self.projected_conversation).unwrap();
        let agent = AgentId::parse(self.projected_agent).unwrap();
        Ok(Ok(ConversationAgentActivityPage::projected(
            UserId::parse(self.projected_viewer).unwrap(),
            conversation.clone(),
            agent.clone(),
            vec![ConversationActivityEvent::projected(
                ActivityEventId::new(7).unwrap(),
                conversation,
                Some(agent),
                ConversationActivityKind::Text,
                ActivityPayload::object_json(self.payload.to_owned()),
                ActivityCreatedAt::new(9).unwrap(),
            )],
            false,
        )
        .unwrap()))
    }
}

async fn call(port: FakeRead, uri: &str, authenticated: bool) -> (StatusCode, Value) {
    let mut request = Request::builder().uri(uri).body(Body::empty()).unwrap();
    if authenticated {
        request
            .extensions_mut()
            .insert(UserId::parse("viewer").unwrap());
    }
    let response = router(port).oneshot(request).await.unwrap();
    let status = response.status();
    let body =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    (status, body)
}

#[tokio::test]
async fn auth_and_product_authority_precede_query_and_projection() {
    let port = FakeRead::successful();
    let (status, body) = call(
        port.clone(),
        "/api/conversations/c/agents/a/activity?limit=bad",
        false,
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body, json!({"error":"internal error"}));
    assert_eq!(port.admit_calls.load(Ordering::SeqCst), 0);
    assert_eq!(port.read_calls.load(Ordering::SeqCst), 0);

    for (refusal, expected_status, expected_body) in [
        (
            ConversationAgentActivityRefusal::NotFound,
            StatusCode::NOT_FOUND,
            json!({"error":"conversation not found"}),
        ),
        (
            ConversationAgentActivityRefusal::Forbidden,
            StatusCode::FORBIDDEN,
            json!({"error":"forbidden"}),
        ),
        (
            ConversationAgentActivityRefusal::TargetNotParticipant,
            StatusCode::NOT_FOUND,
            json!({"error":"agent is not a member of this conversation"}),
        ),
    ] {
        let port = FakeRead {
            admission: Err(refusal),
            ..FakeRead::successful()
        };
        let (status, body) = call(
            port.clone(),
            "/api/conversations/c/agents/a/activity?limit=bad",
            true,
        )
        .await;
        assert_eq!(status, expected_status);
        assert_eq!(body, expected_body);
        assert_eq!(port.admit_calls.load(Ordering::SeqCst), 1);
        assert_eq!(port.read_calls.load(Ordering::SeqCst), 0);
    }

    let changed = FakeRead {
        read_refusal: Some(ConversationAgentActivityRefusal::Forbidden),
        ..FakeRead::successful()
    };
    let (status, body) = call(
        changed.clone(),
        "/api/conversations/c/agents/a/activity?limit=2",
        true,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body, json!({"error":"forbidden"}));
    assert_eq!(changed.admit_calls.load(Ordering::SeqCst), 1);
    assert_eq!(changed.read_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn query_numbers_preserve_defaults_caps_invalids_and_large_cursor_classification() {
    for (query, expected_page, expected_body) in [
        ("", Some((100, None)), None),
        (
            "?limit=1e20&before_id=1e20",
            Some((200, Some(ActivityBefore::BeyondPersistedRange))),
            None,
        ),
        (
            "?limit=1.5",
            None,
            Some(json!({"error":"invalid limit","code":"invalid_limit"})),
        ),
        (
            "?before_id=0",
            None,
            Some(json!({"error":"invalid before_id","code":"invalid_before_id"})),
        ),
    ] {
        let port = FakeRead::successful();
        let (status, body) = call(
            port.clone(),
            &format!("/api/conversations/c/agents/a/activity{query}"),
            true,
        )
        .await;
        assert_eq!(port.admit_calls.load(Ordering::SeqCst), 1);
        let pages = port.pages.lock().unwrap();
        if let Some((limit, before)) = expected_page {
            assert_eq!(status, StatusCode::OK, "{query}");
            assert_eq!(pages.len(), 1, "{query}");
            assert_eq!((pages[0].limit(), pages[0].before()), (limit, before));
        } else {
            assert_eq!(status, StatusCode::BAD_REQUEST, "{query}");
            assert_eq!(body, expected_body.unwrap(), "{query}");
            assert!(pages.is_empty(), "{query}");
            assert_eq!(port.read_calls.load(Ordering::SeqCst), 0);
        }
    }
}

#[tokio::test]
async fn returned_coordinates_and_payload_bind_before_wire() {
    let positive = FakeRead::successful();
    let (status, body) = call(
        positive,
        "/api/conversations/c/agents/a/activity?limit=2&before_id=8",
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({
            "events":[{
                "id":7,
                "conversation_id":"c",
                "agent_id":"a",
                "kind":"text",
                "payload":{"text":"hello"},
                "created_at":9
            }],
            "next_before_id":null
        })
    );

    let foreign = FakeRead {
        projected_conversation: "other",
        ..FakeRead::successful()
    };
    let (status, body) = call(foreign, "/api/conversations/c/agents/a/activity", true).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body, json!({"error":"internal error"}));

    let foreign_viewer = FakeRead {
        projected_viewer: "other",
        ..FakeRead::successful()
    };
    let (status, body) = call(
        foreign_viewer,
        "/api/conversations/c/agents/a/activity",
        true,
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body, json!({"error":"internal error"}));

    let malformed = FakeRead {
        payload: "{",
        ..FakeRead::successful()
    };
    let (status, body) = call(malformed, "/api/conversations/c/agents/a/activity", true).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body, json!({"error":"internal error"}));
}

#[tokio::test]
async fn production_router_reads_the_current_schema_snapshot() {
    const SCHEMA: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../rust/test-fixtures/migration-baseline/schema-current.sql"
    ));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("conversation-agent-activity-wire.sqlite");
    let options = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true)
        .foreign_keys(true);
    let mut db = SqliteConnection::connect_with(&options).await.unwrap();
    db.execute(sqlx::raw_sql(SCHEMA)).await.unwrap();
    for statement in [
        "INSERT INTO hosts(id,name,kind,created_at) VALUES('h','host','local',1)",
        "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES('viewer','activity-wire-viewer','Viewer',1,1)",
        "INSERT INTO agents(id,name,runtime,host_id,created_by_user_id,created_at,updated_at,display_name) VALUES('a','activity-wire-agent','codex','h','viewer',1,1,'Agent')",
        "INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,endpoint_a_kind,endpoint_a_id,endpoint_b_kind,endpoint_b_id,title,title_key,visibility,created_at,updated_at) VALUES('c',NULL,'direct_pair','fixed','agent','a','human','viewer','',NULL,'private',1,1)",
        "INSERT INTO conversation_events(id,conversation_id,agent_id,kind,payload,created_at) VALUES(1,'c','a','text','{\"text\":\"one\"}',11),(2,'c','a','turn_end','[]',12)",
    ] {
        db.execute(statement).await.unwrap();
    }
    db.close().await.unwrap();

    let store =
        silverhand_backend_sqlite::conversation_messaging::messaging::MessagingStore::open(&path)
            .await
            .unwrap();
    let mut request = Request::builder()
        .uri("/api/conversations/c/agents/a/activity?limit=2")
        .body(Body::empty())
        .unwrap();
    request
        .extensions_mut()
        .insert(UserId::parse("viewer").unwrap());
    let response = router(store.conversation_agent_activity())
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body["events"][0]["payload"], json!({"text":"one"}));
    assert_eq!(body["events"][1]["payload"], Value::Null);
    assert_eq!(body["next_before_id"], Value::Null);
}
