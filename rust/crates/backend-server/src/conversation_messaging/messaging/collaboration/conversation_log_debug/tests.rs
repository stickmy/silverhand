use std::{
    convert::Infallible,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering},
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
use serde_json::{Value, json};
use silverhand_backend_core::{
    foundation::identity::{ConversationId, UserId},
    foundation::persistence::PersistenceFailure,
};
use silverhand_backend_sqlite::conversation_messaging::messaging::{
    MessagingStore,
    collaboration::conversation_participation::{
        ConversationParticipationRefusal, SqliteConversationParticipationRead,
    },
};
use sqlx::{Connection, Executor, SqliteConnection, sqlite::SqliteConnectOptions};
use tower::ServiceExt;

use super::*;

#[derive(Clone, Copy)]
enum AdmissionMode {
    Granted,
    NotFound,
    Forbidden,
    ForeignViewer,
    ForeignConversation,
}

#[derive(Clone)]
struct Admission {
    mode: AdmissionMode,
    calls: Arc<AtomicUsize>,
}

impl ConversationLogDebugAdmissionPort for Admission {
    type Error = Infallible;

    async fn admit(
        &self,
        viewer: &UserId,
        conversation: &ConversationId,
    ) -> Result<
        Result<ConversationLogDebugAdmissionFact, ConversationLogDebugAdmissionRefusal>,
        Self::Error,
    > {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(match self.mode {
            AdmissionMode::Granted => Ok(ConversationLogDebugAdmissionFact::selected(
                viewer.clone(),
                conversation.clone(),
            )),
            AdmissionMode::NotFound => Err(ConversationLogDebugAdmissionRefusal::NotFound),
            AdmissionMode::Forbidden => Err(ConversationLogDebugAdmissionRefusal::Forbidden),
            AdmissionMode::ForeignViewer => Ok(ConversationLogDebugAdmissionFact::selected(
                UserId::parse("foreign").unwrap(),
                conversation.clone(),
            )),
            AdmissionMode::ForeignConversation => Ok(ConversationLogDebugAdmissionFact::selected(
                viewer.clone(),
                ConversationId::parse("foreign").unwrap(),
            )),
        })
    }
}

#[derive(Clone, Copy)]
enum ControlMode {
    Matching,
    ForeignConversation,
    WrongEnableExpiration,
    WrongDisableState,
}

#[derive(Clone)]
struct Control {
    mode: ControlMode,
    status: Arc<Mutex<ConversationLogDebugStatus>>,
    reads: Arc<AtomicUsize>,
    enables: Arc<AtomicUsize>,
    disables: Arc<AtomicUsize>,
}

impl Default for Control {
    fn default() -> Self {
        Self {
            mode: ControlMode::Matching,
            status: Arc::new(Mutex::new(ConversationLogDebugStatus::disabled())),
            reads: Arc::new(AtomicUsize::new(0)),
            enables: Arc::new(AtomicUsize::new(0)),
            disables: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl Control {
    fn with_mode(mode: ControlMode) -> Self {
        Self {
            mode,
            ..Self::default()
        }
    }

    fn reported_conversation(&self, requested: &ConversationId) -> ConversationId {
        match self.mode {
            ControlMode::ForeignConversation => ConversationId::parse("foreign").unwrap(),
            _ => requested.clone(),
        }
    }
}

impl ConversationLogDebugControl for Control {
    fn status(&self, conversation: &ConversationId, _now: i64) -> ConversationLogDebugControlFact {
        self.reads.fetch_add(1, Ordering::SeqCst);
        ConversationLogDebugControlFact::observed(
            self.reported_conversation(conversation),
            *self.status.lock().unwrap(),
        )
    }

    fn enable(
        &self,
        conversation: &ConversationId,
        expires_at: i64,
    ) -> ConversationLogDebugControlFact {
        self.enables.fetch_add(1, Ordering::SeqCst);
        *self.status.lock().unwrap() = ConversationLogDebugStatus::enabled(expires_at).unwrap();
        let reported_expiration = match self.mode {
            ControlMode::WrongEnableExpiration => expires_at + 1,
            _ => expires_at,
        };
        ConversationLogDebugControlFact::observed(
            self.reported_conversation(conversation),
            ConversationLogDebugStatus::enabled(reported_expiration).unwrap(),
        )
    }

    fn disable(&self, conversation: &ConversationId) -> ConversationLogDebugControlFact {
        self.disables.fetch_add(1, Ordering::SeqCst);
        *self.status.lock().unwrap() = ConversationLogDebugStatus::disabled();
        let reported = match self.mode {
            ControlMode::WrongDisableState => ConversationLogDebugStatus::enabled(1).unwrap(),
            _ => ConversationLogDebugStatus::disabled(),
        };
        ConversationLogDebugControlFact::observed(
            self.reported_conversation(conversation),
            reported,
        )
    }
}

#[derive(Clone)]
struct Clock {
    now: i64,
    calls: Arc<AtomicUsize>,
}

impl ConversationLogDebugClock for Clock {
    type Error = Infallible;

    fn now_unix_ms(&self) -> Result<i64, Self::Error> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.now)
    }
}

#[derive(Clone)]
struct MovingClock(Arc<AtomicI64>);

impl ConversationLogDebugClock for MovingClock {
    type Error = Infallible;

