//! Real-SQLite gates for the product schedule CANCEL unit of work.
//!
//! Every gate drives the actual `SqliteScheduleCancel` adapter against a database
//! built from the checked-in current-schema snapshot — the same DDL the product
//! ships — so the ONE `BEGIN IMMEDIATE` transaction, the owner re-bind, the
//! `manage_schedules` resolution (agent-home owner arm, explicit grant, and the
//! missing-grant / workspace-less / deleted-target branches), the
//! `pending|firing → cancelled` CAS, and the post-image projection are exercised
//! over real rows rather than a fake.
//!
//! The two race gates pin BOTH legal serial orders of a cancel against the
//! fire-path store's own `IMMEDIATE` claim/settlement — never "one winner": a
//! cancel after a claim wins and the later settlement CAS no-ops, and a cancel
//! before a claim makes the claim find nothing.

use silverhand_backend_core::conversation_messaging::messaging::schedule::{
    Clock, DueCandidate, FiredCount, ScheduleCancelOutcome, ScheduleCancelUnitOfWork, ScheduleId,
    ScheduleStatus, ScheduleStore, SettlementAction,
};
use silverhand_backend_core::foundation::identity::AgentId;
use silverhand_backend_sqlite::conversation_messaging::messaging::schedule::{
    SqliteScheduleCancel, SqliteScheduleStore,
};
use sqlx::{ConnectOptions, Connection, Row, SqliteConnection, sqlite::SqliteConnectOptions};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

use crate::current_schema;

const NOW: i64 = 9_000_000;
const OWNER: &str = "agent-sched";
const SPEC_ONCE: &str = r#"{"kind":"once","fire_at":100000}"#;

/// A test clock so `cancelled_at` / `updated_at` are exact.
struct FixedClock(i64);
impl Clock for FixedClock {
    fn now(&self) -> i64 {
        self.0
    }
}

fn owner() -> AgentId {
    AgentId::parse(OWNER).unwrap()
}

async fn open_db() -> (TempDir, PathBuf, SqliteConnection) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("schedules.sqlite");
    let mut connection = current_schema::create_current_schema(&path).await;
    seed_owner(&mut connection).await;
    (directory, path, connection)
}

async fn seed_owner(connection: &mut SqliteConnection) {
    sqlx::query(
        "INSERT INTO users (id, name, display_name, created_at, updated_at)
         VALUES ('user-1', 'user-1', 'User One', 1, 1)",
    )
    .execute(&mut *connection)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO hosts (id, name, kind, created_at, user_id)
         VALUES ('host-1', 'host-1', 'remote', 1, 'user-1')",
    )
    .execute(&mut *connection)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO agents (
            id, name, runtime, host_id, created_at, updated_at,
            display_name, created_by_user_id, session_scope
         ) VALUES (?, 'agent-sched', 'codex', 'host-1', 1, 1,
             'Schedule Agent', 'user-1', 'conversation')",
    )
    .bind(OWNER)
    .execute(&mut *connection)
    .await
    .unwrap();
}

/// A named (human-owned) workspace, plus `agent-sched` as a member. Grant is
/// added separately so a member-without-grant can be seeded.
async fn seed_named_workspace(connection: &mut SqliteConnection, id: &str, grant: bool) {
    sqlx::query(
        "INSERT INTO workspaces (id, name, name_key, owner_user_id, created_at, updated_at)
         VALUES (?, 'Team', ?, 'user-1', 1, 1)",
    )
    .bind(id)
    .bind(id) // any lowercase, non-empty key is fine
    .execute(&mut *connection)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO workspace_members (workspace_id, agent_id, joined_at) VALUES (?, ?, 1)",
    )
    .bind(id)
    .bind(OWNER)
    .execute(&mut *connection)
    .await
    .unwrap();
    if grant {
        sqlx::query(
            "INSERT INTO workspace_agent_capabilities (workspace_id, agent_id, capability)
             VALUES (?, ?, 'manage_schedules')",
        )
        .bind(id)
        .bind(OWNER)
        .execute(&mut *connection)
        .await
        .unwrap();
    }
}

/// An agent-home workspace owned by `agent-sched` — the owner arm grants
/// `manage_schedules` with no capability row.
async fn seed_agent_home_workspace(connection: &mut SqliteConnection, id: &str) {
    sqlx::query(
        "INSERT INTO workspaces (id, name, owner_agent_id, created_at, updated_at)
         VALUES (?, 'Home', ?, 1, 1)",
    )
    .bind(id)
    .bind(OWNER)
    .execute(&mut *connection)
    .await
    .unwrap();
}

