use std::{
    convert::Infallible,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use axum::{
    body::{Body, Bytes, to_bytes},
    http::{
        Request, StatusCode,
        header::{CONTENT_LENGTH, CONTENT_TYPE},
    },
};
use futures_util::stream;
use serde::Deserialize;
use serde_json::{Value, json};
use silverhand_backend_core::{
    conversation_messaging::messaging::{
        MessageId, MessageSeq, SendClock, SendEntropy,
        collaboration::{
            channel_directory::{ChannelVisibility, DirectoryRoster},
            channel_lifecycle::{
                BoundChannelArchive, BoundChannelRename, BoundChannelVisibility,
                ChannelArchiveAftermath, ChannelArchivePort, ChannelArchiveRefusal,
                ChannelLifecycleRow, ChannelMutationAdmissionPort, ChannelMutationAdmissionRefusal,
                ChannelMutationEffectFailure, ChannelRenameAftermath, ChannelRenamePort,
                ChannelRenameRefusal, ChannelVisibilityAftermath, ChannelVisibilityPort,
                ChannelVisibilityRefusal, CommittedChannelArchive, CommittedChannelRename,
                CommittedChannelVisibility, GovernedChannelTarget,
            },
        },
        conversation_title::CanonicalConversationTitle,
        environment::EnvironmentUnavailable,
    },
    foundation::identity::{AgentId, ConversationId, UserId, WorkspaceId},
};
use silverhand_backend_sqlite::conversation_messaging::messaging::MessagingStore;
use sqlx::{Connection, Executor, Row, SqliteConnection, sqlite::SqliteConnectOptions};
use tower::ServiceExt;
use tracing_subscriber::fmt::MakeWriter;

use super::mutation_router;

fn user(raw: &str) -> UserId {
    UserId::parse(raw).unwrap()
}

fn workspace(raw: &str) -> WorkspaceId {
    WorkspaceId::parse(raw).unwrap()
}

fn title(raw: &str) -> CanonicalConversationTitle {
    CanonicalConversationTitle::parse(raw).unwrap()
}

fn row(
    conversation: &ConversationId,
    title: &CanonicalConversationTitle,
    visibility: ChannelVisibility,
    archived: bool,
) -> ChannelLifecycleRow {
    ChannelLifecycleRow::workspace_placed(
        conversation.clone(),
        workspace("w"),
        DirectoryRoster::Open,
        title.clone(),
        None,
        archived.then_some(9),
        0,
        1,
        2,
        visibility,
    )
    .unwrap()
}

#[derive(Clone, Copy)]
enum PortMode {
    Happy,
    AdmissionNotFound,
    AdmissionForbidden,
    ArchiveNotFound,
    VisibilityNotFound,
    RenameTitleTaken,
}

#[derive(Clone, Default)]
struct Calls {
    admission: Arc<AtomicUsize>,
    archive: Arc<AtomicUsize>,
    visibility: Arc<AtomicUsize>,
    rename: Arc<AtomicUsize>,
}

#[derive(Clone)]
struct Port {
    mode: PortMode,
    calls: Calls,
}

impl ChannelMutationAdmissionPort for Port {
    type Error = Infallible;

    async fn admit(
        &self,
        viewer: &UserId,
        conversation: &ConversationId,
    ) -> Result<Result<GovernedChannelTarget, ChannelMutationAdmissionRefusal>, Self::Error> {
        self.calls.admission.fetch_add(1, Ordering::SeqCst);
        Ok(match self.mode {
            PortMode::AdmissionNotFound => Err(ChannelMutationAdmissionRefusal::NotFound),
            PortMode::AdmissionForbidden => Err(ChannelMutationAdmissionRefusal::Forbidden),
            _ => Ok(GovernedChannelTarget::resolved(
                viewer.clone(),
                conversation.clone(),
                workspace("w"),
            )),
        })
    }
}

impl ChannelArchivePort for Port {
    type Error = Infallible;

    async fn set_archived(
        &self,
        viewer: UserId,
        conversation: ConversationId,
        archived: bool,
    ) -> Result<Result<CommittedChannelArchive, ChannelArchiveRefusal>, Self::Error> {
        self.calls.archive.fetch_add(1, Ordering::SeqCst);
        if matches!(self.mode, PortMode::ArchiveNotFound) {
            return Ok(Err(ChannelArchiveRefusal::NotFound));
        }
        Ok(Ok(CommittedChannelArchive::committed(
            viewer,
            workspace("w"),
            row(
                &conversation,
                &title("General"),
                ChannelVisibility::Private,
                archived,
            ),
            archived,
        )
        .unwrap()))
    }
}

impl ChannelVisibilityPort for Port {
    type Error = Infallible;

    async fn set_visibility(
        &self,
        viewer: UserId,
        conversation: ConversationId,
        visibility: ChannelVisibility,
    ) -> Result<Result<CommittedChannelVisibility, ChannelVisibilityRefusal>, Self::Error> {
        self.calls.visibility.fetch_add(1, Ordering::SeqCst);
        if matches!(self.mode, PortMode::VisibilityNotFound) {
            return Ok(Err(ChannelVisibilityRefusal::NotFound));
        }
        Ok(Ok(CommittedChannelVisibility::changed(
            viewer,
            workspace("w"),
            row(&conversation, &title("General"), visibility, false),
            visibility,
        )
        .unwrap()))
    }
}

impl ChannelRenamePort for Port {
    type Error = Infallible;

    async fn rename(
        &self,
        viewer: UserId,
        conversation: ConversationId,
        requested: CanonicalConversationTitle,
    ) -> Result<Result<CommittedChannelRename, ChannelRenameRefusal>, Self::Error> {
        self.calls.rename.fetch_add(1, Ordering::SeqCst);
        if matches!(self.mode, PortMode::RenameTitleTaken) {
            return Ok(Err(ChannelRenameRefusal::TitleTaken));
        }
        Ok(Ok(CommittedChannelRename::unchanged(
            viewer,
            workspace("w"),
            row(&conversation, &requested, ChannelVisibility::Private, false),
            requested,
        )
        .unwrap()))
    }
}

#[derive(Clone, Default)]
struct Effects {
    events: Arc<Mutex<Vec<String>>>,
    fail: Option<&'static str>,
}

#[derive(Debug, thiserror::Error)]
#[error("effect-driver:{0}")]
struct EffectDriver(&'static str);

impl Effects {
    fn record(&self, event: impl Into<String>) -> Result<(), ChannelMutationEffectFailure> {
        let event = event.into();
        self.events.lock().unwrap().push(event.clone());
        if self.fail == Some(event.as_str()) {
            Err(ChannelMutationEffectFailure::from_source(EffectDriver(
                self.fail.unwrap(),
            )))
        } else {
            Ok(())
        }
    }
}

impl ChannelArchiveAftermath for Effects {
    fn start_runtime_cleanup(&self, _: &BoundChannelArchive) {
        self.events.lock().unwrap().push("cleanup".into());
    }

    async fn workspace_channels_changed(
        &self,
        _: &BoundChannelArchive,
    ) -> Result<(), ChannelMutationEffectFailure> {
        self.record("archive-workspace")
    }

    async fn unread_updated(
        &self,
        _: &BoundChannelArchive,
    ) -> Result<(), ChannelMutationEffectFailure> {
        self.record("archive-unread")
    }
}

impl ChannelVisibilityAftermath for Effects {
    async fn workspace_channels_changed(
        &self,
        _: &BoundChannelVisibility,
    ) -> Result<(), ChannelMutationEffectFailure> {
        self.record("visibility-workspace")
    }
}

impl ChannelRenameAftermath for Effects {
    fn title_address_moved(&self, _: &BoundChannelRename) {
        self.events.lock().unwrap().push("rename-address".into());
    }

    async fn deliver(
        &self,
        _: &BoundChannelRename,
        recipient: &AgentId,
    ) -> Result<(), ChannelMutationEffectFailure> {
        self.record(format!("deliver:{}", recipient.as_str()))
    }

    async fn publish_message(
        &self,
        _: &BoundChannelRename,
    ) -> Result<(), ChannelMutationEffectFailure> {
        self.record("rename-message")
    }

    async fn workspace_channels_changed(
        &self,
        _: &BoundChannelRename,
    ) -> Result<(), ChannelMutationEffectFailure> {
        self.record("rename-workspace")
    }
}

fn fixture_app(mode: PortMode, effects: Effects) -> (axum::Router, Calls) {
    let calls = Calls::default();
    (
        mutation_router(
            Port {
                mode,
                calls: calls.clone(),
            },
            effects,
        ),
        calls,
    )
}

async fn call(
    app: axum::Router,
    method_path: &str,
    body: Body,
    principal: bool,
    declared: Option<usize>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method("PATCH")
        .uri(method_path)
        // The body now crosses the shared `ValidatedJson` extractor, whose native axum Json
        // requires the JSON content type (a missing one is 415); every real client sends it.
        .header(CONTENT_TYPE, "application/json");
    if let Some(length) = declared {
        builder = builder.header(CONTENT_LENGTH, length);
    }
    let mut request = builder.body(body).unwrap();
    if principal {
        request.extensions_mut().insert(user("local"));
    }
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    // A native structural rejection (415/400/422) carries a plain-text body pinned on
    // status only; JSON bodies (success / minimal semantic 422) parse.
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn authentication_and_admission_precede_body_polling_and_product_writes() {
    for (principal, mode, status) in [
        (false, PortMode::Happy, StatusCode::INTERNAL_SERVER_ERROR),
        (true, PortMode::AdmissionNotFound, StatusCode::NOT_FOUND),
        (true, PortMode::AdmissionForbidden, StatusCode::FORBIDDEN),
    ] {
        let polled = Arc::new(AtomicBool::new(false));
        let flag = polled.clone();
        let body = Body::from_stream(stream::once(async move {
            flag.store(true, Ordering::SeqCst);
            Ok::<Bytes, Infallible>(Bytes::from_static(b"{\"archived\":true}"))
        }));
        let (app, calls) = fixture_app(mode, Effects::default());
        let (actual, _) = call(app, "/api/channels/c/archive", body, principal, None).await;
        assert_eq!(actual, status);
        assert!(!polled.load(Ordering::SeqCst));
        assert_eq!(calls.archive.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn archive_requires_a_boolean_field_and_object_body() {
    let cases = [
        // missing `archived` → native data-shape 422; writer untouched
        (Body::from("{}"), None, StatusCode::UNPROCESSABLE_ENTITY, 0),
        // wrong-typed `archived` → native data-shape 422
        (
            Body::from(r#"{"archived":"yes"}"#),
            None,
            StatusCode::UNPROCESSABLE_ENTITY,
            0,
        ),
        // unknown key stripped (schema not `.strict()`) → 200, one write
        (
            Body::from(r#"{"archived":true,"future":1}"#),
            None,
            StatusCode::OK,
            1,
        ),
    ];
    for (body, declared, expected, writes) in cases {
        let (app, calls) = fixture_app(PortMode::Happy, Effects::default());
        let (status, body) = call(app, "/api/channels/c/archive", body, true, declared).await;
        assert_eq!(status, expected);
        if expected == StatusCode::OK {
            assert_eq!(body["channel"]["archived_at"], 9);
        }
        assert_eq!(calls.archive.load(Ordering::SeqCst), writes);
    }

    // A value-shaped array is refused by the shared object-only floor (native structural),
    // never bound positionally into the archive DTO; the writer stays untouched.
    let (app, calls) = fixture_app(PortMode::Happy, Effects::default());
    let (status, _) = call(
        app,
        "/api/channels/c/archive",
        Body::from("[true]"),
        true,
        None,
    )
    .await;
    // A value-shaped array is refused by the shared object-only floor as a native data-shape
    // 422 before positional binding; the writer stays untouched.
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(calls.archive.load(Ordering::SeqCst), 0);
}

#[derive(Deserialize)]
struct AdmissionCell {
    name: String,
    request: Value,
    schema_accepted: bool,
    operation: Option<String>,
    canonical_title: Option<String>,
}

#[tokio::test]
async fn production_update_router_consumes_the_shared_schema_accepted_cells() {
    // Denominator: the shared `channel-lifecycle-update-admission.json` has 9 cells; this
    // Rust replay consumes the **3/9** `schema_accepted=true` XOR-success cells (rename /
    // visibility). The other **6/9** are Zod admission cells whose TS signer emits legacy
    // 400{issues}; the Rust terminal admission is the shared native structural axis plus the
    // minimal-422 exactly-one refinement, so they are NOT dual-signed here and are covered by
    // the focused `schema_rejected_update_cells_take_the_native_axis` local test. The TS
    // retired TS parity test signed its own full 9/9 legacy behavior.
    let cells: Vec<AdmissionCell> = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../rust/test-fixtures/fixtures/channel-lifecycle-update-admission.json"
    )))
    .unwrap();
    let mut consumed = 0;
    for cell in &cells {
        if !cell.schema_accepted {
            continue;
        }
        consumed += 1;
        let (app, calls) = fixture_app(PortMode::Happy, Effects::default());
        let (status, body) = call(
            app,
            "/api/channels/c",
            Body::from(serde_json::to_vec(&cell.request).unwrap()),
            true,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{}", cell.name);
        assert_eq!(body["channel"]["id"], "c", "{}", cell.name);
        match cell.operation.as_deref() {
            Some("rename") => {
                assert_eq!(calls.rename.load(Ordering::SeqCst), 1, "{}", cell.name);
                assert_eq!(calls.visibility.load(Ordering::SeqCst), 0, "{}", cell.name);
                assert_eq!(
                    body["channel"]["title"],
                    cell.canonical_title
                        .as_deref()
                        .expect("rename fixture title"),
                    "{}",
                    cell.name
                );
            }
            Some("visibility") => {
                assert_eq!(calls.visibility.load(Ordering::SeqCst), 1, "{}", cell.name);
                assert_eq!(calls.rename.load(Ordering::SeqCst), 0, "{}", cell.name);
                assert_eq!(
                    body["channel"]["visibility"], cell.request["visibility"],
                    "{}",
                    cell.name
                );
            }
            other => panic!("unexpected operation {other:?}"),
        }
    }
    assert_eq!(cells.len(), 9);
    assert_eq!(consumed, 3, "shared cross-language denominator is 3/9");
}

#[tokio::test]
async fn schema_rejected_update_cells_take_the_native_axis_with_writer_zero_reach() {
    // The 6 `schema_accepted=false` cells: `neither_field`/`both_fields` are the exactly-one
    // `.refine()` → minimal 422; `non_object`/`title_wrong_type`/`visibility_*` are native
    // structural faults (400/422, incl. the object-only floor). None reaches a write port.
    let cells: Vec<AdmissionCell> = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../rust/test-fixtures/fixtures/channel-lifecycle-update-admission.json"
    )))
    .unwrap();
    let mut checked = 0;
    for cell in &cells {
        if cell.schema_accepted {
            continue;
        }
        checked += 1;
        let (app, calls) = fixture_app(PortMode::Happy, Effects::default());
        let (status, _) = call(
            app,
            "/api/channels/c",
            Body::from(serde_json::to_vec(&cell.request).unwrap()),
            true,
            None,
        )
        .await;
        // Exact native status by input class: `neither_field`/`both_fields` are the
        // exactly-one refinement → minimal 422; `non_object`/`title_wrong_type`/
        // `visibility_bad_enum` are data-shape → 422; `visibility_wrong_type` (a number in
        // the shared `RequiredPatchField<enum>` wrapper) surfaces as the native 400.
        let expected = if cell.name == "visibility_wrong_type" {
            StatusCode::BAD_REQUEST
        } else {
            StatusCode::UNPROCESSABLE_ENTITY
        };
        assert_eq!(status, expected, "{}", cell.name);
        assert_eq!(calls.rename.load(Ordering::SeqCst), 0, "{}", cell.name);
        assert_eq!(calls.visibility.load(Ordering::SeqCst), 0, "{}", cell.name);
    }
    assert_eq!(checked, 6, "schema-rejected local denominator is 6/9");
}

#[tokio::test]
async fn explicit_null_title_or_visibility_is_a_native_fault_never_the_other_mutation() {
    // P1: `title`/`visibility` are `.optional()` but NOT `.nullable()`, so an explicit
    // `null` must be a native decode fault — it must NOT collapse to "missing" and let the
    // OTHER field drive a rename/visibility change. `RequiredPatchField` rejects the present
    // `null` as a native data-shape fault → exactly 422; neither write port is reached.
    for raw in [
        r#"{"title":null,"visibility":"public"}"#, // null title must not become a visibility change
        r#"{"title":"X","visibility":null}"#,      // null visibility must not become a rename
        r#"{"title":null,"visibility":null}"#,     // both null
        r#"{"title":null}"#,
        r#"{"visibility":null}"#,
    ] {
        let (app, calls) = fixture_app(PortMode::Happy, Effects::default());
        let (status, _) = call(app, "/api/channels/c", Body::from(raw), true, None).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{raw}");
        assert_eq!(calls.rename.load(Ordering::SeqCst), 0, "{raw}");
        assert_eq!(calls.visibility.load(Ordering::SeqCst), 0, "{raw}");
    }
}

#[tokio::test]
async fn writer_refusals_keep_their_operation_specific_wire() {
    for (mode, path, request, status, body) in [
        (
            PortMode::ArchiveNotFound,
            "/api/channels/c/archive",
            json!({"archived":true}),
            StatusCode::NOT_FOUND,
            json!({"error":"channel not found"}),
        ),
        (
            PortMode::VisibilityNotFound,
            "/api/channels/c",
            json!({"visibility":"public"}),
            StatusCode::NOT_FOUND,
            json!({"error":"channel not found"}),
        ),
        (
            PortMode::RenameTitleTaken,
            "/api/channels/c",
            json!({"title":"Roadmap"}),
            StatusCode::CONFLICT,
            json!({
                "error":"a channel titled Roadmap already exists in this workspace",
                "code":"conversation_title_taken"
            }),
        ),
    ] {
        let (app, _) = fixture_app(mode, Effects::default());
        let (actual_status, actual_body) = call(
            app,
            path,
            Body::from(serde_json::to_vec(&request).unwrap()),
            true,
            None,
        )
        .await;
        assert_eq!((actual_status, actual_body), (status, body));
    }
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
        bytes.fill(7);
        Ok(())
    }
}

async fn real_store() -> (
    tempfile::TempDir,
    std::path::PathBuf,
    MessagingStore<FixedEnvironment>,
) {
    const SCHEMA: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../rust/test-fixtures/migration-baseline/schema-current.sql"
    ));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("channel-lifecycle-mutation-route.sqlite");
    let options = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true)
        .foreign_keys(true);
    let mut db = SqliteConnection::connect_with(&options).await.unwrap();
    db.execute(sqlx::raw_sql(SCHEMA)).await.unwrap();
    for sql in [
        "INSERT INTO hosts(id,name,kind,created_at) VALUES('h','Host','local',1)",
        "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES('local','lifecycle-route','Local',1,1)",
        "INSERT INTO agents(id,name,runtime,host_id,created_by_user_id,created_at,updated_at) VALUES('a','route-agent','codex','h','local',1,1)",
        "INSERT INTO workspaces(id,name,name_key,created_at,updated_at) VALUES('w','Workspace','lifecycle-route',1,1)",
        "INSERT INTO workspace_users(workspace_id,user_id,joined_at,role) VALUES('w','local',1,'admin')",
        "INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,visibility,created_at,updated_at) VALUES('c','w','workspace','open','General','general','private',1,1)",
        "INSERT INTO conversation_agents(conversation_id,agent_id,joined_at) VALUES('c','a',1)",
    ] {
        db.execute(sql).await.unwrap();
    }
    db.close().await.unwrap();
    let store = MessagingStore::open_with(&path, FixedEnvironment)
        .await
        .unwrap();
    (dir, path, store)
}

