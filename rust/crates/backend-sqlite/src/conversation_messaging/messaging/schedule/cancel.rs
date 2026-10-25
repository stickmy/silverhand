//! SQLite adapter for the product schedule CANCEL
//! (`POST /api/agents/{id}/schedules/{scheduleId}/cancel`).
//!
//! ONE `BEGIN IMMEDIATE` transaction carries the whole unit of work: load the
//! schedule (owner, target, status), bind the caller against the schedule's OWN
//! owner FIRST (the pure [`bind_owner`] rule) and short-circuit `NotOwner`
//! before reading anything else, then — only for an owned schedule — resolve the
//! target conversation's workspace and the owner's `manage_schedules` grant, let
//! [`evaluate_capability`] decide, and — only on `Proceed` — run the
//! `pending|firing → cancelled` CAS and project the post-image. SQL reads facts
//! only; the capability judgement is the core rule's. The route's edit gate ran
//! first, but this re-binds the schedule's OWN owner (a gate pass is not carried
//! as proof) — and because owner-bind precedes the target/capability read, a
//! fault on those facts can never mask a non-owner's stable `NotOwner`.
//!
//! Reserving the single writer at `BEGIN IMMEDIATE` serializes cancel against the
//! fire path's own `IMMEDIATE` claim/settlement: whichever transaction commits
//! first is seen whole by the other. Either the cancel commits first (a later
//! settlement CAS, predicated on `status='firing'`, finds nothing and no-ops) or
//! the fire path commits first (this reads the post-fire status and the CAS
//! reflects it). This adapter NEVER repurposes the fire-path claim/settlement CAS.

use std::{path::Path, time::Duration};

use silverhand_backend_core::conversation_messaging::messaging::schedule::{
    CancelTarget, CapabilityVerdict, Clock, OwnerBind, ScheduleCancelOutcome,
    ScheduleCancelUnitOfWork, ScheduleId, ScheduleStatus, bind_owner, evaluate_capability,
};
use silverhand_backend_core::foundation::identity::{AgentId, WorkspaceId};
use silverhand_backend_core::foundation::persistence::PersistenceFailure;
use sqlx::Row;

use super::product::project_schedule_view;
use super::{ScheduleStoreError, SystemClock, corrupt, driver};

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// Reserve the single writer up front so cancel and the fire path serialize; a
/// cancel that races a claim then sees a whole pre- or post-fire state, never a
/// torn one.
const CANCEL_BEGIN: &str = "BEGIN IMMEDIATE";

/// The schedule facts the policy needs, read inside the transaction.
const SELECT_SCHEDULE: &str =
    "SELECT owner_agent_id, target_conversation_id, status FROM schedules WHERE id = ?";

/// The target conversation's placement facts: its workspace (NULL for a
/// direct-pair board), plus the `title`/`roster_policy` the returned view's label
/// needs. Read whether or not the workspace exists — the label is independent of
/// the capability axis.
const SELECT_TARGET_CONVERSATION: &str =
    "SELECT workspace_id, roster_policy, title FROM conversations WHERE id = ?";

/// The `manage_schedules` grant for `owner` against workspace `w`, the SAME
/// `owner_agent_id OR workspace_agent_capabilities` shape the other capability
/// reads use, with the token hardcoded. Returns no row when the workspace does
/// not exist — the caller reads that as "not granted", matching
/// `canAgentUseCapability`'s `if (!workspace) return false`.
const RESOLVE_MANAGE_SCHEDULES: &str = "SELECT CASE WHEN w.owner_agent_id = ? OR EXISTS ( \
     SELECT 1 FROM workspace_agent_capabilities c WHERE c.workspace_id = w.id \
     AND c.agent_id = ? AND c.capability = 'manage_schedules') THEN 1 ELSE 0 END \
     FROM workspaces w WHERE w.id = ?";

/// The conditional cancel: flip `pending|firing → cancelled`, stamping
/// `cancelled_at`/`updated_at`, and project the post-image via `RETURNING`. A
/// terminal row matches nothing (the CAS-miss the caller reports as
/// `NotCancellable`). Identical predicate to the TypeScript `cancel()`.
const CANCEL_CAS: &str = "UPDATE schedules \
     SET status = 'cancelled', cancelled_at = ?, updated_at = ? \
     WHERE id = ? AND status IN ('pending', 'firing') \
     RETURNING *";

/// The cancel write authority. Its clock stamps `cancelled_at`/`updated_at`.
#[derive(Clone)]
pub struct SqliteScheduleCancel<K = SystemClock> {
    pool: crate::foundation::database::SqliteDatabase,
    clock: K,
}

impl SqliteScheduleCancel<SystemClock> {
    /// Opens an existing product database with the real system clock. Durability
    /// matches the fire store (WAL + FULL): a committed cancellation must survive
    /// power loss so a `cancelled` row never rolls back to `pending`.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, ScheduleStoreError> {
        Self::open_with_clock(path, SystemClock).await
    }
}

impl<K> SqliteScheduleCancel<K> {
    pub async fn open_with_clock(
        path: impl AsRef<Path>,
        clock: K,
    ) -> Result<Self, ScheduleStoreError> {
        let pool = crate::foundation::database::store_pool(path.as_ref(), BUSY_TIMEOUT).await?;
        Ok(Self { pool, clock })
    }

