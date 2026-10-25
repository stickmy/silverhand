//! SQLite adapter for the product schedule CREATE
//! (`POST /internal/agents/{aid}/schedules`).
//!
//! Implements the narrow
//! [`ScheduleCreateStore`](silverhand_backend_core::conversation_messaging::messaging::schedule::ScheduleCreateStore):
//! the ONE atomic command that replays every mutable create gate, enforces the
//! active-count cap, samples the policy clock, canonicalizes, and inserts the new
//! row. It is its own authority — it opens no schema, claims no version, and shares
//! with the fire-path store only the reused row-decode boundary
//! ([`super::project_schedule_view`], [`super::decode_spec`]'s spellings of
//! corruption).
//!
//! The concurrency contract is the whole point of this module. `count active +
//! judge cap + insert` runs inside ONE `BEGIN IMMEDIATE` transaction, so the
//! single writer is reserved before the count is read: two concurrent creates at
//! a cap of N cannot both pass, because the second to obtain the write
//! transaction observes the now-full count and returns
//! [`ScheduleCreateOutcome::CapExceeded`] with no row written. Lock contention is
//! resolved by the pool's `busy_timeout` (the loser WAITS for the writer, then
//! proceeds and observes the cap) — it is never surfaced as a third outcome.

use std::{path::Path, time::Duration};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use silverhand_backend_core::conversation_messaging::messaging::schedule::{
    Clock, CronNextOccurrence, SCHEDULE_MAX_ACTIVE_PER_AGENT, ScheduleCreateOutcome,
    ScheduleCreateRequest, ScheduleCreateStore, ScheduleSpec,
};
use silverhand_backend_core::foundation::persistence::PersistenceFailure;
use sqlx::{Row, SqlitePool};

use super::super::SELECT_AGENT_PARTICIPATES;
use super::product::project_schedule_view;
use super::{ScheduleStoreError, SystemClock, driver};

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// The count, cap check, and insert share ONE `IMMEDIATE` transaction: reserving
/// the single writer up front is what makes the cap race-free. A `DEFERRED`
/// transaction would take its write lock only at the INSERT, AFTER the count was
/// read on a shared snapshot — exactly the check-then-act window this must close.
const CREATE_BEGIN: &str = "BEGIN IMMEDIATE";

/// Count the owner's ACTIVE (`pending`/`firing`) schedules. Read inside the write
/// transaction so the value the cap is judged against is the one the INSERT will
/// extend, never a stale pre-lock snapshot. Mirrors `ScheduleRepo.countActive`.
const COUNT_ACTIVE: &str = "SELECT COUNT(*) AS c FROM schedules \
     WHERE owner_agent_id = ? AND status IN ('pending','firing')";

/// Insert the new row with server-minted id/timestamps and `status='pending'`,
/// projecting the stored row via `RETURNING *`. Mirrors `ScheduleRepo.insert`:
/// `fired_count` 0, the failure/cancel columns NULL, `title`/`target_conversation_id`
/// from the plan.
const INSERT_SCHEDULE: &str = "INSERT INTO schedules \
     (id, owner_agent_id, target_conversation_id, title, message, spec_json, next_fire_at, status, \
      fired_count, last_fired_at, max_occurrences, until_at, failure_reason, cancelled_at, \
      created_at, updated_at) \
     VALUES (?, ?, ?, ?, ?, ?, ?, 'pending', 0, NULL, ?, ?, NULL, NULL, ?, ?) \
     RETURNING *";

/// The target conversation's label inputs PLUS its lifecycle `closed_at`, read in
/// the same transaction so the returned view carries the SAME `target_label` the
/// list/cancel faces render AND the create can re-decide the closed barrier under
/// the write lock. `LEFT`-JOIN semantics by hand: a missing row is target-not-found.
const SELECT_TARGET_CONVERSATION: &str =
    "SELECT title, roster_policy, workspace_id, closed_at FROM conversations WHERE id = ?";
