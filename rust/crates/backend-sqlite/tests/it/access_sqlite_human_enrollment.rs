//! Production SQLite human enrollment against the real, checked-in current
//! schema.
//!
//! Like the agent enrollment test, the database is materialized only by the
//! shared current-schema loader, so the real `workspace_users` foreign keys,
//! `role` CHECK and primary key are all in force; this test adds no schema of
//! its own. It drives the production `SqliteAccessStore` through the same
//! `add_workspace_human_member` entry the HTTP surface calls, and pins the
//! behavior the boundary gate cannot: the in-lock authority re-derivation, the
//! create-only role/join-time seed, and idempotent re-enrollment.

use sqlx::{
    Connection, SqliteConnection,
    sqlite::{SqliteConnectOptions, SqliteJournalMode},
};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use silverhand_backend_core::foundation::identity::{Principal, UserId, WorkspaceId};
use silverhand_backend_core::identity_access::access::{
    AddHumanMemberError, EnrollmentClock, HumanEnrollmentOutcome, HumanEnrollmentReadPort,
    ViewerAnnounce, ViewerAnnounceFailure, ViewerWorkspacesChangedSink, WorkspaceRole,
    add_workspace_human_member,
};
use silverhand_backend_sqlite::identity_access::access::SqliteAccessStore;
use tempfile::TempDir;

use crate::current_schema;

const WORKSPACE: &str = "ws-1";
const ADMIN: &str = "user-admin";
const OWNER: &str = "user-owner";
const MEMBER: &str = "user-member";
const TARGET: &str = "user-target";
const TARGET_UPDATED_AT: i64 = 77;

/// A known, fixed join-time candidate, so the create path can be asserted to
/// store *exactly* the value the application's clock produced.
const JOIN_TIME_MS: i64 = 1_700_000_000_123;

/// The pre-existing join time on a row seeded before the test, so a re-enrollment
/// can be shown to keep it rather than overwrite it with the clock candidate.
const ORIGINAL_JOIN_MS: i64 = 111;

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

/// A sink that only counts announces, so a genuine create can be shown to
/// announce exactly once and a re-enrollment to stay silent, without a bus.
#[derive(Clone, Default)]
struct CountingSink {
    announces: Arc<AtomicUsize>,
}

