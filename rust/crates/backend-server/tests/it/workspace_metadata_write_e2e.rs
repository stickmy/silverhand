//! Full-route behaviour for the workspace metadata-write surface (#task:2193):
//! `PATCH /api/workspaces/{id}` and `PATCH /api/workspaces/{id}/archive`, driven
//! through the REAL Axum router with the REAL SQLite governance-read and
//! write-unit-of-work adapters. The `metadata_changed` viewer hint, the per-channel
//! rename fan-out, and the clock are test doubles; the hint and fan-out are
//! recording/failing spies so the route's two committed-rename obligations are
//! observable.
//!
//! This is the Rust-self-guarded half of the slice's evidence: the refusal
//! ORDERING (404 → 403 → 400 → 409 → 200) and the committed `200` body. It is NOT
//! a cross-language gate — those rungs are structural, readable straight from the
//! route contract. Semantic rejection is the minimal 422 response; the old
//! Zod-issue oracle was retired and is not part of this suite.
//!
//! ## Both committed-rename effects are exercised here as REQUIREMENTS
//!
//! A committed rename owes TWO outward effects, and this route DRIVES both off the
//! one sealed `WorkspaceNameChange`: the `viewer_workspaces_changed` cache hint (a
//! pure id-only signal the domain fires) and the per-channel rename SYSTEM MESSAGE
//! (a messaging-context whole fan-out this route calls as a narrow requirement,
//! `WorkspaceRenameFanout`). Here both are recording spies rather than the
//! production hub/daemon adapters. This suite pins
//! that a rename drives BOTH exactly once for the renamed workspace, that a
//! description-only edit drives NEITHER, and — the discriminating grid — that with
//! BOTH the hint AND the fan-out failing, each is still attempted exactly once and
//! the committed `200` never downgrades.

use std::sync::{Arc, Mutex};

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use silverhand_backend_core::{
    foundation::identity::{UserId, WorkspaceId},
    identity_access::workspace_metadata_write::{
        WorkspaceMetadataAnnounceFailure, WorkspaceMetadataChangedSink, WorkspaceNameChange,
    },
};
use silverhand_backend_server::identity_access::workspace_metadata_write::{
    self, WorkspaceMetadataClock, WorkspaceRenameFanout, WorkspaceRenameFanoutFailure,
};
use silverhand_backend_sqlite::{
    identity_access::access::SqliteAccessStore,
    identity_access::workspace_metadata_write::SqliteWorkspaceMetadataWrite,
};
use sqlx::{Connection, Executor, SqliteConnection};
use tower::ServiceExt;

const CURRENT_SCHEMA: &str =
    include_str!("../../../../../rust/test-fixtures/migration-baseline/schema-current.sql");

// user-1 owns ws-1, user-2 is its admin, user-3 a member, user-9 a stranger. ws-2
// holds the fold key `taken` for the collision case; ws-home is an agent-home
// workspace that must read as NoSuchWorkspace through the regular door.
const SEED_SQL: &str = "\
INSERT INTO users (id, name, display_name, created_at, updated_at) VALUES ('user-1', 'owner', 'Owner', 1, 1); \
INSERT INTO users (id, name, display_name, created_at, updated_at) VALUES ('user-2', 'admin', 'Admin', 1, 1); \
INSERT INTO users (id, name, display_name, created_at, updated_at) VALUES ('user-3', 'member', 'Member', 1, 1); \
INSERT INTO users (id, name, display_name, created_at, updated_at) VALUES ('user-9', 'stranger', 'Stranger', 1, 1); \
INSERT INTO hosts (id, name, kind, created_at) VALUES ('host-1', 'Host', 'local', 1); \
INSERT INTO agents (id, name, display_name, runtime, host_id, created_by_user_id, created_at, updated_at) \
  VALUES ('agent-1', 'alice', 'Alice', 'claude', 'host-1', 'user-1', 1, 1); \
INSERT INTO workspaces (id, name, name_key, description, owner_agent_id, owner_user_id, archived_at, created_at, updated_at) \
  VALUES ('ws-1', 'Original', 'original', 'first', NULL, 'user-1', NULL, 10, 100); \
INSERT INTO workspaces (id, name, name_key, description, owner_agent_id, owner_user_id, archived_at, created_at, updated_at) \
  VALUES ('ws-2', 'Taken', 'taken', NULL, NULL, 'user-1', NULL, 10, 100); \
INSERT INTO workspaces (id, name, name_key, description, owner_agent_id, owner_user_id, archived_at, created_at, updated_at) \
  VALUES ('ws-home', 'Agent Home', NULL, NULL, 'agent-1', NULL, NULL, 10, 100); \
INSERT INTO workspace_users (workspace_id, user_id, joined_at, role) VALUES ('ws-1', 'user-1', 1, 'owner'); \
INSERT INTO workspace_users (workspace_id, user_id, joined_at, role) VALUES ('ws-1', 'user-2', 1, 'admin'); \
INSERT INTO workspace_users (workspace_id, user_id, joined_at, role) VALUES ('ws-1', 'user-3', 1, 'member');";

