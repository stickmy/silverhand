//! Perimeter/adapter composition proof for `GET /api/agents` (#task:2197, batch 5).
//!
//! `agent_list_contract.rs` already drives the real `SqliteAgentListColdRead` through
//! the real `identity_access::agent_list::router` and pins the wire bytes — but it injects
//! the `UserId` extension by hand, standing in for the perimeter. What no such test can
//! show is how the route behaves behind the product's human perimeter:
//! that an anonymous caller is refused BEFORE the port is touched, that a real seeded
//! session resolves to the viewer the body reads for, that a stranger's session yields
//! the empty array with no existence oracle, and that the path answers `GET` only.
//!
//! So this composes the exact production seam — `require_human_session` over the real
//! `HumanSessionAuthenticator`/`SqliteSessionStore`, wrapping the real adapter+router —
//! and drives it end to end over real seeded `auth_sessions`. This harness builds
//! that composition itself; application-root mounting is covered by the app suite.
//! The adapter is wrapped in
//! a counting decorator ONLY so "the anonymous arm reads nothing" is a hard assertion;
//! every admitted read still executes the real `creator UNION relation` SQL.
//!
//! Order, filtering, projection and the ACL are the concern of the layers this mounts
//! and are pinned by their own tests; this asserts the MOUNT, not a second reading of
//! visibility.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering::SeqCst},
};

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::Value;
use silverhand_backend_core::{
    foundation::identity::UserId,
    foundation::persistence::PersistenceFailure,
    identity_access::agent_detail_read::AgentDetailFacts,
    identity_access::agent_list_read::AgentListReadModel,
    identity_access::auth::{HumanSessionAuthenticator, SessionClock, SessionTtl},
};
use silverhand_backend_server::{
    identity_access::agent_list::{AGENTS_PATH, router},
    identity_access::auth::{PublicJsonUnauthorized, require_human_session},
};
use silverhand_backend_sqlite::{
    identity_access::agent_list_read::SqliteAgentListColdRead,
    identity_access::auth::SqliteSessionStore,
};
use sqlx::Connection;
use tower::ServiceExt;

use crate::current_schema;

/// `NOW_MS - 9500 = 500 < TTL`, so the seeded sessions are valid — the same window the
/// scope-events perimeter E2E proves the session authority honors.
const NOW_MS: i64 = 10_000;
const TTL_MS: i64 = 1_000;

/// `sha256 → URL_SAFE_NO_PAD` of the raw bearer, the hashing the session authority
/// applies. Two known pairs, reused verbatim from the scope-events perimeter fixture;
/// which viewer a token resolves to is decided purely by the seeded row it hashes into
/// (the hashes are inlined in [`SEED`]'s `auth_sessions` rows).
const VIEWER_TOKEN: &str = "valid-token";
const STRANGER_TOKEN: &str = "admin-token";

/// `u-viewer` creates `a1` (created_at 1000) and reaches `a2` (created_at 2000, created
/// by `u-other`) through workspace `w1`; `u-stranger` is a registered human in no arm.
/// Two seeded sessions bind each real bearer to its user id.
const SEED: &str = "\
    INSERT INTO users (id, name, display_name, created_at, updated_at) \
    VALUES ('u-viewer', 'viewer', 'Viewer', 1, 1); \
    INSERT INTO users (id, name, display_name, created_at, updated_at) \
    VALUES ('u-other', 'other', 'Other', 1, 1); \
    INSERT INTO users (id, name, display_name, created_at, updated_at) \
    VALUES ('u-stranger', 'stranger', 'Stranger', 1, 1); \
    INSERT INTO hosts (id, name, kind, created_at) VALUES ('h1', 'host-one', 'desktop', 1); \
    INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, created_by_user_id, \
      materialization_status) \
    VALUES ('a1', 'agent-one', 'qoder', 'h1', 1000, 1000, 'u-viewer', 'active'); \
    INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, created_by_user_id, \
      materialization_status) \
    VALUES ('a2', 'agent-two', 'codex', 'h1', 2000, 2000, 'u-other', 'active'); \
    INSERT INTO workspaces \
      (id, name, name_key, description, owner_agent_id, archived_at, created_at, updated_at) \
    VALUES ('w1', 'Workspace One', 'workspace-one', NULL, NULL, NULL, 1, 1); \
    INSERT INTO workspace_members (workspace_id, agent_id, joined_at) VALUES ('w1', 'a2', 1); \
    INSERT INTO workspace_users (workspace_id, user_id, joined_at) VALUES ('w1', 'u-viewer', 1); \
    INSERT INTO auth_sessions \
      (id, user_id, token_hash, created_at, last_used_at, revoked_at, user_agent, ip_first_seen) \
    VALUES ('s-viewer', 'u-viewer', 'OXoqnFv14szsOMJZa2grsb0F_m5OzqbBDPQnVf8iVAM', \
      1, 9500, NULL, NULL, NULL); \
    INSERT INTO auth_sessions \
      (id, user_id, token_hash, created_at, last_used_at, revoked_at, user_agent, ip_first_seen) \
    VALUES ('s-stranger', 'u-stranger', 'EKTHyfxSBtbzbcaUSoG7b0o8sOJQFK47EubD5ScSKSo', \
      1, 9500, NULL, NULL, NULL);";

