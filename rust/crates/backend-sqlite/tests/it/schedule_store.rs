//! Real-SQLite gates for the schedule fire-path store.
//!
//! Every test drives the actual adapter against a database created from the
//! checked-in current-schema snapshot — the same `schedules` DDL the product
//! ships — so the CAS predicates, the `RETURNING` projection, and the strict
//! decode are exercised over real rows rather than a fake. The store consumes
//! that database; it creates no table and runs no migration.

use silverhand_backend_core::conversation_messaging::messaging::schedule::{
    Clock, CronSpec, DueCandidate, FiredCount, ScheduleCaps, ScheduleId, ScheduleSpec,
    ScheduleStore, SettlementAction,
};
use silverhand_backend_sqlite::conversation_messaging::messaging::schedule::SqliteScheduleStore;
use sqlx::{ConnectOptions, Connection, Row, SqliteConnection, sqlite::SqliteConnectOptions};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

use crate::current_schema;

/// The clock every gate pins so `updated_at` / `last_failure_at` are exact.
const NOW: i64 = 9_000_000;
const AGENT: &str = "agent-sched";
/// `target_conversation_id` has no FK, so a non-empty string is a legal target
/// without seeding a conversation row.
const TARGET: &str = "conversation-target";

const SPEC_ONCE: &str = r#"{"kind":"once","fire_at":100000}"#;
const SPEC_INTERVAL_60: &str = r#"{"kind":"interval","every_seconds":60}"#;
const SPEC_INTERVAL_START: &str = r#"{"kind":"interval","every_seconds":60,"start_at":50}"#;
const SPEC_CRON: &str = r#"{"kind":"cron","expression":"*/5 * * * *","timezone":"UTC"}"#;
/// Forward-compatible: a field the current wire does not emit is tolerated, not
/// corruption — only invariant violations are.
const SPEC_INTERVAL_UNKNOWN_FIELD: &str =
    r#"{"kind":"interval","every_seconds":60,"future_field":true}"#;
/// Corruption: a non-positive cadence the arithmetic depends on being `> 0`.
const SPEC_INTERVAL_ZERO: &str = r#"{"kind":"interval","every_seconds":0}"#;

/// A test clock; `SqliteScheduleStore` reads the same domain [`Clock`] port the
/// orchestrator does, so one injected clock stamps everything.
struct FixedClock(i64);

impl Clock for FixedClock {
    fn now(&self) -> i64 {
        self.0
    }
}

/// A row to seed. Fields default to a due, pending, one-shot schedule; each gate
/// overrides only what it exercises.
#[derive(Clone)]
struct SeedSchedule {
    id: &'static str,
    status: &'static str,
    fired_count: i64,
    next_fire_at: i64,
    spec_json: &'static str,
    max_occurrences: Option<i64>,
    until_at: Option<i64>,
    title: &'static str,
    message: &'static str,
    consecutive_failure_count: i64,
    last_fired_at: Option<i64>,
}

impl Default for SeedSchedule {
    fn default() -> Self {
        Self {
            id: "sched-1",
            status: "pending",
            fired_count: 0,
            next_fire_at: 100_000,
            spec_json: SPEC_ONCE,
            max_occurrences: None,
            until_at: None,
            title: "daily digest",
            message: "your 9am summary",
            consecutive_failure_count: 0,
            last_fired_at: None,
        }
    }
}

/// The row state a settlement assertion reads back.
#[derive(Debug)]
struct RowSnapshot {
    status: String,
    fired_count: i64,
    next_fire_at: i64,
    last_fired_at: Option<i64>,
    consecutive_failure_count: i64,
    failure_reason: Option<String>,
    last_failure_reason: Option<String>,
    last_failure_at: Option<i64>,
    updated_at: i64,
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
    .bind(AGENT)
    .execute(&mut *connection)
    .await
    .unwrap();
}