/// A workspace-placed conversation in `workspace_id`, roster `open` → `#title`.
async fn seed_workspace_conversation(
    connection: &mut SqliteConnection,
    id: &str,
    workspace_id: &str,
    title: &str,
) {
    sqlx::query(
        "INSERT INTO conversations
            (id, workspace_id, placement_kind, roster_policy, title, title_key, status,
             created_at, updated_at)
         VALUES (?, ?, 'workspace', 'open', ?, ?, 'active', 1, 1)",
    )
    .bind(id)
    .bind(workspace_id)
    .bind(title)
    .bind(id)
    .execute(&mut *connection)
    .await
    .unwrap();
}

/// A direct-pair conversation (NULL workspace, roster `fixed` → `@title`). Its
/// endpoints must name existing principals (a schema trigger enforces it), so it
/// pairs the seeded agent with the seeded user.
async fn seed_direct_pair_conversation(connection: &mut SqliteConnection, id: &str, title: &str) {
    sqlx::query(
        "INSERT INTO conversations
            (id, placement_kind, roster_policy, title,
             endpoint_a_kind, endpoint_a_id, endpoint_b_kind, endpoint_b_id,
             status, created_at, updated_at)
         VALUES (?, 'direct_pair', 'fixed', ?, 'agent', ?, 'human', 'user-1', 'active', 1, 1)",
    )
    .bind(id)
    .bind(title)
    .bind(OWNER)
    .execute(&mut *connection)
    .await
    .unwrap();
}

async fn seed_schedule(
    connection: &mut SqliteConnection,
    id: &str,
    owner_agent_id: &str,
    target: &str,
    status: &str,
) {
    sqlx::query(
        "INSERT INTO schedules (
            id, owner_agent_id, target_conversation_id, title, message, spec_json,
            next_fire_at, status, fired_count, max_occurrences, until_at,
            consecutive_failure_count, created_at, updated_at
         ) VALUES (?, ?, ?, 'digest', 'hi', ?, 100000, ?, 0, NULL, NULL, 0, 1, 1)",
    )
    .bind(id)
    .bind(owner_agent_id)
    .bind(target)
    .bind(SPEC_ONCE)
    .bind(status)
    .execute(&mut *connection)
    .await
    .unwrap();
}

async fn cancel_adapter(path: &Path) -> SqliteScheduleCancel<FixedClock> {
    SqliteScheduleCancel::open_with_clock(path, FixedClock(NOW))
        .await
        .unwrap()
}

async fn row_status(path: &Path, id: &str) -> String {
    let mut connection = SqliteConnectOptions::new()
        .filename(path)
        .connect()
        .await
        .unwrap();
    let status: String = sqlx::query("SELECT status FROM schedules WHERE id = ?")
        .bind(id)
        .fetch_one(&mut connection)
        .await
        .unwrap()
        .get("status");
    connection.close().await.unwrap();
    status
}

fn candidate(id: &str) -> DueCandidate {
    DueCandidate {
        id: ScheduleId::parse(id).unwrap(),
        expected_fired_count: FiredCount::new(0).unwrap(),
        expected_next_fire_at: 100_000,
    }
}

// --- existence & owner-bind --------------------------------------------------

#[tokio::test]
async fn an_absent_schedule_is_not_found() {
    let (_dir, path, connection) = open_db().await;
    connection.close().await.unwrap();
    let cancel = cancel_adapter(&path).await;
    let outcome = cancel
        .cancel_owned_schedule(&ScheduleId::parse("ghost").unwrap(), &owner())
        .await
        .unwrap();
    assert_eq!(outcome, ScheduleCancelOutcome::NotFound);
}

