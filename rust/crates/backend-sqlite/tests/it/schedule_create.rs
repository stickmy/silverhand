//! Real-SQLite gates for the product schedule CREATE atomic command.
//!
//! Drives `SqliteScheduleCreate::insert_within_cap` against a database built
//! from the checked-in current-schema snapshot. Proves the happy-path insert
//! projects the canonical row (including the `target_label` LEFT JOIN and the
//! server-minted id/timestamps/`status='pending'`), the caps round-trip, and —
//! the load-bearing concurrency contract — that two creates racing at a cap of 1
//! settle as exactly one `Created` and one `CapExceeded`, with the loser leaving
//! no row: the cap is enforced INSIDE the single `BEGIN IMMEDIATE` writer, not by
//! a check-then-act split, and lock contention never surfaces as a third
//! outcome.

use silverhand_backend_core::conversation_messaging::messaging::schedule::{
    OnceWhen, ScheduleCreateGuardError, ScheduleCreateOutcome, ScheduleCreateRequest,
    ScheduleCreateStore, ScheduleSpecInput, ScheduleStatus,
};
use silverhand_backend_core::conversation_messaging::messaging::schedule::{
    ScheduleMessage, ScheduleTitle,
};
use silverhand_backend_core::foundation::identity::{AgentId, ConversationId};
use silverhand_backend_sqlite::conversation_messaging::messaging::schedule::{
    ProductionCronEvaluator, SqliteScheduleCreate,
};
use sqlx::{Connection, Row, SqliteConnection};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicI64, Ordering},
};
use tempfile::TempDir;

use crate::current_schema;

const OWNER: &str = "agent-sched";
const TARGET: &str = "conv-1";

#[derive(Clone, Copy)]
struct FixedClock(i64);
impl silverhand_backend_core::conversation_messaging::messaging::schedule::Clock for FixedClock {
    fn now(&self) -> i64 {
        self.0
    }
}

#[derive(Clone)]
struct MovingClock(Arc<AtomicI64>);
impl silverhand_backend_core::conversation_messaging::messaging::schedule::Clock for MovingClock {
    fn now(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

async fn open_db() -> (TempDir, PathBuf, SqliteConnection) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("schedules.sqlite");
    let mut connection = current_schema::create_current_schema(&path).await;
    seed(&mut connection).await;
    (directory, path, connection)
}

async fn seed(connection: &mut SqliteConnection) {
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
         ) VALUES (?, ?, 'codex', 'host-1', 1, 1, 'Agent', 'user-1', 'conversation')",
    )
    .bind(OWNER)
    .bind(OWNER)
    .execute(&mut *connection)
    .await
    .unwrap();
    // A resolvable target board so the create's label read has a real title.
    // A `workspace` placement requires its workspace row plus `title_key`.
    sqlx::query(
        "INSERT INTO workspaces (id, name, name_key, created_at, updated_at)
         VALUES ('ws-1', 'ws', 'ws', 1, 1)",
    )
    .execute(&mut *connection)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO workspace_members (workspace_id, agent_id, joined_at)
         VALUES ('ws-1', ?, 1)",
    )
    .bind(OWNER)
    .execute(&mut *connection)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO workspace_agent_capabilities (workspace_id, agent_id, capability)
         VALUES ('ws-1', ?, 'manage_schedules')",
    )
    .bind(OWNER)
    .execute(&mut *connection)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversations
            (id, workspace_id, placement_kind, roster_policy, title, title_key, status, created_at, updated_at)
         VALUES (?, 'ws-1', 'workspace', 'open', 'general', 'general', 'active', 1, 1)",
    )
    .bind(TARGET)
    .execute(&mut *connection)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversation_agents (conversation_id, agent_id, joined_at)
         VALUES (?, ?, 1)",
    )
    .bind(TARGET)
    .bind(OWNER)
    .execute(&mut *connection)
    .await
    .unwrap();
}

