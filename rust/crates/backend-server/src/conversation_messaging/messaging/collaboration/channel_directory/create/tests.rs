use std::{
    convert::Infallible,
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use axum::{
    body::{Body, Bytes, to_bytes},
    extract::Extension,
    http::{Request, StatusCode, header::CONTENT_LENGTH},
};
use futures_util::stream;
use serde_json::{Value, json};
use silverhand_backend_core::{
    conversation_messaging::messaging::{
        SendClock, SendEntropy,
        collaboration::channel_directory::{
            ChannelCreationPublicationFailure, ChannelDirectoryRow, ChannelVisibility,
            CommittedHumanChannelCreation, DirectoryRoster, HumanChannelCreateAftermath,
            HumanChannelCreatePort, HumanChannelCreateRefusal, HumanChannelCreateRequest,
        },
        conversation_title::CanonicalConversationTitle,
        environment::EnvironmentUnavailable,
    },
    foundation::identity::{ConversationId, UserId, WorkspaceId},
};
use silverhand_backend_sqlite::conversation_messaging::messaging::MessagingStore;
use sqlx::{Connection, Executor};
use tower::ServiceExt;
use tracing_subscriber::fmt::MakeWriter;

use super::create_router;

fn user(raw: &str) -> UserId {
    UserId::parse(raw).unwrap()
}

fn workspace(raw: &str) -> WorkspaceId {
    WorkspaceId::parse(raw).unwrap()
}

fn committed() -> CommittedHumanChannelCreation {
    let viewer = user("local");
    let workspace = workspace("workspace-1");
    let title = CanonicalConversationTitle::parse("General").unwrap();
    let row = ChannelDirectoryRow::projected(
        viewer.clone(),
        workspace.clone(),
        ConversationId::parse("created").unwrap(),
        DirectoryRoster::Open,
        "General".into(),
        None,
        None,
        0,
        77,
        77,
        ChannelVisibility::Private,
        true,
    )
    .unwrap();
    CommittedHumanChannelCreation::regular(viewer, workspace, title, row).unwrap()
}

#[derive(Clone)]
enum Outcome {
    Commit(Box<CommittedHumanChannelCreation>),
    Refuse(HumanChannelCreateRefusal),
}

#[derive(Clone)]
struct Port {
    outcome: Outcome,
    requests: Arc<Mutex<Vec<HumanChannelCreateRequest>>>,
}

impl HumanChannelCreatePort for Port {
    type Error = Infallible;

    async fn create_human_channel(
        &self,
        _: UserId,
        _: WorkspaceId,
        request: HumanChannelCreateRequest,
    ) -> Result<Result<CommittedHumanChannelCreation, HumanChannelCreateRefusal>, Self::Error> {
        self.requests.lock().unwrap().push(request);
        Ok(match &self.outcome {
            Outcome::Commit(value) => Ok(value.as_ref().clone()),
            Outcome::Refuse(value) => Err(value.clone()),
        })
    }
}

#[derive(Clone, Default)]
struct Effects {
    events: Arc<Mutex<Vec<&'static str>>>,
    fail: bool,
}

#[derive(Debug, thiserror::Error)]
#[error("publish-driver-detail")]
struct PublishDriver;

impl HumanChannelCreateAftermath for Effects {
    async fn workspace_channels_changed(
        &self,
        _: &silverhand_backend_core::conversation_messaging::messaging::collaboration::channel_directory::BoundHumanChannelCreation,
    ) -> Result<(), ChannelCreationPublicationFailure> {
        self.events.lock().unwrap().push("publish");
        if self.fail {
            Err(ChannelCreationPublicationFailure::from_source(
                PublishDriver,
            ))
        } else {
            Ok(())
        }
    }

    fn title_address_taken(
        &self,
        _: &silverhand_backend_core::conversation_messaging::messaging::collaboration::channel_directory::BoundHumanChannelCreation,
    ) {
        self.events.lock().unwrap().push("audit");
    }
}

async fn call(
    app: axum::Router,
    path: &str,
    body: &str,
    authenticated: bool,
) -> (StatusCode, Value) {
    call_body(app, path, Body::from(body.to_owned()), authenticated, None).await
}

async fn call_body(
    app: axum::Router,
    path: &str,
    body: Body,
    authenticated: bool,
    content_length: Option<usize>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json");
    if let Some(length) = content_length {
        builder = builder.header(CONTENT_LENGTH, length);
    }
    let mut request = builder.body(body).unwrap();
    if authenticated {
        request.extensions_mut().insert(user("local"));
    }
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    // A native axum structural rejection (415/400/422) carries a plain-text body pinned on
    // status only; a JSON body (success or minimal semantic 422) parses.
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

fn fixture_app(
    outcome: Outcome,
    effects: Effects,
) -> (axum::Router, Arc<Mutex<Vec<HumanChannelCreateRequest>>>) {
    let requests = Arc::new(Mutex::new(vec![]));
    (
        create_router(
            Port {
                outcome,
                requests: requests.clone(),
            },
            effects,
        )
        .layer(Extension(user("local"))),
        requests,
    )
}

#[tokio::test]
async fn production_admission_takes_the_native_structural_axis_and_drops_raw_before_port() {
    // The body crosses the shared `ValidatedJson` extractor. Exact native status by input
    // class: a JSON syntax error is 400; every data-shape fault (missing/wrong-typed field,
    // unknown enum, the object-only floor refusing a value-shaped array/non-object) and the
    // stateless garde title rule are 422. No re-rendered issue schema; the port is never
    // reached (raw dropped). Native body text is NOT pinned.
    let cases = [
        ("{}", StatusCode::UNPROCESSABLE_ENTITY), // missing title
        (r#"{"title":""}"#, StatusCode::UNPROCESSABLE_ENTITY), // empty title → garde
        (
            r#"{"title":"X","member_agent_ids":[1]}"#,
            StatusCode::UNPROCESSABLE_ENTITY,
        ), // wrong element type
        (
            r#"{"title":"X","visibility":"loud"}"#,
            StatusCode::UNPROCESSABLE_ENTITY,
        ), // unknown enum
        (
            r#"{"title":"X","visibility":null}"#,
            StatusCode::UNPROCESSABLE_ENTITY,
        ), // explicit null (optional-not-nullable)
        (
            r#"{"title":"X","member_user_ids":null}"#,
            StatusCode::UNPROCESSABLE_ENTITY,
        ), // explicit null array
        ("[1]", StatusCode::UNPROCESSABLE_ENTITY), // array → object-only
        ("null", StatusCode::UNPROCESSABLE_ENTITY), // non-object
        ("not-json", StatusCode::BAD_REQUEST),    // JSON syntax → 400
    ];
    for (raw, expected) in cases {
        let (app, requests) = fixture_app(
            Outcome::Refuse(HumanChannelCreateRefusal::WorkspaceNotFound),
            Effects::default(),
        );
        let (status, _) = call(app, "/api/workspaces/workspace-1/channels", raw, false).await;
        assert_eq!(status, expected, "{raw}");
        assert!(requests.lock().unwrap().is_empty(), "{raw}");
    }
    // A missing content-type is the native 415, before any body shape is read; port zero-reach.
    let (app, requests) = fixture_app(
        Outcome::Refuse(HumanChannelCreateRefusal::WorkspaceNotFound),
        Effects::default(),
    );
    let request = Request::builder()
        .method("POST")
        .uri("/api/workspaces/workspace-1/channels")
        .body(Body::from(r#"{"title":"X"}"#))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert!(requests.lock().unwrap().is_empty());

    let (app, requests) = fixture_app(
        Outcome::Refuse(HumanChannelCreateRefusal::WorkspaceNotFound),
        Effects::default(),
    );
    let (status, _) = call(
        app,
        "/api/workspaces/workspace-1/channels",
        r#"{"title":"  General  ","member_agent_ids":["a","a"],"visibility":"public","ignored":"raw"}"#,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let request = requests.lock().unwrap().pop().unwrap();
    assert_eq!(request.title().title(), "General");
    assert_eq!(request.member_agent_ids(), ["a"]);
    assert_eq!(request.visibility(), ChannelVisibility::Public);
}

#[tokio::test]
async fn missing_agent_home_owner_refusal_preserves_its_wire_contract() {
    let (app, _) = fixture_app(
        Outcome::Refuse(HumanChannelCreateRefusal::AgentHomeOwnerNotFound),
        Effects::default(),
    );
    let (status, body) = call(
        app,
        "/api/workspaces/workspace-1/channels",
        r#"{"title":"General"}"#,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, json!({"error":"agent_home owner not found"}));
}

#[tokio::test]
async fn authentication_precedes_body_and_product_work() {
    let requests = Arc::new(Mutex::new(vec![]));
    let app = create_router(
        Port {
            outcome: Outcome::Commit(Box::new(committed())),
            requests: requests.clone(),
        },
        Effects::default(),
    );
    let polled = Arc::new(AtomicBool::new(false));
    let stream_flag = polled.clone();
    let body = Body::from_stream(stream::once(async move {
        stream_flag.store(true, Ordering::SeqCst);
        Ok::<Bytes, Infallible>(Bytes::from_static(b"not-json"))
    }));
    let request = Request::builder()
        .method("POST")
        .uri("/api/workspaces/workspace-1/channels")
        .body(body)
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body, json!({"error":"internal error"}));
    assert!(!polled.load(Ordering::SeqCst));
    assert!(requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn ordinary_body_read_failure_takes_the_native_structural_axis() {
    let body = Body::from_stream(stream::once(async {
        Err::<Bytes, _>(std::io::Error::other("body driver failed"))
    }));
    let (app, requests) = fixture_app(
        Outcome::Refuse(HumanChannelCreateRefusal::WorkspaceNotFound),
        Effects::default(),
    );
    let (status, _) = call_body(
        app,
        "/api/workspaces/workspace-1/channels",
        body,
        false,
        None,
    )
    .await;
    // A transport body-read failure is the native axum `400` body-read rejection (not a
    // re-rendered empty-object issue schema); the port is never reached.
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(requests.lock().unwrap().is_empty());
}

#[derive(Clone)]
struct FixedEnvironment;

impl SendClock for FixedEnvironment {
    fn now_unix_ms(&self) -> Result<i64, EnvironmentUnavailable> {
        Ok(77)
    }
}

impl SendEntropy for FixedEnvironment {
    fn fill(&self, bytes: &mut [u8]) -> Result<(), EnvironmentUnavailable> {
        bytes.fill(1);
        Ok(())
    }
}

fn fixture() -> Vec<Value> {
    serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../rust/test-fixtures/fixtures/channel-directory-create-wire.json"
    )))
    .unwrap()
}

fn fixture_request(entry: &Value) -> Value {
    match entry.get("request_case").and_then(Value::as_str) {
        None => entry["request"].clone(),
        Some("title_129") => json!({"title":"x".repeat(129)}),
        Some("malformed_json") => json!({}),
        Some(other) => panic!("unknown create request case {other}"),
    }
}

fn fixture_raw(entry: &Value) -> String {
    match entry.get("request_case").and_then(Value::as_str) {
        Some("malformed_json") => "{".into(),
        _ => serde_json::to_string(&fixture_request(entry)).unwrap(),
    }
}

fn fixture_body(entry: &Value, fixture: &[Value]) -> Value {
    let Some(name) = entry.get("body_case").and_then(Value::as_str) else {
        return entry["body"].clone();
    };
    fixture_body(
        fixture
            .iter()
            .find(|candidate| candidate["name"] == name)
            .unwrap(),
        fixture,
    )
}

async fn seeded_store() -> (tempfile::TempDir, MessagingStore<FixedEnvironment>) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory
        .path()
        .join("channel-directory-create-wire.sqlite");
    let mut db = crate::current_schema::create_current_schema(&path).await;
    db.execute(sqlx::raw_sql(
        "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES \
           ('local','wire-local','Local',1,1),('user-member','wire-user-member','Member',1,1), \
           ('user-outsider','wire-user-outsider','Outsider',1,1); \
         INSERT INTO hosts(id,name,kind,created_at) VALUES \
           ('wire-create-host','Wire Create Host','local',1); \
         INSERT INTO agents(id,name,runtime,host_id,created_at,updated_at,created_by_user_id) VALUES \
           ('agent-member','agent-member','claude','wire-create-host',1,1,'local'), \
           ('agent-outsider','agent-outsider','claude','wire-create-host',1,1,'local'); \
         INSERT INTO workspaces(id,name,name_key,owner_agent_id,created_at,updated_at) VALUES \
           ('workspace-1','Workspace','channel-create-wire',NULL,1,1), \
           ('workspace-2','Forbidden','channel-create-forbidden',NULL,1,1), \
           ('workspace-home','agent-member',NULL,'agent-member',1,1); \
         INSERT INTO workspace_users(workspace_id,user_id,joined_at,role) VALUES \
           ('workspace-1','local',1,'admin'),('workspace-1','user-member',1,'member'), \
           ('workspace-home','local',1,'admin'); \
         INSERT INTO workspace_members(workspace_id,agent_id,joined_at) VALUES \
           ('workspace-1','agent-member',1); \
         INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key, \
           visibility,created_at,updated_at) VALUES \
           ('taken','workspace-1','workspace','open','Taken','taken','private',1,1);",
    ))
    .await
    .unwrap();
    db.close().await.unwrap();
    let store = MessagingStore::open_with(path, FixedEnvironment)
        .await
        .unwrap();
    (directory, store)
}

fn normalize_success(mut body: Value) -> Value {
    let Some(channel) = body.get_mut("channel").and_then(Value::as_object_mut) else {
        return body;
    };
    channel.insert("id".into(), json!("created"));
    body
}

#[tokio::test]
async fn production_sqlite_router_consumes_the_shared_schema_accepted_cells() {
    // Denominator: the shared `channel-directory-create-wire.json` has 24 cells; this Rust
    // replay consumes the **12/24** with `schema_accepted=true` — the genuine cross-language
    // business/domain/order cells (success, workspace/governance/membership/title-uniqueness
    // refusals, agent-home arms). The other **12/24** (`schema_accepted=false`) are Zod
    // admission cells whose TS signer emits legacy 400{issues}; the Rust terminal admission
    // is the shared native structural axis (400/422), so they are NOT dual-signed here and
    // are covered by the focused `schema_rejected_cells_take_the_native_structural_axis`
    // local test. The TS parity test continues to sign its own full 24/24 legacy behavior.
    let fixture = fixture();
    let mut consumed = 0;
    for entry in &fixture {
        if entry["schema_accepted"].as_bool() != Some(true) {
            continue;
        }
        consumed += 1;
        let (_directory, store) = seeded_store().await;
        let effects = Effects::default();
        let app = create_router(store.channel_directory_create(), effects.clone())
            .layer(Extension(user("local")));
        let raw = fixture_raw(entry);
        let (status, body) = call(app, entry["path"].as_str().unwrap(), &raw, false).await;
        assert_eq!(
            status.as_u16(),
            entry["status"].as_u64().unwrap() as u16,
            "{}",
            entry["name"]
        );
        assert_eq!(
            normalize_success(body),
            fixture_body(entry, &fixture),
            "{}",
            entry["name"]
        );
        let expected_events: &[&str] = if status == StatusCode::CREATED {
            &["publish", "audit"]
        } else {
            &[]
        };
        assert_eq!(
            &*effects.events.lock().unwrap(),
            expected_events,
            "{}",
            entry["name"]
        );
    }
    assert_eq!(fixture.len(), 24);
    assert_eq!(consumed, 12, "shared cross-language denominator is 12/24");
}

#[tokio::test]
async fn schema_rejected_cells_take_the_native_structural_axis_with_port_zero_reach() {
    // The 12 `schema_accepted=false` cells cross the shared `ValidatedJson` extractor with
    // an EXACT native status by input class (native body text not pinned; port zero-reach):
    //  * `malformed_json` — a JSON syntax fault → 400;
    //  * `visibility_wrong_type` — a wrong-typed value inside the shared `RequiredPatchField`
    //    optional-non-null wrapper, whose serde error axum surfaces as the native 400;
    //  * every other cell (missing/wrong-typed/null field, non-object root, garde title
    //    grammar) → 422.
    const NATIVE_400: [&str; 2] = [
        "malformed_json_falls_through_empty_object",
        "visibility_wrong_type",
    ];
    let fixture = fixture();
    let mut checked = 0;
    for entry in &fixture {
        if entry["schema_accepted"].as_bool() != Some(false) {
            continue;
        }
        checked += 1;
        let (app, requests) = fixture_app(
            Outcome::Refuse(HumanChannelCreateRefusal::WorkspaceNotFound),
            Effects::default(),
        );
        let raw = fixture_raw(entry);
        let name = entry["name"].as_str().unwrap();
        let expected = if NATIVE_400.contains(&name) {
            StatusCode::BAD_REQUEST
        } else {
            StatusCode::UNPROCESSABLE_ENTITY
        };
        let (status, _) = call(app, entry["path"].as_str().unwrap(), &raw, false).await;
        assert_eq!(status, expected, "{name}");
        assert!(requests.lock().unwrap().is_empty(), "{name}");
    }
    assert_eq!(checked, 12, "schema-rejected local denominator is 12/24");
}

#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

struct LogWriter(Arc<Mutex<Vec<u8>>>);

impl io::Write for LogWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for LogBuffer {
    type Writer = LogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        LogWriter(self.0.clone())
    }
}

impl LogBuffer {
    fn events(&self) -> Vec<Value> {
        String::from_utf8(self.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

#[tokio::test(flavor = "current_thread")]
async fn committed_publication_failure_is_logged_once_with_its_source_and_still_returns_201() {
    let effects = Effects {
        events: Arc::new(Mutex::new(vec![])),
        fail: true,
    };
    let (app, _) = fixture_app(Outcome::Commit(Box::new(committed())), effects.clone());
    let logs = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::ERROR)
        .with_writer(logs.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let (status, body) = call(
        app,
        "/api/workspaces/workspace-1/channels",
        r#"{"title":"General"}"#,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["channel"]["id"], "created");
    assert_eq!(body["channel"]["title"], "General");
    assert_eq!(body["channel"]["visibility"], "private");
    assert!(!body.to_string().contains("publish-driver-detail"));
    assert_eq!(*effects.events.lock().unwrap(), ["publish", "audit"]);
    let events = logs.events();
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0]["fields"]["message"],
        "committed channel creation publication failed"
    );
    let chain = events[0]["fields"]["error_chain"].as_str().unwrap();
    assert!(chain.contains("the committed channel creation could not be published"));
    assert!(chain.contains("publish-driver-detail"));
}