/// A stand-in adapter error, opaque like the production ones.
#[derive(Debug)]
struct FakeError(&'static str);
impl std::fmt::Display for FakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for FakeError {}

/// The clock the route samples; a fixed instant makes `updated_at`/`archived_at`
/// deterministic.
#[derive(Clone)]
struct FixedClock(i64);
impl WorkspaceMetadataClock for FixedClock {
    fn now_unix_millis(&self) -> i64 {
        self.0
    }
}

/// The viewer-hint sink, recording every id it was asked to announce and,
/// optionally, refusing — so a lost hint can be observed to NOT affect the 200.
#[derive(Clone)]
struct RecordingSink {
    announced: Arc<Mutex<Vec<String>>>,
    fail: bool,
}
impl RecordingSink {
    fn new(fail: bool) -> Self {
        Self {
            announced: Arc::new(Mutex::new(Vec::new())),
            fail,
        }
    }
    fn ids(&self) -> Vec<String> {
        self.announced.lock().unwrap().clone()
    }
}
impl WorkspaceMetadataChangedSink for RecordingSink {
    async fn workspace_metadata_changed(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<(), WorkspaceMetadataAnnounceFailure> {
        self.announced
            .lock()
            .unwrap()
            .push(workspace_id.as_str().to_owned());
        if self.fail {
            return Err(WorkspaceMetadataAnnounceFailure::from_source(FakeError(
                "viewer hint transport down",
            )));
        }
        Ok(())
    }
}

/// The rename fan-out requirement, recording every `(workspace_id, new_name)` it
/// was asked to fan out and, optionally, refusing — so a lost fan-out can be
/// observed to NOT affect the 200, and so "fires iff the display name moved" is
/// load-bearing.
#[derive(Clone)]
struct RecordingFanout {
    fanned: Arc<Mutex<Vec<(String, String)>>>,
    fail: bool,
}
impl RecordingFanout {
    fn new(fail: bool) -> Self {
        Self {
            fanned: Arc::new(Mutex::new(Vec::new())),
            fail,
        }
    }
    fn calls(&self) -> Vec<(String, String)> {
        self.fanned.lock().unwrap().clone()
    }
}
impl WorkspaceRenameFanout for RecordingFanout {
    async fn fan_out_rename(
        &self,
        workspace_id: &WorkspaceId,
        change: &WorkspaceNameChange,
    ) -> Result<(), WorkspaceRenameFanoutFailure> {
        self.fanned
            .lock()
            .unwrap()
            .push((workspace_id.as_str().to_owned(), change.new_name.clone()));
        if self.fail {
            return Err(WorkspaceRenameFanoutFailure::from_source(FakeError(
                "rename fan-out transport down",
            )));
        }
        Ok(())
    }
}

/// A seeded database plus the clock/sink fakes that make the route's effects
/// observable.
struct Harness {
    _dir: tempfile::TempDir,
    path: std::path::PathBuf,
}

impl Harness {
    async fn seeded() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace-metadata-write-e2e.sqlite");
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .foreign_keys(true);
        let mut db = SqliteConnection::connect_with(&options).await.unwrap();
        db.execute(sqlx::raw_sql(CURRENT_SCHEMA)).await.unwrap();
        db.execute(sqlx::raw_sql(SEED_SQL)).await.unwrap();
        db.close().await.unwrap();
        Self { _dir: dir, path }
    }

    /// Build the real router over the real stores and the given fakes, with a
    /// silent (non-recording, non-failing) rename fan-out — for the tests that only
    /// care about ordering / the 200 body / the viewer hint.
    async fn router(&self, clock: FixedClock, sink: RecordingSink) -> Router {
        self.router_with_fanout(clock, sink, RecordingFanout::new(false))
            .await
    }

    /// Build the real router with an explicit rename fan-out spy, for the tests that
    /// assert the fan-out obligation.
    async fn router_with_fanout(
        &self,
        clock: FixedClock,
        sink: RecordingSink,
        fanout: RecordingFanout,
    ) -> Router {
        let governance = SqliteAccessStore::open(&self.path).await.unwrap();
        let writes = SqliteWorkspaceMetadataWrite::open(&self.path)
            .await
            .unwrap();
        workspace_metadata_write::router(governance, writes, sink, fanout, clock)
    }

    /// The stored `(name, name_key, archived_at)` — the durable truth a committed
    /// or refused write leaves behind.
    async fn stored(&self, id: &str) -> (String, Option<String>, Option<i64>) {
        let mut db = SqliteConnection::connect(&format!("sqlite://{}", self.path.display()))
            .await
            .unwrap();
        let row = sqlx::query_as::<_, (String, Option<String>, Option<i64>)>(
            "SELECT name, name_key, archived_at FROM workspaces WHERE id = ?",
        )
        .bind(id)
        .fetch_one(&mut db)
        .await
        .unwrap();
        db.close().await.unwrap();
        row
    }
}

fn metadata_request(id: &str, user: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method("PATCH")
        .uri(format!("/api/workspaces/{id}"))
        .header("content-type", "application/json")
        .extension(UserId::parse(user).unwrap())
        .body(Body::from(body.to_owned()))
        .unwrap()
}

fn archive_request(id: &str, user: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method("PATCH")
        .uri(format!("/api/workspaces/{id}/archive"))
        .header("content-type", "application/json")
        .extension(UserId::parse(user).unwrap())
        .body(Body::from(body.to_owned()))
        .unwrap()
}

async fn json_body(response: axum::response::Response) -> Value {
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&body).unwrap()
}

