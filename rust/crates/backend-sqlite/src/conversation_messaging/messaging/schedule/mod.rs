//! SQLite adapter for the schedule fire-path ports.
//!
//! This store CONSUMES the product database. It creates no table, runs no
//! migrator, claims no schema version, and refuses to open a database that does
//! not exist: the `schedules` table has an owner, and a second migration
//! authority would be one too many.
//!
//! It implements the two writes a fire performs
//! ([`ScheduleStore`](silverhand_backend_core::conversation_messaging::messaging::schedule::ScheduleStore))
//! plus the boot-recovery reset that closes the at-least-once loop. Everything
//! engine-specific stays here — SQL text, PRAGMAs, driver errors, and the strict
//! per-column decode that turns a row the create contract could not have written
//! into a safe-500 rather than a legal domain fact. The port returns application
//! facts and `PersistenceFailure`, so no driver type reaches the application or
//! the wire.
//!
//! The near-wake timer driver and the periodic sweep that call `process_one` are
//! a separate seam this crate does not own; product create/list/cancel never
//! depend on them.

use std::{
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use silverhand_backend_core::conversation_messaging::messaging::schedule::{
    ClaimedSchedule, Clock, CronSpec, DeliveryMaterial, DueCandidate, FiredCount, IntervalSeconds,
    ScheduleCaps, ScheduleId, ScheduleMessage, ScheduleSpec, ScheduleStore, ScheduleTitle,
    SettlementAction,
};
use silverhand_backend_core::foundation::identity::{AgentId, ConversationId};
use silverhand_backend_core::foundation::persistence::PersistenceFailure;
use sqlx::Row;
use thiserror::Error;

// The product read/cancel adapters are separate authorities that live beside the
// fire-path store and reuse only its row-decode boundary (`decode_spec`,
// `corrupt`, `driver`) — never its claim/settlement CAS.
pub mod cancel;
pub mod create;
mod cron;
pub mod list_read;
mod product;
mod wake;

pub use cancel::SqliteScheduleCancel;
pub use create::SqliteScheduleCreate;
pub use cron::ProductionCronEvaluator;
pub use list_read::SqliteScheduleListRead;
pub use wake::{
    CommittedScheduleWake, ScheduleWakePlacement, ScheduleWakeTargetUnavailable,
    ScheduleWakeWriteError, SqliteScheduleWakeWrite,
};

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// The claim's occurrence flip and its `RETURNING` decode run inside ONE
/// `IMMEDIATE` transaction: reserving the single writer up front (so contention
/// surfaces before the CAS) and, decisively, letting a corrupt-row decode failure
/// roll the `pending → firing` flip back rather than leave a poison row stranded
/// `firing`.
const CLAIM_BEGIN: &str = "BEGIN IMMEDIATE";

/// The occurrence-bound claim: a single `pending → firing` CAS predicated on the
/// candidate's `(id, fired_count, next_fire_at)`, projecting the row it actually
/// won via `RETURNING`. Binding the occurrence coordinates — not just the id — is
/// the ABA guard: a stale candidate for occurrence N matches no row that a
/// concurrent tick already fired and re-armed to N+1 (its `fired_count` moved on),
/// nor a row whose due instant it misremembers. The `status='pending'` conjunct
/// additionally refuses a row already claimed or terminal.
const CLAIM_OCCURRENCE: &str = "UPDATE schedules \
     SET status = 'firing', updated_at = ? \
     WHERE id = ? AND status = 'pending' AND fired_count = ? AND next_fire_at = ? \
     RETURNING id, owner_agent_id, target_conversation_id, title, message, spec_json, \
     next_fire_at, fired_count, max_occurrences, until_at";

/// Repeat success: count the fire, re-arm to the planned next instant, drop back
/// to `pending`, and clear the failure streak. Predicated on `status='firing'` so
/// a cancel that raced in after the claim wins (this matches nothing).
const REARM: &str = "UPDATE schedules \
     SET status = 'pending', fired_count = fired_count + 1, last_fired_at = ?, \
     next_fire_at = ?, consecutive_failure_count = 0, updated_at = ? \
     WHERE id = ? AND status = 'firing'";

/// Terminal success: a delivered `once`, or a repeat whose delivered fire reached
/// a cap. Counts the fire and clears the streak.
const MARK_DONE: &str = "UPDATE schedules \
     SET status = 'done', fired_count = fired_count + 1, last_fired_at = ?, \
     consecutive_failure_count = 0, updated_at = ? \
     WHERE id = ? AND status = 'firing'";

/// One-shot delivery failure: terminal `failed`. Reserved for `once`; a repeat
/// never lands here so a single bad tick cannot kill a long-running plan.
const MARK_FAILED: &str = "UPDATE schedules \
     SET status = 'failed', failure_reason = ?, updated_at = ? \
     WHERE id = ? AND status = 'firing'";

/// Repeat delivery failure that stays live: record the fire-level failure
/// (`last_failure_*`, streak++) and re-arm on the natural cadence tick. The fire
/// consumed no occurrence, so `fired_count` is untouched.
const MARK_FIRE_FAILED: &str = "UPDATE schedules \
     SET status = 'pending', last_failure_reason = ?, last_failure_at = ?, \
     consecutive_failure_count = consecutive_failure_count + 1, next_fire_at = ?, updated_at = ? \
     WHERE id = ? AND status = 'firing'";

/// Repeat delivery FAILED and the next tick overruns the plan window: terminal
/// `done`, but recorded honestly as a failure — the fire did not succeed, so no
/// occurrence is counted and `last_fired_at` is left untouched.
const MARK_PLAN_ENDED_AFTER_FAILURE: &str = "UPDATE schedules \
     SET status = 'done', last_failure_reason = ?, last_failure_at = ?, \
     consecutive_failure_count = consecutive_failure_count + 1, updated_at = ? \
     WHERE id = ? AND status = 'firing'";

/// Crash recovery: every row left `firing` by a prior process lifetime is reset to
/// `pending` so a later tick re-claims and re-fires it. This is the at-least-once
/// contract's other half — a fire abandoned mid-flight (the orchestrator left the
/// row `firing` on a fault) is redelivered rather than lost. It is deliberately
/// unconditional over `status='firing'`: at boot no fire is in flight, so any such
/// row is stale by definition.
const RESET_STALE_FIRING: &str =
    "UPDATE schedules SET status = 'pending', updated_at = ? WHERE status = 'firing'";

/// A real system clock, the default the store stamps its bookkeeping columns with
/// (`updated_at`, and the failure arms' `last_failure_at`). This is the domain
/// [`Clock`] port — the SAME one the orchestrator reads the fire instant from — so
/// a composition root can inject ONE clock for both. It is distinct from the
/// fired-occurrence's `actual_fire_at`, which the orchestrator computed at fire
/// time and hands to [`ScheduleStore::apply_settlement`]; that instant is what
/// `last_fired_at` records. Millisecond epoch time; the impossible pre-epoch or
/// overflow cases clamp rather than panic, because reading the clock is infallible
/// at this boundary.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> i64 {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis())
            .unwrap_or(0);
        i64::try_from(millis).unwrap_or(i64::MAX)
    }
}