/// A once-spec create request firing comfortably inside the delay window.
fn once_request() -> ScheduleCreateRequest {
    ScheduleCreateRequest {
        owner_agent_id: AgentId::parse(OWNER).unwrap(),
        target_conversation_id: ConversationId::parse(TARGET).unwrap(),
        title: ScheduleTitle::parse("digest").unwrap(),
        message: ScheduleMessage::parse("hello").unwrap(),
        spec: ScheduleSpecInput::Once(OnceWhen::FireAt(10_000_000)),
        max_occurrences: None,
        until_at: None,
        allow_unbounded: false,
    }
}

fn cron_request(expression: &str, timezone: &str, bounded: bool) -> ScheduleCreateRequest {
    ScheduleCreateRequest {
        owner_agent_id: AgentId::parse(OWNER).unwrap(),
        target_conversation_id: ConversationId::parse(TARGET).unwrap(),
        title: ScheduleTitle::parse("digest").unwrap(),
        message: ScheduleMessage::parse("hello").unwrap(),
        spec: ScheduleSpecInput::Cron {
            expression: expression.into(),
            timezone: timezone.into(),
        },
        max_occurrences: bounded.then_some(1),
        until_at: None,
        allow_unbounded: false,
    }
}

async fn count_rows(path: &Path, owner: &str) -> i64 {
    let mut connection = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
        .await
        .unwrap();
    let row = sqlx::query("SELECT COUNT(*) AS c FROM schedules WHERE owner_agent_id = ?")
        .bind(owner)
        .fetch_one(&mut connection)
        .await
        .unwrap();
    row.try_get::<i64, _>("c").unwrap()
}

#[tokio::test]
async fn a_valid_create_inserts_one_pending_row_and_projects_the_canonical_view() {
    let (_dir, path, _connection) = open_db().await;
    let store = SqliteScheduleCreate::open_with_clock(&path, FixedClock(1_000))
        .await
        .unwrap();

    // The core canonicalization is exercised in its own unit tests; here the plan
    // is built through the same request→plan path with a trivial cron evaluator
    // (unused by a `once` request).
    let outcome = store.create(once_request(), &NoCron).await.unwrap();

    let view = match outcome {
        ScheduleCreateOutcome::Created(view) => view,
        other => panic!("unexpected non-created outcome on a valid empty board: {other:?}"),
    };
    assert_eq!(view.owner_agent_id.as_str(), OWNER);
    assert_eq!(view.target_conversation_id, TARGET);
    assert_eq!(view.status, ScheduleStatus::Pending);
    assert_eq!(view.fired_count, 0);
    assert_eq!(view.next_fire_at, 10_000_000);
    // A resolvable open board renders `#title`.
    assert_eq!(view.target_label, "#general");
    assert!(!view.id.as_str().is_empty());

    assert_eq!(count_rows(&path, OWNER).await, 1);
}

#[tokio::test]
async fn stateful_agent_participation_and_capability_gates_are_fresh_and_zero_write() {
    enum Mutation {
        Agent,
        Participation,
        Capability,
    }
    for (mutation, expected) in [
        (Mutation::Agent, ScheduleCreateOutcome::AgentNotFound),
        (
            Mutation::Participation,
            ScheduleCreateOutcome::ActorNotParticipant,
        ),
        (
            Mutation::Capability,
            ScheduleCreateOutcome::CapabilityDenied,
        ),
    ] {
        let (_dir, path, mut connection) = open_db().await;
        match mutation {
            Mutation::Agent => {
                sqlx::query("DELETE FROM agents WHERE id=?")
                    .bind(OWNER)
                    .execute(&mut connection)
                    .await
                    .unwrap();
            }
            Mutation::Participation => {
                sqlx::query(
                    "DELETE FROM conversation_agents WHERE conversation_id=? AND agent_id=?",
                )
                .bind(TARGET)
                .bind(OWNER)
                .execute(&mut connection)
                .await
                .unwrap();
            }
            Mutation::Capability => {
                sqlx::query("DELETE FROM workspace_agent_capabilities WHERE workspace_id='ws-1' AND agent_id=? AND capability='manage_schedules'")
                    .bind(OWNER)
                    .execute(&mut connection)
                    .await
                    .unwrap();
            }
        }
        connection.close().await.unwrap();
        let store = SqliteScheduleCreate::open_with_clock(&path, FixedClock(1_000))
            .await
            .unwrap();
        let outcome = store.create(once_request(), &NoCron).await.unwrap();
        assert_eq!(outcome, expected);
        assert_eq!(count_rows(&path, OWNER).await, 0);
    }
}