    /// Wraps an already-open pool with an injected clock, for a shared-pool
    /// composition root or a deterministic test clock.
    pub fn from_pool(
        pool: impl Into<crate::foundation::database::SqliteDatabase>,
        clock: K,
    ) -> Self {
        let pool = pool.into();
        Self { pool, clock }
    }
}

impl<K: Clock + Send + Sync> ScheduleCancelUnitOfWork for SqliteScheduleCancel<K> {
    async fn cancel_owned_schedule(
        &self,
        schedule_id: &ScheduleId,
        owner: &AgentId,
    ) -> Result<ScheduleCancelOutcome, PersistenceFailure> {
        let mut tx = self.pool.begin_with(CANCEL_BEGIN).await.map_err(driver)?;

        // 1. Load the schedule. Absent → the anti-guess 404.
        let Some(schedule_row) = sqlx::query(SELECT_SCHEDULE)
            .bind(schedule_id.as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(driver)?
        else {
            tx.rollback().await.map_err(driver)?;
            return Ok(ScheduleCancelOutcome::NotFound);
        };
        let schedule_owner: String = schedule_row.try_get("owner_agent_id").map_err(driver)?;
        let target_conversation_id: String = schedule_row
            .try_get("target_conversation_id")
            .map_err(driver)?;
        let schedule_owner =
            AgentId::parse(schedule_owner).map_err(|_| corrupt("schedules.owner_agent_id"))?;

        // 2. OWNER-BIND FIRST — from the schedule's own owner ALONE. A schedule
        //    the caller does not own is refused here, before ANY target or
        //    capability fact is read, so a fault or corruption on the target side
        //    can never turn `NotOwner` into a 500. This is the core rule
        //    ([`bind_owner`]); the SQLite side holds no second owner comparison.
        if let OwnerBind::NotOwner = bind_owner(owner, &schedule_owner) {
            tx.rollback().await.map_err(driver)?;
            return Ok(ScheduleCancelOutcome::NotOwner);
        }

        // 3. Owner confirmed: resolve the target's label facts (independent of
        //    workspace) and its capability axis. A missing conversation or a
        //    null/absent workspace is owner cleanup — no `manage_schedules` gate.
        let target_conversation = sqlx::query(SELECT_TARGET_CONVERSATION)
            .bind(&target_conversation_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(driver)?;

        let mut label_title: Option<String> = None;
        let mut label_roster: Option<String> = None;
        let mut denied_workspace: Option<WorkspaceId> = None;
        let target = match target_conversation {
            None => CancelTarget::OwnerCleanup,
            Some(conversation) => {
                label_title = conversation.try_get("title").map_err(driver)?;
                label_roster = conversation.try_get("roster_policy").map_err(driver)?;
                let workspace_id: Option<String> =
                    conversation.try_get("workspace_id").map_err(driver)?;
                match workspace_id {
                    None => CancelTarget::OwnerCleanup,
                    Some(workspace_id) => {
                        let workspace_id = WorkspaceId::parse(workspace_id)
                            .map_err(|_| corrupt("conversations.workspace_id"))?;
                        let granted: Option<i64> = sqlx::query_scalar(RESOLVE_MANAGE_SCHEDULES)
                            .bind(schedule_owner.as_str())
                            .bind(schedule_owner.as_str())
                            .bind(workspace_id.as_str())
                            .fetch_optional(&mut *tx)
                            .await
                            .map_err(driver)?;
                        // No workspace row → not granted (matches `if (!workspace)`).
                        let manage_schedules = granted.unwrap_or(0) != 0;
                        denied_workspace = Some(workspace_id);
                        CancelTarget::Workspace { manage_schedules }
                    }
                }
            }
        };

        // 4. The capability gate ([`evaluate_capability`]) — reached only for an
        //    owned schedule. Its verdict set is closed to Proceed/Denied, so this
        //    match is exhaustive with NO fail-open wildcard: owner-bind (step 2)
        //    is the ONLY thing that can refuse for ownership.
        match evaluate_capability(&target) {
            CapabilityVerdict::Proceed => {}
            CapabilityVerdict::Denied => {
                tx.rollback().await.map_err(driver)?;
                // Denied arises only from `Workspace { manage_schedules: false }`,
                // where `denied_workspace` was set. If that invariant is ever
                // broken, fail CLOSED (safe 500), never panic as a protocol guard.
                let workspace_id = denied_workspace
                    .ok_or_else(|| corrupt("cancel denied without a resolved workspace"))?;
                return Ok(ScheduleCancelOutcome::CapabilityDenied { workspace_id });
            }
        }

        // 5. The CAS. A terminal row matches nothing → NotCancellable with the
        //    pre-image status (the tx's writer reservation means the pre-image and
        //    the CAS-time status are the same fact).
        let now = self.clock.now();
        let Some(updated) = sqlx::query(CANCEL_CAS)
            .bind(now)
            .bind(now)
            .bind(schedule_id.as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(driver)?
        else {
            let current_status: String = schedule_row.try_get("status").map_err(driver)?;
            let current_status = ScheduleStatus::from_token(&current_status)
                .ok_or_else(|| corrupt("schedules.status"))?;
            tx.rollback().await.map_err(driver)?;
            return Ok(ScheduleCancelOutcome::NotCancellable { current_status });
        };

        let view =
            project_schedule_view(&updated, label_title.as_deref(), label_roster.as_deref())?;
        tx.commit().await.map_err(driver)?;
        Ok(ScheduleCancelOutcome::Cancelled(Box::new(view)))
    }
}