#[tokio::test]
async fn a_schedule_owned_by_another_agent_is_not_owner_and_leaves_it_pending() {
    let (_dir, path, mut connection) = open_db().await;
    // Owner-cleanup target (no conversation) so only the owner-bind can refuse.
    seed_schedule(&mut connection, "s1", OWNER, "gone", "pending").await;
    connection.close().await.unwrap();
    let cancel = cancel_adapter(&path).await;
    let outcome = cancel
        .cancel_owned_schedule(
            &ScheduleId::parse("s1").unwrap(),
            &AgentId::parse("someone-else").unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(outcome, ScheduleCancelOutcome::NotOwner);
    assert_eq!(row_status(&path, "s1").await, "pending");
}

/// The owner-bind-precedes-consult falsifier. The schedule's target carries a
/// CORRUPT workspace id (`''`, which `WorkspaceId::parse` rejects): a cancel that
/// reached the target/capability read would fault and surface a 500. A NON-owner
/// still gets `NotOwner` — observable proof the owner gate ran BEFORE, and short
/// of, any target/capability consult. (Before the reorder this returned an Err.)
#[tokio::test]
async fn a_non_owner_is_refused_before_a_faulting_target_is_ever_read() {
    let (_dir, path, mut connection) = open_db().await;
    // A workspace whose id is the empty string: a satisfying FK row exists, but
    // the adapter's `WorkspaceId::parse("")` would fault if it read this target.
    seed_agent_home_workspace(&mut connection, "").await;
    seed_workspace_conversation(&mut connection, "c-corrupt", "", "ops").await;
    seed_schedule(&mut connection, "s1", OWNER, "c-corrupt", "pending").await;
    connection.close().await.unwrap();
    let cancel = cancel_adapter(&path).await;

    // First prove the fault is REAL: the OWNER, who passes owner-bind and so does
    // reach the target read, gets a driver/corruption Err (not a clean outcome).
    let owner_reaches_the_read = cancel
        .cancel_owned_schedule(&ScheduleId::parse("s1").unwrap(), &owner())
        .await;
    assert!(
        owner_reaches_the_read.is_err(),
        "the corrupt workspace id must fault ONCE the target read runs"
    );
    assert_eq!(row_status(&path, "s1").await, "pending");

    // A non-owner must be refused BEFORE that read — a clean NotOwner, not the 500.
    let outcome = cancel
        .cancel_owned_schedule(
            &ScheduleId::parse("s1").unwrap(),
            &AgentId::parse("someone-else").unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(outcome, ScheduleCancelOutcome::NotOwner);
    assert_eq!(row_status(&path, "s1").await, "pending");
}

#[tokio::test]
async fn a_member_without_manage_schedules_is_capability_denied() {
    let (_dir, path, mut connection) = open_db().await;
    seed_named_workspace(&mut connection, "ws-team", false).await;
    seed_workspace_conversation(&mut connection, "c1", "ws-team", "ops").await;
    seed_schedule(&mut connection, "s1", OWNER, "c1", "pending").await;
    connection.close().await.unwrap();
    let cancel = cancel_adapter(&path).await;
    let outcome = cancel
        .cancel_owned_schedule(&ScheduleId::parse("s1").unwrap(), &owner())
        .await
        .unwrap();
    match outcome {
        ScheduleCancelOutcome::CapabilityDenied { workspace_id } => {
            assert_eq!(workspace_id.as_str(), "ws-team");
        }
        other => panic!("expected CapabilityDenied, got {other:?}"),
    }
    assert_eq!(row_status(&path, "s1").await, "pending");
}

#[tokio::test]
async fn an_explicit_grant_lets_the_owner_cancel() {
    let (_dir, path, mut connection) = open_db().await;
    seed_named_workspace(&mut connection, "ws-team", true).await;
    seed_workspace_conversation(&mut connection, "c1", "ws-team", "ops").await;
    seed_schedule(&mut connection, "s1", OWNER, "c1", "pending").await;
    connection.close().await.unwrap();
    let cancel = cancel_adapter(&path).await;
    let outcome = cancel
        .cancel_owned_schedule(&ScheduleId::parse("s1").unwrap(), &owner())
        .await
        .unwrap();
    match outcome {
        ScheduleCancelOutcome::Cancelled(view) => {
            assert_eq!(view.status, ScheduleStatus::Cancelled);
            assert_eq!(view.cancelled_at, Some(NOW));
            assert_eq!(view.updated_at, NOW);
            // roster `open` → `#title`.
            assert_eq!(view.target_label, "#ops");
        }
        other => panic!("expected Cancelled, got {other:?}"),
    }
    assert_eq!(row_status(&path, "s1").await, "cancelled");
}

#[tokio::test]
async fn the_agent_home_owner_arm_grants_without_a_capability_row() {
    let (_dir, path, mut connection) = open_db().await;
    seed_agent_home_workspace(&mut connection, "ws-home").await;
    seed_workspace_conversation(&mut connection, "c1", "ws-home", "home-board").await;
    seed_schedule(&mut connection, "s1", OWNER, "c1", "pending").await;
    connection.close().await.unwrap();
    let cancel = cancel_adapter(&path).await;
    let outcome = cancel
        .cancel_owned_schedule(&ScheduleId::parse("s1").unwrap(), &owner())
        .await
        .unwrap();
    assert!(matches!(outcome, ScheduleCancelOutcome::Cancelled(_)));
    assert_eq!(row_status(&path, "s1").await, "cancelled");
}

// --- owner cleanup for a target with no capability axis ----------------------

#[tokio::test]
async fn a_deleted_target_permits_owner_cleanup_and_labels_it_unavailable() {
    let (_dir, path, mut connection) = open_db().await;
    // No conversation row for "gone" — the target was deleted.
    seed_schedule(&mut connection, "s1", OWNER, "gone", "pending").await;
    connection.close().await.unwrap();
    let cancel = cancel_adapter(&path).await;
    let outcome = cancel
        .cancel_owned_schedule(&ScheduleId::parse("s1").unwrap(), &owner())
        .await
        .unwrap();
    match outcome {
        ScheduleCancelOutcome::Cancelled(view) => {
            assert_eq!(view.status, ScheduleStatus::Cancelled);
            assert_eq!(view.target_label, "Unavailable conversation");
        }
        other => panic!("expected Cancelled, got {other:?}"),
    }
}

#[tokio::test]
async fn a_workspace_less_direct_pair_target_permits_owner_cleanup() {
    let (_dir, path, mut connection) = open_db().await;
    seed_direct_pair_conversation(&mut connection, "dp1", "Alice ↔ Bob").await;
    seed_schedule(&mut connection, "s1", OWNER, "dp1", "pending").await;
    connection.close().await.unwrap();
    let cancel = cancel_adapter(&path).await;
    let outcome = cancel
        .cancel_owned_schedule(&ScheduleId::parse("s1").unwrap(), &owner())
        .await
        .unwrap();
    match outcome {
        ScheduleCancelOutcome::Cancelled(view) => {
            // roster `fixed` → `@title`.
            assert_eq!(view.target_label, "@Alice ↔ Bob");
        }
        other => panic!("expected Cancelled, got {other:?}"),
    }
}

// --- terminal-state CAS miss -------------------------------------------------

#[tokio::test]
async fn a_terminal_schedule_is_not_cancellable_with_its_current_status() {
    for (seed_status, expected) in [
        ("done", ScheduleStatus::Done),
        ("cancelled", ScheduleStatus::Cancelled),
        ("failed", ScheduleStatus::Failed),
    ] {
        let (_dir, path, mut connection) = open_db().await;
        seed_schedule(&mut connection, "s1", OWNER, "gone", seed_status).await;
        connection.close().await.unwrap();
        let cancel = cancel_adapter(&path).await;
        let outcome = cancel
            .cancel_owned_schedule(&ScheduleId::parse("s1").unwrap(), &owner())
            .await
            .unwrap();
        assert_eq!(
            outcome,
            ScheduleCancelOutcome::NotCancellable {
                current_status: expected
            },
            "status {seed_status} must be a CAS miss"
        );
        assert_eq!(row_status(&path, "s1").await, seed_status);
    }
}

// --- the two legal serial orders against the fire path -----------------------

#[tokio::test]
async fn claim_then_cancel_wins_and_the_later_settlement_no_ops() {
    let (_dir, path, mut connection) = open_db().await;
    seed_schedule(&mut connection, "s1", OWNER, "gone", "pending").await;
    connection.close().await.unwrap();

    let store = SqliteScheduleStore::open_with_clock(&path, FixedClock(NOW))
        .await
        .unwrap();
    let cancel = cancel_adapter(&path).await;

    // (a) The fire path claims first: pending → firing.
    let claimed = store
        .claim(&candidate("s1"))
        .await
        .unwrap()
        .expect("the occurrence must be claimable");
    assert_eq!(claimed.id.as_str(), "s1");
    assert_eq!(row_status(&path, "s1").await, "firing");

    // The cancel races in after the claim and wins: firing → cancelled.
    let outcome = cancel
        .cancel_owned_schedule(&ScheduleId::parse("s1").unwrap(), &owner())
        .await
        .unwrap();
    assert!(matches!(outcome, ScheduleCancelOutcome::Cancelled(_)));
    assert_eq!(row_status(&path, "s1").await, "cancelled");

    // The delivered fire's settlement CAS (predicated status='firing') no-ops.
    let settled = store
        .apply_settlement(
            &ScheduleId::parse("s1").unwrap(),
            NOW,
            &SettlementAction::MarkDone,
        )
        .await
        .unwrap();
    assert!(!settled, "settlement after a cancel must be a benign no-op");
    assert_eq!(row_status(&path, "s1").await, "cancelled");
}

#[tokio::test]
async fn cancel_then_claim_finds_nothing() {
    let (_dir, path, mut connection) = open_db().await;
    seed_schedule(&mut connection, "s1", OWNER, "gone", "pending").await;
    connection.close().await.unwrap();

    let store = SqliteScheduleStore::open_with_clock(&path, FixedClock(NOW))
        .await
        .unwrap();
    let cancel = cancel_adapter(&path).await;

    // (b) The cancel commits first: pending → cancelled.
    let outcome = cancel
        .cancel_owned_schedule(&ScheduleId::parse("s1").unwrap(), &owner())
        .await
        .unwrap();
    assert!(matches!(outcome, ScheduleCancelOutcome::Cancelled(_)));

    // The fire path's occurrence claim (predicated status='pending') finds nothing.
    let claimed = store.claim(&candidate("s1")).await.unwrap();
    assert!(claimed.is_none(), "a cancelled row is no longer claimable");
    assert_eq!(row_status(&path, "s1").await, "cancelled");
}