#[tokio::test]
async fn production_router_and_sqlite_adapter_commit_all_three_operation_faces() {
    let (_dir, path, store) = real_store().await;
    let effects = Effects::default();
    let app = mutation_router(store.channel_lifecycle_mutation(), effects.clone());
    for (path, request, expected) in [
        (
            "/api/channels/c/archive",
            json!({"archived":true}),
            json!({"archived_at":77}),
        ),
        (
            "/api/channels/c",
            json!({"visibility":"public"}),
            json!({"visibility":"public"}),
        ),
        (
            "/api/channels/c",
            json!({"title":"Road Map"}),
            json!({"title":"Road Map"}),
        ),
    ] {
        let (status, body) = call(
            app.clone(),
            path,
            Body::from(serde_json::to_vec(&request).unwrap()),
            true,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        for (key, value) in expected.as_object().unwrap() {
            assert_eq!(&body["channel"][key], value);
        }
    }
    assert_eq!(
        *effects.events.lock().unwrap(),
        [
            "cleanup",
            "archive-workspace",
            "archive-unread",
            "visibility-workspace",
            "rename-address",
            "deliver:a",
            "rename-message",
            "rename-workspace",
        ]
    );
    let mut db = SqliteConnection::connect(path.to_str().unwrap())
        .await
        .unwrap();
    let channel = sqlx::query(
        "SELECT title,title_key,visibility,archived_at,latest_message_seq FROM conversations WHERE id='c'",
    )
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(channel.get::<String, _>("title"), "Road Map");
    assert_eq!(channel.get::<String, _>("title_key"), "road map");
    assert_eq!(channel.get::<String, _>("visibility"), "public");
    assert_eq!(channel.get::<i64, _>("archived_at"), 77);
    assert_eq!(channel.get::<i64, _>("latest_message_seq"), 1);
    let message = sqlx::query("SELECT id,seq,metadata FROM messages WHERE conversation_id='c'")
        .fetch_one(&mut db)
        .await
        .unwrap();
    MessageId::parse(message.get::<String, _>("id")).unwrap();
    assert_eq!(
        MessageSeq::try_from(message.get::<i64, _>("seq"))
            .unwrap()
            .get(),
        1
    );
    assert_eq!(
        serde_json::from_str::<Value>(&message.get::<String, _>("metadata")).unwrap(),
        json!({
            "subtype":"rename", "old_name":"General", "new_name":"Road Map",
            "scope":"conversation"
        })
    );
}

#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

struct LogWriter(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
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
async fn post_commit_failure_is_logged_once_with_source_and_never_changes_200() {
    let effects = Effects {
        events: Arc::new(Mutex::new(vec![])),
        fail: Some("archive-workspace"),
    };
    let (app, _) = fixture_app(PortMode::Happy, effects.clone());
    let logs = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::ERROR)
        .with_writer(logs.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let (status, body) = call(
        app,
        "/api/channels/c/archive",
        Body::from(r#"{"archived":true}"#),
        true,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.to_string().contains("effect-driver"));
    assert_eq!(
        *effects.events.lock().unwrap(),
        ["cleanup", "archive-workspace", "archive-unread"]
    );
    let events = logs.events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["fields"]["operation"], "archive");
    let chain = events[0]["fields"]["error_chain"].as_str().unwrap();
    assert!(chain.contains("channel lifecycle post-commit effect failed"));
    assert!(chain.contains("effect-driver:archive-workspace"));
}