/// Startup and driver failures, surfaced only to the composition root that opens
/// the store, plus the decode-corruption reasons that a broken persisted row
/// carries to a safe-500. The port returns `PersistenceFailure`, so none of these
/// spellings reach the application or the wire.
#[derive(Debug, Error)]
pub enum ScheduleStoreError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    /// `spec_json` did not parse as one of the three schedule spec shapes. The
    /// column is the store's own, so an unparseable value is a broken database.
    #[error("stored spec_json is not a valid schedule spec")]
    SpecJson(#[from] serde_json::Error),
    /// A stored column decoded to a value the domain contract rejects — an empty
    /// id, an untrimmed or oversized title, a non-positive interval/cap, a
    /// negative fired_count. The create path could not have written it, so it is
    /// corruption rather than a caller's mistake, reported with the offending
    /// column so an operator can find it.
    #[error("stored schedule column {field} is outside the domain contract")]
    Corrupt { field: &'static str },
    /// The OS entropy source failed while minting a new schedule id. Surfaced to
    /// the caller as a `PersistenceFailure`; the create wrote nothing.
    #[error("secure random source failed")]
    Entropy(#[source] getrandom::Error),
}

fn driver(error: sqlx::Error) -> PersistenceFailure {
    PersistenceFailure::from_source(ScheduleStoreError::Sql(error))
}

fn corrupt(field: &'static str) -> PersistenceFailure {
    PersistenceFailure::from_source(ScheduleStoreError::Corrupt { field })
}

/// The persisted `spec_json` shapes, mirroring `ScheduleSpecSchema` in
/// `packages/shared/src/api/schedule.ts`. Deserialized as its own type and then
/// converted through the domain value objects, so a non-positive interval is
/// rejected at the decode boundary rather than smuggled into the recurrence
/// policy. Unknown fields are tolerated: a stored row that gained a field in a
/// later wire version is not corruption, unlike a value that violates an invariant
/// the arithmetic depends on.
#[derive(serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StoredSpec {
    Once {
        fire_at: i64,
    },
    Interval {
        every_seconds: i64,
        #[serde(default)]
        start_at: Option<i64>,
    },
    Cron {
        expression: String,
        timezone: String,
    },
}

fn decode_spec(spec_json: &str) -> Result<ScheduleSpec, PersistenceFailure> {
    let stored: StoredSpec = serde_json::from_str(spec_json)
        .map_err(|error| PersistenceFailure::from_source(ScheduleStoreError::SpecJson(error)))?;
    Ok(match stored {
        StoredSpec::Once { fire_at } => ScheduleSpec::Once { fire_at },
        StoredSpec::Interval {
            every_seconds,
            start_at,
        } => ScheduleSpec::Interval {
            every: IntervalSeconds::new(every_seconds)
                .map_err(|_| corrupt("spec_json.every_seconds"))?,
            start_at,
        },
        StoredSpec::Cron {
            expression,
            timezone,
        } => ScheduleSpec::Cron(CronSpec::new(expression, timezone)),
    })
}

/// The SQLite schedule store.
///
/// Its clock (`K`) stamps the bookkeeping timestamps; the fire instant that a
/// delivered occurrence records travels in [`ScheduleStore::apply_settlement`]'s
/// `actual_fire_at`, not from here.
#[derive(Clone)]
pub struct SqliteScheduleStore<K = SystemClock> {
    pool: crate::foundation::database::SqliteDatabase,
    clock: K,
}

impl SqliteScheduleStore<SystemClock> {
    pub fn from_pool(pool: impl Into<crate::foundation::database::SqliteDatabase>) -> Self {
        let pool = pool.into();
        Self {
            pool,
            clock: SystemClock,
        }
    }

    /// Opens an existing product database with the real system clock.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, ScheduleStoreError> {
        Self::open_with_clock(path, SystemClock).await
    }
}