/// The concurrency contract: two creates racing at a cap of 1 settle as exactly
/// one `Created` and one `CapExceeded`, and the loser leaves no row. The loser
/// must OBSERVE the cap after acquiring the write transaction — a `SQLITE_BUSY`
/// / `PersistenceFailure` is NOT an accepted second outcome.
#[tokio::test]
async fn two_concurrent_creates_at_cap_one_yield_exactly_one_created_and_one_cap_exceeded() {
    // Cap the board at one active schedule for the duration of this test by
    // pre-seeding `SCHEDULE_MAX_ACTIVE_PER_AGENT - 1` active rows, so the two
    // racing creates compete for the single remaining slot.
    use silverhand_backend_core::conversation_messaging::messaging::schedule::SCHEDULE_MAX_ACTIVE_PER_AGENT;

    let (_dir, path, mut connection) = open_db().await;
    let filler = SCHEDULE_MAX_ACTIVE_PER_AGENT - 1;
    for index in 0..filler {
        sqlx::query(
            "INSERT INTO schedules (
                id, owner_agent_id, target_conversation_id, title, message, spec_json,
                next_fire_at, status, fired_count, max_occurrences, until_at,
                consecutive_failure_count, created_at, updated_at
             ) VALUES (?, ?, ?, 'digest', 'hi', ?, 100, 'pending', 0, NULL, NULL, 0, 1, 1)",
        )
        .bind(format!("fill-{index}"))
        .bind(OWNER)
        .bind(TARGET)
        .bind(r#"{"kind":"once","fire_at":100}"#)
        .execute(&mut connection)
        .await
        .unwrap();
    }
    drop(connection);

    let store_a = SqliteScheduleCreate::open_with_clock(&path, FixedClock(1_000))
        .await
        .unwrap();
    let store_b = SqliteScheduleCreate::open_with_clock(&path, FixedClock(1_000))
        .await
        .unwrap();
    let request_a = once_request();
    let request_b = once_request();

    // Fire both concurrently. One obtains the IMMEDIATE writer first and inserts
    // the last slot; the other waits on the writer (busy_timeout), then observes
    // the now-full count and returns CapExceeded.
    let (a, b) = tokio::join!(
        store_a.create(request_a, &NoCron),
        store_b.create(request_b, &NoCron),
    );
    let a = a.expect("first create must not fail with a lock error");
    let b = b.expect("second create must not fail with a lock error");

    let created = [&a, &b]
        .iter()
        .filter(|o| matches!(o, ScheduleCreateOutcome::Created(_)))
        .count();
    let refused = [&a, &b]
        .iter()
        .filter(|o| matches!(o, ScheduleCreateOutcome::CapExceeded { .. }))
        .count();
    assert_eq!(created, 1, "exactly one create wins the last slot");
    assert_eq!(refused, 1, "exactly one create observes CapExceeded");

    // The loser wrote nothing: the board holds the filler rows plus the single
    // winner, never both racers.
    assert_eq!(
        count_rows(&path, OWNER).await,
        SCHEDULE_MAX_ACTIVE_PER_AGENT
    );
}

/// A cron evaluator stub. The tests here use only `once` specs, which never
/// consult it; it exists solely to satisfy the `plan` type parameter.
struct NoCron;