    fn now_unix_ms(&self) -> Result<i64, Self::Error> {
        Ok(self.0.load(Ordering::SeqCst))
    }
}

struct Fixture {
    app: Router,
    admission: Arc<AtomicUsize>,
    control: Control,
    clock: Arc<AtomicUsize>,
}

fn fixture(mode: AdmissionMode) -> Fixture {
    fixture_with_control(mode, Control::default())
}

fn fixture_with_control(mode: AdmissionMode, control: Control) -> Fixture {
    let admission = Arc::new(AtomicUsize::new(0));
    let clock = Arc::new(AtomicUsize::new(0));
    let app = router(
        Admission {
            mode,
            calls: admission.clone(),
        },
        control.clone(),
        Clock {
            now: 1_000,
            calls: clock.clone(),
        },
    );
    Fixture {
        app,
        admission,
        control,
        clock,
    }
}

async fn call(
    app: Router,
    method: &str,
    body: Body,
    principal: bool,
    declared: Option<usize>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri("/api/conversations/c/log-debug")
        // The POST body now crosses the shared `ValidatedJson` extractor, whose native
        // axum `Json` requires the JSON content type (a missing one is 415 — the ratified
        // wire contract). Every real product client sends it; the harness does too. The
        // content-type-absent case is asserted separately.
        .header(CONTENT_TYPE, "application/json");
    if let Some(length) = declared {
        builder = builder.header(CONTENT_LENGTH, length);
    }
    let mut request = builder.body(body).unwrap();
    if principal {
        request
            .extensions_mut()
            .insert(UserId::parse("viewer").unwrap());
    }
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    // A native axum rejection (415/400/422 structural) carries a plain-text body, not
    // JSON; the caller pins those on status only. A JSON body (success or the minimal
    // semantic refusal) parses; anything else surfaces as `Value::Null`.
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

#[tokio::test]
async fn get_binds_both_source_coordinates_and_reads_the_expiry_clock_once() {
    for (mode, expected) in [
        (AdmissionMode::Granted, StatusCode::OK),
        (
            AdmissionMode::ForeignViewer,
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
        (
            AdmissionMode::ForeignConversation,
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    ] {
        let fixture = fixture(mode);
        let (status, body) = call(fixture.app, "GET", Body::empty(), true, None).await;
        assert_eq!(status, expected);
        if expected == StatusCode::OK {
            assert_eq!(
                body,
                json!({"conversationId":"c","enabled":false,"expiresAt":null})
            );
            assert_eq!(fixture.control.reads.load(Ordering::SeqCst), 1);
        } else {
            assert_eq!(fixture.control.reads.load(Ordering::SeqCst), 0);
        }
        assert_eq!(
            fixture.clock.load(Ordering::SeqCst),
            usize::from(expected == StatusCode::OK)
        );
    }
}

#[tokio::test]
async fn every_local_control_fact_binds_its_conversation_and_requested_state_before_wire() {
    let read = fixture_with_control(
        AdmissionMode::Granted,
        Control::with_mode(ControlMode::ForeignConversation),
    );
    let (status, _) = call(read.app, "GET", Body::empty(), true, None).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(read.control.reads.load(Ordering::SeqCst), 1);
    assert_eq!(read.clock.load(Ordering::SeqCst), 1);

    for mode in [
        ControlMode::ForeignConversation,
        ControlMode::WrongEnableExpiration,
    ] {
        let enabled = fixture_with_control(AdmissionMode::Granted, Control::with_mode(mode));
        let (status, _) = call(
            enabled.app,
            "POST",
            Body::from(r#"{"enabled":true}"#),
            true,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(enabled.clock.load(Ordering::SeqCst), 1);
        assert_eq!(enabled.control.enables.load(Ordering::SeqCst), 1);
        assert_eq!(enabled.control.disables.load(Ordering::SeqCst), 0);
    }

    for mode in [
        ControlMode::ForeignConversation,
        ControlMode::WrongDisableState,
    ] {
        let disabled = fixture_with_control(AdmissionMode::Granted, Control::with_mode(mode));
        let (status, _) = call(
            disabled.app,
            "POST",
            Body::from(r#"{"enabled":false}"#),
            true,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(disabled.clock.load(Ordering::SeqCst), 0);
        assert_eq!(disabled.control.enables.load(Ordering::SeqCst), 0);
        assert_eq!(disabled.control.disables.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn authentication_and_strict_participation_precede_body_polling() {
    for (principal, mode, expected, admission_calls) in [
        (
            false,
            AdmissionMode::Granted,
            StatusCode::INTERNAL_SERVER_ERROR,
            0,
        ),
        (true, AdmissionMode::NotFound, StatusCode::NOT_FOUND, 1),
        (true, AdmissionMode::Forbidden, StatusCode::FORBIDDEN, 1),
        (
            true,
            AdmissionMode::ForeignViewer,
            StatusCode::INTERNAL_SERVER_ERROR,
            1,
        ),
        (
            true,
            AdmissionMode::ForeignConversation,
            StatusCode::INTERNAL_SERVER_ERROR,
            1,
        ),
    ] {
        let polled = Arc::new(AtomicBool::new(false));
        let flag = polled.clone();
        let body = Body::from_stream(stream::once(async move {
            flag.store(true, Ordering::SeqCst);
            Ok::<Bytes, Infallible>(Bytes::from_static(b"{\"enabled\":true}"))
        }));
        let fixture = fixture(mode);
        let (status, _) = call(fixture.app, "POST", body, principal, None).await;
        assert_eq!(status, expected);
        assert_eq!(fixture.admission.load(Ordering::SeqCst), admission_calls);
        assert!(!polled.load(Ordering::SeqCst));
        assert_eq!(fixture.control.enables.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.control.disables.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn enable_reads_one_local_clock_and_disable_reads_none() {
    let enabled = fixture(AdmissionMode::Granted);
    let (status, body) = call(
        enabled.app,
        "POST",
        Body::from(r#"{"enabled":true}"#),
        true,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({
            "conversationId":"c", "enabled":true, "expiresAt":601_000,
            "daemonSynced":true
        })
    );
    assert_eq!(enabled.clock.load(Ordering::SeqCst), 1);
    assert_eq!(enabled.control.enables.load(Ordering::SeqCst), 1);
    assert_eq!(enabled.control.disables.load(Ordering::SeqCst), 0);

    let disabled = fixture(AdmissionMode::Granted);
    let (status, body) = call(
        disabled.app,
        "POST",
        Body::from(r#"{"enabled":false}"#),
        true,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({
            "conversationId":"c", "enabled":false, "expiresAt":null,
            "daemonSynced":true
        })
    );
    assert_eq!(disabled.clock.load(Ordering::SeqCst), 0);
    assert_eq!(disabled.control.enables.load(Ordering::SeqCst), 0);
    assert_eq!(disabled.control.disables.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn production_control_expires_at_the_same_boundary_the_get_route_reads() {
    let calls = Arc::new(AtomicUsize::new(0));
    let now = Arc::new(AtomicI64::new(1_000));
    let control = ProductionConversationLogDebugControl::new();
    let app = router(
        Admission {
            mode: AdmissionMode::Granted,
            calls,
        },
        control.clone(),
        MovingClock(now.clone()),
    );
    let (status, body) = call(
        app.clone(),
        "POST",
        Body::from(r#"{"enabled":true}"#),
        true,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["expiresAt"], 601_000);
    now.store(600_999, Ordering::SeqCst);
    let (status, body) = call(app.clone(), "GET", Body::empty(), true, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({"conversationId":"c","enabled":true,"expiresAt":601_000})
    );
    now.store(601_000, Ordering::SeqCst);
    let (status, body) = call(app, "GET", Body::empty(), true, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({"conversationId":"c","enabled":false,"expiresAt":null})
    );
    assert!(!control.is_enabled_at(&ConversationId::parse("c").unwrap(), 601_000));
}

#[tokio::test]
async fn strict_boolean_shape_and_content_type_precede_local_control() {
    // The closed body crosses the shared `ValidatedJson` extractor AFTER admit. Structural
    // faults keep their NATIVE axum status — 400 for a JSON syntax error, 422 for a
    // data-shape fault (missing/wrong-typed key, unknown key under `.strict()`, and a
    // value-shaped ARRAY the object-only floor refuses before it can bind positionally) —
    // and there is no stateless rule, so nothing collapses to a re-rendered issue schema.
    // NONE reaches the local control (authority/effect zero-reach). Native body text is
    // deliberately NOT pinned.
    let structural_422 = [
        "{}",
        r#"{"enabled":"yes"}"#,
        r#"{"enabled":true,"future":1}"#,
        "[true]",
        "true",
        "null",
    ];
    for raw in structural_422 {
        let fixture = fixture(AdmissionMode::Granted);
        let (status, _) = call(fixture.app, "POST", Body::from(raw), true, None).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{raw}");
        assert_eq!(fixture.clock.load(Ordering::SeqCst), 0, "{raw}");
        assert_eq!(fixture.control.enables.load(Ordering::SeqCst), 0, "{raw}");
        assert_eq!(fixture.control.disables.load(Ordering::SeqCst), 0, "{raw}");
    }
    // A JSON syntax error keeps the native 400 (distinct from the 422 data-shape axis).
    {
        let fixture = fixture(AdmissionMode::Granted);
        let (status, _) = call(fixture.app, "POST", Body::from("not-json"), true, None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(fixture.control.enables.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.control.disables.load(Ordering::SeqCst), 0);
    }
    // A missing content-type is the native 415, decided by the extractor before any body
    // shape is read — still after admit, still zero-reach to the control.
    {
        let fixture = fixture(AdmissionMode::Granted);
        let request = Request::builder()
            .method("POST")
            .uri("/api/conversations/c/log-debug")
            .extension(UserId::parse("viewer").unwrap())
            .body(Body::from(r#"{"enabled":true}"#))
            .unwrap();
        let response = fixture.app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(fixture.control.enables.load(Ordering::SeqCst), 0);
    }

    // Body is polled only AFTER admit: a streamed body whose first poll would set the
    // flag must not be read when admission has already succeeded until the extractor runs,
    // and never before it. Here admission is granted, so the body IS consumed by the
    // shared extractor — the ordering contract (admit-before-body) is proven by the
    // refusal cases below, where a refused admission leaves the body unpolled.
    let polled = Arc::new(AtomicBool::new(false));
    let flag = polled.clone();
    let body = Body::from_stream(stream::once(async move {
        flag.store(true, Ordering::SeqCst);
        Ok::<Bytes, Infallible>(Bytes::from_static(b"{}"))
    }));
    let fixture = fixture(AdmissionMode::Granted);
    let (status, _) = call(fixture.app, "POST", body, true, None).await;
    // `{}` is a data-shape fault (missing `enabled`) → native 422 through the shared
    // extractor; the local control is never touched.
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(polled.load(Ordering::SeqCst));
    assert_eq!(fixture.control.enables.load(Ordering::SeqCst), 0);
}

#[derive(Clone)]
struct SqliteAdmission(SqliteConversationParticipationRead);

impl ConversationLogDebugAdmissionPort for SqliteAdmission {
    type Error = PersistenceFailure;

    async fn admit(
        &self,
        viewer: &UserId,
        conversation: &ConversationId,
    ) -> Result<
        Result<ConversationLogDebugAdmissionFact, ConversationLogDebugAdmissionRefusal>,
        Self::Error,
    > {
        Ok(match self.0.read(viewer, conversation).await? {
            Ok(source) => Ok(ConversationLogDebugAdmissionFact::selected(
                source.viewer().clone(),
                source.conversation().clone(),
            )),
            Err(ConversationParticipationRefusal::NotFound) => {
                Err(ConversationLogDebugAdmissionRefusal::NotFound)
            }
            Err(ConversationParticipationRefusal::Forbidden) => {
                Err(ConversationLogDebugAdmissionRefusal::Forbidden)
            }
        })
    }
}

async fn sqlite_fixture(setup: &str) -> (tempfile::TempDir, SqliteAdmission) {
    const SCHEMA: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../rust/test-fixtures/migration-baseline/schema-current.sql"
    ));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("conversation-log-debug.sqlite");
    let options = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true)
        .foreign_keys(true);
    let mut db = SqliteConnection::connect_with(&options).await.unwrap();
    db.execute(sqlx::raw_sql(SCHEMA)).await.unwrap();
    db.execute(
        "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES \
         ('viewer','log-debug-viewer','Viewer',1,1),('peer','log-debug-peer','Peer',1,1)",
    )
    .await
    .unwrap();
    if setup.starts_with("workspace") {
        db.execute(
            "INSERT INTO workspaces(id,name,name_key,created_at,updated_at) VALUES \
             ('w','Workspace','log-debug-workspace',1,1)",
        )
        .await
        .unwrap();
        db.execute(
            "INSERT INTO workspace_users(workspace_id,user_id,joined_at) VALUES('w','viewer',1)",
        )
        .await
        .unwrap();
        db.execute(seed_channel!(
            id = "c",
            workspace_id = "w",
            title = "General",
            title_key = "general-log-debug",
            visibility = "public",
        ))
        .await
        .unwrap();
        if setup == "workspace-member" {
            db.execute(
                "INSERT INTO conversation_users(conversation_id,user_id,joined_at) VALUES('c','viewer',1)",
            )
            .await
            .unwrap();
        }
    } else if setup == "direct-pair-endpoint" {
        db.execute(seed_direct_pair!(
            id = "c",
            endpoint_a_kind = "human",
            endpoint_a_id = "peer",
            endpoint_b_kind = "human",
            endpoint_b_id = "viewer",
        ))
        .await
        .unwrap();
    }
    db.close().await.unwrap();
    let store = MessagingStore::open(&path).await.unwrap();
    (dir, SqliteAdmission(store.conversation_participation()))
}

#[tokio::test]
async fn production_sqlite_source_preserves_strict_workspace_and_direct_pair_participation() {
    for (setup, expected) in [
        ("workspace-member", StatusCode::OK),
        ("workspace-public-nonmember", StatusCode::FORBIDDEN),
        ("direct-pair-endpoint", StatusCode::OK),
        ("missing", StatusCode::NOT_FOUND),
    ] {
        let (_dir, admission) = sqlite_fixture(setup).await;
        let control = Control::default();
        let app = router(
            admission,
            control,
            Clock {
                now: 1_000,
                calls: Arc::new(AtomicUsize::new(0)),
            },
        );
        let (status, body) = call(app, "GET", Body::empty(), true, None).await;
        assert_eq!(status, expected, "{setup}");
        if expected == StatusCode::OK {
            assert_eq!(body["conversationId"], "c", "{setup}");
        }
    }
}