async fn insert_schedule(connection: &mut SqliteConnection, seed: &SeedSchedule) {
    sqlx::query(
        "INSERT INTO schedules (
            id, owner_agent_id, target_conversation_id, title, message, spec_json,
            next_fire_at, status, fired_count, last_fired_at, max_occurrences, until_at,
            consecutive_failure_count, created_at, updated_at
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1, 1)",
    )
    .bind(seed.id)
    .bind(AGENT)
    .bind(TARGET)
    .bind(seed.title)
    .bind(seed.message)
    .bind(seed.spec_json)
    .bind(seed.next_fire_at)
    .bind(seed.status)
    .bind(seed.fired_count)
    .bind(seed.last_fired_at)
    .bind(seed.max_occurrences)
    .bind(seed.until_at)
    .bind(seed.consecutive_failure_count)
    .execute(&mut *connection)
    .await
    .unwrap();
}

/// Builds a database from the current-schema snapshot, seeds the owner agent and
/// the given schedule rows, then opens the store on a fixed clock. The `TempDir`
/// is returned so the caller keeps it alive for the test's duration.
async fn store_with(seeds: &[SeedSchedule]) -> (TempDir, PathBuf, SqliteScheduleStore<FixedClock>) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("schedules.sqlite");
    let mut connection = current_schema::create_current_schema(&path).await;
    seed_owner(&mut connection).await;
    for seed in seeds {
        insert_schedule(&mut connection, seed).await;
    }
    connection.close().await.unwrap();
    let store = SqliteScheduleStore::open_with_clock(&path, FixedClock(NOW))
        .await
        .unwrap();
    (directory, path, store)
}

async fn snapshot(path: &Path, id: &str) -> RowSnapshot {
    let mut connection = SqliteConnectOptions::new()
        .filename(path)
        .connect()
        .await
        .unwrap();
    let row = sqlx::query(
        "SELECT status, fired_count, next_fire_at, last_fired_at, consecutive_failure_count,
                failure_reason, last_failure_reason, last_failure_at, updated_at
           FROM schedules WHERE id = ?",
    )
    .bind(id)
    .fetch_one(&mut connection)
    .await
    .unwrap();
    let snapshot = RowSnapshot {
        status: row.get("status"),
        fired_count: row.get("fired_count"),
        next_fire_at: row.get("next_fire_at"),
        last_fired_at: row.get("last_fired_at"),
        consecutive_failure_count: row.get("consecutive_failure_count"),
        failure_reason: row.get("failure_reason"),
        last_failure_reason: row.get("last_failure_reason"),
        last_failure_at: row.get("last_failure_at"),
        updated_at: row.get("updated_at"),
    };
    connection.close().await.unwrap();
    snapshot
}

fn candidate(id: &str, fired_count: i64, next_fire_at: i64) -> DueCandidate {
    DueCandidate {
        id: ScheduleId::parse(id).unwrap(),
        expected_fired_count: FiredCount::new(fired_count).unwrap(),
        expected_next_fire_at: next_fire_at,
    }
}

// --- claim: projection ------------------------------------------------------

#[tokio::test]
async fn claim_projects_the_full_authoritative_occurrence() {
    let (_dir, path, store) = store_with(&[SeedSchedule {
        spec_json: SPEC_INTERVAL_60,
        fired_count: 2,
        next_fire_at: 100_000,
        max_occurrences: Some(5),
        until_at: Some(500_000),
        title: "weekly report",
        message: "  the numbers  ", // message is NOT trimmed by the wire
        ..Default::default()
    }])
    .await;

    let claimed = store
        .claim(&candidate("sched-1", 2, 100_000))
        .await
        .unwrap()
        .expect("the exact occurrence must be claimable");

    assert_eq!(claimed.id.as_str(), "sched-1");
    assert_eq!(
        claimed.spec,
        ScheduleSpec::Interval {
            every: silverhand_backend_core::conversation_messaging::messaging::schedule::IntervalSeconds::new(60).unwrap(),
            start_at: None,
        }
    );
    assert_eq!(
        claimed.caps,
        ScheduleCaps::new(Some(5), Some(500_000)).unwrap()
    );
    assert_eq!(claimed.fired_count, FiredCount::new(2).unwrap());
    // The row's stored next_fire_at is the tick this fire is FOR.
    assert_eq!(claimed.scheduled_fire_at, 100_000);
    assert_eq!(claimed.delivery.owner_agent_id.as_str(), AGENT);
    assert_eq!(claimed.delivery.target_conversation_id.as_str(), TARGET);
    assert_eq!(claimed.delivery.title.as_str(), "weekly report");
    assert_eq!(claimed.delivery.message.as_str(), "  the numbers  ");

    // The winning claim flipped the row to `firing` and stamped the clock.
    let row = snapshot(&path, "sched-1").await;
    assert_eq!(row.status, "firing");
    assert_eq!(row.updated_at, NOW);
    // Claiming counts nothing: fired_count only moves on settlement.
    assert_eq!(row.fired_count, 2);
}