/// Under the write lock the store re-decides the target lifecycle: a target that
/// no longer exists at insert time is `TargetNotFound`, with no row written.
#[tokio::test]
async fn a_missing_target_at_insert_time_is_target_not_found() {
    let (_dir, path, _c) = open_db().await;
    let store = SqliteScheduleCreate::open_with_clock(&path, FixedClock(1_000))
        .await
        .unwrap();
    // Point the request at a conversation that was never created.
    let mut request = once_request();
    request.target_conversation_id = ConversationId::parse("conv-missing").unwrap();
    let outcome = store.create(request, &NoCron).await.unwrap();
    assert!(matches!(outcome, ScheduleCreateOutcome::TargetNotFound));
    assert_eq!(count_rows(&path, OWNER).await, 0);
}

/// A closed target (`closed_at` set) is the hard barrier: `TargetClosed`, no row.
#[tokio::test]
async fn a_closed_target_at_insert_time_is_target_closed() {
    let (_dir, path, mut connection) = open_db().await;
    sqlx::query("UPDATE conversations SET closed_at = 123 WHERE id = ?")
        .bind(TARGET)
        .execute(&mut connection)
        .await
        .unwrap();
    drop(connection);

    let store = SqliteScheduleCreate::open_with_clock(&path, FixedClock(1_000))
        .await
        .unwrap();
    let outcome = store.create(once_request(), &NoCron).await.unwrap();
    assert!(matches!(outcome, ScheduleCreateOutcome::TargetClosed));
    assert_eq!(count_rows(&path, OWNER).await, 0);
}

/// Seed `count` ACTIVE (`pending`) filler rows for `owner`, so a subsequent
/// create competes for the remaining slot(s) up to the cap.
async fn seed_active_fillers(connection: &mut SqliteConnection, owner: &str, count: i64) {
    seed_active_fillers_for(connection, owner, TARGET, count).await;
}