/// A clock frozen at the instant the seeded sessions are valid under.
#[derive(Clone)]
struct FrozenClock(i64);

impl SessionClock for FrozenClock {
    fn now_unix_millis(&self) -> i64 {
        self.0
    }
}

/// Wraps the real cold read so the anonymous arm can assert the port was NEVER touched;
/// every admitted read delegates to the real `SqliteAgentListColdRead` SQL.
#[derive(Clone)]
struct CountingColdRead {
    inner: SqliteAgentListColdRead,
    reads: Arc<AtomicUsize>,
}

impl AgentListReadModel for CountingColdRead {
    async fn read_agent_candidates(
        &self,
        viewer: &UserId,
    ) -> Result<Vec<AgentDetailFacts>, PersistenceFailure> {
        self.reads.fetch_add(1, SeqCst);
        self.inner.read_agent_candidates(viewer).await
    }
}

/// The mounted vertical: the real adapter+router guarded by the real human perimeter,
/// plus the shared read counter and the `TempDir` guard the pools outlive.
struct Mounted {
    app: Router,
    reads: Arc<AtomicUsize>,
    _directory: tempfile::TempDir,
}

async fn mounted() -> Mounted {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("agent-list-mount.sqlite");

    let mut db = current_schema::create_current_schema(&path).await;
    for statement in SEED.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        sqlx::query(statement).execute(&mut db).await.unwrap();
    }
    db.close().await.unwrap();

    let reads = Arc::new(AtomicUsize::new(0));
    let cold_read = CountingColdRead {
        inner: SqliteAgentListColdRead::open(&path).await.unwrap(),
        reads: reads.clone(),
    };
    let authority = HumanSessionAuthenticator::new(
        SqliteSessionStore::open(&path).await.unwrap(),
        FrozenClock(NOW_MS),
        SessionTtl::from_millis(TTL_MS).unwrap(),
    );

    let app = require_human_session(router(cold_read), authority, PublicJsonUnauthorized);
    Mounted {
        app,
        reads,
        _directory: directory,
    }
}

/// Drives the mounted app. `bearer` of `None` omits the `Authorization` header entirely
/// — a genuinely anonymous request, not a malformed token.
async fn call(app: &Router, method: &str, bearer: Option<&str>) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(AGENTS_PATH)
        .header("content-type", "application/json");
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

fn ids(body: &Value) -> Vec<String> {
    body["agents"]
        .as_array()
        .expect("the envelope carries an agents array")
        .iter()
        .map(|agent| agent["id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn an_anonymous_caller_is_refused_before_the_port_is_touched() {
    let mounted = mounted().await;
    let (status, body) = call(&mounted.app, "GET", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body, serde_json::json!({ "error": "unauthorized" }));
    // The perimeter rejected before dispatch: the product read never ran.
    assert_eq!(mounted.reads.load(SeqCst), 0);
}

#[tokio::test]
async fn a_seeded_session_resolves_to_the_viewer_the_body_reads_for() {
    let mounted = mounted().await;
    let (status, body) = call(&mounted.app, "GET", Some(VIEWER_TOKEN)).await;
    assert_eq!(status, StatusCode::OK);
    // The real adapter's `created_at ASC`: `a1` (own) then `a2` (workspace-reached).
    assert_eq!(ids(&body), ["a1", "a2"]);
    assert_eq!(mounted.reads.load(SeqCst), 1);
}

#[tokio::test]
async fn a_stranger_session_gets_the_empty_array_with_no_existence_oracle() {
    let mounted = mounted().await;
    let (status, body) = call(&mounted.app, "GET", Some(STRANGER_TOKEN)).await;
    assert_eq!(status, StatusCode::OK);
    // A list face has no 404: an agent the caller may not see is simply ABSENT.
    assert_eq!(body, serde_json::json!({ "agents": [] }));
    assert_eq!(mounted.reads.load(SeqCst), 1);
}

#[tokio::test]
async fn the_mounted_path_answers_get_only() {
    let mounted = mounted().await;
    // Authenticated, so a 405 is the ROUTE's method surface, not the perimeter's 401.
    let (status, _) = call(&mounted.app, "POST", Some(VIEWER_TOKEN)).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
}