impl ViewerWorkspacesChangedSink for CountingSink {
    async fn viewer_added_to_workspace(
        &self,
        _user_id: &UserId,
        _workspace_id: &WorkspaceId,
        _role: WorkspaceRole,
    ) -> Result<(), ViewerAnnounceFailure> {
        self.announces.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// Materializes the real schema, inserts the parent rows the enrollment foreign
/// keys require — the workspace, the actor users seeded at their governance
/// roles, and the target user — and returns the tempdir (kept alive) and path.
async fn schema_with_fixtures() -> (TempDir, std::path::PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("access.db");
    let mut connection = current_schema::create_current_schema(&path).await;

    // Actor users carry no avatar (both avatar columns null, per the CHECK).
    for user in [ADMIN, OWNER, MEMBER] {
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

    // The target user carries an avatar, so the read port's avatar_url derivation
    // is exercised against the real schema (both avatar columns set together).
    sqlx::query(
        "INSERT INTO users \
         (id, name, display_name, avatar_storage_key, avatar_content_type, created_at, updated_at) \
         VALUES (?, 'Target Person', 'Target', 'avatars/target.png', 'image/png', 10, ?)",
    )
    .bind(TARGET)
    .bind(TARGET_UPDATED_AT)
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

    // Seed each actor's governance role row. The target is deliberately left
    // out; individual tests seed it when they need a re-enrollment.
    for (user, role) in [(ADMIN, "admin"), (OWNER, "owner"), (MEMBER, "member")] {
        seed_membership(&mut connection, user, role, 1).await;
    }

    connection.close().await.unwrap();
    (directory, path)
}

async fn seed_membership(
    connection: &mut SqliteConnection,
    user: &str,
    role: &str,
    joined_at: i64,
) {
    sqlx::query(
        "INSERT INTO workspace_users (workspace_id, user_id, joined_at, role) VALUES (?, ?, ?, ?)",
    )
    .bind(WORKSPACE)
    .bind(user)
    .bind(joined_at)
    .bind(role)
    .execute(connection)
    .await
    .unwrap();
}

async fn open_probe(path: &Path) -> SqliteConnection {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Wal);
    SqliteConnection::connect_with(&options).await.unwrap()
}

/// The stored `(joined_at, role)` for a user's membership row, or `None` when no
/// row exists — so "nothing was written" is distinguishable from a written row.
async fn stored_membership(probe: &mut SqliteConnection, user: &str) -> Option<(i64, String)> {
    sqlx::query_as(
        "SELECT joined_at, role FROM workspace_users WHERE workspace_id = ? AND user_id = ?",
    )
    .bind(WORKSPACE)
    .bind(user)
    .fetch_optional(probe)
    .await
    .unwrap()
}

fn human(user: &str) -> Principal {
    Principal::Human {
        user_id: UserId::parse(user).unwrap(),
    }
}

/// Drive the production human-enrollment use case through the real store.
async fn enroll(
    store: &SqliteAccessStore,
    sink: &CountingSink,
    actor: Principal,
    target: &str,
) -> Result<HumanEnrollmentOutcome, AddHumanMemberError> {
    add_workspace_human_member(
        store,
        sink,
        &clock(),
        &actor,
        WorkspaceId::parse(WORKSPACE).unwrap(),
        UserId::parse(target).unwrap(),
    )
    .await
}

#[tokio::test]
async fn an_admin_enrolls_a_new_human_at_member_role_and_the_clock_join_time() {
    let (_dir, path) = schema_with_fixtures().await;
    let store = SqliteAccessStore::open(&path).await.unwrap();
    let sink = CountingSink::default();

    let (member, announce) = enroll(&store, &sink, human(ADMIN), TARGET)
        .await
        .unwrap()
        .into_parts();

    assert_eq!(member.role, WorkspaceRole::Member);
    assert_eq!(
        member.joined_at_ms, JOIN_TIME_MS,
        "a create stores exactly the candidate the clock produced"
    );
    assert!(matches!(announce, ViewerAnnounce::Delivered));
    assert_eq!(
        sink.announces.load(Ordering::SeqCst),
        1,
        "a genuine create announces exactly once"
    );

    let mut probe = open_probe(&path).await;
    assert_eq!(
        stored_membership(&mut probe, TARGET).await,
        Some((JOIN_TIME_MS, "member".to_owned()))
    );
}

#[tokio::test]
async fn a_re_enrollment_keeps_the_original_role_and_join_time_and_is_silent() {
    let (_dir, path) = schema_with_fixtures().await;
    // The target already governs the workspace as owner, from an earlier join.
    let mut setup = open_probe(&path).await;
    seed_membership(&mut setup, TARGET, "owner", ORIGINAL_JOIN_MS).await;
    setup.close().await.unwrap();

    let store = SqliteAccessStore::open(&path).await.unwrap();
    let sink = CountingSink::default();

    let (member, announce) = enroll(&store, &sink, human(ADMIN), TARGET)
        .await
        .unwrap()
        .into_parts();

    assert_eq!(
        member.role,
        WorkspaceRole::Owner,
        "the idempotent write never demotes the existing owner to member"
    );
    assert_eq!(
        member.joined_at_ms, ORIGINAL_JOIN_MS,
        "re-enrollment keeps the original join time, not the new clock read"
    );
    assert!(
        matches!(announce, ViewerAnnounce::NotOwed),
        "no announce was OWED here — a different fact from one that was delivered"
    );
    assert_eq!(
        sink.announces.load(Ordering::SeqCst),
        0,
        "a re-enrollment created nothing, so it announces nothing"
    );

    let mut probe = open_probe(&path).await;
    assert_eq!(
        stored_membership(&mut probe, TARGET).await,
        Some((ORIGINAL_JOIN_MS, "owner".to_owned())),
        "the stored row is untouched"
    );
}

#[tokio::test]
async fn an_owner_actor_may_enroll() {
    let (_dir, path) = schema_with_fixtures().await;
    let store = SqliteAccessStore::open(&path).await.unwrap();
    let sink = CountingSink::default();

    let (member, _announce) = enroll(&store, &sink, human(OWNER), TARGET)
        .await
        .unwrap()
        .into_parts();

    assert_eq!(member.role, WorkspaceRole::Member);
    let mut probe = open_probe(&path).await;
    assert!(stored_membership(&mut probe, TARGET).await.is_some());
}

#[tokio::test]
async fn the_system_principal_may_enroll_without_a_role_row() {
    let (_dir, path) = schema_with_fixtures().await;
    let store = SqliteAccessStore::open(&path).await.unwrap();
    let sink = CountingSink::default();

    // System holds no workspace_users row; the rule admits it without one.
    let (member, announce) = enroll(&store, &sink, Principal::System, TARGET)
        .await
        .unwrap()
        .into_parts();

    assert!(matches!(announce, ViewerAnnounce::Delivered));
    assert_eq!(sink.announces.load(Ordering::SeqCst), 1);
    assert_eq!(member.role, WorkspaceRole::Member);
    let mut probe = open_probe(&path).await;
    assert!(stored_membership(&mut probe, TARGET).await.is_some());
}

#[tokio::test]
async fn a_member_actor_is_refused_in_the_lock_and_writes_nothing() {
    let (_dir, path) = schema_with_fixtures().await;
    let store = SqliteAccessStore::open(&path).await.unwrap();
    let sink = CountingSink::default();

    let error = enroll(&store, &sink, human(MEMBER), TARGET)
        .await
        .unwrap_err();

    assert!(
        matches!(error, AddHumanMemberError::Forbidden),
        "a plain member does not govern the people roster"
    );
    assert_eq!(
        sink.announces.load(Ordering::SeqCst),
        0,
        "a refused enrollment announces nothing"
    );

    let mut probe = open_probe(&path).await;
    assert_eq!(
        stored_membership(&mut probe, TARGET).await,
        None,
        "the rollback left no row for the target"
    );
}

#[tokio::test]
async fn an_agent_actor_is_refused_in_the_lock_and_writes_nothing() {
    let (_dir, path) = schema_with_fixtures().await;
    let store = SqliteAccessStore::open(&path).await.unwrap();
    let sink = CountingSink::default();

    let actor = Principal::Agent {
        agent_id: silverhand_backend_core::foundation::identity::AgentId::parse("agent-x").unwrap(),
    };
    let error = enroll(&store, &sink, actor, TARGET).await.unwrap_err();

    assert!(
        matches!(error, AddHumanMemberError::Forbidden),
        "an agent principal never governs a workspace people roster"
    );

    let mut probe = open_probe(&path).await;
    assert_eq!(stored_membership(&mut probe, TARGET).await, None);
}

#[tokio::test]
async fn the_read_port_derives_the_avatar_url_and_reports_a_missing_user_as_none() {
    let (_dir, path) = schema_with_fixtures().await;
    let store = SqliteAccessStore::open(&path).await.unwrap();

    let present = store
        .user(&UserId::parse(TARGET).unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(present.display_name, "Target");
    assert_eq!(
        present.avatar_url.as_deref(),
        Some(format!("/api/users/{TARGET}/avatar?v={TARGET_UPDATED_AT}").as_str()),
        "a present storage key yields the versioned avatar path"
    );

    // An actor user with no avatar columns has no avatar URL.
    let no_avatar = store
        .user(&UserId::parse(ADMIN).unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(no_avatar.avatar_url, None);

    // A user that does not exist is the `None` the surface maps to 404.
    let missing = store.user(&UserId::parse("ghost").unwrap()).await.unwrap();
    assert!(missing.is_none());
}
