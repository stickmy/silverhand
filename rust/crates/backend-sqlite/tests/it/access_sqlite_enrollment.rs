//! Production SQLite enrollment against the real, checked-in current schema.
//!
//! The database is materialized only by the shared current-schema loader
//! (`support/current_schema_snapshot.rs`, cherry-picked from the Host lane), so
//! the real foreign keys, triggers and indexes are all in force; this test adds
//! no schema of its own. Fixture rows (user / host / agent / workspace) are
//! inserted to satisfy those foreign keys, then the production `SqliteAccessStore`
//! runs the same enrollment path the HTTP surface calls.

use sqlx::{
    Connection, SqliteConnection,
    sqlite::{SqliteConnectOptions, SqliteJournalMode},
};
use std::path::Path;

use silverhand_backend_core::foundation::identity::{AgentId, Principal, UserId, WorkspaceId};
use silverhand_backend_core::identity_access::access::{
    AddAgentMemberError, Capability, DEFAULT_AGENT_CAPABILITIES, EnrollmentClock,
    add_workspace_agent_member,
};
use silverhand_backend_sqlite::identity_access::access::SqliteAccessStore;
use tempfile::TempDir;

use crate::current_schema;

const OWNER: &str = "user-owner";
const OTHER: &str = "user-other";
const AGENT: &str = "agent-1";
const WORKSPACE: &str = "ws-1";

/// A known, fixed join-time candidate, so the create path can be asserted to
/// store *exactly* the value the application's clock produced — not merely a
/// positive number.
const JOIN_TIME_MS: i64 = 1_700_000_000_123;

/// A deterministic clock: enrollment samples it after authorization and hands
/// the value to the unit of work as the create-time candidate. Infallible, like
/// the production clock — reading the time is not a failure mode.
#[derive(Clone, Copy)]
struct FixedClock(i64);

impl EnrollmentClock for FixedClock {
    fn now_unix_millis(&self) -> i64 {
        self.0
    }
}

fn clock() -> FixedClock {
    FixedClock(JOIN_TIME_MS)
}

/// Materializes the real schema, inserts the fixture parent rows the enrollment
/// foreign keys require, and returns the tempdir (kept alive) plus the db path.
async fn schema_with_fixtures() -> (TempDir, std::path::PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("access.db");
    let mut connection = current_schema::create_current_schema(&path).await;

    for user in [OWNER, OTHER] {
        sqlx::query(
            "INSERT INTO users (id, name, display_name, created_at, updated_at) \
             VALUES (?, ?, ?, 1, 1)",
        )
        .bind(user)
        .bind(user)
        .bind(user)
        .execute(&mut connection)
        .await
        .unwrap();
    }

    sqlx::query(
        "INSERT INTO hosts (id, name, kind, created_at, user_id, provisioning_source) \
         VALUES ('host-1', 'host-1', 'remote', 1, ?, 'user_invite')",
    )
    .bind(OWNER)
    .execute(&mut connection)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, created_by_user_id) \
         VALUES (?, ?, 'claude', 'host-1', 1, 1, ?)",
    )
    .bind(AGENT)
    .bind(AGENT)
    .bind(OWNER)
    .execute(&mut connection)
    .await
    .unwrap();

    // A regular workspace: owner_agent_id NULL requires a non-null name_key.
    sqlx::query(
        "INSERT INTO workspaces (id, name, name_key, created_at, updated_at) \
         VALUES (?, 'WS One', ?, 1, 1)",
    )
    .bind(WORKSPACE)
    .bind(WORKSPACE)
    .execute(&mut connection)
    .await
    .unwrap();

    connection.close().await.unwrap();
    (directory, path)
}

async fn open_probe(path: &Path) -> SqliteConnection {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Wal);
    SqliteConnection::connect_with(&options).await.unwrap()
}

async fn membership_count(probe: &mut SqliteConnection) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM workspace_members")
        .fetch_one(probe)
        .await
        .unwrap()
}

