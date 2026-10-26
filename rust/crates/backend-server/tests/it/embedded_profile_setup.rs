//! Real bootstrap, SQLite migration and authenticated HTTP setup boundary.
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use silverhand_backend_core::{
    embedded_bootstrap::{BootstrapChoice, EmbeddedBootstrapUnitOfWork},
    foundation::identity::UserId,
    identity_access::{
        auth::{
            HumanSessionAuthenticator, SessionAudit, SessionClock, SessionIssue, SessionTokenHash,
            SessionTtl,
        },
        user_identity::{UserChangedAnnounceFailure, UserChangedSink},
    },
};
use silverhand_backend_server::{
    foundation::deployment_profile::DeploymentProfile, identity_access::embedded_profile,
};
use silverhand_backend_sqlite::{
    embedded_bootstrap::SqliteEmbeddedBootstrap,
    foundation::database_startup::prepare_database,
    identity_access::{auth::SqliteSessionStore, user_identity::SqliteUserStore},
};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const PATH: &str = "/api/me/embedded-profile-setup";
#[derive(Clone)]
struct Clock;
impl SessionClock for Clock {
    fn now_unix_millis(&self) -> i64 {
        1000
    }
}
#[derive(Clone, Default)]
struct Changes(Arc<Mutex<Vec<String>>>);
impl UserChangedSink for Changes {
    async fn user_changed(&self, user: &UserId) -> Result<(), UserChangedAnnounceFailure> {
        self.0.lock().unwrap().push(user.as_str().to_owned());
        Ok(())
    }
}
struct Fixture {
    _dir: tempfile::TempDir,
    path: std::path::PathBuf,
    user: UserId,
    changes: Changes,
}
impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.sqlite");
        let preparation = prepare_database(&path, true).await.unwrap();
        assert_eq!((preparation.after_step, preparation.after_epoch), (101, 71));
        let bootstrap = SqliteEmbeddedBootstrap::open(&path).await.unwrap();
        let BootstrapChoice::Created { user } = bootstrap.choose_or_create(100).await.unwrap()
        else {
            panic!("fresh identity")
        };
        let sessions = SqliteSessionStore::open(&path).await.unwrap();
        sessions
            .issue(
                &user.id,
                SessionTokenHash::of_plaintext("local-token"),
                &SessionAudit::default(),
                1000,
            )
            .await
            .unwrap();
        Self {
            _dir: dir,
            path,
            user: user.id,
            changes: Changes::default(),
        }
    }
    async fn db(&self) -> SqliteConnection {
        SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&self.path)
                .foreign_keys(true),
        )
        .await
        .unwrap()
    }
    async fn router(&self, profile: DeploymentProfile) -> Router {
        let store = SqliteUserStore::open(&self.path).await.unwrap();
        let sessions = SqliteSessionStore::open(&self.path).await.unwrap();
        let authority = HumanSessionAuthenticator::new(sessions, Clock, SessionTtl::default());
        embedded_profile::router(store, self.changes.clone(), Clock, profile, authority)
    }
}
async fn request(
    router: &Router,
    method: &str,
    body: Option<Value>,
    token: Option<&str>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(PATH);
    if let Some(token) = token {
        req = req.header("authorization", format!("Bearer {token}"));
    }
    if body.is_some() {
        req = req.header("content-type", "application/json");
    }
    let response = router
        .clone()
        .oneshot(
            req.body(
                body.map(|v| Body::from(v.to_string()))
                    .unwrap_or_else(Body::empty),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}
fn profile(name: &str) -> Value {
    json!({"name":name,"display_name":" Alice "})
}

#[tokio::test]
async fn completion_response_loss_restart_and_late_requests_preserve_identity() {
    let fixture = Fixture::new().await;
    let app = fixture.router(DeploymentProfile::Embedded).await;
    let initial = request(&app, "GET", None, Some("local-token")).await;
    assert_eq!(initial.1["required"], true);
    assert_eq!(initial.1["user"]["name"], "local");
    let bootstrap = SqliteEmbeddedBootstrap::open(&fixture.path).await.unwrap();
    assert!(matches!(
        bootstrap.choose_or_create(200).await.unwrap(),
        BootstrapChoice::Selected { .. }
    ));
    assert_eq!(
        request(
            &fixture.router(DeploymentProfile::Embedded).await,
            "GET",
            None,
            Some("local-token")
        )
        .await
        .1["required"],
        true
    );

    // Discard the successful response as if the transport were lost after commit.
    let committed = request(&app, "PUT", Some(profile("Alice")), Some("local-token")).await;
    assert_eq!(committed.0, StatusCode::OK);
    assert_eq!(committed.1["user"]["name"], "Alice");
    assert_eq!(committed.1["user"]["display_name"], "Alice");
    assert_eq!(
        *fixture.changes.0.lock().unwrap(),
        vec![fixture.user.as_str()]
    );
    drop(app);
    let recovered = fixture.router(DeploymentProfile::Embedded).await;
    let state = request(&recovered, "GET", None, Some("local-token")).await;
    assert_eq!(state.1, committed.1);
    for name in ["Alice", "LateRename"] {
        let retry = request(&recovered, "PUT", Some(profile(name)), Some("local-token")).await;
        assert_eq!(retry.0, StatusCode::CONFLICT);
        assert_eq!(retry.1["code"], "profile_setup_completed");
    }
    assert_eq!(fixture.changes.0.lock().unwrap().len(), 1);
    let bootstrap = SqliteEmbeddedBootstrap::open(&fixture.path).await.unwrap();
    assert!(matches!(
        bootstrap.choose_or_create(2000).await.unwrap(),
        BootstrapChoice::Selected { .. }
    ));
    assert_eq!(
        request(&recovered, "GET", None, Some("local-token"))
            .await
            .1["required"],
        false
    );
}

#[tokio::test]
async fn concurrent_completion_has_one_winner_and_one_immutable_conflict() {
    let fixture = Fixture::new().await;
    let app = fixture.router(DeploymentProfile::Embedded).await;
    let (a, b) = tokio::join!(
        request(&app, "PUT", Some(profile("First")), Some("local-token")),
        request(&app, "PUT", Some(profile("Second")), Some("local-token"))
    );
    assert!(
        (a.0 == StatusCode::OK && b.0 == StatusCode::CONFLICT)
            || (b.0 == StatusCode::OK && a.0 == StatusCode::CONFLICT)
    );
    assert_eq!(fixture.changes.0.lock().unwrap().len(), 1);
    let expected = if a.0 == StatusCode::OK { a.1 } else { b.1 };
    assert_eq!(
        request(&app, "GET", None, Some("local-token")).await.1,
        expected
    );
}

#[tokio::test]
async fn session_and_deployment_authority_reject_invalid_setup_requests() {
    let fixture = Fixture::new().await;
    let app = fixture.router(DeploymentProfile::Embedded).await;
    assert_eq!(
        request(&app, "GET", None, None).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(&app, "PUT", Some(profile("Alice")), Some("invalid"))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let remote = fixture.router(DeploymentProfile::RemoteOnly).await;
    for method in ["GET", "PUT"] {
        assert_eq!(
            request(&remote, method, Some(profile("Alice")), Some("local-token"))
                .await
                .0,
            StatusCode::FORBIDDEN
        );
    }
    let mut injected = profile("Alice");
    injected["user_id"] = json!("someone-else");
    assert_eq!(
        request(&app, "PUT", Some(injected), Some("local-token"))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        request(&app, "PUT", Some(profile("123")), Some("local-token"))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );

    assert_eq!(
        request(&app, "GET", None, Some("local-token")).await.1["required"],
        true
    );
}

#[tokio::test]
async fn other_viewers_and_occupied_handles_cannot_complete_the_pending_identity() {
    let fixture = Fixture::new().await;
    let app = fixture.router(DeploymentProfile::Embedded).await;
    let mut db = fixture.db().await;
    sqlx::query("INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES ('other','Taken','Other',0,0)")
        .execute(&mut db)
        .await
        .unwrap();
    assert_eq!(
        request(&app, "PUT", Some(profile("Taken")), Some("local-token"))
            .await
            .1["code"],
        "name_taken"
    );
    let sessions = SqliteSessionStore::open(&fixture.path).await.unwrap();
    sessions
        .issue(
            &UserId::parse("other").unwrap(),
            SessionTokenHash::of_plaintext("other-token"),
            &SessionAudit::default(),
            1000,
        )
        .await
        .unwrap();
    assert_eq!(
        request(&app, "PUT", Some(profile("Steal")), Some("other-token"))
            .await
            .1["code"],
        "profile_setup_completed"
    );
    assert_eq!(
        request(&app, "GET", None, Some("local-token")).await.1["required"],
        true
    );
    sqlx::raw_sql("INSERT INTO hosts(id,name,kind,created_at) VALUES('h','H','local',1);
        INSERT INTO agents(id,name,display_name,runtime,host_id,created_by_user_id,created_at,updated_at)
        VALUES('agent','AgentTaken','Agent','claude','h','other',1,1);")
        .execute(&mut db).await.unwrap();
    assert_eq!(
        request(
            &app,
            "PUT",
            Some(profile("AgentTaken")),
            Some("local-token")
        )
        .await
        .1["code"],
        "name_taken"
    );
}

#[tokio::test]
async fn failed_core_write_rolls_back_fields_and_completion_together() {
    let fixture = Fixture::new().await;
    let app = fixture.router(DeploymentProfile::Embedded).await;
    let mut db = fixture.db().await;
    sqlx::query("CREATE TRIGGER fail_setup BEFORE UPDATE OF embedded_profile_pending ON users BEGIN SELECT RAISE(ABORT,'test failure'); END")
        .execute(&mut db)
        .await
        .unwrap();
    assert_eq!(
        request(&app, "PUT", Some(profile("Alice")), Some("local-token"))
            .await
            .0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    let state = request(&app, "GET", None, Some("local-token")).await.1;
    assert_eq!(state["required"], true);
    assert_eq!(state["user"]["name"], "local");
    assert!(fixture.changes.0.lock().unwrap().is_empty());
    sqlx::query("DROP TRIGGER fail_setup")
        .execute(&mut db)
        .await
        .unwrap();
    assert_eq!(
        request(&app, "PUT", Some(profile("Alice")), Some("local-token"))
            .await
            .0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn step98_upgrade_keeps_existing_identity_complete_and_preserves_other_fields() {
    // Build an isolated step98 database from the current plan by removing only
    // the additive step99-101 changes and their ledger entries; no historical fixture changes.
    let fixture = Fixture::new().await;
    let mut db = fixture.db().await;
    rollback_step100(&mut db).await;
    sqlx::query("ALTER TABLE users DROP COLUMN embedded_profile_pending")
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::query("DELETE FROM _migrations WHERE version = 99")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    assert!(prepare_database(&fixture.path, false).await.is_err());
    let upgrade = prepare_database(&fixture.path, true).await.unwrap();
    assert_eq!(
        (
            upgrade.before_step,
            upgrade.after_step,
            upgrade.before_epoch,
            upgrade.after_epoch
        ),
        (98, 101, 69, 71)
    );
    assert_eq!(upgrade.applied_steps, vec![99, 100, 101]);
    assert!(
        prepare_database(&fixture.path, false)
            .await
            .unwrap()
            .applied_steps
            .is_empty()
    );
    let app = fixture.router(DeploymentProfile::Embedded).await;
    let state = request(&app, "GET", None, Some("local-token")).await.1;
    assert_eq!(state["required"], false);
    assert_eq!(
        state["user"]["name"], "local",
        "legacy default name does not imply pending"
    );
    assert_eq!(state["user"]["created_at"], 100);
    assert_eq!(
        request(&app, "PUT", Some(profile("Rename")), Some("local-token"))
            .await
            .0,
        StatusCode::CONFLICT
    );
}

use silverhand_backend_core::identity_access::avatar::{
    AvatarBlobPointer, AvatarBlobRead, AvatarBlobReadOutcome, AvatarBlobStore, AvatarContentType,
    AvatarStorageFailure,
};
#[derive(Clone)]
struct FailingAvatarStorage;
impl AvatarBlobRead for FailingAvatarStorage {
    async fn read(
        &self,
        _: &AvatarBlobPointer,
    ) -> Result<AvatarBlobReadOutcome, AvatarStorageFailure> {
        Ok(AvatarBlobReadOutcome::Missing)
    }
}
impl AvatarBlobStore for FailingAvatarStorage {
    async fn put(
        &self,
        _: &UserId,
        _: AvatarContentType,
        _: Vec<u8>,
    ) -> Result<AvatarBlobPointer, AvatarStorageFailure> {
        Err(AvatarStorageFailure::from_source(std::io::Error::other(
            "isolated avatar failure",
        )))
    }
    async fn delete(&self, _: &AvatarBlobPointer) -> Result<(), AvatarStorageFailure> {
        Ok(())
    }
}

#[tokio::test]
async fn avatar_storage_failure_after_completion_does_not_reopen_setup() {
    let fixture = Fixture::new().await;
    let app = fixture.router(DeploymentProfile::Embedded).await;
    assert_eq!(
        request(&app, "PUT", Some(profile("Alice")), Some("local-token"))
            .await
            .0,
        StatusCode::OK
    );
    let store = SqliteUserStore::open(&fixture.path).await.unwrap();
    let sessions = SqliteSessionStore::open(&fixture.path).await.unwrap();
    let authority = HumanSessionAuthenticator::new(sessions, Clock, SessionTtl::default());
    let avatar = silverhand_backend_server::identity_access::users::router(
        silverhand_backend_server::identity_access::users::UsersDeps {
            directory: store.clone(),
            avatar_refs: store.clone(),
            reads: store.clone(),
            avatar_writes: store,
            blobs: FailingAvatarStorage,
            user_changed: fixture.changes.clone(),
            clock: Clock,
        },
        authority,
    );
    let body = "--test\r\nContent-Disposition: form-data; name=\"file\"; filename=\"avatar.png\"\r\nContent-Type: image/png\r\n\r\nbytes\r\n--test--\r\n";
    let failed = avatar
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/me/avatar")
                .header("authorization", "Bearer local-token")
                .header("content-type", "multipart/form-data; boundary=test")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(failed.status(), StatusCode::BAD_REQUEST);
    let failure: Value =
        serde_json::from_slice(&to_bytes(failed.into_body(), 65536).await.unwrap()).unwrap();
    assert_eq!(failure["code"], "avatar_storage_failed");
    let state = request(&app, "GET", None, Some("local-token")).await.1;
    assert_eq!(state["required"], false);
    assert_eq!(state["user"]["name"], "Alice");
    assert_eq!(state["user"]["avatar_url"], Value::Null);
    assert_eq!(fixture.changes.0.lock().unwrap().len(), 1);
}

/// Return a current database to step 99 by undoing only the additive steps 100-101.
async fn rollback_step100(db: &mut SqliteConnection) {
    sqlx::raw_sql(
        "DROP TABLE password_recovery_state; DELETE FROM _migrations WHERE version = 101;
         DROP TRIGGER users_avatar_design_follows_image; DROP TRIGGER users_avatar_design_release;
         DROP TRIGGER users_avatar_design_delete; DROP TRIGGER agents_avatar_design_follows_image;
         DROP TRIGGER agents_avatar_design_release; DROP TRIGGER agents_avatar_design_delete;
         DROP INDEX idx_users_avatar_design; DROP INDEX idx_agents_avatar_design;
         ALTER TABLE users DROP COLUMN avatar_design_id; ALTER TABLE agents DROP COLUMN avatar_design_id;
         DROP TABLE avatar_designs; DELETE FROM _migrations WHERE version = 100;",
    )
    .execute(&mut *db)
    .await
    .unwrap();
}

#[tokio::test]
async fn step99_upgrade_adds_avatar_designs_and_keeps_existing_avatars() {
    let fixture = Fixture::new().await;
    let mut db = fixture.db().await;
    rollback_step100(&mut db).await;
    sqlx::query(
        "UPDATE users SET avatar_storage_key = 'k', avatar_content_type = 'image/png' WHERE id = ?",
    )
    .bind(fixture.user.as_str())
    .execute(&mut db)
    .await
    .unwrap();
    db.close().await.unwrap();

    let upgrade = prepare_database(&fixture.path, true).await.unwrap();
    assert_eq!(
        (
            upgrade.before_step,
            upgrade.after_step,
            upgrade.before_epoch,
            upgrade.after_epoch
        ),
        (99, 101, 70, 71)
    );
    assert_eq!(upgrade.applied_steps, vec![100, 101]);
    let mut db = fixture.db().await;
    let row: (Option<String>, Option<String>) =
        sqlx::query_as("SELECT avatar_storage_key, avatar_design_id FROM users WHERE id = ?")
            .bind(fixture.user.as_str())
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(row, (Some("k".to_owned()), None));
    db.close().await.unwrap();
}