#[tokio::test]
async fn claim_decodes_each_spec_shape() {
    for (id, spec_json, expected) in [
        ("once", SPEC_ONCE, ScheduleSpec::Once { fire_at: 100_000 }),
        (
            "cron",
            SPEC_CRON,
            ScheduleSpec::Cron(CronSpec::new("*/5 * * * *".into(), "UTC".into())),
        ),
        (
            "unknown-field",
            SPEC_INTERVAL_UNKNOWN_FIELD,
            ScheduleSpec::Interval {
                every: silverhand_backend_core::conversation_messaging::messaging::schedule::IntervalSeconds::new(60)
                    .unwrap(),
                start_at: None,
            },
        ),
        (
            "start-at",
            SPEC_INTERVAL_START,
            ScheduleSpec::Interval {
                every: silverhand_backend_core::conversation_messaging::messaging::schedule::IntervalSeconds::new(60)
                    .unwrap(),
                start_at: Some(50),
            },
        ),
    ] {
        let (_dir, _path, store) = store_with(&[SeedSchedule {
            id,
            spec_json,
            next_fire_at: 100_000,
            ..Default::default()
        }])
        .await;
        let claimed = store
            .claim(&candidate(id, 0, 100_000))
            .await
            .unwrap()
            .expect("spec must decode and claim");
        assert_eq!(claimed.spec, expected, "spec {id} decoded wrong");
    }
}

// --- claim: occurrence-bound ABA falsifier ----------------------------------

#[tokio::test]
async fn claim_binds_the_exact_occurrence_against_two_stale_coordinates() {
    // A row pending at occurrence (fired_count=2, next_fire_at=100_000). Two
    // distinct stale candidates must each fail to win it, proving BOTH occurrence
    // coordinates are bound — not just the id, and not just one of the pair.
    let (_dir, path, store) = store_with(&[SeedSchedule {
        spec_json: SPEC_INTERVAL_60,
        fired_count: 2,
        next_fire_at: 100_000,
        ..Default::default()
    }])
    .await;

    // Stale in the count coordinate (an earlier occurrence of the same instant).
    assert!(
        store
            .claim(&candidate("sched-1", 1, 100_000))
            .await
            .unwrap()
            .is_none(),
        "a stale fired_count must not win the current occurrence"
    );
    // Stale in the instant coordinate (a misremembered due time).
    assert!(
        store
            .claim(&candidate("sched-1", 2, 50_000))
            .await
            .unwrap()
            .is_none(),
        "a stale next_fire_at must not win the current occurrence"
    );
    // Both misses left the row untouched — still pending, never flipped to firing.
    assert_eq!(snapshot(&path, "sched-1").await.status, "pending");

    // The exact occurrence wins.
    assert!(
        store
            .claim(&candidate("sched-1", 2, 100_000))
            .await
            .unwrap()
            .is_some(),
        "the exact occurrence must be claimable"
    );
    assert_eq!(snapshot(&path, "sched-1").await.status, "firing");
}

#[tokio::test]
async fn claim_refuses_a_non_pending_row_even_on_exact_coordinates() {
    // The occurrence coordinates match, but the row is already firing: a second
    // claimant of the same tick must lose.
    let (_dir, _path, store) = store_with(&[SeedSchedule {
        status: "firing",
        fired_count: 0,
        next_fire_at: 100_000,
        ..Default::default()
    }])
    .await;
    assert!(
        store
            .claim(&candidate("sched-1", 0, 100_000))
            .await
            .unwrap()
            .is_none(),
        "a firing row must not be re-claimable"
    );
}

// --- settlement: each arm ---------------------------------------------------