const RESOLVE_MANAGE_SCHEDULES: &str = "SELECT CASE WHEN w.owner_agent_id = ? OR EXISTS ( \
     SELECT 1 FROM workspace_agent_capabilities c WHERE c.workspace_id = w.id \
     AND c.agent_id = ? AND c.capability = 'manage_schedules') THEN 1 ELSE 0 END \
     FROM workspaces w WHERE w.id = ?";

/// Serialize a canonical [`ScheduleSpec`] into the persisted `spec_json` shape,
/// byte-identical to the TypeScript `JSON.stringify(spec)` and to what
/// [`super::decode_spec`] reads back: a `{kind, ...}` object with snake_case
/// fields, `start_at` omitted when absent.
fn spec_json(spec: &ScheduleSpec) -> String {
    match spec {
        ScheduleSpec::Once { fire_at } => {
            format!(r#"{{"kind":"once","fire_at":{fire_at}}}"#)
        }
        ScheduleSpec::Interval { every, start_at } => match start_at {
            Some(start) => format!(
                r#"{{"kind":"interval","every_seconds":{},"start_at":{start}}}"#,
                every.get()
            ),
            None => format!(r#"{{"kind":"interval","every_seconds":{}}}"#, every.get()),
        },
        ScheduleSpec::Cron(cron) => serde_json::json!({
            "kind": "cron",
            "expression": cron.expression(),
            "timezone": cron.timezone(),
        })
        .to_string(),
    }
}

/// A random base64url schedule id, mirroring the TypeScript
/// `randomBytes(12).toString('base64url')` — 12 bytes of OS entropy, unpadded
/// URL-safe base64 (16 chars). Id generation is an infrastructure concern the
/// write authority owns; it is not a domain decision.
const SCHEDULE_ID_BYTES: usize = 12;

fn new_schedule_id() -> Result<String, PersistenceFailure> {
    let mut bytes = [0_u8; SCHEDULE_ID_BYTES];
    getrandom::fill(&mut bytes)
        .map_err(|error| PersistenceFailure::from_source(ScheduleStoreError::Entropy(error)))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// The SQLite schedule create store. Holds only a pool; it is its own sole
/// authority, never fused with the fire-path store or the list/cancel readers.
#[derive(Clone)]
pub struct SqliteScheduleCreate<K = SystemClock> {
    pool: SqlitePool,
    clock: K,
}

impl SqliteScheduleCreate<SystemClock> {
    /// Opens an existing product database. A missing file is a failure, not an
    /// empty database: the schema has an owner, and this writer is not it. WAL +
    /// FULL sync matches the fire-path store so a durable create survives power
    /// loss.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, ScheduleStoreError> {
        Self::open_with_clock(path, SystemClock).await
    }

    /// Construct over an already-open pool with the real system clock.
    pub fn from_pool(pool: SqlitePool) -> Self {
        Self {
            pool,
            clock: SystemClock,
        }
    }
}

impl<K> SqliteScheduleCreate<K> {
    pub async fn open_with_clock(
        path: impl AsRef<Path>,
        clock: K,
    ) -> Result<Self, ScheduleStoreError> {
        let pool = crate::foundation::database::store_pool(path.as_ref(), BUSY_TIMEOUT).await?;
        Ok(Self { pool, clock })
    }

    pub fn from_pool_with_clock(pool: SqlitePool, clock: K) -> Self {
        Self { pool, clock }
    }
}

impl<K: Clock + Send + Sync> ScheduleCreateStore for SqliteScheduleCreate<K> {
    async fn create<C>(
        &self,
        request: ScheduleCreateRequest,
        cron: &C,
    ) -> Result<ScheduleCreateOutcome, PersistenceFailure>
    where
        C: CronNextOccurrence + Sync,
    {
        // Reserve the single writer up front. The count is then read against the
        // state the INSERT will extend, and no concurrent create can slip a row
        // between the count and the insert.
        let mut tx = self.pool.begin_with(CREATE_BEGIN).await.map_err(driver)?;

        let owner = request.owner_agent_id.clone();
        let target_id = request.target_conversation_id.clone();

        // Product-visible stateful gate order, all under the same writer.
        let agent_exists: Option<i64> = sqlx::query_scalar("SELECT 1 FROM agents WHERE id = ?")
            .bind(owner.as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(driver)?;
        if agent_exists.is_none() {
            tx.rollback().await.map_err(driver)?;
            return Ok(ScheduleCreateOutcome::AgentNotFound);
        }

        let target = sqlx::query(SELECT_TARGET_CONVERSATION)
            .bind(target_id.as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(driver)?;
        let Some(target) = target else {
            tx.rollback().await.map_err(driver)?;
            return Ok(ScheduleCreateOutcome::TargetNotFound);
        };
        let target_closed_at: Option<i64> = target.try_get("closed_at").map_err(driver)?;
        if target_closed_at.is_some() {
            tx.rollback().await.map_err(driver)?;
            return Ok(ScheduleCreateOutcome::TargetClosed);
        }
        let target_title: Option<String> = target.try_get("title").map_err(driver)?;
        let target_roster_policy: Option<String> =
            target.try_get("roster_policy").map_err(driver)?;

        let participates: Option<i64> = sqlx::query_scalar(SELECT_AGENT_PARTICIPATES)
            .bind(target_id.as_str())
            .bind(owner.as_str())
            .bind(owner.as_str())
            .bind(owner.as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(driver)?;
        if participates.is_none() {
            tx.rollback().await.map_err(driver)?;
            return Ok(ScheduleCreateOutcome::ActorNotParticipant);
        }

        let workspace_id: Option<String> = target.try_get("workspace_id").map_err(driver)?;
        if let Some(workspace_id) = workspace_id {
            let granted: Option<i64> = sqlx::query_scalar(RESOLVE_MANAGE_SCHEDULES)
                .bind(owner.as_str())
                .bind(owner.as_str())
                .bind(&workspace_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(driver)?;
            if granted.unwrap_or(0) == 0 {
                tx.rollback().await.map_err(driver)?;
                return Ok(ScheduleCreateOutcome::CapabilityDenied);
            }
        }

        let active: i64 = sqlx::query(COUNT_ACTIVE)
            .bind(owner.as_str())
            .fetch_one(&mut *tx)
            .await
            .map_err(driver)?
            .try_get("c")
            .map_err(driver)?;

        if active >= SCHEDULE_MAX_ACTIVE_PER_AGENT {
            // Nothing written; discard the empty transaction. The loser of a
            // concurrent race lands here having ACQUIRED the writer and OBSERVED
            // the full count — the business outcome, not a lock error.
            tx.rollback().await.map_err(driver)?;
            return Ok(ScheduleCreateOutcome::CapExceeded { active });
        }

        let now = self.clock.now();
        let plan = match request.plan(now, cron) {
            Ok(plan) => plan,
            Err(refusal) => {
                tx.rollback().await.map_err(driver)?;
                return Ok(ScheduleCreateOutcome::Invalid(refusal));
            }
        };

        let id = new_schedule_id()?;
        let spec = spec_json(&plan.spec);

        let inserted = sqlx::query(INSERT_SCHEDULE)
            .bind(&id)
            .bind(plan.owner_agent_id.as_str())
            .bind(plan.target_conversation_id.as_str())
            .bind(plan.title.as_str())
            .bind(plan.message.as_str())
            .bind(&spec)
            .bind(plan.next_fire_at)
            .bind(plan.caps.max_occurrences())
            .bind(plan.caps.until_at())
            .bind(now)
            .bind(now)
            .fetch_one(&mut *tx)
            .await
            .map_err(driver)?;

        let view = project_schedule_view(
            &inserted,
            target_title.as_deref(),
            target_roster_policy.as_deref(),
        )?;

        tx.commit().await.map_err(driver)?;
        Ok(ScheduleCreateOutcome::Created(Box::new(view)))
    }
}