impl<K> SqliteScheduleStore<K> {
    /// Opens an existing product database with an injected clock. A missing file is
    /// a failure rather than an empty database: the schema has an owner, and this
    /// store is not it.
    pub async fn open_with_clock(
        path: impl AsRef<Path>,
        clock: K,
    ) -> Result<Self, ScheduleStoreError> {
        let pool = crate::foundation::database::store_pool(path.as_ref(), BUSY_TIMEOUT).await?;

        Ok(Self { pool, clock })
    }
}

impl<K: Clock + Send + Sync> SqliteScheduleStore<K> {
    /// Returns a bounded occurrence-identity snapshot for every pending row due
    /// at `now`.  The subsequent claim CAS re-binds all three coordinates, so a
    /// concurrent cancel/rearm turns a stale candidate into a clean claim loss.
    pub async fn due_candidates(
        &self,
        now: i64,
        limit: i64,
    ) -> Result<Vec<DueCandidate>, PersistenceFailure> {
        let rows = sqlx::query(
            "SELECT id,fired_count,next_fire_at FROM schedules \
             WHERE status='pending' AND next_fire_at<=? \
             ORDER BY next_fire_at ASC,id ASC LIMIT ?",
        )
        .bind(now)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(driver)?;
        rows.into_iter()
            .map(|row| {
                Ok(DueCandidate {
                    id: ScheduleId::parse(row.try_get::<String, _>("id").map_err(driver)?)
                        .map_err(|_| corrupt("schedules.id"))?,
                    expected_fired_count: FiredCount::new(
                        row.try_get("fired_count").map_err(driver)?,
                    )
                    .map_err(|_| corrupt("schedules.fired_count"))?,
                    expected_next_fire_at: row.try_get("next_fire_at").map_err(driver)?,
                })
            })
            .collect()
    }