#[tokio::test]
async fn a_rename_by_the_owner_commits_and_drives_both_the_hint_and_the_fanout() {
    let harness = Harness::seeded().await;
    let sink = RecordingSink::new(false);
    let fanout = RecordingFanout::new(false);
    let app = harness
        .router_with_fanout(FixedClock(777), sink.clone(), fanout.clone())
        .await;

    let response = app
        .oneshot(metadata_request(
            "ws-1",
            "user-1",
            r#"{"name":"Design Team"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(
        body,
        json!({
            "workspace": {
                "id": "ws-1",
                "name": "Design Team",
                "description": "first",
                "owner_agent_id": null,
                "owner_user_id": "user-1",
                "dm_channel_id": null,
                "dm_channel_ids": [],
                "archived_at": null,
                "created_at": 10,
                "updated_at": 777
            }
        })
    );
    // The rename is durably committed with the folded key.
    assert_eq!(
        harness.stored("ws-1").await,
        (
            "Design Team".to_owned(),
            Some("design team".to_owned()),
            None
        )
    );
    // BOTH committed-rename effects fired for exactly this workspace, off the one
    // sealed gate: the viewer hint and the per-channel rename fan-out.
    assert_eq!(sink.ids(), vec!["ws-1".to_owned()]);
    assert_eq!(
        fanout.calls(),
        vec![("ws-1".to_owned(), "Design Team".to_owned())]
    );
}

#[tokio::test]
async fn a_non_admin_is_forbidden_before_the_body_is_parsed() {
    let harness = Harness::seeded().await;
    let sink = RecordingSink::new(false);
    let app = harness.router(FixedClock(777), sink.clone()).await;

    // A member sends a MALFORMED body. If the body were parsed first this would be
    // a 400; the 403 proves the governance floor is resolved before the body.
    let response = app
        .oneshot(metadata_request("ws-1", "user-3", "not even json"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(json_body(response).await, json!({ "error": "forbidden" }));
    // No name moved, so the viewer hint never fired.
    assert!(sink.ids().is_empty());
    assert_eq!(harness.stored("ws-1").await.0, "Original");
}

#[tokio::test]
async fn an_unknown_workspace_is_not_found() {
    let harness = Harness::seeded().await;
    let app = harness
        .router(FixedClock(777), RecordingSink::new(false))
        .await;

    let response = app
        .oneshot(metadata_request("ws-missing", "user-1", r#"{"name":"X"}"#))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        json_body(response).await,
        json!({ "error": "workspace not found" })
    );
}

#[tokio::test]
async fn an_agent_home_workspace_is_not_found_through_the_regular_door() {
    let harness = Harness::seeded().await;
    let app = harness
        .router(FixedClock(777), RecordingSink::new(false))
        .await;

    let response = app
        .oneshot(metadata_request("ws-home", "user-1", r#"{"name":"X"}"#))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_empty_edit_from_an_admin_is_invalid_input() {
    let harness = Harness::seeded().await;
    let app = harness
        .router(FixedClock(777), RecordingSink::new(false))
        .await;

    // user-2 is an admin, so it clears the rename floor and the body IS decoded.
    // `{}` is structurally valid, so it clears native `Json` and reaches the
    // classifier: no field is present, so `WorkspaceMetadataEdit::try_from` refuses
    // (the Zod `.refine` "no fields" case) and collapses into the one terminal
    // semantic `422` envelope (ratified divergence — the Zod issue array is gone).
    let response = app
        .oneshot(metadata_request("ws-1", "user-2", "{}"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        json_body(response).await,
        json!({ "error": "invalid input" })
    );
}

#[tokio::test]
async fn a_value_shaped_array_from_the_owner_is_a_native_rejection_at_body_decode_with_zero_effect()
{
    let harness = Harness::seeded().await;
    let sink = RecordingSink::new(false);
    let app = harness.router(FixedClock(777), sink.clone()).await;

    // The owner CLEARS the governance floor, so decode runs — which is exactly
    // where the shared object floor must catch a top-level array. This body lines
    // up by type/value/order with `WorkspaceUpdateRaw` (name, description): decoded
    // positionally it would be a valid rename. `ObjectOnly` rejects the sequence
    // shape (native structural `422`, not our semantic envelope), so nothing is
    // classified and nothing commits.
    let response = app
        .oneshot(metadata_request(
            "ws-1",
            "user-1",
            r#"["Renamed Team","a new description"]"#,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    // Zero effect: the name is untouched and the viewer hint never fired.
    assert_eq!(harness.stored("ws-1").await.0, "Original");
    assert!(sink.ids().is_empty());
}

#[tokio::test]
async fn a_name_held_by_another_workspace_is_conflict() {
    let harness = Harness::seeded().await;
    let app = harness
        .router(FixedClock(777), RecordingSink::new(false))
        .await;

    let response = app
        .oneshot(metadata_request("ws-1", "user-1", r#"{"name":"Taken"}"#))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        json_body(response).await,
        json!({ "error": "workspace name already exists" })
    );
    // Nothing moved.
    assert_eq!(harness.stored("ws-1").await.0, "Original");
}

#[tokio::test]
async fn a_description_only_edit_owes_neither_the_hint_nor_the_fanout() {
    let harness = Harness::seeded().await;
    let sink = RecordingSink::new(false);
    let fanout = RecordingFanout::new(false);
    let app = harness
        .router_with_fanout(FixedClock(500), sink.clone(), fanout.clone())
        .await;

    let response = app
        .oneshot(metadata_request(
            "ws-1",
            "user-1",
            r#"{"description":"second"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["workspace"]["description"], json!("second"));
    assert_eq!(body["workspace"]["name"], json!("Original"));
    assert_eq!(body["workspace"]["updated_at"], json!(500));
    // No display name moved: neither committed-rename effect is owed.
    assert!(sink.ids().is_empty());
    assert!(fanout.calls().is_empty());
}

#[tokio::test]
async fn a_committed_rename_survives_both_a_failing_hint_and_a_failing_fanout() {
    let harness = Harness::seeded().await;
    // BOTH committed-rename effects are broken. The commit must still stand as a
    // 200 with the committed face, and — the discriminating grid — each effect must
    // still have been ATTEMPTED exactly once and independently: a failing hint does
    // not stop the fan-out, a failing fan-out does not downgrade the 200.
    let sink = RecordingSink::new(true);
    let fanout = RecordingFanout::new(true);
    let app = harness
        .router_with_fanout(FixedClock(777), sink.clone(), fanout.clone())
        .await;

    let response = app
        .oneshot(metadata_request(
            "ws-1",
            "user-1",
            r#"{"name":"Design Team"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        json_body(response).await["workspace"]["name"],
        json!("Design Team")
    );
    // The rename is durably committed regardless of both failures.
    assert_eq!(harness.stored("ws-1").await.0, "Design Team");
    // Each effect was attempted exactly once; neither failure downgraded the 200.
    assert_eq!(sink.ids(), vec!["ws-1".to_owned()]);
    assert_eq!(
        fanout.calls(),
        vec![("ws-1".to_owned(), "Design Team".to_owned())]
    );
}

#[tokio::test]
async fn archive_requires_the_owner_floor() {
    let harness = Harness::seeded().await;
    let app = harness
        .router(FixedClock(777), RecordingSink::new(false))
        .await;

    // An admin clears the rename floor but NOT the archive floor.
    let response = app
        .oneshot(archive_request("ws-1", "user-2", r#"{"archived":true}"#))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(harness.stored("ws-1").await.2, None);
}

#[tokio::test]
async fn an_archive_value_shaped_array_from_the_owner_is_a_native_rejection_with_zero_effect() {
    let harness = Harness::seeded().await;
    let app = harness
        .router(FixedClock(900), RecordingSink::new(false))
        .await;

    // The owner clears the archive floor, so decode runs. `WorkspaceArchiveRaw`'s
    // single `archived: bool` field means `[true]` lines up positionally with a
    // valid archive. The shared object floor rejects the array shape (native `422`)
    // before the flag is read, so the workspace is never archived.
    let response = app
        .oneshot(archive_request("ws-1", "user-1", r#"[true]"#))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(harness.stored("ws-1").await.2, None);
}

#[tokio::test]
async fn the_owner_archives_and_unarchives() {
    let harness = Harness::seeded().await;
    let archive_app = harness
        .router(FixedClock(900), RecordingSink::new(false))
        .await;

    let response = archive_app
        .oneshot(archive_request("ws-1", "user-1", r#"{"archived":true}"#))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["workspace"]["archived_at"], json!(900));
    assert_eq!(harness.stored("ws-1").await.2, Some(900));

    // A fresh router (a new pool) clears it again.
    let unarchive_app = harness
        .router(FixedClock(950), RecordingSink::new(false))
        .await;
    let response = unarchive_app
        .oneshot(archive_request("ws-1", "user-1", r#"{"archived":false}"#))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        json_body(response).await["workspace"]["archived_at"],
        Value::Null
    );
    assert_eq!(harness.stored("ws-1").await.2, None);
}

#[tokio::test]
async fn a_missing_archived_flag_is_a_native_structural_rejection() {
    let harness = Harness::seeded().await;
    let app = harness
        .router(FixedClock(777), RecordingSink::new(false))
        .await;

    // `{}` is missing the required `archived` boolean, so the native `Json`
    // extractor for `WorkspaceArchiveRaw` refuses it BEFORE any classifier runs.
    // That keeps axum's native structural `422` (with its own plain-text body),
    // not our semantic envelope — the Zod issue array parity is retired.
    let response = app
        .oneshot(archive_request("ws-1", "user-1", "{}"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn a_body_above_the_legacy_16kib_cap_with_unknown_keys_is_accepted() {
    let harness = Harness::seeded().await;
    let sink = RecordingSink::new(false);
    let app = harness.router(FixedClock(777), sink.clone()).await;

    // A syntactically valid rename body with a huge UNKNOWN key. The leaf imposes
    // no size cap (a body-size perimeter is a mount-seam concern), and `z.object`
    // strips unknown keys — so this parses to `{name:"Design"}` and renames, it is
    // NOT a 413. This is the regression guard for the removed 16 KiB→413 cap: the
    // product route has no such limit, so a large legitimate body must succeed.
    let large = format!(r#"{{"name":"Design","pad":"{}"}}"#, "a".repeat(20_000));
    let response = app
        .oneshot(metadata_request("ws-1", "user-1", &large))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(harness.stored("ws-1").await.0, "Design");
}

#[tokio::test]
async fn a_non_governor_is_refused_without_polling_the_body() {
    let harness = Harness::seeded().await;
    let app = harness
        .router(FixedClock(777), RecordingSink::new(false))
        .await;

    let polls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = polls.clone();
    let body = Body::from_stream(futures_util::stream::once(async move {
        observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok::<_, std::io::Error>(r#"{"name":"Design"}"#)
    }));
    let request = metadata_request("ws-1", "user-3", "").map(|_| body);
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(polls.load(std::sync::atomic::Ordering::SeqCst), 0);
}
