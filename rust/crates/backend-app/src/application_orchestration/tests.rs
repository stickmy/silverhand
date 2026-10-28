use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use futures_util::{SinkExt, StreamExt};
use silverhand_backend_core::{
    application_orchestration::teardown::intent::{BeginDeletion, begin_agent_deletion},
    foundation::identity::AgentId,
};
use silverhand_backend_server::{
    application_orchestration::daemon_delivery::{HOST_WEBSOCKET_PATH, OUTBOUND_PROTOCOL_VERSION},
    conversation_messaging::messaging::MESSAGE_SEND_MAX_BODY_BYTES,
    foundation::scope_event_transport::{
        DESKTOP_API_EPOCH_HEADER, EVENT_WEBSOCKET_PATH, EVENT_WS_SESSION_COOKIE,
        MINIMUM_CLIENT_API_EPOCH,
    },
    host_runtime::host_agent_background::agent_deletion_resumer::DeletionRunOutcome,
};
use sqlx::{Connection, Executor};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{
        Message as ClientMessage,
        client::IntoClientRequest,
        http::header::{COOKIE, ORIGIN},
    },
};
use tower::ServiceExt;

use super::*;
use crate::*;

const TOKEN: &str = "valid-token";
const TOKEN_HASH: &str = "OXoqnFv14szsOMJZa2grsb0F_m5OzqbBDPQnVf8iVAM";

/// Unit-composition convenience: obtain a real bound-listener witness, finalize the
/// app once through the production seam, then release the unused socket. Tests that
/// exercise serving or bootstrap lifecycle directly use [`bind_and_prepare`] so the
/// exact witnessed listener is also the one they serve.
async fn prepare(config: &AppConfig) -> Result<PreparedApp, StartupError> {
    let (listener, app) = bind_and_prepare(config).await?;
    drop(listener);
    Ok(app)
}

// #2257's static topology guard (a string-count over include_str!(lib.rs)) is
// superseded by the #task:2265 composition checker in `build.rs`
// (`build_support/composition_check.rs`): a `syn` whole-crate production module-graph
// walk that closes the three semantic doors (bind / bootstrap-provision /
// router-finalize) with direct-callee + cardinality proofs and rejects alias / wrapper
// / function-value bypass, failing `cargo check` rather than a test.
// A second seeded principal: the admin actor who performs the enrollment. The
// target viewer (TOKEN) and this admin are told apart purely by which seeded
// session their bearer hashes to.
const ADMIN_TOKEN: &str = "admin-token";

#[test]
fn migration_permission_preserves_launcher_defaults_and_strict_override() {
    for profile in [DeploymentProfile::Embedded, DeploymentProfile::RemoteOnly] {
        assert_eq!(
            parse_schema_upgrade(profile, None).unwrap(),
            profile == DeploymentProfile::Embedded
        );
        assert_eq!(
            parse_schema_upgrade(profile, Some("")).unwrap(),
            profile == DeploymentProfile::Embedded
        );
        assert!(!parse_schema_upgrade(profile, Some(" 0 ")).unwrap());
        assert!(parse_schema_upgrade(profile, Some(" 1 ")).unwrap());
        for bad in ["true", "false", " ", "2"] {
            assert!(parse_schema_upgrade(profile, Some(bad)).is_err());
        }
    }
}
const ADMIN_HASH: &str = "EKTHyfxSBtbzbcaUSoG7b0o8sOJQFK47EubD5ScSKSo";
const CONVERSATION_ID: &str = "conv000000000001";
const WORKSPACE_ID: &str = "workspace0000001";
// A long-lived host credential: its bearer (`HOST_TOKEN`) hashes to
// `HOST_TOKEN_HASH`, the value seeded into `host_credentials` so a daemon can
// authenticate the mounted host WebSocket and attach `h-online`.
const HOST_TOKEN: &str = "host-token-cp2";
const HOST_TOKEN_HASH: &str = "3fQKxe8pqexFp6vP8YxtFscumzfMzrDjOG4Jz-e7j1E";

/// The same shared cold-read fixture the #2190 parity gate replays: it makes
/// `user-1` a member of `ws-archived`/`ws-regular`/`ws-agent` and NOT of
/// `ws-foreign`.
const ENROLLMENT_SEED_SQL: &str =
    include_str!("../../../../../rust/test-fixtures/fixtures/workspace-cold-read/seed.sql");

struct Fixture {
    _directory: TempDir,
    path: PathBuf,
}

async fn fixture(with_ledger: bool) -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("backend-app.sqlite");
    let mut database = current_schema::create_current_schema(&path).await;
    if with_ledger {
        database
                .execute(
                    "CREATE TABLE _migrations (version INTEGER PRIMARY KEY, name TEXT NOT NULL, applied_at INTEGER NOT NULL)",
                )
                .await
                .unwrap();
        for step in MIGRATION_STEPS.split(',') {
            sqlx::query(
                "INSERT INTO _migrations (version, name, applied_at) VALUES (?, 'applied', 1)",
            )
            .bind(step.parse::<i64>().unwrap())
            .execute(&mut database)
            .await
            .unwrap();
        }
    }
    database.close().await.unwrap();
    Fixture {
        _directory: directory,
        path,
    }
}

/// The default composition config. #2257 promotes a categorized-log directory to a
/// REQUIRED startup input (the mounted CLI-preflight and agent owner-audit routes are
/// durable audit producers), so the default supplies one — a `logs/` sibling inside
/// the fixture tempdir, created by the sink on open. Tests that must prove the
/// missing-directory fail-closed contract use [`config_without_log_dir`].
fn config(path: PathBuf) -> AppConfig {
    let log_dir = path.parent().unwrap().join("logs");
    config_with_log_dir(path, Some(log_dir))
}

/// [`config`] with NO log directory: the resident falsifier for #2257's mount
/// precondition. Driving bound-listener finalization with this must fail closed with
/// [`StartupError::AuditLogDirectoryRequired`] — the audit routes never mount.
fn config_without_log_dir(path: PathBuf) -> AppConfig {
    config_with_log_dir(path, None)
}

/// [`config`] with an explicit categorized-log directory, so a composition test can
/// drive composition into constructing a REAL CLI audit sink at a known path.
fn config_with_log_dir(path: PathBuf, log_dir: Option<PathBuf>) -> AppConfig {
    // A sibling blobs dir inside the fixture's tempdir, created on blob-store open.
    let blobs_root = path.parent().unwrap().join("blobs");
    AppConfig::new(
        path,
        blobs_root,
        "127.0.0.1:0".parse().unwrap(),
        vec![AllowedOrigin::parse("http://localhost:5173".to_owned()).unwrap()],
        Some("@silverhand-ai/daemon@test".to_owned()),
        // The composition gates seed a human perimeter and probe the embedded shape:
        // Embedded projects `ServerMode::Embedded` + `DeploymentMode::Embedded`, so
        // `/api/runtimes` is the detecting router (still anon → 401 behind the
        // perimeter) and `/readyz` omits the fanout key.
        DeploymentProfile::Embedded,
        true,
        log_dir,
        None,
        None,
    )
    .unwrap()
}

/// The same config as [`config`] but under the standalone launcher's profile, so a
/// composition differential can drive the RemoteOnly half of the ONE product:
/// `ServerMode::Remote` + `DeploymentMode::Standalone`, the control-plane runtimes
/// router (no detector), and a `/readyz` whose Standalone shape carries the
/// installed fanout key (independent of daemon attachment).
fn remote_config(path: PathBuf) -> AppConfig {
    let blobs_root = path.parent().unwrap().join("blobs");
    let log_dir = path.parent().unwrap().join("logs");
    AppConfig::new(
        path,
        blobs_root,
        "127.0.0.1:0".parse().unwrap(),
        vec![AllowedOrigin::parse("http://localhost:5173".to_owned()).unwrap()],
        Some("@silverhand-ai/daemon@test".to_owned()),
        DeploymentProfile::RemoteOnly,
        true,
        Some(log_dir),
        None,
        None,
    )
    .unwrap()
}

/// [`config`] variant that also pins the listen address, deployment profile, and the
/// embedded-bootstrap token path. The actual bound listener address, not this requested
/// address, completes the enable/disable decision. Blobs live in the fixture tempdir.
fn config_with_bootstrap(
    path: PathBuf,
    listen_addr: SocketAddr,
    profile: DeploymentProfile,
    token_path: Option<PathBuf>,
) -> AppConfig {
    let blobs_root = path.parent().unwrap().join("blobs");
    let log_dir = path.parent().unwrap().join("logs");
    AppConfig::new(
        path,
        blobs_root,
        listen_addr,
        vec![AllowedOrigin::parse("http://localhost:5173".to_owned()).unwrap()],
        Some("@silverhand-ai/daemon@test".to_owned()),
        profile,
        true,
        Some(log_dir),
        token_path,
        None,
    )
    .unwrap()
}

async fn seed_human(path: &Path) {
    let now = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    let pool = SqlitePoolOptions::new()
        .connect(path.to_str().unwrap())
        .await
        .unwrap();
    sqlx::query(
            "INSERT INTO users (id, name, display_name, created_at, updated_at) VALUES ('user-1', 'viewer', 'Viewer', 1, 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
            "INSERT INTO auth_sessions (id, user_id, token_hash, created_at, last_used_at, revoked_at, user_agent, ip_first_seen) VALUES ('session-1', 'user-1', ?, ?, ?, NULL, NULL, NULL)",
        )
        .bind(TOKEN_HASH)
        .bind(now)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}

async fn seed_runtime_agent(path: &Path, agent_id: &str) {
    let pool = SqlitePoolOptions::new()
        .connect(path.to_str().unwrap())
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO hosts (id, name, kind, created_at, user_id) \
         VALUES ('h-registry-probe', 'host-registry-probe', 'remote', 1, 'user-1')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, \
         created_by_user_id, materialization_status, session_scope) \
         VALUES (?, 'registry-probe', 'qoder', 'h-registry-probe', 1, 1, \
         'user-1', 'active', 'agent')",
    )
    .bind(agent_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT OR IGNORE INTO host_credentials \
         (id,host_id,token_hash,kind,expires_at,consumed_at,revoked_at) \
         VALUES('cred-registry-probe','h-registry-probe',?,'long_lived',NULL,NULL,NULL)",
    )
    .bind(HOST_TOKEN_HASH)
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
}

async fn mint_runtime_bearer(app: &PreparedApp, agent_id: &str) -> String {
    mint_runtime_bearer_for(app, agent_id, CONVERSATION_ID).await
}

async fn mint_runtime_bearer_for(
    app: &PreparedApp,
    agent_id: &str,
    conversation_id: &str,
) -> String {
    mint_runtime_bearer_from(&app.test_runtime_credentials, agent_id, conversation_id).await
}

async fn mint_runtime_bearer_from(
    credentials: &silverhand_backend_server::host_runtime::runtime_credential::RuntimeCredentialAuthority<
        silverhand_backend_sqlite::host_runtime::runtime_credential::SqliteRuntimeCustodyStore,
    >,
    agent_id: &str,
    conversation_id: &str,
) -> String {
    use std::{
        convert::Infallible,
        future::Future,
        sync::{Arc, Mutex},
    };

    use silverhand_backend_core::{
        foundation::identity::{ConversationId, DaemonInstanceId},
        host_runtime::runtime_credential::RuntimeScope,
    };
    use silverhand_backend_server::host_runtime::runtime_credential::{
        RuntimeDispatchFailure, RuntimeStartHandoff, RuntimeStartTransport,
    };

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

    let transport = CapturingStart::default();
    credentials
        .dispatch_start(
            RuntimeScope::AgentSlot {
                agent: AgentId::parse(agent_id).unwrap(),
            },
            DaemonInstanceId::parse("daemon-registry-probe").unwrap(),
            ConversationId::parse(conversation_id).unwrap(),
            &transport,
        )
        .await
        .unwrap();
    format!("Bearer {}", transport.0.lock().unwrap().clone().unwrap())
}

async fn seed_workspace_conversation(path: &Path) {
    let pool = SqlitePoolOptions::new()
        .connect(path.to_str().unwrap())
        .await
        .unwrap();
    sqlx::query(
            "INSERT INTO workspaces (id, name, name_key, owner_user_id, created_at, updated_at) VALUES (?, 'Space', 'space', 'user-1', 1, 1)",
        )
        .bind(WORKSPACE_ID)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
            "INSERT INTO workspace_users (workspace_id, user_id, joined_at, role) VALUES (?, 'user-1', 1, 'owner')",
        )
        .bind(WORKSPACE_ID)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
            "INSERT INTO conversations (id, workspace_id, placement_kind, roster_policy, title, title_key, status, visibility, created_at, updated_at) VALUES (?, ?, 'workspace', 'open', 'General', 'general', 'open', 'private', 1, 1)",
        )
        .bind(CONVERSATION_ID)
        .bind(WORKSPACE_ID)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
            "INSERT INTO conversation_users (conversation_id, user_id, joined_at) VALUES (?, 'user-1', 1)",
        )
        .bind(CONVERSATION_ID)
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}

/// Seed the channel-directory/detail read surface for viewer `user-1`: a Regular
/// workspace they belong to, one open workspace channel they are a member of, and the
/// sort state the active projection reads. Mirrors the leaf directory fixture's proven
/// column/value set — a valid `ChannelStatus` (`idle`) and a `title_key` that is both
/// canonical to its title and spelling-valid (so the detail decode's
/// `CanonicalConversationTitle` and the directory decode's spelling check both pass) —
/// which `seed_workspace_conversation`'s `status='open'` does NOT satisfy for this
/// adapter (that helper serves routes that never decode a `ChannelStatus`). The
/// literals equal `WORKSPACE_ID`/`CONVERSATION_ID`; the E2E asserts on those consts,
/// so any drift fails loudly.
async fn seed_channel_directory(path: &Path) {
    let pool = SqlitePoolOptions::new()
        .connect(path.to_str().unwrap())
        .await
        .unwrap();
    sqlx::raw_sql(
            "INSERT INTO workspaces(id,name,name_key,created_at,updated_at) VALUES \
               ('workspace0000001','Workspace','workspace',1,1); \
             INSERT INTO workspace_users(workspace_id,user_id,joined_at,role) VALUES \
               ('workspace0000001','user-1',1,'owner'); \
             INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,status,visibility,archived_at,created_at,updated_at) VALUES \
               ('conv000000000001','workspace0000001','workspace','open','General','general','idle','private',NULL,1,1); \
             INSERT INTO conversation_users(conversation_id,user_id,joined_at) VALUES \
               ('conv000000000001','user-1',1); \
             INSERT INTO user_list_sort_mode(user_id,list_key,scope,mode,updated_at) VALUES \
               ('user-1','channels:workspace0000001','','manual',1); \
             INSERT INTO user_sort_entry(user_id,list_key,entity_id,rank,last_accessed_at,starred_at) VALUES \
               ('user-1','channels:workspace0000001','conv000000000001',1,4,5);",
        )
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}

/// Seed two agents `user-1` can view through the creator arm, each on its own
/// live, unrevoked host. `a-online` sits on `h-online`, whose long-lived
/// credential lets a daemon attach over the mounted host WebSocket; `a-offline`
/// sits on `h-offline`, which no daemon ever attaches. The two are equally
/// visible and equally reachable — their ONLY difference is the live attachment,
/// so a difference in the availability answer can come only from the route
/// reading the real transport registry the WebSocket mutates.
async fn seed_reachable_agents(path: &Path) {
    let pool = SqlitePoolOptions::new()
        .connect(path.to_str().unwrap())
        .await
        .unwrap();
    sqlx::query(
            "INSERT INTO hosts (id, name, kind, created_at, user_id) VALUES ('h-online', 'host-online', 'remote', 1, 'user-1')",
        )
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
            "INSERT INTO hosts (id, name, kind, created_at, user_id) VALUES ('h-offline', 'host-offline', 'remote', 1, 'user-1')",
        )
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
            "INSERT INTO host_credentials (id, host_id, token_hash, kind, expires_at, consumed_at, revoked_at) VALUES ('cred-online', 'h-online', ?, 'long_lived', NULL, NULL, NULL)",
        )
        .bind(HOST_TOKEN_HASH)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
            "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, created_by_user_id) VALUES ('a-online', 'agent-online', 'qoder', 'h-online', 1, 1, 'user-1')",
        )
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
            "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, created_by_user_id) VALUES ('a-offline', 'agent-offline', 'qoder', 'h-offline', 1, 1, 'user-1')",
        )
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}

/// Seed the agent-presence ACL fixture behind the perimeter: a creator-visible
/// agent carrying a committed `last_activity_at` (so a real away deadline
/// projects), and a registered agent owned by a DIFFERENT human that the viewer
/// (`user-1`) is in no arm of (not creator, no direct pair, no shared workspace)
/// — its presence must collapse to the same anti-enumeration 404 as an absent id.
/// Both rows carry `materialization_status = 'active'`, which the presence adapter
/// strict-decodes; `seed_reachable_agents` omits it, so this leaf seeds its own.
async fn seed_presence_agents(path: &Path) {
    let pool = SqlitePoolOptions::new()
        .connect(path.to_str().unwrap())
        .await
        .unwrap();
    sqlx::query(
            "INSERT INTO users (id, name, display_name, created_at, updated_at) VALUES ('u-other', 'other', 'Other', 1, 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
            "INSERT INTO hosts (id, name, kind, created_at, user_id) VALUES ('h-presence', 'host-presence', 'remote', 1, 'user-1')",
        )
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
            "INSERT INTO hosts (id, name, kind, created_at, user_id) VALUES ('h-other', 'host-other', 'remote', 1, 'u-other')",
        )
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
            "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, created_by_user_id, materialization_status, last_activity_at) VALUES ('a-presence', 'agent-presence', 'qoder', 'h-presence', 1, 1, 'user-1', 'active', 1000)",
        )
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
            "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, created_by_user_id, materialization_status, last_activity_at) VALUES ('a-stranger', 'agent-stranger', 'qoder', 'h-other', 1, 1, 'u-other', 'active', 1000)",
        )
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}

async fn request(
    router: &Router,
    method: &str,
    uri: &str,
    bearer: Option<&str>,
) -> (StatusCode, String) {
    request_with_body(router, method, uri, bearer, Body::from("{}")).await
}

async fn request_with_body(
    router: &Router,
    method: &str,
    uri: &str,
    bearer: Option<&str>,
    body: Body,
) -> (StatusCode, String) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(bearer) = bearer {
        builder = builder.header("authorization", bearer);
    }
    let response = router
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

// Same as `request_with_body` but declares `Content-Type: application/json`.
// #2245 convergence harness-only fix: the validator wave migrated every JSON
// write face onto the frozen `ValidatedJson` admission (#2246 object-only
// floor), whose native Axum extractor requires the JSON content type and
// answers `415` without it — the ratified, deliberate wave contract (see
// `identity_access/agent_patch.rs`: "a missing `Content-Type` is `415`"). These
// mount-composition assertions predate the wave and drove those faces with a
// bodiless / CT-less client, so they must now send the header every real
// product client already sends. Production routing/composition is unchanged;
// only these test call sites carry the header.
async fn json_request_with_body(
    router: &Router,
    method: &str,
    uri: &str,
    bearer: Option<&str>,
    body: Body,
) -> (StatusCode, String) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(bearer) = bearer {
        builder = builder.header("authorization", bearer);
    }
    let response = router
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

async fn http_over_tcp(
    address: SocketAddr,
    method: &str,
    path: &str,
    bearer: Option<&str>,
    body: &str,
) -> String {
    let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
    let authorization = bearer
        .map(|value| format!("Authorization: {value}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\n{authorization}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    client.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    client.read_to_end(&mut response).await.unwrap();
    String::from_utf8(response).unwrap()
}

/// Like [`http_over_tcp`] but carries an explicit `Content-Type` and a RAW byte
/// body, and returns the whole response as bytes — so a multipart upload and a
/// binary avatar-serve response both round-trip without a lossy UTF-8 coercion.
async fn http_over_tcp_typed(
    address: SocketAddr,
    method: &str,
    path: &str,
    bearer: Option<&str>,
    content_type: &str,
    body: &[u8],
) -> Vec<u8> {
    let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
    let authorization = bearer
        .map(|value| format!("Authorization: {value}\r\n"))
        .unwrap_or_default();
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\n{authorization}Content-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    client.write_all(head.as_bytes()).await.unwrap();
    client.write_all(body).await.unwrap();
    let mut response = Vec::new();
    client.read_to_end(&mut response).await.unwrap();
    response
}

/// Split a raw HTTP response into its (lossy-decoded) head and its raw body bytes.
fn split_http_response(response: &[u8]) -> (String, Vec<u8>) {
    let separator = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("an HTTP response has a header/body separator");
    let head = String::from_utf8_lossy(&response[..separator]).into_owned();
    (head, response[separator + 4..].to_vec())
}

#[test]
fn config_has_no_implicit_database_listener_or_origin_defaults() {
    assert!(matches!(
        AppConfig::new(
            PathBuf::new(),
            "blobs".into(),
            "127.0.0.1:0".parse().unwrap(),
            vec![AllowedOrigin::parse("http://localhost:5173".to_owned()).unwrap()],
            None,
            DeploymentProfile::Embedded,
            true,
            None,
            None,
            None,
        ),
        Err(ConfigError::EmptyDatabasePath)
    ));
    assert!(matches!(
        AppConfig::new(
            "database.sqlite".into(),
            PathBuf::new(),
            "127.0.0.1:0".parse().unwrap(),
            vec![AllowedOrigin::parse("http://localhost:5173".to_owned()).unwrap()],
            None,
            DeploymentProfile::Embedded,
            true,
            None,
            None,
            None,
        ),
        Err(ConfigError::EmptyBlobsRoot)
    ));
    assert!(matches!(
        AppConfig::new(
            "database.sqlite".into(),
            "blobs".into(),
            "127.0.0.1:0".parse().unwrap(),
            Vec::new(),
            None,
            DeploymentProfile::Embedded,
            true,
            None,
            None,
            None,
        ),
        Err(ConfigError::EmptyAllowedOrigins)
    ));
}

/// Real prepared routes and credential authorities reach the composition-owned
/// writers. Anonymous requests emit no audit record; runtime calls, CLI preflight,
/// and human agent reads land in their respective streams before shutdown returns.
#[tokio::test]
async fn prepared_routes_drain_into_disjoint_cli_and_agent_audit_files() {
    use std::{
        convert::Infallible,
        future::Future,
        sync::{Arc, Mutex},
    };

    use silverhand_backend_core::{
        foundation::identity::{ConversationId, DaemonInstanceId},
        host_runtime::runtime_credential::RuntimeScope,
    };
    use silverhand_backend_server::host_runtime::runtime_credential::{
        RuntimeDispatchFailure, RuntimeStartHandoff, RuntimeStartTransport,
    };

    let fixture = fixture(true).await;
    seed_human(&fixture.path).await;
    let pool = SqlitePoolOptions::new()
        .connect(fixture.path.to_str().unwrap())
        .await
        .unwrap();
    for statement in [
        "INSERT INTO hosts (id, name, kind, created_at, user_id) \
             VALUES ('h-cli-audit', 'host-cli-audit', 'remote', 1, 'user-1')",
        "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, \
             created_by_user_id, materialization_status, session_scope) \
             VALUES ('agent-cli-audit', 'agent-cli-audit', 'qoder', 'h-cli-audit', \
                     1, 1, 'user-1', 'active', 'agent')",
    ] {
        sqlx::query(statement).execute(&pool).await.unwrap();
    }
    pool.close().await;

    let log_dir = tempfile::tempdir().unwrap();
    let app = prepare(&config_with_log_dir(
        fixture.path.clone(),
        Some(log_dir.path().to_path_buf()),
    ))
    .await
    .unwrap();

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

    let transport = CapturingStart::default();
    app.test_runtime_credentials
        .dispatch_start(
            RuntimeScope::AgentSlot {
                agent: AgentId::parse("agent-cli-audit").unwrap(),
            },
            DaemonInstanceId::parse("daemon-cli-audit").unwrap(),
            ConversationId::parse(CONVERSATION_ID).unwrap(),
            &transport,
        )
        .await
        .unwrap();
    let bearer = format!("Bearer {}", transport.0.lock().unwrap().clone().unwrap());
    let reaction_path = "/internal/agents/agent-cli-audit/react-message";
    let (status, body) = request(&app.router, "POST", reaction_path, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");

    let faces = [
        ("GET", "/internal/agents/agent-cli-audit/list-workspaces"),
        ("GET", "/internal/agents/agent-cli-audit/list-channels"),
        ("POST", "/internal/agents/agent-cli-audit/follow-thread"),
        ("POST", "/internal/agents/agent-cli-audit/unfollow-thread"),
        ("POST", "/internal/agents/agent-cli-audit/search-messages"),
        ("POST", "/internal/agents/agent-cli-audit/read-history"),
        ("POST", "/internal/agents/agent-cli-audit/create-channel"),
        ("POST", reaction_path),
        ("DELETE", reaction_path),
        ("POST", "/internal/agents/agent-cli-audit/attachments"),
        (
            "PATCH",
            "/internal/agents/agent-cli-audit/attachments/attachment-missing",
        ),
        (
            "GET",
            "/internal/agents/agent-cli-audit/attachments/attachment-missing/locate",
        ),
        (
            "GET",
            "/internal/agents/agent-cli-audit/attachments/attachment-missing/download",
        ),
    ];
    for (method, path) in faces {
        let (status, body) = request(&app.router, method, path, Some(&bearer)).await;
        assert_ne!(status, StatusCode::UNAUTHORIZED, "{method} {path}: {body}");
    }

    let (status, body) = json_request_with_body(
        &app.router,
        "POST",
        "/internal/agents/agent-cli-audit/cli-trace",
        Some(&bearer),
        Body::from(r#"{"noun":"message","verb":"send","flags_seen":["--strict"],"error_code":"E_UNKNOWN_FLAG","error_message":"unknown flag --nope","duration_ms":3.5}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let chats = "/api/agents/agent-cli-audit/chats";
    let (status, _) = request(&app.router, "GET", chats, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, body) = request(&app.router, "GET", chats, Some(&format!("Bearer {TOKEN}"))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({"chats": []})
    );

    app.close_connections().await;
    use silverhand_backend_filesystem::foundation::audit_sink::WriterState;
    assert_eq!(app.cli_audit_sink.state(), WriterState::ClosedShutdown);
    assert_eq!(app.agent_audit_sink.state(), WriterState::ClosedShutdown);

    let mut entries = tokio::fs::read_dir(log_dir.path()).await.unwrap();
    let mut cli_log = None;
    let mut agent_log = None;
    while let Some(entry) = entries.next_entry().await.unwrap() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("cli.") && name.ends_with(".log") {
            assert!(
                cli_log.replace(entry.path()).is_none(),
                "one CLI file for the current UTC day"
            );
        } else if name.starts_with("agent-audit.") && name.ends_with(".log") {
            assert!(
                agent_log.replace(entry.path()).is_none(),
                "one agent audit file for the current UTC day"
            );
        }
    }
    let contents = tokio::fs::read_to_string(cli_log.expect("the real CLI audit file"))
        .await
        .unwrap();
    let lines: Vec<serde_json::Value> = contents
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(lines.iter().all(|line| line["app"] == "silverhand-desktop"));
    assert_eq!(
        lines.len(),
        14,
        "thirteen invokes plus the one cli-trace fact"
    );
    let invokes: Vec<_> = lines
        .iter()
        .filter(|line| line["category"] == "cli.invoke")
        .collect();
    assert_eq!(invokes.len(), 13, "one invoke per admitted residual face");
    assert!(invokes[..7].iter().any(|line| line["http_status"] == 200));
    assert!(invokes[..7].iter().any(|line| line["http_status"] == 400));

    let actual: Vec<_> = invokes
        .iter()
        .map(|line| {
            assert_eq!(line["agentId"], "agent-cli-audit");
            assert!(line["request_id"].as_str().is_some());
            assert!(line["duration_ms"].is_number());
            (
                line["noun"].as_str().unwrap(),
                line["verb"].as_str().unwrap(),
                line["method"].as_str().unwrap(),
                line["route_suffix"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        actual,
        [
            ("workspace", "list", "GET", "list-workspaces"),
            ("channel", "list", "GET", "list-channels"),
            ("thread", "follow", "POST", "follow-thread"),
            ("thread", "unfollow", "POST", "unfollow-thread"),
            ("message", "search", "POST", "search-messages"),
            ("message", "history", "POST", "read-history"),
            ("channel", "create", "POST", "create-channel"),
            ("message", "react", "POST", "react-message"),
            ("message", "react", "DELETE", "react-message"),
            ("file", "upload", "POST", "attachments"),
            ("file", "update", "PATCH", "attachments/*"),
            ("file", "view", "GET", "attachments/*/locate"),
            ("file", "view", "GET", "attachments/*/download"),
        ]
    );
    assert_eq!(
        lines
            .iter()
            .filter(|line| line["category"] == "cli.preflight_failure")
            .count(),
        1,
        "cli-trace is excluded from cli.invoke and shares the one file owner"
    );
    let preflight = lines
        .iter()
        .find(|line| line["category"] == "cli.preflight_failure")
        .unwrap();
    assert_eq!(preflight["agentId"], "agent-cli-audit");
    assert_eq!(preflight["flags_seen"], serde_json::json!(["--strict"]));
    assert_eq!(preflight["error_code"], "E_UNKNOWN_FLAG");
    assert_eq!(preflight["duration_ms"], 3.5);

    let contents = tokio::fs::read_to_string(agent_log.expect("the composed agent audit file"))
        .await
        .unwrap();
    let records: Vec<serde_json::Value> = contents
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        records.len(),
        1,
        "only the admitted owner read is audited, with no CLI records"
    );
    assert_eq!(records[0]["category"], "agent.chats_listed");
    assert_eq!(records[0]["viewer"], "user-1");
    assert_eq!(records[0]["agentId"], "agent-cli-audit");
    assert_eq!(records[0]["chatCount"], 0);
    assert_eq!(records[0]["app"], "silverhand-desktop");
}

/// #2255/#2257 convergence falsifier — the embedded-bootstrap ENABLE arm. Under the
/// embedded profile on a loopback listener with a launcher-provided token path,
/// the bound-listener seam mints a token, writes it to a `0600` file (plaintext +
/// trailing newline) BEFORE that listener can accept, and arms the authority with that token's
/// hash — proven here by consuming the exact plaintext the file holds. The raw token
/// lives only in the file the local client reads; it is never returned from `prepare`
/// nor recoverable from the authority.
#[tokio::test]
async fn prepare_arms_the_embedded_bootstrap_token_on_a_loopback_embedded_listener() {
    let fixture = fixture(true).await;
    let token_dir = tempfile::tempdir().unwrap();
    let token_path = token_dir.path().join("bootstrap.token");
    let config = config_with_bootstrap(
        fixture.path.clone(),
        "127.0.0.1:0".parse().unwrap(),
        DeploymentProfile::Embedded,
        Some(token_path.clone()),
    );
    let (listener, app) = bind_and_prepare(&config).await.unwrap();
    let bound = listener.local_addr().unwrap();
    assert!(bound.ip().is_loopback(), "the actual bound IP is loopback");
    assert_ne!(
        bound.port(),
        config.listen_addr.port(),
        "`:0` resolved to the listener's concrete bound port"
    );
    drop(listener);

    let raw = tokio::fs::read_to_string(&token_path).await.unwrap();
    assert!(raw.ends_with('\n'), "the token file ends with a newline");
    let plaintext = raw.trim_end_matches('\n');
    assert!(!plaintext.is_empty(), "a non-empty token was minted");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = tokio::fs::metadata(&token_path)
            .await
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "the token file is owner-only 0600");
    }

    // The authority is armed with THIS file's plaintext: consuming it succeeds
    // exactly once, and the one-shot consume unlinks the file.
    let token = app.embedded_bootstrap_token();
    assert!(
        token.consume(plaintext).await,
        "the armed authority accepts the file's plaintext"
    );
    assert!(!token.consume(plaintext).await, "the token is one-shot");
    assert!(!token_path.exists(), "consume unlinks the token file");

    app.close_connections().await;
}

/// The load-bearing witness differential: the already-bound listener is loopback while
/// the config value deliberately says wildcard. Finalization must arm from
/// `listener.local_addr()`; regressing to `config.listen_addr.ip()` disables this token
/// and reddens the test. Production obtains the listener from the same config, but this
/// differential seals which value the finalize seam is actually authorized to trust.
#[tokio::test]
async fn bootstrap_decision_uses_the_bound_listener_witness_not_the_config_value() {
    let fixture = fixture(true).await;
    let token_dir = tempfile::tempdir().unwrap();
    let token_path = token_dir.path().join("bootstrap.token");
    let config = config_with_bootstrap(
        fixture.path.clone(),
        "0.0.0.0:0".parse().unwrap(),
        DeploymentProfile::Embedded,
        Some(token_path.clone()),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let actual = listener.local_addr().unwrap();
    assert!(actual.ip().is_loopback());
    assert!(!config.listen_addr.ip().is_loopback());

    let (listener, app) = prepare_existing_listener_observed(&config, listener, |_, _| {})
        .await
        .unwrap();
    assert_eq!(listener.local_addr().unwrap(), actual);
    let raw = tokio::fs::read_to_string(&token_path).await.unwrap();
    assert!(
        app.embedded_bootstrap_token().consume(raw.trim_end()).await,
        "the authority arms from the actual loopback listener"
    );
    drop(listener);
    app.close_connections().await;
}

/// #2255/#2257 convergence falsifier — a NON-LOOPBACK listener disables the authority.
/// Even under the embedded profile with a token path, a wildcard actual bind means the
/// route's `LoopbackPeer` gate could never trust a peer, so finalization best-effort
/// removes stale final/temp token artifacts and arms nothing; every consume fails.
/// Unlink faults remain warning-observable while the authority stays disabled.
#[tokio::test]
async fn prepare_disables_embedded_bootstrap_on_a_non_loopback_listener() {
    let fixture = fixture(true).await;
    let token_dir = tempfile::tempdir().unwrap();
    let token_path = token_dir.path().join("bootstrap.token");
    tokio::fs::write(&token_path, b"stale-token\n")
        .await
        .unwrap();
    let token_tmp_path = bootstrap_token_tmp_path(&token_path);
    tokio::fs::write(&token_tmp_path, b"stale-raw-temp-token\n")
        .await
        .unwrap();
    let config = config_with_bootstrap(
        fixture.path.clone(),
        "0.0.0.0:0".parse().unwrap(),
        DeploymentProfile::Embedded,
        Some(token_path.clone()),
    );
    let (listener, app) = bind_and_prepare(&config).await.unwrap();
    assert_eq!(
        listener.local_addr().unwrap().ip(),
        "0.0.0.0".parse::<std::net::IpAddr>().unwrap(),
        "the decision witness is the actual wildcard listener"
    );
    drop(listener);
    assert!(
        !token_path.exists(),
        "a non-loopback listener best-effort clears the stale final token"
    );
    assert!(
        !token_tmp_path.exists(),
        "a non-loopback listener best-effort clears a crashed boot's raw temp token"
    );
    assert!(
        !app.embedded_bootstrap_token().consume("anything").await,
        "the authority is disabled off loopback"
    );
    app.close_connections().await;
}

/// #2255/#2257 convergence falsifier — the REMOTE-ONLY profile disables the authority.
/// Embedded-bootstrap is an embedded-only first-run grant; on a loopback listener under
/// the remote-only profile no token is provisioned.
#[tokio::test]
async fn prepare_disables_embedded_bootstrap_for_a_remote_only_profile() {
    let fixture = fixture(true).await;
    let token_dir = tempfile::tempdir().unwrap();
    let token_path = token_dir.path().join("bootstrap.token");
    let token_tmp_path = bootstrap_token_tmp_path(&token_path);
    tokio::fs::write(&token_path, b"stale-token\n")
        .await
        .unwrap();
    tokio::fs::write(&token_tmp_path, b"stale-raw-temp-token\n")
        .await
        .unwrap();
    let app = prepare(&config_with_bootstrap(
        fixture.path.clone(),
        "127.0.0.1:0".parse().unwrap(),
        DeploymentProfile::RemoteOnly,
        Some(token_path.clone()),
    ))
    .await
    .unwrap();
    assert!(
        !token_path.exists(),
        "the remote-only profile best-effort clears the stale final token"
    );
    assert!(
        !token_tmp_path.exists(),
        "the remote-only profile best-effort clears a crashed boot's raw temp token"
    );
    assert!(
        !app.embedded_bootstrap_token().consume("anything").await,
        "the authority is disabled under remote-only"
    );
    app.close_connections().await;
}

/// #2255/#2257 convergence falsifier — NO token path disables the authority. The
/// embedded+loopback AND still needs the launcher to name a file; without one there is
/// nothing to provision and the route stays a closed 404.
#[tokio::test]
async fn prepare_disables_embedded_bootstrap_without_a_token_path() {
    let fixture = fixture(true).await;
    let app = prepare(&config_with_bootstrap(
        fixture.path.clone(),
        "127.0.0.1:0".parse().unwrap(),
        DeploymentProfile::Embedded,
        None,
    ))
    .await
    .unwrap();
    assert!(
        !app.embedded_bootstrap_token().consume("anything").await,
        "the authority is disabled without a launcher token path"
    );
    app.close_connections().await;
}

/// #2255/#2257 — an UNCONSUMED token is unlinked on graceful shutdown. The real
/// connect-info `serve()` runs its termination tail on the shutdown signal, and the
/// token authority's idempotent cleanup deletes the file no client ever consumed.
/// (Serving to completion also witnesses that switching to
/// `into_make_service_with_connect_info` did not break a plain start/stop.)
#[tokio::test]
async fn serve_unlinks_an_unconsumed_embedded_bootstrap_token_on_shutdown() {
    let fixture = fixture(true).await;
    let token_dir = tempfile::tempdir().unwrap();
    let token_path = token_dir.path().join("bootstrap.token");
    let config = config_with_bootstrap(
        fixture.path.clone(),
        "127.0.0.1:0".parse().unwrap(),
        DeploymentProfile::Embedded,
        Some(token_path.clone()),
    );
    let (listener, app) = bind_and_prepare(&config).await.unwrap();
    assert!(
        token_path.exists(),
        "armed: the token file exists before serving"
    );

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let running = tokio::spawn(serve(listener, app, async move {
        let _ = shutdown_rx.await;
        Ok::<(), std::io::Error>(())
    }));
    shutdown_tx.send(()).unwrap();
    running.await.unwrap().unwrap();

    assert!(
        !token_path.exists(),
        "graceful shutdown unlinks the unconsumed token"
    );
}

/// #2255/#2257 — bind failure happens BEFORE every provision step. When the address is
/// already occupied the seam returns `Bind` with zero audit-writer files, zero token
/// mint/write, and therefore no rollback artifact.
#[tokio::test]
async fn bind_failure_prevents_bootstrap_token_and_audit_writer_creation() {
    let fixture = fixture(true).await;
    let token_dir = tempfile::tempdir().unwrap();
    let token_path = token_dir.path().join("bootstrap.token");

    // Occupy a loopback address so the subsequent `run` bind is guaranteed to fail.
    let occupier = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let occupied = occupier.local_addr().unwrap();

    let config = config_with_bootstrap(
        fixture.path.clone(),
        occupied,
        DeploymentProfile::Embedded,
        Some(token_path.clone()),
    );
    let log_dir = config.log_dir.clone().unwrap();
    let Err(error) = run(config).await else {
        panic!("binding an occupied address must fail");
    };
    assert!(
        matches!(error, StartupError::Bind(_)),
        "a bind error, not a token error: {error:?}"
    );
    assert!(
        !token_path.exists(),
        "bind failure has no path to token mint/write"
    );
    assert!(
        !log_dir.exists(),
        "bind failure has no path to either audit writer"
    );
}

/// A post-bind token-provision failure returns only after the listener is released and
/// both already-open audit writers have drained into the terminal closed state. This is
/// the rollback witness that excludes merely cancelling the startup future while its
/// background writer tasks remain live.
#[tokio::test]
async fn post_bind_startup_failure_closes_writers_and_leaves_no_token() {
    use silverhand_backend_filesystem::foundation::audit_sink::WriterState;

    let fixture = fixture(true).await;
    let token_path = fixture
        .path
        .parent()
        .unwrap()
        .join("missing-token-parent/bootstrap.token");
    let config = config_with_bootstrap(
        fixture.path.clone(),
        "127.0.0.1:0".parse().unwrap(),
        DeploymentProfile::Embedded,
        Some(token_path.clone()),
    );
    let listener = TcpListener::bind(config.listen_addr).await.unwrap();
    let bound = listener.local_addr().unwrap();
    let observed = std::sync::Arc::new(std::sync::Mutex::new(None));
    let observed_for_startup = observed.clone();

    let Err(error) = prepare_existing_listener_observed(&config, listener, move |cli, agent| {
        *observed_for_startup.lock().unwrap() = Some((cli, agent));
    })
    .await
    else {
        panic!("an absent token parent must fail provisioning");
    };
    assert!(matches!(error, StartupError::BootstrapToken(_)));
    assert!(!token_path.exists(), "no final token survives rollback");
    assert!(
        !bootstrap_token_tmp_path(&token_path).exists(),
        "no plaintext temp token survives rollback"
    );

    let (cli, agent) = observed.lock().unwrap().take().unwrap();
    assert_eq!(cli.state(), WriterState::ClosedShutdown);
    assert_eq!(agent.state(), WriterState::ClosedShutdown);

    // The error arm released the exact bound socket only after the awaited writer
    // rollback, so its address is immediately reusable.
    let rebound = TcpListener::bind(bound).await.unwrap();
    drop(rebound);
}

/// #2257 P0 resident falsifier — a categorized-log directory is now a REQUIRED
/// startup input, not an option. #2256 allowed the sink to be constructed only
/// optionally because its routes were UNMOUNTED; #2257 mounts two real audit
/// producers (CLI-preflight records + agent owner-audit reads), so a missing
/// directory MUST fail `prepare()` CLOSED — the router is never constructed and
/// neither audit route can mount or succeed. This drives the real `prepare()`, not
/// just a config helper, and asserts the explicit typed startup error.
#[tokio::test]
async fn prepare_fails_closed_without_an_audit_log_directory() {
    let fixture = fixture(true).await;
    let Err(error) = prepare(&config_without_log_dir(fixture.path.clone())).await else {
        panic!("a missing audit log directory must fail prepare() closed");
    };
    assert!(
        matches!(error, StartupError::AuditLogDirectoryRequired),
        "the mount precondition surfaces as the typed startup error: {error:?}"
    );
}

/// #2262: blob storage is a required production authority. If its configured root
/// cannot be opened, composition fails before returning any router and therefore
/// before any of the ten attachment faces can be exposed.
#[tokio::test]
async fn prepare_fails_closed_before_routes_when_blob_root_cannot_open() {
    let fixture = fixture(true).await;
    let log_dir = tempfile::tempdir().unwrap();
    let blob_file = fixture.path.parent().unwrap().join("not-a-blob-directory");
    tokio::fs::write(&blob_file, b"occupied").await.unwrap();
    let mut config = config_with_log_dir(fixture.path.clone(), Some(log_dir.path().to_path_buf()));
    config.blobs_root = blob_file;

    let Err(error) = prepare(&config).await else {
        panic!("an unusable blob root must fail prepare() before route exposure");
    };
    assert!(
        matches!(
            error,
            StartupError::Adapter {
                name: "blob store",
                ..
            }
        ),
        "the required authority surfaces as the typed adapter error: {error:?}"
    );
}

#[tokio::test]
async fn startup_initializes_missing_store_and_refuses_unowned_schema_before_serving() {
    use silverhand_backend_sqlite::foundation::database_startup::DatabaseStartupError;

    let directory = tempfile::tempdir().unwrap();
    let missing = directory.path().join("missing.sqlite");
    let app = prepare(&config(missing.clone())).await.unwrap();
    assert!(missing.exists());
    app.close_connections().await;

    let unowned = fixture(false).await;
    let Err(error) = prepare(&config(unowned.path)).await else {
        panic!("schema without migration ownership must fail closed");
    };
    assert!(matches!(
        error,
        StartupError::DatabasePreparation(DatabaseStartupError::TooOld(0))
    ));
}

/// #2223 CP5: the four host residual routes are mounted behind the human perimeter and
/// reach their real `ComputerReadModel`/ownership authority — not a no-op mount. For
/// each (method, path): anonymous is refused `401` by the perimeter (auth precedes the
/// handler), and an authenticated request naming an UNKNOWN computer returns the shared
/// anti-enumeration `404` with the EXACT domain body `{host not found, host_not_found}`.
/// That domain body is the discriminator a bare status code cannot give: an unmounted
/// route falls to axum's empty-body `404`, and a no-op mount could not synthesize the
/// domain code — only the real read authority, consulted and returning "no such
/// computer", produces it. All four collapse absent and unowned to that one answer.
#[tokio::test]
async fn the_root_mounts_the_four_host_residual_routes_behind_auth_over_the_real_read() {
    let fixture = fixture(true).await;
    seed_human(&fixture.path).await;
    let app = prepare(&config(fixture.path.clone())).await.unwrap();
    let bearer = format!("Bearer {TOKEN}");
    let host_not_found = r#"{"error":"host not found","error_code":"host_not_found"}"#;

    // (method, unknown-id path) for detail, revoke, connect-token rotate, delete. Each
    // uses the `{id}` param subtree; `unknown-host` never matches the static
    // `connect-token`/`claim-or-create` siblings.
    let routes = [
        ("GET", "/api/hosts/unknown-host"),
        ("POST", "/api/hosts/unknown-host/revoke"),
        ("POST", "/api/hosts/unknown-host/connect-token/rotate"),
        ("DELETE", "/api/hosts/unknown-host"),
    ];

    for (method, uri) in routes {
        let (anon_status, _) = request(&app.router, method, uri, None).await;
        assert_eq!(
            anon_status,
            StatusCode::UNAUTHORIZED,
            "{method} {uri}: the perimeter must refuse an anonymous caller before the handler"
        );

        let (authed_status, authed_body) = request(&app.router, method, uri, Some(&bearer)).await;
        assert_eq!(
            authed_status,
            StatusCode::NOT_FOUND,
            "{method} {uri}: an authenticated unknown-computer request reaches the mounted \
                 route and its read authority"
        );
        assert_eq!(
            authed_body, host_not_found,
            "{method} {uri}: the domain 404 body proves the real read authority was \
                 consulted, not an empty-body fallback from an absent route"
        );
    }
}

/// #2223 CP4: the composed avatar body-limit proof. It drives the avatar upload
/// route through the FULLY composed production router — the real 128 MiB request
/// perimeter (a `Limited` stream-wrap plus `DefaultBodyLimit::disable()`, NOT a
/// `DefaultBodyLimit::max`) over the real `users::router`, the real `SqliteUserStore`,
/// and the real `LocalBlobStore` at the configured blobs root. It proves the two size
/// tiers stay observably separate at composition: a 5 MiB avatar at the cap is
/// admitted (the perimeter wrap never truncates it below its own ceiling), one byte
/// over yields the LEAF's exact `avatar_too_large` 413 (the domain's own bounded read
/// — not the envelope — is the sub-128-MiB authority), and a request DECLARING more
/// than 128 MiB is refused by the perimeter with its own distinct body before any
/// route is matched. The ceiling is a resource cap, not a uniform-response contract:
/// an UNDECLARED oversize body is bounded (never aggregated without limit) but its
/// wire error is the downstream reader's, so this proof drives the declared-length
/// short-circuit. All three hit the SAME method/path, so only the body size
/// distinguishes the outcomes.
#[tokio::test]
async fn composed_avatar_upload_separates_the_five_mib_cap_from_the_perimeter_ceiling() {
    let fixture = fixture(true).await;
    seed_human(&fixture.path).await;
    let app = prepare(&config(fixture.path.clone())).await.unwrap();
    let bearer = format!("Bearer {TOKEN}");

    let boundary = "XBOUNDARY223";
    let content_type = format!("multipart/form-data; boundary={boundary}");
    let upload = |bytes: &[u8]| -> Body {
        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            b"Content-Disposition: form-data; name=\"file\"; filename=\"a.png\"\r\n",
        );
        body.extend_from_slice(b"Content-Type: image/png\r\n\r\n");
        body.extend_from_slice(bytes);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        Body::from(body)
    };

    // Tier 1 — exactly AVATAR_MAX_BYTES: admitted through the real perimeter (200),
    // and the row's cache-busted avatar_url is set (proving the bytes reached the blob
    // store, not truncated by the 128 MiB stream wrap).
    let cap = 5 * 1024 * 1024;
    let response = app
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(users::ME_AVATAR_PATH)
                .header("authorization", &bearer)
                .header("content-type", &content_type)
                .body(upload(&vec![0x21u8; cap]))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "a 5 MiB avatar at the cap is admitted through the real composed perimeter"
    );
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        text.contains("/api/users/user-1/avatar?v="),
        "the 200 body carries the cache-busted avatar_url: {text}"
    );

    // Tier 2 — one byte over the cap: the leaf's EXACT `avatar_too_large` 413, never
    // the perimeter body and never a silent truncation to success.
    let response = app
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(users::ME_AVATAR_PATH)
                .header("authorization", &bearer)
                .header("content-type", &content_type)
                .body(upload(&vec![0x21u8; cap + 1]))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(
        &body[..],
        br#"{"error":"Avatar must be 5 MB or smaller.","code":"avatar_too_large"}"#,
        "5 MiB+1 is the domain 413, observably distinct from the perimeter body"
    );

    // Tier 3 — a request DECLARING more than 128 MiB: the perimeter's own 413,
    // returned from its declared-Content-Length short-circuit before any byte is read
    // or any route matched. A one-byte actual body proves the HEADER alone drives it.
    let over_ceiling = 134_217_729_u64;
    let response = app
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(users::ME_AVATAR_PATH)
                .header("authorization", &bearer)
                .header("content-type", &content_type)
                .header("content-length", over_ceiling.to_string())
                .body(Body::from("x"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(
        &body[..],
        br#"{"error":"payload too large"}"#,
        "a declared 128 MiB+1 is the perimeter's own 413, distinct from the avatar 413"
    );
}

#[tokio::test]
async fn the_executable_root_mounts_real_human_products_and_auth_precedes_product_work() {
    let fixture = fixture(true).await;
    seed_human(&fixture.path).await;
    let app = prepare(&config(fixture.path.clone())).await.unwrap();

    let before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages")
        .fetch_one(
            &SqlitePoolOptions::new()
                .connect(fixture.path.to_str().unwrap())
                .await
                .unwrap(),
        )
        .await
        .unwrap();
    let refused_human = request(&app.router, "POST", "/api/channels/missing/messages", None).await;
    assert_eq!(refused_human.0, StatusCode::UNAUTHORIZED);
    assert_eq!(refused_human.1, r#"{"error":"unauthorized"}"#);
    let refused_agent = request(
        &app.router,
        "POST",
        "/internal/agents/agent-1/send-message",
        Some("Bearer unknown-runtime-token"),
    )
    .await;
    assert_eq!(refused_agent.0, StatusCode::UNAUTHORIZED);
    let after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages")
        .fetch_one(
            &SqlitePoolOptions::new()
                .connect(fixture.path.to_str().unwrap())
                .await
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!((before, after), (0, 0));

    let direct_pairs = request(
        &app.router,
        "GET",
        "/api/direct-pairs",
        Some(&format!("Bearer {TOKEN}")),
    )
    .await;
    assert_eq!(direct_pairs.0, StatusCode::OK);
    assert_eq!(direct_pairs.1, r#"{"direct_pairs":[]}"#);

    let host = json_request_with_body(
        &app.router,
        "POST",
        "/api/hosts",
        Some(&format!("Bearer {TOKEN}")),
        Body::from("{}"),
    )
    .await;
    assert_eq!(host.0, StatusCode::OK, "{}", host.1);
    assert!(
        host.1
            .contains(r#""daemon_npm_spec":"@silverhand-ai/daemon@test""#)
    );
    app.close_connections().await;
}

#[tokio::test]
async fn both_auth_boundaries_precede_body_fences_and_broken_product_authorities() {
    let fixture = fixture(true).await;
    let app = prepare(&config(fixture.path.clone())).await.unwrap();
    let oversized = vec![b'x'; MESSAGE_SEND_MAX_BODY_BYTES + 1];

    let human_oversized = request_with_body(
        &app.router,
        "POST",
        &format!("/api/channels/{CONVERSATION_ID}/messages"),
        None,
        Body::from(oversized.clone()),
    )
    .await;
    assert_eq!(human_oversized.0, StatusCode::UNAUTHORIZED);
    let agent_oversized = request_with_body(
        &app.router,
        "POST",
        "/internal/agents/agent-1/send-message",
        Some("Bearer unknown-runtime-token"),
        Body::from(oversized),
    )
    .await;
    assert_eq!(agent_oversized.0, StatusCode::UNAUTHORIZED);

    let pool = SqlitePoolOptions::new()
        .connect(fixture.path.to_str().unwrap())
        .await
        .unwrap();
    sqlx::query("DROP TABLE conversations")
        .execute(&pool)
        .await
        .unwrap();
    let messages_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages")
        .fetch_one(&pool)
        .await
        .unwrap();

    let human_broken_authority = request_with_body(
        &app.router,
        "POST",
        &format!("/api/channels/{CONVERSATION_ID}/messages"),
        Some("Bearer unknown-human-token"),
        Body::from(r#"{"content":"hello"}"#),
    )
    .await;
    assert_eq!(human_broken_authority.0, StatusCode::UNAUTHORIZED);
    let agent_broken_authority = request_with_body(
        &app.router,
        "POST",
        "/internal/agents/agent-1/send-message",
        Some("Bearer unknown-runtime-token"),
        Body::from(format!(
            r##"{{"content":"hello","target":"#conv:{CONVERSATION_ID}"}}"##
        )),
    )
    .await;
    assert_eq!(agent_broken_authority.0, StatusCode::UNAUTHORIZED);
    let messages_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!((messages_before, messages_after), (0, 0));
    pool.close().await;
    app.close_connections().await;
}

#[tokio::test]
async fn a_composed_human_send_commits_and_reaches_the_same_production_event_hub() {
    let fixture = fixture(true).await;
    seed_human(&fixture.path).await;
    seed_workspace_conversation(&fixture.path).await;
    let app = prepare(&config(fixture.path.clone())).await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let running = tokio::spawn(serve(listener, app, async move {
        let _ = shutdown_rx.await;
        Ok::<(), std::io::Error>(())
    }));

    let mut websocket_request = format!("ws://{address}{EVENT_WEBSOCKET_PATH}")
        .into_client_request()
        .unwrap();
    websocket_request
        .headers_mut()
        .insert(ORIGIN, "http://localhost:5173".parse().unwrap());
    websocket_request.headers_mut().insert(
        COOKIE,
        format!("{EVENT_WS_SESSION_COOKIE}={TOKEN}")
            .parse()
            .unwrap(),
    );
    websocket_request
        .headers_mut()
        .insert(DESKTOP_API_EPOCH_HEADER, MINIMUM_CLIENT_API_EPOCH.into());
    let (mut socket, _) = connect_async(websocket_request).await.unwrap();
    let authenticated = socket.next().await.unwrap().unwrap();
    let ClientMessage::Text(authenticated) = authenticated else {
        panic!("authenticated frame must be text");
    };
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&authenticated).unwrap()["type"],
        "authenticated"
    );
    socket
        .send(ClientMessage::Text(
            serde_json::json!({
                "type": "subscribe",
                "scope": format!("conversation:{CONVERSATION_ID}"),
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
    let subscribed = socket.next().await.unwrap().unwrap();
    let ClientMessage::Text(subscribed) = subscribed else {
        panic!("subscribed frame must be text");
    };
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&subscribed).unwrap()["type"],
        "subscribed"
    );

    let response = http_over_tcp(
        address,
        "POST",
        &format!("/api/channels/{CONVERSATION_ID}/messages"),
        Some(&format!("Bearer {TOKEN}")),
        r#"{"content":"composed hello"}"#,
    )
    .await;
    assert!(
        response.starts_with("HTTP/1.1 201 Created\r\n"),
        "{response}"
    );

    let event = socket.next().await.unwrap().unwrap();
    let ClientMessage::Text(event) = event else {
        panic!("message event must be text");
    };
    let event: serde_json::Value = serde_json::from_str(&event).unwrap();
    assert_eq!(event["scope"], format!("conversation:{CONVERSATION_ID}"));
    assert_eq!(event["kind"], "message");
    assert_eq!(event["payload"]["message"]["content"], "composed hello");

    let pool = SqlitePoolOptions::new()
        .connect(fixture.path.to_str().unwrap())
        .await
        .unwrap();
    let committed: (String, String) =
        sqlx::query_as("SELECT id, content FROM messages WHERE conversation_id = ?")
            .bind(CONVERSATION_ID)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(committed.1, "composed hello");
    assert_eq!(event["payload"]["message"]["id"], committed.0);
    pool.close().await;

    socket.close(None).await.unwrap();
    shutdown_tx.send(()).unwrap();
    running.await.unwrap().unwrap();
}

#[tokio::test]
async fn the_real_listener_serves_the_composed_router_then_shuts_down_cleanly() {
    let fixture = fixture(true).await;
    let app = prepare(&config(fixture.path)).await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let running = tokio::spawn(serve(listener, app, async move {
        let _ = shutdown_rx.await;
        Ok::<(), std::io::Error>(())
    }));

    let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
    client
        .write_all(
            b"GET /api/direct-pairs HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await
        .unwrap();
    let mut response = Vec::new();
    client.read_to_end(&mut response).await.unwrap();
    let response = String::from_utf8(response).unwrap();
    assert!(response.starts_with("HTTP/1.1 401 Unauthorized\r\n"));
    assert!(response.ends_with(r#"{"error":"unauthorized"}"#));

    shutdown_tx.send(()).unwrap();
    running.await.unwrap().unwrap();
}

#[tokio::test]
async fn shutdown_signal_failure_is_a_sourced_nonzero_run_failure() {
    let fixture = fixture(true).await;
    let app = prepare(&config(fixture.path)).await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let failure = std::io::Error::other("signal authority unavailable");
    let error = serve(listener, app, async move { Err(failure) })
        .await
        .unwrap_err();
    assert!(matches!(error, StartupError::ShutdownSignal(_)));
    assert_eq!(
        describe(&error),
        "shutdown signal authority failed: signal authority unavailable"
    );
}

/// Seeds the enrollment E2E database: the shared cold-read fixture, the
/// enrolling admin and its `ws-foreign` admin membership, two live sessions
/// (valid under the real system clock), and the migration ledger the schema
/// admission owns — so `prepare` opens it as a fully-owned production database.
async fn seed_enrollment_scenario(path: &Path) {
    let now = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    let mut db = current_schema::create_current_schema(path).await;
    db.execute(sqlx::raw_sql(ENROLLMENT_SEED_SQL))
        .await
        .expect("shared cold-read seed must load against the current schema");
    for statement in [
        "INSERT INTO users (id, name, display_name, created_at, updated_at) \
             VALUES ('admin-1', 'Admin', 'Admin', 1, 1)",
        "INSERT INTO workspace_users (workspace_id, user_id, joined_at, role) \
             VALUES ('ws-foreign', 'admin-1', 1, 'admin')",
    ] {
        sqlx::query(statement).execute(&mut db).await.unwrap();
    }
    for (id, user, hash) in [
        ("session-target", "user-1", TOKEN_HASH),
        ("session-admin", "admin-1", ADMIN_HASH),
    ] {
        sqlx::query(
            "INSERT INTO auth_sessions (id, user_id, token_hash, created_at, last_used_at, \
                 revoked_at, user_agent, ip_first_seen) VALUES (?, ?, ?, ?, ?, NULL, NULL, NULL)",
        )
        .bind(id)
        .bind(user)
        .bind(hash)
        .bind(now)
        .bind(now)
        .execute(&mut db)
        .await
        .unwrap();
    }
    db.execute(
            "CREATE TABLE _migrations (version INTEGER PRIMARY KEY, name TEXT NOT NULL, applied_at INTEGER NOT NULL)",
        )
        .await
        .unwrap();
    for step in MIGRATION_STEPS.split(',') {
        sqlx::query("INSERT INTO _migrations (version, name, applied_at) VALUES (?, 'applied', 1)")
            .bind(step.parse::<i64>().unwrap())
            .execute(&mut db)
            .await
            .unwrap();
    }
    db.close().await.unwrap();
}

/// Parses the JSON body of a `Connection: close` HTTP/1.1 response (headers and
/// body are split by the blank line; these small bodies carry `Content-Length`,
/// never chunked framing).
fn http_json_body(response: &str) -> serde_json::Value {
    let body = response
        .split_once("\r\n\r\n")
        .expect("response separates headers from body")
        .1;
    serde_json::from_str(body).expect("response body is JSON")
}

fn workspace_ids(envelope: &serde_json::Value) -> Vec<String> {
    envelope["workspaces"]
        .as_array()
        .expect("the envelope carries a workspaces array")
        .iter()
        .map(|workspace| workspace["id"].as_str().unwrap().to_owned())
        .collect()
}

/// The mounted enrollment→hint→cold-read contract, proven through the ONE served
/// composition root over real TCP (#task:2171).
///
/// The seam-level causal proof (`add_workspace_human_member` → one hub → frame +
/// cold read) is owned by #2190 and composes the surfaces ad hoc. This test proves
/// the remaining, composition-only obligation: that `prepare` actually MOUNTS the
/// enrollment route, `GET /api/workspaces`, and the scope-events WebSocket onto the
/// single served router, all sharing ONE process `ScopeEventHub` and ONE real
/// `HumanSessionAuthenticator`. Nothing here is driven in-process — every HTTP call
/// and the socket cross the real listener.
///
/// It goes RED if the mount regresses: if the enrollment sink and the WS perimeter
/// held different hubs the frame would never reach the socket; if the two doors did
/// not share the one session authority the seeded bearers would not authenticate;
/// if either route were left unmounted its request would 404 instead of the asserted
/// answer.
#[tokio::test]
async fn the_mounted_root_carries_enrollment_to_the_ws_hint_and_the_fresh_cold_read() {
    use futures_util::SinkExt;
    use tokio_tungstenite::{
        connect_async,
        tungstenite::{
            Message as ClientMessage,
            client::IntoClientRequest,
            http::header::{COOKIE, ORIGIN},
        },
    };

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("enroll-e2e.sqlite");
    seed_enrollment_scenario(&path).await;
    let app = prepare(&config(path.clone())).await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let running = tokio::spawn(serve(listener, app, async move {
        let _ = shutdown_rx.await;
        Ok::<(), std::io::Error>(())
    }));

    // Auth-before-product at BOTH mounted doors: a missing credential is a 401 that
    // never reaches the product port, so `ws-foreign` gains no member.
    let unauth_read = http_over_tcp(address, "GET", "/api/workspaces", None, "").await;
    assert!(
        unauth_read.starts_with("HTTP/1.1 401 Unauthorized\r\n"),
        "{unauth_read}"
    );
    let unauth_enroll = http_over_tcp(
        address,
        "POST",
        "/api/workspaces/ws-foreign/members",
        None,
        r#"{"kind":"human","user_id":"user-1"}"#,
    )
    .await;
    assert!(
        unauth_enroll.starts_with("HTTP/1.1 401 Unauthorized\r\n"),
        "{unauth_enroll}"
    );

    // The page-load authority BEFORE the change, through the mounted cold endpoint:
    // `ws-foreign` is absent for the target viewer.
    let before = http_over_tcp(
        address,
        "GET",
        "/api/workspaces",
        Some(&format!("Bearer {TOKEN}")),
        "",
    )
    .await;
    assert!(before.starts_with("HTTP/1.1 200 OK\r\n"), "{before}");
    let before_ids = workspace_ids(&http_json_body(&before));
    assert_eq!(before_ids, ["ws-archived", "ws-regular", "ws-agent"]);

    // The target subscribes its own viewer stream on the mounted WebSocket, its WS
    // principal resolved from the seeded target bearer by the same real session
    // authority the HTTP doors use.
    let mut request = format!("ws://{address}{EVENT_WEBSOCKET_PATH}")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert(ORIGIN, "http://localhost:5173".parse().unwrap());
    request.headers_mut().insert(
        COOKIE,
        format!("{EVENT_WS_SESSION_COOKIE}={TOKEN}")
            .parse()
            .unwrap(),
    );
    request
        .headers_mut()
        .insert(DESKTOP_API_EPOCH_HEADER, MINIMUM_CLIENT_API_EPOCH.into());
    let (mut socket, _) = connect_async(request).await.unwrap();
    let authenticated = next_socket_json(&mut socket).await;
    assert_eq!(authenticated["type"], "authenticated");
    socket
        .send(ClientMessage::Text(
            serde_json::json!({ "type": "subscribe", "scope": "viewer:self" })
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    assert_eq!(next_socket_json(&mut socket).await["type"], "subscribed");

    // The CAUSE: the admin's authenticated enrollment POST across real TCP.
    let enroll = http_over_tcp(
        address,
        "POST",
        "/api/workspaces/ws-foreign/members",
        Some(&format!("Bearer {ADMIN_TOKEN}")),
        r#"{"kind":"human","user_id":"user-1"}"#,
    )
    .await;
    assert!(enroll.starts_with("HTTP/1.1 201 Created\r\n"), "{enroll}");

    // EFFECT — the hint crosses the mounted WebSocket to the target's own viewer
    // subscription: the coordinate + role, carried as a whole envelope, no list.
    let hint = next_socket_json(&mut socket).await;
    assert_eq!(hint["scope"], "viewer:user-1");
    assert_eq!(hint["kind"], "viewer_workspaces_changed");
    assert_eq!(hint["seq"], 1);
    assert_eq!(
        hint["payload"],
        serde_json::json!({
            "change": "added", "workspace_id": "ws-foreign", "role": "member"
        })
    );

    // The durable committed row the frame must agree with: exactly one, default role.
    let pool = SqlitePoolOptions::new()
        .connect(path.to_str().unwrap())
        .await
        .unwrap();
    let row: (String,) = sqlx::query_as(
        "SELECT role FROM workspace_users WHERE workspace_id = 'ws-foreign' AND user_id = 'user-1'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.0, "member");

    // The SAME mounted cold read now honors the hint: `ws-foreign` leads and is the
    // only id the target gained. Cold read — not the transport seq — is the list
    // authority.
    let after = http_over_tcp(
        address,
        "GET",
        "/api/workspaces",
        Some(&format!("Bearer {TOKEN}")),
        "",
    )
    .await;
    assert!(after.starts_with("HTTP/1.1 200 OK\r\n"), "{after}");
    let after_ids = workspace_ids(&http_json_body(&after));
    assert_eq!(
        after_ids,
        ["ws-foreign", "ws-archived", "ws-regular", "ws-agent"]
    );
    let gained: Vec<&String> = after_ids
        .iter()
        .filter(|id| !before_ids.contains(id))
        .collect();
    assert_eq!(gained, ["ws-foreign"]);

    // Idempotent re-enroll: still 201, still exactly one row.
    let reenroll = http_over_tcp(
        address,
        "POST",
        "/api/workspaces/ws-foreign/members",
        Some(&format!("Bearer {ADMIN_TOKEN}")),
        r#"{"kind":"human","user_id":"user-1"}"#,
    )
    .await;
    assert!(
        reenroll.starts_with("HTTP/1.1 201 Created\r\n"),
        "{reenroll}"
    );
    let count: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM workspace_users WHERE workspace_id = 'ws-foreign' AND user_id = 'user-1'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count.0, 1, "re-enroll must not add a second membership");
    pool.close().await;
    socket.close(None).await.unwrap();

    // The re-enroll owed NO announce. Prove the stream did not advance by the hub's
    // own high-water, not by a wait that merely fails to observe: a FRESH subscription
    // to the same viewer scope is TOLD the current seq in its `subscribed` ack, and
    // that value is deterministic. Exactly one hint was ever emitted (the create), so
    // the witness must be 1; a spurious re-enroll frame would have advanced it to 2.
    // The cold read above — not this seq — is the list-completion authority; seq only
    // bounds how many events were emitted.
    let mut witness = format!("ws://{address}{EVENT_WEBSOCKET_PATH}")
        .into_client_request()
        .unwrap();
    witness
        .headers_mut()
        .insert(ORIGIN, "http://localhost:5173".parse().unwrap());
    witness.headers_mut().insert(
        COOKIE,
        format!("{EVENT_WS_SESSION_COOKIE}={TOKEN}")
            .parse()
            .unwrap(),
    );
    witness
        .headers_mut()
        .insert(DESKTOP_API_EPOCH_HEADER, MINIMUM_CLIENT_API_EPOCH.into());
    let (mut witness_socket, _) = connect_async(witness).await.unwrap();
    assert_eq!(
        next_socket_json(&mut witness_socket).await["type"],
        "authenticated"
    );
    witness_socket
        .send(ClientMessage::Text(
            serde_json::json!({ "type": "subscribe", "scope": "viewer:self" })
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    let subscribed = next_socket_json(&mut witness_socket).await;
    assert_eq!(subscribed["type"], "subscribed");
    assert_eq!(subscribed["scope"], "viewer:user-1");
    assert_eq!(
        subscribed["seq"], 1,
        "one create hint was emitted and the idempotent re-enroll emitted none, \
             so a fresh subscription's high-water must be exactly 1"
    );

    witness_socket.close(None).await.unwrap();
    shutdown_tx.send(()).unwrap();
    running.await.unwrap().unwrap();
}

/// Auth-before-product, proven positively at BOTH mounted doors under a broken product
/// state (`workspace_users` dropped after `prepare`, so the cold read's membership
/// `EXISTS` and the enrollment policy's role read both fault). A missing OR unknown
/// credential is still an EXACT 401 at each door — auth runs to completion before the
/// product port is consulted, so the break is invisible to it. The SAME broken state
/// drives an authenticated caller through to the product surface and a safe 500, which
/// excludes the two false-greens a bare 401 cannot: a route never mounted (would 404)
/// and a "broken" table that never took effect (would 200/201).
#[tokio::test]
async fn auth_precedes_product_at_both_doors_under_a_broken_product_state() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("auth-order-e2e.sqlite");
    seed_enrollment_scenario(&path).await;
    let app = prepare(&config(path.clone())).await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let running = tokio::spawn(serve(listener, app, async move {
        let _ = shutdown_rx.await;
        Ok::<(), std::io::Error>(())
    }));

    // Healthy first: the authenticated target's read WORKS, so a later 500 is caused by
    // the break we introduce, not by a pre-existing fault.
    let healthy = http_over_tcp(
        address,
        "GET",
        "/api/workspaces",
        Some(&format!("Bearer {TOKEN}")),
        "",
    )
    .await;
    assert!(healthy.starts_with("HTTP/1.1 200 OK\r\n"), "{healthy}");

    // Break the one product table BOTH doors consult; auth's own tables are untouched.
    let pool = SqlitePoolOptions::new()
        .connect(path.to_str().unwrap())
        .await
        .unwrap();
    sqlx::query("DROP TABLE workspace_users")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    // Missing AND unknown credential, both doors → exact 401: auth refuses before the
    // now-broken product port is ever touched.
    for bearer in [None, Some("Bearer not-a-real-token")] {
        let read = http_over_tcp(address, "GET", "/api/workspaces", bearer, "").await;
        assert!(
            read.starts_with("HTTP/1.1 401 Unauthorized\r\n"),
            "unauthenticated read must be 401 regardless of product health: {read}"
        );
        let enroll = http_over_tcp(
            address,
            "POST",
            "/api/workspaces/ws-foreign/members",
            bearer,
            r#"{"kind":"human","user_id":"user-1"}"#,
        )
        .await;
        assert!(
            enroll.starts_with("HTTP/1.1 401 Unauthorized\r\n"),
            "unauthenticated enroll must be 401 regardless of product health: {enroll}"
        );
    }

    // The SAME broken state with real credentials: each door authenticates, then reaches
    // the broken product port and fails safe. Not 404 (route IS mounted), not 200/201
    // (the break IS effective) — a safe 500.
    let broken_read = http_over_tcp(
        address,
        "GET",
        "/api/workspaces",
        Some(&format!("Bearer {TOKEN}")),
        "",
    )
    .await;
    assert!(
        broken_read.starts_with("HTTP/1.1 500 Internal Server Error\r\n"),
        "an authenticated read must reach the broken product port and fail safe: {broken_read}"
    );
    let broken_enroll = http_over_tcp(
        address,
        "POST",
        "/api/workspaces/ws-foreign/members",
        Some(&format!("Bearer {ADMIN_TOKEN}")),
        r#"{"kind":"human","user_id":"user-1"}"#,
    )
    .await;
    assert!(
        broken_enroll.starts_with("HTTP/1.1 500 Internal Server Error\r\n"),
        "an authenticated enroll must reach the broken product port and fail safe: {broken_enroll}"
    );

    shutdown_tx.send(()).unwrap();
    running.await.unwrap().unwrap();
}

/// The composition mounts `GET /api/agents/{id}/availability` behind the SAME
/// human bearer/session perimeter that fronts the other human products, wired to
/// the real SQLite availability cold read and the real daemon attachment
/// controller. This proves the composition-only obligation — routed to the leaf,
/// behind the perimeter, over a real adapter — and not the leaf's
/// ACL/reachability behavior, which its own component and parity tests own.
#[tokio::test]
async fn the_composition_serves_agent_availability_behind_the_perimeter() {
    let fixture = fixture(true).await;
    seed_human(&fixture.path).await;
    let app = prepare(&config(fixture.path.clone())).await.unwrap();
    let router = app.router;
    let bearer = format!("Bearer {TOKEN}");
    // `AVAILABILITY_PATH` is a `{id}` template; drive a concrete, unseeded id.
    let path = "/api/agents/agent-absent/availability";

    // Anonymous → the shared perimeter's exact refusal envelope, reading nothing:
    // the 401 belongs to the perimeter the root mounts, so it is byte-identical to
    // the envelope every human product behind it answers with.
    let (status, body) = request(&router, "GET", path, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body, r#"{"error":"unauthorized"}"#);

    // Authenticated → the request clears the perimeter, reaches the mounted leaf,
    // and the real cold read finds no such agent: the route's OWN anti-enumeration
    // 404 JSON. An UNMOUNTED path would give a bare framework 404; a leaf reached
    // without its injected principal would give a 500. Neither is this.
    let (status, body) = request(&router, "GET", path, Some(&bearer)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({ "error": "agent not found" })
    );

    // Route checksum: the availability path carries exactly one method. An
    // authenticated POST clears the perimeter and then meets 405 — one route, one
    // handler entry, no second mount.
    let (status, body) = request(&router, "POST", path, Some(&bearer)).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{body}");
}

/// The composition's availability route consumes the SAME live attachment the
/// production daemon WebSocket populates. `a-online` and `a-offline` are equally
/// visible (creator arm) and equally reachable (live, unrevoked hosts); they
/// differ ONLY in that a real daemon attaches `h-online` over the mounted
/// `HOST_WEBSOCKET_PATH` through the production admission→attachment path — no
/// test-only attach hook or fake registry. The route reads `online` for the
/// attached agent and `offline` for the other through the one
/// `DaemonDeliveryController` the composition root shares between the daemon
/// runtime and the availability leaf. The leaf's own fake-attachment component
/// tests prove the reachability RULE; only this proves the MOUNT reaches the real
/// registry the transport mutates — swapping the mounted arg for an
/// always-detached adapter, or a SECOND controller registry that the WebSocket
/// never touches, turns the `online` assertion RED.
#[tokio::test]
async fn the_composition_availability_reads_the_live_daemon_attachment() {
    let fixture = fixture(true).await;
    seed_human(&fixture.path).await;
    seed_reachable_agents(&fixture.path).await;
    let app = prepare(&config(fixture.path.clone())).await.unwrap();
    // A clone shares the served router's Arc-backed daemon registry, so a GET on
    // this handle observes the live attachment made against the TCP server below.
    let router = app.router.clone();
    let bearer = format!("Bearer {TOKEN}");

    // Baseline: before any daemon attaches, BOTH visible/reachable agents read
    // offline — the shared controller holds no attachment for either host. This
    // is the state the live attach must move, so a later `online` cannot be a
    // fixture that was online all along.
    for agent in ["a-online", "a-offline"] {
        let uri = format!("/api/agents/{agent}/availability");
        let (status, body) = request(&router, "GET", &uri, Some(&bearer)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["availability"],
            "offline",
            "{agent} must be offline before any daemon attaches"
        );
    }

    // Serve over real TCP so a daemon can dial the mounted host WebSocket.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let running = tokio::spawn(serve(listener, app, async move {
        let _ = shutdown_rx.await;
        Ok::<(), std::io::Error>(())
    }));

    // Drive the REAL admission→attachment path: a daemon authenticates with the
    // seeded host credential, announces `ready`, and answers the runtime-status
    // command the server issues — the same handshake a production daemon runs.
    let mut ws_request = format!("ws://{address}{HOST_WEBSOCKET_PATH}")
        .into_client_request()
        .unwrap();
    ws_request.headers_mut().insert(
        tokio_tungstenite::tungstenite::http::header::AUTHORIZATION,
        format!("Bearer {HOST_TOKEN}").parse().unwrap(),
    );
    let (mut socket, _) = connect_async(ws_request).await.unwrap();
    socket
        .send(ClientMessage::Text(
            serde_json::json!({
                "direction": "daemon_to_server",
                "type": "event",
                "kind": "ready",
                "protocol_version": OUTBOUND_PROTOCOL_VERSION,
                "daemon_version": "test",
                "daemon_id": "daemon-cp2",
                "instance_id": "instance-cp2",
                "host_descriptor": { "name": "CP2 host", "kind": "remote" },
                "capabilities": [],
                "daemon_pid": 7,
                "started_at": 100,
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
    let command = next_daemon_json(&mut socket).await;
    assert_eq!(command["kind"], "runtime_status", "{command}");
    socket
        .send(ClientMessage::Text(
            serde_json::json!({
                "direction": "daemon_to_server",
                "type": "command_response",
                "id": command["id"],
                "kind": command["kind"],
                "result": { "runtimes": [] },
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

    // The attach + hydrate lands asynchronously; poll the SHARED availability GET
    // until it observes the live attachment (bounded, so broken wiring fails
    // instead of hanging).
    let mut online = false;
    for _ in 0..200 {
        let (status, body) = request(
            &router,
            "GET",
            "/api/agents/a-online/availability",
            Some(&bearer),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        if serde_json::from_str::<serde_json::Value>(&body).unwrap()["availability"] == "online" {
            online = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        online,
        "an agent whose host attached over the mounted daemon WebSocket must read online — \
             proving the availability route's port IS the composition's live DaemonDeliveryController"
    );

    // Positive control through the SAME controller instance: the equally visible,
    // equally reachable agent whose host never attached stays offline. An
    // always-detached adapter would (wrongly) return this for BOTH; here only the
    // unattached host reads it.
    let (status, body) = request(
        &router,
        "GET",
        "/api/agents/a-offline/availability",
        Some(&bearer),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap()["availability"],
        "offline",
        "an agent on a live host with no attachment must read offline through the same controller"
    );

    socket.close(None).await.unwrap();
    shutdown_tx.send(()).unwrap();
    running.await.unwrap().unwrap();
}

/// The composition mounts `GET /api/agents/{id}` (agent-detail) behind the same
/// perimeter, wired to the real SQLite agent-detail cold read. Proves the
/// composition obligation: the anonymous perimeter refusal, an authenticated
/// anti-enumeration 404 for an absent agent, a one-method route checksum, AND — an
/// admitted creator reading a real seeded row projected off the live SQLite
/// adapter (id/name/runtime/host, plus the `host_label` derivation off the host
/// LEFT JOIN). The projection and stage-flag matrix stay with the leaf's own
/// tests; this proves the mounted leaf reads the composition's real adapter.
#[tokio::test]
async fn the_composition_serves_agent_detail_behind_the_perimeter() {
    let fixture = fixture(true).await;
    seed_human(&fixture.path).await;
    seed_reachable_agents(&fixture.path).await;
    let app = prepare(&config(fixture.path.clone())).await.unwrap();
    let router = app.router;
    let bearer = format!("Bearer {TOKEN}");
    // `AGENT_DETAIL_PATH` is a `{id}` template; drive a concrete, unseeded id.
    let path = "/api/agents/agent-absent";

    let (status, body) = request(&router, "GET", path, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body, r#"{"error":"unauthorized"}"#);

    let (status, body) = request(&router, "GET", path, Some(&bearer)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({ "error": "agent not found" })
    );

    let (status, body) = request(&router, "POST", path, Some(&bearer)).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{body}");

    // Admitted creator (`user-1` created `a-online`): the mounted leaf projects the
    // real seeded row off the live SQLite adapter, including the `host_label`
    // derivation off the host LEFT JOIN (`host-online`, the host's name).
    let (status, body) = request(&router, "GET", "/api/agents/a-online", Some(&bearer)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let agent = &serde_json::from_str::<serde_json::Value>(&body).unwrap()["agent"];
    assert_eq!(agent["id"], "a-online");
    assert_eq!(agent["name"], "agent-online");
    assert_eq!(agent["runtime"], "qoder");
    assert_eq!(agent["host_id"], "h-online");
    assert_eq!(agent["host_label"], "host-online");
    assert_eq!(agent["created_by_user_id"], "user-1");
}

/// The composition mounts `GET /internal/agents/{aid}/tasks` (the agent task-LIST)
/// behind the runtime-credential perimeter, NOT the human session one. The route's
/// full vertical — real minted credential resolving to the agent, the union board
/// scope, the candidate fetch and the wire projection, and the port-untouched
/// anonymous refusal — is proven over the real perimeter seam by the backend-server
/// `task_list_mount` gate; minting a runtime credential is internal to `prepare`, so
/// what THIS composition test owns is the remaining, composition-only obligation:
/// that `prepare` actually placed the route in the served `agent_products` chain.
///
/// The discriminator is `route_layer`: the runtime-credential middleware is attached
/// with `route_layer`, so it runs ONLY for a request that matches a mounted route. A
/// bad bearer on the tasks path therefore reaches the perimeter and is refused `401`
/// — while the SAME bad bearer on a sibling path that is NOT a route falls straight
/// through to the `404` fallback, the perimeter never consulted. The `401`/`404`
/// split is the exact mounted-versus-absent signal: were the route left unmounted,
/// the tasks path would `404` like its unrouted sibling.
#[tokio::test]
async fn the_composition_mounts_the_agent_task_list_behind_the_runtime_perimeter() {
    let fixture = fixture(true).await;
    let app = prepare(&config(fixture.path.clone())).await.unwrap();
    let router = app.router;
    let bad_bearer = "Bearer unknown-runtime-token";

    // The tasks path IS a route: the runtime perimeter runs and refuses the bad
    // bearer `401`, never a `404` — the route is mounted behind the agent perimeter.
    let (status, body) = request(
        &router,
        "GET",
        "/internal/agents/agent-1/tasks",
        Some(bad_bearer),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body, r#"{"error":"unauthorized"}"#);

    // A sibling path under `/internal/agents/{aid}` that is NOT a route: the
    // `route_layer` perimeter is skipped and the request falls to the `404`
    // fallback, proving the `401` above is the task route's presence, not the
    // perimeter blanket-refusing every internal path.
    let (status, _) = request(
        &router,
        "GET",
        "/internal/agents/agent-1/not-a-mounted-route",
        Some(bad_bearer),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// The composition mounts `POST /internal/agents/{aid}/stale-message-resolution` behind
/// the runtime-credential perimeter, in the SAME `agent_products` chain as the agent
/// task-list. The route's full vertical — real minted credential resolving to the agent,
/// the real one-snapshot admission, the real `send_messages` capability, the port-
/// untouched refusals, and the production observer's single record — is proven over the
/// real perimeter seam by the backend-server `stale_message_resolution_mount` gate; what
/// THIS composition test owns is that `prepare` actually placed the route in the served
/// chain, wired to the SQLite adapter answering the core admission port and to the
/// production observer.
///
/// The discriminator is the same `route_layer`: a bad bearer on the stale path reaches
/// the perimeter and is refused `401` (the route is mounted), while the SAME bad bearer on
/// a sibling path that is NOT a route falls straight through to the `404` fallback. The
/// `401`/`404` split is the exact mounted-versus-absent signal.
#[tokio::test]
async fn the_composition_mounts_stale_message_resolution_behind_the_runtime_perimeter() {
    let fixture = fixture(true).await;
    let app = prepare(&config(fixture.path.clone())).await.unwrap();
    let router = app.router;
    let bad_bearer = "Bearer unknown-runtime-token";

    // The stale-resolution path IS a route: the runtime perimeter runs and refuses the
    // bad bearer `401`, never a `404` — the route is mounted behind the agent perimeter.
    let (status, body) = request(
        &router,
        "POST",
        "/internal/agents/agent-1/stale-message-resolution",
        Some(bad_bearer),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body, r#"{"error":"unauthorized"}"#);

    // A sibling path under `/internal/agents/{aid}` that is NOT a route: the `route_layer`
    // perimeter is skipped and the request falls to the `404` fallback, proving the `401`
    // above is the stale route's presence, not the perimeter blanket-refusing.
    let (status, _) = request(
        &router,
        "POST",
        "/internal/agents/agent-1/not-a-mounted-route",
        Some(bad_bearer),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Gate 1 of the CP2 convergence closure — the composed root's COMPLETE positive
/// mounted table. It enumerates every `(method, path)` the bound-listener finalizer
/// mounts across
/// ALL lanes (already-served + #2193 + #2194 + #2196 + #2195 + #2198 + #2218 + the
/// #2219 CP4 batch-B custody doors + the #2223 CP3 workspace-metadata / human-`me`
/// doors + the #2223 CP4 directory/avatar doors + the #2223 CP5 host residual doors +
/// the #2234 CP1 host-management doors + the #2234 CP2 public-agent write doors + the
/// #2234 CP3 public agent-DELETE door + the #2257 residual wave) as
/// one hundred and fifty entries — one hundred and twelve behind the human
/// session perimeter (including #2218's authenticated `/api/runtimes`, the three #2219
/// host custody/credential doors, the four #2223 CP5 host residual doors: detail,
/// revoke, connect-token rotate and delete, the six #2234 CP1 host-management doors:
/// update `PATCH /api/hosts/{id}`, disconnect, proxy-config GET+PUT, runtime-catalog
/// and status, the four #2223 CP3 workspace-metadata /
/// detail doors, the `me` pair, and the four #2223 CP4 directory/avatar doors), three
/// behind the runtime
/// credential perimeter, the two authenticated WebSockets, and seven ANONYMOUS faces
/// (#2218's three root faces, #2219's allowlisted `connect-token/resolve`, and the
/// #2223 CP3 signup/login/logout trio mounted ahead of the perimeter) — and
/// asserts each is PRESENT: a guarded route anon → its
/// perimeter's `401`, a WebSocket → its upgrade surface, an anonymous face → its
/// handler's own status (never the perimeter's bodiless `401`), and never the `404`
/// fallback. Removing any mounted route's
/// `.merge(...)` from `prepare()` flips its entry to `404` and reddens this gate.
/// The tables are named consts with per-table `assert_eq!(len)` (114 human / 28 runtime
/// / 2 WebSocket / 7 anonymous / 1 connect-info-gated) so a mis-added or mis-deleted
/// row reddens the count directly.
///
/// This is a PRESENCE ledger, not a closed universe: black-box probing cannot
/// prove "no further route was silently added" (axum exposes no route
/// introspection), so this gate makes no such claim; the length asserts guard the
/// table rows against mis-add/mis-delete, they make no closed-universe claim (the live
/// canonical-registry reconciliation below owns that property). A new registrar mounted into
/// `prepare()` MUST add its route here (and, if in the #2195 lane, move it from
/// residual to landed in gate 2). Each door's per-path method surface and adapter
/// wiring stays pinned by that route's own mount/unit test — the composed `.layer`
/// perimeter answers `401` before method dispatch, so per-method surface is not
/// observable here. F3 (#2217): the six thread-follow/saved methods collapse onto four
/// distinct paths (`.../follow` and `/api/messages/{id}/save` each carry POST+DELETE),
/// so anonymous presence here witnesses only those four path-guards; the six methods'
/// routing truth rides the authenticated per-method gate
/// `the_message_state_lane_dispatches_every_method_behind_the_perimeter` below.
#[tokio::test]
async fn the_composition_mounts_every_declared_route() {
    let fixture = fixture(true).await;
    let app = prepare(&config(fixture.path.clone())).await.unwrap();
    let router = app.router;
    let unauthorized = r#"{"error":"unauthorized"}"#;

    // The 114 human-perimeter product doors. Anonymous (no bearer) → the human
    // session perimeter's `401` envelope, never a `404`: each path IS mounted and
    // guarded. (`.layer` runs the perimeter for any matched human route.)
    const HUMAN_PERIMETER_DOORS: [(&str, &str); 114] = [
        ("POST", "/api/hosts"),
        // #2219 CP4 host-custody batch B — the three perimeter-guarded custody doors.
        // `GET /api/hosts` shares the path node with the invite POST above, so its anon
        // 401 only witnesses the shared path-guard; the authenticated two-arm gate below
        // bears that GET actually dispatches to the list handler while POST still reaches
        // the invite. `connect-token/resolve` is NOT here — it is the allowlisted
        // anonymous host door, asserted with the other anon faces below. The other two
        // carry their own distinct paths.
        ("GET", "/api/hosts"),
        ("POST", "/api/hosts/claim-or-create"),
        ("POST", "/api/hosts/host-1/rotate-token"),
        ("POST", "/api/channels/conv000000000001/messages"),
        ("GET", "/api/direct-pairs"),
        // Direct-pair initiation write door, Wave 3A checkpoint A6 (#2203). Distinct
        // method+path from the direct-pair read above; anon POST → the perimeter 401.
        ("POST", "/api/agents/agent-1/direct-messages"),
        ("POST", "/api/users/user-2/direct-messages"),
        ("GET", "/api/workspaces"),
        ("POST", "/api/workspaces/workspace0000001/members"),
        ("GET", "/api/agents"),
        ("GET", "/api/agents/agent-1"),
        ("GET", "/api/agents/agent-1/presence"),
        ("GET", "/api/agents/agent-1/availability"),
        ("GET", "/api/presence"),
        // #2194 channel directory/detail lane, Wave 3A checkpoint A1.
        ("GET", "/api/workspaces/workspace0000001/channels"),
        ("GET", "/api/workspaces/workspace0000001/channels/archived"),
        ("GET", "/api/archived-channels"),
        ("GET", "/api/channels/conv000000000001"),
        ("GET", "/api/channels/conv000000000001/agents"),
        // Runtime/activity projection lane, Wave 3A checkpoint A2.
        ("GET", "/api/channels/conv000000000001/agent-runtime"),
        ("GET", "/api/channels/conv000000000001/agent-run-states"),
        (
            "GET",
            "/api/conversations/conv000000000001/agents/agent-1/activity",
        ),
        ("GET", "/api/activity"),
        // Message navigation/search lane, Wave 3A checkpoint A3. The messages GET
        // shares its path with the send POST above; anon → 401 here proves only that
        // the path is guarded, not which method entered — the authenticated
        // double-method gate below distinguishes GET-window from POST-send.
        ("GET", "/api/conversations/conv000000000001/ref"),
        ("GET", "/api/conversations/conv000000000001/unread"),
        ("GET", "/api/conversations/latest"),
        ("GET", "/api/conversations/conv000000000001/threads"),
        ("GET", "/api/channels/conv000000000001/messages"),
        ("GET", "/api/search"),
        // Unread/attention read lane, Wave 3A checkpoint A4.
        ("GET", "/api/workspaces/workspace0000001/message-unread"),
        ("GET", "/api/message-unread-summary"),
        ("GET", "/api/channels/conv000000000001/message-unread"),
        ("GET", "/api/attention/badges"),
        ("GET", "/api/attention/modes"),
        // Host-resources read lane, Wave 3A checkpoint A5 (#2198).
        ("GET", "/api/hosts/host-1/resources"),
        // #2194 viewer-owned message-state lane (#2217). Six methods on four paths:
        // `.../follow` and `/api/messages/{id}/save` each carry POST+DELETE, so anon
        // presence witnesses only the four path-guards — the authenticated per-method
        // gate below bears which of the six methods actually dispatch.
        (
            "GET",
            "/api/channels/conv000000000001/threads/msg000000000000001/followers",
        ),
        (
            "POST",
            "/api/channels/conv000000000001/threads/msg000000000000001/follow",
        ),
        (
            "DELETE",
            "/api/channels/conv000000000001/threads/msg000000000000001/follow",
        ),
        ("GET", "/api/saved"),
        ("POST", "/api/messages/msg000000000000001/save"),
        ("DELETE", "/api/messages/msg000000000000001/save"),
        // #2218/#task:2321: `/api/runtimes`, the local runtime-catalog face that is
        // authenticated (under
        // the product's `/api/*` bearer gate with no allowlist entry). Anon → 401.
        ("GET", "/api/runtimes"),
        // #2223 CP5: the four host residual doors, mounted behind the human perimeter.
        // `GET` and `DELETE /api/hosts/{id}` share one path node (detail + teardown,
        // disjoint methods); anon presence here witnesses only the shared path-guard,
        // and the authenticated GET→detail / DELETE→teardown / unregistered→405 dispatch
        // is borne by `the_api_hosts_id_path_dispatches_get_detail_patch_update_and_delete_teardown`.
        ("GET", "/api/hosts/host-1"),
        ("DELETE", "/api/hosts/host-1"),
        ("POST", "/api/hosts/host-1/revoke"),
        ("POST", "/api/hosts/host-1/connect-token/rotate"),
        // #2234 CP1: the six host-management doors, mounted behind the human perimeter.
        // `PATCH /api/hosts/{id}` accumulates onto the CP5 GET+DELETE node (disjoint
        // method); anon presence here witnesses the shared path-guard, and the
        // authenticated PATCH→update dispatch is borne by
        // `the_api_hosts_id_path_dispatches_get_detail_patch_update_and_delete_teardown`.
        ("PATCH", "/api/hosts/host-1"),
        ("POST", "/api/hosts/host-1/disconnect"),
        ("GET", "/api/hosts/host-1/proxy-config"),
        ("PUT", "/api/hosts/host-1/proxy-config"),
        ("GET", "/api/hosts/host-1/runtime-catalog"),
        ("GET", "/api/hosts/host-1/status"),
        // #2223 CP3: the #2193 CP1 workspace-metadata / detail lane, behind the human
        // perimeter. `GET`/`PATCH /api/workspaces/{id}` share one node (detail read +
        // metadata write, disjoint methods), `{id}/archive` is a distinct sub-path, and
        // the static `name-availability` GET coexists with the `{id}` param node
        // (matchit static-over-param). Anon on each → the perimeter's bodiless 401; the
        // authenticated behaviour of these routes is borne by their own leaf suites.
        ("GET", "/api/workspaces/workspace0000001"),
        ("PATCH", "/api/workspaces/workspace0000001"),
        ("PATCH", "/api/workspaces/workspace0000001/archive"),
        ("GET", "/api/workspaces/name-availability"),
        // #2223 CP3: the #2193 CP2 human-identity `me` pair. `human_auth` self-wraps
        // `require_human_session` around ONLY this pair; the signup/login/logout trio is
        // anonymous and asserted with the other anon faces below, so anon here → 401.
        ("GET", "/api/me"),
        ("PATCH", "/api/me"),
        // #2223 CP4: the #2222 human-directory + avatar family. All four self-wrap the
        // same perimeter. `/api/me/avatar` is a child of `/api/me`, and
        // `/api/users/{id}/avatar` shares no node with the `/api/users` directory read.
        ("GET", "/api/users"),
        ("POST", "/api/me/avatar"),
        ("DELETE", "/api/me/avatar"),
        ("GET", "/api/users/user-1/avatar"),
        // #2234 CP2: the five #2225 public-agent write doors, behind the same human
        // perimeter as the agent read routes above. `PATCH /api/agents/{id}`
        // accumulates onto the GET-detail `{id}` node (line above); the three avatar
        // methods share one `/api/agents/{id}/avatar` node (GET serve + POST upload +
        // DELETE clear, disjoint methods); the static `name-availability` GET coexists
        // with the `{id}` param node (matchit static-over-param). Anon on each → the
        // perimeter's bodiless 401; the authenticated per-method dispatch (GET stays
        // detail, PATCH commits, the avatar round-trip, static route wins) is borne by
        // `the_composition_agent_write_faces_share_one_profile_authority_and_avatar_root`.
        ("GET", "/api/agents/name-availability"),
        ("PATCH", "/api/agents/agent-1"),
        ("GET", "/api/agents/agent-1/avatar"),
        ("POST", "/api/agents/agent-1/avatar"),
        ("DELETE", "/api/agents/agent-1/avatar"),
        // #2234 CP3: the public agent-DELETE door, accumulating onto the same
        // GET-detail/PATCH `{id}` node (disjoint method). Anon → the perimeter 401;
        // the authenticated lifecycle (route→shared resumer `try_now`, boot recover,
        // daemon-attach `on_host_ready`, shutdown close) is borne by
        // `the_composition_agent_deletion_resumer_drives_the_full_production_lifecycle`.
        ("DELETE", "/api/agents/agent-1"),
        // #2234 CP4: the two agent runtime write faces on their own path nodes (distinct
        // from the `{id}` node above). Anon → the perimeter 401; the authenticated reach
        // through the shared production `ProductionRuntimeControl` (and the switch saga's
        // durable commit) is borne by
        // `the_composition_agent_runtime_control_faces_reach_the_shared_production_control`.
        ("POST", "/api/agents/agent-1/clear-session"),
        ("PUT", "/api/agents/agent-1/runtime"),
        // #2234 CP5: the public agent-stop door on its own path node (distinct from the
        // `{id}` and runtime nodes above). Anon → the perimeter 401; the authenticated
        // enumerate-all-seats reach through the exact per-seat stop transport is borne by
        // `the_composition_agent_stop_face_reaches_every_seat_through_the_exact_transport`.
        ("POST", "/api/agents/agent-1/stop"),
        // #2234 CP6: the public agent-create door, accumulating onto the `/api/agents`
        // node the GET list already owns (disjoint method). Anon → the perimeter 401
        // (the `.layer` answers before method dispatch, so anon presence witnesses only
        // the shared path-guard); the authenticated GET-list-vs-POST-create dispatch and
        // the full reserve→materialize→provision-graph→201 reach are borne by
        // `the_composition_agent_create_face_drives_the_full_saga_and_post_commit_effects`.
        ("POST", "/api/agents"),
        // #2257 convergence: the ten residual-wave human-perimeter doors. Anon on each
        // → the perimeter's bodiless 401 (the door IS mounted and guarded); the
        // authenticated behavior of each is borne by its owning leaf's suite.
        ("GET", "/api/agents/agent-1/runtime-state"),
        ("GET", "/api/agents/agent-1/chats"),
        ("GET", "/api/agents/agent-1/chats/conv-1/messages"),
        ("GET", "/api/agents/agent-1/chats/conv-1/attachments/att-1"),
        ("GET", "/api/agents/agent-1/schedules"),
        ("POST", "/api/agents/agent-1/schedules/sched-1/cancel"),
        ("GET", "/api/agents/agent-1/workspace/tree"),
        ("GET", "/api/agents/agent-1/workspace/file"),
        ("GET", "/api/agents/agent-1/profile/mcp"),
        ("POST", "/api/agents/agent-1/profile/mcp/probe"),
        // #2338: conversation-control mutations/debug (the four directory/detail
        // reads already appear above), workspace/conversation membership, and the
        // ten public task faces.
        ("POST", "/api/attention/observe"),
        (
            "PUT",
            "/api/conversations/conv000000000001/attention-preference",
        ),
        (
            "PUT",
            "/api/workspaces/workspace0000001/attention-preference",
        ),
        ("POST", "/api/workspaces/workspace0000001/channels"),
        ("PATCH", "/api/channels/conv000000000001/archive"),
        ("PATCH", "/api/channels/conv000000000001"),
        ("GET", "/api/conversations/conv000000000001/log-debug"),
        ("POST", "/api/conversations/conv000000000001/log-debug"),
        ("GET", "/api/workspaces/workspace0000001/members"),
        (
            "PATCH",
            "/api/workspaces/workspace0000001/members/users/user-1/role",
        ),
        ("DELETE", "/api/workspaces/workspace0000001/members/agent-1"),
        (
            "DELETE",
            "/api/workspaces/workspace0000001/members/users/user-1",
        ),
        ("POST", "/api/workspaces/workspace0000001/leave"),
        (
            "GET",
            "/api/workspaces/workspace0000001/agents/agent-1/capabilities",
        ),
        (
            "PUT",
            "/api/workspaces/workspace0000001/agents/agent-1/capabilities",
        ),
        ("GET", "/api/conversations/conv000000000001/members"),
        ("POST", "/api/conversations/conv000000000001/members"),
        ("POST", "/api/channels/conv000000000001/join"),
        ("POST", "/api/channels/conv000000000001/leave"),
        (
            "DELETE",
            "/api/conversations/conv000000000001/members/agents/agent-1",
        ),
        (
            "DELETE",
            "/api/conversations/conv000000000001/members/users/user-1",
        ),
        ("POST", "/api/tasks"),
        ("POST", "/api/tasks/task-1/claim"),
        ("POST", "/api/tasks/task-1/unclaim"),
        ("POST", "/api/tasks/task-1/status"),
        ("POST", "/api/tasks/task-1/edit"),
        ("GET", "/api/tasks/board/column"),
        ("GET", "/api/tasks/board/counts"),
        ("GET", "/api/tasks/board/facets"),
        ("GET", "/api/tasks/by-number"),
        ("GET", "/api/tasks/task-1"),
    ];
    assert_eq!(HUMAN_PERIMETER_DOORS.len(), 114);
    for (method, path) in HUMAN_PERIMETER_DOORS {
        let (status, body) = request(&router, method, path, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {path}: {body}");
        assert_eq!(body, unauthorized, "{method} {path}");
    }

    // The 35 runtime-perimeter internal doors. Anonymous → the runtime credential
    // perimeter's `401` on the matched route, never `404`. The original three (#2226-era
    // send/tasks/stale), the #2234 CP7a self-scope profile family (`/profile` GET+POST,
    // `/avatar` POST+DELETE, `/peer-profile` GET), the #2234 CP7b profile-MCP family
    // (`/profile/mcp` GET+POST + `/profile/mcp/probe` POST), and the #2250/#2257
    // cli-trace door, and #2262's six reaction/attachment doors. All eighteen ride the
    // ONE `require_runtime_credential` layer, so anon
    // on any listed method is the same bodiless 401.
    assert_eq!(CLI_INVOKE_PRODUCTION_PERIMETER.len(), 35);
    for (method, path_template) in CLI_INVOKE_PRODUCTION_PERIMETER {
        let path = path_template.replace("{aid}", "agent-1");
        let method = (*method).as_str();
        let (status, body) = request(&router, method, &path, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {path}: {body}");
        assert_eq!(body, unauthorized, "{method} {path}");
    }

    // The two authenticated WebSockets are mounted: a plain (non-upgrade) GET is
    // refused by the upgrade/auth surface, never falling to the `404` fallback. This
    // table is path-only — the upgrade surface, not a method router, answers here.
    const AUTHENTICATED_WEBSOCKETS: [&str; 2] = [EVENT_WEBSOCKET_PATH, HOST_WEBSOCKET_PATH];
    assert_eq!(AUTHENTICATED_WEBSOCKETS.len(), 2);
    for path in AUTHENTICATED_WEBSOCKETS {
        let (status, _) = request(&router, "GET", path, None).await;
        assert_ne!(status, StatusCode::NOT_FOUND, "{path} must be mounted");
    }

    // #2218 + #2219: the FOUR ANONYMOUS faces are mounted ahead of every perimeter, so
    // an anonymous probe reaches the handler — never the 404 an absent path gives. The
    // three #2218 root faces self-distinguish by status (200/200/503, none of which the
    // perimeter or the 404 fallback returns): discovery and liveness always answer 200;
    // `/readyz` on this prepared-but-not-served router answers 503 because
    // `listen_ready` is a serving-boundary fact that composition does not mark
    // (migrations are marked, but serving has not begun).
    for (method, path, expected) in [
        ("GET", "/api/server/info", StatusCode::OK),
        ("GET", "/api/health", StatusCode::OK),
        ("GET", "/readyz", StatusCode::SERVICE_UNAVAILABLE),
    ] {
        let (status, body) = request(&router, method, path, None).await;
        assert_eq!(status, expected, "anonymous {method} {path}: {body}");
    }

    // #2219: `GET /api/hosts/connect-token/resolve` is the fourth anonymous face — the
    // one host door the product allowlists past the bearer gate. An anonymous probe
    // reaches the handler, which finds no connect-token bearer and answers its OWN
    // `401 {"error":"unauthorized","error_code":"unauthorized"}`. Its status (401)
    // coincides with a guarded door's, so status alone would not prove it landed
    // anonymously — the `error_code` key is the discriminant: the human perimeter emits
    // `{"error":"unauthorized"}` with NO `error_code`. Mounting this behind the
    // perimeter instead would drop the `error_code` (perimeter body) and reject a real
    // connect-token bearer, reddening this assert.
    let (status, body) = request(&router, "GET", "/api/hosts/connect-token/resolve", None).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "anonymous resolve: {body}"
    );
    assert_eq!(
        body, r#"{"error":"unauthorized","error_code":"unauthorized"}"#,
        "anonymous resolve reaches its own handler, not the perimeter"
    );

    // #2255/#2257: `POST /auth/embedded-bootstrap` is mounted at the top level, but its
    // `LoopbackPeer` extractor fails closed to 404 when a request carries no
    // `ConnectInfo<SocketAddr>` (the in-process helper injects none), so an anon POST
    // cannot witness the mount. An UNLISTED method on the same node witnesses it
    // instead: 405 (the node exists) rather than 404 (no node), because method routing
    // precedes the extractor. Deleting the merge flips this to 404.
    let (status, _) = request(&router, "GET", "/auth/embedded-bootstrap", None).await;
    assert_eq!(
        status,
        StatusCode::METHOD_NOT_ALLOWED,
        "embedded-bootstrap must be MOUNTED (an unlisted method → 405, not 404)"
    );
    let (status, _) = request(&router, "POST", "/auth/embedded-bootstrap", None).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "missing ConnectInfo must fail closed"
    );

    // #2223 CP3: the #2193 CP2 signup/login/logout trio is mounted ahead of the human
    // perimeter (login must be reachable before any session exists). They share ONE
    // public-router construction, so witnessing signup + login reach their own handlers
    // proves the whole trio sits ahead of the perimeter — had the trio been wrapped,
    // anonymous signup/login would carry the perimeter's bodiless 401 and redden below.
    //
    // signup + login: an anonymous POST reaches the handler's own admission (a 4xx on
    // the empty probe body), never the 404 an absent path gives and never the
    // perimeter's `{"error":"unauthorized"}`. The exact status is that handler's
    // business; the invariant is presence + reached-anonymously.
    for (method, path) in [("POST", "/auth/signup"), ("POST", "/auth/login")] {
        let (status, body) = request(&router, method, path, None).await;
        assert_ne!(
            status,
            StatusCode::NOT_FOUND,
            "anonymous {method} {path} must be mounted: {body}"
        );
        assert_ne!(
            body, unauthorized,
            "anonymous {method} {path} must reach its own handler, not the perimeter"
        );
    }

    // logout is the third trio member, but UNLIKE signup/login its own no-bearer answer
    // is `401 {"error":"unauthorized"}` — byte-identical to the perimeter's — because it
    // reads the raw bearer itself (living outside the perimeter so it can revoke
    // unknown/expired bearers) and refuses a missing one with the same shape. Body
    // therefore cannot witness anonymity here; presence is the `401 != 404` (an absent
    // path would fall to the 404 fallback proven below), and its ahead-of-perimeter
    // placement rides the shared public-router construction the signup/login asserts
    // above already pin.
    let (status, _) = request(&router, "POST", "/auth/logout", None).await;
    assert_ne!(
        status,
        StatusCode::NOT_FOUND,
        "anonymous POST /auth/logout must be mounted"
    );

    // Mounted-vs-absent discriminator: sibling paths that are NOT routes fall to
    // the top `404` fallback (no perimeter blanket-refusing), proving the `401`s
    // above are route presence and not a catch-all.
    for (method, path) in [
        ("GET", "/api/nonexistent"),
        ("GET", "/api/agents/agent-1/not-a-route"),
        ("GET", "/api/workspaces/workspace0000001/not-a-route"),
        ("POST", "/internal/agents/agent-1/not-a-route"),
        ("GET", "/internal/nonexistent"),
    ] {
        let (status, _) = request(&router, method, path, None).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "{method} {path} must be absent"
        );
    }
}

/// The frozen product registry independently guards route/method compatibility.
/// Protected routes must reject anonymous requests and let genuine credentials
/// cross the perimeter. Domain-specific admission and effects have their own suites.
#[tokio::test]
async fn registered_product_routes_enforce_auth_and_dispatch_declared_methods() {
    let rows: Vec<String> = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../rust/test-fixtures/routes/endpoint-registry-snapshot.json"
    )))
    .unwrap();
    let registry: std::collections::BTreeSet<String> = rows.into_iter().collect();
    assert!(
        !registry.is_empty(),
        "the independent product route fixture is empty"
    );

    let fixture = fixture(true).await;
    seed_human(&fixture.path).await;
    seed_runtime_agent(&fixture.path, "registry-probe").await;
    let app = prepare(&config(fixture.path.clone())).await.unwrap();
    let runtime_bearer = mint_runtime_bearer(&app, "registry-probe").await;
    let human_bearer = format!("Bearer {TOKEN}");
    for identity in registry {
        let (method, template) = identity.split_once(' ').unwrap();
        let path = template
            .split('/')
            .map(|segment| {
                if segment.starts_with(':') {
                    "registry-probe"
                } else {
                    segment
                }
            })
            .collect::<Vec<_>>()
            .join("/");
        if identity == "POST /auth/embedded-bootstrap" {
            // This POST hides absence of ConnectInfo behind404; its method node
            // is independently witnessed by the unsupported GET.
            assert_eq!(
                request(&app.router, "GET", &path, None).await.0,
                StatusCode::METHOD_NOT_ALLOWED,
                "{identity}"
            );
            continue;
        }
        let public = matches!(
            identity.as_str(),
            "GET /api/server/info"
                | "GET /api/health"
                | "GET /readyz"
                | "GET /api/ws/events"
                | "GET /api/hosts/connect"
                | "GET /api/hosts/connect-token/resolve"
                | "POST /auth/signup"
                | "POST /auth/login"
                | "POST /auth/logout"
        );
        let (status, body) = request(&app.router, method, &path, None).await;
        if public {
            assert_ne!(status, StatusCode::NOT_FOUND, "{identity}: {body}");
            assert_ne!(status, StatusCode::METHOD_NOT_ALLOWED, "{identity}: {body}");
            // Public method presence needs no credential. In particular, a real
            // logout would revoke the bearer required by later protected probes.
            continue;
        } else {
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{identity}: {body}");
        }
        let bearer = if path.starts_with("/internal/") {
            &runtime_bearer
        } else {
            &human_bearer
        };
        let (status, body) = request(&app.router, method, &path, Some(bearer)).await;
        assert_ne!(status, StatusCode::METHOD_NOT_ALLOWED, "{identity}: {body}");
        assert_ne!(status, StatusCode::UNAUTHORIZED, "{identity}: {body}");
    }
}

/// Production schedule-driver proof: `prepare` owns a live driver, which claims a
/// due row, writes the immutable schedule-fire message and owner inbox delivery through
/// the real SQLite adapter, then settles the occurrence. A no-op committed effect, a
/// test-only driver, or an unstarted driver leaves the row pending and this gate red.
#[tokio::test]
async fn agent_scoped_dm_control_admits_only_the_creator_endpoint_and_keeps_one_seat() {
    let fixture = fixture(true).await;
    seed_human(&fixture.path).await;
    seed_runtime_agent(&fixture.path, "dm-control").await;
    let pool = SqlitePoolOptions::new()
        .connect(fixture.path.to_str().unwrap())
        .await
        .unwrap();
    sqlx::raw_sql(
        "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES('dm-other','dm-other','Other',1,1); \
         INSERT INTO agents(id,name,runtime,host_id,created_at,updated_at,created_by_user_id,materialization_status,session_scope) VALUES \
         ('dm-conv','dm-conv','qoder','h-registry-probe',1,1,'user-1','active','conversation'), \
         ('dm-human-deleted','dm-human-deleted','qoder','h-registry-probe',1,1,'user-1','active','agent'), \
         ('dm-foreign','dm-foreign','qoder','h-registry-probe',1,1,'dm-other','active','agent'); \
         INSERT INTO conversations(id,title,placement_kind,roster_policy,endpoint_a_kind,endpoint_a_id,endpoint_b_kind,endpoint_b_id,status,visibility,created_at,updated_at) VALUES \
         ('dm-control-pair','DM','direct_pair','fixed','agent','dm-control','human','user-1','idle','private',1,1), \
         ('dm-foreign-pair','DM','direct_pair','fixed','agent','dm-foreign','human','user-1','idle','private',1,1), \
         ('dm-conv-pair','DM','direct_pair','fixed','agent','dm-conv','human','user-1','idle','private',1,1), \
         ('dm-human-deleted-pair','DM','direct_pair','fixed','agent','dm-human-deleted','human','user-1','idle','private',1,1), \
         ('dm-other-pair','DM','direct_pair','fixed','agent','dm-control','human','dm-other','idle','private',1,1);"
    ).execute(&pool).await.unwrap();
    pool.close().await;
    let app = prepare(&config(fixture.path.clone())).await.unwrap();
    let router = app.router.clone();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let running = tokio::spawn(serve(listener, app, async move {
        let _ = shutdown_rx.await;
        Ok::<(), std::io::Error>(())
    }));
    let daemon = AttachedDeliveryDaemon::connect(address).await;
    let bearer = format!("Bearer {TOKEN}");
    let (unauthenticated, _) = request(
        &router,
        "POST",
        "/api/channels/dm-control-pair/agents/dm-control/start",
        None,
    )
    .await;
    assert_eq!(unauthenticated, StatusCode::UNAUTHORIZED);
    for (pair, agent, expected) in [
        ("dm-control-pair", "dm-foreign", StatusCode::BAD_REQUEST),
        ("dm-conv-pair", "dm-conv", StatusCode::BAD_REQUEST),
        ("dm-foreign-pair", "dm-foreign", StatusCode::FORBIDDEN),
        ("dm-other-pair", "dm-control", StatusCode::FORBIDDEN),
        ("dm-control-pair", "missing", StatusCode::NOT_FOUND),
    ] {
        for operation in ["start", "stop"] {
            let (status, body) = request(
                &router,
                "POST",
                &format!("/api/channels/{pair}/agents/{agent}/{operation}"),
                Some(&bearer),
            )
            .await;
            assert_eq!(status, expected, "{pair}/{agent}/{operation}: {body}");
        }
    }
    assert!(
        daemon.seen.lock().unwrap().is_empty(),
        "refusals must not dispatch runtime commands"
    );
    let (pending, body) = request(
        &router,
        "POST",
        "/api/channels/dm-control-pair/agents/dm-control/start",
        Some(&bearer),
    )
    .await;
    assert_eq!(pending, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("host_pending_attach"));
    assert!(daemon.seen.lock().unwrap().is_empty());
    let activation = SqlitePoolOptions::new()
        .connect(fixture.path.to_str().unwrap())
        .await
        .unwrap();
    sqlx::query("UPDATE hosts SET activated_at=1,finalized_at=1 WHERE id='h-registry-probe'")
        .execute(&activation)
        .await
        .unwrap();
    activation.close().await;
    let (status, body) = request(
        &router,
        "POST",
        "/api/channels/dm-control-pair/agents/dm-control/start",
        Some(&bearer),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let starts = daemon.seen.lock().unwrap().clone();
    let start = starts
        .iter()
        .find(|c| c["kind"] == "runtime_start")
        .expect("real start dispatch");
    assert_eq!(
        start["params"]["options"]["runtime_scope"],
        serde_json::json!({"kind":"agent","agent_id":"dm-control"})
    );
    assert!(
        start["params"]["runtime_generation"]
            .as_i64()
            .is_some_and(|g| g > 0)
    );
    assert_eq!(start["params"]["conversation_id"], "dm-control-pair");
    let (state_status, state_body) = request(
        &router,
        "GET",
        "/api/agents/dm-control/runtime-state",
        Some(&bearer),
    )
    .await;
    assert_eq!(state_status, StatusCode::OK, "{state_body}");
    let state: serde_json::Value = serde_json::from_str(&state_body).unwrap();
    assert_eq!(state["seats"].as_array().unwrap().len(), 1);
    assert_eq!(
        state["seats"][0]["scope"],
        serde_json::json!({"kind":"agent"})
    );
    assert_eq!(
        state["seats"][0]["generation"],
        start["params"]["runtime_generation"]
    );
    let (status, body) = request(
        &router,
        "POST",
        "/api/channels/dm-control-pair/agents/dm-control/stop",
        Some(&bearer),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let commands = daemon.seen.lock().unwrap().clone();
    let stop = commands
        .iter()
        .find(|c| c["kind"] == "runtime_stop")
        .expect("real stop dispatch");
    assert_eq!(
        stop["params"]["runtime_scope"],
        start["params"]["options"]["runtime_scope"]
    );
    assert_eq!(
        stop["params"]["runtime_generation"],
        start["params"]["runtime_generation"]
    );
    let pool = SqlitePoolOptions::new()
        .connect(fixture.path.to_str().unwrap())
        .await
        .unwrap();
    let seats: (i64, i64) = sqlx::query_as("SELECT COUNT(*),COUNT(conversation_id) FROM agent_runtime_slots WHERE agent_id='dm-control'").fetch_one(&pool).await.unwrap();
    assert_eq!(seats, (1, 0), "DM uses the sole global seat");
    sqlx::query("UPDATE conversations SET endpoint_a_deleted_at=1 WHERE id='dm-control-pair'")
        .execute(&pool)
        .await
        .unwrap();
    for operation in ["start", "stop"] {
        let (status, _) = request(
            &router,
            "POST",
            &format!("/api/channels/dm-control-pair/agents/dm-control/{operation}"),
            Some(&bearer),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
    assert_eq!(
        daemon.seen.lock().unwrap().len(),
        commands.len(),
        "tombstoned endpoint has zero control effects"
    );
    sqlx::query(
        "UPDATE conversations SET endpoint_b_deleted_at=1 WHERE id='dm-human-deleted-pair'",
    )
    .execute(&pool)
    .await
    .unwrap();
    for operation in ["start", "stop"] {
        let (status, _) = request(
            &router,
            "POST",
            &format!("/api/channels/dm-human-deleted-pair/agents/dm-human-deleted/{operation}"),
            Some(&bearer),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
    assert_eq!(daemon.seen.lock().unwrap().len(), commands.len());
    pool.close().await;
    daemon.close().await;
    shutdown_tx.send(()).unwrap();
    running.await.unwrap().unwrap();
}

#[tokio::test]
async fn the_composed_schedule_driver_delivers_and_settles_a_due_occurrence() {
    let fixture = fixture(true).await;
    seed_human(&fixture.path).await;
    seed_runtime_agent(&fixture.path, "schedule-owner").await;
    seed_workspace_conversation(&fixture.path).await;
    let pool = SqlitePoolOptions::new()
        .connect(fixture.path.to_str().unwrap())
        .await
        .unwrap();
    // This test also exercises the strict message reader; use the current status domain.
    sqlx::query("UPDATE conversations SET status='idle' WHERE id=?")
        .bind(CONVERSATION_ID)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO agents(id,name,runtime,host_id,created_at,updated_at,created_by_user_id,materialization_status,session_scope) \
         VALUES('schedule-peer','schedule-peer','qoder','h-registry-probe',1,1,'user-1','active','agent')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversation_agents(conversation_id,agent_id,joined_at) VALUES(?,?,1)",
    )
    .bind(CONVERSATION_ID)
    .bind("schedule-owner")
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO workspace_members(workspace_id,agent_id,joined_at) VALUES(?,?,1)")
        .bind(WORKSPACE_ID)
        .bind("schedule-owner")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO workspace_agent_capabilities(workspace_id,agent_id,capability) VALUES(?,?,'manage_schedules')")
        .bind(WORKSPACE_ID)
        .bind("schedule-owner")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO conversation_agents(conversation_id,agent_id,joined_at) VALUES(?,?,1)",
    )
    .bind(CONVERSATION_ID)
    .bind("schedule-peer")
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO workspace_members(workspace_id,agent_id,joined_at) VALUES(?,?,1)")
        .bind(WORKSPACE_ID)
        .bind("schedule-peer")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO workspace_agent_capabilities(workspace_id,agent_id,capability) VALUES(?,?,'send_messages')")
        .bind(WORKSPACE_ID)
        .bind("schedule-peer")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    let app = prepare(&config(fixture.path.clone())).await.unwrap();
    let runtime_credentials = app.test_runtime_credentials.clone();
    let router = app.router.clone();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let running = tokio::spawn(serve(listener, app, async move {
        let _ = shutdown_rx.await;
        Ok::<(), std::io::Error>(())
    }));
    let delivery_daemon = AttachedDeliveryDaemon::connect(address).await;
    let runtime_bearer =
        mint_runtime_bearer_from(&runtime_credentials, "schedule-owner", CONVERSATION_ID).await;
    let pool = SqlitePoolOptions::new()
        .connect(fixture.path.to_str().unwrap())
        .await
        .unwrap();
    let fire_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
        + 31_000;
    let (rearm_status, rearm_body) = json_request_with_body(
        &router,
        "POST",
        "/internal/agents/schedule-owner/schedules",
        Some(&runtime_bearer),
        Body::from(format!(
            r##"{{"target_conversation":"#conv:{CONVERSATION_ID}","title":"Rearm","message":"later","spec":{{"kind":"once","fire_at":{fire_at}}}}}"##
        )),
    )
    .await;
    assert_eq!(rearm_status, StatusCode::OK, "{rearm_body}");
    let created: serde_json::Value = serde_json::from_str(&rearm_body).unwrap();
    let schedule_id = created["schedule"]["id"].as_str().unwrap().to_owned();
    assert_eq!(created["schedule"]["owner_agent_id"], "schedule-owner");
    assert_eq!(
        created["schedule"]["target_conversation_id"],
        CONVERSATION_ID
    );
    assert_eq!(created["schedule"]["next_fire_at"], fire_at);
    delivery_daemon.close().await;
    shutdown_tx.send(()).unwrap();
    running.await.unwrap().unwrap();
    drop(router);
    drop(runtime_credentials);
    pool.close().await;
    // Pools close asynchronously on drop. Wait for a real exclusive SQLite
    // transaction to prove the previous app released its database connections.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let mut probe = sqlx::SqliteConnection::connect(fixture.path.to_str().unwrap())
                .await
                .unwrap();
            sqlx::query("PRAGMA busy_timeout=0")
                .execute(&mut probe)
                .await
                .unwrap();
            sqlx::query("PRAGMA locking_mode=EXCLUSIVE")
                .execute(&mut probe)
                .await
                .unwrap();
            if sqlx::query("BEGIN EXCLUSIVE")
                .execute(&mut probe)
                .await
                .is_ok()
            {
                sqlx::query("ROLLBACK").execute(&mut probe).await.unwrap();
                probe.close().await.unwrap();
                break;
            }
            probe.close().await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("previous app retained database connections");
    let app = prepare(&config(fixture.path.clone())).await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let running = tokio::spawn(serve(listener, app, async move {
        let _ = shutdown_rx.await;
        Ok::<(), std::io::Error>(())
    }));
    let delivery_daemon = AttachedDeliveryDaemon::connect(address).await;
    let pool = SqlitePoolOptions::new()
        .connect(fixture.path.to_str().unwrap())
        .await
        .unwrap();
    let (status, messages, owner_inboxes, peer_inboxes): (String, i64, i64, i64) = tokio::time::timeout(
        std::time::Duration::from_secs(45),
        async {
            loop {
                let status: String =
                    sqlx::query_scalar("SELECT status FROM schedules WHERE id=?")
                        .bind(&schedule_id)
                        .fetch_one(&pool)
                        .await
                        .unwrap();
                let messages: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM messages WHERE conversation_id=? \
                     AND json_extract(metadata,'$.subtype')='schedule_fire' \
                     AND json_extract(metadata,'$.schedule_id')=?",
                )
                .bind(CONVERSATION_ID)
                .bind(&schedule_id)
                .fetch_one(&pool)
                .await
                .unwrap();
                let inboxes: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM conversation_inboxes ci JOIN messages m ON m.id=ci.message_id \
                     WHERE ci.conversation_id=? AND ci.agent_id='schedule-owner' \
                     AND json_extract(m.metadata,'$.schedule_id')=?",
                )
                .bind(CONVERSATION_ID)
                .bind(&schedule_id)
                .fetch_one(&pool)
                .await
                .unwrap();
                let peer_inboxes: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM conversation_inboxes ci JOIN messages m ON m.id=ci.message_id \
                     WHERE ci.conversation_id=? AND ci.agent_id='schedule-peer' \
                     AND json_extract(m.metadata,'$.schedule_id')=?",
                )
                .bind(CONVERSATION_ID)
                .bind(&schedule_id)
                .fetch_one(&pool)
                .await
                .unwrap();
                if status != "pending" && status != "firing" {
                    break (status, messages, inboxes, peer_inboxes);
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        },
    )
    .await
    .expect("production schedule driver did not settle the due occurrence");
    assert_eq!(status, "done");
    assert_eq!(
        messages, 1,
        "one claimed occurrence writes one wake message"
    );
    assert_eq!(
        owner_inboxes, 1,
        "the wake is durably addressed only to its owner"
    );
    assert_eq!(
        peer_inboxes, 0,
        "advancing the message head must not address another target participant"
    );
    let head_before_send: Option<i64> = sqlx::query_scalar(
        "SELECT latest_seq FROM conversation_stream_heads WHERE conversation_id=? AND thread_root_id IS NULL",
    )
    .bind(CONVERSATION_ID)
    .fetch_optional(&pool)
    .await
    .unwrap();
    let wake_seq: i64 = sqlx::query_scalar(
        "SELECT seq FROM messages WHERE conversation_id=? \
         AND json_extract(metadata,'$.schedule_id')=?",
    )
    .bind(CONVERSATION_ID)
    .bind(&schedule_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(head_before_send, Some(wake_seq));
    {
        use silverhand_backend_core::conversation_messaging::messaging::collaboration::conversation_message_window::{
            ConversationMessageWindowReadPort, ConversationMessageWindowRequest,
            MessageWindowAnchor, MessageWindowStreamRequest,
        };
        use silverhand_backend_core::foundation::identity::{ConversationId, UserId};
        use silverhand_backend_sqlite::conversation_messaging::messaging::MessagingStore;
        use silverhand_backend_core::conversation_messaging::messaging::collaboration::{
            activity::observe::{ObservationPort, ObserveBatch, ObserveSource, StreamPosition},
            conversation_unread::ConversationUnreadReadPort,
        };
        let store = MessagingStore::open(&fixture.path).await.unwrap();
        let viewer = UserId::parse("user-1").unwrap();
        let conversation = ConversationId::parse(CONVERSATION_ID).unwrap();
        let request = ConversationMessageWindowRequest::new(
            MessageWindowStreamRequest::Mainline,
            200,
            MessageWindowAnchor::Latest,
        )
        .unwrap();
        let page = store
            .conversation_message_window()
            .read(&viewer, &conversation, request.clone())
            .await
            .unwrap()
            .unwrap()
            .bind(&viewer, &conversation, &request)
            .unwrap();
        assert_eq!(page.stream_head().get(), wake_seq);
        assert!(
            page.messages()
                .iter()
                .any(|row| row.projection().seq.get() == wake_seq)
        );
        let unread = store
            .conversation_unread()
            .read(&viewer, &conversation)
            .await
            .unwrap()
            .unwrap()
            .bind(&conversation)
            .unwrap();
        assert_eq!(
            unread.mainline().count(),
            0,
            "a system wake is not unread conversation discourse"
        );
        store
            .observation()
            .observe(
                viewer,
                ObserveBatch::new(vec![ObserveSource::Stream {
                    conversation,
                    thread_root: None,
                    position: StreamPosition::StreamHead,
                }])
                .unwrap(),
            )
            .await
            .unwrap()
            .unwrap();
        let read_seq: i64 = sqlx::query_scalar(
            "SELECT last_read_seq FROM conversation_reads WHERE user_id='user-1' AND conversation_id=? AND thread_root_id IS NULL",
        ).bind(CONVERSATION_ID).fetch_one(&pool).await.unwrap();
        assert_eq!(read_seq, wake_seq);
    }
    let persisted: (String, String, i64, i64) = sqlx::query_as(
        "SELECT owner_agent_id,target_conversation_id,next_fire_at,fired_count FROM schedules WHERE id=?",
    ).bind(&schedule_id).fetch_one(&pool).await.unwrap();
    assert_eq!(
        persisted,
        ("schedule-owner".into(), CONVERSATION_ID.into(), fire_at, 1)
    );
    let rescanner = silverhand_backend_sqlite::conversation_messaging::messaging::schedule::SqliteScheduleStore::open(&fixture.path).await.unwrap();
    assert!(
        rescanner
            .due_candidates(fire_at + 60_000, 200)
            .await
            .unwrap()
            .is_empty(),
        "a later driver sweep must not reselect the settled one-shot schedule"
    );
    delivery_daemon.wait_for_deliveries(1).await;
    let message_id: String = sqlx::query_scalar(
        "SELECT id FROM messages WHERE conversation_id=? \
         AND json_extract(metadata,'$.schedule_id')=?",
    )
    .bind(CONVERSATION_ID)
    .bind(&schedule_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        delivery_daemon
            .seen
            .lock()
            .unwrap()
            .iter()
            .any(|command| command["params"]["text"]
                .as_str()
                .is_some_and(|text| text.contains(&message_id))),
        "ordinary schedule wake must claim and dispatch its exact committed inbox message"
    );
    pool.close().await;
    delivery_daemon.close().await;
    shutdown_tx.send(()).unwrap();
    running.await.unwrap().unwrap();
}

#[tokio::test]
async fn dead_schedule_target_notifies_the_owner_home_and_keeps_original_failure() {
    let fixture = fixture(true).await;
    seed_human(&fixture.path).await;
    seed_runtime_agent(&fixture.path, "dead-target-owner").await;
    let pool = SqlitePoolOptions::new()
        .connect(fixture.path.to_str().unwrap())
        .await
        .unwrap();
    sqlx::raw_sql(
        "INSERT INTO agents(id,name,runtime,host_id,created_at,updated_at,created_by_user_id,materialization_status,session_scope) VALUES \
           ('no-home-owner','no-home-owner','qoder','h-registry-probe',1,1,'user-1','active','agent'); \
         INSERT INTO workspaces(id,name,owner_agent_id,created_at,updated_at) VALUES \
           ('home-dead-target','Home','dead-target-owner',1,1); \
         INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,status,visibility,created_at,updated_at) VALUES \
           ('homedmdeadtarget','home-dead-target','workspace','fixed','Onboarding','onboarding','idle','private',1,1); \
         INSERT INTO conversation_agents(conversation_id,agent_id,joined_at) VALUES \
           ('homedmdeadtarget','dead-target-owner',1); \
         INSERT INTO workspaces(id,name,name_key,owner_user_id,created_at,updated_at) VALUES \
           ('wsdeadrearm00001','Rearm Space','rearm space','user-1',1,1); \
         INSERT INTO workspace_users(workspace_id,user_id,joined_at,role) VALUES \
           ('wsdeadrearm00001','user-1',1,'owner'); \
         INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,status,visibility,created_at,updated_at) VALUES \
           ('convdeadrearm001','wsdeadrearm00001','workspace','open','Rearm','rearm','idle','private',1,1); \
         INSERT INTO conversation_agents(conversation_id,agent_id,joined_at) VALUES \
           ('convdeadrearm001','dead-target-owner',1); \
         INSERT INTO workspace_members(workspace_id,agent_id,joined_at) VALUES \
           ('wsdeadrearm00001','dead-target-owner',1); \
         INSERT INTO workspace_agent_capabilities(workspace_id,agent_id,capability) VALUES \
           ('wsdeadrearm00001','dead-target-owner','manage_schedules'); \
         INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,status,visibility,closed_at,created_at,updated_at) VALUES \
           ('closed-target','home-dead-target','workspace','open','Closed','closed','idle','private',2,1,1), \
           ('left-target','home-dead-target','workspace','open','Left','left','idle','private',NULL,1,1); \
         INSERT INTO schedules(id,owner_agent_id,target_conversation_id,title,message,spec_json,next_fire_at,status,fired_count,max_occurrences,until_at,consecutive_failure_count,created_at,updated_at) VALUES \
           ('schedule-dead','dead-target-owner','missing-target','Missing','wake now','{\"kind\":\"once\",\"fire_at\":1}',9000000000000000,'pending',0,NULL,NULL,0,1,1), \
           ('schedule-closed','dead-target-owner','closed-target','Closed','wake now','{\"kind\":\"once\",\"fire_at\":1}',9000000000000000,'pending',0,NULL,NULL,0,1,1), \
           ('schedule-left','dead-target-owner','left-target','Left','wake now','{\"kind\":\"once\",\"fire_at\":1}',9000000000000000,'pending',0,NULL,NULL,0,1,1), \
           ('schedule-no-home','no-home-owner','missing-target','No Home','wake now','{\"kind\":\"once\",\"fire_at\":1}',9000000000000000,'pending',0,NULL,NULL,0,1,1);",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    let app = prepare(&config(fixture.path.clone())).await.unwrap();
    let runtime_credentials = app.test_runtime_credentials.clone();
    let router = app.router.clone();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let running = tokio::spawn(serve(listener, app, async move {
        let _ = shutdown_rx.await;
        Ok::<(), std::io::Error>(())
    }));
    let delivery_daemon = AttachedDeliveryDaemon::connect(address).await;
    let runtime_bearer = mint_runtime_bearer_from(
        &runtime_credentials,
        "dead-target-owner",
        "convdeadrearm001",
    )
    .await;
    let pool = SqlitePoolOptions::new()
        .connect(fixture.path.to_str().unwrap())
        .await
        .unwrap();
    sqlx::query("UPDATE schedules SET next_fire_at=1 WHERE id LIKE 'schedule-%'")
        .execute(&pool)
        .await
        .unwrap();
    let fire_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
        + 60_000;
    let (rearm_status, rearm_body) = json_request_with_body(
        &router,
        "POST",
        "/internal/agents/dead-target-owner/schedules",
        Some(&runtime_bearer),
        Body::from(format!(
            r##"{{"target_conversation":"#conv:convdeadrearm001","title":"Rearm","message":"later","spec":{{"kind":"once","fire_at":{fire_at}}}}}"##
        )),
    )
    .await;
    assert_eq!(rearm_status, StatusCode::OK, "{rearm_body}");
    let (failures, notice_count, inbox_count): (String, i64, i64) = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        async {
            loop {
                let failed: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM schedules WHERE id IN ('schedule-dead','schedule-closed','schedule-left','schedule-no-home') AND status='failed'")
                    .fetch_one(&pool).await.unwrap();
                if failed == 4 {
                    let failure: String = sqlx::query_scalar("SELECT group_concat(failure_reason,'|') FROM (SELECT failure_reason FROM schedules WHERE id IN ('schedule-dead','schedule-closed','schedule-left') ORDER BY id)")
                        .fetch_one(&pool).await.unwrap();
                    let notices: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE conversation_id='homedmdeadtarget' AND content LIKE 'Schedule % could not fire:%'")
                        .fetch_one(&pool).await.unwrap();
                    let inboxes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM conversation_inboxes ci JOIN messages m ON m.id=ci.message_id WHERE ci.conversation_id='homedmdeadtarget' AND ci.agent_id='dead-target-owner' AND m.content LIKE 'Schedule % could not fire:%'")
                        .fetch_one(&pool).await.unwrap();
                    break (failure, notices, inboxes);
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        },
    ).await.expect("dead-target schedule did not settle");
    assert_eq!(
        failures,
        "target_conversation_unavailable: closed-target (closed)|target_conversation_unavailable: missing-target (missing)|target_conversation_unavailable: left-target (owner not participant)"
    );
    let list_bearer = mint_runtime_bearer_from(
        &runtime_credentials,
        "dead-target-owner",
        "convdeadrearm001",
    )
    .await;
    let (list_status, list_body) = json_request_with_body(
        &router,
        "POST",
        "/internal/agents/dead-target-owner/schedules/list",
        Some(&list_bearer),
        Body::from("{}"),
    )
    .await;
    assert_eq!(list_status, StatusCode::OK, "{list_body}");
    for reason in [
        "target_conversation_unavailable: missing-target (missing)",
        "target_conversation_unavailable: closed-target (closed)",
        "target_conversation_unavailable: left-target (owner not participant)",
    ] {
        assert!(
            list_body.contains(reason),
            "schedule list must expose the canonical settlement reason {reason}: {list_body}"
        );
    }
    assert_eq!(notice_count, 3);
    assert_eq!(inbox_count, 3);
    delivery_daemon.wait_for_deliveries(3).await;
    let notice_ids: Vec<String> = sqlx::query_scalar(
        "SELECT id FROM messages WHERE conversation_id='homedmdeadtarget' \
         AND content LIKE 'Schedule % could not fire:%' ORDER BY seq",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(notice_ids.len(), 3);
    for message_id in notice_ids {
        assert!(
            delivery_daemon
                .seen
                .lock()
                .unwrap()
                .iter()
                .any(|command| command["params"]["text"]
                    .as_str()
                    .is_some_and(|text| text.contains(&message_id))),
            "dead-target notice {message_id} must be claimed by a real runtime_deliver"
        );
    }
    pool.close().await;
    delivery_daemon.close().await;
    shutdown_tx.send(()).unwrap();
    running.await.unwrap().unwrap();
}

/// A committed task mutation crosses the composition-owned effect adapter with its
/// immutable post-image: one system-message frame followed by one `task_changed` on
/// the canonical conversation stream, plus one live-participant viewer fan-out. The
/// cursor-inert conversation-side stream must remain untouched.
#[tokio::test]
async fn the_composed_task_create_publishes_committed_payload_on_canonical_and_live_viewer_scopes()
{
    let fixture = fixture(true).await;
    seed_human(&fixture.path).await;
    seed_runtime_agent(&fixture.path, "task-recipient").await;
    seed_workspace_conversation(&fixture.path).await;
    let pool = SqlitePoolOptions::new()
        .connect(fixture.path.to_str().unwrap())
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO conversation_agents(conversation_id,agent_id,joined_at) VALUES(?,?,1)",
    )
    .bind(CONVERSATION_ID)
    .bind("task-recipient")
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
    let app = prepare(&config(fixture.path.clone())).await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let running = tokio::spawn(serve(listener, app, async move {
        let _ = shutdown_rx.await;
        Ok::<(), std::io::Error>(())
    }));
    let mut conversation =
        subscribe_event_scope(address, &format!("conversation:{CONVERSATION_ID}"), 0).await;
    let mut viewer = subscribe_own_viewer(address).await;
    let delivery_daemon = AttachedDeliveryDaemon::connect(address).await;
    let bearer = format!("Bearer {TOKEN}");

    let response = http_over_tcp(
        address,
        "POST",
        "/api/tasks",
        Some(&bearer),
        &format!(r#"{{"title":"Committed title","conversation_id":"{CONVERSATION_ID}"}}"#),
    )
    .await;
    assert!(
        response.starts_with("HTTP/1.1 201 Created\r\n"),
        "{response}"
    );
    let created = http_json_body(&response);
    let task_id = created["task"]["id"].as_str().unwrap();
    let task_number = created["task"]["task_number"].as_i64().unwrap();

    let message = next_socket_json(&mut conversation).await;
    assert_eq!(message["kind"], "message");
    assert_eq!(message["scope"], format!("conversation:{CONVERSATION_ID}"));
    let canonical = next_socket_json(&mut conversation).await;
    assert_eq!(canonical["kind"], "task_changed");
    assert_eq!(canonical["payload"]["task_id"], task_id);
    assert_eq!(canonical["payload"]["task_number"], task_number);
    assert_eq!(canonical["payload"]["title"], "Committed title");
    assert_eq!(canonical["payload"]["conversation_id"], CONVERSATION_ID);
    let viewer_frame = loop {
        let frame = next_socket_json(&mut viewer).await;
        if frame["kind"] == "task_changed" {
            break frame;
        }
    };
    assert_eq!(viewer_frame["kind"], "task_changed");
    assert_eq!(viewer_frame["payload"], canonical["payload"]);
    delivery_daemon.wait_for_deliveries(1).await;
    assert_eq!(
        fresh_scope_high_water(address, &format!("conversation:{CONVERSATION_ID}")).await,
        2,
        "one committed event message and one task_changed advance the canonical stream"
    );
    assert_eq!(
        fresh_scope_high_water(address, &format!("conversation-side:{CONVERSATION_ID}")).await,
        0,
        "task_changed never enters the cursor-inert side stream"
    );
    let pool = SqlitePoolOptions::new()
        .connect(fixture.path.to_str().unwrap())
        .await
        .unwrap();
    let addressed: Vec<String> = sqlx::query_scalar(
        "SELECT ci.message_id FROM conversation_inboxes ci JOIN messages m ON m.id=ci.message_id \
         WHERE ci.conversation_id=? AND ci.agent_id='task-recipient' \
         AND json_extract(m.metadata,'$.task_id')=?",
    )
    .bind(CONVERSATION_ID)
    .bind(task_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        addressed.len(),
        1,
        "the daemon wake is preceded by custody for the same committed task event"
    );
    assert!(
        delivery_daemon
            .seen
            .lock()
            .unwrap()
            .iter()
            .any(|command| command["params"]["text"]
                .as_str()
                .is_some_and(|text| text.contains(&addressed[0]))),
        "the real runtime_deliver must claim the custody row for the committed task message"
    );
    // Archive after a live delivery. Task history and UI events remain writable,
    // but the new status event must not create inbox custody or daemon commands.
    let commands_before = delivery_daemon.seen.lock().unwrap().len();
    sqlx::query("UPDATE conversations SET archived_at = 100 WHERE id = ?")
        .bind(CONVERSATION_ID)
        .execute(&pool)
        .await
        .unwrap();
    let response = http_over_tcp(
        address,
        "POST",
        &format!("/api/tasks/{task_id}/status"),
        Some(&bearer),
        r#"{"status":"done"}"#,
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    let message = next_socket_json(&mut conversation).await;
    assert_eq!(message["kind"], "message");
    let changed = next_socket_json(&mut conversation).await;
    assert_eq!(changed["kind"], "task_changed");
    assert_eq!(changed["payload"]["status"], "done");
    let status: String = sqlx::query_scalar("SELECT status FROM tasks WHERE id = ?")
        .bind(task_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "done");
    let inbox_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM conversation_inboxes WHERE conversation_id = ?")
            .bind(CONVERSATION_ID)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(inbox_count, 1);
    assert_eq!(delivery_daemon.seen.lock().unwrap().len(), commands_before);

    pool.close().await;

    conversation.close(None).await.unwrap();
    viewer.close(None).await.unwrap();
    delivery_daemon.close().await;
    shutdown_tx.send(()).unwrap();
    running.await.unwrap().unwrap();
}

/// Conversation-membership effects are wired to the same production hub as the
/// final app: a fresh public self-join emits exactly the roster invalidation on the
/// cursor-inert conversation-side scope, then the channel-list invalidation on the
/// workspace scope. No shaped/no-op aftermath can satisfy both subscriptions.
#[tokio::test]
async fn the_composed_membership_join_publishes_both_production_invalidations() {
    const PUBLIC_CHANNEL: &str = "conversation-public-join";
    let fixture = fixture(true).await;
    seed_human(&fixture.path).await;
    seed_workspace_conversation(&fixture.path).await;
    let pool = SqlitePoolOptions::new()
        .connect(fixture.path.to_str().unwrap())
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,\
         status,visibility,created_at,updated_at) VALUES(?,?,'workspace','open','Public',\
         'public','idle','public',1,1)",
    )
    .bind(PUBLIC_CHANNEL)
    .bind(WORKSPACE_ID)
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    let app = prepare(&config(fixture.path.clone())).await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let running = tokio::spawn(serve(listener, app, async move {
        let _ = shutdown_rx.await;
        Ok::<(), std::io::Error>(())
    }));
    let mut workspace =
        subscribe_event_scope(address, &format!("workspace:{WORKSPACE_ID}"), 0).await;
    let bearer = format!("Bearer {TOKEN}");
    let response = http_over_tcp(
        address,
        "POST",
        &format!("/api/channels/{PUBLIC_CHANNEL}/join"),
        Some(&bearer),
        "",
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");

    assert_eq!(
        fresh_scope_high_water(address, &format!("conversation-side:{PUBLIC_CHANNEL}")).await,
        1,
        "the fresh join publishes exactly one roster invalidation"
    );
    let mut side =
        subscribe_event_scope(address, &format!("conversation-side:{PUBLIC_CHANNEL}"), 1).await;
    let channels = next_socket_json(&mut workspace).await;
    assert_eq!(channels["kind"], "workspace_channels_changed");
    assert_eq!(channels["payload"]["workspace_id"], WORKSPACE_ID);
    assert_eq!(channels["payload"]["channel_id"], PUBLIC_CHANNEL);

    side.close(None).await.unwrap();
    workspace.close(None).await.unwrap();
    shutdown_tx.send(()).unwrap();
    running.await.unwrap().unwrap();
}

#[tokio::test]
async fn membership_notice_and_channel_rename_create_real_runtime_inbox_custody() {
    let fixture = fixture(true).await;
    seed_human(&fixture.path).await;
    seed_runtime_agent(&fixture.path, "lane-recipient").await;
    seed_channel_directory(&fixture.path).await;
    let pool = SqlitePoolOptions::new()
        .connect(fixture.path.to_str().unwrap())
        .await
        .unwrap();
    sqlx::query("INSERT INTO workspace_members(workspace_id,agent_id,joined_at) VALUES(?,?,1)")
        .bind(WORKSPACE_ID)
        .bind("lane-recipient")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    let app = prepare(&config(fixture.path.clone())).await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let running = tokio::spawn(serve(listener, app, async move {
        let _ = shutdown_rx.await;
        Ok::<(), std::io::Error>(())
    }));
    let delivery_daemon = AttachedDeliveryDaemon::connect(address).await;
    let bearer = format!("Bearer {TOKEN}");
    let response = http_over_tcp(
        address,
        "POST",
        &format!("/api/conversations/{CONVERSATION_ID}/members"),
        Some(&bearer),
        r#"{"kind":"agent","agent_id":"lane-recipient"}"#,
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    let response = http_over_tcp(
        address,
        "PATCH",
        &format!("/api/channels/{CONVERSATION_ID}"),
        Some(&bearer),
        r#"{"title":"Renamed"}"#,
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    delivery_daemon.wait_for_deliveries(2).await;

    let pool = SqlitePoolOptions::new()
        .connect(fixture.path.to_str().unwrap())
        .await
        .unwrap();
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT json_extract(m.metadata,'$.subtype'),ci.message_id \
         FROM conversation_inboxes ci JOIN messages m ON m.id=ci.message_id \
         WHERE ci.conversation_id=? AND ci.agent_id='lane-recipient' \
         AND json_extract(m.metadata,'$.subtype') IN ('conversation_member_added','rename') \
         ORDER BY m.seq",
    )
    .bind(CONVERSATION_ID)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        rows.len(),
        2,
        "membership and rename must each custody their own committed system message: {rows:?}"
    );
    assert_ne!(rows[0].1, rows[1].1);
    for (_, message) in &rows {
        assert!(
            delivery_daemon
                .seen
                .lock()
                .unwrap()
                .iter()
                .any(|command| command["params"]["text"]
                    .as_str()
                    .is_some_and(|text| text.contains(message))),
            "the real daemon dispatch must claim the exact membership/rename custody {message}"
        );
    }
    let (latest_message_seq, main_head): (i64, i64) = sqlx::query_as(
        "SELECT c.latest_message_seq,h.latest_seq FROM conversations c \
         JOIN conversation_stream_heads h ON h.conversation_id=c.id \
           AND h.thread_root_id IS NULL WHERE c.id=?",
    )
    .bind(CONVERSATION_ID)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(latest_message_seq, 2);
    assert_eq!(
        main_head, latest_message_seq,
        "membership and rename notices are both visible to mainline observe/window readers"
    );
    pool.close().await;
    delivery_daemon.close().await;
    shutdown_tx.send(()).unwrap();
    running.await.unwrap().unwrap();
}

#[tokio::test]
async fn membership_notice_head_failure_rolls_back_member_message_and_head() {
    let fixture = fixture(true).await;
    seed_human(&fixture.path).await;
    seed_runtime_agent(&fixture.path, "rollback-recipient").await;
    seed_channel_directory(&fixture.path).await;
    let pool = SqlitePoolOptions::new()
        .connect(fixture.path.to_str().unwrap())
        .await
        .unwrap();
    sqlx::query("INSERT INTO workspace_members(workspace_id,agent_id,joined_at) VALUES(?,?,1)")
        .bind(WORKSPACE_ID)
        .bind("rollback-recipient")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "CREATE TRIGGER reject_membership_head BEFORE INSERT ON conversation_stream_heads \
         BEGIN SELECT RAISE(ABORT,'head rejected'); END",
    )
    .execute(&pool)
    .await
    .unwrap();

    let app = prepare(&config(fixture.path.clone())).await.unwrap();
    let bearer = format!("Bearer {TOKEN}");
    let (status, _) = json_request_with_body(
        &app.router,
        "POST",
        &format!("/api/conversations/{CONVERSATION_ID}/members"),
        Some(&bearer),
        Body::from(r#"{"kind":"agent","agent_id":"rollback-recipient"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    let (member, message, head): (i64, i64, i64) = sqlx::query_as(
        "SELECT \
           (SELECT COUNT(*) FROM conversation_agents WHERE conversation_id=? AND agent_id='rollback-recipient'), \
           (SELECT COUNT(*) FROM messages WHERE conversation_id=?), \
           (SELECT COUNT(*) FROM conversation_stream_heads WHERE conversation_id=?)",
    )
    .bind(CONVERSATION_ID)
    .bind(CONVERSATION_ID)
    .bind(CONVERSATION_ID)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((member, message, head), (0, 0, 0));
    pool.close().await;
    app.close_connections().await;
}

/// Seed one message authored by `user-1` in `CONVERSATION_ID` (a valid thread root:
/// `thread_root_id` is NULL). Enough for the viewer to save it and to follow its
/// thread through the composed root. Requires `seed_workspace_conversation` first (it
/// establishes the conversation and the viewer's participation).
async fn seed_message(path: &Path, message_id: &str) {
    let pool = SqlitePoolOptions::new()
        .connect(path.to_str().unwrap())
        .await
        .unwrap();
    // `seed_workspace_conversation` stores `status = 'open'`, which the message-send
    // path never decodes but the saved/thread-follow authorization does. Normalize to
    // a published status so the conversation-participation snapshot decodes.
    sqlx::query("UPDATE conversations SET status = 'idle' WHERE id = ?")
        .bind(CONVERSATION_ID)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
            "INSERT INTO messages (id, conversation_id, from_kind, from_user_id, content, seq, created_at, thread_root_id, author_id, author_name, author_display_name) VALUES (?, ?, 'user', 'user-1', 'hello', 1, 1, NULL, 'user-1', 'user-1', 'User One')",
        )
        .bind(message_id)
        .bind(CONVERSATION_ID)
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}

/// #2217 F3 — per-METHOD dispatch truth for the two shared-path pairs, which the
/// presence ledger above cannot bear. The perimeter `.layer` answers `401` before
/// method dispatch, so anonymous presence witnesses only the four path-guards; a
/// dropped sibling method (e.g. `DELETE .../follow`) would stay green there under
/// `assert_eq!(len)`. Here an AUTHENTICATED probe passes the perimeter and reaches the
/// path's `MethodRouter`: each of the six mounted methods reaches handler-level
/// behavior (never `405`), and an UNREGISTERED method (`PATCH` — follow/save are
/// POST/DELETE toggles with no PATCH semantics and no lane plans to mount it) returns
/// `405`, proving method dispatch actually happened and no catch-all swallows the path.
#[tokio::test]
async fn the_message_state_lane_dispatches_every_method_behind_the_perimeter() {
    let fixture = fixture(true).await;
    seed_human(&fixture.path).await;
    let app = prepare(&config(fixture.path.clone())).await.unwrap();
    let router = app.router;
    let bearer = format!("Bearer {TOKEN}");

    // Clearly-absent coordinates: every id parses (non-empty) but resolves to nothing,
    // so each registered method reaches its handler's domain refusal — never `405`.
    let followers = "/api/channels/conv-absent/threads/root-absent/followers";
    let follow = "/api/channels/conv-absent/threads/root-absent/follow";
    let save = "/api/messages/msg-absent/save";
    let saved_list = "/api/saved";

    // The six REGISTERED methods → handler behavior (asserted exact), never `405`.
    for (method, path, expected) in [
        ("GET", saved_list, StatusCode::OK),
        ("POST", save, StatusCode::NOT_FOUND),
        ("DELETE", save, StatusCode::NOT_FOUND),
        ("GET", followers, StatusCode::NOT_FOUND),
        ("POST", follow, StatusCode::NOT_FOUND),
        ("DELETE", follow, StatusCode::NOT_FOUND),
    ] {
        let (status, body) = request(&router, method, path, Some(&bearer)).await;
        assert_ne!(
            status,
            StatusCode::METHOD_NOT_ALLOWED,
            "{method} {path} must dispatch to a handler, not 405: {body}"
        );
        assert_eq!(status, expected, "{method} {path}: {body}");
    }

    // The UNREGISTERED method on each shared path → `405`. Falsifier: a dropped
    // registered sibling would ALSO be 405 (caught above); a catch-all accepting any
    // method would NOT be 405 here.
    for path in [follow, save] {
        let (status, _) = request(&router, "PATCH", path, Some(&bearer)).await;
        assert_eq!(
            status,
            StatusCode::METHOD_NOT_ALLOWED,
            "PATCH {path} is unregistered and must be 405"
        );
    }
}

/// #2217 cut 2 — DUAL proof that `prepare()` wires the REAL hub as the saved
/// aftermath (not a no-op / second hub) AND addresses the WRITER's own viewer scope.
/// A committed save/unsave publishes exactly one `saved_changed` frame to
/// `viewer:<writer>`, projected by the saved context's sole wire authority. A no-op
/// aftermath would leave the subscription silent; a wrong scope would land the frame
/// off `viewer:user-1`; a re-typed shape would drift the payload. The frame is read off
/// a real event-WS subscription opened BEFORE the mutation, so `seq` is deterministic.
#[tokio::test]
async fn the_composition_publishes_saved_changed_to_the_writer_on_the_real_hub() {
    let fixture = fixture(true).await;
    seed_human(&fixture.path).await;
    seed_workspace_conversation(&fixture.path).await;
    seed_message(&fixture.path, "message00000001").await;
    let app = prepare(&config(fixture.path.clone())).await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let running = tokio::spawn(serve(listener, app, async move {
        let _ = shutdown_rx.await;
        Ok::<(), std::io::Error>(())
    }));
    let bearer = format!("Bearer {TOKEN}");
    let mut socket = subscribe_own_viewer(address).await;

    // Save → one `saved_changed { action: "save" }` on the writer's own viewer scope.
    let save = http_over_tcp(
        address,
        "POST",
        "/api/messages/message00000001/save",
        Some(&bearer),
        "",
    )
    .await;
    assert!(save.starts_with("HTTP/1.1 201 Created\r\n"), "{save}");
    let saved = next_socket_json(&mut socket).await;
    assert_eq!(saved["scope"], "viewer:user-1");
    assert_eq!(saved["kind"], "saved_changed");
    assert_eq!(saved["seq"], 1);
    assert_eq!(
        saved["payload"],
        serde_json::json!({"message_id":"message00000001","action":"save"})
    );

    // Unsave → one `saved_changed { action: "unsave" }` on the SAME scope.
    let unsave = http_over_tcp(
        address,
        "DELETE",
        "/api/messages/message00000001/save",
        Some(&bearer),
        "",
    )
    .await;
    assert!(
        unsave.starts_with("HTTP/1.1 204 No Content\r\n"),
        "{unsave}"
    );
    let unsaved = next_socket_json(&mut socket).await;
    assert_eq!(unsaved["scope"], "viewer:user-1");
    assert_eq!(unsaved["kind"], "saved_changed");
    assert_eq!(unsaved["seq"], 2);
    assert_eq!(
        unsaved["payload"],
        serde_json::json!({"message_id":"message00000001","action":"unsave"})
    );

    socket.close(None).await.unwrap();
    shutdown_tx.send(()).unwrap();
    running.await.unwrap().unwrap();
}

/// #2845: a composed send publishes `conversation_latest_changed` to the sender's own
/// viewer scope, and the snapshot route serves the same summary.
#[tokio::test]
async fn the_composition_publishes_the_latest_summary_and_serves_its_snapshot() {
    let fixture = fixture(true).await;
    seed_human(&fixture.path).await;
    seed_workspace_conversation(&fixture.path).await;
    let app = prepare(&config(fixture.path.clone())).await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let running = tokio::spawn(serve(listener, app, async move {
        let _ = shutdown_rx.await;
        Ok::<(), std::io::Error>(())
    }));
    let bearer = format!("Bearer {TOKEN}");
    let mut socket = subscribe_own_viewer(address).await;

    let sent = http_over_tcp(
        address,
        "POST",
        &format!("/api/channels/{CONVERSATION_ID}/messages"),
        Some(&bearer),
        r#"{"content":"latest  hello"}"#,
    )
    .await;
    assert!(sent.starts_with("HTTP/1.1 201 Created\r\n"), "{sent}");

    let latest = loop {
        let frame = next_socket_json(&mut socket).await;
        if frame["kind"] == "conversation_latest_changed" {
            break frame;
        }
    };
    assert_eq!(latest["scope"], "viewer:user-1");
    assert_eq!(latest["payload"]["conversation_id"], CONVERSATION_ID);
    assert_eq!(latest["payload"]["preview"], "latest hello");
    assert_eq!(latest["payload"]["thread_root_id"], serde_json::Value::Null);
    assert_eq!(latest["payload"]["sender"]["id"], "user-1");

    let snapshot = http_over_tcp(
        address,
        "GET",
        "/api/conversations/latest",
        Some(&bearer),
        "",
    )
    .await;
    assert!(snapshot.starts_with("HTTP/1.1 200 OK\r\n"), "{snapshot}");
    assert_eq!(
        http_json_body(&snapshot),
        serde_json::json!({ "items": [latest["payload"].clone()] })
    );

    socket.close(None).await.unwrap();
    shutdown_tx.send(()).unwrap();
    running.await.unwrap().unwrap();
}

/// #2217 cut 1 — proof that `prepare()` wires the thread-follow post-commit seam to
/// the REAL `HubUnreadPublisher` over the REAL `SqliteUnreadReadModel` (not a no-op /
/// second audience path). A committed follow republishes the conversation's unread
/// through the ONE unread publisher, which co-emits `unread_updated` +
/// `attention_badge_updated` to each seated viewer — here the follower on
/// `viewer:user-1`. A no-op aftermath would leave the subscription silent; a second,
/// unrelated hub would not reach this subscription. The audience and counts come from
/// the real read model, so this also proves the reuse (no duplicate calculation).
#[tokio::test]
async fn the_composition_republishes_thread_follow_unread_through_the_real_publisher() {
    let fixture = fixture(true).await;
    seed_human(&fixture.path).await;
    seed_workspace_conversation(&fixture.path).await;
    seed_message(&fixture.path, "message00000001").await;
    let app = prepare(&config(fixture.path.clone())).await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let running = tokio::spawn(serve(listener, app, async move {
        let _ = shutdown_rx.await;
        Ok::<(), std::io::Error>(())
    }));
    let bearer = format!("Bearer {TOKEN}");
    let mut socket = subscribe_own_viewer(address).await;

    // Follow the seeded thread root → the real publisher republishes this viewer's
    // unread as the `unread_updated` + `attention_badge_updated` co-emit.
    let follow = http_over_tcp(
        address,
        "POST",
        &format!("/api/channels/{CONVERSATION_ID}/threads/message00000001/follow"),
        Some(&bearer),
        "",
    )
    .await;
    assert!(follow.starts_with("HTTP/1.1 200 OK\r\n"), "{follow}");

    let unread = next_socket_json(&mut socket).await;
    assert_eq!(unread["scope"], "viewer:user-1");
    assert_eq!(unread["kind"], "unread_updated");
    assert_eq!(unread["seq"], 1);
    assert_eq!(unread["payload"]["conversation_id"], CONVERSATION_ID);
    assert_eq!(unread["payload"]["workspace_id"], WORKSPACE_ID);

    // The batch's second frame — proving it is the one unread publisher's co-emit, not
    // a bespoke single frame.
    let badge = next_socket_json(&mut socket).await;
    assert_eq!(badge["scope"], "viewer:user-1");
    assert_eq!(badge["kind"], "attention_badge_updated");
    assert_eq!(badge["seq"], 2);
    assert_eq!(badge["payload"]["workspace_id"], WORKSPACE_ID);

    socket.close(None).await.unwrap();
    shutdown_tx.send(()).unwrap();
    running.await.unwrap().unwrap();
}

/// Open the event WebSocket as `user-1` (the seeded `TOKEN`), authenticate, subscribe
/// to the caller's own viewer scope, and return the live socket positioned to read the
/// first post-subscription frame. `subscribed.seq` is asserted so the caller's frame
/// sequence numbers start from a known baseline.
async fn subscribe_own_viewer(
    address: std::net::SocketAddr,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    subscribe_event_scope(address, "viewer:self", 0).await
}

/// Open a real authenticated event socket and subscribe to one named scope. The
/// expected high-water makes callers explicit about any pre-existing frame history.
async fn subscribe_event_scope(
    address: std::net::SocketAddr,
    scope: &str,
    expected_seq: i64,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let mut request = format!("ws://{address}{EVENT_WEBSOCKET_PATH}")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert(ORIGIN, "http://localhost:5173".parse().unwrap());
    request.headers_mut().insert(
        COOKIE,
        format!("{EVENT_WS_SESSION_COOKIE}={TOKEN}")
            .parse()
            .unwrap(),
    );
    request
        .headers_mut()
        .insert(DESKTOP_API_EPOCH_HEADER, MINIMUM_CLIENT_API_EPOCH.into());
    let (mut socket, _) = connect_async(request).await.unwrap();
    assert_eq!(next_socket_json(&mut socket).await["type"], "authenticated");
    socket
        .send(ClientMessage::Text(
            serde_json::json!({ "type": "subscribe", "scope": scope })
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    let subscribed = next_socket_json(&mut socket).await;
    assert_eq!(subscribed["type"], "subscribed", "{subscribed}");
    assert_eq!(subscribed["seq"], expected_seq, "scope baseline: {scope}");
    socket
}

/// #2262: notification clear runs through the prepared production SQLite store and
/// the SAME hub the mounted event transport serves. The aftermath emits one thin
/// viewer-private refetch signal — no absolute badge frame and no message-scope frame.
#[tokio::test]
async fn mounted_notification_clear_emits_one_thin_refetch_on_the_real_hub() {
    let fixture = fixture(true).await;
    seed_human(&fixture.path).await;
    seed_workspace_conversation(&fixture.path).await;
    seed_message(&fixture.path, "message-notification").await;
    let pool = SqlitePoolOptions::new()
        .connect(fixture.path.to_str().unwrap())
        .await
        .unwrap();
    sqlx::query(
            "INSERT INTO notifications(id,kind,user_id,workspace_id,conversation_id,message_id,payload,created_at) \
             VALUES('notification-1','mention_self','user-1',?,?,?,'{}',10)",
        )
        .bind(WORKSPACE_ID)
        .bind(CONVERSATION_ID)
        .bind("message-notification")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    let app = prepare(&config(fixture.path.clone())).await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let running = tokio::spawn(serve(listener, app, async move {
        let _ = shutdown_rx.await;
        Ok::<(), std::io::Error>(())
    }));
    let bearer = format!("Bearer {TOKEN}");
    let mut socket = subscribe_own_viewer(address).await;
    let app_before = fresh_scope_high_water(address, "app").await;

    let response = http_over_tcp(
        address,
        "POST",
        "/api/notifications/read",
        Some(&bearer),
        r#"{"all":true}"#,
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert_eq!(http_json_body(&response), serde_json::json!({"marked": 1}));

    let frame = next_socket_json(&mut socket).await;
    assert_eq!(frame["scope"], "viewer:user-1");
    assert_eq!(frame["seq"], 1);
    assert_eq!(frame["kind"], "unread_updated");
    assert_eq!(frame["payload"], serde_json::json!({"marked": 1}));
    assert_eq!(fresh_scope_high_water(address, "viewer:self").await, 1);
    assert_eq!(
        fresh_scope_high_water(address, &format!("conversation:{CONVERSATION_ID}")).await,
        0,
        "notification clear must not impersonate a message-stream update"
    );
    assert_eq!(
        fresh_scope_high_water(address, "app").await,
        app_before,
        "notification clear must not publish an absolute app badge frame"
    );

    socket.close(None).await.unwrap();
    shutdown_tx.send(()).unwrap();
    running.await.unwrap().unwrap();
}

/// #2262: a mounted human reaction mutation commits through the production SQLite
/// reaction authority and publishes exactly one canonical message-stream delta on the
/// SAME hub. It must not leak onto the cursor-inert attachment side channel or any
/// viewer/app scope.
#[tokio::test]
async fn mounted_reaction_mutation_emits_one_canonical_delta_on_the_real_hub() {
    let fixture = fixture(true).await;
    seed_human(&fixture.path).await;
    seed_workspace_conversation(&fixture.path).await;
    seed_message(&fixture.path, "message-reaction").await;

    let app = prepare(&config(fixture.path.clone())).await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let running = tokio::spawn(serve(listener, app, async move {
        let _ = shutdown_rx.await;
        Ok::<(), std::io::Error>(())
    }));
    let bearer = format!("Bearer {TOKEN}");
    let message_scope = format!("conversation:{CONVERSATION_ID}");
    let mut socket = subscribe_event_scope(address, &message_scope, 0).await;
    let app_before = fresh_scope_high_water(address, "app").await;

    let response = http_over_tcp(
        address,
        "POST",
        "/api/messages/message-reaction/reactions",
        Some(&bearer),
        r#"{"emoji":"👍"}"#,
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert_eq!(
        http_json_body(&response),
        serde_json::json!({"changed": true, "emoji": "👍", "count": 1})
    );

    let frame = next_socket_json(&mut socket).await;
    assert_eq!(frame["scope"], message_scope);
    assert_eq!(frame["seq"], 1);
    assert_eq!(frame["kind"], "message_reaction_changed");
    assert_eq!(
        frame["payload"],
        serde_json::json!({
            "message_id": "message-reaction",
            "conversation_id": CONVERSATION_ID,
            "thread_root_id": null,
            "emoji": "👍",
            "op": "add",
            "reactor_kind": "user",
            "reactor_id": "user-1",
            "count": 1,
        })
    );
    assert_eq!(
        fresh_scope_high_water(address, &format!("conversation-side:{CONVERSATION_ID}")).await,
        0,
        "reaction deltas are canonical message events, never side-channel invalidations"
    );
    assert_eq!(fresh_scope_high_water(address, "viewer:self").await, 0);
    assert_eq!(fresh_scope_high_water(address, "app").await, app_before);

    socket.close(None).await.unwrap();
    shutdown_tx.send(()).unwrap();
    running.await.unwrap().unwrap();
}

/// #2262: public upload→update→download crosses the prepared production write/read
/// adapters and ONE `LocalBlobStore`. The committed update publishes exactly one
/// cursor-inert `attachment_changed` frame on the real conversation-side hub and no
/// canonical message/viewer/app frame.
#[tokio::test]
async fn mounted_attachment_update_is_cursor_inert_and_reuses_the_one_blob_root() {
    let fixture = fixture(true).await;
    seed_human(&fixture.path).await;
    seed_workspace_conversation(&fixture.path).await;
    seed_message(&fixture.path, "message-attachment-anchor").await;
    let app = prepare(&config(fixture.path.clone())).await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let running = tokio::spawn(serve(listener, app, async move {
        let _ = shutdown_rx.await;
        Ok::<(), std::io::Error>(())
    }));
    let bearer = format!("Bearer {TOKEN}");
    let boundary = "XBOUNDARY2262";
    let multipart = |bytes: &[u8]| {
        let mut body = Vec::new();
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            b"Content-Disposition: form-data; name=\"file\"; filename=\"note.txt\"\r\n",
        );
        body.extend_from_slice(b"Content-Type: text/plain\r\n\r\n");
        body.extend_from_slice(bytes);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        body
    };
    let content_type = format!("multipart/form-data; boundary={boundary}");

    let upload = http_over_tcp_typed(
        address,
        "POST",
        &format!("/api/channels/{CONVERSATION_ID}/attachments"),
        Some(&bearer),
        &content_type,
        &multipart(b"before"),
    )
    .await;
    let (upload_head, upload_body) = split_http_response(&upload);
    assert!(
        upload_head.starts_with("HTTP/1.1 201 Created\r\n"),
        "{upload_head}"
    );
    let upload_body: serde_json::Value = serde_json::from_slice(&upload_body).unwrap();
    let attachment = upload_body["id"].as_str().unwrap().to_owned();

    let side_scope = format!("conversation-side:{CONVERSATION_ID}");
    let mut socket = subscribe_event_scope(address, &side_scope, 0).await;
    let app_before = fresh_scope_high_water(address, "app").await;
    let update = http_over_tcp_typed(
        address,
        "PATCH",
        &format!("/api/attachments/{attachment}"),
        Some(&bearer),
        &content_type,
        &multipart(b"after"),
    )
    .await;
    let (update_head, update_body) = split_http_response(&update);
    assert!(
        update_head.starts_with("HTTP/1.1 200 OK\r\n"),
        "{update_head}"
    );
    let update_body: serde_json::Value = serde_json::from_slice(&update_body).unwrap();
    assert_eq!(update_body["content_revision"], 2);

    let frame = next_socket_json(&mut socket).await;
    assert_eq!(frame["scope"], side_scope);
    assert_eq!(frame["seq"], 1);
    assert_eq!(frame["kind"], "attachment_changed");
    assert_eq!(
        frame["payload"],
        serde_json::json!({"attachment_id": attachment, "content_revision": 2})
    );
    assert_eq!(
        fresh_scope_high_water(address, &format!("conversation:{CONVERSATION_ID}")).await,
        0,
        "attachment bytes never advance the message stream"
    );
    assert_eq!(fresh_scope_high_water(address, "viewer:self").await, 0);
    assert_eq!(fresh_scope_high_water(address, "app").await, app_before);

    let download = http_over_tcp_typed(
        address,
        "GET",
        &format!("/api/attachments/{attachment}"),
        Some(&bearer),
        "application/json",
        b"",
    )
    .await;
    let (download_head, download_body) = split_http_response(&download);
    assert!(
        download_head.starts_with("HTTP/1.1 200 OK\r\n"),
        "{download_head}"
    );
    assert_eq!(download_body, b"after");

    socket.close(None).await.unwrap();
    shutdown_tx.send(()).unwrap();
    running.await.unwrap().unwrap();
}

/// Mount, authentication and method dispatch for the notification/reaction/attachment
/// families. Anonymous401 proves node presence even when the authenticated handler
/// returns404 for an absent resource; unregistered methods must remain405.
#[tokio::test]
async fn notification_reaction_and_attachment_routes_enforce_auth_and_dispatch_methods() {
    use std::{
        convert::Infallible,
        future::Future,
        sync::{Arc, Mutex},
    };

    use silverhand_backend_core::{
        foundation::identity::{ConversationId, DaemonInstanceId},
        host_runtime::runtime_credential::RuntimeScope,
    };
    use silverhand_backend_server::host_runtime::runtime_credential::{
        RuntimeDispatchFailure, RuntimeStartHandoff, RuntimeStartTransport,
    };

    let fixture = fixture(true).await;
    seed_human(&fixture.path).await;
    let pool = SqlitePoolOptions::new()
        .connect(fixture.path.to_str().unwrap())
        .await
        .unwrap();
    for statement in [
        "INSERT INTO hosts (id, name, kind, created_at, user_id) VALUES \
             ('h-2262', 'host-2262', 'remote', 1, 'user-1')",
        "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, \
             created_by_user_id, materialization_status, session_scope) VALUES \
             ('agent-2262', 'agent-2262', 'qoder', 'h-2262', 1, 1, 'user-1', 'active', 'agent')",
    ] {
        sqlx::query(statement).execute(&pool).await.unwrap();
    }
    pool.close().await;

    let app = prepare(&config(fixture.path.clone())).await.unwrap();

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

    let transport = CapturingStart::default();
    app.test_runtime_credentials
        .dispatch_start(
            RuntimeScope::AgentSlot {
                agent: AgentId::parse("agent-2262").unwrap(),
            },
            DaemonInstanceId::parse("daemon-2262").unwrap(),
            ConversationId::parse(CONVERSATION_ID).unwrap(),
            &transport,
        )
        .await
        .unwrap();
    let agent_bearer = format!("Bearer {}", transport.0.lock().unwrap().clone().unwrap());
    let human_bearer = format!("Bearer {TOKEN}");
    let router = app.router;

    struct Case {
        method: &'static str,
        path: &'static str,
        bearer: &'static str,
        json: Option<&'static str>,
        expected: StatusCode,
    }
    let cases = [
        Case {
            method: "GET",
            path: "/api/notifications",
            bearer: "human",
            json: None,
            expected: StatusCode::OK,
        },
        Case {
            method: "GET",
            path: "/api/notifications/unread",
            bearer: "human",
            json: None,
            expected: StatusCode::OK,
        },
        Case {
            method: "POST",
            path: "/api/notifications/read",
            bearer: "human",
            json: Some(r#"{"all":true}"#),
            expected: StatusCode::OK,
        },
        Case {
            method: "POST",
            path: "/api/messages/message-absent/reactions",
            bearer: "human",
            json: Some(r#"{"emoji":"👍"}"#),
            expected: StatusCode::NOT_FOUND,
        },
        Case {
            method: "DELETE",
            path: "/api/messages/message-absent/reactions",
            bearer: "human",
            json: Some(r#"{"emoji":"👍"}"#),
            expected: StatusCode::NOT_FOUND,
        },
        Case {
            method: "GET",
            path: "/api/messages/message-absent/reactions",
            bearer: "human",
            json: None,
            expected: StatusCode::NOT_FOUND,
        },
        Case {
            method: "POST",
            path: "/internal/agents/agent-2262/react-message",
            bearer: "agent",
            json: Some(r#"{"message_id":"message-absent","emoji":"👍"}"#),
            expected: StatusCode::NOT_FOUND,
        },
        Case {
            method: "DELETE",
            path: "/internal/agents/agent-2262/react-message",
            bearer: "agent",
            json: Some(r#"{"message_id":"message-absent","emoji":"👍"}"#),
            expected: StatusCode::NOT_FOUND,
        },
        Case {
            method: "POST",
            path: "/api/channels/channel-absent/attachments",
            bearer: "human",
            json: Some("{}"),
            expected: StatusCode::NOT_FOUND,
        },
        Case {
            method: "GET",
            path: "/api/channels/channel-absent/attachments",
            bearer: "human",
            json: None,
            expected: StatusCode::NOT_FOUND,
        },
        Case {
            method: "GET",
            path: "/api/channels/channel-absent/attachments/senders",
            bearer: "human",
            json: None,
            expected: StatusCode::NOT_FOUND,
        },
        Case {
            method: "GET",
            path: "/api/attachments/attachment-absent",
            bearer: "human",
            json: None,
            expected: StatusCode::NOT_FOUND,
        },
        Case {
            method: "PATCH",
            path: "/api/attachments/attachment-absent",
            bearer: "human",
            json: Some("{}"),
            expected: StatusCode::NOT_FOUND,
        },
        Case {
            method: "DELETE",
            path: "/api/attachments/attachment-absent",
            bearer: "human",
            json: None,
            expected: StatusCode::NOT_FOUND,
        },
        Case {
            method: "POST",
            path: "/internal/agents/agent-2262/attachments",
            bearer: "agent",
            json: Some("{}"),
            expected: StatusCode::BAD_REQUEST,
        },
        Case {
            method: "PATCH",
            path: "/internal/agents/agent-2262/attachments/attachment-absent",
            bearer: "agent",
            json: Some("{}"),
            expected: StatusCode::NOT_FOUND,
        },
        Case {
            method: "GET",
            path: "/internal/agents/agent-2262/attachments/attachment-absent/locate",
            bearer: "agent",
            json: None,
            expected: StatusCode::NOT_FOUND,
        },
        Case {
            method: "GET",
            path: "/internal/agents/agent-2262/attachments/attachment-absent/download",
            bearer: "agent",
            json: None,
            expected: StatusCode::NOT_FOUND,
        },
    ];
    for case in cases {
        let (status, body) = request(&router, case.method, case.path, None).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "anonymous {} {}: {body}",
            case.method,
            case.path
        );
        assert_eq!(body, r#"{"error":"unauthorized"}"#);
        let bearer = match case.bearer {
            "human" => &human_bearer,
            "agent" => &agent_bearer,
            _ => unreachable!(),
        };
        let (status, body) = match case.json {
            Some(body) => {
                json_request_with_body(
                    &router,
                    case.method,
                    case.path,
                    Some(bearer),
                    Body::from(body),
                )
                .await
            }
            None => request(&router, case.method, case.path, Some(bearer)).await,
        };
        assert_ne!(
            status,
            StatusCode::METHOD_NOT_ALLOWED,
            "{} {} did not dispatch: {body}",
            case.method,
            case.path,
        );
        assert_eq!(
            status, case.expected,
            "{} {}: {body}",
            case.method, case.path
        );
    }

    // Shared-node negative controls: admitted callers reach method dispatch, and an
    // unregistered method is exactly 405. A catch-all could otherwise make every
    // positive probe above look registered.
    for (path, bearer) in [
        ("/api/messages/message-absent/reactions", &human_bearer),
        ("/api/attachments/attachment-absent", &human_bearer),
        ("/internal/agents/agent-2262/react-message", &agent_bearer),
        (
            "/internal/agents/agent-2262/attachments/attachment-absent/locate",
            &agent_bearer,
        ),
    ] {
        let (status, _) = request(&router, "PUT", path, Some(bearer)).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "PUT {path}");
    }
}

/// #2219: `/api/hosts` carries TWO methods on one path — `GET` (the newly mounted
/// list) and `POST` (the pre-existing invite). axum accumulates them into one
/// `MethodRouter`, so this gate proves neither clobbered the other and no accidental
/// catch-all swallowed the path: authenticated `GET` dispatches to the LIST handler
/// (a `{"hosts":[...]}` body with no `connect_token`), authenticated `POST` dispatches
/// to the INVITE handler (a `host_id` + `connect_token` body, no `hosts`), and an
/// unregistered method (`DELETE`/`PATCH`) is `405` from the shared `MethodRouter` — not
/// `404` (which would mean the path vanished) nor a handler status (which would mean a
/// catch-all). The two success bodies are disjoint in their keys, so a mount that
/// accidentally pointed both methods at one handler reddens here.
#[tokio::test]
async fn the_api_hosts_path_dispatches_get_to_list_and_post_to_invite() {
    let fixture = fixture(true).await;
    seed_human(&fixture.path).await;
    let app = prepare(&config(fixture.path.clone())).await.unwrap();
    let router = app.router;
    let bearer = format!("Bearer {TOKEN}");

    // Authenticated GET → the list handler. `user-1` owns no hosts yet, so the real
    // owner-scoped read off live SQLite is an empty list — the LIST shape, never the
    // invite's `connect_token`.
    let (status, body) = request(&router, "GET", "/api/hosts", Some(&bearer)).await;
    assert_eq!(status, StatusCode::OK, "authed GET /api/hosts: {body}");
    let listed: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        listed,
        serde_json::json!({ "hosts": [] }),
        "GET dispatches to the list handler"
    );
    assert!(
        listed.get("connect_token").is_none(),
        "the list arm is not the invite arm"
    );

    // Authenticated POST (empty body) → the invite handler: it mints a pending
    // user-managed computer off live SQLite and reveals the connect token. The INVITE
    // shape carries `host_id` + `connect_token` and no `hosts` array.
    let (status, body) = json_request_with_body(
        &router,
        "POST",
        "/api/hosts",
        Some(&bearer),
        Body::from("{}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "authed POST /api/hosts: {body}");
    let invited: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(
        invited.get("host_id").is_some_and(|v| v.is_string()),
        "POST dispatches to the invite handler: {body}"
    );
    assert!(
        invited.get("connect_token").is_some_and(|v| v.is_string()),
        "the invite arm reveals a connect token: {body}"
    );
    assert!(
        invited.get("hosts").is_none(),
        "the invite arm is not the list arm"
    );

    // Unregistered methods → 405 from the shared `MethodRouter` (path present, method
    // absent), never 404 (path gone) nor a handler status (accidental catch-all).
    for method in ["DELETE", "PATCH", "PUT"] {
        let (status, body) = request(&router, method, "/api/hosts", Some(&bearer)).await;
        assert_eq!(
            status,
            StatusCode::METHOD_NOT_ALLOWED,
            "{method} /api/hosts must be 405 (no clobber, no catch-all): {body}"
        );
    }
}

/// #2223 CP5 + #2234 CP1 shared-path dispatch: `/api/hosts/{id}` now carries THREE
/// methods on one node — `GET` (detail), `DELETE` (teardown) and `PATCH` (update).
/// axum accumulates them into one
/// `MethodRouter`, so this gate proves none clobbered the others and no catch-all
/// swallowed the node: over a REAL seeded owner-managed computer, authenticated `GET`
/// dispatches to the DETAIL handler (a `{"host":{...}}` body, no `deleted`),
/// `PATCH` (a valid rename body) dispatches to the UPDATE handler — routed through the
/// real `UpdateComputer` domain, which refuses this never-attached user-invite computer
/// with its own typed `409 host_pending_attach` (not `405`, not detail/teardown) — and
/// authenticated `DELETE` dispatches to the TEARDOWN handler (a `{"host_id",deleted:true}`
/// body, no `host`), and after that teardown the row is GONE — a second `GET` collapses
/// to the domain `404 host_not_found`, which a no-op or shared-handler mount could never
/// produce. The detail and teardown
/// success bodies are key-disjoint (`host` vs `host_id`+`deleted`), so a mount that
/// accidentally pointed both methods at one handler reddens here.
#[tokio::test]
async fn the_api_hosts_id_path_dispatches_get_detail_patch_update_and_delete_teardown() {
    let fixture = fixture(true).await;
    seed_human(&fixture.path).await;
    // One owner-managed (remote + user_invite), dependent-free computer owned by the
    // seeded `user-1`: deletable through the full composed cascade with no agents to
    // stop, no files to purge and no blobs to forget.
    let pool = SqlitePoolOptions::new()
        .connect(fixture.path.to_str().unwrap())
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO hosts (id, name, kind, created_at, user_id, provisioning_source) \
             VALUES ('h-own', 'own', 'remote', 1, 'user-1', 'user_invite')",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    let app = prepare(&config(fixture.path.clone())).await.unwrap();
    let router = app.router;
    let bearer = format!("Bearer {TOKEN}");

    // Authenticated GET → the DETAIL handler: the host wire for the owned computer.
    // The DETAIL shape carries `host` and no `deleted`.
    let (status, body) = request(&router, "GET", "/api/hosts/h-own", Some(&bearer)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "authed GET /api/hosts/h-own: {body}"
    );
    let detail: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        detail["host"]["id"], "h-own",
        "GET dispatches to the detail handler: {body}"
    );
    assert!(
        detail.get("deleted").is_none(),
        "the detail arm is not the teardown arm: {body}"
    );

    // PATCH on the shared node → the UPDATE handler (#2234 CP1), never 405. The node
    // now bears three disjoint methods (GET detail + DELETE teardown + PATCH update). A
    // valid rename body routes past the body-parse guard into the real `UpdateComputer`
    // authority, which refuses this never-attached user-invite computer with its OWN
    // typed 409 — proving PATCH reached the update handler and its domain, not a 405
    // `MethodRouter` miss nor the detail/teardown arms. (PendingAttach is a pre-write
    // refusal, so h-own is untouched and the DELETE teardown below still fires.)
    let (status, body) = request_with_body(
        &router,
        "PATCH",
        "/api/hosts/h-own",
        Some(&bearer),
        Body::from(r#"{"display_label":"renamed"}"#),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "PATCH /api/hosts/{{id}} must reach the update handler, not 405: {body}"
    );
    assert_eq!(
        body,
        r#"{"error":"host has not completed its first daemon attachment","error_code":"host_pending_attach"}"#,
        "PATCH dispatches to the update handler's domain, not 405/detail/teardown"
    );

    // Authenticated DELETE → the TEARDOWN handler: the dependent-free owner-managed
    // computer is removed through the full composed cascade. The TEARDOWN shape carries
    // `host_id` + `deleted:true` and no `host` array.
    let (status, body) = request(&router, "DELETE", "/api/hosts/h-own", Some(&bearer)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "authed DELETE /api/hosts/h-own: {body}"
    );
    let torn_down: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        torn_down,
        serde_json::json!({ "host_id": "h-own", "deleted": true }),
        "DELETE dispatches to the teardown handler: {body}"
    );

    // Real effect, not a no-op: the row is gone, so a second GET collapses to the
    // domain 404 — proving DELETE reached the teardown, not the detail, handler.
    let (status, body) = request(&router, "GET", "/api/hosts/h-own", Some(&bearer)).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "post-teardown GET must be gone: {body}"
    );
    assert_eq!(
        body, r#"{"error":"host not found","error_code":"host_not_found"}"#,
        "the teardown really removed the row"
    );
}

/// #2234 CP1 no-op-mount discriminator (A6 lesson §1): a *successful* PATCH rename
/// must drive the real post-commit `ScopeEventHostLabelChange` chain end to end —
/// the production `SqliteComputerAgentsRead` roster read → the SAME production
/// `ScopeEventHub` → an app-scoped `agent_changed` frame — none of which the
/// dispatch test above exercises (its user-invite host exits at the PendingAttach
/// pre-write refusal, before any commit or effect). Here the owner-managed host is
/// already finalized (`finalized_at` set ⇒ `finalizing == false`), so the rename
/// takes the clean `Write`/`Renamed` path and fires the effect. We prove three
/// things at once: (1) the label change is durable (the committed reprojection in
/// the PATCH response carries the new label), and (2) the effect published a frame
/// for EXACTLY the host's own agent — a second agent bound to a different host of
/// the same owner is silent, proving the roster read is host-scoped, not owner- or
/// app-wide. A silent no-op mount would deliver no frame at all.
#[tokio::test]
async fn the_composition_host_rename_publishes_agent_changed_for_only_the_hosts_own_agents() {
    let fixture = fixture(true).await;
    seed_human(&fixture.path).await;
    let pool = SqlitePoolOptions::new()
        .connect(fixture.path.to_str().unwrap())
        .await
        .unwrap();
    // An owner-managed (remote + user_invite) computer that has ALREADY finalized
    // its first attachment (`activated_at` + `finalized_at` set) ⇒ `finalizing`
    // is false, so a valid rename reaches the real `Write` and returns `Renamed`.
    sqlx::query(
            "INSERT INTO hosts (id, name, kind, created_at, user_id, provisioning_source, activated_at, finalized_at) \
             VALUES ('h-mgmt', 'mgmt', 'remote', 1, 'user-1', 'user_invite', 1, 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
    // A second owner-managed computer, so its agent proves host-scoping (same owner,
    // different host_id) — it must NOT receive a frame from h-mgmt's rename.
    sqlx::query(
            "INSERT INTO hosts (id, name, kind, created_at, user_id, provisioning_source, activated_at, finalized_at) \
             VALUES ('h-peer', 'peer', 'remote', 1, 'user-1', 'user_invite', 1, 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
            "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, created_by_user_id) \
             VALUES ('agent-on-mgmt', 'on-mgmt', 'qoder', 'h-mgmt', 1, 1, 'user-1')",
        )
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
            "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, created_by_user_id) \
             VALUES ('agent-on-peer', 'on-peer', 'qoder', 'h-peer', 1, 1, 'user-1')",
        )
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    let app = prepare(&config(fixture.path.clone())).await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let running = tokio::spawn(serve(listener, app, async move {
        let _ = shutdown_rx.await;
        Ok::<(), std::io::Error>(())
    }));

    // Subscribe to the APP scope on the real production event WebSocket FIRST, so
    // the `agent_changed` frame the rename publishes cannot slip past a late
    // subscription (fresh-subscription high-water; A6 lesson §2).
    let mut websocket_request = format!("ws://{address}{EVENT_WEBSOCKET_PATH}")
        .into_client_request()
        .unwrap();
    websocket_request
        .headers_mut()
        .insert(ORIGIN, "http://localhost:5173".parse().unwrap());
    websocket_request.headers_mut().insert(
        COOKIE,
        format!("{EVENT_WS_SESSION_COOKIE}={TOKEN}")
            .parse()
            .unwrap(),
    );
    websocket_request
        .headers_mut()
        .insert(DESKTOP_API_EPOCH_HEADER, MINIMUM_CLIENT_API_EPOCH.into());
    let (mut socket, _) = connect_async(websocket_request).await.unwrap();
    let authenticated = socket.next().await.unwrap().unwrap();
    let ClientMessage::Text(authenticated) = authenticated else {
        panic!("authenticated frame must be text");
    };
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&authenticated).unwrap()["type"],
        "authenticated"
    );
    socket
        .send(ClientMessage::Text(
            serde_json::json!({ "type": "subscribe", "scope": "app" })
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    let subscribed = socket.next().await.unwrap().unwrap();
    let ClientMessage::Text(subscribed) = subscribed else {
        panic!("subscribed frame must be text");
    };
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&subscribed).unwrap()["type"],
        "subscribed"
    );
    // Deterministic app-scope high-water BEFORE the rename. The effect runs
    // post-commit inside the handler, so once the PATCH returns the count is
    // settled; the delta below is the exact number of app-scope events the rename
    // published — the falsifier for "only h-mgmt's agent".
    let app_high_water_before = fresh_scope_high_water(address, "app").await;

    // A valid rename → the real `UpdateComputer` domain takes the `Write`/`Renamed`
    // path (finalized host, so no PendingAttach), commits, and the committed
    // reprojection is returned as `{"host": HostDto}` with the new label.
    let response = http_over_tcp(
        address,
        "PATCH",
        "/api/hosts/h-mgmt",
        Some(&format!("Bearer {TOKEN}")),
        r#"{"display_label":"renamed-host"}"#,
    )
    .await;
    assert!(
        response.starts_with("HTTP/1.1 200 OK\r\n"),
        "PATCH rename must succeed on the finalized owner host: {response}"
    );
    let body = response
        .split_once("\r\n\r\n")
        .expect("response has a body")
        .1;
    let renamed: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(
        renamed["host"]["id"], "h-mgmt",
        "PATCH returns the committed host detail: {body}"
    );
    assert_eq!(
        renamed["host"]["display_label"], "renamed-host",
        "the label change is durable in the committed reprojection: {body}"
    );

    // The real effect published EXACTLY the host's own agent on the app scope.
    let event = socket.next().await.unwrap().unwrap();
    let ClientMessage::Text(event) = event else {
        panic!("agent_changed frame must be text");
    };
    let event: serde_json::Value = serde_json::from_str(&event).unwrap();
    assert_eq!(
        event["scope"], "app",
        "agent_changed is app-scoped: {event}"
    );
    assert_eq!(event["kind"], "agent_changed", "{event}");
    assert_eq!(
        event["payload"]["agent_id"], "agent-on-mgmt",
        "the roster read is host-scoped: only h-mgmt's agent, never h-peer's: {event}"
    );

    // Deterministic peer-silence proof: the rename published EXACTLY ONE app-scope
    // event. Frame-1 alone can't prove this — a broken roster returning
    // [agent-on-mgmt, agent-on-peer] would still deliver agent-on-mgmt first and
    // leave the second frame queued. The settled high-water delta is 1 iff the
    // host-scoped roster read yielded only h-mgmt's agent; a second publish (peer
    // leaked in) makes it 2 and the gate goes red.
    let app_high_water_after = fresh_scope_high_water(address, "app").await;
    assert_eq!(
        app_high_water_after - app_high_water_before,
        1,
        "the rename must publish exactly one app-scope event (only h-mgmt's agent), \
             not one per owner-agent: before={app_high_water_before} after={app_high_water_after}"
    );

    socket.close(None).await.unwrap();
    shutdown_tx.send(()).unwrap();
    running.await.unwrap().unwrap();
}

/// #2234 CP2 — the thin public-agent write faces (#2225) are MOUNTED and reachable,
/// and the shared-authority contract named is proven by real effect, not by
/// construction alone:
///
///  - `PATCH /api/agents/{id}` reaches the real profile-write authority (a durable
///    `display_name` change, read back through the shared `/api/agents/{id}` GET)
///    and publishes exactly one `agent_changed` on the ONE production hub;
///  - the three avatar faces (`GET`+`POST`+`DELETE /api/agents/{id}/avatar`) ride the
///    ONE `SingleKindAvatarBlobs` root through a genuine upload→read→clear closed
///    loop — the bytes POSTed come back byte-identical on GET, then the DELETE clears
///    them and the same GET becomes the `avatar not found` 404 — and the upload and
///    the clear each publish exactly one `agent_changed` on that SAME hub;
///  - the shared `/api/agents/{id}` node still dispatches GET to the detail handler
///    (PATCH accumulated its method onto the node; it did not shadow the GET);
///  - `GET /api/agents/name-availability` reaches the real handle namespace — the
///    STATIC segment wins over the `{id}` template, so it is the availability probe,
///    never the detail route's `agent not found`.
///
/// The per-mutation app-scope high-water delta is the falsifier for a silent no-op
/// mount: were any write face left unmounted (or wired to a dead sink), its delta
/// would be 0; the durable read-backs prove the write authority itself was consulted.
#[tokio::test]
async fn the_composition_agent_write_faces_share_one_profile_authority_and_avatar_root() {
    let fixture = fixture(true).await;
    seed_human(&fixture.path).await;
    let pool = SqlitePoolOptions::new()
        .connect(fixture.path.to_str().unwrap())
        .await
        .unwrap();
    // One creator-visible, ACTIVE agent `user-1` may edit — the single subject every
    // CP2 face resolves. `materialization_status = 'active'` satisfies the
    // active-only visibility view each face reads (see `seed_presence_agents`); the
    // creator arm (`created_by_user_id = 'user-1'`) satisfies the edit authority.
    sqlx::query(
            "INSERT INTO hosts (id, name, kind, created_at, user_id) VALUES ('h-cp2', 'host-cp2', 'remote', 1, 'user-1')",
        )
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
            "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, created_by_user_id, materialization_status) \
             VALUES ('a-cp2', 'agent-cp2', 'qoder', 'h-cp2', 1, 1, 'user-1', 'active')",
        )
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    let app = prepare(&config(fixture.path.clone())).await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let running = tokio::spawn(serve(listener, app, async move {
        let _ = shutdown_rx.await;
        Ok::<(), std::io::Error>(())
    }));

    // Subscribe to the APP scope on the real production event WebSocket FIRST, so no
    // `agent_changed` frame can slip past a late subscription (fresh-subscription
    // high-water; A6 lesson §2).
    let mut websocket_request = format!("ws://{address}{EVENT_WEBSOCKET_PATH}")
        .into_client_request()
        .unwrap();
    websocket_request
        .headers_mut()
        .insert(ORIGIN, "http://localhost:5173".parse().unwrap());
    websocket_request.headers_mut().insert(
        COOKIE,
        format!("{EVENT_WS_SESSION_COOKIE}={TOKEN}")
            .parse()
            .unwrap(),
    );
    websocket_request
        .headers_mut()
        .insert(DESKTOP_API_EPOCH_HEADER, MINIMUM_CLIENT_API_EPOCH.into());
    let (mut socket, _) = connect_async(websocket_request).await.unwrap();
    let authenticated = socket.next().await.unwrap().unwrap();
    let ClientMessage::Text(authenticated) = authenticated else {
        panic!("authenticated frame must be text");
    };
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&authenticated).unwrap()["type"],
        "authenticated"
    );
    socket
        .send(ClientMessage::Text(
            serde_json::json!({ "type": "subscribe", "scope": "app" })
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    let subscribed = socket.next().await.unwrap().unwrap();
    let ClientMessage::Text(subscribed) = subscribed else {
        panic!("subscribed frame must be text");
    };
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&subscribed).unwrap()["type"],
        "subscribed"
    );

    let bearer = format!("Bearer {TOKEN}");

    // ---- (1) PATCH rides the shared profile-write authority + `agent_changed` hub.
    let before = fresh_scope_high_water(address, "app").await;
    let response = http_over_tcp(
        address,
        "PATCH",
        "/api/agents/a-cp2",
        Some(&bearer),
        r#"{"display_name":"Renamed CP2"}"#,
    )
    .await;
    assert!(
        response.starts_with("HTTP/1.1 200 OK\r\n"),
        "PATCH reaches the mounted profile-write face: {response}"
    );
    let body: serde_json::Value =
        serde_json::from_str(response.split_once("\r\n\r\n").expect("body").1).unwrap();
    assert_eq!(
        body["agent"]["display_name"], "Renamed CP2",
        "the profile-write authority committed the new label: {body}"
    );
    let after = fresh_scope_high_water(address, "app").await;
    assert_eq!(
        after - before,
        1,
        "PATCH publishes exactly one agent_changed on the shared hub: {before}→{after}"
    );

    // The change is durable through the SAME `/api/agents/{id}` node — GET still
    // dispatches to the detail handler; PATCH accumulated its method, never shadowed
    // the GET.
    let detail = http_over_tcp(address, "GET", "/api/agents/a-cp2", Some(&bearer), "").await;
    assert!(
        detail.starts_with("HTTP/1.1 200 OK\r\n"),
        "GET /api/agents/{{id}} stays the detail handler after PATCH accumulates: {detail}"
    );
    let detail: serde_json::Value =
        serde_json::from_str(detail.split_once("\r\n\r\n").expect("body").1).unwrap();
    assert_eq!(
        detail["agent"]["display_name"], "Renamed CP2",
        "the detail GET reads the durable committed label: {detail}"
    );

    // ---- (2) upload→read→clear closed loop over the ONE `SingleKindAvatarBlobs` root.
    let avatar_bytes = b"PNGDATA-cp2-avatar-payload".to_vec();
    let boundary = "XBOUNDARYCP2";
    let mut multipart: Vec<u8> = Vec::new();
    multipart.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    multipart.extend_from_slice(
        b"Content-Disposition: form-data; name=\"file\"; filename=\"a.png\"\r\n",
    );
    multipart.extend_from_slice(b"Content-Type: image/png\r\n\r\n");
    multipart.extend_from_slice(&avatar_bytes);
    multipart.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

    let before = fresh_scope_high_water(address, "app").await;
    let upload = http_over_tcp_typed(
        address,
        "POST",
        "/api/agents/a-cp2/avatar",
        Some(&bearer),
        &format!("multipart/form-data; boundary={boundary}"),
        &multipart,
    )
    .await;
    let (upload_head, upload_body) = split_http_response(&upload);
    assert!(
        upload_head.starts_with("HTTP/1.1 200 OK\r\n"),
        "avatar upload reaches the mounted write face: {upload_head}"
    );
    let upload_body: serde_json::Value = serde_json::from_slice(&upload_body).unwrap();
    assert!(
        upload_body["avatar_url"]
            .as_str()
            .is_some_and(|url| url.starts_with("/api/agents/a-cp2/avatar?v=")),
        "the upload response carries the cache-busted avatar_url: {upload_body}"
    );
    let after = fresh_scope_high_water(address, "app").await;
    assert_eq!(
        after - before,
        1,
        "the avatar upload publishes exactly one agent_changed on the shared hub: {before}→{after}"
    );

    // GET returns the SAME bytes off the SAME blob root — the write and the read
    // faces share one `SingleKindAvatarBlobs`.
    let served = http_over_tcp_typed(
        address,
        "GET",
        "/api/agents/a-cp2/avatar",
        Some(&bearer),
        "application/json",
        b"",
    )
    .await;
    let (served_head, served_body) = split_http_response(&served);
    assert!(
        served_head.starts_with("HTTP/1.1 200 OK\r\n"),
        "avatar GET serves the just-uploaded blob: {served_head}"
    );
    assert!(
        served_head
            .to_ascii_lowercase()
            .contains("content-type: image/png"),
        "the served avatar carries its stored content type: {served_head}"
    );
    assert_eq!(
        served_body, avatar_bytes,
        "the avatar GET returns the exact bytes the upload stored (one blob root)"
    );

    // DELETE clears the avatar and announces once.
    let before = fresh_scope_high_water(address, "app").await;
    let cleared = http_over_tcp(
        address,
        "DELETE",
        "/api/agents/a-cp2/avatar",
        Some(&bearer),
        "",
    )
    .await;
    assert!(
        cleared.starts_with("HTTP/1.1 200 OK\r\n"),
        "avatar DELETE reaches the mounted clear face: {cleared}"
    );
    let cleared: serde_json::Value =
        serde_json::from_str(cleared.split_once("\r\n\r\n").expect("body").1).unwrap();
    assert_eq!(
        cleared["agent"]["id"], "a-cp2",
        "the clear returns the subject's committed detail: {cleared}"
    );
    let after = fresh_scope_high_water(address, "app").await;
    assert_eq!(
        after - before,
        1,
        "the avatar clear publishes exactly one agent_changed on the shared hub: {before}→{after}"
    );

    // The SAME GET is now the `avatar not found` 404 — the clear removed the blob
    // from the one root the serve face reads.
    let gone = http_over_tcp(
        address,
        "GET",
        "/api/agents/a-cp2/avatar",
        Some(&bearer),
        "",
    )
    .await;
    assert!(
        gone.starts_with("HTTP/1.1 404 Not Found\r\n"),
        "the cleared avatar GET is 404: {gone}"
    );
    let gone: serde_json::Value =
        serde_json::from_str(gone.split_once("\r\n\r\n").expect("body").1).unwrap();
    assert_eq!(
        gone["error"], "avatar not found",
        "the closed loop ends at the leaf's avatar-not-found body: {gone}"
    );

    // ---- (3) name-availability reaches the real namespace: the static segment wins.
    let availability = http_over_tcp(
        address,
        "GET",
        "/api/agents/name-availability?name=freehandle",
        Some(&bearer),
        "",
    )
    .await;
    assert!(
        availability.starts_with("HTTP/1.1 200 OK\r\n"),
        "name-availability is mounted behind the perimeter: {availability}"
    );
    let availability: serde_json::Value =
        serde_json::from_str(availability.split_once("\r\n\r\n").expect("body").1).unwrap();
    assert_eq!(
        availability,
        serde_json::json!({ "available": true, "reason": "available" }),
        "the STATIC name-availability route wins over the {{id}} detail template \
             (a shadow would answer the detail route's `agent not found`): {availability}"
    );

    socket.close(None).await.unwrap();
    shutdown_tx.send(()).unwrap();
    running.await.unwrap().unwrap();
}

/// #2234 CP7a — the internal self-scope profile/avatar/peer family, driven through the
/// REAL `prepare()` router over a REAL minted runtime credential.
///
/// The `RUNTIME_PERIMETER_DOORS` ledger already proves each `/internal/agents/{aid}/…`
/// path is MOUNTED and refuses an anonymous caller with `{"error":"unauthorized"}`. What
/// no anonymous probe can show is that the handlers BEHIND that perimeter reach real
/// durable authority. So this mints a bearer through the SAME `Arc`-backed
/// `RuntimeCredentialAuthority` the perimeter authenticates against (the `#[cfg(test)]`
/// `test_runtime_credentials` handle is a clone sharing its process-local token store),
/// then drives the family end to end:
///   * a valid bearer reaches its OWN `{aid}`; a foreign `{aid}` and a missing/invalid
///     bearer are the uniform perimeter 401; an unlisted method is the ROUTE's 405.
///   * profile POST commits durably (GET readback) and announces exactly one
///     `agent_changed`; the readback proves GET and POST coexist on the one profile node.
///   * internal avatar POST → public `/api/agents/{id}/avatar` GET returns those exact
///     bytes → internal DELETE → the public GET is 404 — proving the internal write and
///     the #2225 public read share ONE `SingleKindAvatarBlobs` root, and that POST and
///     DELETE coexist on the one internal avatar node.
///   * peer-profile passes the real caller ladder (horizon + `read_messages`) and runs
///     the real by-handle reader — an unknown handle collapses to the leaf's 404, which a
///     shaped stub could never reach through the ladder.
/// The refusal grammar, projection bytes and effect semantics are each pinned by the
/// leaves' own tests; this asserts the MOUNT + authenticated wiring, not a second reading.
#[tokio::test]
async fn the_composition_internal_self_scope_family_reaches_and_effects_over_one_runtime_credential()
 {
    use std::{
        convert::Infallible,
        future::Future,
        sync::{Arc, Mutex},
    };

    use silverhand_backend_core::{
        foundation::identity::{ConversationId, DaemonInstanceId},
        host_runtime::runtime_credential::RuntimeScope,
    };
    use silverhand_backend_server::host_runtime::runtime_credential::{
        RuntimeDispatchFailure, RuntimeStartHandoff, RuntimeStartTransport,
    };

    let fixture = fixture(true).await;
    // The APP-scope high-water probe below rides the human event WebSocket, which
    // authenticates as `user-1` via the seeded `auth_sessions` row (bearer `TOKEN`).
    // `seed_human` also inserts the `user-1` row the host/agent/workspace rows reference.
    seed_human(&fixture.path).await;
    let pool = SqlitePoolOptions::new()
        .connect(fixture.path.to_str().unwrap())
        .await
        .unwrap();
    // `agent-1` is the ONE self-scope subject. `session_scope='agent'` fires the
    // `agents_create_agent_runtime_slot` trigger, so a claimable `agent_runtime_slots`
    // row exists for `dispatch_start` to mint against. Membership in `workspace0000001`
    // plus its `read_messages` capability lets the peer-profile caller ladder pass all
    // the way to the real by-handle reader.
    for statement in [
        "INSERT INTO hosts (id, name, kind, created_at, user_id) \
             VALUES ('h-cp7a', 'host-cp7a', 'remote', 1, 'user-1')",
        "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, \
             created_by_user_id, materialization_status, session_scope) \
             VALUES ('agent-1', 'agent-one', 'qoder', 'h-cp7a', 1, 1, 'user-1', 'active', 'agent')",
        "INSERT INTO workspaces (id, name, name_key, owner_user_id, created_at, updated_at) \
             VALUES ('workspace0000001', 'Product Space', 'product space', 'user-1', 0, 0)",
        "INSERT INTO workspace_members (workspace_id, agent_id, joined_at) \
             VALUES ('workspace0000001', 'agent-1', 0)",
        "INSERT INTO workspace_agent_capabilities (workspace_id, agent_id, capability) \
             VALUES ('workspace0000001', 'agent-1', 'read_messages')",
    ] {
        sqlx::query(statement).execute(&pool).await.unwrap();
    }
    pool.close().await;

    let app = prepare(&config(fixture.path.clone())).await.unwrap();

    // Mint a REAL bearer through the SAME authority the mounted perimeter holds — BEFORE
    // `serve` consumes `app`. The authority is `Arc`-backed, so the token this records in
    // the shared process-local store is exactly the one the running router authenticates.
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
    let transport = CapturingStart::default();
    app.test_runtime_credentials
        .dispatch_start(
            RuntimeScope::AgentSlot {
                agent: AgentId::parse("agent-1").unwrap(),
            },
            DaemonInstanceId::parse("daemon-1").unwrap(),
            ConversationId::parse(CONVERSATION_ID).unwrap(),
            &transport,
        )
        .await
        .unwrap();
    let token = transport.0.lock().unwrap().clone().unwrap();
    let bearer = format!("Bearer {token}");
    // The public avatar GET rides the HUMAN perimeter, not the runtime one, so the
    // round-trip reads it back as the seeded human session (`user-1`, creator of
    // `agent-1`) — the same bearer #2225's public avatar test uses.
    let human_bearer = format!("Bearer {TOKEN}");

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let running = tokio::spawn(serve(listener, app, async move {
        let _ = shutdown_rx.await;
        Ok::<(), std::io::Error>(())
    }));

    // ---- (1) the perimeter: a valid bearer reaches self; a foreign `{aid}`, a missing
    // bearer and an unknown bearer all collapse to the uniform 401; an unlisted method is
    // the ROUTE's 405 (asserted after an admitted call, below).
    let anon = http_over_tcp(address, "GET", "/internal/agents/agent-1/profile", None, "").await;
    assert!(
        anon.starts_with("HTTP/1.1 401 Unauthorized\r\n"),
        "a missing bearer is the runtime perimeter 401: {anon}"
    );
    let bad = http_over_tcp(
        address,
        "GET",
        "/internal/agents/agent-1/profile",
        Some("Bearer unknown-runtime-token"),
        "",
    )
    .await;
    assert!(
        bad.starts_with("HTTP/1.1 401 Unauthorized\r\n"),
        "an unknown bearer is the runtime perimeter 401: {bad}"
    );
    let foreign = http_over_tcp(
        address,
        "GET",
        "/internal/agents/agent-2/profile",
        Some(&bearer),
        "",
    )
    .await;
    assert!(
        foreign.starts_with("HTTP/1.1 401 Unauthorized\r\n"),
        "the bearer self-pins to agent-1: a foreign {{aid}} is refused at the perimeter: {foreign}"
    );
    let unauthorized: serde_json::Value =
        serde_json::from_str(foreign.split_once("\r\n\r\n").expect("body").1).unwrap();
    assert_eq!(
        unauthorized["error"], "unauthorized",
        "the runtime perimeter shares the human perimeter's refusal body: {unauthorized}"
    );

    // ---- (2) profile POST commits durably + announces exactly one agent_changed; the
    // GET readback proves the GET and POST method-union coexist on the one node.
    let boundary = "XBOUNDARYCP7A";
    let mut profile_form: Vec<u8> = Vec::new();
    profile_form.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    profile_form
        .extend_from_slice(b"Content-Disposition: form-data; name=\"display_name\"\r\n\r\n");
    profile_form.extend_from_slice(b"Renamed Self");
    profile_form.extend_from_slice(format!("\r\n--{boundary}\r\n").as_bytes());
    profile_form.extend_from_slice(b"Content-Disposition: form-data; name=\"description\"\r\n\r\n");
    profile_form.extend_from_slice(b"a self-authored bio");
    profile_form.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

    let before = fresh_scope_high_water(address, "app").await;
    let posted = http_over_tcp_typed(
        address,
        "POST",
        "/internal/agents/agent-1/profile",
        Some(&bearer),
        &format!("multipart/form-data; boundary={boundary}"),
        &profile_form,
    )
    .await;
    let (posted_head, posted_body) = split_http_response(&posted);
    assert!(
        posted_head.starts_with("HTTP/1.1 200 OK\r\n"),
        "the self profile POST reaches the mounted write face: {posted_head}"
    );
    let posted_body: serde_json::Value = serde_json::from_slice(&posted_body).unwrap();
    assert_eq!(
        posted_body["profile"]["display_name"], "Renamed Self",
        "the profile-write authority committed the new label: {posted_body}"
    );
    let after = fresh_scope_high_water(address, "app").await;
    assert_eq!(
        after - before,
        1,
        "the self profile POST announces exactly one agent_changed: {before}→{after}"
    );

    let readback = http_over_tcp(
        address,
        "GET",
        "/internal/agents/agent-1/profile",
        Some(&bearer),
        "",
    )
    .await;
    assert!(
        readback.starts_with("HTTP/1.1 200 OK\r\n"),
        "GET /internal/agents/{{aid}}/profile stays the read handler alongside POST: {readback}"
    );
    let readback: serde_json::Value =
        serde_json::from_str(readback.split_once("\r\n\r\n").expect("body").1).unwrap();
    assert_eq!(
        readback["profile"]["display_name"], "Renamed Self",
        "the read face returns the durable label: {readback}"
    );
    assert_eq!(
        readback["profile"]["description"], "a self-authored bio",
        "the read face returns the durable description: {readback}"
    );

    // An unlisted method on the profile node is the ROUTE's 405, not the perimeter 401 —
    // the authenticated caller is admitted, and the node answers GET+POST only.
    let method = http_over_tcp(
        address,
        "DELETE",
        "/internal/agents/agent-1/profile",
        Some(&bearer),
        "",
    )
    .await;
    assert!(
        method.starts_with("HTTP/1.1 405 Method Not Allowed\r\n"),
        "the profile node answers GET+POST only — an authenticated DELETE is 405: {method}"
    );

    // ---- (3) internal avatar POST → public GET bytes → internal DELETE → public GET 404,
    // proving the internal write and the #2225 public read share ONE blob root; POST and
    // DELETE coexist on the one internal avatar node.
    let avatar_bytes = b"PNGDATA-cp7a-self-avatar".to_vec();
    let mut avatar_form: Vec<u8> = Vec::new();
    avatar_form.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    avatar_form.extend_from_slice(
        b"Content-Disposition: form-data; name=\"file\"; filename=\"a.png\"\r\n",
    );
    avatar_form.extend_from_slice(b"Content-Type: image/png\r\n\r\n");
    avatar_form.extend_from_slice(&avatar_bytes);
    avatar_form.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

    let before = fresh_scope_high_water(address, "app").await;
    let upload = http_over_tcp_typed(
        address,
        "POST",
        "/internal/agents/agent-1/avatar",
        Some(&bearer),
        &format!("multipart/form-data; boundary={boundary}"),
        &avatar_form,
    )
    .await;
    let (upload_head, _) = split_http_response(&upload);
    assert!(
        upload_head.starts_with("HTTP/1.1 200 OK\r\n"),
        "the internal self avatar POST reaches the mounted write face: {upload_head}"
    );
    let after = fresh_scope_high_water(address, "app").await;
    assert_eq!(
        after - before,
        1,
        "the internal avatar POST announces exactly one agent_changed: {before}→{after}"
    );

    let served = http_over_tcp_typed(
        address,
        "GET",
        "/api/agents/agent-1/avatar",
        Some(&human_bearer),
        "application/json",
        b"",
    )
    .await;
    let (served_head, served_body) = split_http_response(&served);
    assert!(
        served_head.starts_with("HTTP/1.1 200 OK\r\n"),
        "the PUBLIC avatar GET serves the internally-uploaded blob: {served_head}"
    );
    assert!(
        served_head
            .to_ascii_lowercase()
            .contains("content-type: image/png"),
        "the served avatar carries its stored content type: {served_head}"
    );
    assert_eq!(
        served_body, avatar_bytes,
        "the public GET returns the exact bytes the internal POST stored — ONE blob root"
    );

    let before = fresh_scope_high_water(address, "app").await;
    let cleared = http_over_tcp(
        address,
        "DELETE",
        "/internal/agents/agent-1/avatar",
        Some(&bearer),
        "",
    )
    .await;
    assert!(
        cleared.starts_with("HTTP/1.1 200 OK\r\n"),
        "the internal self avatar DELETE reaches the mounted clear face: {cleared}"
    );
    let after = fresh_scope_high_water(address, "app").await;
    assert_eq!(
        after - before,
        1,
        "the internal avatar DELETE announces exactly one agent_changed: {before}→{after}"
    );

    let gone = http_over_tcp(
        address,
        "GET",
        "/api/agents/agent-1/avatar",
        Some(&human_bearer),
        "",
    )
    .await;
    assert!(
        gone.starts_with("HTTP/1.1 404 Not Found\r\n"),
        "the cleared avatar is 404 on the public read — the internal DELETE cleared the shared root: {gone}"
    );

    // ---- (4) peer-profile passes the real caller ladder (horizon + `read_messages`) and
    // runs the real by-handle reader; an unknown handle collapses to the leaf's 404, which
    // a shaped stub could never reach through the ladder.
    let peer = http_over_tcp(
        address,
        "GET",
        "/internal/agents/agent-1/peer-profile?handle=ghost&workspace_id=workspace0000001",
        Some(&bearer),
        "",
    )
    .await;
    assert!(
        peer.starts_with("HTTP/1.1 404 Not Found\r\n"),
        "peer-profile reaches the real by-handle reader and collapses an unknown handle to 404: {peer}"
    );
    let peer: serde_json::Value =
        serde_json::from_str(peer.split_once("\r\n\r\n").expect("body").1).unwrap();
    assert_eq!(
        peer["error"], "agent not found",
        "the real reader's miss is the collapsed 404 body: {peer}"
    );

    shutdown_tx.send(()).unwrap();
    running.await.unwrap().unwrap();
}

/// Drive a connected daemon socket like a REAL daemon for the internal profile-MCP
/// family until told to stop, then hand the socket back for a clean close: answer the
/// `runtime_status` hydration and the promotion-edge `host_status` probe, and answer each
/// of the three MCP commands with the schema-exact result its transport codec admits
/// (`decode_read` wants `{ ok, raw }`; `decode_write` wants `{ ok, bytes_written }` as a
/// JS-safe count; `decode_probe` wants `{ ok, phase, server_info?, tools? }`). Every MCP
/// command is RECORDED in `seen` — with its `kind` and `params` — so the gate can prove
/// each face crosses the wire as its OWN distinct command carrying the pinned self's id
/// and payload, and that a face whose owning host is detached reaches this transport zero
/// times. Recording happens BEFORE the response is sent, so a returned HTTP call always
/// finds its command already in `seen`. `promoted` is notified once the promotion-edge
/// `host_status` probe is answered — which the attach flow fires only AFTER `hydrate()`
/// has returned, so a credential minted after it is past the hydration finalize pass that
/// would otherwise rotate any custody persisted-for-host the daemon did not list as active.
async fn service_mcp_commands<S>(
    mut socket: tokio_tungstenite::WebSocketStream<S>,
    stop: std::sync::Arc<tokio::sync::Notify>,
    promoted: std::sync::Arc<tokio::sync::Notify>,
    seen: std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
) -> tokio_tungstenite::WebSocketStream<S>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    loop {
        tokio::select! {
            () = stop.notified() => break,
            command = next_daemon_json(&mut socket) => {
                let result = match command["kind"].as_str() {
                    Some("runtime_status") => serde_json::json!({ "runtimes": [] }),
                    Some("host_status") => {
                        promoted.notify_one();
                        serde_json::json!({
                            "os": "linux",
                            "daemon_pid": 1,
                            "daemon_version": "0.0.0-test",
                            "started_at": 0,
                            "uptime_ms": 0,
                            "active_agent_ids": [],
                        })
                    }
                    Some("agent_profile_mcp_read") => {
                        seen.lock().unwrap().push(command.clone());
                        serde_json::json!({ "ok": true, "raw": "{\"mcpServers\":{}}" })
                    }
                    Some("agent_profile_mcp_write") => {
                        seen.lock().unwrap().push(command.clone());
                        serde_json::json!({ "ok": true, "bytes_written": 128 })
                    }
                    Some("mcp_probe") => {
                        seen.lock().unwrap().push(command.clone());
                        serde_json::json!({
                            "ok": true,
                            "phase": "tools",
                            "server_info": { "name": "srv" },
                            "tools": [ { "name": "t1", "description": "does a thing" } ],
                        })
                    }
                    other => panic!("unexpected daemon command kind: {other:?}"),
                };
                socket
                    .send(ClientMessage::Text(
                        serde_json::json!({
                            "direction": "daemon_to_server",
                            "type": "command_response",
                            "id": command["id"],
                            "kind": command["kind"],
                            "result": result,
                        })
                        .to_string()
                        .into(),
                    ))
                    .await
                    .unwrap();
            }
        }
    }
    socket
}

/// #2234 CP7b — the three internal profile-MCP faces reach the REAL daemon wire, each as
/// its OWN distinct correlated command, over the ONE `ProductionAgentProfileMcpDataPlane`
/// the composition builds from `daemon_runtime.agent_profile_mcp_data_plane(host_agent_store)`.
///
/// `RUNTIME_PERIMETER_DOORS` already proves the three `/profile/mcp[/probe]` paths are
/// MOUNTED and refuse an anonymous caller. What no anonymous probe can show is that the
/// handlers BEHIND that perimeter dispatch a correlated RPC to the agent's OWNING host —
/// the right command kind, carrying the pinned self's id and the exact payload — and
/// project the daemon's typed answer. So this attaches a REAL daemon over the mounted
/// `HOST_WEBSOCKET_PATH`, mints a bearer through the SAME `Arc`-backed authority the
/// perimeter holds (the `#[cfg(test)]` `test_runtime_credentials` handle), and drives all
/// three faces end to end, over two layers of composition evidence:
///
///   1. **Positive wire — attached host.** `agent-1` lives on the attached host
///      `h-online`. `GET` crosses as `agent_profile_mcp_read`, `POST` as
///      `agent_profile_mcp_write` (carrying the verbatim raw), and probe as `mcp_probe`
///      (carrying `source.agent_id` + `server_name` + `include_tools`). Each command is
///      recorded with its kind and params, the daemon answers the codec-exact result, and
///      the HTTP response projects it (`{raw}`, `{ok,bytes_written}`, `{ok,phase,…}`).
///      Exactly THREE distinct commands cross — proving the faces do not collapse onto one
///      leaf and pass falsely green, and each carries agent-1's own coordinates.
///   2. **Zero wire — detached host.** `agent-2` lives on `h-detached`, a host with NO
///      live attachment. Its three faces each resolve the live per-host registry, miss,
///      and become `HostAttachPending` → 503; the attached daemon records ZERO new
///      commands. A no-op/canned stub would 200; a mis-route onto the one attached leaf
///      would record an agent-2 command. Neither happens.
///
/// The stale-generation falsifier (capture → re-resolve → dispatch reads the live
/// attachment twice and binds the second read as `expected`) is NOT borne here: HTTP
/// composition 无稳定 interleaving seam，故由 lower-boundary discriminator 承重 — the named
/// backend-server unit tests `profile_mcp_commands_are_typed_host_bound_and_generation_bound`
/// (generation/instance bind) and
/// `profile_mcp_persisted_refusals_bind_source_before_registry_or_deletion` (source-bind
/// and wrong-subject zero-registry) own it. This gate asserts the MOUNT + authenticated,
/// host-resolved wiring, not a second reading of those leaf semantics.
#[tokio::test]
async fn the_composition_internal_profile_mcp_family_reaches_the_daemon_wire() {
    use std::{
        convert::Infallible,
        future::Future,
        sync::{Arc, Mutex},
    };

    use silverhand_backend_core::{
        foundation::identity::{ConversationId, DaemonInstanceId},
        host_runtime::runtime_credential::RuntimeScope,
    };
    use silverhand_backend_server::host_runtime::runtime_credential::{
        RuntimeDispatchFailure, RuntimeStartHandoff, RuntimeStartTransport,
    };

    let fixture = fixture(true).await;
    seed_human(&fixture.path).await;
    let pool = SqlitePoolOptions::new()
        .connect(fixture.path.to_str().unwrap())
        .await
        .unwrap();
    // `h-online` is the ATTACHED host (its credential admits the daemon below); `h-detached`
    // is a real host row with NO credential and thus NO live attachment. `agent-1` lives on
    // the attached host, `agent-2` on the detached one; both are `session_scope='agent'` so
    // the `agents_create_agent_runtime_slot` trigger yields a claimable slot for
    // `dispatch_start` to mint a self-pinned bearer against.
    sqlx::query(
        "INSERT INTO hosts (id, name, kind, created_at, user_id) \
             VALUES ('h-online', 'host-online', 'remote', 1, 'user-1')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
            "INSERT INTO host_credentials (id, host_id, token_hash, kind, expires_at, consumed_at, revoked_at) \
             VALUES ('cred-online', 'h-online', ?, 'long_lived', NULL, NULL, NULL)",
        )
        .bind(HOST_TOKEN_HASH)
        .execute(&pool)
        .await
        .unwrap();
    for statement in [
        "INSERT INTO hosts (id, name, kind, created_at, user_id) \
             VALUES ('h-detached', 'host-detached', 'remote', 1, 'user-1')",
        "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, \
             created_by_user_id, materialization_status, session_scope) \
             VALUES ('agent-1', 'agent-one', 'qoder', 'h-online', 1, 1, 'user-1', 'active', 'agent')",
        "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, \
             created_by_user_id, materialization_status, session_scope) \
             VALUES ('agent-2', 'agent-two', 'qoder', 'h-detached', 1, 1, 'user-1', 'active', 'agent')",
    ] {
        sqlx::query(statement).execute(&pool).await.unwrap();
    }
    pool.close().await;

    let app = prepare(&config(fixture.path.clone())).await.unwrap();

    // Mint a REAL self-pinned bearer for each subject through the SAME `Arc`-backed
    // authority the mounted perimeter holds, BEFORE `serve` consumes `app`. The custody's
    // daemon instance is irrelevant to routing — the data plane re-resolves the agent's
    // OWNING host from the live registry — so the bearer only carries the self-pin `{aid}`.
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
    let credentials = app.test_runtime_credentials.clone();
    let mint = |agent: &str, instance: &str| {
        let agent = agent.to_owned();
        let instance = instance.to_owned();
        let credentials = &credentials;
        async move {
            let transport = CapturingStart::default();
            credentials
                .dispatch_start(
                    RuntimeScope::AgentSlot {
                        agent: AgentId::parse(&agent).unwrap(),
                    },
                    DaemonInstanceId::parse(&instance).unwrap(),
                    ConversationId::parse(CONVERSATION_ID).unwrap(),
                    &transport,
                )
                .await
                .unwrap();
            format!("Bearer {}", transport.0.lock().unwrap().clone().unwrap())
        }
    };

    let router = app.router.clone();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let running = tokio::spawn(serve(listener, app, async move {
        let _ = shutdown_rx.await;
        Ok::<(), std::io::Error>(())
    }));

    // Attach a REAL daemon for `h-online` over the mounted host WS.
    let mut ws_request = format!("ws://{address}{HOST_WEBSOCKET_PATH}")
        .into_client_request()
        .unwrap();
    ws_request.headers_mut().insert(
        tokio_tungstenite::tungstenite::http::header::AUTHORIZATION,
        format!("Bearer {HOST_TOKEN}").parse().unwrap(),
    );
    let (mut socket, _) = connect_async(ws_request).await.unwrap();
    socket
        .send(ClientMessage::Text(
            serde_json::json!({
                "direction": "daemon_to_server",
                "type": "event",
                "kind": "ready",
                "protocol_version": OUTBOUND_PROTOCOL_VERSION,
                "daemon_version": "test",
                "daemon_id": "daemon-cp7b",
                "instance_id": "instance-cp7b",
                "host_descriptor": { "name": "CP7b host", "kind": "remote" },
                "capabilities": [],
                "daemon_pid": 7,
                "started_at": 100,
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

    let seen = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let stop = Arc::new(tokio::sync::Notify::new());
    let promoted = Arc::new(tokio::sync::Notify::new());
    let servicer = tokio::spawn(service_mcp_commands(
        socket,
        stop.clone(),
        promoted.clone(),
        seen.clone(),
    ));

    // Mint the self-pinned bearers only AFTER the host is promoted (the `host_status`
    // probe answered), which the attach flow fires once `hydrate()` has returned — past
    // the finalize pass that rotates any custody persisted-for-host the daemon's empty
    // `runtime_status` did not list as active. `agent-2` lives on the never-attached
    // `h-detached`, so no hydration ever touches its slot.
    promoted.notified().await;
    let bearer_1 = mint("agent-1", "daemon-1").await;
    let bearer_2 = mint("agent-2", "daemon-2").await;

    // ---- (1) positive wire: GET → read command, projected. A bounded poll absorbs the
    // brief window between promotion and the attachment becoming resolvable for dispatch;
    // a mount that never reaches the live transport reddens by timeout. A pre-resolve miss
    // is HostAttachPending (503) and faults BEFORE dispatch, recording no command.
    let raw = "{\n  \"mcpServers\": {}\n}";
    let mut read_body = String::new();
    for _ in 0..200 {
        let (status, body) = request_with_body(
            &router,
            "GET",
            "/internal/agents/agent-1/profile/mcp",
            Some(&bearer_1),
            Body::empty(),
        )
        .await;
        if status == StatusCode::OK {
            read_body = body;
            break;
        }
        assert_eq!(
            status,
            StatusCode::SERVICE_UNAVAILABLE,
            "before the attachment resolves the read is HostAttachPending, never another status: {body}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        !read_body.is_empty(),
        "the read never reached the attached daemon — the mount did not carry the face to the live per-host transport"
    );
    let read_body: serde_json::Value = serde_json::from_str(&read_body).unwrap();
    assert_eq!(
        read_body,
        serde_json::json!({ "raw": "{\"mcpServers\":{}}" }),
        "GET projects the daemon's read result verbatim: {read_body}"
    );

    // POST → write command carrying the verbatim raw, projected.
    let (status, body) = request_with_body(
        &router,
        "POST",
        "/internal/agents/agent-1/profile/mcp",
        Some(&bearer_1),
        Body::from(raw),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the write reaches the daemon: {body}"
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({ "ok": true, "bytes_written": 128 }),
        "POST projects the daemon's byte count: {body}"
    );

    // Probe → probe command carrying source + payload, projected.
    let (status, body) = json_request_with_body(
        &router,
        "POST",
        "/internal/agents/agent-1/profile/mcp/probe",
        Some(&bearer_1),
        Body::from(serde_json::json!({ "server_name": "srv", "include_tools": true }).to_string()),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the probe reaches the daemon: {body}"
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({
            "ok": true,
            "phase": "tools",
            "server_info": { "name": "srv" },
            "tools": [ { "name": "t1", "description": "does a thing" } ],
        }),
        "probe projects the daemon's outcome: {body}"
    );

    // Each face crossed as its OWN distinct command with agent-1's exact coordinates.
    let recorded = seen.lock().unwrap().clone();
    let read_cmd = recorded
        .iter()
        .find(|c| c["kind"] == "agent_profile_mcp_read")
        .expect("the read command crossed the wire");
    assert_eq!(
        read_cmd["params"],
        serde_json::json!({ "agent_id": "agent-1" }),
        "the read carries the pinned self's id: {read_cmd}"
    );
    let write_cmd = recorded
        .iter()
        .find(|c| c["kind"] == "agent_profile_mcp_write")
        .expect("the write command crossed the wire");
    assert_eq!(
        write_cmd["params"],
        serde_json::json!({ "agent_id": "agent-1", "raw": raw }),
        "the write carries the verbatim raw for the pinned self: {write_cmd}"
    );
    let probe_cmd = recorded
        .iter()
        .find(|c| c["kind"] == "mcp_probe")
        .expect("the probe command crossed the wire");
    assert_eq!(
        probe_cmd["params"],
        serde_json::json!({
            "source": { "kind": "profile", "agent_id": "agent-1" },
            "server_name": "srv",
            "include_tools": true,
        }),
        "the probe carries the profile source + payload for the pinned self: {probe_cmd}"
    );
    assert_eq!(
        recorded.len(),
        3,
        "exactly three DISTINCT commands crossed — no face collapsed onto another's leaf: {recorded:?}"
    );

    // ---- (2) zero wire: agent-2's owning host is detached. Every face resolves the live
    // per-host registry, misses, and is HostAttachPending 503 — and NO new command crosses
    // the attached daemon (no no-op success, no mis-route onto the one live leaf).
    for (method, path, body, json_ct) in [
        (
            "GET",
            "/internal/agents/agent-2/profile/mcp",
            Body::empty(),
            false,
        ),
        (
            "POST",
            "/internal/agents/agent-2/profile/mcp",
            Body::from("{\"mcpServers\":{}}"),
            false,
        ),
        (
            "POST",
            "/internal/agents/agent-2/profile/mcp/probe",
            Body::from(serde_json::json!({ "server_name": "srv" }).to_string()),
            true,
        ),
    ] {
        // The probe reaches the handler through the frozen `ValidatedJson` admission
        // (#2242/#2246), so its leg must carry the JSON content type to be classified as
        // HostAttachPending 503 rather than a pre-handler 415; the raw/verbatim write and
        // the bodiless read are content-type agnostic and keep the plain harness call.
        let (status, body) = if json_ct {
            json_request_with_body(&router, method, path, Some(&bearer_2), body).await
        } else {
            request_with_body(&router, method, path, Some(&bearer_2), body).await
        };
        assert_eq!(
            status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{method} {path} on a detached host is HostAttachPending 503: {body}"
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["code"],
            "host_attach_pending",
            "the detached-host fault is host_attach_pending: {body}"
        );
    }
    assert_eq!(
        seen.lock().unwrap().len(),
        3,
        "still exactly the three agent-1 commands — every detached-host face dispatched nothing: {:?}",
        seen.lock().unwrap()
    );

    stop.notify_one();
    drop(servicer.await.unwrap());
    shutdown_tx.send(()).unwrap();
    running.await.unwrap().unwrap();
}

/// Seed one host `h-online` (owned by the seeded `user-1`) with a long-lived
/// credential whose bearer is `HOST_TOKEN`, so a REAL daemon can attach over
/// `HOST_WEBSOCKET_PATH`. Each `(agent_id, marker)` is an ACTIVE agent on that host:
/// `Some(at)` pre-stamps `deletion_started_at` — a durable deletion marker with no
/// request behind it, exactly the shape a prior boot's begin left on disk — and `None`
/// leaves the row deletable by a fresh request.
async fn seed_deletable_agents(path: &Path, agents: &[(&str, Option<i64>)]) {
    let pool = SqlitePoolOptions::new()
        .connect(path.to_str().unwrap())
        .await
        .unwrap();
    sqlx::query(
            "INSERT INTO hosts (id, name, kind, created_at, user_id) VALUES ('h-online', 'host-online', 'remote', 1, 'user-1')",
        )
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
            "INSERT INTO host_credentials (id, host_id, token_hash, kind, expires_at, consumed_at, revoked_at) VALUES ('cred-online', 'h-online', ?, 'long_lived', NULL, NULL, NULL)",
        )
        .bind(HOST_TOKEN_HASH)
        .execute(&pool)
        .await
        .unwrap();
    for (id, marker) in agents {
        sqlx::query(
                "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, created_by_user_id, materialization_status, deletion_started_at) \
                 VALUES (?, ?, 'qoder', 'h-online', 1, 1, 'user-1', 'active', ?)",
            )
            .bind(id)
            .bind(format!("agent-{id}"))
            .bind(marker)
            .execute(&pool)
            .await
            .unwrap();
    }
    pool.close().await;
}

/// Drive a connected daemon socket like a REAL daemon until told to stop, then hand the
/// socket back for a clean close: answer the `runtime_status` hydration command and
/// every `agent_files_purge` the production deletion fence sends
/// (`{"ok":true,"purged_count":0,"quiesced_count":0}` — the exact shape
/// `valid_agent_files_purge_result` admits). A composed deletion COMMITS only once this
/// purge round-trip is answered, so a mount whose resumer never reaches the live daemon
/// transport would leave the marker un-purged and redden by the bounded GET poll below.
async fn service_daemon_commands<S>(
    mut socket: tokio_tungstenite::WebSocketStream<S>,
    stop: std::sync::Arc<tokio::sync::Notify>,
) -> tokio_tungstenite::WebSocketStream<S>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    loop {
        tokio::select! {
            () = stop.notified() => break,
            command = next_daemon_json(&mut socket) => {
                let result = match command["kind"].as_str() {
                    Some("runtime_status") => serde_json::json!({ "runtimes": [] }),
                    // The status-seed layer fires one background `host_status` probe at
                    // the promotion edge (decoupled from attach; a bad answer only logs).
                    // Answer the exact `StatusWire` contract so the seed seals in one
                    // round rather than deferring into a retry loop.
                    Some("host_status") => serde_json::json!({
                        "os": "linux",
                        "daemon_pid": 1,
                        "daemon_version": "0.0.0-test",
                        "started_at": 0,
                        "uptime_ms": 0,
                        "active_agent_ids": [],
                    }),
                    Some("agent_files_purge") => {
                        serde_json::json!({ "ok": true, "purged_count": 0, "quiesced_count": 0 })
                    }
                    other => panic!("unexpected daemon command kind: {other:?}"),
                };
                socket
                    .send(ClientMessage::Text(
                        serde_json::json!({
                            "direction": "daemon_to_server",
                            "type": "command_response",
                            "id": command["id"],
                            "kind": command["kind"],
                            "result": result,
                        })
                        .to_string()
                        .into(),
                    ))
                    .await
                    .unwrap();
            }
        }
    }
    socket
}

/// Bounded-poll the composed agent-detail GET until the row is GONE — the witness that
/// a durable deletion COMMITTED end-to-end (`204`-worthy row teardown, not merely a
/// marker write). `200` while present, then the detail route's own anti-enumeration
/// `404 {"error":"agent not found"}` once the resumer's fence→complete chain deletes the
/// row. Bounded so broken wiring reddens by timeout instead of hanging.
async fn poll_agent_deleted(router: &Router, agent_id: &str, bearer: &str) {
    let uri = format!("/api/agents/{agent_id}");
    for _ in 0..200 {
        let (status, body) = request(router, "GET", &uri, Some(bearer)).await;
        if status == StatusCode::NOT_FOUND {
            assert_eq!(
                body, r#"{"error":"agent not found"}"#,
                "a completed deletion collapses to the detail route's anti-enum 404"
            );
            return;
        }
        assert_eq!(
            status,
            StatusCode::OK,
            "{agent_id} present until deleted: {body}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!(
        "{agent_id} was never deleted — the composed resumer did not carry the marker to a committed teardown"
    );
}

/// The #2234 CP3 close: `DELETE /api/agents/{id}` is not just mounted — the ONE shared
/// production [`AgentDeletionResumer`] the composition builds is reached from every
/// entrypoint that must carry a durable deletion marker to its committed end. This is
/// the composition obligation fixed for the mount (not the resumer's own
/// scheduling semantics, which its component tests own), reduced to three named
/// falsifiers over a REAL daemon attaching through the mounted `HOST_WEBSOCKET_PATH`:
///
/// 1. **Boot `recover` + exact host-ready, no request.** A durable marker pre-exists on
///    disk (a prior boot's begin). `prepare()` runs `recover().await` fail-closed BEFORE
///    serving, which arms the marker but drives NO purge while the host is detached; only
///    when the daemon completes its real `attach Ok(true)` does the mandatory
///    `HostReadyObserver` push the exact generation to the resumer, which fences and
///    commits the teardown. The agent's row vanishes with no HTTP request ever sent.
///    Un-wiring `recover` (marker never armed) OR the host-ready observer (attach never
///    reaches the resumer) leaves the row and reddens the poll.
/// 2. **Composed `DELETE` request → `try_now` → committed teardown.** An
///    authenticated `DELETE /api/agents/{id}` with `{"confirm":true}` on the SAME
///    attached host writes its begin marker and dispatches the SAME resumer's `try_now`,
///    whose fence→purge→complete commits synchronously and answers `204 No Content`. A
///    route wired to a SECOND resumer instance, or to none, could never return `204` here
///    (it would `202`-retain, having no live generation of its own) — proving the mounted
///    route and the boot/host-ready worker are ONE instance sharing one `Arc` of state.
/// 3. **Shutdown `close`, durable retention, next-boot recovery.** A marker whose host
///    is NEVER attached stays pending across a full production shutdown. Boot 1 holds a
///    clone of the SAME production resumer instance and, AFTER `serve`'s shutdown arm
///    runs, drives a real begin-minted `try_now` against it: the arm's synchronous
///    `resumer.close()` (ahead of transport teardown) is what makes that attempt resolve
///    to `Closed`. Delete the `close()` line from `serve` and the detached-host attempt
///    resolves to `Retained` instead — the gate reddens on the wiring itself, not merely
///    on a panic. The durable marker is also NOT lost: a fresh `prepare()` over the same
///    store re-lists it, and once its host finally attaches the deletion completes. The
///    resumer's `close_stops_the_ladder_and_refuses_new_work` component test still owns
///    the abort-the-timers internals; this proves the composed shutdown path invokes
///    close on the one shared instance and never strands the fail-forward marker.
#[tokio::test]
async fn the_composition_agent_deletion_resumer_drives_the_full_production_lifecycle() {
    // ── Falsifiers 1 & 2 share one served app on one attached host. ──
    let fixture1 = fixture(true).await;
    seed_human(&fixture1.path).await;
    // `a-marked` carries a pre-stored durable marker (no request behind it);
    // `a-live` is a plain active agent deleted through the composed HTTP route.
    seed_deletable_agents(
        &fixture1.path,
        &[("a-marked", Some(7_000)), ("a-live", None)],
    )
    .await;
    let app = prepare(&config(fixture1.path.clone())).await.unwrap();
    let router = app.router.clone();
    let bearer = format!("Bearer {TOKEN}");

    // Baseline: both rows are present and reachable BEFORE any daemon attaches. The
    // pre-stored marker has driven nothing — `recover` armed it, but its host is
    // detached, so the row still answers `200`.
    for agent in ["a-marked", "a-live"] {
        let (status, body) = request(
            &router,
            "GET",
            &format!("/api/agents/{agent}"),
            Some(&bearer),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{agent} present pre-attach: {body}");
    }

    // Serve over real TCP so a real daemon can attach through the mounted host WS.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let running = tokio::spawn(serve(listener, app, async move {
        let _ = shutdown_rx.await;
        Ok::<(), std::io::Error>(())
    }));

    // Drive the REAL admission→attachment handshake.
    let mut ws_request = format!("ws://{address}{HOST_WEBSOCKET_PATH}")
        .into_client_request()
        .unwrap();
    ws_request.headers_mut().insert(
        tokio_tungstenite::tungstenite::http::header::AUTHORIZATION,
        format!("Bearer {HOST_TOKEN}").parse().unwrap(),
    );
    let (mut socket, _) = connect_async(ws_request).await.unwrap();
    socket
        .send(ClientMessage::Text(
            serde_json::json!({
                "direction": "daemon_to_server",
                "type": "event",
                "kind": "ready",
                "protocol_version": OUTBOUND_PROTOCOL_VERSION,
                "daemon_version": "test",
                "daemon_id": "daemon-cp3",
                "instance_id": "instance-cp3",
                "host_descriptor": { "name": "CP3 host", "kind": "remote" },
                "capabilities": [],
                "daemon_pid": 7,
                "started_at": 100,
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

    // A background servicer answers the hydration command AND every deletion purge, so
    // the push-driven marker and the request-driven delete both make wire progress.
    let stop = std::sync::Arc::new(tokio::sync::Notify::new());
    let servicer = tokio::spawn(service_daemon_commands(socket, stop.clone()));

    // ── Falsifier 1: attach alone (boot recover + exact host-ready) completes the
    // pre-stored marker, with NO HTTP request. ──
    poll_agent_deleted(&router, "a-marked", &bearer).await;

    // ── Falsifier 2: the composed DELETE dispatches the SAME resumer and commits
    // synchronously → 204. Spawned so the servicer answers its purge concurrently. ──
    let del_router = router.clone();
    let del_bearer = bearer.clone();
    let deletion = tokio::spawn(async move {
        request_with_body(
            &del_router,
            "DELETE",
            "/api/agents/a-live",
            Some(&del_bearer),
            Body::from(r#"{"confirm":true}"#),
        )
        .await
    });
    let (status, body) = deletion.await.unwrap();
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "the composed DELETE drives the shared resumer to a committed teardown (204): {body}"
    );
    // The row is gone through the SAME detail read.
    let (status, body) = request(&router, "GET", "/api/agents/a-live", Some(&bearer)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "a-live deleted: {body}");
    assert_eq!(body, r#"{"error":"agent not found"}"#);

    // Wind the servicer down and shut the app cleanly.
    stop.notify_one();
    let mut socket = servicer.await.unwrap();
    socket.close(None).await.unwrap();
    shutdown_tx.send(()).unwrap();
    running.await.unwrap().unwrap();

    // ── Falsifier 3: a marker whose host never attaches survives a full production
    // shutdown (close ahead of transport teardown), and the NEXT boot recovers it.
    // Boot 1 ALSO witnesses that the production shutdown arm actually calls
    // `resumer.close()` — not merely that the marker persists. ──
    let fixture3 = fixture(true).await;
    seed_human(&fixture3.path).await;
    // `a-retained` proves marker survival + next-boot recovery; `a-witness` is the
    // close witness — an active agent whose real begin-marker mints a genuine
    // `AgentDeletionAttempt` (the token's constructor is sealed to the marker
    // authority, so this is the only way to hold one).
    seed_deletable_agents(
        &fixture3.path,
        &[("a-retained", Some(9_000)), ("a-witness", None)],
    )
    .await;

    // Boot 1: recover arms the marker, but the detached host drives no purge, so the
    // row stays. Shut down through the production arm, which runs `resumer.close()`
    // synchronously before transport teardown.
    let boot1 = prepare(&config(fixture3.path.clone())).await.unwrap();
    let router1 = boot1.router.clone();
    // The SAME production resumer instance the shutdown arm will close. It shares the
    // one `Arc<Mutex<Tracked>>`, so a `close()` inside `serve` flips this clone too.
    let close_probe = boot1.deletion_resumer.clone();
    let (status, body) = request(&router1, "GET", "/api/agents/a-retained", Some(&bearer)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a detached host's marker completes nothing at boot: {body}"
    );
    // Mint a REAL attempt through the production begin-marker authority (the exact
    // path the DELETE route uses). The host is detached in boot 1, so before the
    // shutdown close this attempt would only ever resolve to `Retained` (no live
    // generation to fence against) — never `Closed`.
    let lifecycle = SqliteHostAgentStore::open(&fixture3.path).await.unwrap();
    let witness_attempt =
        match begin_agent_deletion(&lifecycle, &AgentId::parse("a-witness").unwrap(), 12_000)
            .await
            .unwrap()
        {
            BeginDeletion::Started(attempt) | BeginDeletion::AlreadyPending(attempt) => attempt,
            other => panic!("expected a minted deletion attempt, got {other:?}"),
        };
    let listener1 = TcpListener::bind("127.0.0.1:0").await.unwrap();
    drop(lifecycle); // The test-only minting owner is not part of boot 1's lifecycle.
    let (stop1_tx, stop1_rx) = oneshot::channel();
    let boot1_running = tokio::spawn(serve(listener1, boot1, async move {
        let _ = stop1_rx.await;
        Ok::<(), std::io::Error>(())
    }));
    stop1_tx.send(()).unwrap();
    boot1_running.await.unwrap().unwrap();
    drop(router1);

    // The close witness: after the production shutdown arm ran, the SAME resumer
    // rejects new work with `Closed`. This is the falsifier for the wiring itself —
    // delete the `deletion_resumer.close()` line from `serve`'s shutdown arm and this
    // attempt resolves to `Retained` (detached host) instead, reddening the gate. A
    // marker-persistence check alone (below) cannot catch that regression.
    assert_eq!(
        close_probe.try_now(witness_attempt).await,
        DeletionRunOutcome::Closed,
        "the production shutdown arm must have closed the shared resumer"
    );

    // Boot 2 over the SAME store: the durable marker survived shutdown, so recover
    // re-lists it; once the host attaches the deletion finally commits.
    drop(close_probe);
    // A real process exit releases every connection synchronously. This in-process
    // test must also await aborted workers/SQLx pool-drop cleanup before simulating
    // the next process. Witness the actual exclusive lock, not an arbitrary sleep.
    {
        use sqlx::Connection as _;
        let quiesced = sqlx::SqliteConnection::connect_with(
            &sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&fixture3.path)
                .locking_mode(sqlx::sqlite::SqliteLockingMode::Exclusive)
                .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
                .busy_timeout(std::time::Duration::from_secs(5)),
        )
        .await
        .unwrap();
        quiesced.close().await.unwrap();
    }
    let boot2 = prepare(&config(fixture3.path.clone())).await.unwrap();
    let router2 = boot2.router.clone();
    let (status, body) = request(&router2, "GET", "/api/agents/a-retained", Some(&bearer)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the durable marker survived boot-1 shutdown for the next boot: {body}"
    );
    let listener2 = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address2 = listener2.local_addr().unwrap();
    let (shutdown2_tx, shutdown2_rx) = oneshot::channel();
    let boot2_running = tokio::spawn(serve(listener2, boot2, async move {
        let _ = shutdown2_rx.await;
        Ok::<(), std::io::Error>(())
    }));
    let mut ws2 = format!("ws://{address2}{HOST_WEBSOCKET_PATH}")
        .into_client_request()
        .unwrap();
    ws2.headers_mut().insert(
        tokio_tungstenite::tungstenite::http::header::AUTHORIZATION,
        format!("Bearer {HOST_TOKEN}").parse().unwrap(),
    );
    let (mut socket2, _) = connect_async(ws2).await.unwrap();
    socket2
        .send(ClientMessage::Text(
            serde_json::json!({
                "direction": "daemon_to_server",
                "type": "event",
                "kind": "ready",
                "protocol_version": OUTBOUND_PROTOCOL_VERSION,
                "daemon_version": "test",
                "daemon_id": "daemon-cp3-boot2",
                "instance_id": "instance-cp3-boot2",
                "host_descriptor": { "name": "CP3 host", "kind": "remote" },
                "capabilities": [],
                "daemon_pid": 7,
                "started_at": 100,
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
    let stop2 = std::sync::Arc::new(tokio::sync::Notify::new());
    let servicer2 = tokio::spawn(service_daemon_commands(socket2, stop2.clone()));
    poll_agent_deleted(&router2, "a-retained", &bearer).await;
    stop2.notify_one();
    let mut socket2 = servicer2.await.unwrap();
    socket2.close(None).await.unwrap();
    shutdown2_tx.send(()).unwrap();
    boot2_running.await.unwrap().unwrap();
}

/// Seed the topology the CP4 runtime-control gate drives: one online host (both
/// `activated_at` and `finalized_at` set, so its lifecycle resolves to `Active` — the
/// placement gate `clear_session`/switch enforce) with a `long_lived` credential whose hash
/// is `HOST_TOKEN_HASH` (so the same daemon the other gates use can attach), plus
///
/// * `a-clear` — an AGENT-scope agent (its AgentSlot is the subject of `POST /clear-session`),
/// * `a-switch` — a CONVERSATION-scope member of `conv-cp4` (its ConversationSlot is the
///   subject of `PUT /runtime`; a live conversation seat is always re-driven after a
///   committed switch, so the post-commit `runtime_start` re-drive is guaranteed to dispatch).
///
/// Their runtime slots are left IDLE here (auto-created, no claimed generation), so they are
/// NOT current custodies at daemon-attach hydration and survive it untouched. The gate makes
/// them LIVE via [`activate_runtime_control_slots`] only AFTER the daemon has attached and
/// hydrated — a slot claimed before attach would be reconciled to idle (finalized `Crashed`)
/// because the freshly-attached daemon reports no active runtime for it.
async fn seed_runtime_control_agents(path: &Path) {
    let pool = SqlitePoolOptions::new()
        .connect(path.to_str().unwrap())
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO hosts (id, name, kind, created_at, user_id, activated_at, finalized_at) \
             VALUES ('h-online', 'host-online', 'remote', 1, 'user-1', 1, 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
            "INSERT INTO host_credentials (id, host_id, token_hash, kind, expires_at, consumed_at, revoked_at) \
             VALUES ('cred-online', 'h-online', ?, 'long_lived', NULL, NULL, NULL)",
        )
        .bind(HOST_TOKEN_HASH)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
            "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, display_name, \
                 created_by_user_id, materialization_status, session_scope) VALUES \
                 ('a-clear', 'a-clear', 'claude', 'h-online', 1, 1, 'Clear', 'user-1', 'active', 'agent'), \
                 ('a-switch', 'a-switch', 'claude', 'h-online', 1, 1, 'Switch', 'user-1', 'active', 'conversation')",
        )
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
            "INSERT INTO workspaces (id, name, name_key, owner_user_id, created_at, updated_at) \
                 VALUES ('ws-cp4', 'Space', 'space', 'user-1', 0, 0); \
             INSERT INTO conversations (id, workspace_id, placement_kind, roster_policy, title, title_key, \
                 status, visibility, created_at, updated_at, latest_message_seq) \
                 VALUES ('conv-cp4', 'ws-cp4', 'workspace', 'open', 'General', 'general', 'open', 'private', 0, 0, 1); \
             INSERT INTO conversation_agents (conversation_id, agent_id, joined_at) \
                 VALUES ('conv-cp4', 'a-switch', 0);",
        )
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}

/// Claim both seeded runtime slots LIVE (`observed_status='active'`, generation `1`, owned by
/// `instance-cp4` — the instance the gate's daemon announces) with an exact CLI session id.
/// Called AFTER attach so hydration cannot reconcile them away; from here the switch's seat
/// read sees a genuinely live conversation seat (re-driven post-commit) and clear-session sees
/// a live agent seat (`was_running`).
async fn activate_runtime_control_slots(path: &Path) {
    let pool = SqlitePoolOptions::new()
        .connect(path.to_str().unwrap())
        .await
        .unwrap();
    sqlx::query(
            "UPDATE agent_runtime_slots SET generation=1, claimed_by_daemon_instance_id='instance-cp4', \
                 observed_status='active', cli_session_id='session-clear' \
               WHERE agent_id='a-clear' AND conversation_id IS NULL; \
             UPDATE agent_runtime_slots SET generation=1, claimed_by_daemon_instance_id='instance-cp4', \
                 observed_status='active', cli_session_id='session-switch' \
               WHERE agent_id='a-switch' AND conversation_id='conv-cp4';",
        )
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}

/// Drive a connected daemon socket like a REAL daemon for the runtime-control faces:
/// answer hydration (`runtime_status`), the promotion-edge `host_status` probe (and
/// signal `attached` once, so the gate can wait for the host to be `Active` before it
/// dispatches), the forced availability `host_runtime_catalog` (target `codex` PRESENT,
/// so the switch clears its deterministic-absence gate), every `runtime_clear_session`
/// (`{"cleared":false}` — the already-idle daemon answer that still drives the real
/// control through custody `finalize` → durable `settle_clear`), and every post-commit
/// `runtime_start` with a decode-rejecting `{"started":false}` so the switch's re-drive
/// counts a `restart_failure` WITHOUT rolling back the committed transition.
async fn service_runtime_control_commands<S>(
    mut socket: tokio_tungstenite::WebSocketStream<S>,
    stop: std::sync::Arc<tokio::sync::Notify>,
    attached: std::sync::Arc<tokio::sync::Notify>,
) -> tokio_tungstenite::WebSocketStream<S>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    loop {
        tokio::select! {
            () = stop.notified() => break,
            command = next_daemon_json(&mut socket) => {
                let result = match command["kind"].as_str() {
                    Some("runtime_status") => serde_json::json!({ "runtimes": [] }),
                    Some("host_status") => {
                        // The promotion edge: custody is attached by the time this fires,
                        // so it is the signal the gate waits on before dispatching.
                        attached.notify_one();
                        serde_json::json!({
                            "os": "linux",
                            "daemon_pid": 1,
                            "daemon_version": "0.0.0-test",
                            "started_at": 0,
                            "uptime_ms": 0,
                            "active_agent_ids": [],
                        })
                    }
                    // Forced availability probe: `codex` is PRESENT, so the transition's
                    // deterministic-absence gate defers to the daemon spawn last-line.
                    Some("host_runtime_catalog") => serde_json::json!({
                        "runtimes": [ {
                            "kind": "codex",
                            "state": "present",
                            "reason": "resolved",
                            "installed": true,
                            "binary": "/usr/bin/codex",
                            "version": "1.0.0",
                        } ],
                        "detected_at": 1_700_000_000_000_i64,
                    }),
                    // The already-idle clear answer: nothing was awaited on an exit, but the
                    // real control still finalizes custody and durably settles the seat.
                    Some("runtime_clear_session") => serde_json::json!({ "cleared": false }),
                    // A decode-rejecting start result: the post-commit re-drive counts one
                    // `restart_failure` and NEVER reverts the committed switch.
                    Some("runtime_start") => serde_json::json!({ "started": false }),
                    other => panic!("unexpected daemon command kind: {other:?}"),
                };
                socket
                    .send(ClientMessage::Text(
                        serde_json::json!({
                            "direction": "daemon_to_server",
                            "type": "command_response",
                            "id": command["id"],
                            "kind": command["kind"],
                            "result": result,
                        })
                        .to_string()
                        .into(),
                    ))
                    .await
                    .unwrap();
            }
        }
    }
    socket
}

/// The #2234 CP4 close: `POST /api/agents/{id}/clear-session` and `PUT
/// /api/agents/{id}/runtime` are not merely mounted behind the human perimeter — each
/// reaches the ONE `ProductionRuntimeControl` the composition builds (constructed once
/// in `prepare()` and cloned to both routes) and drives it against a REAL daemon
/// attaching through the mounted host WS. This is the composition/reachability
/// obligation (A6 no-op-mount lesson), reduced to two product facts over one attached
/// host; delete EITHER `.merge(...)` from `prepare()`'s human chain and the matching
/// fact reddens (a `404`/`405`, never these bodies):
///
/// 1. **clear-session reaches command → durable settle/audit.** `a-clear` owns a live
///    claimed AgentSlot. The composed route reads that live seat through the production
///    control (proved by `was_running:true` + the exact seeded `prev_cli_session_id`),
///    passes the `Active` placement gate, dispatches the real `runtime_clear_session`
///    command to the attached daemon (the servicer must answer it — a no-op mount could
///    not), and — on the already-idle `{"cleared":false}` answer — finalizes custody and
///    durably settles, returning `200 {"ok":true,...}`. (The exit-waiter *await* branch
///    is a `ProductionRuntimeControl` internal owned by the backend-server e2e
///    `clear_session_waiter_settles_after_the_exact_runtime_exit_frame_commits`; this
///    composition gate proves the mount reaches the real command→settle path, well
///    beyond a `401`/`405`.)
/// 2. **runtime-switch truly commits, announces once, and does not roll back.** `a-switch`
///    (runtime `claude`) is switched to `codex`. The saga probes availability
///    (`host_runtime_catalog`, codex PRESENT), clears its live ConversationSlot through
///    the SAME production control (`cleared_sessions >= 1`), commits `runtime=codex` +
///    tuning, publishes `agent_changed` EXACTLY ONCE (fresh app-scope high-water delta
///    `== 1`), then re-drives the conversation seat with a `runtime_start` the daemon
///    FAILS (`restart_failures >= 1`). The committed runtime survives that post-commit
///    failure: a fresh `GET` still reads `codex`, proving the re-drive never reverted the
///    transition.
#[tokio::test]
async fn the_composition_agent_runtime_control_faces_reach_the_shared_production_control() {
    let fixture = fixture(true).await;
    seed_human(&fixture.path).await;
    seed_runtime_control_agents(&fixture.path).await;
    let app = prepare(&config(fixture.path.clone())).await.unwrap();
    let router = app.router.clone();
    let bearer = format!("Bearer {TOKEN}");

    // Serve over real TCP so a real daemon can attach through the mounted host WS; the
    // in-process `router` clone shares the served app's state (the one production control,
    // the daemon registry), exactly as the CP3 gate relies on.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let running = tokio::spawn(serve(listener, app, async move {
        let _ = shutdown_rx.await;
        Ok::<(), std::io::Error>(())
    }));

    // Drive the REAL admission→attachment handshake for `instance-cp4` (the instance that
    // owns both seeded seats' custody).
    let mut ws_request = format!("ws://{address}{HOST_WEBSOCKET_PATH}")
        .into_client_request()
        .unwrap();
    ws_request.headers_mut().insert(
        tokio_tungstenite::tungstenite::http::header::AUTHORIZATION,
        format!("Bearer {HOST_TOKEN}").parse().unwrap(),
    );
    let (socket, _) = connect_async(ws_request).await.unwrap();
    {
        use futures_util::SinkExt;
        let mut socket = socket;
        socket
            .send(ClientMessage::Text(
                serde_json::json!({
                    "direction": "daemon_to_server",
                    "type": "event",
                    "kind": "ready",
                    "protocol_version": OUTBOUND_PROTOCOL_VERSION,
                    "daemon_version": "test",
                    "daemon_id": "daemon-cp4",
                    "instance_id": "instance-cp4",
                    "host_descriptor": { "name": "CP4 host", "kind": "remote" },
                    "capabilities": [],
                    "daemon_pid": 7,
                    "started_at": 100,
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();

        // Background servicer answers hydration + the runtime-control commands; wait for the
        // promotion-edge `host_status` so custody is attached before we dispatch.
        let stop = std::sync::Arc::new(tokio::sync::Notify::new());
        let attached = std::sync::Arc::new(tokio::sync::Notify::new());
        let servicer = tokio::spawn(service_runtime_control_commands(
            socket,
            stop.clone(),
            attached.clone(),
        ));
        tokio::time::timeout(std::time::Duration::from_secs(5), attached.notified())
            .await
            .expect("the daemon must attach (host_status probe) before the gate dispatches");

        // Claim both seats LIVE now that hydration is done — a pre-attach claim would have
        // been reconciled to idle when this daemon reported no active runtime for it.
        activate_runtime_control_slots(&fixture.path).await;

        // ── Fact 1: clear-session reaches the real command → durable settle. ──
        let (status, body) = request(
            &router,
            "POST",
            "/api/agents/a-clear/clear-session",
            Some(&bearer),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "clear-session must reach the mounted production control: {body}"
        );
        let cleared: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(cleared["ok"], true, "clear-session settled: {body}");
        assert_eq!(
            cleared["was_running"], true,
            "the route read the LIVE claimed agent seat through the production control: {body}"
        );
        assert_eq!(
            cleared["prev_cli_session_id"], "session-clear",
            "the durable settle carried the exact seeded live session: {body}"
        );

        // ── Fact 2: runtime-switch commits, announces once, and survives the restart failure. ──
        let app_high_water_before = fresh_scope_high_water(address, "app").await;
        let (status, body) = json_request_with_body(
            &router,
            "PUT",
            "/api/agents/a-switch/runtime",
            Some(&bearer),
            Body::from(
                r#"{"runtime":"codex","model":null,"reasoning_effort":null,"fast_mode":null}"#,
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "runtime-switch must reach the mounted production control: {body}"
        );
        let switched: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            switched["agent"]["runtime"], "codex",
            "the switch committed the target runtime: {body}"
        );
        assert!(
            switched["cleared_sessions"].as_u64().unwrap() >= 1,
            "the old-runtime live seat was cleared through the same production control: {body}"
        );
        assert!(
            switched["restart_failures"].as_u64().unwrap() >= 1,
            "the post-commit re-drive dispatched runtime_start and counted its failure: {body}"
        );

        // Durability: the committed transition stands despite the restart failure.
        let (status, body) = request(&router, "GET", "/api/agents/a-switch", Some(&bearer)).await;
        assert_eq!(status, StatusCode::OK, "a-switch detail: {body}");
        let detail: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            detail["agent"]["runtime"], "codex",
            "the post-commit restart failure did NOT roll back the committed runtime: {body}"
        );

        // Exactly one app-scope `agent_changed` for the whole switch.
        let app_high_water_after = fresh_scope_high_water(address, "app").await;
        assert_eq!(
            app_high_water_after - app_high_water_before,
            1,
            "runtime switch publishes agent_changed exactly once"
        );

        // Wind the servicer down and shut the app cleanly.
        stop.notify_one();
        let mut socket = servicer.await.unwrap();
        socket.close(None).await.unwrap();
    }
    shutdown_tx.send(()).unwrap();
    running.await.unwrap().unwrap();
}

/// Seed one CONVERSATION-scoped agent (`a-stop`) with THREE live workspace-conversation
/// seats on the online host, so the composed `/stop` face has a genuine multi-seat fan-out
/// to enumerate. The seat rows are NOT inserted directly: each `conversation_agents` join to
/// a `workspace`-placement conversation fires the production
/// `conversation_agents_create_runtime_slot` trigger, which is the only way a conversation
/// seat comes to exist. Reuses the CP4 online host + credential so the same real daemon
/// attach hydrates custody for `h-online`.
async fn seed_agent_stop_agent(path: &Path) {
    let pool = SqlitePoolOptions::new()
        .connect(path.to_str().unwrap())
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO hosts (id, name, kind, created_at, user_id, activated_at, finalized_at) \
             VALUES ('h-online', 'host-online', 'remote', 1, 'user-1', 1, 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
            "INSERT INTO host_credentials (id, host_id, token_hash, kind, expires_at, consumed_at, revoked_at) \
             VALUES ('cred-online', 'h-online', ?, 'long_lived', NULL, NULL, NULL)",
        )
        .bind(HOST_TOKEN_HASH)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
            "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, display_name, \
                 created_by_user_id, materialization_status, session_scope) VALUES \
                 ('a-stop', 'a-stop', 'claude', 'h-online', 1, 1, 'Stop', 'user-1', 'active', 'conversation')",
        )
        .execute(&pool)
        .await
        .unwrap();
    // Three workspace conversations; each membership trigger-materializes one seat.
    sqlx::query(
            "INSERT INTO workspaces (id, name, name_key, owner_user_id, created_at, updated_at) \
                 VALUES ('ws-stop', 'Space', 'space', 'user-1', 0, 0); \
             INSERT INTO conversations (id, workspace_id, placement_kind, roster_policy, title, title_key, \
                 status, visibility, created_at, updated_at, latest_message_seq) VALUES \
                 ('conv-s1', 'ws-stop', 'workspace', 'open', 'One', 'one', 'open', 'private', 0, 0, 1), \
                 ('conv-s2', 'ws-stop', 'workspace', 'open', 'Two', 'two', 'open', 'private', 0, 0, 1), \
                 ('conv-s3', 'ws-stop', 'workspace', 'open', 'Three', 'three', 'open', 'private', 0, 0, 1); \
             INSERT INTO conversation_agents (conversation_id, agent_id, joined_at) VALUES \
                 ('conv-s1', 'a-stop', 0), ('conv-s2', 'a-stop', 0), ('conv-s3', 'a-stop', 0);",
        )
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}

/// Claim all three trigger-materialized seats LIVE (`observed_status='active'`, generation
/// `1`) AFTER attach, so daemon hydration cannot reconcile them back to idle. From here the
/// bulk-stop snapshot reads three genuinely running conversation seats, each with a positive
/// generation the `runtime_stop` codec will accept.
async fn activate_agent_stop_slots(path: &Path) {
    let pool = SqlitePoolOptions::new()
        .connect(path.to_str().unwrap())
        .await
        .unwrap();
    sqlx::query(
            "UPDATE agent_runtime_slots SET generation=1, claimed_by_daemon_instance_id='instance-cp5', \
                 observed_status='active', cli_session_id='session-' || conversation_id \
               WHERE agent_id='a-stop' AND conversation_id IN ('conv-s1', 'conv-s2', 'conv-s3');",
        )
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}

/// Drive a connected daemon socket like a REAL daemon for the bulk-stop face: answer
/// hydration (`runtime_status`), signal `attached` on the promotion-edge `host_status`
/// probe, tolerate an attach-time `host_runtime_catalog`, and service every `runtime_stop`
/// — RECORDING each command's `runtime_scope` (so the gate can prove every seat was
/// dispatched, in scope, without short-circuit) and answering a schema-VALID
/// `{"mode":"hard","stopped":true,"exit_code":null}` for every seat EXCEPT the designated
/// poison conversation, for which it returns a schema-INVALID body so that one seat faults
/// (`ProtocolCorruption`) while the others still stop.
async fn service_agent_stop_commands<S>(
    mut socket: tokio_tungstenite::WebSocketStream<S>,
    stop: std::sync::Arc<tokio::sync::Notify>,
    attached: std::sync::Arc<tokio::sync::Notify>,
    poison_conversation: &'static str,
    stop_scopes: std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
) -> tokio_tungstenite::WebSocketStream<S>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    loop {
        tokio::select! {
            () = stop.notified() => break,
            command = next_daemon_json(&mut socket) => {
                let result = match command["kind"].as_str() {
                    Some("runtime_status") => serde_json::json!({ "runtimes": [] }),
                    Some("host_status") => {
                        attached.notify_one();
                        serde_json::json!({
                            "os": "linux",
                            "daemon_pid": 1,
                            "daemon_version": "0.0.0-test",
                            "started_at": 0,
                            "uptime_ms": 0,
                            "active_agent_ids": [],
                        })
                    }
                    Some("host_runtime_catalog") => serde_json::json!({
                        "runtimes": [],
                        "detected_at": 1_700_000_000_000_i64,
                    }),
                    Some("runtime_stop") => {
                        let scope = command["params"]["runtime_scope"].clone();
                        let is_poison = scope["conversation_id"].as_str()
                            == Some(poison_conversation);
                        stop_scopes.lock().unwrap().push(scope);
                        if is_poison {
                            // Schema-INVALID answer (missing keys ⇒ len != 3): the codec's
                            // `valid_result` rejects it ⇒ ProtocolCorruption ⇒ this seat
                            // Faulted, while the sibling seats still stop.
                            serde_json::json!({ "stopped": true })
                        } else {
                            serde_json::json!({
                                "mode": "hard",
                                "stopped": true,
                                "exit_code": null,
                            })
                        }
                    }
                    other => panic!("unexpected daemon command kind: {other:?}"),
                };
                socket
                    .send(ClientMessage::Text(
                        serde_json::json!({
                            "direction": "daemon_to_server",
                            "type": "command_response",
                            "id": command["id"],
                            "kind": command["kind"],
                            "result": result,
                        })
                        .to_string()
                        .into(),
                    ))
                    .await
                    .unwrap();
            }
        }
    }
    socket
}

/// The #2234 CP5 close: `POST /api/agents/{id}/stop` is not merely mounted behind the human
/// perimeter — it reaches the ONE scope-keyed stop transport the composition builds
/// (`daemon_runtime.scope_runtime_stop()`, bound to the daemon's own host credentials + live
/// registry) and, through the production `bulk_stop_agent_runtimes`, ENUMERATES AND ATTEMPTS
/// EVERY running seat the agent owns against a REAL daemon attaching over the mounted host
/// WS. Delete the `.merge(agent_stop::router(...))` from `prepare()`'s human chain and the
/// route 404s: the first assert reddens (the A6 no-op-mount falsifier).
///
/// `a-stop` owns THREE live conversation seats on the online host. The composed route reads
/// them through the two distinct sole authorities (the shared `agent_detail_cold_read` gate
/// + the `SqliteHostAgentStore` seat snapshot), then hard-stops each seat through the exact
/// transport. The middle seat's daemon answer is schema-invalid, so it FAULTS — but that
/// fault does NOT short-circuit the run: all three seats are dispatched, the two healthy
/// seats stop, and the aggregate is the product's `200 {"stopped":2}` (fulfilled count only,
/// the faulted seat kept as ordered residue). The gate proves the real wire hit EACH seat's
/// exact scope: every recorded dispatch is a `conversation`-kind scope naming `a-stop`, and
/// the set of dispatched `conversation_id`s is EXACTLY the three seeded seats.
#[tokio::test]
async fn the_composition_agent_stop_face_reaches_every_seat_through_the_exact_transport() {
    let fixture = fixture(true).await;
    seed_human(&fixture.path).await;
    seed_agent_stop_agent(&fixture.path).await;
    let app = prepare(&config(fixture.path.clone())).await.unwrap();
    let router = app.router.clone();
    let bearer = format!("Bearer {TOKEN}");

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let running = tokio::spawn(serve(listener, app, async move {
        let _ = shutdown_rx.await;
        Ok::<(), std::io::Error>(())
    }));

    let mut ws_request = format!("ws://{address}{HOST_WEBSOCKET_PATH}")
        .into_client_request()
        .unwrap();
    ws_request.headers_mut().insert(
        tokio_tungstenite::tungstenite::http::header::AUTHORIZATION,
        format!("Bearer {HOST_TOKEN}").parse().unwrap(),
    );
    let (socket, _) = connect_async(ws_request).await.unwrap();
    {
        use futures_util::SinkExt;
        let mut socket = socket;
        socket
            .send(ClientMessage::Text(
                serde_json::json!({
                    "direction": "daemon_to_server",
                    "type": "event",
                    "kind": "ready",
                    "protocol_version": OUTBOUND_PROTOCOL_VERSION,
                    "daemon_version": "test",
                    "daemon_id": "daemon-cp5",
                    "instance_id": "instance-cp5",
                    "host_descriptor": { "name": "CP5 host", "kind": "remote" },
                    "capabilities": [],
                    "daemon_pid": 7,
                    "started_at": 100,
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();

        let stop = std::sync::Arc::new(tokio::sync::Notify::new());
        let attached = std::sync::Arc::new(tokio::sync::Notify::new());
        let stop_scopes =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::<serde_json::Value>::new()));
        let servicer = tokio::spawn(service_agent_stop_commands(
            socket,
            stop.clone(),
            attached.clone(),
            "conv-s2",
            stop_scopes.clone(),
        ));
        tokio::time::timeout(std::time::Duration::from_secs(5), attached.notified())
            .await
            .expect("the daemon must attach (host_status probe) before the gate dispatches");

        // Claim all three seats LIVE now that hydration is done.
        activate_agent_stop_slots(&fixture.path).await;

        // The composed face enumerates and stops every running seat; the middle seat faults
        // but does not short-circuit the run.
        let (status, body) =
            request(&router, "POST", "/api/agents/a-stop/stop", Some(&bearer)).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "stop must reach the mounted bulk-stop transport: {body}"
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap(),
            serde_json::json!({ "stopped": 2 }),
            "two healthy seats stopped; the poison seat is residue, not a short-circuit: {body}"
        );

        // Every seat was dispatched through the real transport, in its exact scope: three
        // `conversation`-kind scopes naming `a-stop`, whose conversation_ids are EXACTLY the
        // three seeded seats (proves enumerate-all + no short-circuit + exact scope).
        let scopes = stop_scopes.lock().unwrap().clone();
        assert_eq!(
            scopes.len(),
            3,
            "all three running seats must be dispatched (no seat short-circuited): {scopes:?}"
        );
        for scope in &scopes {
            assert_eq!(scope["kind"], "conversation", "exact seat scope: {scope:?}");
            assert_eq!(scope["agent_id"], "a-stop", "exact seat scope: {scope:?}");
        }
        let mut dispatched: Vec<String> = scopes
            .iter()
            .map(|scope| scope["conversation_id"].as_str().unwrap().to_owned())
            .collect();
        dispatched.sort();
        assert_eq!(
            dispatched,
            vec!["conv-s1", "conv-s2", "conv-s3"],
            "the real wire hit each seeded seat's exact scope"
        );

        stop.notify_one();
        let mut socket = servicer.await.unwrap();
        socket.close(None).await.unwrap();
    }
    shutdown_tx.send(()).unwrap();
    running.await.unwrap().unwrap();
}

/// Seed exactly ONE Active host owned by the acting human, plus its daemon credential, so
/// auto-placement resolves unambiguously to `h-online` and the fake daemon can attach the
/// mounted host WS (making the saga's `capture` return custody). No agent/workspace rows:
/// the create saga mints the entire graph itself.
async fn seed_agent_create_host(path: &Path) {
    let pool = SqlitePoolOptions::new()
        .connect(path.to_str().unwrap())
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO hosts (id, name, kind, created_at, user_id, activated_at, finalized_at) \
             VALUES ('h-online', 'host-online', 'remote', 1, 'user-1', 1, 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
            "INSERT INTO host_credentials (id, host_id, token_hash, kind, expires_at, consumed_at, revoked_at) \
             VALUES ('cred-online', 'h-online', ?, 'long_lived', NULL, NULL, NULL)",
        )
        .bind(HOST_TOKEN_HASH)
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}

/// Drive a connected daemon socket like a REAL daemon for the create saga: answer
/// hydration (`runtime_status`), signal `attached` on the promotion-edge `host_status`
/// probe, tolerate the attach-time `host_runtime_catalog`, and — the point of the gate —
/// answer `agent_materialize` with the schema-valid success the codec requires, so the
/// production `ProductionAgentMaterialize` port completes and the saga proceeds to provision
/// the graph. Both post-commit aftermath legs cross this wire on a real create: `runtime_start`
/// (which fires `start_seen`) AND the onboarding `runtime_deliver` — the onboarding adapter's
/// durable inbox-ensure enqueues the `conversation_inboxes` turn row BEFORE it wakes the agent,
/// so the wake's `claim_pending` finds a pending row and dispatches the delivery. Every
/// create-effect command is RECORDED in `seen` so the gate can prove the real wire was hit
/// exactly once each and that an idempotent echo replays none of it.
async fn service_agent_create_commands<S>(
    mut socket: tokio_tungstenite::WebSocketStream<S>,
    stop: std::sync::Arc<tokio::sync::Notify>,
    attached: std::sync::Arc<tokio::sync::Notify>,
    start_seen: std::sync::Arc<tokio::sync::Notify>,
    seen: std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
) -> tokio_tungstenite::WebSocketStream<S>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    loop {
        tokio::select! {
            () = stop.notified() => break,
            command = next_daemon_json(&mut socket) => {
                let result = match command["kind"].as_str() {
                    Some("runtime_status") => serde_json::json!({ "runtimes": [] }),
                    Some("host_status") => {
                        attached.notify_one();
                        serde_json::json!({
                            "os": "linux",
                            "daemon_pid": 1,
                            "daemon_version": "0.0.0-test",
                            "started_at": 0,
                            "uptime_ms": 0,
                            "active_agent_ids": [],
                        })
                    }
                    Some("host_runtime_catalog") => serde_json::json!({
                        "runtimes": [],
                        "detected_at": 1_700_000_000_000_i64,
                    }),
                    Some("agent_materialize") => {
                        seen.lock().unwrap().push(command.clone());
                        // The codec's `decode_result` requires EXACTLY these four keys,
                        // `ok == true`, `agent_dir == agent_home_workspace_path`, and a
                        // positive `materialized_at`. Anything else faults the saga.
                        serde_json::json!({
                            "ok": true,
                            "agent_dir": "/a",
                            "agent_home_workspace_path": "/a",
                            "materialized_at": 1,
                        })
                    }
                    Some("runtime_start") => {
                        seen.lock().unwrap().push(command.clone());
                        start_seen.notify_one();
                        serde_json::json!({ "started": true, "pid": 1, "started_at": 1 })
                    }
                    Some("runtime_deliver") => {
                        seen.lock().unwrap().push(command.clone());
                        serde_json::json!({
                            "disposition": "admitted",
                            "pid": 1,
                            "started_at": 1,
                        })
                    }
                    other => panic!("unexpected daemon command kind: {other:?}"),
                };
                socket
                    .send(ClientMessage::Text(
                        serde_json::json!({
                            "direction": "daemon_to_server",
                            "type": "command_response",
                            "id": command["id"],
                            "kind": command["kind"],
                            "result": result,
                        })
                        .to_string()
                        .into(),
                    ))
                    .await
                    .unwrap();
            }
        }
    }
    socket
}

/// POST `/api/agents` with the human bearer, the required `Idempotency-Key`, and a JSON
/// create body. Goes through the mounted router (a oneshot on the same app the served
/// listener runs, so the production `ProductionAgentMaterialize` reads the same daemon
/// controller the fake daemon attached to).
async fn post_create(
    router: &Router,
    bearer: &str,
    idempotency_key: &str,
    body: &str,
) -> (StatusCode, String) {
    let request = Request::builder()
        .method("POST")
        .uri("/api/agents")
        .header("authorization", bearer)
        .header("idempotency-key", idempotency_key)
        .header("content-type", "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

fn count_kind(seen: &std::sync::Mutex<Vec<serde_json::Value>>, kind: &str) -> usize {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|command| command["kind"].as_str() == Some(kind))
        .count()
}

/// The #2234 CP6 close: `POST /api/agents` (agent create) is not merely mounted behind the
/// human perimeter — it drives the FULL production saga end to end. Reserve → materialize
/// (over the mounted host WS, through the real `ProductionAgentMaterialize`) → provision the
/// whole conversation-arm graph → seal `201`, then hand the two remote legs to the owned
/// `CreateAftermath` boundary (the start leg reaches the same real daemon transport). Delete
/// the `.merge(agent_create::router(...))` from `prepare()`'s human chain and the
/// authenticated POST 405s on the `/api/agents` node (it is shared with `GET`; anonymous is
/// 401 for both, so the AUTHENTICATED 201 is the real mount witness) — the first assert
/// reddens (the A6 no-op-mount falsifier).
///
/// A no-op or stubbed mount cannot fake the DB graph facts asserted below: the real
/// provision-graph minted the agent's home workspace (owner_agent edge), the fixed
/// onboarding DM channel, the onboarding message on it, and the creator's owner + DM roster
/// edges. The real materialize crossed the WS exactly once; the real aftermath reached the
/// real transport (`runtime_start`); the created arm's inline `agent_changed` fired exactly
/// once through the real app-scope hub sink. An idempotent replay (same key + body) echoes
/// `200` and replays NONE of it: no second materialize, no second start leg, no second
/// `agent_changed` frame, no duplicate rows. The shared `/api/agents` node still lists
/// agents (disjoint-method merge did not clobber `GET`), and the shared `CreateAftermath`
/// the shutdown arm closed rejects post-shutdown legs.
///
/// The four candidate post-commit effects are direct-pairs / agent_changed / onboarding
/// deliver / runtime start. This conversation-arm gate witnesses the three reachable on it —
/// `agent_changed` (app-scope high-water delta), `runtime_start`, and the onboarding
/// `runtime_deliver` — and their zero-replay on echo. The direct-pairs frame belongs to the
/// agent arm and is witnessed by
/// `the_composition_agent_create_agent_arm_publishes_the_fresh_direct_pair`. The onboarding
/// leg is a genuine two-step effect: the composed adapter's durable inbox-ensure writes the
/// `conversation_inboxes` turn row (proving message-in-conversation + canonical participation
/// against committed rows) and THEN wakes the agent, so `claim_pending` finds a pending row
/// and a `runtime_deliver` crosses the wire exactly once. Drop the ensure (wake alone) and the
/// row is absent → `claim_pending` returns `Empty` → no `runtime_deliver`: both the row assert
/// and the delivery-count assert redden, the A6 falsifier for the ensure authority.
///
/// The handler-level orchestration (both legs scheduled, `201` sealed before the legs run,
/// echo fires nothing, per-arm coordinates, direct-pairs gating) is proven by the leaf
/// `agent_create` unit tests over fakes; the tracker lifecycle (cancel-in-flight, no-op
/// after close, recycle, panic isolation) by the `create_aftermath` unit tests. This gate
/// proves only what those cannot: the REAL ports wired into `prepare()`.
#[tokio::test]
async fn the_composition_agent_create_face_drives_the_full_saga_and_post_commit_effects() {
    let fixture = fixture(true).await;
    seed_human(&fixture.path).await;
    seed_agent_create_host(&fixture.path).await;
    let app = prepare(&config(fixture.path.clone())).await.unwrap();
    let router = app.router.clone();
    // The SAME production aftermath instance the shutdown arm will close: a clone shares
    // the one `Arc<Mutex<..>>`, so `serve`'s `create_aftermath.close()` flips it too.
    let close_probe = app.create_aftermath.clone();
    let bearer = format!("Bearer {TOKEN}");

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let running = tokio::spawn(serve(listener, app, async move {
        let _ = shutdown_rx.await;
        Ok::<(), std::io::Error>(())
    }));

    let mut ws_request = format!("ws://{address}{HOST_WEBSOCKET_PATH}")
        .into_client_request()
        .unwrap();
    ws_request.headers_mut().insert(
        tokio_tungstenite::tungstenite::http::header::AUTHORIZATION,
        format!("Bearer {HOST_TOKEN}").parse().unwrap(),
    );
    let (socket, _) = connect_async(ws_request).await.unwrap();
    {
        use futures_util::SinkExt;
        let mut socket = socket;
        socket
            .send(ClientMessage::Text(
                serde_json::json!({
                    "direction": "daemon_to_server",
                    "type": "event",
                    "kind": "ready",
                    "protocol_version": OUTBOUND_PROTOCOL_VERSION,
                    "daemon_version": "test",
                    "daemon_id": "daemon-cp6",
                    "instance_id": "instance-cp6",
                    "host_descriptor": { "name": "CP6 host", "kind": "remote" },
                    "capabilities": [],
                    "daemon_pid": 7,
                    "started_at": 100,
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();

        let stop = std::sync::Arc::new(tokio::sync::Notify::new());
        let attached = std::sync::Arc::new(tokio::sync::Notify::new());
        let start_seen = std::sync::Arc::new(tokio::sync::Notify::new());
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<serde_json::Value>::new()));
        let servicer = tokio::spawn(service_agent_create_commands(
            socket,
            stop.clone(),
            attached.clone(),
            start_seen.clone(),
            seen.clone(),
        ));
        tokio::time::timeout(std::time::Duration::from_secs(5), attached.notified())
            .await
            .expect("the daemon must attach (host_status probe) before create captures");

        // Baseline for the inline post-commit `agent_changed` (app scope): nothing
        // published for this create yet. Read after attach so a daemon-attach frame, if
        // any, is already behind the high-water.
        let app_before = fresh_scope_high_water(address, "app").await;

        // The full saga to 201 (conversation arm — session_scope defaults to conversation).
        let (status, body) = post_create(
            &router,
            &bearer,
            "key-conv",
            r#"{"name":"createbot","runtime":"codex"}"#,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::CREATED,
            "create must reach the mounted saga and materialize over the real WS: {body}"
        );
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        let agent_id = json["agent"]["id"].as_str().unwrap().to_owned();
        let home = json["agent_home_workspace_id"].as_str().unwrap().to_owned();
        let dm = json["dm_channel_id"].as_str().unwrap().to_owned();
        let onboarding_message = json["onboarding_message_id"].as_str().unwrap().to_owned();
        assert!(
            !home.is_empty() && !dm.is_empty() && !onboarding_message.is_empty(),
            "conversation-arm coordinates are all present: {body}"
        );

        // The aftermath start leg reaches the SAME real daemon transport (proves the
        // `CreateAftermath` port drives a real effect, not `/dev/null`).
        tokio::time::timeout(std::time::Duration::from_secs(5), start_seen.notified())
            .await
            .expect("the aftermath runtime-start leg must reach the real daemon transport");

        // The onboarding leg is best-effort and async, so wait for its `runtime_deliver`
        // to land rather than snapshot before it arrives. It crosses because the composed
        // adapter enqueued the durable inbox row ahead of the wake; drop the ensure and this
        // poll times out (Empty → NoPending → no command), reddening the gate.
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if count_kind(&seen, "runtime_deliver") >= 1 {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the onboarding deliver leg must reach the daemon transport as runtime_deliver");

        // The real materialize crossed the mounted host WS exactly once.
        assert_eq!(
            count_kind(&seen, "agent_materialize"),
            1,
            "the production materialize port hit the daemon exactly once"
        );
        assert_eq!(
            count_kind(&seen, "runtime_start"),
            1,
            "the aftermath start leg hit the daemon exactly once"
        );
        assert_eq!(
            count_kind(&seen, "runtime_deliver"),
            1,
            "the onboarding deliver leg hit the daemon exactly once"
        );

        // The inline post-commit `agent_changed` (app scope) fired EXACTLY once through
        // the real `hub.agent_changed_sink()` wired into `prepare()`. Break that sink (or
        // strip the inline publish from the created arm) and this delta drops to 0.
        let app_after_create = fresh_scope_high_water(address, "app").await;
        assert_eq!(
            app_after_create - app_before,
            1,
            "the created arm publishes exactly one app-scope agent_changed frame"
        );

        // DB graph facts — the real provision-graph completed the conversation arm. A
        // stubbed/no-op mount could return 201-shaped JSON but never write this graph.
        let pool = SqlitePoolOptions::new()
            .connect(fixture.path.to_str().unwrap())
            .await
            .unwrap();
        let owner_agent: Option<String> =
            sqlx::query_scalar("SELECT owner_agent_id FROM workspaces WHERE id = ?")
                .bind(&home)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            owner_agent.as_deref(),
            Some(agent_id.as_str()),
            "the home workspace is owned by the created agent"
        );
        let (dm_placement, dm_policy, dm_workspace): (String, String, Option<String>) =
                sqlx::query_as(
                    "SELECT placement_kind, roster_policy, workspace_id FROM conversations WHERE id = ?",
                )
                .bind(&dm)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            dm_placement, "workspace",
            "the onboarding DM is a workspace channel"
        );
        assert_eq!(dm_policy, "fixed", "the onboarding DM roster is fixed");
        assert_eq!(
            dm_workspace.as_deref(),
            Some(home.as_str()),
            "the onboarding DM lives in the agent's home workspace"
        );
        let onboarding_conversation: String =
            sqlx::query_scalar("SELECT conversation_id FROM messages WHERE id = ?")
                .bind(&onboarding_message)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            onboarding_conversation, dm,
            "the onboarding message sits on the onboarding DM"
        );
        let agent_on_dm: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM conversation_agents WHERE conversation_id = ? AND agent_id = ?",
        )
        .bind(&dm)
        .bind(&agent_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(agent_on_dm, 1, "the agent is a member of its onboarding DM");
        // The onboarding leg's durable half: the composed adapter's inbox-ensure wrote the
        // `conversation_inboxes` turn row binding THIS onboarding message to THIS conversation
        // for THIS agent. Its presence (not merely the `runtime_deliver` on the wire) is the
        // committed-fact witness that the ensure ran ahead of the wake; break the message-id
        // thread or drop the ensure and no row exists here.
        let inbox_row: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM conversation_inboxes WHERE conversation_id = ? AND agent_id = ? \
                 AND message_id = ? AND delivery_kind = 'turn'",
        )
        .bind(&dm)
        .bind(&agent_id)
        .bind(&onboarding_message)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            inbox_row, 1,
            "the onboarding adapter enqueued exactly one durable inbox turn for the agent"
        );
        let creator_on_dm: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM conversation_users WHERE conversation_id = ? AND user_id = 'user-1'",
            )
            .bind(&dm)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            creator_on_dm, 1,
            "the creator is a member of the onboarding DM"
        );
        let creator_role: String = sqlx::query_scalar(
            "SELECT role FROM workspace_users WHERE workspace_id = ? AND user_id = 'user-1'",
        )
        .bind(&home)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(creator_role, "owner", "the creator owns the home workspace");
        let (materialization, created_by, scope): (String, String, String) = sqlx::query_as(
                "SELECT materialization_status, created_by_user_id, session_scope FROM agents WHERE id = ?",
            )
            .bind(&agent_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            materialization, "active",
            "the reservation flipped to active"
        );
        assert_eq!(created_by, "user-1", "the creator is recorded");
        assert_eq!(scope, "conversation", "the conversation arm was taken");

        // The shared `/api/agents` node still lists agents — the disjoint-method merge did
        // not clobber the pre-existing `GET`.
        let (list_status, list_body) = request(&router, "GET", "/api/agents", Some(&bearer)).await;
        assert_eq!(
            list_status,
            StatusCode::OK,
            "GET on the shared /api/agents node still lists: {list_body}"
        );
        assert!(
            list_body.contains(&agent_id),
            "the created agent appears in the shared-node list: {list_body}"
        );

        // Idempotent replay: SAME key + body echoes 200 and replays NONE of the reachable
        // post-commit effects — no second materialize, no second start leg, no second
        // onboarding delivery, and no second app-scope `agent_changed` frame.
        let materialize_before = count_kind(&seen, "agent_materialize");
        let start_before = count_kind(&seen, "runtime_start");
        let deliver_before = count_kind(&seen, "runtime_deliver");
        let (echo_status, echo_body) = post_create(
            &router,
            &bearer,
            "key-conv",
            r#"{"name":"createbot","runtime":"codex"}"#,
        )
        .await;
        assert_eq!(
            echo_status,
            StatusCode::OK,
            "the same key + body echoes 200, not a second create: {echo_body}"
        );
        let echo_json: serde_json::Value = serde_json::from_str(&echo_body).unwrap();
        assert_eq!(
            echo_json["agent"]["id"].as_str(),
            Some(agent_id.as_str()),
            "the echo returns the same agent"
        );
        assert_eq!(
            echo_json["dm_channel_id"].as_str(),
            Some(dm.as_str()),
            "the echo returns the same coordinates"
        );
        assert_eq!(
            count_kind(&seen, "agent_materialize"),
            materialize_before,
            "the echo does not re-run the real materialize"
        );
        assert_eq!(
            count_kind(&seen, "runtime_start"),
            start_before,
            "the echo schedules no second runtime-start leg"
        );
        // Give any (erroneously) scheduled echo delivery leg a chance to reach the wire,
        // then prove none did — the echo neither re-ensures the inbox row nor re-wakes.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(
            count_kind(&seen, "runtime_deliver"),
            deliver_before,
            "the echo schedules no second onboarding deliver leg"
        );
        let app_after_echo = fresh_scope_high_water(address, "app").await;
        assert_eq!(
            app_after_echo - app_after_create,
            0,
            "the echo publishes no second app-scope agent_changed frame"
        );
        let agents_named: i64 =
            sqlx::query_scalar("SELECT count(*) FROM agents WHERE name = 'createbot'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(agents_named, 1, "the echo created no duplicate agent row");
        let homes: i64 =
            sqlx::query_scalar("SELECT count(*) FROM workspaces WHERE owner_agent_id = ?")
                .bind(&agent_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(homes, 1, "the echo created no duplicate home workspace");
        let inbox_rows_after_echo: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM conversation_inboxes WHERE conversation_id = ? AND agent_id = ? \
                 AND message_id = ?",
        )
        .bind(&dm)
        .bind(&agent_id)
        .bind(&onboarding_message)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            inbox_rows_after_echo, 1,
            "the echo enqueued no duplicate inbox turn (idempotent ensure, ON CONFLICT DO NOTHING)"
        );
        pool.close().await;

        stop.notify_one();
        let mut socket = servicer.await.unwrap();
        socket.close(None).await.unwrap();
    }
    shutdown_tx.send(()).unwrap();
    running.await.unwrap().unwrap();

    // The close-wiring witness: after the production shutdown arm ran serve's
    // `create_aftermath.close()`, the SHARED tracker rejects new work — a fresh dispatch is
    // dropped unpolled. If shutdown forgot to close the tracker, this sender is still
    // queued (Empty), or has run (Ok), rather than immediately dropped (Closed).
    let (sent, mut received) = tokio::sync::oneshot::channel();
    silverhand_backend_server::application_orchestration::agent_create::AftermathDispatch::dispatch(
        &close_probe,
        Box::pin(async move {
            let _ = sent.send(());
        }),
    );
    assert_eq!(
        received.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Closed),
        "serve's shutdown arm must have closed the shared aftermath tracker"
    );
}

/// The #2234 CP6 agent-arm witness: an agent-scoped create returns null coordinates, mints
/// the creator↔agent direct pair, and — the point of this gate — publishes the fresh pair
/// through the NEW production `HubDirectPairsChanged` sink wired into `prepare()`. That sink
/// has no other composition consumer, so this is its A6 no-op falsifier: the viewer's
/// direct-pairs list scope high-water advances by one on the fresh create and NOT on the
/// idempotent echo. (The conversation arm never fires this edge; the agent arm with a fresh
/// pair always does — the leaf `agent_create` tests prove that gating over fakes.)
#[tokio::test]
async fn the_composition_agent_create_agent_arm_publishes_the_fresh_direct_pair() {
    let fixture = fixture(true).await;
    seed_human(&fixture.path).await;
    seed_agent_create_host(&fixture.path).await;
    let app = prepare(&config(fixture.path.clone())).await.unwrap();
    let router = app.router.clone();
    let bearer = format!("Bearer {TOKEN}");

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let running = tokio::spawn(serve(listener, app, async move {
        let _ = shutdown_rx.await;
        Ok::<(), std::io::Error>(())
    }));

    let mut ws_request = format!("ws://{address}{HOST_WEBSOCKET_PATH}")
        .into_client_request()
        .unwrap();
    ws_request.headers_mut().insert(
        tokio_tungstenite::tungstenite::http::header::AUTHORIZATION,
        format!("Bearer {HOST_TOKEN}").parse().unwrap(),
    );
    let (socket, _) = connect_async(ws_request).await.unwrap();
    {
        use futures_util::SinkExt;
        let mut socket = socket;
        socket
            .send(ClientMessage::Text(
                serde_json::json!({
                    "direction": "daemon_to_server",
                    "type": "event",
                    "kind": "ready",
                    "protocol_version": OUTBOUND_PROTOCOL_VERSION,
                    "daemon_version": "test",
                    "daemon_id": "daemon-cp6b",
                    "instance_id": "instance-cp6b",
                    "host_descriptor": { "name": "CP6 host", "kind": "remote" },
                    "capabilities": [],
                    "daemon_pid": 7,
                    "started_at": 100,
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();

        let stop = std::sync::Arc::new(tokio::sync::Notify::new());
        let attached = std::sync::Arc::new(tokio::sync::Notify::new());
        let start_seen = std::sync::Arc::new(tokio::sync::Notify::new());
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<serde_json::Value>::new()));
        let servicer = tokio::spawn(service_agent_create_commands(
            socket,
            stop.clone(),
            attached.clone(),
            start_seen.clone(),
            seen.clone(),
        ));
        tokio::time::timeout(std::time::Duration::from_secs(5), attached.notified())
            .await
            .expect("the daemon must attach before create captures");

        // Baseline: nothing published to the creator's direct-pairs list yet.
        let viewer_before = fresh_scope_high_water(address, "viewer:self").await;

        let (status, body) = post_create(
            &router,
            &bearer,
            "key-agent",
            r#"{"name":"createbot2","runtime":"codex","session_scope":"agent"}"#,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::CREATED,
            "the agent-arm create must reach the mounted saga: {body}"
        );
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        let agent_id = json["agent"]["id"].as_str().unwrap().to_owned();
        assert!(
            json["agent_home_workspace_id"].is_null()
                && json["dm_channel_id"].is_null()
                && json["onboarding_message_id"].is_null(),
            "agent-arm coordinates are all null: {body}"
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), start_seen.notified())
            .await
            .expect("the aftermath runtime-start leg must reach the daemon");

        // The agent-scoped arm greets the agent on the direct line it just opened with its
        // creator, so its onboarding delivery is what exercises the DIRECT-PAIR branch of the
        // participation predicate in production (the workspace arm exercises the roster
        // branch). Wait for that `runtime_deliver` to land: it crosses because the composed
        // adapter enqueued the durable inbox row — whose ensure witnessed message-in-pair AND
        // the endpoint participation — ahead of the wake. Break that seam and the poll times
        // out (Empty → NoPending → no command).
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if count_kind(&seen, "runtime_deliver") >= 1 {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the agent-arm onboarding deliver leg must reach the daemon as runtime_deliver");
        assert_eq!(
            count_kind(&seen, "runtime_deliver"),
            1,
            "the agent-arm onboarding deliver leg hit the daemon exactly once"
        );
        let deliver_after_create = count_kind(&seen, "runtime_deliver");

        // The NEW HubDirectPairsChanged sink fired exactly once for the fresh pair.
        let viewer_after_create = fresh_scope_high_water(address, "viewer:self").await;
        assert_eq!(
            viewer_after_create - viewer_before,
            1,
            "the fresh direct pair advanced the creator's direct-pairs list by one frame"
        );

        // The direct pair exists as a direct_pair-placement conversation between the human
        // and the agent.
        let pool = SqlitePoolOptions::new()
            .connect(fixture.path.to_str().unwrap())
            .await
            .unwrap();
        let pairs: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM conversations \
                 WHERE placement_kind = 'direct_pair' \
                   AND (endpoint_a_id = ? OR endpoint_b_id = ?)",
        )
        .bind(&agent_id)
        .bind(&agent_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(pairs, 1, "exactly one creator-agent direct pair was minted");

        // Idempotent replay: 200, and NO second direct-pairs frame.
        let (echo_status, echo_body) = post_create(
            &router,
            &bearer,
            "key-agent",
            r#"{"name":"createbot2","runtime":"codex","session_scope":"agent"}"#,
        )
        .await;
        assert_eq!(
            echo_status,
            StatusCode::OK,
            "the agent-arm echo is 200: {echo_body}"
        );
        let viewer_after_echo = fresh_scope_high_water(address, "viewer:self").await;
        assert_eq!(
            viewer_after_echo - viewer_after_create,
            0,
            "the echo publishes no second direct-pairs frame"
        );
        assert_eq!(
            count_kind(&seen, "agent_materialize"),
            1,
            "the echo does not re-run the real materialize"
        );
        assert_eq!(
            count_kind(&seen, "runtime_deliver"),
            deliver_after_create,
            "the idempotent echo re-drives no onboarding delivery (ensure conflicts, wake \
                 finds no pending row)"
        );
        pool.close().await;

        stop.notify_one();
        let mut socket = servicer.await.unwrap();
        socket.close(None).await.unwrap();
    }
    shutdown_tx.send(()).unwrap();
    running.await.unwrap().unwrap();
}

/// #2219 real-authority binding — the invite→resolve chain proves BOTH placement and
/// wiring in one shot. (1) A connect-token bearer is NOT a human session; a resolve
/// mounted behind the human perimeter would reject it with the perimeter body before
/// its own logic ran, so a 200 here proves resolve sits at the anonymous edge (matching
/// TS's `installPublicBearerMiddleware` allowlist). (2) Only a token this composition
/// just minted resolves to the host it minted — an empty no-op mount could never return
/// the invited `host_id`, so the round-trip proves resolve is bound to the real
/// credential lookup + computer read off live SQLite (the A6 no-op-mount lesson).
#[tokio::test]
async fn the_composition_resolves_a_freshly_invited_connect_token_anonymously() {
    let fixture = fixture(true).await;
    seed_human(&fixture.path).await;
    let app = prepare(&config(fixture.path.clone())).await.unwrap();
    let router = app.router;
    let bearer = format!("Bearer {TOKEN}");

    // Authenticated invite mints a pending computer and reveals its connect token.
    let (status, body) = json_request_with_body(
        &router,
        "POST",
        "/api/hosts",
        Some(&bearer),
        Body::from("{}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "invite: {body}");
    let invited: serde_json::Value = serde_json::from_str(&body).unwrap();
    let host_id = invited["host_id"].as_str().unwrap().to_owned();
    let connect_token = invited["connect_token"].as_str().unwrap().to_owned();

    // The daemon presents that connect token at the ANONYMOUS resolve door (no human
    // session). Resolve verifies the credential against live SQLite and reveals the one
    // fact it may — the `host_id` the token was minted for.
    let connect_bearer = format!("Bearer {connect_token}");
    let (status, body) = request(
        &router,
        "GET",
        "/api/hosts/connect-token/resolve",
        Some(&connect_bearer),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "resolve: {body}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({ "host_id": host_id }),
        "resolve returns the invited host_id off live SQLite"
    );

    // A garbage bearer reaches the same anonymous handler and is refused by ITS OWN
    // logic — a 401 carrying the handler's discriminated `error_code`, never the
    // perimeter's `{"error":"unauthorized"}` (which omits `error_code`).
    let (status, body) = request(
        &router,
        "GET",
        "/api/hosts/connect-token/resolve",
        Some("Bearer not-a-real-connect-token"),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "garbage token: {body}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({ "error": "unauthorized", "error_code": "unauthorized" }),
        "resolve's own 401 body, not the perimeter's"
    );
}

/// #2219 real-authority binding — the claim→rotate chain under the Embedded profile.
/// Claim mints a `kind:"local"` computer off live SQLite; rotate then re-mints that
/// computer's long-lived token via the embedded router's real `RotateHostToken`. A
/// no-op mount could not (a) return a fresh 43-char token that DIFFERS from the claim's,
/// nor (b) answer a byte-exact `host_not_found` for an absent id. Anti-enumeration and
/// the method checksum ride along so a mount that widened the door reddens here.
#[tokio::test]
async fn the_composition_claims_a_local_computer_and_rotates_its_token_embedded() {
    let fixture = fixture(true).await;
    seed_human(&fixture.path).await;
    let app = prepare(&config(fixture.path.clone())).await.unwrap();
    let router = app.router;
    let bearer = format!("Bearer {TOKEN}");

    // Anonymous claim is refused at the human perimeter before any body is read.
    let (status, _) = request(&router, "POST", "/api/hosts/claim-or-create", None).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "claim is behind the perimeter"
    );

    // A well-formed claim mints a local computer and reveals its first token.
    let claim_body = r#"{"installation_id":"11111111-1111-4111-8111-111111111111"}"#;
    let (status, body) = json_request_with_body(
        &router,
        "POST",
        "/api/hosts/claim-or-create",
        Some(&bearer),
        Body::from(claim_body),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "claim: {body}");
    let claimed: serde_json::Value = serde_json::from_str(&body).unwrap();
    let host_id = claimed["host_id"].as_str().unwrap().to_owned();
    assert_eq!(
        claimed["host"]["kind"], "local",
        "claim mints a local computer"
    );
    let first_token = claimed["host_token"].as_str().unwrap().to_owned();
    assert_eq!(
        first_token.len(),
        43,
        "claim reveals the minted token: {body}"
    );

    // A malformed installation_id reaches the frozen `ValidatedJson` admission and
    // collapses to the wave's minimal semantic `422 {"error":"invalid input"}` (the
    // ratified `is_uuid` garde rule; see `host_runtime/claim.rs`
    // `a_semantic_value_fault_is_the_minimal_422_before_the_claim`), never a lookup.
    let (status, body) = json_request_with_body(
        &router,
        "POST",
        "/api/hosts/claim-or-create",
        Some(&bearer),
        Body::from(r#"{"installation_id":"not-a-uuid"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "bad uuid: {body}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap()["error"],
        "invalid input",
        "a malformed body is invalid input, not a not-found"
    );

    // Rotate that freshly claimed local computer through the embedded router's real
    // authority: a new 43-char token that differs from the claim's.
    let rotate_uri = format!("/api/hosts/{host_id}/rotate-token");
    let (status, body) = request(&router, "POST", &rotate_uri, Some(&bearer)).await;
    assert_eq!(status, StatusCode::OK, "rotate: {body}");
    let rotated: serde_json::Value = serde_json::from_str(&body).unwrap();
    let second_token = rotated["host_token"].as_str().unwrap().to_owned();
    assert_eq!(second_token.len(), 43, "rotate re-mints a token: {body}");
    assert_ne!(first_token, second_token, "the token was actually rotated");

    // Anti-enumeration: rotating an absent computer is a byte-exact `host_not_found`
    // from the HANDLER (proving the handler ran), never a bare unmounted-path 404.
    let (status, body) = request(
        &router,
        "POST",
        "/api/hosts/host-absent/rotate-token",
        Some(&bearer),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "absent rotate: {body}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({ "error": "host not found", "error_code": "host_not_found" }),
        "the handler's anti-enumeration 404, not an unmounted-path 404"
    );

    // Anon → 401 (behind the perimeter); GET → 405 (path present, method absent).
    let (status, _) = request(&router, "POST", &rotate_uri, None).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "rotate is behind the perimeter"
    );
    let (status, _) = request(&router, "GET", &rotate_uri, Some(&bearer)).await;
    assert_eq!(
        status,
        StatusCode::METHOD_NOT_ALLOWED,
        "rotate is POST-only"
    );
}

/// #2219 real-authority binding — the standalone (RemoteOnly) rotate arm. The key
/// differential: with no daemon connected, rotate answers `503 transport_no_connection`,
/// NOT `503 host_runtime_disabled`. The latter is the composition-time `None` stub; the
/// former can only come from a wired `Some(coordinator)` that attempted delivery and
/// found no transport. This proves `standalone_router` was composed with the real daemon
/// coordinator, not disabled.
#[tokio::test]
async fn the_standalone_composition_rotates_a_claimed_computer_through_the_daemon_coordinator() {
    let fixture = fixture(true).await;
    seed_human(&fixture.path).await;
    let app = prepare(&remote_config(fixture.path.clone())).await.unwrap();
    let router = app.router;
    let bearer = format!("Bearer {TOKEN}");

    // Claim mints a local computer under RemoteOnly too — the perimeter chain mounts
    // claim regardless of profile.
    let (status, body) = json_request_with_body(
        &router,
        "POST",
        "/api/hosts/claim-or-create",
        Some(&bearer),
        Body::from(r#"{"installation_id":"11111111-1111-4111-8111-111111111111"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "claim (standalone): {body}");
    let host_id = serde_json::from_str::<serde_json::Value>(&body).unwrap()["host_id"]
        .as_str()
        .unwrap()
        .to_owned();

    // Rotate reaches the standalone coordinator, which finds no connected daemon.
    let (status, body) = request(
        &router,
        "POST",
        &format!("/api/hosts/{host_id}/rotate-token"),
        Some(&bearer),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "standalone rotate: {body}"
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({
            "error": "host token rotation transport failed",
            "error_code": "transport_no_connection"
        }),
        "a wired Some(coordinator) with no transport, NOT the None host_runtime_disabled stub"
    );
}

/// P0-1: `listen_ready` is a SERVING-boundary fact witnessed by the bound listener,
/// not a flag anyone can flip. There is no public `mark_listen_ready`; the ONLY way
/// to establish it is to hand a bound `TcpListener` to [`serve`], which marks the
/// holder the `/readyz` router shares. So this test binds a real listener and
/// actually starts `serve`, then watches that SAME shared holder go 503 → 200. A
/// prepared-but-unserved app stays 503; if `prepare()` marked it, or if the mark
/// were a caller-callable method independent of a listener, the first probe would be
/// 200 and this reddens.
#[tokio::test]
async fn readyz_reports_listen_ready_only_after_serve_receives_the_bound_listener() {
    let fixture = fixture(true).await;
    let app = prepare(&config(fixture.path.clone())).await.unwrap();
    // The router shares the SAME `BootProgress` handle `serve` will mark, so probing
    // this clone observes the real serving-boundary flip — not a copy.
    let router = app.router.clone();

    // Prepared but not served: migrations + db green, listen_ready false → 503.
    let (status, body) = request(&router, "GET", "/readyz", None).await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "before serve: {body}"
    );
    let before: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(before["ok"], serde_json::json!(false));
    assert_eq!(
        before["checks"]["migrations_applied"],
        serde_json::json!(true)
    );
    assert_eq!(before["checks"]["listen_ready"], serde_json::json!(false));

    // Bind a REAL listener and actually start serving. `serve` marks `listen_ready`
    // from the bound listener it now holds — nothing else in the process can.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    // The receiver IS the shutdown future (`Output = Result<(), RecvError>`), so it
    // satisfies `serve`'s bound directly.
    let served = tokio::spawn(serve(listener, app, shutdown_rx));

    // Poll the SAME holder until the serve task's mark lands. `serve` marks before
    // its first await, so a bounded yield loop is enough; we never flip the holder
    // ourselves.
    let mut ready = false;
    for _ in 0..100 {
        let (status, _) = request(&router, "GET", "/readyz", None).await;
        if status == StatusCode::OK {
            ready = true;
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(
        ready,
        "serve must establish listen_ready from the bound listener"
    );

    let (status, body) = request(&router, "GET", "/readyz", None).await;
    assert_eq!(status, StatusCode::OK, "after serve: {body}");
    let after: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(after["ok"], serde_json::json!(true));
    assert_eq!(after["checks"]["listen_ready"], serde_json::json!(true));

    let _ = shutdown_tx.send(());
    let _ = served.await;
}

/// P0-2: `SILVERHAND_DISABLE_SIGNUP` has TWO launcher contracts, so the parser is
/// profile×input — never a generic truthy helper. Embedded disables ONLY on the
/// exact `"1"` and never fails; RemoteOnly trims, accepts only `0`/`1`, defaults
/// unset/empty to enabled, and hard-fails on anything else. This pins both against
/// the TS authorities (`packages/backend/src/auth/config.ts`,
/// `packages/server/src/config.ts`), including the values where they DIVERGE.
#[tokio::test]
async fn features_advertise_recovery_with_remote_mail_and_deletion_off_embedded() {
    let mail = crate::foundation::mail::MailConfig::parse(
        Some("smtps://user:secret@smtp.example.com:465"),
        Some("no-reply@example.com"),
    )
    .unwrap();
    for (remote, with_mail, expected) in [
        (true, true, true),
        (true, false, false),
        (false, true, false),
        (false, false, false),
    ] {
        let fixture = fixture(true).await;
        let mut config = if remote {
            remote_config(fixture.path.clone())
        } else {
            config(fixture.path.clone())
        };
        if with_mail {
            config.mail = mail.clone();
        }
        let app = prepare(&config).await.unwrap();
        let (status, body) = request(&app.router, "GET", "/api/server/info", None).await;
        assert_eq!(status, StatusCode::OK);
        let body: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["features"]["password_recovery_enabled"], expected);
        assert_eq!(body["features"]["account_deletion_enabled"], remote);
        assert_eq!(body["features"]["signup_enabled"], body["signup_enabled"]);
        let (status, _) = json_request_with_body(
            &app.router,
            "POST",
            "/auth/password-recovery/request",
            None,
            Body::from(r#"{"email":"nobody@example.com"}"#),
        )
        .await;
        let refused = if expected {
            StatusCode::ACCEPTED
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        };
        assert_eq!(status, refused);
    }
}

#[test]
fn signup_admission_matches_each_launchers_contract() {
    use DeploymentProfile::{Embedded, RemoteOnly};

    // Embedded: only exact "1" disables; every other value enables; nothing fails.
    assert!(parse_signup_enabled(Embedded, None).unwrap()); // unset → enabled
    assert!(parse_signup_enabled(Embedded, Some("0")).unwrap());
    assert!(!parse_signup_enabled(Embedded, Some("1")).unwrap()); // disabled
    assert!(parse_signup_enabled(Embedded, Some("true")).unwrap()); // NOT "1" → enabled
    assert!(parse_signup_enabled(Embedded, Some(" 1 ")).unwrap()); // no trim → enabled
    assert!(parse_signup_enabled(Embedded, Some("garbage")).unwrap());
    assert!(parse_signup_enabled(Embedded, Some("")).unwrap());

    // RemoteOnly: unset/empty default enabled; only trimmed 0/1 legal; else fails.
    assert!(parse_signup_enabled(RemoteOnly, None).unwrap());
    assert!(parse_signup_enabled(RemoteOnly, Some("")).unwrap());
    assert!(parse_signup_enabled(RemoteOnly, Some("0")).unwrap());
    assert!(!parse_signup_enabled(RemoteOnly, Some("1")).unwrap()); // disabled
    assert!(!parse_signup_enabled(RemoteOnly, Some(" 1 ")).unwrap()); // trimmed → disabled
    for fails in ["true", "TRUE", "garbage", " "] {
        assert!(
            matches!(
                parse_signup_enabled(RemoteOnly, Some(fails)),
                Err(ConfigError::InvalidDisableSignup(_))
            ),
            "RemoteOnly {fails:?} must fail startup, not silently enable"
        );
    }
}

/// P0-3: the RemoteOnly half of the ONE product is a real `prepare()` composition,
/// not just an enum arm — so this differential drives it end-to-end and would redden
/// if composition swapped the runtimes arms, mis-projected a mode axis, or dropped a
/// root face. Under the SAME profile it asserts: `/api/health` reports
/// `mode:"remote"`; `/readyz` reports `mode:"remote"` + `deployment_mode:"standalone"`
/// + an installed fanout key + 503 before serving; and `/api/runtimes` is the control-plane
/// refusal (503 `host_runtime_disabled` when authenticated, still 401 anonymous
/// behind the perimeter — never a detection pass).
#[tokio::test]
async fn the_remote_only_profile_composes_a_control_plane_backend() {
    let fixture = fixture(true).await;
    seed_human(&fixture.path).await;
    let app = prepare(&remote_config(fixture.path.clone())).await.unwrap();
    let router = app.router;
    let bearer = format!("Bearer {TOKEN}");

    // `/api/health` — anonymous, remote mode.
    let (status, body) = request(&router, "GET", "/api/health", None).await;
    assert_eq!(status, StatusCode::OK, "remote health: {body}");
    let health: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(health["ok"], serde_json::json!(true));
    assert_eq!(health["mode"], serde_json::json!("remote"));

    // `/readyz` — fanout installed, but no listener has been served yet → 503.
    let (status, body) = request(&router, "GET", "/readyz", None).await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "remote readyz: {body}"
    );
    let ready: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(ready["ok"], serde_json::json!(false));
    assert_eq!(ready["mode"], serde_json::json!("remote"));
    assert_eq!(ready["deployment_mode"], serde_json::json!("standalone"));
    assert_eq!(
        ready["checks"]["outbound_event_fanout_installed"],
        serde_json::json!(true),
        "the shared daemon-to-viewer event chain is installed during composition"
    );
    assert_eq!(ready["checks"]["listen_ready"], false);

    // `/api/runtimes` — anonymous still 401 (behind the perimeter)…
    let (status, _) = request(&router, "GET", "/api/runtimes", None).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "remote runtimes anon → 401"
    );

    // …and authenticated, the control-plane refusal, never a detection pass.
    let (status, body) = request(&router, "GET", "/api/runtimes", Some(&bearer)).await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "remote runtimes: {body}"
    );
    let refusal: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(refusal["code"], serde_json::json!("host_runtime_disabled"));
}

#[path = "remote_fanout_tests.rs"]
mod remote_fanout_tests;
/// one host with DISTINCT `created_at`, so the list's `created_at ASC` order is
/// defined, plus a registered STRANGER (`user-stranger`, its own seeded session over
/// `ADMIN_HASH`) who is in no arm of either agent — so the list face collapses that
/// caller to an empty array with no existence oracle.
async fn seed_list_agents(path: &Path) {
    let pool = SqlitePoolOptions::new()
        .connect(path.to_str().unwrap())
        .await
        .unwrap();
    for statement in [
        "INSERT INTO hosts (id, name, kind, created_at, user_id) VALUES ('h-list', 'host-list', 'remote', 1, 'user-1')",
        "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, created_by_user_id, materialization_status) \
             VALUES ('al-1', 'agent-list-one', 'qoder', 'h-list', 100, 100, 'user-1', 'active')",
        "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, created_by_user_id, materialization_status) \
             VALUES ('al-2', 'agent-list-two', 'qoder', 'h-list', 200, 200, 'user-1', 'active')",
        "INSERT INTO users (id, name, display_name, created_at, updated_at) VALUES ('user-stranger', 'stranger', 'Stranger', 1, 1)",
    ] {
        sqlx::query(statement).execute(&pool).await.unwrap();
    }
    let now = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    sqlx::query(
            "INSERT INTO auth_sessions (id, user_id, token_hash, created_at, last_used_at, revoked_at, user_agent, ip_first_seen) VALUES ('session-stranger', 'user-stranger', ?, ?, ?, NULL, NULL, NULL)",
        )
        .bind(ADMIN_HASH)
        .bind(now)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}

/// The composition mounts `GET /api/agents` (agent-LIST) behind the SAME human
/// perimeter, wired to the real SQLite agent-list cold read. Proves the composition
/// obligation the leaf's own tests cannot: the anonymous perimeter refusal; an
/// admitted creator reading the real seeded set off the live SQLite adapter in the
/// port's `created_at ASC` order; a registered STRANGER collapsing to the empty
/// array with no existence oracle; and a one-method route checksum. The per-branch
/// matrix (stage flags, `?scope=owned`, projection) stays with the leaf's own tests;
/// this proves the route is MOUNTED and reads the composition's real adapter.
#[tokio::test]
async fn the_composition_serves_agent_list_behind_the_perimeter() {
    let fixture = fixture(true).await;
    seed_human(&fixture.path).await;
    seed_list_agents(&fixture.path).await;
    let app = prepare(&config(fixture.path.clone())).await.unwrap();
    let router = app.router;
    let bearer = format!("Bearer {TOKEN}");
    let stranger = format!("Bearer {ADMIN_TOKEN}");
    let path = "/api/agents";

    // Anonymous → the perimeter's refusal envelope, never a list answer.
    let (status, body) = request(&router, "GET", path, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body, r#"{"error":"unauthorized"}"#);

    // Admitted creator → the real adapter's `created_at ASC` set off live SQLite.
    let (status, body) = request(&router, "GET", path, Some(&bearer)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let ids = serde_json::from_str::<serde_json::Value>(&body).unwrap()["agents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|agent| agent["id"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(ids, ["al-1", "al-2"]);

    // Registered stranger → the empty array; a list face has no 404, a hidden agent
    // is simply ABSENT, so the mounted surface is never an enumeration oracle.
    let (status, body) = request(&router, "GET", path, Some(&stranger)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({ "agents": [] })
    );

    // Shared-node checksum: since CP6 the `/api/agents` node serves BOTH `GET` (this
    // list) and `POST` (create) — a disjoint-method merge, not a clobber. A bare POST
    // (bearer, but no JSON content type) is no longer method-shadowed (405); it reaches
    // the mounted create handler's frozen `ValidatedJson` admission, which — absent the
    // JSON content type — refuses with the wave's ratified native `415` before the body
    // runs (see `identity_access/agent_patch.rs`: "a missing `Content-Type` is `415`"). The
    // non-405 refusal is the mount proof; the exact status is the ratified wire contract.
    let (status, body) = request(&router, "POST", path, Some(&bearer)).await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{body}");
}

/// The direct-pair initiation TARGET for viewer `user-1`: one host and one active
/// agent the viewer created with an `agent` session scope. This is the "creator" arm
/// of the initiation authority (`direct_pair_initiation_candidate`: `created_by_user_id
/// = viewer AND materialization_status = 'active' AND session_scope = 'agent'`), so no
/// workspace membership or pre-existing pair is needed to be allowed to open the line.
/// The literal id equals `DP_TARGET_AGENT`, which the E2E builds its path from, so any
/// drift fails loudly.
const DP_TARGET_AGENT: &str = "dp-target-1";

async fn seed_direct_pair_target(path: &Path) {
    let pool = SqlitePoolOptions::new()
        .connect(path.to_str().unwrap())
        .await
        .unwrap();
    for statement in [
        "INSERT INTO hosts (id, name, kind, created_at, user_id) VALUES ('h-dp', 'host-dp', 'remote', 1, 'user-1')",
        "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, created_by_user_id, session_scope, materialization_status) \
             VALUES ('dp-target-1', 'pair-target', 'qoder', 'h-dp', 100, 100, 'user-1', 'agent', 'active')",
    ] {
        sqlx::query(statement).execute(&pool).await.unwrap();
    }
    pool.close().await;
}

async fn count(path: &Path, sql: &'static str) -> i64 {
    let pool = SqlitePoolOptions::new()
        .connect(path.to_str().unwrap())
        .await
        .unwrap();
    let total: i64 = sqlx::query_scalar(sql).fetch_one(&pool).await.unwrap();
    pool.close().await;
    total
}

/// The one direct-pair conversation id from the live store — the pair the initiation
/// door minted. Used to name the conversation-scope the aftermath's `message` frame
/// rides, without the test having to predict the allocated id.
async fn direct_pair_conversation_id(path: &Path) -> String {
    let pool = SqlitePoolOptions::new()
        .connect(path.to_str().unwrap())
        .await
        .unwrap();
    let id: String =
        sqlx::query_scalar("SELECT id FROM conversations WHERE placement_kind = 'direct_pair'")
            .fetch_one(&pool)
            .await
            .unwrap();
    pool.close().await;
    id
}

/// A FRESH authenticated event-WS subscription's `subscribed.seq` for `scope`: the hub's
/// own high-water, which it reports deterministically in the ack. Reading it from a fresh
/// subscription (rather than draining a live socket) seals the "under-read / extra frame
/// left queued" false-green — the hub tells the truth about how many frames it published.
async fn fresh_scope_high_water(address: std::net::SocketAddr, scope: &str) -> i64 {
    let mut request = format!("ws://{address}{EVENT_WEBSOCKET_PATH}")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert(ORIGIN, "http://localhost:5173".parse().unwrap());
    request.headers_mut().insert(
        COOKIE,
        format!("{EVENT_WS_SESSION_COOKIE}={TOKEN}")
            .parse()
            .unwrap(),
    );
    request
        .headers_mut()
        .insert(DESKTOP_API_EPOCH_HEADER, MINIMUM_CLIENT_API_EPOCH.into());
    let (mut socket, _) = connect_async(request).await.unwrap();
    assert_eq!(next_socket_json(&mut socket).await["type"], "authenticated");
    socket
        .send(ClientMessage::Text(
            serde_json::json!({ "type": "subscribe", "scope": scope })
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    let subscribed = next_socket_json(&mut socket).await;
    assert_eq!(subscribed["type"], "subscribed", "{subscribed}");
    let seq = subscribed["seq"].as_i64().unwrap();
    socket.close(None).await.unwrap();
    seq
}

/// Wave 3A checkpoint A6 (#2203): the human direct-pair initiation WRITE door,
/// `POST /api/agents/{id}/direct-messages`, mounted in `prepare()` behind the SAME
/// `require_human_session` perimeter and wired to the ONE production `MessagingStore`
/// the ordinary send door holds (as its get-or-create initiation authority, its
/// direct-line authority, and its send unit-of-work), the SAME `ScopeEventHub`
/// (winner-only pair-opened publication), and the SAME required `MessagingAftermath`.
///
/// Two composition obligations the leaf router/adapter tests cannot prove:
/// * the route is MOUNTED behind the perimeter and reaches the composition's real
///   adapters: an anonymous POST is refused `401` by the perimeter's `.layer` before
///   routing (the live store showing no pair and no message is a zero-SIDE-EFFECT
///   corroboration only — NOT a 0-call proof, since a read-then-write authority could
///   also leave 0/0; the auth-before-authority order is proven by
///   `the_direct_pair_initiation_authority_is_consulted_only_after_the_perimeter`, and
///   the port's zero consult on missing auth by the leaf counting test);
/// * an admitted viewer opening a pair with an agent they own reaches the live SQLite
///   mint through the real hub/aftermath (`201`), and the new pair then appears in the
///   composed cold `GET /api/direct-pairs` read that was empty before — write→read both
///   off the one production store. A second POST returns the SAME pair (`201`, still ONE
///   pair in the cold list) yet still sends a second message — get-or-create idempotency,
///   not a rollback. (That the FIRST send additionally announces the winner pair-open and
///   BOTH sends run the real aftermath is proven on the event WS by
///   `the_composition_delivers_the_pair_open_and_the_aftermath_to_the_real_hubs`; row
///   counts here prove neither.) A one-method checksum pins the path to POST only.
///
/// The per-branch refusal matrix (the two size-gate field shapes, reply-target-before-
/// content-cap ordering, the winner-only LIVE announce) stays with the door's own
/// router/adapter tests; this gate proves the route is MOUNTED and reaches the
/// composition's real adapters.
#[tokio::test]
async fn the_composition_opens_a_direct_pair_and_sends_behind_the_perimeter() {
    let fixture = fixture(true).await;
    seed_human(&fixture.path).await;
    seed_direct_pair_target(&fixture.path).await;
    let app = prepare(&config(fixture.path.clone())).await.unwrap();
    let router = app.router;
    let bearer = format!("Bearer {TOKEN}");
    let path = format!("/api/agents/{DP_TARGET_AGENT}/direct-messages");
    let pairs_sql = "SELECT COUNT(*) FROM conversations WHERE placement_kind = 'direct_pair'";

    // Zero-side-effect corroboration (NOT a 0-call proof): the anonymous POST is refused
    // by the perimeter before any product work, so the real store shows no pair and no
    // message. Auth-before-authority order and the port's zero consult live in their own
    // gates below.
    let (status, body) = request_with_body(
        &router,
        "POST",
        &path,
        None,
        Body::from(r#"{"content":"hi"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body, r#"{"error":"unauthorized"}"#);
    assert_eq!(
        count(&fixture.path, pairs_sql).await,
        0,
        "anon POST minted a pair"
    );
    assert_eq!(
        count(&fixture.path, "SELECT COUNT(*) FROM messages").await,
        0,
        "anon POST sent a message"
    );

    // Cold list is empty before the first admitted open.
    let (status, body) = request(&router, "GET", "/api/direct-pairs", Some(&bearer)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, r#"{"direct_pairs":[]}"#);

    // Admitted viewer opens the pair with an agent they own → the live mint + send.
    let (status, body) = request_with_body(
        &router,
        "POST",
        &path,
        Some(&bearer),
        Body::from(r#"{"content":"hello"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        count(&fixture.path, pairs_sql).await,
        1,
        "the winner mints one pair"
    );
    assert_eq!(
        count(&fixture.path, "SELECT COUNT(*) FROM messages").await,
        1,
        "the first message is sent"
    );

    // The new pair is now visible in the composed cold read — write→read off one store.
    let (status, body) = request(&router, "GET", "/api/direct-pairs", Some(&bearer)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let listed = serde_json::from_str::<serde_json::Value>(&body).unwrap();
    assert_eq!(
        listed["direct_pairs"].as_array().unwrap().len(),
        1,
        "{body}"
    );

    // Get-or-create idempotency: a second POST re-uses the SAME pair (still ONE) yet
    // still sends a second message — parity, not a rollback. (The winner-only pair-open
    // announce and the per-send aftermath are proven on the event WS gate below, not by
    // these row counts.)
    let (status, body) = request_with_body(
        &router,
        "POST",
        &path,
        Some(&bearer),
        Body::from(r#"{"content":"again"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        count(&fixture.path, pairs_sql).await,
        1,
        "get-or-create loser re-uses the pair, never a second"
    );
    assert_eq!(
        count(&fixture.path, "SELECT COUNT(*) FROM messages").await,
        2,
        "both paths send their first message"
    );

    // One-method checksum: the mounted path serves POST only.
    let (status, _) = request(&router, "GET", &path, Some(&bearer)).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
}

/// The direct-pair initiation authority is a read-then-write port, so a
/// zero write count can never prove it was not CONSULTED. This proves the auth-before-
/// authority ORDER on the real `prepare()` wiring, by a broken-authority differential:
/// after `prepare()`, DROP the ONE table the initiation authority reads (`agents`) — the
/// perimeter's own session tables are untouched — and drive the SAME broken tree two
/// ways. A missing OR unknown credential is still an EXACT `401` (auth refuses before the
/// now-broken authority is ever touched, so the break is invisible to it); the SAME
/// broken tree with a real credential clears the perimeter, reaches the authority, and
/// faults into a safe `500`. That `500` excludes the two false-greens a bare `401`
/// cannot: a route never mounted (would `404`) and a break that never took effect (would
/// `201`). A "consult-authority-then-reject-anon" mis-mount would surface the break on the
/// anonymous call too, so the exact anon `401` is the load-bearing discriminator.
#[tokio::test]
async fn the_direct_pair_initiation_authority_is_consulted_only_after_the_perimeter() {
    let fixture = fixture(true).await;
    seed_human(&fixture.path).await;
    seed_direct_pair_target(&fixture.path).await;
    let app = prepare(&config(fixture.path.clone())).await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let running = tokio::spawn(serve(listener, app, async move {
        let _ = shutdown_rx.await;
        Ok::<(), std::io::Error>(())
    }));
    let path = format!("/api/agents/{DP_TARGET_AGENT}/direct-messages");

    // Healthy first: the authenticated door works end-to-end, so a later 500 is caused by
    // the break we introduce and not a pre-existing fault.
    let healthy = http_over_tcp(
        address,
        "POST",
        &path,
        Some(&format!("Bearer {TOKEN}")),
        r#"{"content":"healthy"}"#,
    )
    .await;
    assert!(healthy.starts_with("HTTP/1.1 201 Created\r\n"), "{healthy}");

    // Break the ONE table the initiation authority reads (`SELECT ... FROM agents`).
    let pool = SqlitePoolOptions::new()
        .connect(fixture.path.to_str().unwrap())
        .await
        .unwrap();
    sqlx::query("DROP TABLE agents")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    // Missing AND unknown credential → EXACT 401 with the exact unauthorized envelope:
    // auth refuses before the now-broken authority is ever consulted, so the drop is
    // invisible on this path. Asserting the body (not just the status line) is what makes
    // "EXACT 401" true for BOTH branches — the missing-auth envelope proven on the mount
    // gate cannot stand in for the unknown-token branch.
    for bearer in [None, Some("Bearer not-a-real-token")] {
        let refused = http_over_tcp(address, "POST", &path, bearer, r#"{"content":"x"}"#).await;
        assert!(
            refused.starts_with("HTTP/1.1 401 Unauthorized\r\n"),
            "an unauthenticated POST must be 401 regardless of authority health: {refused}"
        );
        assert_eq!(
            http_json_body(&refused),
            serde_json::json!({ "error": "unauthorized" }),
            "the refusal must carry the exact unauthorized envelope, not a leaked fault: {refused}"
        );
    }

    // SAME broken tree, real credential: the perimeter admits, the authority IS reached
    // and faults on the dropped table → safe 500. Not 404 (route mounted), not 201/created
    // (the break is effective).
    let broken = http_over_tcp(
        address,
        "POST",
        &path,
        Some(&format!("Bearer {TOKEN}")),
        r#"{"content":"x"}"#,
    )
    .await;
    assert!(
        broken.starts_with("HTTP/1.1 500 Internal Server Error\r\n"),
        "an authenticated POST must reach the broken authority and fail safe: {broken}"
    );

    shutdown_tx.send(()).unwrap();
    running.await.unwrap().unwrap();
}

/// DUAL-AXIS causal proof that `prepare()` wires the REAL hub and
/// the REAL `MessagingAftermath` to this write door (not a no-op / second hub). Row
/// counts and the leaf `Opened` counter cannot see the activation's composition seam;
/// these two live event-WS axes can, and each reddens one mutation BY NAME:
///
/// * CONVERSATION scope carries the aftermath. Every committed send publishes its own
///   `message` frame there — a kind ONLY the aftermath produces (no-mention sends emit no
///   viewer `Notification`). Swapping the mounted aftermath for `NoAftermath` drops this
///   axis's high-water to 0, reddening the aftermath seam on its own axis.
/// * VIEWER scope carries the winner-only pair-open sink. `direct_pairs_changed` is a
///   same-shape funnel — mint, direct-line invalidation, AND unread all publish the
///   identical `{conversation_id}` frame — so a kind-set assertion has ZERO discriminating
///   power here (Lio). The axis is pinned by two ABSOLUTE increments instead: the first
///   POST adds exactly `N` viewer frames, the second (loser) exactly `M`, each measured
///   against real production behavior. `N == M + 1` (explanation only) is the winner-only
///   pair-open; a no-op / second-hub opened sink drops ONLY the first increment to `N-1`
///   with `M` unchanged, so the first absolute count reddens alone. Pinning both absolutes
///   (not just their difference) means any drift in another same-kind producer reddens a
///   named count instead of silently re-tuning the gate.
///
/// Both axes read their high-water from a FRESH subscription's `subscribed.seq`, which is
/// the hub's own deterministic count and seals the "under-read / extra frame left queued"
/// false-green.
#[tokio::test]
async fn the_composition_delivers_the_pair_open_and_the_aftermath_to_the_real_hubs() {
    let fixture = fixture(true).await;
    seed_human(&fixture.path).await;
    seed_direct_pair_target(&fixture.path).await;
    let app = prepare(&config(fixture.path.clone())).await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let running = tokio::spawn(serve(listener, app, async move {
        let _ = shutdown_rx.await;
        Ok::<(), std::io::Error>(())
    }));
    let path = format!("/api/agents/{DP_TARGET_AGENT}/direct-messages");
    let bearer = format!("Bearer {TOKEN}");

    // Baseline: nothing published to the viewer's list yet.
    assert_eq!(fresh_scope_high_water(address, "viewer:self").await, 0);

    // First POST — winner: mints the pair AND sends the first message.
    let first = http_over_tcp(
        address,
        "POST",
        &path,
        Some(&bearer),
        r#"{"content":"first"}"#,
    )
    .await;
    assert!(first.starts_with("HTTP/1.1 201 Created\r\n"), "{first}");
    let viewer_after_first = fresh_scope_high_water(address, "viewer:self").await;

    // The conversation the winner minted names the scope the aftermath's `message` rides.
    let conversation = direct_pair_conversation_id(&fixture.path).await;
    let conversation_scope = format!("conversation:{conversation}");
    let conversation_after_first = fresh_scope_high_water(address, &conversation_scope).await;

    // Second POST — loser: get-or-creates the SAME pair, sends a second message, and does
    // NOT re-announce the pair-open.
    let second = http_over_tcp(
        address,
        "POST",
        &path,
        Some(&bearer),
        r#"{"content":"second"}"#,
    )
    .await;
    assert!(second.starts_with("HTTP/1.1 201 Created\r\n"), "{second}");
    let viewer_after_second = fresh_scope_high_water(address, "viewer:self").await;
    let conversation_after_second = fresh_scope_high_water(address, &conversation_scope).await;

    let first_viewer_increment = viewer_after_first;
    let second_viewer_increment = viewer_after_second - viewer_after_first;
    let first_conversation_increment = conversation_after_first;
    let second_conversation_increment = conversation_after_second - conversation_after_first;

    // CONVERSATION axis — the aftermath seam. Each send announces exactly one `message`
    // frame; `NoAftermath` would leave both increments at 0.
    assert_eq!(
        first_conversation_increment, 1,
        "the first send's aftermath must announce exactly one conversation `message` frame"
    );
    assert_eq!(
        second_conversation_increment, 1,
        "the second send's aftermath must announce exactly one conversation `message` frame"
    );

    // VIEWER axis — the winner-only pair-open sink. Two ABSOLUTE increments, pinned to the
    // measured production behavior; `N == M + 1` is the winner-only pair-open.
    assert_eq!(
        first_viewer_increment, 3,
        "first POST viewer frames: winner pair-open + direct-pair invalidation + latest summary"
    );
    assert_eq!(
        second_viewer_increment, 2,
        "second POST viewer frames: direct-pair invalidation + latest summary (no re-announce)"
    );
    assert_eq!(
        first_viewer_increment,
        second_viewer_increment + 1,
        "the winner-only pair-open is exactly one viewer frame more on the first send"
    );

    shutdown_tx.send(()).unwrap();
    running.await.unwrap().unwrap();
}

/// Wave 3A checkpoint A1: the five channel directory/detail read doors, mounted in
/// `prepare()` behind the SAME human perimeter and wired to the ONE production
/// `MessagingStore` the send door holds (`.channel_directory()`,
/// `.channel_lifecycle()`, `.conversation_agent_directory()` — pool clones, no second
/// authority). Two composition obligations the leaf factories' own tests cannot
/// prove:
///
/// * `the_composition_serves_channel_directory_and_detail_behind_the_perimeter`
///   drives every one of the five through the real composed root: anonymous → the
///   perimeter's `401`; an admitted workspace member → a real `200` read off the live
///   SQLite adapter (the active list and the detail door carry the seeded
///   conversation id, proving the store adapter — not a stub — answered), plus a
///   one-method checksum on the detail path.
/// * `the_channel_perimeter_refuses_before_touching_the_product_port` is the
///   load-bearing refusal gate: it mounts the SAME three factories behind the SAME
///   `require_human_session` over a counting spy port, and proves an anonymous read
///   is refused `401` with the product port invoked ZERO times — the perimeter
///   `.layer` runs before routing, so a handler (and its port call) never executes.
///   A `401` status alone cannot prove this; the call counter can.
mod channel_directory_activation {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use silverhand_backend_core::{
        conversation_messaging::messaging::collaboration::{
            channel_directory::{
                ActiveChannelDirectoryPort, ActiveWorkspaceChannelDirectory, ArchivedSince,
                ArchivedWorkspaceChannelDirectory, ArchivedWorkspaceChannelDirectoryPort,
                GlobalArchivedChannelDirectory, GlobalArchivedChannelDirectoryPort,
                GlobalArchivedDirectoryRefusal, WorkspaceDirectoryRefusal,
            },
            channel_lifecycle::{ChannelDetailFact, ChannelDetailReadPort, ChannelDetailRefusal},
            conversation_agent_directory::{
                ConversationAgentDirectory, ConversationAgentDirectoryReadPort,
                ConversationAgentDirectoryRefusal,
            },
        },
        foundation::identity::{ConversationId, UserId, WorkspaceId},
        identity_access::auth::{HumanSessionAuthenticator, SessionTtl, SystemSessionClock},
    };
    use silverhand_backend_server::{
        conversation_messaging::messaging::collaboration::{
            channel_directory, channel_lifecycle, conversation_agent_directory,
        },
        identity_access::auth::{PublicJsonUnauthorized, require_human_session},
    };
    use silverhand_backend_sqlite::identity_access::auth::SqliteSessionStore;

    use super::*;

    #[tokio::test]
    async fn the_composition_serves_channel_directory_and_detail_behind_the_perimeter() {
        let fixture = fixture(true).await;
        seed_human(&fixture.path).await;
        seed_channel_directory(&fixture.path).await;
        let app = prepare(&config(fixture.path.clone())).await.unwrap();
        let router = app.router;
        let bearer = format!("Bearer {TOKEN}");

        let workspace_channels = format!("/api/workspaces/{WORKSPACE_ID}/channels");
        let workspace_archived = format!("/api/workspaces/{WORKSPACE_ID}/channels/archived");
        let global_archived = "/api/archived-channels".to_owned();
        let channel_detail = format!("/api/channels/{CONVERSATION_ID}");
        let channel_agents = format!("/api/channels/{CONVERSATION_ID}/agents");

        // Anonymous → the perimeter's refusal envelope on every door, never a read.
        for path in [
            &workspace_channels,
            &workspace_archived,
            &global_archived,
            &channel_detail,
            &channel_agents,
        ] {
            let (status, body) = request(&router, "GET", path, None).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "anon {path}: {body}");
            assert_eq!(body, r#"{"error":"unauthorized"}"#, "anon {path}");
        }

        // Admitted workspace member → a real `200` off the live SQLite adapter; the
        // active list and the detail door both carry the seeded conversation id.
        let (status, body) = request(&router, "GET", &workspace_channels, Some(&bearer)).await;
        assert_eq!(status, StatusCode::OK, "active channels: {body}");
        assert!(
            body.contains(CONVERSATION_ID),
            "active list must carry the seeded channel off the real adapter: {body}"
        );

        let (status, body) = request(&router, "GET", &channel_detail, Some(&bearer)).await;
        assert_eq!(status, StatusCode::OK, "channel detail: {body}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["channel"]["id"].as_str(),
            Some(CONVERSATION_ID),
            "detail door must project the seeded channel id: {body}"
        );

        for path in [&workspace_archived, &global_archived, &channel_agents] {
            let (status, body) = request(&router, "GET", path, Some(&bearer)).await;
            assert_eq!(status, StatusCode::OK, "authed {path}: {body}");
        }

        // One-method checksum: the detail path serves GET only.
        let (status, body) = request(&router, "POST", &channel_detail, Some(&bearer)).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{body}");
    }

    /// A spy satisfying all five A1 read ports; every method bumps a shared counter
    /// then diverges. Reaching any of them past the perimeter is a bug, so the bodies
    /// never need to fabricate a success value.
    #[derive(Clone, Default)]
    struct RefusalCountingPort {
        calls: Arc<AtomicUsize>,
    }

    impl ActiveChannelDirectoryPort for RefusalCountingPort {
        type Error = std::convert::Infallible;
        async fn active(
            &self,
            _: &UserId,
            _: &WorkspaceId,
        ) -> Result<Result<ActiveWorkspaceChannelDirectory, WorkspaceDirectoryRefusal>, Self::Error>
        {
            self.calls.fetch_add(1, Ordering::SeqCst);
            unreachable!("perimeter must refuse the anonymous read before the product port")
        }
    }
    impl ArchivedWorkspaceChannelDirectoryPort for RefusalCountingPort {
        type Error = std::convert::Infallible;
        async fn archived(
            &self,
            _: &UserId,
            _: &WorkspaceId,
        ) -> Result<Result<ArchivedWorkspaceChannelDirectory, WorkspaceDirectoryRefusal>, Self::Error>
        {
            self.calls.fetch_add(1, Ordering::SeqCst);
            unreachable!("perimeter must refuse the anonymous read before the product port")
        }
    }
    impl GlobalArchivedChannelDirectoryPort for RefusalCountingPort {
        type Error = std::convert::Infallible;
        async fn archived_global(
            &self,
            _: &UserId,
            _: Option<&WorkspaceId>,
            _: Option<ArchivedSince>,
        ) -> Result<
            Result<GlobalArchivedChannelDirectory, GlobalArchivedDirectoryRefusal>,
            Self::Error,
        > {
            self.calls.fetch_add(1, Ordering::SeqCst);
            unreachable!("perimeter must refuse the anonymous read before the product port")
        }
    }
    impl ChannelDetailReadPort for RefusalCountingPort {
        type Error = std::convert::Infallible;
        async fn detail(
            &self,
            _: &UserId,
            _: &ConversationId,
        ) -> Result<Result<ChannelDetailFact, ChannelDetailRefusal>, Self::Error> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            unreachable!("perimeter must refuse the anonymous read before the product port")
        }
    }
    impl ConversationAgentDirectoryReadPort for RefusalCountingPort {
        type Error = std::convert::Infallible;
        async fn read(
            &self,
            _: &UserId,
            _: &ConversationId,
        ) -> Result<
            Result<ConversationAgentDirectory, ConversationAgentDirectoryRefusal>,
            Self::Error,
        > {
            self.calls.fetch_add(1, Ordering::SeqCst);
            unreachable!("perimeter must refuse the anonymous read before the product port")
        }
    }

    #[tokio::test]
    async fn the_channel_perimeter_refuses_before_touching_the_product_port() {
        let fixture = fixture(false).await;
        let session_store = SqliteSessionStore::open(&fixture.path).await.unwrap();
        let sessions = HumanSessionAuthenticator::new(
            session_store,
            SystemSessionClock,
            SessionTtl::default(),
        );
        let spy = RefusalCountingPort::default();
        let calls = spy.calls.clone();
        // The SAME three factories `prepare()` mounts, behind the SAME perimeter.
        let router = require_human_session(
            channel_directory::router(spy.clone())
                .merge(channel_lifecycle::router(spy.clone()))
                .merge(conversation_agent_directory::router(spy)),
            sessions,
            PublicJsonUnauthorized,
        );

        for path in [
            format!("/api/workspaces/{WORKSPACE_ID}/channels"),
            format!("/api/workspaces/{WORKSPACE_ID}/channels/archived"),
            "/api/archived-channels".to_owned(),
            format!("/api/channels/{CONVERSATION_ID}"),
            format!("/api/channels/{CONVERSATION_ID}/agents"),
        ] {
            let (status, body) = request(&router, "GET", &path, None).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "anon {path}: {body}");
            assert_eq!(body, r#"{"error":"unauthorized"}"#, "anon {path}");
        }

        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "the perimeter must refuse every anonymous read before the product port is touched"
        );
    }
}

/// Wave 3A checkpoint A2: the four runtime/activity projection doors, mounted in
/// `prepare()` behind the SAME human perimeter. Three ride pool-clone `&self` reads
/// off the ONE production `MessagingStore` (`.conversation_agent_runtime()`,
/// `.conversation_agent_activity()`, `.activity_read_model()`); the run-state door
/// additionally joins the SAME live `daemon_runtime` owner already composed for
/// delivery — cloned, sharing the owner's `Arc<Mutex<..>>` state, never a second
/// runtime snapshot. Two composition obligations the leaf factories' own tests cannot
/// prove:
///
/// * `the_composition_serves_runtime_and_activity_behind_the_perimeter` drives every
///   one of the four through the real composed root: anonymous → the perimeter's
///   `401`; an admitted workspace member (whose committed agent seat is seeded) → a
///   real `200` off the live composed authorities. Each door's own success envelope
///   (`runtime`/`states`/`events`/`items`) proves the real adapter — not a stub —
///   answered, and the run-state door's `200` proves the joined SQLite admission and
///   live-owner snapshot both resolved. A one-method checksum pins the feed to GET.
/// * `the_runtime_activity_perimeter_refuses_before_touching_the_product_port` is the
///   load-bearing refusal gate: it mounts the SAME four factories behind the SAME
///   `require_human_session` over a counting spy port (which serves the run-state
///   door's admission AND owner snapshot), and proves an anonymous read is refused
///   `401` with the product port invoked ZERO times — the perimeter `.layer` runs
///   before routing, so a handler (and its port call) never executes. A `401` status
///   alone cannot prove this; the call counter can.
mod runtime_activity_activation {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use silverhand_backend_core::{
        conversation_messaging::messaging::collaboration::{
            activity::{ActivityList, ActivityReadModel},
            channel_lifecycle::{ChannelDetailFact, ChannelDetailReadPort, ChannelDetailRefusal},
            conversation_agent_activity::{
                ActivityPageRequest, ConversationAgentActivityPage,
                ConversationAgentActivityReadPort, ConversationAgentActivityRefusal,
            },
            conversation_agent_runtime::{
                ConversationAgentRuntimeProjection, ConversationAgentRuntimeReadPort,
                ConversationAgentRuntimeRefusal,
            },
        },
        foundation::identity::{AgentId, ConversationId, UserId, WorkspaceId},
        identity_access::auth::{HumanSessionAuthenticator, SessionTtl, SystemSessionClock},
    };
    use silverhand_backend_server::{
        conversation_messaging::messaging::collaboration::{
            activity, conversation_agent_activity, conversation_agent_run_states,
            conversation_agent_run_states::{
                AgentRunStateProjection, ConversationAgentRunStateSource,
            },
            conversation_agent_runtime,
        },
        identity_access::auth::{PublicJsonUnauthorized, require_human_session},
    };
    use silverhand_backend_sqlite::identity_access::auth::SqliteSessionStore;

    use super::*;

    const AGENT_ID: &str = "agent-1";

    /// Seed the target agent's committed participation for the conversation
    /// `seed_channel_directory` created: a host, the agent, and its conversation seat.
    /// No live runtime slot and no activity event are seeded, so the run-state,
    /// activity, and feed doors answer an empty projection while the runtime door
    /// projects the committed seat as a single idle row — each the real adapter's
    /// honest answer, none a fabricated fact.
    async fn seed_channel_agent(path: &Path) {
        let pool = SqlitePoolOptions::new()
            .connect(path.to_str().unwrap())
            .await
            .unwrap();
        sqlx::raw_sql(
                "INSERT INTO hosts(id,name,kind,created_at) VALUES ('host-1','host-1','app',1); \
                 INSERT INTO agents(id,name,runtime,host_id,created_at,updated_at,created_by_user_id) \
                   VALUES ('agent-1','agent-1','claude','host-1',1,1,'user-1'); \
                 INSERT INTO conversation_agents(conversation_id,agent_id,joined_at) VALUES \
                   ('conv000000000001','agent-1',1);",
            )
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
    }

    #[tokio::test]
    async fn the_composition_serves_runtime_and_activity_behind_the_perimeter() {
        let fixture = fixture(true).await;
        seed_human(&fixture.path).await;
        seed_channel_directory(&fixture.path).await;
        seed_channel_agent(&fixture.path).await;
        let app = prepare(&config(fixture.path.clone())).await.unwrap();
        let router = app.router;
        let bearer = format!("Bearer {TOKEN}");

        let agent_runtime = format!("/api/channels/{CONVERSATION_ID}/agent-runtime");
        let agent_run_states = format!("/api/channels/{CONVERSATION_ID}/agent-run-states");
        let agent_activity =
            format!("/api/conversations/{CONVERSATION_ID}/agents/{AGENT_ID}/activity");
        let activity = "/api/activity".to_owned();

        // Anonymous → the perimeter's refusal envelope on every door, never a read.
        for path in [
            &agent_runtime,
            &agent_run_states,
            &agent_activity,
            &activity,
        ] {
            let (status, body) = request(&router, "GET", path, None).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "anon {path}: {body}");
            assert_eq!(body, r#"{"error":"unauthorized"}"#, "anon {path}");
        }

        // Admitted member → a real `200` off the live composed authorities; each
        // door's own success envelope proves the real adapter (not a stub) answered.
        // The committed-roster runtime projection emits an idle row for the seeded
        // agent seat even with no live slot, so a stub could never fabricate it.
        let (status, body) = request(&router, "GET", &agent_runtime, Some(&bearer)).await;
        assert_eq!(status, StatusCode::OK, "agent-runtime: {body}");
        let runtime = serde_json::from_str::<serde_json::Value>(&body).unwrap();
        assert_eq!(
            runtime["runtime"][0]["agent_id"].as_str(),
            Some(AGENT_ID),
            "agent-runtime projects the committed roster off the real SQLite adapter: {body}"
        );
        assert_eq!(
            runtime["runtime"][0]["status"].as_str(),
            Some("idle"),
            "the seeded seat with no live slot projects the idle runtime status: {body}"
        );

        // The run-state door joins two live authorities: SQLite admission (member)
        // and the SAME `daemon_runtime` owner's snapshot (empty in a fresh owner).
        let (status, body) = request(&router, "GET", &agent_run_states, Some(&bearer)).await;
        assert_eq!(status, StatusCode::OK, "agent-run-states: {body}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["states"],
            serde_json::json!([]),
            "agent-run-states rides the live daemon owner snapshot: {body}"
        );

        let (status, body) = request(&router, "GET", &agent_activity, Some(&bearer)).await;
        assert_eq!(status, StatusCode::OK, "agent-activity: {body}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["events"],
            serde_json::json!([]),
            "agent-activity rides the persisted-event SQLite projection: {body}"
        );

        let (status, body) = request(&router, "GET", &activity, Some(&bearer)).await;
        assert_eq!(status, StatusCode::OK, "activity: {body}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["items"],
            serde_json::json!([]),
            "activity rides the viewer Activity cold projection: {body}"
        );

        // One-method checksum: the activity feed path serves GET only.
        let (status, body) = request(&router, "POST", &activity, Some(&bearer)).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{body}");
    }

    /// A spy satisfying the four A2 doors' read ports (the run-state door needs two —
    /// the SQLite admission and the live-owner snapshot); every method bumps a shared
    /// counter then diverges. Reaching any of them past the perimeter is a bug, so the
    /// bodies never need to fabricate a success value.
    #[derive(Clone, Default)]
    struct RuntimeActivityCountingPort {
        calls: Arc<AtomicUsize>,
    }

    impl ConversationAgentRuntimeReadPort for RuntimeActivityCountingPort {
        type Error = std::convert::Infallible;
        async fn read(
            &self,
            _: &UserId,
            _: &ConversationId,
        ) -> Result<
            Result<ConversationAgentRuntimeProjection, ConversationAgentRuntimeRefusal>,
            Self::Error,
        > {
            self.calls.fetch_add(1, Ordering::SeqCst);
            unreachable!("perimeter must refuse the anonymous read before the product port")
        }
    }
    impl ChannelDetailReadPort for RuntimeActivityCountingPort {
        type Error = std::convert::Infallible;
        async fn detail(
            &self,
            _: &UserId,
            _: &ConversationId,
        ) -> Result<Result<ChannelDetailFact, ChannelDetailRefusal>, Self::Error> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            unreachable!("perimeter must refuse the anonymous read before the product port")
        }
    }
    impl ConversationAgentRunStateSource for RuntimeActivityCountingPort {
        type Error = std::convert::Infallible;
        async fn snapshot(
            &self,
            _: &ConversationId,
        ) -> Result<Vec<AgentRunStateProjection>, Self::Error> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            unreachable!("perimeter must refuse the anonymous read before the product port")
        }
    }
    impl ConversationAgentActivityReadPort for RuntimeActivityCountingPort {
        type Error = std::convert::Infallible;
        async fn admit(
            &self,
            _: &UserId,
            _: &ConversationId,
            _: &AgentId,
        ) -> Result<Result<(), ConversationAgentActivityRefusal>, Self::Error> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            unreachable!("perimeter must refuse the anonymous read before the product port")
        }
        async fn read(
            &self,
            _: &UserId,
            _: &ConversationId,
            _: &AgentId,
            _: ActivityPageRequest,
        ) -> Result<
            Result<ConversationAgentActivityPage, ConversationAgentActivityRefusal>,
            Self::Error,
        > {
            self.calls.fetch_add(1, Ordering::SeqCst);
            unreachable!("perimeter must refuse the anonymous read before the product port")
        }
    }
    impl ActivityReadModel for RuntimeActivityCountingPort {
        type Error = std::convert::Infallible;
        async fn list_activity(
            &self,
            _: &UserId,
            _: Option<&WorkspaceId>,
        ) -> Result<ActivityList, Self::Error> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            unreachable!("perimeter must refuse the anonymous read before the product port")
        }
    }

    #[tokio::test]
    async fn the_runtime_activity_perimeter_refuses_before_touching_the_product_port() {
        let fixture = fixture(false).await;
        let session_store = SqliteSessionStore::open(&fixture.path).await.unwrap();
        let sessions = HumanSessionAuthenticator::new(
            session_store,
            SystemSessionClock,
            SessionTtl::default(),
        );
        let spy = RuntimeActivityCountingPort::default();
        let calls = spy.calls.clone();
        // The SAME four factories `prepare()` mounts, behind the SAME perimeter. The
        // run-state door takes the spy for BOTH its admission and its owner snapshot.
        let router = require_human_session(
            conversation_agent_runtime::router(spy.clone())
                .merge(conversation_agent_run_states::router(
                    spy.clone(),
                    spy.clone(),
                ))
                .merge(conversation_agent_activity::router(spy.clone()))
                .merge(activity::router(spy)),
            sessions,
            PublicJsonUnauthorized,
        );

        for path in [
            format!("/api/channels/{CONVERSATION_ID}/agent-runtime"),
            format!("/api/channels/{CONVERSATION_ID}/agent-run-states"),
            format!("/api/conversations/{CONVERSATION_ID}/agents/{AGENT_ID}/activity"),
            "/api/activity".to_owned(),
        ] {
            let (status, body) = request(&router, "GET", &path, None).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "anon {path}: {body}");
            assert_eq!(body, r#"{"error":"unauthorized"}"#, "anon {path}");
        }

        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "the perimeter must refuse every anonymous read before the product port is touched"
        );
    }
}

/// The five Wave 3A message-navigation and search reads, mounted behind the SAME
/// `require_human_session` perimeter in the real composed root. Two composition
/// obligations the leaf factories' own tests cannot prove:
///
/// * `the_composition_serves_message_nav_and_search_behind_the_perimeter` drives all
///   five through the real `prepare()`: anonymous → the perimeter's `401`; an admitted
///   member → a real `200` off each door's live SQLite adapter, asserted by each door's
///   OWN success envelope (`ref`/`mainline`/`threads`/`messages`/`results`). It also
///   proves the load-bearing A3 coexistence gate: `GET /api/channels/{id}/messages` and
///   the already-mounted `POST` on the SAME path each reach their OWN handler — the POST
///   commits a send (201 + its own ACK echoing the content) and the GET then reads that
///   live write back through the window projection (200 + a `messages` row carrying the
///   just-committed content). Neither shadows nor replaces the other, and neither answer
///   could come from a shared fallback: a send ACK has no `messages` array and a window
///   read never returns 201.
/// * `the_message_nav_and_search_perimeter_refuses_before_touching_the_product_port`
///   mounts the SAME five factories behind the SAME perimeter over a counting spy and
///   proves an anonymous read is refused `401` with every product port invoked ZERO
///   times — the `.layer` runs before routing, so no handler (and no port call) executes.
///   A `401` status alone cannot prove this; the call counter can.
mod message_nav_search_activation {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use silverhand_backend_core::{
        conversation_messaging::messaging::collaboration::{
            conversation_message_window::{
                ConversationMessageWindow, ConversationMessageWindowReadPort,
                ConversationMessageWindowRequest, MessageWindowRefusal,
            },
            conversation_ref::{
                ConversationRefReadPort, ConversationRefRefusal, WorkspaceConversationRef,
            },
            conversation_threads::{
                ConversationThreadsPage, ConversationThreadsReadPort, ConversationThreadsRefusal,
                ThreadListQuery,
            },
            conversation_unread::{
                ConversationUnread, ConversationUnreadReadPort, ConversationUnreadRefusal,
            },
            human_search::{
                HumanSearchPage, HumanSearchQuery, HumanSearchReadPort, HumanSearchRefusal,
            },
        },
        foundation::identity::{ConversationId, UserId},
        identity_access::auth::{HumanSessionAuthenticator, SessionTtl, SystemSessionClock},
    };
    use silverhand_backend_server::{
        conversation_messaging::messaging::collaboration::{
            conversation_message_window, conversation_ref, conversation_threads,
            conversation_unread, human_search,
        },
        identity_access::auth::{PublicJsonUnauthorized, require_human_session},
    };
    use silverhand_backend_sqlite::identity_access::auth::SqliteSessionStore;

    use super::*;

    #[tokio::test]
    async fn the_composition_serves_message_nav_and_search_behind_the_perimeter() {
        let fixture = fixture(true).await;
        seed_human(&fixture.path).await;
        seed_channel_directory(&fixture.path).await;
        let app = prepare(&config(fixture.path.clone())).await.unwrap();
        let router = app.router;
        let bearer = format!("Bearer {TOKEN}");

        let conversation_ref = format!("/api/conversations/{CONVERSATION_ID}/ref");
        let unread = format!("/api/conversations/{CONVERSATION_ID}/unread");
        let threads = format!("/api/conversations/{CONVERSATION_ID}/threads");
        let messages = format!("/api/channels/{CONVERSATION_ID}/messages");
        let search = "/api/search?q=General".to_owned();

        // Anonymous → the perimeter's refusal envelope on every door, never a read.
        for path in [&conversation_ref, &unread, &threads, &messages, &search] {
            let (status, body) = request(&router, "GET", path, None).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "anon GET {path}: {body}");
            assert_eq!(body, r#"{"error":"unauthorized"}"#, "anon GET {path}");
        }
        // The POST on the shared messages path is guarded by the SAME perimeter: an
        // anonymous send is refused before method dispatch, so the coexistence is real
        // on both methods rather than a GET-only mount.
        let (status, body) = request_with_body(
            &router,
            "POST",
            &messages,
            None,
            Body::from(r#"{"content":"anon"}"#),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "anon POST messages: {body}"
        );

        // ref → its own `{"ref":{...}}` envelope off the real SQLite adapter.
        let (status, body) = request(&router, "GET", &conversation_ref, Some(&bearer)).await;
        assert_eq!(status, StatusCode::OK, "ref: {body}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["ref"]["id"].as_str(),
            Some(CONVERSATION_ID),
            "ref resolves the seeded conversation off the real adapter: {body}"
        );

        // unread → its own `{"mainline":...}` envelope.
        let (status, body) = request(&router, "GET", &unread, Some(&bearer)).await;
        assert_eq!(status, StatusCode::OK, "unread: {body}");
        assert!(
            serde_json::from_str::<serde_json::Value>(&body)
                .unwrap()
                .get("mainline")
                .is_some(),
            "unread projects its mainline stream off the real adapter: {body}"
        );

        // threads → its own `{"threads":[...]}` envelope (empty page, honestly read).
        let (status, body) = request(&router, "GET", &threads, Some(&bearer)).await;
        assert_eq!(status, StatusCode::OK, "threads: {body}");
        assert!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["threads"].is_array(),
            "threads projects its page off the real adapter: {body}"
        );

        // search → its own `{"results":[...]}` envelope.
        let (status, body) = request(&router, "GET", &search, Some(&bearer)).await;
        assert_eq!(status, StatusCode::OK, "search: {body}");
        assert!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["results"].is_array(),
            "search projects its result list off the real adapter: {body}"
        );

        // THE COEXISTENCE GATE. Same path, two methods, two handlers, two observables.
        // POST commits a send: 201 and the send ACK, whose `message.content` echoes the
        // request — an answer only the send handler produces.
        let (status, body) = request_with_body(
            &router,
            "POST",
            &messages,
            Some(&bearer),
            Body::from(r#"{"content":"hello A3"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "POST messages: {body}");
        let ack = serde_json::from_str::<serde_json::Value>(&body).unwrap();
        assert_eq!(
            ack["message"]["content"].as_str(),
            Some("hello A3"),
            "the POST entered the send handler and committed the content: {body}"
        );
        assert!(
            ack.get("messages").is_none(),
            "the send ACK is NOT the window envelope — no shared fallback: {body}"
        );

        // GET on the SAME path reads that live write back: 200 and a `messages` array
        // containing the just-committed content — an answer only the window handler
        // produces, and one that proves it read the real committed row, not a stub.
        let (status, body) = request(&router, "GET", &messages, Some(&bearer)).await;
        assert_eq!(status, StatusCode::OK, "GET messages: {body}");
        let window = serde_json::from_str::<serde_json::Value>(&body).unwrap();
        let rows = window["messages"]
            .as_array()
            .expect("the window handler answers with a messages array");
        assert!(
            rows.iter()
                .any(|row| row["content"].as_str() == Some("hello A3")),
            "the GET entered the window handler and read the committed send back: {body}"
        );
    }

    /// A spy satisfying the five A3 doors' read ports; every method bumps a shared
    /// counter then diverges. Reaching any past the perimeter is a bug, so the bodies
    /// never fabricate a success value.
    #[derive(Clone, Default)]
    struct MessageNavSearchCountingPort {
        calls: Arc<AtomicUsize>,
    }

    impl ConversationRefReadPort for MessageNavSearchCountingPort {
        type Error = std::convert::Infallible;
        async fn resolve(
            &self,
            _: &UserId,
            _: &ConversationId,
        ) -> Result<Result<WorkspaceConversationRef, ConversationRefRefusal>, Self::Error> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            unreachable!("perimeter must refuse the anonymous read before the product port")
        }
    }
    impl ConversationUnreadReadPort for MessageNavSearchCountingPort {
        type Error = std::convert::Infallible;
        async fn read(
            &self,
            _: &UserId,
            _: &ConversationId,
        ) -> Result<Result<ConversationUnread, ConversationUnreadRefusal>, Self::Error> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            unreachable!("perimeter must refuse the anonymous read before the product port")
        }
    }
    impl ConversationThreadsReadPort for MessageNavSearchCountingPort {
        type Error = std::convert::Infallible;
        async fn admit(
            &self,
            _: &UserId,
            _: &ConversationId,
        ) -> Result<Result<(), ConversationThreadsRefusal>, Self::Error> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            unreachable!("perimeter must refuse the anonymous read before the product port")
        }
        async fn read(
            &self,
            _: &UserId,
            _: &ConversationId,
            _: ThreadListQuery,
        ) -> Result<Result<ConversationThreadsPage, ConversationThreadsRefusal>, Self::Error>
        {
            self.calls.fetch_add(1, Ordering::SeqCst);
            unreachable!("perimeter must refuse the anonymous read before the product port")
        }
    }
    impl ConversationMessageWindowReadPort for MessageNavSearchCountingPort {
        type Error = std::convert::Infallible;
        async fn read(
            &self,
            _: &UserId,
            _: &ConversationId,
            _: ConversationMessageWindowRequest,
        ) -> Result<Result<ConversationMessageWindow, MessageWindowRefusal>, Self::Error> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            unreachable!("perimeter must refuse the anonymous read before the product port")
        }
    }
    impl HumanSearchReadPort for MessageNavSearchCountingPort {
        type Error = std::convert::Infallible;
        async fn search(
            &self,
            _: &UserId,
            _: HumanSearchQuery,
        ) -> Result<Result<HumanSearchPage, HumanSearchRefusal>, Self::Error> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            unreachable!("perimeter must refuse the anonymous read before the product port")
        }
    }

    #[tokio::test]
    async fn the_message_nav_and_search_perimeter_refuses_before_touching_the_product_port() {
        let fixture = fixture(false).await;
        let session_store = SqliteSessionStore::open(&fixture.path).await.unwrap();
        let sessions = HumanSessionAuthenticator::new(
            session_store,
            SystemSessionClock,
            SessionTtl::default(),
        );
        let spy = MessageNavSearchCountingPort::default();
        let calls = spy.calls.clone();
        // The SAME five factories `prepare()` mounts, behind the SAME perimeter.
        let router = require_human_session(
            conversation_ref::router(spy.clone())
                .merge(conversation_unread::router(spy.clone()))
                .merge(conversation_threads::router(spy.clone()))
                .merge(conversation_message_window::router(spy.clone()))
                .merge(human_search::router(spy)),
            sessions,
            PublicJsonUnauthorized,
        );

        for path in [
            format!("/api/conversations/{CONVERSATION_ID}/ref"),
            format!("/api/conversations/{CONVERSATION_ID}/unread"),
            format!("/api/conversations/{CONVERSATION_ID}/threads"),
            format!("/api/channels/{CONVERSATION_ID}/messages"),
            "/api/search?q=General".to_owned(),
        ] {
            let (status, body) = request(&router, "GET", &path, None).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "anon {path}: {body}");
            assert_eq!(body, r#"{"error":"unauthorized"}"#, "anon {path}");
        }

        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "the perimeter must refuse every anonymous read before the product port is touched"
        );
    }
}

/// The five Wave 3A unread/attention reads, mounted behind the SAME
/// `require_human_session` perimeter in the real composed root. Two composition
/// obligations the leaf factories' own tests cannot prove:
///
/// * `the_composition_serves_unread_and_attention_behind_the_perimeter` drives all five
///   through the real `prepare()`: anonymous → the perimeter's `401`; an admitted member
///   → a real `200` off each door's live SQLite adapter, asserted by each door's OWN
///   success envelope (`by_conversation`/`by_workspace`/`by_workspace`+`muted_workspace_ids`/
///   `channels`). A one-method checksum pins the badges door to GET.
/// * `the_unread_and_attention_perimeter_refuses_before_touching_the_product_port` mounts
///   the SAME two factories behind the SAME perimeter over a counting spy implementing all
///   four read ports and proves an anonymous read is refused `401` with every product port
///   invoked ZERO times — the `.layer` runs before routing. A `401` status alone cannot
///   prove this; the call counter can.
mod unread_attention_activation {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use silverhand_backend_core::{
        conversation_messaging::messaging::collaboration::{
            attention_projection::{
                AttentionBadgeRollup, AttentionModesRefusal, AttentionProjectionPort,
                WorkspaceAttentionModes,
            },
            message_unread::{
                ConversationMessageUnread, ConversationMessageUnreadReadPort,
                ConversationMessageUnreadRefusal, ViewerMessageUnreadSummary,
                ViewerMessageUnreadSummaryReadPort, WorkspaceMessageUnread,
                WorkspaceMessageUnreadReadPort, WorkspaceMessageUnreadRefusal,
            },
        },
        foundation::identity::{ConversationId, UserId, WorkspaceId},
        identity_access::auth::{HumanSessionAuthenticator, SessionTtl, SystemSessionClock},
    };
    use silverhand_backend_server::{
        conversation_messaging::messaging::collaboration::{attention_projection, message_unread},
        identity_access::auth::{PublicJsonUnauthorized, require_human_session},
    };
    use silverhand_backend_sqlite::identity_access::auth::SqliteSessionStore;

    use super::*;

    #[tokio::test]
    async fn the_composition_serves_unread_and_attention_behind_the_perimeter() {
        let fixture = fixture(true).await;
        seed_human(&fixture.path).await;
        seed_channel_directory(&fixture.path).await;
        let app = prepare(&config(fixture.path.clone())).await.unwrap();
        let router = app.router;
        let bearer = format!("Bearer {TOKEN}");

        let workspace_unread = format!("/api/workspaces/{WORKSPACE_ID}/message-unread");
        let summary = "/api/message-unread-summary".to_owned();
        let conversation_unread = format!("/api/channels/{CONVERSATION_ID}/message-unread");
        let badges = "/api/attention/badges".to_owned();
        let modes = format!("/api/attention/modes?workspace_id={WORKSPACE_ID}");

        // Anonymous → the perimeter's refusal envelope on every door, never a read.
        for path in [
            &workspace_unread,
            &summary,
            &conversation_unread,
            &badges,
            &modes,
        ] {
            let (status, body) = request(&router, "GET", path, None).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "anon GET {path}: {body}");
            assert_eq!(body, r#"{"error":"unauthorized"}"#, "anon GET {path}");
        }

        // workspace roll-up → its own `{"by_conversation":..,"total":..}` envelope.
        let (status, body) = request(&router, "GET", &workspace_unread, Some(&bearer)).await;
        assert_eq!(status, StatusCode::OK, "workspace-message-unread: {body}");
        let value = serde_json::from_str::<serde_json::Value>(&body).unwrap();
        assert!(
            value.get("by_conversation").is_some() && value.get("total").is_some(),
            "workspace roll-up projects its own envelope off the real adapter: {body}"
        );

        // viewer summary → its own `{"by_workspace":..}` envelope.
        let (status, body) = request(&router, "GET", &summary, Some(&bearer)).await;
        assert_eq!(status, StatusCode::OK, "message-unread-summary: {body}");
        assert!(
            serde_json::from_str::<serde_json::Value>(&body)
                .unwrap()
                .get("by_workspace")
                .is_some(),
            "viewer summary projects its by_workspace map off the real adapter: {body}"
        );

        // per-conversation roll-up → its own `{"by_conversation":..,"total":..}` envelope.
        let (status, body) = request(&router, "GET", &conversation_unread, Some(&bearer)).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "conversation-message-unread: {body}"
        );
        let value = serde_json::from_str::<serde_json::Value>(&body).unwrap();
        assert!(
            value.get("by_conversation").is_some() && value.get("total").is_some(),
            "per-conversation roll-up projects its own envelope off the real adapter: {body}"
        );

        // attention badges → its own `{"by_workspace":..,"total":..,"muted_workspace_ids":..}`.
        let (status, body) = request(&router, "GET", &badges, Some(&bearer)).await;
        assert_eq!(status, StatusCode::OK, "attention-badges: {body}");
        assert!(
            serde_json::from_str::<serde_json::Value>(&body)
                .unwrap()
                .get("muted_workspace_ids")
                .is_some(),
            "attention badges projects its rollup off the real adapter: {body}"
        );

        // attention modes → its own `{"workspace_id":..,"muted":..,"channels":[..]}`
        // envelope, keyed to the requested workspace.
        let (status, body) = request(&router, "GET", &modes, Some(&bearer)).await;
        assert_eq!(status, StatusCode::OK, "attention-modes: {body}");
        let value = serde_json::from_str::<serde_json::Value>(&body).unwrap();
        assert_eq!(
            value["workspace_id"].as_str(),
            Some(WORKSPACE_ID),
            "attention modes binds its answer to the requested workspace: {body}"
        );
        assert!(
            value["channels"].is_array(),
            "attention modes projects its channel list off the real adapter: {body}"
        );

        // One-method checksum: the badges path serves GET only.
        let (status, body) = request(&router, "POST", &badges, Some(&bearer)).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{body}");
    }

    /// A spy satisfying the five A4 doors' four read ports; every method bumps a shared
    /// counter then diverges. Reaching any past the perimeter is a bug, so the bodies
    /// never fabricate a success value.
    #[derive(Clone, Default)]
    struct UnreadAttentionCountingPort {
        calls: Arc<AtomicUsize>,
    }

    impl WorkspaceMessageUnreadReadPort for UnreadAttentionCountingPort {
        type Error = std::convert::Infallible;
        async fn read_workspace(
            &self,
            _: &UserId,
            _: &WorkspaceId,
        ) -> Result<Result<WorkspaceMessageUnread, WorkspaceMessageUnreadRefusal>, Self::Error>
        {
            self.calls.fetch_add(1, Ordering::SeqCst);
            unreachable!("perimeter must refuse the anonymous read before the product port")
        }
    }
    impl ViewerMessageUnreadSummaryReadPort for UnreadAttentionCountingPort {
        type Error = std::convert::Infallible;
        async fn read_summary(
            &self,
            _: &UserId,
        ) -> Result<ViewerMessageUnreadSummary, Self::Error> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            unreachable!("perimeter must refuse the anonymous read before the product port")
        }
    }
    impl ConversationMessageUnreadReadPort for UnreadAttentionCountingPort {
        type Error = std::convert::Infallible;
        async fn read_conversation(
            &self,
            _: &UserId,
            _: &ConversationId,
        ) -> Result<Result<ConversationMessageUnread, ConversationMessageUnreadRefusal>, Self::Error>
        {
            self.calls.fetch_add(1, Ordering::SeqCst);
            unreachable!("perimeter must refuse the anonymous read before the product port")
        }
    }
    impl AttentionProjectionPort for UnreadAttentionCountingPort {
        type Error = std::convert::Infallible;
        async fn badges(&self, _: &UserId) -> Result<AttentionBadgeRollup, Self::Error> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            unreachable!("perimeter must refuse the anonymous read before the product port")
        }
        async fn modes(
            &self,
            _: &UserId,
            _: &WorkspaceId,
        ) -> Result<Result<WorkspaceAttentionModes, AttentionModesRefusal>, Self::Error> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            unreachable!("perimeter must refuse the anonymous read before the product port")
        }
    }

    #[tokio::test]
    async fn the_unread_and_attention_perimeter_refuses_before_touching_the_product_port() {
        let fixture = fixture(false).await;
        let session_store = SqliteSessionStore::open(&fixture.path).await.unwrap();
        let sessions = HumanSessionAuthenticator::new(
            session_store,
            SystemSessionClock,
            SessionTtl::default(),
        );
        let spy = UnreadAttentionCountingPort::default();
        let calls = spy.calls.clone();
        // The SAME two factories `prepare()` mounts, behind the SAME perimeter.
        let router = require_human_session(
            message_unread::router(spy.clone()).merge(attention_projection::router(spy)),
            sessions,
            PublicJsonUnauthorized,
        );

        for path in [
            format!("/api/workspaces/{WORKSPACE_ID}/message-unread"),
            "/api/message-unread-summary".to_owned(),
            format!("/api/channels/{CONVERSATION_ID}/message-unread"),
            "/api/attention/badges".to_owned(),
            format!("/api/attention/modes?workspace_id={WORKSPACE_ID}"),
        ] {
            let (status, body) = request(&router, "GET", &path, None).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "anon {path}: {body}");
            assert_eq!(body, r#"{"error":"unauthorized"}"#, "anon {path}");
        }

        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "the perimeter must refuse every anonymous read before the product port is touched"
        );
    }
}

/// Wave 3A checkpoint A5: the host-resources read door (`GET /api/hosts/{id}/resources`,
/// #2198), mounted in `prepare()` behind the SAME human perimeter and wired to its own
/// by-path `SqliteHostResourcesColdRead` cold-read pool — the sanctioned cold-read
/// pattern every sibling `*ColdRead` uses, kept distinct from the teardown resource
/// authority. Two composition obligations the leaf/contract tests cannot prove:
///
/// * `the_composition_serves_host_resources_behind_the_perimeter` drives the door
///   through the real composed root: anonymous → the perimeter's `401`; an admitted
///   OWNER reads the real seeded agents off the live SQLite adapter in the port's
///   `created_at ASC` order with the full `{id,name,display_name,runtime}` projection;
///   a REAL other-owned host and an ABSENT host id BOTH collapse to the byte-identical
///   owner-scoped `404` (no existence oracle); plus a one-method route checksum.
/// * `the_host_resources_perimeter_refuses_before_touching_the_product_port` is the
///   load-bearing refusal gate: it mounts the SAME factory behind the SAME
///   `require_human_session` over a counting spy, and proves an anonymous read is
///   refused `401` with the product port invoked ZERO times.
mod host_resources_activation {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use silverhand_backend_core::{
        foundation::identity::HostId,
        foundation::persistence::PersistenceFailure,
        host_runtime::host_resources_read::{HostResourcesFacts, HostResourcesReadModel},
        identity_access::auth::{HumanSessionAuthenticator, SessionTtl, SystemSessionClock},
    };
    use silverhand_backend_server::{
        host_runtime::host_resources,
        identity_access::auth::{PublicJsonUnauthorized, require_human_session},
    };
    use silverhand_backend_sqlite::identity_access::auth::SqliteSessionStore;

    use super::*;

    /// One host owned by the admitted viewer (`user-1`, seeded by `seed_human`) with
    /// two agents whose insertion order is the REVERSE of their `created_at`, plus a
    /// SECOND host owned by a different registered user. The reversed stamps let the
    /// composed read prove the adapter's `created_at ASC` order; the foreign host lets
    /// it prove owner-scoping is not an existence oracle.
    async fn seed_host_resources(path: &Path) {
        let pool = SqlitePoolOptions::new()
            .connect(path.to_str().unwrap())
            .await
            .unwrap();
        for statement in [
            "INSERT INTO users (id, name, display_name, created_at, updated_at) VALUES ('user-other', 'other', 'Other', 1, 1)",
            "INSERT INTO hosts (id, name, kind, created_at, user_id) VALUES ('h-res', 'host-res', 'desktop', 1, 'user-1')",
            "INSERT INTO hosts (id, name, kind, created_at, user_id) VALUES ('h-other', 'host-other', 'desktop', 1, 'user-other')",
            "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, display_name, created_by_user_id, materialization_status) \
                 VALUES ('res-late', 'late', 'codex', 'h-res', 3000, 3000, 'Late One', 'user-1', 'failed')",
            "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, display_name, created_by_user_id, materialization_status) \
                 VALUES ('res-early', 'early', 'qoder', 'h-res', 1000, 1000, 'Early One', 'user-1', 'active')",
        ] {
            sqlx::query(statement).execute(&pool).await.unwrap();
        }
        pool.close().await;
    }

    #[tokio::test]
    async fn the_composition_serves_host_resources_behind_the_perimeter() {
        let fixture = fixture(true).await;
        seed_human(&fixture.path).await;
        seed_host_resources(&fixture.path).await;
        let app = prepare(&config(fixture.path.clone())).await.unwrap();
        let router = app.router;
        let bearer = format!("Bearer {TOKEN}");
        let owned = "/api/hosts/h-res/resources";
        let foreign = "/api/hosts/h-other/resources";
        let absent = "/api/hosts/h-absent/resources";

        // Anonymous → the perimeter's refusal envelope, never a resource answer.
        let (status, body) = request(&router, "GET", owned, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "anon: {body}");
        assert_eq!(body, r#"{"error":"unauthorized"}"#);

        // Admitted owner → the real adapter's `created_at ASC` set off live SQLite,
        // each agent carrying its full `{id,name,display_name,runtime}` projection.
        let (status, body) = request(&router, "GET", owned, Some(&bearer)).await;
        assert_eq!(status, StatusCode::OK, "owner: {body}");
        let value = serde_json::from_str::<serde_json::Value>(&body).unwrap();
        let agents = value["agents"].as_array().unwrap();
        let ids = agents
            .iter()
            .map(|agent| agent["id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            ["res-early", "res-late"],
            "adapter orders by created_at ASC"
        );
        assert_eq!(
            agents[0],
            serde_json::json!({
                "id": "res-early",
                "name": "early",
                "display_name": "Early One",
                "runtime": "qoder"
            }),
            "the real projection carries every declared field: {body}"
        );

        // Admitted, REAL other-owned host → the owner-scoped 404. A registered user who
        // simply does not own this computer is indistinguishable from asking about one
        // that does not exist, so the surface is never an existence oracle for host ids.
        let (status, foreign_body) = request(&router, "GET", foreign, Some(&bearer)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "foreign: {foreign_body}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&foreign_body).unwrap(),
            serde_json::json!({ "error": "host not found", "error_code": "host_not_found" })
        );

        // Admitted, ABSENT host id → the SAME 404 bytes as the other-owned host.
        let (status, absent_body) = request(&router, "GET", absent, Some(&bearer)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "absent: {absent_body}");
        assert_eq!(
            absent_body, foreign_body,
            "an absent host and someone else's collapse to byte-identical refusals"
        );

        // One-method checksum: the mounted path serves GET only.
        let (status, body) = request(&router, "POST", owned, Some(&bearer)).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{body}");
    }

    /// A spy satisfying the host-resources cold-read port; the single read bumps a
    /// counter then diverges. Reaching it past the perimeter is a bug, so the body
    /// never fabricates a success value.
    #[derive(Clone, Default)]
    struct HostResourcesCountingPort {
        calls: Arc<AtomicUsize>,
    }

    impl HostResourcesReadModel for HostResourcesCountingPort {
        async fn read_host_resources(
            &self,
            _: &HostId,
        ) -> Result<Option<HostResourcesFacts>, PersistenceFailure> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            unreachable!("perimeter must refuse the anonymous read before the product port")
        }
    }

    #[tokio::test]
    async fn the_host_resources_perimeter_refuses_before_touching_the_product_port() {
        let fixture = fixture(false).await;
        let session_store = SqliteSessionStore::open(&fixture.path).await.unwrap();
        let sessions = HumanSessionAuthenticator::new(
            session_store,
            SystemSessionClock,
            SessionTtl::default(),
        );
        let spy = HostResourcesCountingPort::default();
        let calls = spy.calls.clone();
        // The SAME factory `prepare()` mounts, behind the SAME perimeter.
        let router = require_human_session(
            host_resources::router(spy),
            sessions,
            PublicJsonUnauthorized,
        );

        let (status, body) = request(&router, "GET", "/api/hosts/host-1/resources", None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
        assert_eq!(body, r#"{"error":"unauthorized"}"#);

        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "the perimeter must refuse the anonymous read before the product port is touched"
        );
    }
}

/// The composition mounts `GET /api/agents/{id}/presence` behind the SAME human
/// perimeter, wired to the real SQLite agent-presence cold read and the production
/// `SystemProcessClock`. Proves the composition obligation the leaf's own tests
/// cannot: the anonymous perimeter refusal; the anti-enumeration 404 a registered
/// stranger and an absent id share BYTE-for-byte; a one-method route checksum; AND
/// an admitted creator reading a real seeded stamp projected off the live SQLite
/// adapter to the shared away deadline, alongside a live `server_now` bracketed by
/// the wall clock — proving the mount injected the real production clock, not a
/// fixture. The per-arm/stage matrix stays with the leaf's own tests.
#[tokio::test]
async fn the_composition_serves_agent_presence_behind_the_perimeter() {
    let fixture = fixture(true).await;
    seed_human(&fixture.path).await;
    seed_presence_agents(&fixture.path).await;
    let app = prepare(&config(fixture.path.clone())).await.unwrap();
    let router = app.router;
    let bearer = format!("Bearer {TOKEN}");
    let absent = "/api/agents/agent-absent/presence";

    // Anonymous → the perimeter's refusal envelope, never a presence answer.
    let (status, body) = request(&router, "GET", absent, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body, r#"{"error":"unauthorized"}"#);

    // Admitted, absent id → the anti-enumeration 404.
    let (status, absent_body) = request(&router, "GET", absent, Some(&bearer)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{absent_body}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&absent_body).unwrap(),
        serde_json::json!({ "error": "agent not found" })
    );

    // Admitted stranger: a REAL row the viewer is in no arm of collapses to the
    // SAME 404 bytes as an absent id, so the mounted surface is never an oracle.
    let (status, stranger_body) = request(
        &router,
        "GET",
        "/api/agents/a-stranger/presence",
        Some(&bearer),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{stranger_body}");
    assert_eq!(stranger_body, absent_body);

    // One-method route checksum: the mounted path serves GET only.
    let (status, body) = request(&router, "POST", absent, Some(&bearer)).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{body}");

    // Admitted creator: the mounted leaf projects the real seeded stamp off the live
    // SQLite adapter to the shared away deadline (`away_at` is the shared TTL policy
    // applied to 1000, not a literal — drift in adapter or policy fails here), and
    // `server_now` is a live wall-clock stamp from the injected production clock.
    let before = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let (status, body) = request(
        &router,
        "GET",
        "/api/agents/a-presence/presence",
        Some(&bearer),
    )
    .await;
    let after = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    assert_eq!(status, StatusCode::OK, "{body}");
    let presence = serde_json::from_str::<serde_json::Value>(&body).unwrap();
    assert_eq!(
        presence["away_at"],
        serde_json::json!(
            1_000 + silverhand_backend_core::host_runtime::agent_presence::AWAY_TTL_MS
        )
    );
    let server_now = presence["server_now"]
        .as_i64()
        .expect("server_now must be an integer stamp from the injected clock");
    assert!(
        before <= server_now && server_now <= after,
        "server_now {server_now} not bracketed by wall clock [{before}, {after}]"
    );
}

/// Read the next JSON frame from a daemon socket, answering server pings so the
/// connection stays live across the attach handshake (the daemon side of the WS
/// must keep-alive; a text frame is the command/event the test asserts on).
async fn next_daemon_json<S>(
    socket: &mut tokio_tungstenite::WebSocketStream<S>,
) -> serde_json::Value
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use futures_util::{SinkExt, StreamExt};
    loop {
        match socket.next().await.unwrap().unwrap() {
            ClientMessage::Text(frame) => return serde_json::from_str(&frame).unwrap(),
            ClientMessage::Ping(payload) => {
                socket.send(ClientMessage::Pong(payload)).await.unwrap()
            }
            other => panic!("unexpected daemon frame: {other:?}"),
        }
    }
}

struct AttachedDeliveryDaemon {
    stop: std::sync::Arc<tokio::sync::Notify>,
    seen: std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    task: tokio::task::JoinHandle<()>,
}

impl AttachedDeliveryDaemon {
    async fn connect(address: std::net::SocketAddr) -> Self {
        let mut request = format!("ws://{address}{HOST_WEBSOCKET_PATH}")
            .into_client_request()
            .unwrap();
        request.headers_mut().insert(
            tokio_tungstenite::tungstenite::http::header::AUTHORIZATION,
            format!("Bearer {HOST_TOKEN}").parse().unwrap(),
        );
        let (mut socket, _) = connect_async(request).await.unwrap();
        socket
            .send(ClientMessage::Text(
                serde_json::json!({
                    "direction": "daemon_to_server",
                    "type": "event",
                    "kind": "ready",
                    "protocol_version": OUTBOUND_PROTOCOL_VERSION,
                    "daemon_version": "test",
                    "daemon_id": "daemon-lane-delivery",
                    "instance_id": "instance-lane-delivery",
                    "host_descriptor": { "name": "Lane delivery host", "kind": "remote" },
                    "capabilities": [],
                    "daemon_pid": 7,
                    "started_at": 100,
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        let stop = std::sync::Arc::new(tokio::sync::Notify::new());
        let attached = std::sync::Arc::new(tokio::sync::Notify::new());
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let task = tokio::spawn(service_lane_delivery_commands(
            socket,
            stop.clone(),
            attached.clone(),
            seen.clone(),
        ));
        tokio::time::timeout(std::time::Duration::from_secs(5), attached.notified())
            .await
            .expect("delivery daemon must attach before a post-commit effect runs");
        Self { stop, seen, task }
    }

    async fn wait_for_deliveries(&self, expected: usize) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if count_kind(&self.seen, "runtime_deliver") >= expected {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("expected {expected} real runtime_deliver commands"));
    }

    async fn close(self) {
        self.stop.notify_one();
        self.task.await.unwrap();
    }
}

async fn service_lane_delivery_commands<S>(
    mut socket: tokio_tungstenite::WebSocketStream<S>,
    stop: std::sync::Arc<tokio::sync::Notify>,
    attached: std::sync::Arc<tokio::sync::Notify>,
    seen: std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    loop {
        tokio::select! {
            () = stop.notified() => break,
            command = next_daemon_json(&mut socket) => {
                if command["type"] == "heartbeat" && command["kind"] == "ping" {
                    socket.send(ClientMessage::Text(serde_json::json!({
                        "direction": "daemon_to_server", "type": "heartbeat",
                        "kind": "pong", "ts": command["ts"], "echo_ts": command["ts"],
                    }).to_string().into())).await.unwrap();
                    continue;
                }
                let result = match command["kind"].as_str() {
                    Some("runtime_status") => serde_json::json!({ "runtimes": [] }),
                    Some("host_status") => {
                        attached.notify_one();
                        serde_json::json!({
                            "os": "linux", "daemon_pid": 1,
                            "daemon_version": "0.0.0-test", "started_at": 0,
                            "uptime_ms": 0, "active_agent_ids": [],
                        })
                    }
                    Some("host_runtime_catalog") => serde_json::json!({
                        "runtimes": [], "detected_at": 1_700_000_000_000_i64,
                    }),
                    Some("runtime_deliver") => {
                        seen.lock().unwrap().push(command.clone());
                        serde_json::json!({
                            "disposition": "admitted", "pid": 1, "started_at": 1,
                        })
                    }
                    Some("runtime_start") => {
                        seen.lock().unwrap().push(command.clone());
                        serde_json::json!({ "started": true, "pid": 1, "started_at": 1 })
                    }
                    Some("runtime_stop") => {
                        seen.lock().unwrap().push(command.clone());
                        serde_json::json!({ "mode": "hard", "stopped": true, "exit_code": null })
                    }
                    other => panic!("unexpected lane delivery command: {other:?}"),
                };
                socket.send(ClientMessage::Text(serde_json::json!({
                    "direction": "daemon_to_server", "type": "command_response",
                    "id": command["id"], "kind": command["kind"], "result": result,
                }).to_string().into())).await.unwrap();
            }
        }
    }
    socket.close(None).await.unwrap();
}

async fn next_socket_json<S>(
    socket: &mut tokio_tungstenite::WebSocketStream<S>,
) -> serde_json::Value
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use futures_util::StreamExt;
    let tokio_tungstenite::tungstenite::Message::Text(frame) =
        socket.next().await.unwrap().unwrap()
    else {
        panic!("expected a text frame");
    };
    serde_json::from_str(&frame).unwrap()
}

/// The presence vertical mounted through the composition root: the `GET
/// /api/presence` route behind the human perimeter, the event-WS lifecycle feed,
/// and the single `PresenceGraceTimers` both share. Kept in its own module so it
/// reuses the parent harness (`fixture`, `seed_human`, `config`, `request`,
/// `next_socket_json`) without touching its import surface.
mod presence_composition {
    use silverhand_backend_core::{
        foundation::identity::UserId,
        foundation::persistence::PersistenceFailure,
        viewer_read_model::presence::{LastSeenAt, UserPresenceFacts},
        viewer_read_model::presence_grace::PresenceFact,
        viewer_read_model::user_presence_cold_read::UserPresenceReadModel,
        viewer_read_model::user_presence_last_seen::LastSeenSink,
    };
    use silverhand_backend_server::{
        foundation::scope_event_transport::ConnectionLifecycle,
        host_runtime::host_agent_background::presence_grace_timer::PresenceFactSink,
        viewer_read_model::presence::PresenceConnectionFeed,
        viewer_read_model::user_presence_http::{self, PRESENCE_PATH},
    };

    use super::*;

    /// A silent last-seen sink for wiring tests: `on_connect` never touches it, so
    /// the online proof needs no working persistence behind the timers.
    #[derive(Clone)]
    struct SilentLastSeen;

    impl LastSeenSink for SilentLastSeen {
        async fn touch_last_seen(
            &self,
            _user_id: UserId,
            _last_seen_at: LastSeenAt,
        ) -> Result<(), PersistenceFailure> {
            Ok(())
        }
    }

    /// A silent presence-fact sink: the offline wire is exercised by the timer's own
    /// already-PASS'd unit tests, not re-driven here.
    #[derive(Clone)]
    struct SilentFacts;

    impl PresenceFactSink for SilentFacts {
        type Error = std::convert::Infallible;

        fn publish(&self, _fact: PresenceFact) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    /// A cold read that serves a fixed fact set, so a wiring test can assert the
    /// live-liveness join without a real database.
    #[derive(Clone)]
    struct FactsFixture(Vec<UserPresenceFacts>);

    impl UserPresenceReadModel for FactsFixture {
        async fn list_user_presence_facts(
            &self,
        ) -> Result<Vec<UserPresenceFacts>, PersistenceFailure> {
            Ok(self.0.clone())
        }
    }

    /// The `GET /api/presence` auth refusal, signed by the REAL TypeScript
    /// perimeter over the REAL presence route
    /// (packages/backend/tests/presence-auth-refusal-parity.test.ts) and checked in
    /// as the sole artifact both implementations answer to. The refusal belongs to
    /// the perimeter the composition root mounts, not to the presence leaf, which is
    /// why the leaf's whole-wire gate deliberately excludes it — and why this
    /// composition consumes the value rather than self-signing it.
    const PRESENCE_REFUSAL: &str = include_str!(
        "../../../../../rust/test-fixtures/fixtures/presence/unauthorized/refusal.json"
    );

    /// The refusal fixture's two fields: an exact status and the raw body TEXT, so
    /// the differential is over bytes rather than a re-serialized tree. Both are
    /// required — a fixture missing either would silently weaken the gate, so this
    /// panics rather than defaulting.
    fn presence_refusal() -> (u16, String) {
        let refusal: serde_json::Value = serde_json::from_str(PRESENCE_REFUSAL).unwrap();
        let status = u16::try_from(refusal["status"].as_u64().expect("signed status")).unwrap();
        let body = refusal["body"]
            .as_str()
            .expect("signed body text")
            .to_owned();
        (status, body)
    }

    /// The composition mounts `GET /api/presence` behind the human bearer/session
    /// perimeter (exact anonymous 401, authenticated 200, one route), and a live
    /// event-WS connection flips the viewer to online in the very read the GET
    /// serves — the object-identity singleton observed end to end through the real
    /// composed router, not a leaf handler.
    #[tokio::test]
    async fn the_composition_serves_presence_behind_the_perimeter_and_a_live_connection_reads_online()
     {
        let fixture = fixture(true).await;
        seed_human(&fixture.path).await;
        let app = prepare(&config(fixture.path.clone())).await.unwrap();
        // A clone shares the same Arc-backed presence timers with the served router,
        // so a GET on this handle observes the live connection made against the TCP
        // server below.
        let router = app.router.clone();
        let bearer = format!("Bearer {TOKEN}");

        // Anonymous → the perimeter's exact refusal envelope, never the viewer set.
        // The expected status and body BYTES are not this test's to invent: they are
        // the artifact the real TypeScript perimeter signed
        // (packages/backend/tests/presence-auth-refusal-parity.test.ts), consumed
        // here verbatim. Comparison is over the raw body text, so the two
        // implementations cannot drift on separators or key order.
        let (status, body) = request(&router, "GET", PRESENCE_PATH, None).await;
        let (refused_status, refused_body) = presence_refusal();
        assert_eq!(status.as_u16(), refused_status, "{body}");
        assert_eq!(body, refused_body);

        // Authenticated → 200 with the closed wire shape; no live connection yet, so
        // the viewer is offline with a null stamp (never seen).
        let (status, body) = request(&router, "GET", PRESENCE_PATH, Some(&bearer)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap(),
            serde_json::json!({
                "users": [
                    { "user_id": "user-1", "presence": "offline", "last_seen_at": null },
                ]
            })
        );

        // Route checksum = 1: the path carries exactly one method. An AUTHENTICATED
        // POST clears the perimeter and then meets 405 — one route, one handler
        // entry, no second mount.
        let (status, body) = request(&router, "POST", PRESENCE_PATH, Some(&bearer)).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{body}");

        // A live event-WS connection as user-1. Its lifecycle feeds the single
        // presence timers the GET reads.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let running = tokio::spawn(serve(listener, app, async move {
            let _ = shutdown_rx.await;
            Ok::<(), std::io::Error>(())
        }));

        let mut websocket_request = format!("ws://{address}{EVENT_WEBSOCKET_PATH}")
            .into_client_request()
            .unwrap();
        websocket_request
            .headers_mut()
            .insert(ORIGIN, "http://localhost:5173".parse().unwrap());
        websocket_request.headers_mut().insert(
            COOKIE,
            format!("{EVENT_WS_SESSION_COOKIE}={TOKEN}")
                .parse()
                .unwrap(),
        );
        websocket_request
            .headers_mut()
            .insert(DESKTOP_API_EPOCH_HEADER, MINIMUM_CLIENT_API_EPOCH.into());
        let (mut socket, _) = connect_async(websocket_request).await.unwrap();
        assert_eq!(next_socket_json(&mut socket).await["type"], "authenticated");

        // The connect mark lands around the authenticated handshake; poll the shared
        // GET until it observes online (bounded, so a broken singleton fails instead
        // of hanging).
        let mut online = false;
        for _ in 0..200 {
            let (status, body) = request(&router, "GET", PRESENCE_PATH, Some(&bearer)).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let json: serde_json::Value = serde_json::from_str(&body).unwrap();
            if json["users"][0]["presence"] == "online" {
                assert_eq!(json["users"][0]["last_seen_at"], serde_json::Value::Null);
                online = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            online,
            "a live event-WS connection must mark the viewer online in the GET the composition shares with it"
        );

        socket.close(None).await.unwrap();
        shutdown_tx.send(()).unwrap();
        running.await.unwrap().unwrap();
    }

    /// The singleton wiring is load-bearing: online only appears when the connect
    /// feed and the presence route read ONE timers instance. The counterexample —
    /// two separate `new()` — stays RED (offline), proving the positive control is
    /// not an accident of two registries agreeing.
    #[tokio::test]
    async fn presence_online_requires_the_feed_and_the_route_to_share_one_timers_instance() {
        let viewer = UserId::parse("user-1").unwrap();
        let cold = FactsFixture(vec![UserPresenceFacts {
            id: viewer.clone(),
            last_seen_at: None,
            created_at: 1,
        }]);

        async fn presence_of(router: &Router, viewer: &UserId) -> String {
            let response = router
                .clone()
                .oneshot(
                    Request::get(PRESENCE_PATH)
                        .extension(viewer.clone())
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            json["users"][0]["presence"].as_str().unwrap().to_owned()
        }

        // Positive control: ONE instance. Its clone feeds the connect lifecycle; the
        // instance itself is the route's OnlineViewers.
        let shared = PresenceGraceTimers::new(
            SilentLastSeen,
            SilentFacts,
            GraceWindow::from_millis_or_default(None),
        );
        let feed = PresenceConnectionFeed::new(shared.clone(), SystemDisconnectClock);
        feed.on_connect(&viewer, "conn-1");
        let shared_router = user_presence_http::router(cold.clone(), shared);
        assert_eq!(presence_of(&shared_router, &viewer).await, "online");

        // Counterexample: TWO separate `new()`. The feed marks online in registry A;
        // the route reads registry B, which never saw the connection.
        let feed_registry = PresenceGraceTimers::new(
            SilentLastSeen,
            SilentFacts,
            GraceWindow::from_millis_or_default(None),
        );
        let route_registry = PresenceGraceTimers::new(
            SilentLastSeen,
            SilentFacts,
            GraceWindow::from_millis_or_default(None),
        );
        let split_feed = PresenceConnectionFeed::new(feed_registry, SystemDisconnectClock);
        split_feed.on_connect(&viewer, "conn-1");
        let split_router = user_presence_http::router(cold, route_registry);
        assert_eq!(presence_of(&split_router, &viewer).await, "offline");
    }
}

#[tokio::test]
async fn activity_retention_sweeps_idle_database_and_stops_with_its_owner() {
    use std::sync::{Arc, atomic::{AtomicI64, Ordering}};
    use silverhand_backend_server::foundation::clock::ProcessClock;
    use silverhand_backend_core::conversation_messaging::messaging::collaboration::conversation_agent_activity::ACTIVITY_RETENTION_MS;
    #[derive(Clone)]
    struct Clock(Arc<AtomicI64>);
    impl ProcessClock for Clock {
        fn now_unix_millis(&self) -> i64 {
            self.0.load(Ordering::SeqCst)
        }
    }
    async fn wake_after(duration: std::time::Duration) {
        // Jump only the timer. Resume before waiting for the real SQLite worker,
        // so idle Tokio auto-advance cannot expire SQLx acquisition deadlines.
        tokio::time::pause();
        tokio::time::advance(duration).await;
        tokio::time::resume();
        tokio::task::yield_now().await;
    }
    let f = fixture(true).await;
    let mut db = sqlx::SqliteConnection::connect(&format!("sqlite:{}", f.path.display()))
        .await
        .unwrap();
    db.execute(ENROLLMENT_SEED_SQL).await.unwrap();
    db.execute("INSERT INTO conversation_events(conversation_id,agent_id,kind,created_at) VALUES('conv-open','agent-1','turn_end',100)").await.unwrap();
    let six = 1788732000000i64;
    sqlx::query("UPDATE conversation_events SET created_at=?")
        .bind(six - 61_000 - ACTIVITY_RETENTION_MS)
        .execute(&mut db)
        .await
        .unwrap();
    let clock = Clock(Arc::new(AtomicI64::new(six - 61_000)));
    let store = MessagingStore::open(&f.path).await.unwrap();
    let retention = super::activity_retention::ActivityRetention::start(
        store,
        ScopeEventHub::production().unwrap(),
        clock.clone(),
    )
    .await
    .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM conversation_events")
            .fetch_one(&mut db)
            .await
            .unwrap(),
        1
    );
    // Let the spawned task register its real sixty-second timer.
    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    clock.0.store(six - 1_000, Ordering::SeqCst);
    wake_after(std::time::Duration::from_secs(60)).await;
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM conversation_events")
            .fetch_one(&mut db)
            .await
            .unwrap(),
        1,
        "an expired row stays until the daily deadline, not every wake"
    );
    clock.0.store(six, Ordering::SeqCst);
    wake_after(std::time::Duration::from_secs(1)).await;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if sqlx::query_scalar::<_, i64>("SELECT count(*) FROM conversation_events")
                .fetch_one(&mut db)
                .await
                .unwrap()
                == 0
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the idle daily timer must prune through the real store");
    // Staying at 06:00 must not repeatedly sweep the same deadline.
    db.execute("INSERT INTO conversation_events(conversation_id,agent_id,kind,created_at) VALUES('conv-open','agent-1','turn_end',1)").await.unwrap();
    wake_after(std::time::Duration::from_secs(60)).await;
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM conversation_events")
            .fetch_one(&mut db)
            .await
            .unwrap(),
        1
    );
    db.execute("DELETE FROM conversation_events").await.unwrap();
    retention.close().await;
    retention.close().await;
    db.execute("INSERT INTO conversation_events(conversation_id,agent_id,kind,created_at) VALUES('conv-open','agent-1','turn_end',1)").await.unwrap();
    clock.0.store(six + 86_400_000, Ordering::SeqCst);
    wake_after(std::time::Duration::from_secs(60)).await;
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM conversation_events")
            .fetch_one(&mut db)
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn activity_retention_is_wired_into_real_preparation() {
    let f = fixture(true).await;
    let mut db = sqlx::SqliteConnection::connect(&format!("sqlite:{}", f.path.display()))
        .await
        .unwrap();
    db.execute(ENROLLMENT_SEED_SQL).await.unwrap();
    db.execute("INSERT INTO conversation_events(conversation_id,agent_id,kind,created_at) VALUES('conv-open','agent-1','turn_end',1)").await.unwrap();
    let app = prepare(&config(f.path.clone())).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM conversation_events")
            .fetch_one(&mut db)
            .await
            .unwrap(),
        0
    );
    app.close_connections().await;
}

#[tokio::test]
async fn client_message_identity_survives_concurrent_http_ws_ack_and_cold_read() {
    let fixture = fixture(true).await;
    seed_human(&fixture.path).await;
    seed_workspace_conversation(&fixture.path).await;
    let db = SqlitePoolOptions::new()
        .connect(fixture.path.to_str().unwrap())
        .await
        .unwrap();
    sqlx::query("UPDATE conversations SET status='idle'")
        .execute(&db)
        .await
        .unwrap();
    db.close().await;
    let app = prepare(&config(fixture.path.clone())).await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let running = tokio::spawn(serve(listener, app, async move {
        let _ = shutdown_rx.await;
        Ok::<(), std::io::Error>(())
    }));

    let mut websocket_request = format!("ws://{address}{EVENT_WEBSOCKET_PATH}")
        .into_client_request()
        .unwrap();
    websocket_request
        .headers_mut()
        .insert(ORIGIN, "http://localhost:5173".parse().unwrap());
    websocket_request.headers_mut().insert(
        COOKIE,
        format!("{EVENT_WS_SESSION_COOKIE}={TOKEN}")
            .parse()
            .unwrap(),
    );
    websocket_request
        .headers_mut()
        .insert(DESKTOP_API_EPOCH_HEADER, MINIMUM_CLIENT_API_EPOCH.into());
    let (mut socket, _) = connect_async(websocket_request).await.unwrap();
    let authenticated = socket.next().await.unwrap().unwrap();
    let ClientMessage::Text(authenticated) = authenticated else {
        panic!("authenticated frame must be text");
    };
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&authenticated).unwrap()["type"],
        "authenticated"
    );
    socket
        .send(ClientMessage::Text(
            serde_json::json!({
                "type": "subscribe",
                "scope": format!("conversation:{CONVERSATION_ID}"),
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
    let subscribed = socket.next().await.unwrap().unwrap();
    let ClientMessage::Text(subscribed) = subscribed else {
        panic!("subscribed frame must be text");
    };
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&subscribed).unwrap()["type"],
        "subscribed"
    );

    let path = format!("/api/channels/{CONVERSATION_ID}/messages");
    let bearer = format!("Bearer {TOKEN}");
    let body = r#"{"content":"composed hello","client_message_id":"12345678-1234-4234-8234-123456789abc"}"#;
    let (first, second) = tokio::join!(
        http_over_tcp(address, "POST", &path, Some(&bearer), body),
        http_over_tcp(address, "POST", &path, Some(&bearer), body)
    );
    let ack = |response: &str| {
        assert!(
            response.starts_with("HTTP/1.1 201 Created\r\n"),
            "{response}"
        );
        serde_json::from_str::<serde_json::Value>(response.split("\r\n\r\n").nth(1).unwrap())
            .unwrap()
    };
    let first = ack(&first);
    let second = ack(&second);
    assert_eq!(first["message"], second["message"]);
    assert_eq!(
        first["message"]["client_message_id"],
        "12345678-1234-4234-8234-123456789abc"
    );
    let event = socket.next().await.unwrap().unwrap();
    let ClientMessage::Text(event) = event else {
        panic!("message event must be text");
    };
    let event: serde_json::Value = serde_json::from_str(&event).unwrap();
    assert_eq!(event["scope"], format!("conversation:{CONVERSATION_ID}"));
    assert_eq!(event["kind"], "message");
    assert_eq!(event["payload"]["message"]["content"], "composed hello");

    let pool = SqlitePoolOptions::new()
        .connect(fixture.path.to_str().unwrap())
        .await
        .unwrap();
    let committed: (String, String) =
        sqlx::query_as("SELECT id, content FROM messages WHERE conversation_id = ?")
            .bind(CONVERSATION_ID)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(committed.1, "composed hello");
    assert_eq!(event["payload"]["message"]["id"], committed.0);
    assert_eq!(event["payload"]["message"], first["message"]);
    // A lost ACK retry returns the same committed fact without another event.
    assert_eq!(
        ack(&http_over_tcp(address, "POST", &path, Some(&bearer), body).await)["message"],
        first["message"]
    );
    let cold = http_over_tcp(address, "GET", &path, Some(&bearer), "").await;
    assert!(
        cold.contains("12345678-1234-4234-8234-123456789abc"),
        "{cold}"
    );
    let conflict = http_over_tcp(
        address,
        "POST",
        &path,
        Some(&bearer),
        &body.replace("composed hello", "different"),
    )
    .await;
    assert!(conflict.starts_with("HTTP/1.1 409"), "{conflict}");
    assert!(conflict.contains("client_message_conflict"));
    // Drain any non-message projection frames; no second message may be published.
    let duplicate = tokio::time::timeout(std::time::Duration::from_millis(100), async {
        while let Some(Ok(ClientMessage::Text(frame))) = socket.next().await {
            if serde_json::from_str::<serde_json::Value>(&frame).unwrap()["kind"] == "message" {
                return true;
            }
        }
        false
    })
    .await;
    assert!(duplicate.is_err(), "duplicate message publication");
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM messages")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    // Post authority is rechecked on retry after access is removed.
    sqlx::query("DELETE FROM workspace_users WHERE user_id=?")
        .bind("user-1")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM conversation_users WHERE user_id=?")
        .bind("user-1")
        .execute(&pool)
        .await
        .unwrap();
    let denied = http_over_tcp(address, "POST", &path, Some(&bearer), body).await;
    assert!(
        denied.starts_with("HTTP/1.1 403") || denied.starts_with("HTTP/1.1 404"),
        "{denied}"
    );
    pool.close().await;

    socket.close(None).await.unwrap();
    shutdown_tx.send(()).unwrap();
    running.await.unwrap().unwrap();
}

#[tokio::test]
async fn profile_message_counts_mounts_behind_the_real_human_perimeter() {
    let fixture = fixture(true).await;
    seed_human(&fixture.path).await;
    let app = prepare(&config(fixture.path.clone())).await.unwrap();
    let path = "/api/profiles/user/user-1/message-counts?year=2024";
    assert_eq!(
        request(&app.router, "GET", path, None).await.0,
        StatusCode::UNAUTHORIZED
    );
    let bearer = format!("Bearer {TOKEN}");
    let (status, body) = request(&app.router, "GET", path, Some(&bearer)).await;
    assert_eq!(status, StatusCode::OK);
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["days"].as_array().unwrap().len(), 366);
    assert_eq!(value["subject"]["id"], "user-1");
}

/// Real authenticated HTTP + WS: empty-workspace roster changes invalidate the
/// creator's related-human read, independently of conversation/channel events.
#[tokio::test]
async fn related_users_follow_owned_workspace_memberships_over_http_and_ws() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("related.sqlite");
    seed_enrollment_scenario(&path).await;
    let pool = SqlitePoolOptions::new()
        .connect(path.to_str().unwrap())
        .await
        .unwrap();
    sqlx::raw_sql(
        "INSERT INTO workspaces(id,name,name_key,owner_user_id,created_at,updated_at) VALUES
      ('owned-empty','Owned Empty','owned empty','admin-1',1,1),
      ('owned-second','Owned Second','owned second','admin-1',1,1);
      INSERT INTO workspace_users(workspace_id,user_id,joined_at,role) VALUES
      ('owned-empty','admin-1',1,'owner'),('owned-second','admin-1',1,'owner');",
    )
    .execute(&pool)
    .await
    .unwrap();
    let app = prepare(&config(path.clone())).await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let running = tokio::spawn(serve(listener, app, async move {
        let _ = shutdown_rx.await;
        Ok::<(), std::io::Error>(())
    }));
    let admin = format!("Bearer {ADMIN_TOKEN}");
    let get_ids = |value: serde_json::Value| {
        value["users"]
            .as_array()
            .unwrap()
            .iter()
            .map(|u| u["id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    let read = http_over_tcp(address, "GET", "/api/users/related", None, "").await;
    assert!(read.starts_with("HTTP/1.1 401"), "{read}");
    let read = http_over_tcp(address, "GET", "/api/users/related", Some(&admin), "").await;
    assert!(read.starts_with("HTTP/1.1 200"), "{read}");
    assert_eq!(get_ids(http_json_body(&read)), vec!["admin-1"]);
    // The viewer query parameter cannot impersonate the owner. Directory remains independent.
    let read = http_over_tcp(
        address,
        "GET",
        "/api/users/related?viewer=admin-1",
        Some(&format!("Bearer {TOKEN}")),
        "",
    )
    .await;
    assert!(!get_ids(http_json_body(&read)).contains(&"admin-1".to_owned()));
    let read = http_over_tcp(address, "GET", "/api/users", Some(&admin), "").await;
    assert!(get_ids(http_json_body(&read)).len() > 1);
    let mut req = format!("ws://{address}{EVENT_WEBSOCKET_PATH}")
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert(ORIGIN, "http://localhost:5173".parse().unwrap());
    req.headers_mut().insert(
        COOKIE,
        format!("{EVENT_WS_SESSION_COOKIE}={ADMIN_TOKEN}")
            .parse()
            .unwrap(),
    );
    req.headers_mut()
        .insert(DESKTOP_API_EPOCH_HEADER, MINIMUM_CLIENT_API_EPOCH.into());
    let (mut socket, _) = connect_async(req).await.unwrap();
    assert_eq!(next_socket_json(&mut socket).await["type"], "authenticated");
    socket
        .send(ClientMessage::Text(
            serde_json::json!({"type":"subscribe","scope":"workspace:owned-empty"})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    assert_eq!(next_socket_json(&mut socket).await["type"], "subscribed");
    let add = http_over_tcp(
        address,
        "POST",
        "/api/workspaces/owned-empty/members",
        Some(&admin),
        r#"{"kind":"human","user_id":"user-1"}"#,
    )
    .await;
    assert!(add.starts_with("HTTP/1.1 201"), "{add}");
    let event = next_socket_json(&mut socket).await;
    assert_eq!(event["kind"], "workspace_members_changed");
    assert_eq!(event["scope"], "workspace:owned-empty");
    assert_eq!(
        event["payload"],
        serde_json::json!({"workspace_id":"owned-empty"})
    );
    let read = http_over_tcp(address, "GET", "/api/users/related", Some(&admin), "").await;
    assert_eq!(get_ids(http_json_body(&read)), vec!["admin-1", "user-1"]);
    // A second relationship does not duplicate a human; removing only one preserves them.
    sqlx::query("INSERT INTO workspace_users(workspace_id,user_id,joined_at,role) VALUES('owned-second','user-1',1,'member')").execute(&pool).await.unwrap();
    let read = http_over_tcp(address, "GET", "/api/users/related", Some(&admin), "").await;
    assert_eq!(get_ids(http_json_body(&read)), vec!["admin-1", "user-1"]);
    let remove = http_over_tcp(
        address,
        "DELETE",
        "/api/workspaces/owned-empty/members/users/user-1",
        Some(&admin),
        "",
    )
    .await;
    assert!(
        remove.starts_with("HTTP/1.1 200") || remove.starts_with("HTTP/1.1 204"),
        "{remove}"
    );
    assert_eq!(
        next_socket_json(&mut socket).await["kind"],
        "workspace_members_changed"
    );
    let read = http_over_tcp(address, "GET", "/api/users/related", Some(&admin), "").await;
    assert_eq!(get_ids(http_json_body(&read)), vec!["admin-1", "user-1"]);
    socket
        .send(ClientMessage::Text(
            serde_json::json!({
                "type":"subscribe", "scope":"viewer:self"
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
    assert_eq!(next_socket_json(&mut socket).await["type"], "subscribed");

    for workspace in ["owned-second", "owned-empty"] {
        let delete = http_over_tcp(
            address,
            "DELETE",
            &format!("/api/workspaces/{workspace}"),
            Some(&admin),
            "",
        )
        .await;
        assert!(delete.starts_with("HTTP/1.1 204"), "{delete}");
        let event = next_socket_json(&mut socket).await;
        assert_eq!(event["kind"], "viewer_workspaces_changed");
        assert_eq!(event["payload"]["change"], "removed");
        assert_eq!(event["payload"]["workspace_id"], workspace);
        let read = http_over_tcp(address, "GET", "/api/users/related", Some(&admin), "").await;
        let expected = if workspace == "owned-second" {
            vec!["admin-1"]
        } else {
            vec![]
        };
        assert_eq!(get_ids(http_json_body(&read)), expected);
    }
    socket.close(None).await.unwrap();
    pool.close().await;
    shutdown_tx.send(()).unwrap();
    running.await.unwrap().unwrap();
}

#[path = "human_direct_pair_tests.rs"]
mod human_direct_pair_tests;

#[path = "account_deletion_tests.rs"]
mod account_deletion_tests;
#[path = "avatar_design_tests.rs"]
mod avatar_design_tests;
#[path = "password_change_tests.rs"]
mod password_change_tests;
#[path = "task_content_length_tests.rs"]
mod task_content_length_tests;

#[path = "host_activation_tests.rs"]
mod host_activation_tests;