    /// Earliest pending instant, used only to tighten the next wake-up.  The
    /// periodic sweep remains the durable backstop and the claim CAS remains the
    /// occurrence authority.
    pub async fn next_pending_fire_at(&self) -> Result<Option<i64>, PersistenceFailure> {
        sqlx::query_scalar("SELECT MIN(next_fire_at) FROM schedules WHERE status='pending'")
            .fetch_one(&self.pool)
            .await
            .map_err(driver)
    }

    /// Resets every row a prior process left `firing` back to `pending`, returning
    /// how many were recovered. Called once at boot, before the sweep begins — see
    /// [`RESET_STALE_FIRING`]. Not part of the fire-path [`ScheduleStore`] trait:
    /// its consumer is the recovery step of the driver seam, which defines its own
    /// port when it lands.
    pub async fn reset_stale_firing(&self) -> Result<u64, PersistenceFailure> {
        let affected = sqlx::query(RESET_STALE_FIRING)
            .bind(self.clock.now())
            .execute(&self.pool)
            .await
            .map_err(driver)?
            .rows_affected();
        Ok(affected)
    }

    /// Projects the authoritative [`ClaimedSchedule`] from the row the claim CAS
    /// won. Every column is decoded through its domain value object, so a row the
    /// create contract could not have written (empty id/title, untrimmed or
    /// oversized title, non-positive interval/cap, negative fired_count, empty
    /// target) fails HERE as a safe-500 rather than firing on a corrupt fact.
    fn project_claimed(
        row: &sqlx::sqlite::SqliteRow,
    ) -> Result<ClaimedSchedule, PersistenceFailure> {
        let id: String = row.try_get("id").map_err(driver)?;
        let owner_agent_id: String = row.try_get("owner_agent_id").map_err(driver)?;
        let target_conversation_id: String =
            row.try_get("target_conversation_id").map_err(driver)?;
        let title: String = row.try_get("title").map_err(driver)?;
        let message: String = row.try_get("message").map_err(driver)?;
        let spec_json: String = row.try_get("spec_json").map_err(driver)?;
        let next_fire_at: i64 = row.try_get("next_fire_at").map_err(driver)?;
        let fired_count: i64 = row.try_get("fired_count").map_err(driver)?;
        let max_occurrences: Option<i64> = row.try_get("max_occurrences").map_err(driver)?;
        let until_at: Option<i64> = row.try_get("until_at").map_err(driver)?;

        Ok(ClaimedSchedule {
            id: ScheduleId::parse(id).map_err(|_| corrupt("schedules.id"))?,
            spec: decode_spec(&spec_json)?,
            caps: ScheduleCaps::new(max_occurrences, until_at)
                .map_err(|_| corrupt("schedules.caps"))?,
            fired_count: FiredCount::new(fired_count)
                .map_err(|_| corrupt("schedules.fired_count"))?,
            scheduled_fire_at: next_fire_at,
            delivery: DeliveryMaterial {
                owner_agent_id: AgentId::parse(owner_agent_id)
                    .map_err(|_| corrupt("schedules.owner_agent_id"))?,
                target_conversation_id: ConversationId::parse(target_conversation_id)
                    .map_err(|_| corrupt("schedules.target_conversation_id"))?,
                title: ScheduleTitle::parse(title).map_err(|_| corrupt("schedules.title"))?,
                message: ScheduleMessage::parse(message)
                    .map_err(|_| corrupt("schedules.message"))?,
            },
        })
    }
}