/// Claim the row (so it is `firing`) and then settle it, returning the row state.
async fn claim_then_settle(
    seed: SeedSchedule,
    action: SettlementAction,
    actual_fire_at: i64,
) -> (bool, RowSnapshot) {
    let id = seed.id;
    let claim = candidate(id, seed.fired_count, seed.next_fire_at);
    let (_dir, path, store) = store_with(&[seed]).await;
    let schedule_id = claim.id.clone();
    store.claim(&claim).await.unwrap().expect("must claim");
    let applied = store
        .apply_settlement(&schedule_id, actual_fire_at, &action)
        .await
        .unwrap();
    (applied, snapshot(&path, id).await)
}

#[tokio::test]
async fn mark_done_counts_the_fire_and_terminates() {
    let (applied, row) = claim_then_settle(
        SeedSchedule {
            spec_json: SPEC_ONCE,
            fired_count: 0,
            next_fire_at: 100_000,
            consecutive_failure_count: 3,
            ..Default::default()
        },
        SettlementAction::MarkDone,
        123_456,
    )
    .await;
    assert!(applied);
    assert_eq!(row.status, "done");
    assert_eq!(row.fired_count, 1);
    assert_eq!(row.last_fired_at, Some(123_456));
    assert_eq!(row.consecutive_failure_count, 0);
    assert_eq!(row.updated_at, NOW);
}

#[tokio::test]
async fn rearm_advances_and_returns_to_pending() {
    let (applied, row) = claim_then_settle(
        SeedSchedule {
            spec_json: SPEC_INTERVAL_60,
            fired_count: 2,
            next_fire_at: 100_000,
            consecutive_failure_count: 4,
            ..Default::default()
        },
        SettlementAction::Rearm {
            next_fire_at: 160_000,
        },
        100_000,
    )
    .await;
    assert!(applied);
    assert_eq!(row.status, "pending");
    assert_eq!(row.fired_count, 3);
    assert_eq!(row.next_fire_at, 160_000);
    assert_eq!(row.last_fired_at, Some(100_000));
    assert_eq!(row.consecutive_failure_count, 0);
}

#[tokio::test]
async fn mark_failed_records_a_terminal_once_failure() {
    let (applied, row) = claim_then_settle(
        SeedSchedule {
            spec_json: SPEC_ONCE,
            fired_count: 0,
            next_fire_at: 100_000,
            ..Default::default()
        },
        SettlementAction::MarkFailed {
            reason: "transport down".into(),
        },
        100_000,
    )
    .await;
    assert!(applied);
    assert_eq!(row.status, "failed");
    assert_eq!(row.failure_reason.as_deref(), Some("transport down"));
    // A once failure counts no occurrence.
    assert_eq!(row.fired_count, 0);
    assert_eq!(row.last_fired_at, None);
}

#[tokio::test]
async fn mark_fire_failed_records_the_failure_and_stays_live() {
    let (applied, row) = claim_then_settle(
        SeedSchedule {
            spec_json: SPEC_INTERVAL_60,
            fired_count: 2,
            next_fire_at: 100_000,
            consecutive_failure_count: 1,
            ..Default::default()
        },
        SettlementAction::MarkFireFailed {
            reason: "boom".into(),
            next_fire_at: 160_000,
        },
        100_000,
    )
    .await;
    assert!(applied);
    assert_eq!(row.status, "pending");
    assert_eq!(row.next_fire_at, 160_000);
    assert_eq!(row.last_failure_reason.as_deref(), Some("boom"));
    assert_eq!(row.last_failure_at, Some(NOW));
    assert_eq!(row.consecutive_failure_count, 2);
    // The failed fire consumed no occurrence and set no success timestamp.
    assert_eq!(row.fired_count, 2);
    assert_eq!(row.last_fired_at, None);
}