async fn capability_tokens(probe: &mut SqliteConnection) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT capability FROM workspace_agent_capabilities \
         WHERE workspace_id = ? AND agent_id = ? ORDER BY capability",
    )
    .bind(WORKSPACE)
    .bind(AGENT)
    .fetch_all(probe)
    .await
    .unwrap()
}

fn human(user: &str) -> Principal {
    Principal::Human {
        user_id: UserId::parse(user).unwrap(),
    }
}

#[tokio::test]
async fn enrolling_a_new_owned_agent_seeds_every_default_capability() {
    let (_dir, path) = schema_with_fixtures().await;
    let store = SqliteAccessStore::open(&path).await.unwrap();

    let member = add_workspace_agent_member(
        &store,
        &store,
        &clock(),
        &human(OWNER),
        WorkspaceId::parse(WORKSPACE).unwrap(),
        AgentId::parse(AGENT).unwrap(),
    )
    .await
    .unwrap();

    assert_eq!(member.workspace_id.as_str(), WORKSPACE);
    assert_eq!(member.agent_id.as_str(), AGENT);
    assert_eq!(
        member.joined_at_ms, JOIN_TIME_MS,
        "the create path stores exactly the candidate the application's clock produced"
    );

    let mut probe = open_probe(&path).await;
    assert_eq!(membership_count(&mut probe).await, 1);
    let mut expected: Vec<&str> = DEFAULT_AGENT_CAPABILITIES
        .iter()
        .map(Capability::as_str)
        .collect();
    expected.sort_unstable();
    assert_eq!(capability_tokens(&mut probe).await, expected);
}

#[tokio::test]
async fn re_enrolling_is_idempotent_and_keeps_the_original_join_time() {
    let (_dir, path) = schema_with_fixtures().await;
    let store = SqliteAccessStore::open(&path).await.unwrap();

    let first = add_workspace_agent_member(
        &store,
        &store,
        &clock(),
        &human(OWNER),
        WorkspaceId::parse(WORKSPACE).unwrap(),
        AgentId::parse(AGENT).unwrap(),
    )
    .await
    .unwrap();

    let second = add_workspace_agent_member(
        &store,
        &store,
        &FixedClock(JOIN_TIME_MS + 1000),
        &human(OWNER),
        WorkspaceId::parse(WORKSPACE).unwrap(),
        AgentId::parse(AGENT).unwrap(),
    )
    .await
    .unwrap();

    assert_eq!(
        second.joined_at_ms, first.joined_at_ms,
        "re-enrollment must return the original join time, not a new clock read"
    );

    let mut probe = open_probe(&path).await;
    assert_eq!(membership_count(&mut probe).await, 1);
    assert_eq!(capability_tokens(&mut probe).await.len(), 11);
    let stored: i64 = sqlx::query_scalar(
        "SELECT joined_at FROM workspace_members WHERE workspace_id = ? AND agent_id = ?",
    )
    .bind(WORKSPACE)
    .bind(AGENT)
    .fetch_one(&mut probe)
    .await
    .unwrap();
    assert_eq!(stored, JOIN_TIME_MS);
    assert_eq!(second.joined_at_ms, stored);
}

/// Gate 3 ①: two concurrent first-enrollments of the same key serialize on the
/// root write lock, so exactly one creates the row — the membership is unique
/// and the capability set is seeded exactly once (11 rows, not doubled), and
/// both callers observe the single create-time join clock.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_first_enrollments_create_the_membership_exactly_once() {
    let (_dir, path) = schema_with_fixtures().await;
    let store = SqliteAccessStore::open(&path).await.unwrap();

    let a = {
        let store = store.clone();
        tokio::spawn(async move {
            add_workspace_agent_member(
                &store,
                &store,
                &clock(),
                &human(OWNER),
                WorkspaceId::parse(WORKSPACE).unwrap(),
                AgentId::parse(AGENT).unwrap(),
            )
            .await
        })
    };
    let b = {
        let store = store.clone();
        tokio::spawn(async move {
            add_workspace_agent_member(
                &store,
                &store,
                &FixedClock(JOIN_TIME_MS + 1000),
                &human(OWNER),
                WorkspaceId::parse(WORKSPACE).unwrap(),
                AgentId::parse(AGENT).unwrap(),
            )
            .await
        })
    };

    let first = a.await.unwrap().unwrap();
    let second = b.await.unwrap().unwrap();

    assert_eq!(
        first.joined_at_ms, second.joined_at_ms,
        "both callers must observe the one create-time join clock"
    );

    let mut probe = open_probe(&path).await;
    assert_eq!(membership_count(&mut probe).await, 1);
    let stored: i64 = sqlx::query_scalar(
        "SELECT joined_at FROM workspace_members WHERE workspace_id = ? AND agent_id = ?",
    )
    .bind(WORKSPACE)
    .bind(AGENT)
    .fetch_one(&mut probe)
    .await
    .unwrap();
    assert!([JOIN_TIME_MS, JOIN_TIME_MS + 1000].contains(&stored));
    assert_eq!(first.joined_at_ms, stored);
    assert_eq!(
        capability_tokens(&mut probe).await.len(),
        11,
        "the seed ran exactly once"
    );
}