impl<K: Clock + Send + Sync> ScheduleStore for SqliteScheduleStore<K> {
    async fn claim(
        &self,
        candidate: &DueCandidate,
    ) -> Result<Option<ClaimedSchedule>, PersistenceFailure> {
        // The occurrence flip and its RETURNING decode share ONE transaction, so
        // the `pending → firing` write only becomes durable when the row projects
        // to a valid ClaimedSchedule. A row a create contract could not have
        // written (title/spec/identity outside its domain contract) rolls the flip
        // back: the safe-500 the caller receives leaves the row in its ORIGINAL
        // pending occurrence — coordinates and updated_at untouched, re-claimable
        // once the data is repaired — instead of stranding it `firing` until a
        // process restart runs boot recovery. The CAS stays INSIDE the tx (never a
        // pre-read), so the occurrence-ABA guard and writer reservation are intact.
        let mut tx = self.pool.begin_with(CLAIM_BEGIN).await.map_err(driver)?;
        let row = sqlx::query(CLAIM_OCCURRENCE)
            .bind(self.clock.now())
            .bind(candidate.id.as_str())
            .bind(candidate.expected_fired_count.get())
            .bind(candidate.expected_next_fire_at)
            .fetch_optional(&mut *tx)
            .await
            .map_err(driver)?;

        let Some(row) = row else {
            // The CAS matched no row — gone, no longer pending, or re-armed to a
            // newer occurrence than the candidate named. Nothing changed; discard
            // the empty transaction and report the clean claim-lost.
            tx.rollback().await.map_err(driver)?;
            return Ok(None);
        };

        match Self::project_claimed(&row) {
            Ok(claimed) => {
                tx.commit().await.map_err(driver)?;
                Ok(Some(claimed))
            }
            Err(corruption) => {
                // Undo the `firing` flip so the corrupt row stays pending.
                tx.rollback().await.map_err(driver)?;
                Err(corruption)
            }
        }
    }

    async fn apply_settlement(
        &self,
        id: &ScheduleId,
        actual_fire_at: i64,
        action: &SettlementAction,
    ) -> Result<bool, PersistenceFailure> {
        let now = self.clock.now();
        let affected = match action {
            SettlementAction::MarkDone => sqlx::query(MARK_DONE)
                .bind(actual_fire_at)
                .bind(now)
                .bind(id.as_str())
                .execute(&self.pool)
                .await
                .map_err(driver)?
                .rows_affected(),
            SettlementAction::Rearm { next_fire_at } => sqlx::query(REARM)
                .bind(actual_fire_at)
                .bind(next_fire_at)
                .bind(now)
                .bind(id.as_str())
                .execute(&self.pool)
                .await
                .map_err(driver)?
                .rows_affected(),
            SettlementAction::MarkFailed { reason } => sqlx::query(MARK_FAILED)
                .bind(reason)
                .bind(now)
                .bind(id.as_str())
                .execute(&self.pool)
                .await
                .map_err(driver)?
                .rows_affected(),
            SettlementAction::MarkFireFailed {
                reason,
                next_fire_at,
            } => sqlx::query(MARK_FIRE_FAILED)
                .bind(reason)
                .bind(now)
                .bind(next_fire_at)
                .bind(now)
                .bind(id.as_str())
                .execute(&self.pool)
                .await
                .map_err(driver)?
                .rows_affected(),
            SettlementAction::MarkPlanEndedAfterFailure { reason } => {
                sqlx::query(MARK_PLAN_ENDED_AFTER_FAILURE)
                    .bind(reason)
                    .bind(now)
                    .bind(now)
                    .bind(id.as_str())
                    .execute(&self.pool)
                    .await
                    .map_err(driver)?
                    .rows_affected()
            }
        };
        // At most one row carries a given id, so a nonzero count means this
        // caller's `firing` row settled; zero means a racing terminal write (a
        // cancel) got there first and the settlement is a benign no-op.
        Ok(affected > 0)
    }
}