#[tokio::test]
async fn mark_plan_ended_after_failure_terminates_but_records_the_failure() {
    let (applied, row) = claim_then_settle(
        SeedSchedule {
            spec_json: SPEC_INTERVAL_60,
            fired_count: 2,
            next_fire_at: 100_000,
            consecutive_failure_count: 1,
            ..Default::default()
        },
        SettlementAction::MarkPlanEndedAfterFailure {
            reason: "boom".into(),
        },
        100_000,
    )
    .await;
    assert!(applied);
    assert_eq!(row.status, "done");
    assert_eq!(row.last_failure_reason.as_deref(), Some("boom"));
    assert_eq!(row.last_failure_at, Some(NOW));
    assert_eq!(row.consecutive_failure_count, 2);
    // Terminal by failure: no occurrence counted, no success timestamp.
    assert_eq!(row.fired_count, 2);
    assert_eq!(row.last_fired_at, None);
}

// --- settlement: cancel-race no-op ------------------------------------------

#[tokio::test]
async fn a_cancel_that_races_after_the_claim_makes_settlement_a_no_op() {
    // Claim (row → firing), then a cancel flips it to cancelled before the
    // settlement CAS lands. The `status='firing'` predicate matches nothing, so
    // the settlement is a benign no-op: it returns false and does NOT overwrite
    // the terminal cancel.
    let (_dir, path, store) = store_with(&[SeedSchedule {
        spec_json: SPEC_INTERVAL_60,
        fired_count: 2,
        next_fire_at: 100_000,
        ..Default::default()
    }])
    .await;
    let id = candidate("sched-1", 2, 100_000);
    store.claim(&id).await.unwrap().expect("must claim");

    // A racing cancel wins the row.
    let mut connection = SqliteConnectOptions::new()
        .filename(&path)
        .connect()
        .await
        .unwrap();
    sqlx::query(
        "UPDATE schedules SET status = 'cancelled', cancelled_at = 42, updated_at = 42
         WHERE id = 'sched-1'",
    )
    .execute(&mut connection)
    .await
    .unwrap();
    connection.close().await.unwrap();

    let applied = store
        .apply_settlement(
            &id.id,
            100_000,
            &SettlementAction::Rearm {
                next_fire_at: 160_000,
            },
        )
        .await
        .unwrap();

    assert!(
        !applied,
        "settlement must be a no-op once cancel won the row"
    );
    let row = snapshot(&path, "sched-1").await;
    assert_eq!(row.status, "cancelled", "the terminal cancel must survive");
    assert_eq!(row.next_fire_at, 100_000, "the no-op re-armed nothing");
    assert_eq!(row.fired_count, 2, "the no-op counted no fire");
}

// --- boot recovery: stale-firing reset + redelivery -------------------------

#[tokio::test]
async fn reset_stale_firing_re_enables_redelivery_and_leaves_pending_rows_alone() {
    // A row a prior process left `firing` (crash mid-fire) plus a healthy pending
    // row. Recovery resets only the stale one, and the reset row becomes claimable
    // again — the at-least-once redelivery path.
    let (_dir, path, store) = store_with(&[
        SeedSchedule {
            id: "stale",
            status: "firing",
            spec_json: SPEC_INTERVAL_60,
            fired_count: 2,
            next_fire_at: 100_000,
            ..Default::default()
        },
        SeedSchedule {
            id: "healthy",
            status: "pending",
            spec_json: SPEC_INTERVAL_60,
            fired_count: 0,
            next_fire_at: 200_000,
            ..Default::default()
        },
    ])
    .await;

    let reset = store.reset_stale_firing().await.unwrap();
    assert_eq!(reset, 1, "only the stale firing row is recovered");

    let stale = snapshot(&path, "stale").await;
    assert_eq!(stale.status, "pending");
    assert_eq!(stale.updated_at, NOW);
    // The reset preserved the occurrence coordinates, so the SAME occurrence
    // redelivers rather than a fabricated one.
    assert_eq!(stale.fired_count, 2);
    assert_eq!(stale.next_fire_at, 100_000);

    // The healthy row was not touched.
    assert_eq!(snapshot(&path, "healthy").await.updated_at, 1);

    // Redelivery: the recovered occurrence is claimable at its own coordinates.
    assert!(
        store
            .claim(&candidate("stale", 2, 100_000))
            .await
            .unwrap()
            .is_some(),
        "the recovered occurrence must be re-claimable"
    );
}

#[tokio::test]
async fn reset_stale_firing_recovers_nothing_when_no_row_is_firing() {
    let (_dir, _path, store) = store_with(&[SeedSchedule::default()]).await;
    assert_eq!(store.reset_stale_firing().await.unwrap(), 0);
}