async fn seed_active_fillers_for(
    connection: &mut SqliteConnection,
    owner: &str,
    target: &str,
    count: i64,
) {
    for index in 0..count {
        sqlx::query(
            "INSERT INTO schedules (
                id, owner_agent_id, target_conversation_id, title, message, spec_json,
                next_fire_at, status, fired_count, max_occurrences, until_at,
                consecutive_failure_count, created_at, updated_at
             ) VALUES (?, ?, ?, 'digest', 'hi', ?, 100, 'pending', 0, NULL, NULL, 0, 1, 1)",
        )
        .bind(format!("{owner}-fill-{index}"))
        .bind(owner)
        .bind(target)
        .bind(r#"{"kind":"once","fire_at":100}"#)
        .execute(&mut *connection)
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn target_state_precedes_cap_and_cap_precedes_time_policy() {
    use silverhand_backend_core::conversation_messaging::messaging::schedule::SCHEDULE_MAX_ACTIVE_PER_AGENT;

    for closed in [false, true] {
        let (_dir, path, mut connection) = open_db().await;
        seed_active_fillers_for(
            &mut connection,
            OWNER,
            "home-dm-1",
            SCHEDULE_MAX_ACTIVE_PER_AGENT,
        )
        .await;
        if closed {
            sqlx::query("UPDATE conversations SET closed_at=9 WHERE id=?")
                .bind(TARGET)
                .execute(&mut connection)
                .await
                .unwrap();
        } else {
            sqlx::query("DELETE FROM conversations WHERE id=?")
                .bind(TARGET)
                .execute(&mut connection)
                .await
                .unwrap();
        }
        connection.close().await.unwrap();
        let store = SqliteScheduleCreate::open_with_clock(&path, FixedClock(1_000))
            .await
            .unwrap();
        let outcome = store.create(once_request(), &NoCron).await.unwrap();
        assert!(if closed {
            matches!(outcome, ScheduleCreateOutcome::TargetClosed)
        } else {
            matches!(outcome, ScheduleCreateOutcome::TargetNotFound)
        });
        assert_eq!(
            count_rows(&path, OWNER).await,
            SCHEDULE_MAX_ACTIVE_PER_AGENT
        );
    }

    let (_dir, path, mut connection) = open_db().await;
    seed_active_fillers(&mut connection, OWNER, SCHEDULE_MAX_ACTIVE_PER_AGENT).await;
    connection.close().await.unwrap();
    let store = SqliteScheduleCreate::open_with_clock(&path, FixedClock(1_000))
        .await
        .unwrap();
    let mut invalid_time = once_request();
    invalid_time.spec = ScheduleSpecInput::Once(OnceWhen::FireAt(1_000));
    let outcome = store.create(invalid_time, &NoCron).await.unwrap();
    assert!(matches!(outcome, ScheduleCreateOutcome::CapExceeded { .. }));
    assert_eq!(
        count_rows(&path, OWNER).await,
        SCHEDULE_MAX_ACTIVE_PER_AGENT
    );
}

#[tokio::test]
async fn cron_create_preserves_repeat_bound_then_timezone_then_expression_precedence() {
    let cases = [
        (
            cron_request("not cron", "Mars/Olympus", false),
            ScheduleCreateGuardError::UnboundedRepeatRequiresExplicitLimit,
        ),
        (
            cron_request("not cron", "Mars/Olympus", true),
            ScheduleCreateGuardError::InvalidTimezone("Mars/Olympus".into()),
        ),
        (
            cron_request("not cron", "UTC", true),
            ScheduleCreateGuardError::InvalidCronExpression("not cron".into()),
        ),
    ];

    for (request, expected) in cases {
        let (_dir, path, connection) = open_db().await;
        connection.close().await.unwrap();
        let store = SqliteScheduleCreate::open_with_clock(&path, FixedClock(1_000))
            .await
            .unwrap();
        let outcome = store
            .create(request, &ProductionCronEvaluator)
            .await
            .unwrap();
        assert_eq!(outcome, ScheduleCreateOutcome::Invalid(expected));
        assert_eq!(count_rows(&path, OWNER).await, 0);
    }
}

/// The DETERMINISTIC contention positive-control: a real held write lock forces
/// the create to WAIT at `BEGIN IMMEDIATE`, then — once the lock is released with
/// the board AT the cap — observe `CapExceeded`. This proves the loser's outcome
/// is the business cap seen after acquiring the writer, NOT a `SQLITE_BUSY` /
/// `PersistenceFailure`, and does not infer overlap from any sleep.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_create_blocked_on_a_held_write_lock_resolves_to_cap_exceeded_never_busy() {
    use futures_util::poll;
    use silverhand_backend_core::conversation_messaging::messaging::schedule::SCHEDULE_MAX_ACTIVE_PER_AGENT;
    use std::pin::pin;
    use std::task::Poll;

    let (_dir, path, _seed) = open_db().await;

    // Connection A: hold the single writer with `BEGIN IMMEDIATE`, and inside the
    // UNCOMMITTED transaction insert exactly enough active rows to reach the cap.
    // The lock is held until A commits, so no other writer can proceed. A must
    // share the store's WAL journal mode + a busy_timeout, or a journal-mode
    // mismatch on the shared file surfaces as a spurious lock error.
    let mut a = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
        .await
        .unwrap();
    sqlx::raw_sql("PRAGMA journal_mode = WAL; PRAGMA busy_timeout = 5000")
        .execute(&mut a)
        .await
        .unwrap();
    sqlx::raw_sql("BEGIN IMMEDIATE")
        .execute(&mut a)
        .await
        .unwrap();
    seed_active_fillers(&mut a, OWNER, SCHEDULE_MAX_ACTIVE_PER_AGENT).await;

    // LOCK POSITIVE CONTROL (separately recomputable): an independent connection
    // with `busy_timeout = 0` that tries `BEGIN IMMEDIATE` right now MUST hit a
    // SQLite locked/busy error. This proves the writer is genuinely held at this
    // instant — the lock attribution rides on THIS deterministic probe, not on
    // the `poll!` below. `busy_timeout = 0` means "do not wait", so contention
    // surfaces as an immediate error rather than a wait.
    {
        let mut probe = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        sqlx::raw_sql("PRAGMA journal_mode = WAL; PRAGMA busy_timeout = 0")
            .execute(&mut probe)
            .await
            .unwrap();
        let contended = sqlx::raw_sql("BEGIN IMMEDIATE").execute(&mut probe).await;
        let error = contended.expect_err("a no-wait writer must fail while A holds the lock");
        let sqlite = error
            .as_database_error()
            .and_then(|db| db.code().map(|code| code.into_owned()));
        assert!(
            matches!(sqlite.as_deref(), Some("5") | Some("6")),
            "the immediate probe must report SQLite locked(6)/busy(5), got {sqlite:?}",
        );
    }

    // Connection B: the real create with production config. Its future cannot pass
    // `BEGIN IMMEDIATE` while A holds the writer.
    let store = SqliteScheduleCreate::open_with_clock(&path, FixedClock(1_000))
        .await
        .unwrap();
    let mut b = pin!(store.create(once_request(), &NoCron));

    // Supporting (not lock-attributing) evidence: B is NOT complete before A
    // commits. The lock is already proven held by the probe above; this only
    // shows the production command has not resolved early.
    for _ in 0..8 {
        assert!(
            matches!(poll!(b.as_mut()), Poll::Pending),
            "create must not complete while the write lock is held elsewhere"
        );
        tokio::task::yield_now().await;
    }

    // Release the lock with the board AT the cap. B now acquires the writer,
    // counts the cap-full board, and must return CapExceeded.
    sqlx::raw_sql("COMMIT").execute(&mut a).await.unwrap();

    let outcome = (&mut b)
        .await
        .expect("blocked create must not fail with a lock error");
    assert!(
        matches!(outcome, ScheduleCreateOutcome::CapExceeded { .. }),
        "a create that waited on the lock and then saw a full board is CapExceeded, never a busy/persistence error"
    );

    // The waiter wrote nothing: only A's cap-filling rows remain.
    assert_eq!(
        count_rows(&path, OWNER).await,
        SCHEDULE_MAX_ACTIVE_PER_AGENT
    );
}

#[tokio::test]
async fn policy_clock_is_sampled_after_the_reserved_writer_is_acquired() {
    use futures_util::poll;
    use std::pin::pin;
    use std::task::Poll;

    let (_dir, path, mut a) = open_db().await;
    sqlx::raw_sql("PRAGMA journal_mode = WAL; BEGIN IMMEDIATE")
        .execute(&mut a)
        .await
        .unwrap();

    let clock_value = Arc::new(AtomicI64::new(1_000));
    let store = SqliteScheduleCreate::open_with_clock(&path, MovingClock(clock_value.clone()))
        .await
        .unwrap();
    let mut request = once_request();
    request.spec = ScheduleSpecInput::Once(OnceWhen::FireAt(31_000));
    let mut pending = pin!(store.create(request, &NoCron));
    for _ in 0..4 {
        assert!(matches!(poll!(pending.as_mut()), Poll::Pending));
        tokio::task::yield_now().await;
    }

    // At route-entry time the fire instant is exactly 30 seconds away. While B
    // waits for the writer, advance the injected clock: after linearization it is
    // only 29 seconds away and must be rejected.
    clock_value.store(2_000, Ordering::SeqCst);
    sqlx::raw_sql("COMMIT").execute(&mut a).await.unwrap();
    let outcome = pending.await.unwrap();
    assert!(matches!(
        outcome,
        ScheduleCreateOutcome::Invalid(
            silverhand_backend_core::conversation_messaging::messaging::schedule::ScheduleCreateGuardError::FireAtTooSoon
        )
    ));
    assert_eq!(count_rows(&path, OWNER).await, 0);
}

/// Scope, positive: the cap counts the OWNER's active rows only. A second owner
/// at the cap does not consume this owner's quota — a create for an empty owner
/// succeeds even while another owner is full.
#[tokio::test]
async fn the_cap_is_scoped_per_owner_and_does_not_share_quota_across_agents() {
    use silverhand_backend_core::conversation_messaging::messaging::schedule::SCHEDULE_MAX_ACTIVE_PER_AGENT;

    let (_dir, path, mut connection) = open_db().await;
    // A DIFFERENT owner, filled to the cap.
    sqlx::query(
        "INSERT INTO agents (
            id, name, runtime, host_id, created_at, updated_at,
            display_name, created_by_user_id, session_scope
         ) VALUES ('agent-other', 'agent-other', 'codex', 'host-1', 1, 1, 'Agent', 'user-1', 'conversation')",
    )
    .execute(&mut connection)
    .await
    .unwrap();
    seed_active_fillers(
        &mut connection,
        "agent-other",
        SCHEDULE_MAX_ACTIVE_PER_AGENT,
    )
    .await;
    drop(connection);

    let store = SqliteScheduleCreate::open_with_clock(&path, FixedClock(1_000))
        .await
        .unwrap();
    let outcome = store.create(once_request(), &NoCron).await.unwrap();
    assert!(
        matches!(outcome, ScheduleCreateOutcome::Created(_)),
        "OWNER's empty board admits a create even while another owner is at the cap"
    );
    assert_eq!(count_rows(&path, OWNER).await, 1);
}