/// Gate 3 ②: after a capability is actively removed, a re-enrollment must not
/// re-widen it — the seed is create-only, and this enrollment does not create.
#[tokio::test]
async fn re_enrollment_never_re_widens_a_narrowed_capability_set() {
    let (_dir, path) = schema_with_fixtures().await;
    let store = SqliteAccessStore::open(&path).await.unwrap();

    let created = add_workspace_agent_member(
        &store,
        &store,
        &clock(),
        &human(OWNER),
        WorkspaceId::parse(WORKSPACE).unwrap(),
        AgentId::parse(AGENT).unwrap(),
    )
    .await
    .unwrap();

    // Actively narrow the set out-of-band, then re-enroll.
    let mut probe = open_probe(&path).await;
    sqlx::query(
        "DELETE FROM workspace_agent_capabilities \
         WHERE workspace_id = ? AND agent_id = ? AND capability = 'manage_schedules'",
    )
    .bind(WORKSPACE)
    .bind(AGENT)
    .execute(&mut probe)
    .await
    .unwrap();
    assert_eq!(capability_tokens(&mut probe).await.len(), 10);

    let re_enrolled = add_workspace_agent_member(
        &store,
        &store,
        &FixedClock(JOIN_TIME_MS + 1000),
        &human(OWNER),
        WorkspaceId::parse(WORKSPACE).unwrap(),
        AgentId::parse(AGENT).unwrap(),
    )
    .await
    .unwrap();

    assert_eq!(re_enrolled.joined_at_ms, created.joined_at_ms);
    let mut probe = open_probe(&path).await;
    assert_eq!(
        capability_tokens(&mut probe).await.len(),
        10,
        "re-enrollment re-widened a set it did not create"
    );
}

#[tokio::test]
async fn a_missing_agent_is_refused_before_any_write() {
    let (_dir, path) = schema_with_fixtures().await;
    let store = SqliteAccessStore::open(&path).await.unwrap();

    let error = add_workspace_agent_member(
        &store,
        &store,
        &clock(),
        &human(OWNER),
        WorkspaceId::parse(WORKSPACE).unwrap(),
        AgentId::parse("ghost").unwrap(),
    )
    .await
    .unwrap_err();

    assert!(matches!(error, AddAgentMemberError::AgentNotFound));
    let mut probe = open_probe(&path).await;
    assert_eq!(membership_count(&mut probe).await, 0);
}

#[tokio::test]
async fn a_foreign_agent_is_refused_before_any_write() {
    let (_dir, path) = schema_with_fixtures().await;
    let store = SqliteAccessStore::open(&path).await.unwrap();

    let error = add_workspace_agent_member(
        &store,
        &store,
        &clock(),
        &human(OTHER),
        WorkspaceId::parse(WORKSPACE).unwrap(),
        AgentId::parse(AGENT).unwrap(),
    )
    .await
    .unwrap_err();

    assert!(matches!(error, AddAgentMemberError::AgentNotOwned));
    let mut probe = open_probe(&path).await;
    assert_eq!(membership_count(&mut probe).await, 0);
}