// --- decode-corruption falsifiers -------------------------------------------

/// A row the create contract could not have written must fail the claim as a
/// persistence failure, never surface as a legal claimed occurrence — AND, because
/// the flip and its decode share one transaction, the failed claim must leave the
/// row exactly as it found it: still `pending`, on the same occurrence coordinates,
/// with `updated_at` (seeded to 1) untouched. That is what makes it re-claimable
/// rather than a poison row stranded `firing` until a restart runs boot recovery.
/// `repair` is an `UPDATE ... WHERE id = ?` that fixes the one corrupt column; once
/// applied, the SAME occurrence claims cleanly.
async fn assert_claim_rejects_corrupt(seed: SeedSchedule, repair: &'static str) {
    let id = seed.id;
    let fired = seed.fired_count;
    let next = seed.next_fire_at;
    let (_dir, path, store) = store_with(&[seed]).await;

    let error = store.claim(&candidate(id, fired, next)).await;
    assert!(
        error.is_err(),
        "a corrupt row must fail the claim, not project a bad occurrence"
    );

    // The tx-scoped claim rolled the `pending → firing` flip back on decode
    // failure: the row is byte-for-byte where it started. `updated_at == 1` (the
    // seed value, never the clock's NOW) proves the flip's write did not survive.
    let after = snapshot(&path, id).await;
    assert_eq!(after.status, "pending", "the corrupt row must stay pending");
    assert_eq!(after.fired_count, fired, "the occurrence must be untouched");
    assert_eq!(after.next_fire_at, next, "the occurrence must be untouched");
    assert_eq!(
        after.updated_at, 1,
        "a rolled-back flip must not stamp updated_at"
    );

    // Repair the one bad column; the same occurrence now claims cleanly, so the
    // failure was a recoverable safe-500 and not a lost schedule.
    let mut connection = SqliteConnectOptions::new()
        .filename(&path)
        .connect()
        .await
        .unwrap();
    sqlx::query(repair)
        .bind(id)
        .execute(&mut connection)
        .await
        .unwrap();
    connection.close().await.unwrap();

    let reclaimed = store
        .claim(&candidate(id, fired, next))
        .await
        .expect("the repaired row must claim without error");
    assert!(
        reclaimed.is_some(),
        "the same occurrence must be claimable once the data is repaired"
    );
    assert_eq!(
        snapshot(&path, id).await.status,
        "firing",
        "the repaired claim flips the row to firing"
    );
}

#[tokio::test]
async fn claim_rejects_an_empty_title_as_corruption() {
    assert_claim_rejects_corrupt(
        SeedSchedule {
            title: "",
            ..Default::default()
        },
        "UPDATE schedules SET title = 'daily digest' WHERE id = ?",
    )
    .await;
}

#[tokio::test]
async fn claim_rejects_an_untrimmed_title_as_corruption() {
    // The wire persists trimmed titles; surrounding whitespace is a row create
    // could not have produced.
    assert_claim_rejects_corrupt(
        SeedSchedule {
            title: "  padded  ",
            ..Default::default()
        },
        "UPDATE schedules SET title = 'padded' WHERE id = ?",
    )
    .await;
}

#[tokio::test]
async fn claim_rejects_a_non_positive_interval_as_corruption() {
    assert_claim_rejects_corrupt(
        SeedSchedule {
            spec_json: SPEC_INTERVAL_ZERO,
            ..Default::default()
        },
        r#"UPDATE schedules SET spec_json = '{"kind":"interval","every_seconds":60}' WHERE id = ?"#,
    )
    .await;
}

// A negative `fired_count` is corruption for the recurrence arithmetic, but it is
// UNREACHABLE through `claim`: the CAS predicate binds `fired_count = ?` to the
// candidate's `FiredCount`, which has no negative representation, so the
// projection never decodes a negative value. The projection still routes it
// through `FiredCount::new` (its only constructor) so a future read model that
// reuses the decode inherits the same safe-500 boundary; pricing a dedicated
// gate for a path the port cannot reach is not worth a contrived SQL fixture.