/// Scope, negative: only ACTIVE (`pending`/`firing`) rows count toward the cap.
/// A board filled to the cap with TERMINAL rows (`done`/`cancelled`/`failed`)
/// still admits a new create.
#[tokio::test]
async fn terminal_rows_do_not_count_toward_the_active_cap() {
    use silverhand_backend_core::conversation_messaging::messaging::schedule::SCHEDULE_MAX_ACTIVE_PER_AGENT;

    let (_dir, path, mut connection) = open_db().await;
    // Fill the owner's board to the cap with TERMINAL rows across all three
    // non-active statuses.
    let terminal = ["done", "cancelled", "failed"];
    for index in 0..SCHEDULE_MAX_ACTIVE_PER_AGENT {
        let status = terminal[(index as usize) % terminal.len()];
        sqlx::query(
            "INSERT INTO schedules (
                id, owner_agent_id, target_conversation_id, title, message, spec_json,
                next_fire_at, status, fired_count, max_occurrences, until_at,
                consecutive_failure_count, created_at, updated_at
             ) VALUES (?, ?, ?, 'digest', 'hi', ?, 100, ?, 0, NULL, NULL, 0, 1, 1)",
        )
        .bind(format!("term-{index}"))
        .bind(OWNER)
        .bind(TARGET)
        .bind(r#"{"kind":"once","fire_at":100}"#)
        .bind(status)
        .execute(&mut connection)
        .await
        .unwrap();
    }
    drop(connection);

    let store = SqliteScheduleCreate::open_with_clock(&path, FixedClock(1_000))
        .await
        .unwrap();
    let outcome = store.create(once_request(), &NoCron).await.unwrap();
    assert!(
        matches!(outcome, ScheduleCreateOutcome::Created(_)),
        "terminal rows are not active, so the cap is not reached"
    );
}

impl silverhand_backend_core::conversation_messaging::messaging::schedule::CronNextOccurrence
    for NoCron
{
    fn next_after(
        &self,
        _query: silverhand_backend_core::conversation_messaging::messaging::schedule::CronQuery<'_>,
    ) -> silverhand_backend_core::conversation_messaging::messaging::schedule::CronOutcome {
        silverhand_backend_core::conversation_messaging::messaging::schedule::CronOutcome::NoFutureOccurrence
    }
}
