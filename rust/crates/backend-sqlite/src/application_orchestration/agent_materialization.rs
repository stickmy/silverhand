//! The SQLite adapter for the agent-materialization lifecycle: the reserve INSERT, the
//! two failure compare-and-sets, the reopen compare-and-set, and the crash-recovery bulk
//! fail — each under the store's single-writer reservation.
//!
//! This adapter WITNESSES; it never mints the domain verdict. Every write projects the
//! row it actually touched from its own `RETURNING` clause and hands it back for the core
//! free functions to bind and seal. A guarded compare-and-set that changed no row returns
//! `None` (→ `Lost`); the reserve reports [`ReserveRecord::ConstraintConflict`] from an
//! in-transaction preflight — under the store's single-writer reservation, before the
//! INSERT — of the shared @-handle namespace and the per-creator idempotency key. The
//! preflight, not a driver error kind, is the conflict authority: the two collisions the
//! reserve could hit (the handle uniqueness — including the cross-namespace user/agent
//! trigger that surfaces as an opaque `SQLITE_CONSTRAINT_TRIGGER`, not a unique violation —
//! and the per-creator `idempotency_key` index) both fold into one honest fact, and any
//! error the INSERT itself raises after a clean preflight is a hard fault, not a product
//! answer. There is no activate write here — see the core module docs.

use silverhand_backend_core::{
    application_orchestration::agent_materialization::{
        ActivationFlipRow, AgentArmProvisioningPort, AgentMaterializationLookup,
        AgentMaterializationWrite, DEPENDENT_GRAPH_FAILED_CODE, IdempotentReservationRow,
        MaterializationFlipRow, RawAgentArmGraph, RawConversationAgentEdge,
        RawConversationArmGraph, RawConversationUserEdge, RawCreatorIdentity, RawHomeWorkspaceRow,
        RawOnboardingDmRow, RawProvisionOutcome, RawWorkspaceOwnerEdge, ReserveAgentInput,
        ReserveRecord, ReservedAgentRow, STALE_MATERIALIZATION_FAILED_CODE,
        agent_home_workspace_description, home_workspace_id_for,
    },
    conversation_messaging::messaging::{
        DirectPairMintIdentityMismatch, DirectPairOpenedFact, DirectPairOpenedPort,
        DirectPairParticipants, MessageId, OpenDirectPairError, ParticipantHandle,
        PrincipalIdentity,
        agent_onboarding::{ONBOARDING_DM_TITLE, ONBOARDING_DM_TITLE_KEY, OnboardingSystemMessage},
    },
    foundation::identity::{AgentId, ConversationId, UserId},
    foundation::persistence::PersistenceFailure,
    identity_access::agents::session_scope::AgentSessionScope,
};
use sqlx::{Row, SqliteConnection, sqlite::SqliteRow};

use crate::foundation::host_agent_store::{SqliteHostAgentStore, decode};

/// Insert a fresh `pending_materialization` row and return the columns the domain seals
/// its committed identity against. `display_name` seeds from the handle, matching the
/// TypeScript `reserveForMaterialize`.
const RESERVE_INSERT: &str = "INSERT INTO agents \
        (id, name, display_name, runtime, model, reasoning_effort, fast_mode, \
         host_id, description, session_scope, created_by_user_id, \
         materialization_status, idempotency_key, created_at, updated_at) \
     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending_materialization', ?, ?, ?) \
    RETURNING id, name, display_name, runtime, model, reasoning_effort, fast_mode, \
              host_id, description, session_scope, created_by_user_id, \
              materialization_status, idempotency_key, created_at, updated_at";

/// Whether a reservation already exists for this `(creator, idempotency_key)` pair — the
/// per-creator half of the reserve preflight, mirroring the partial unique index
/// `idx_agents_idempotency_key_per_user`. Bound with a concrete non-null key, so it never
/// matches the `WHERE idempotency_key IS NOT NULL` rows the index ignores.
const IDEMPOTENCY_TAKEN: &str = "SELECT EXISTS(SELECT 1 FROM agents \
     WHERE created_by_user_id = ? AND idempotency_key = ?)";

/// `pending|failed → failed`, stamping the typed code and timestamp. The status guard is
/// the sole eligibility gate; `active` is deliberately unmatched (a live row's failure
/// surface is destructive delete, not an in-place flip).
const FAIL_CAS: &str = "UPDATE agents \
        SET materialization_status = 'failed', \
            materialization_failed_code = ?, \
            materialization_failed_at = ?, \
            updated_at = ? \
      WHERE id = ? \
        AND materialization_status IN ('pending_materialization', 'failed') \
    RETURNING id, materialization_status, materialization_failed_code, \
              materialization_failed_at, updated_at";

/// `failed → pending_materialization`, clearing the failure columns so a later failure
/// writes a fresh stamp. Strictly `failed`-guarded: pending and active are unmatched.
const REOPEN_CAS: &str = "UPDATE agents \
        SET materialization_status = 'pending_materialization', \
            materialization_failed_code = NULL, \
            materialization_failed_at = NULL, \
            updated_at = ? \
      WHERE id = ? \
        AND materialization_status = 'failed' \
    RETURNING id, materialization_status, materialization_failed_code, \
              materialization_failed_at, updated_at";

/// Crash recovery: fail every row still pending and created strictly before the cutoff,
/// stamping the canonical code, and return the rows it touched. `active`/`failed` rows are
/// unmatched.
const MARK_STALE_FAILED: &str = "UPDATE agents \
        SET materialization_status = 'failed', \
            materialization_failed_code = ?, \
            materialization_failed_at = ?, \
            updated_at = ? \
      WHERE materialization_status = 'pending_materialization' \
        AND created_at < ? \
    RETURNING id, materialization_status, materialization_failed_code, \
              materialization_failed_at, updated_at";

/// Read the row for a `(created_by_user_id, idempotency_key)` pair. The creator and key
/// ride the row's OWN columns so the domain can bind them against the request; the `WHERE`
/// is this adapter's implementation, not the port contract.
const FIND_BY_IDEMPOTENCY: &str = "SELECT id, created_by_user_id, idempotency_key, \
            materialization_status, materialization_failed_code, materialization_failed_at, \
            name, runtime, model, description, host_id, session_scope \
       FROM agents \
      WHERE created_by_user_id = ? \
        AND idempotency_key = ?";

/// The stored `fast_mode` boolean as its `0`/`1`/null column, matching the
/// `fast_mode === undefined ? null : fast_mode ? 1 : 0` the TypeScript reserve applies.
fn fast_mode_column(fast_mode: Option<bool>) -> Option<i64> {
    fast_mode.map(|on| if on { 1 } else { 0 })
}

/// Project the reserve after-image from its `RETURNING` row covering every inserted
/// column. The `AgentId` is decoded from the row's OWN `id`, never the query parameter;
/// the typed columns (runtime, host, session scope, creator, fast_mode) ride back raw for
/// the domain to parse and bind.
fn reserved_agent_row(row: &SqliteRow) -> Result<ReservedAgentRow, sqlx::Error> {
    Ok(ReservedAgentRow {
        agent_id: decode::agent_id(row.try_get::<String, _>("id")?)?,
        name: row.try_get::<String, _>("name")?,
        display_name: row.try_get::<String, _>("display_name")?,
        runtime: row.try_get::<String, _>("runtime")?,
        model: row.try_get::<Option<String>, _>("model")?,
        reasoning_effort: row.try_get::<Option<String>, _>("reasoning_effort")?,
        fast_mode: row.try_get::<Option<i64>, _>("fast_mode")?,
        host_id: row.try_get::<String, _>("host_id")?,
        description: row.try_get::<Option<String>, _>("description")?,
        session_scope: row.try_get::<String, _>("session_scope")?,
        created_by_user_id: row.try_get::<String, _>("created_by_user_id")?,
        materialization_status: row.try_get::<String, _>("materialization_status")?,
        idempotency_key: row.try_get::<Option<String>, _>("idempotency_key")?,
        created_at: row.try_get::<i64, _>("created_at")?,
        updated_at: row.try_get::<i64, _>("updated_at")?,
    })
}

/// Project a flip after-image from its `RETURNING` row.
fn flip_row(row: &SqliteRow) -> Result<MaterializationFlipRow, sqlx::Error> {
    Ok(MaterializationFlipRow {
        agent_id: decode::agent_id(row.try_get::<String, _>("id")?)?,
        materialization_status: row.try_get::<String, _>("materialization_status")?,
        materialization_failed_code: row
            .try_get::<Option<String>, _>("materialization_failed_code")?,
        materialization_failed_at: row.try_get::<Option<i64>, _>("materialization_failed_at")?,
        updated_at: row.try_get::<i64, _>("updated_at")?,
    })
}

/// Project a lookup witness. Identity columns are decoded fail-closed; the creator, key,
/// status, and runtime are carried raw for the domain to bind and parse.
fn idempotent_reservation_row(row: &SqliteRow) -> Result<IdempotentReservationRow, sqlx::Error> {
    Ok(IdempotentReservationRow {
        agent_id: decode::agent_id(row.try_get::<String, _>("id")?)?,
        created_by_user_id: row.try_get::<String, _>("created_by_user_id")?,
        idempotency_key: row.try_get::<Option<String>, _>("idempotency_key")?,
        materialization_status: row.try_get::<String, _>("materialization_status")?,
        materialization_failed_code: row
            .try_get::<Option<String>, _>("materialization_failed_code")?,
        materialization_failed_at: row.try_get::<Option<i64>, _>("materialization_failed_at")?,
        name: row.try_get::<String, _>("name")?,
        runtime: row.try_get::<String, _>("runtime")?,
        model: row.try_get::<Option<String>, _>("model")?,
        description: row.try_get::<Option<String>, _>("description")?,
        host_id: row.try_get::<String, _>("host_id")?,
        session_scope: row.try_get::<String, _>("session_scope")?,
    })
}

impl AgentMaterializationLookup for SqliteHostAgentStore {
    async fn find_by_idempotency(
        &self,
        creator: &UserId,
        key: &silverhand_backend_core::application_orchestration::agent_materialization::IdempotencyKey,
    ) -> Result<Option<IdempotentReservationRow>, PersistenceFailure> {
        let row = sqlx::query(FIND_BY_IDEMPOTENCY)
            .bind(creator.as_str())
            .bind(key.as_str())
            .fetch_optional(self.pool())
            .await
            .map_err(PersistenceFailure::from_source)?;
        match row {
            Some(row) => Ok(Some(
                idempotent_reservation_row(&row).map_err(PersistenceFailure::from_source)?,
            )),
            None => Ok(None),
        }
    }
}

impl AgentMaterializationWrite for SqliteHostAgentStore {
    async fn reserve(
        &self,
        input: &ReserveAgentInput,
        now_ms: i64,
    ) -> Result<ReserveRecord, PersistenceFailure> {
        let mut tx = self
            .begin_root_write()
            .await
            .map_err(PersistenceFailure::from_source)?;

        // Preflight both uniqueness axes under the write reservation, before the INSERT.
        // The shared-handle check reuses the one canonical namespace predicate, so it also
        // catches the cross-namespace user/agent collision the trigger would otherwise
        // raise as an opaque constraint error. A hit rolls back as the transaction drops.
        if crate::identity_access::handle_namespace::handle_taken(&mut *tx, input.name.as_str())
            .await
            .map_err(PersistenceFailure::from_source)?
        {
            return Ok(ReserveRecord::ConstraintConflict);
        }
        let idempotency_taken: i64 = sqlx::query_scalar(IDEMPOTENCY_TAKEN)
            .bind(input.created_by_user_id.as_str())
            .bind(input.idempotency_key.as_str())
            .fetch_one(&mut *tx)
            .await
            .map_err(PersistenceFailure::from_source)?;
        if idempotency_taken != 0 {
            return Ok(ReserveRecord::ConstraintConflict);
        }

        let attempted = sqlx::query(RESERVE_INSERT)
            .bind(input.agent_id.as_str())
            .bind(input.name.as_str())
            .bind(input.name.as_str())
            .bind(input.runtime.as_str())
            .bind(input.model.as_deref())
            .bind(input.reasoning_effort.as_deref())
            .bind(fast_mode_column(input.fast_mode))
            .bind(input.host_id.as_str())
            .bind(input.description.as_deref())
            .bind(input.session_scope.as_str())
            .bind(input.created_by_user_id.as_str())
            .bind(input.idempotency_key.as_str())
            .bind(now_ms)
            .bind(now_ms)
            .fetch_optional(&mut *tx)
            .await;

        match attempted {
            Ok(Some(row)) => {
                let witness = reserved_agent_row(&row).map_err(PersistenceFailure::from_source)?;
                tx.commit().await.map_err(PersistenceFailure::from_source)?;
                Ok(ReserveRecord::Inserted(Box::new(witness)))
            }
            // An `INSERT ... RETURNING` that inserts a row always returns it; no row means
            // a broken invariant, not a product answer.
            Ok(None) => Err(PersistenceFailure::from_source(ReserveReturnedNoRow)),
            // The preflight already folded the two honest conflicts into
            // `ConstraintConflict`; any error the INSERT raises now is a hard fault, never a
            // product answer, and the transaction drops without a commit.
            Err(error) => Err(PersistenceFailure::from_source(error)),
        }
    }

    async fn fail(
        &self,
        agent: &AgentId,
        failed_code: &str,
        failed_at: i64,
        now_ms: i64,
    ) -> Result<Option<MaterializationFlipRow>, PersistenceFailure> {
        let mut tx = self
            .begin_root_write()
            .await
            .map_err(PersistenceFailure::from_source)?;
        let returned = sqlx::query(FAIL_CAS)
            .bind(failed_code)
            .bind(failed_at)
            .bind(now_ms)
            .bind(agent.as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(PersistenceFailure::from_source)?;
        let witness = match returned {
            Some(row) => Some(flip_row(&row).map_err(PersistenceFailure::from_source)?),
            None => None,
        };
        tx.commit().await.map_err(PersistenceFailure::from_source)?;
        Ok(witness)
    }

    async fn reopen_failed(
        &self,
        agent: &AgentId,
        now_ms: i64,
    ) -> Result<Option<MaterializationFlipRow>, PersistenceFailure> {
        let mut tx = self
            .begin_root_write()
            .await
            .map_err(PersistenceFailure::from_source)?;
        let returned = sqlx::query(REOPEN_CAS)
            .bind(now_ms)
            .bind(agent.as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(PersistenceFailure::from_source)?;
        let witness = match returned {
            Some(row) => Some(flip_row(&row).map_err(PersistenceFailure::from_source)?),
            None => None,
        };
        tx.commit().await.map_err(PersistenceFailure::from_source)?;
        Ok(witness)
    }

    async fn mark_stale_failed(
        &self,
        cutoff_created_at: i64,
        failed_at: i64,
        now_ms: i64,
    ) -> Result<Vec<MaterializationFlipRow>, PersistenceFailure> {
        let mut tx = self
            .begin_root_write()
            .await
            .map_err(PersistenceFailure::from_source)?;
        let returned = sqlx::query(MARK_STALE_FAILED)
            .bind(STALE_MATERIALIZATION_FAILED_CODE)
            .bind(failed_at)
            .bind(now_ms)
            .bind(cutoff_created_at)
            .fetch_all(&mut *tx)
            .await
            .map_err(PersistenceFailure::from_source)?;
        let mut rows = Vec::with_capacity(returned.len());
        for row in &returned {
            rows.push(flip_row(row).map_err(PersistenceFailure::from_source)?);
        }
        tx.commit().await.map_err(PersistenceFailure::from_source)?;
        Ok(rows)
    }
}

/// A reserve `INSERT ... RETURNING` that reported success but handed back no row — a broken
/// SQLite invariant the boundary surfaces as a safe `500`, never a product answer.
#[derive(Debug, thiserror::Error)]
#[error("reserve insert returned no row")]
struct ReserveReturnedNoRow;

/// The activation flip: `pending_materialization → active`, strictly pending-guarded and
/// stamping the request clock, returning the after-image the domain seals its identity
/// against. `active`/`failed`/absent rows are unmatched, so a guard miss changes no row —
/// the flip that fires FIRST inside the provisioning transaction and, when it does not win,
/// means nothing has been written. The `RETURNING` carries the OWN id, handle, scope, and
/// creator the dependent graph is provisioned against, never the query parameters.
const FLIP_TO_ACTIVE: &str = "UPDATE agents \
        SET materialization_status = 'active', \
            updated_at = ? \
      WHERE id = ? \
        AND materialization_status = 'pending_materialization' \
    RETURNING id, name, session_scope, created_by_user_id, materialization_status, created_at, updated_at";

/// Read the creator's OWN identity from its `users` row, bound to the id the activation flip
/// proved: both the row's `id` and its `name` (the canonical @-handle every principal is
/// addressed by). The core use case source-binds the returned `id` against the activation
/// creator before rendering the handle into the greeting; an absent row is a broken FK
/// invariant the provisioning unit of work surfaces as a fault.
const READ_CREATOR_IDENTITY: &str = "SELECT id, name FROM users WHERE id = ?";

/// Project the activation flip after-image from its `RETURNING` row. The `AgentId` is
/// decoded from the row's OWN `id`; the handle, scope, and creator ride back raw for the
/// domain (`ActivatedAgent::seal`) to parse and bind.
fn activation_flip_row(row: &SqliteRow) -> Result<ActivationFlipRow, sqlx::Error> {
    Ok(ActivationFlipRow {
        agent_id: decode::agent_id(row.try_get::<String, _>("id")?)?,
        name: row.try_get::<String, _>("name")?,
        materialization_status: row.try_get::<String, _>("materialization_status")?,
        session_scope: row.try_get::<String, _>("session_scope")?,
        created_by_user_id: row.try_get::<String, _>("created_by_user_id")?,
        created_at: row.try_get::<i64, _>("created_at")?,
        updated_at: row.try_get::<i64, _>("updated_at")?,
    })
}

/// Run the guarded activation flip on the caller's connection, inside its transaction.
/// `None` means the guarded CAS changed no row (absent, already `active`, or `failed`): the
/// caller has written nothing and rolls back without compensating.
async fn flip_to_active_on(
    conn: &mut SqliteConnection,
    agent: &AgentId,
    now_ms: i64,
) -> Result<Option<ActivationFlipRow>, PersistenceFailure> {
    let returned = sqlx::query(FLIP_TO_ACTIVE)
        .bind(now_ms)
        .bind(agent.as_str())
        .fetch_optional(&mut *conn)
        .await
        .map_err(PersistenceFailure::from_source)?;
    match returned {
        Some(row) => Ok(Some(
            activation_flip_row(&row).map_err(PersistenceFailure::from_source)?,
        )),
        None => Ok(None),
    }
}

/// Read the creator's own-row identity on the caller's connection. `None` means no user row
/// bears the creator id the flip proved — a broken FK invariant, which the provisioning unit
/// of work maps to a fault rather than a product answer.
async fn read_creator_identity_on(
    conn: &mut SqliteConnection,
    creator: &UserId,
) -> Result<Option<RawCreatorIdentity>, PersistenceFailure> {
    let row = sqlx::query(READ_CREATOR_IDENTITY)
        .bind(creator.as_str())
        .fetch_optional(&mut *conn)
        .await
        .map_err(PersistenceFailure::from_source)?;
    match row {
        Some(row) => Ok(Some(RawCreatorIdentity {
            source_user_id: row
                .try_get::<String, _>("id")
                .map_err(PersistenceFailure::from_source)?,
            name: row
                .try_get::<String, _>("name")
                .map_err(PersistenceFailure::from_source)?,
        })),
        None => Ok(None),
    }
}

/// Records whether the direct-pair open announced a freshly minted line, deriving
/// `pair_created` from the fact rather than a second read. `open_direct_pair` announces only
/// on a fresh mint, so a captured fact means this open created the line; a get-or-create that
/// resolved an existing one leaves the flag unset. The announcement itself is deferred to the
/// route/mount slice, so the port only records — it never fails.
#[derive(Default)]
struct RecordingDirectPairOpened {
    captured: std::sync::atomic::AtomicBool,
}

impl RecordingDirectPairOpened {
    fn pair_created(&self) -> bool {
        self.captured.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl DirectPairOpenedPort for RecordingDirectPairOpened {
    type Error = std::convert::Infallible;

    async fn direct_pair_opened(&self, _fact: &DirectPairOpenedFact) -> Result<(), Self::Error> {
        self.captured
            .store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }
}

/// `pending_materialization → failed` guarded to PENDING ONLY — the compensating fail after a
/// provisioning transaction has rolled back. Unlike [`FAIL_CAS`], `failed` is deliberately
/// UNMATCHED: once the rollback returned the row to pending, the only legal prior state is
/// that pending, so a row now `active` or `failed` was won by a concurrent transition whose
/// terminal state is authoritative and must never be clobbered. Stamps the intrinsic
/// dependent-graph code the domain expects.
const FAIL_PENDING_ONLY: &str = "UPDATE agents \
        SET materialization_status = 'failed', \
            materialization_failed_code = ?, \
            materialization_failed_at = ?, \
            updated_at = ? \
      WHERE id = ? \
        AND materialization_status = 'pending_materialization' \
    RETURNING id, materialization_status, materialization_failed_code, \
              materialization_failed_at, updated_at";

/// The agent's personal home workspace, written as ONE committed row: owned by the agent
/// (`owner_agent_id`), its human-owner pointer set to the creator, `name_key` NULL — the arc
/// CHECK pairs a NULL key with a non-null agent owner. This is `insertAgentHome` folded with
/// `setOwnerUserId`; the committed identity is what the domain seals, not the write sequence.
const INSERT_HOME_WORKSPACE: &str = "INSERT INTO workspaces \
        (id, name, name_key, description, owner_agent_id, archived_at, \
         created_at, updated_at, owner_user_id) \
     VALUES (?, ?, NULL, ?, ?, NULL, ?, ?, ?)";

/// The home `Onboarding` DM: a fixed-roster workspace channel carrying the canonical title
/// and folded key, private, un-spawned. Mirrors `insertDmChannel` / the fixed-roster arm of
/// the conversation insert.
const INSERT_DM_CHANNEL: &str = "INSERT INTO conversations \
        (id, workspace_id, placement_kind, roster_policy, title, title_key, \
         spawned_by_agent_id, created_at, updated_at, visibility) \
     VALUES (?, ?, 'workspace', 'fixed', ?, ?, NULL, ?, ?, 'private')";

/// The agent's membership of its home DM.
const INSERT_CONVERSATION_AGENT: &str = "INSERT INTO conversation_agents \
        (conversation_id, agent_id, joined_at) VALUES (?, ?, ?)";

/// The creator's OWNER membership of the home workspace, folding `addUserMember` +
/// `setOwnerUserId`'s role promotion into the committed `role = 'owner'`.
const INSERT_WORKSPACE_OWNER: &str = "INSERT INTO workspace_users \
        (workspace_id, user_id, joined_at, role) VALUES (?, ?, ?, 'owner')";

/// The creator's membership of the home DM.
const INSERT_CONVERSATION_USER: &str = "INSERT INTO conversation_users \
        (conversation_id, user_id, joined_at) VALUES (?, ?, ?)";

/// Every column the writer touched on the home-workspace row, so the domain replays the
/// committed row against its intent, not just ownership.
const READ_HOME_WORKSPACE_BACK: &str = "SELECT id, name, name_key, description, owner_agent_id, \
     owner_user_id, archived_at, created_at, updated_at FROM workspaces WHERE id = ?";
/// Every column the writer touched on the home-DM row.
const READ_DM_CHANNEL_BACK: &str = "SELECT id, workspace_id, placement_kind, roster_policy, \
     title, title_key, visibility, spawned_by_agent_id, \
     created_at, updated_at FROM conversations WHERE id = ?";
/// The three roster edges, read back by their composite keys, each with its join clock.
const READ_CONVERSATION_AGENT_BACK: &str = "SELECT conversation_id, agent_id, joined_at \
     FROM conversation_agents WHERE conversation_id = ? AND agent_id = ?";
const READ_WORKSPACE_OWNER_BACK: &str = "SELECT workspace_id, user_id, role, joined_at \
     FROM workspace_users WHERE workspace_id = ? AND user_id = ?";
const READ_CONVERSATION_USER_BACK: &str = "SELECT conversation_id, user_id, joined_at \
     FROM conversation_users WHERE conversation_id = ? AND user_id = ?";

/// A row the provisioning transaction read back is structurally broken — an id that will not
/// parse, a creator with no user row, a mint whose endpoints disagree. The boundary surfaces
/// it as a safe `500` (a `PersistenceFailure`), never a product answer; the domain's own
/// invariant checks ride the sealed after-image separately.
#[derive(Debug, thiserror::Error)]
#[error("agent provisioning read a structurally broken row: {0}")]
struct ProvisioningRowBroken(&'static str);

fn raw_home_workspace_row(row: &SqliteRow) -> Result<RawHomeWorkspaceRow, sqlx::Error> {
    Ok(RawHomeWorkspaceRow {
        id: row.try_get::<String, _>("id")?,
        name: row.try_get::<String, _>("name")?,
        name_key: row.try_get::<Option<String>, _>("name_key")?,
        description: row.try_get::<Option<String>, _>("description")?,
        owner_agent_id: row.try_get::<Option<String>, _>("owner_agent_id")?,
        owner_user_id: row.try_get::<Option<String>, _>("owner_user_id")?,
        archived_at: row.try_get::<Option<i64>, _>("archived_at")?,
        created_at: row.try_get::<i64, _>("created_at")?,
        updated_at: row.try_get::<i64, _>("updated_at")?,
    })
}

fn raw_onboarding_dm_row(row: &SqliteRow) -> Result<RawOnboardingDmRow, sqlx::Error> {
    Ok(RawOnboardingDmRow {
        id: row.try_get::<String, _>("id")?,
        workspace_id: row.try_get::<Option<String>, _>("workspace_id")?,
        placement_kind: row.try_get::<String, _>("placement_kind")?,
        roster_policy: row.try_get::<String, _>("roster_policy")?,
        title: row.try_get::<String, _>("title")?,
        title_key: row.try_get::<Option<String>, _>("title_key")?,
        visibility: row.try_get::<String, _>("visibility")?,
        spawned_by_agent_id: row.try_get::<Option<String>, _>("spawned_by_agent_id")?,
        created_at: row.try_get::<i64, _>("created_at")?,
        updated_at: row.try_get::<i64, _>("updated_at")?,
    })
}

/// Provision the AGENT-scoped dependent graph on an already-flipped, agent-scoped reservation,
/// on the caller's connection inside its transaction. This is a pure WRITER: the flip fired in
/// the dispatcher, so this never touches the reservation's status — it opens the creator↔agent
/// direct line (get-or-create, so a concurrent reopen resolves rather than conflicts) riding
/// the flipped row's OWN `created_at`, and writes the onboarding system message on that line
/// riding `now_ms`. Every row read back is returned raw; the core use case subject-binds,
/// source-binds the creator identity, replays the minted candidate, and seals. A dependent-
/// write failure propagates as `Err`, and the dispatcher that owns the transaction rolls it
/// back — the core use case then drives the pending-only compensation.
async fn write_agent_graph_on(
    conn: &mut SqliteConnection,
    flip: ActivationFlipRow,
    candidate_direct_line: ConversationId,
    onboarding_message_id: MessageId,
    now_ms: i64,
) -> Result<RawAgentArmGraph, PersistenceFailure> {
    // The creator and agent identities ride the flip's OWN after-image columns; the writer
    // needs them to WRITE the graph, and returns the creator's own-row identity (and the
    // participants it opened the pair against) raw so the core use case can re-derive and
    // source-bind the same facts from the seal.
    let creator = decode::user_id(flip.created_by_user_id.clone())
        .map_err(PersistenceFailure::from_source)?;
    let creator_identity = read_creator_identity_on(conn, &creator)
        .await?
        .ok_or_else(|| {
            PersistenceFailure::from_source(ProvisioningRowBroken(
                "activation creator names no user row to greet on the direct line",
            ))
        })?;
    let creator_handle =
        ParticipantHandle::parse(creator_identity.name.as_str()).map_err(|_| {
            PersistenceFailure::from_source(ProvisioningRowBroken(
                "activation creator's stored handle is not addressable",
            ))
        })?;
    let agent_handle = ParticipantHandle::parse(flip.name.as_str()).map_err(|_| {
        PersistenceFailure::from_source(ProvisioningRowBroken(
            "activated agent's stored handle is not addressable",
        ))
    })?;

    let participants = DirectPairParticipants::between(
        PrincipalIdentity::Human(creator),
        PrincipalIdentity::Agent(flip.agent_id.clone()),
    )
    .map_err(|_| {
        PersistenceFailure::from_source(ProvisioningRowBroken(
            "activation direct line would name a single principal twice",
        ))
    })?;

    let recorder = RecordingDirectPairOpened::default();
    let direct_line = crate::conversation_messaging::messaging::open_direct_pair_on(
        conn,
        candidate_direct_line,
        &recorder,
        participants.clone(),
        flip.created_at,
    )
    .await
    .map_err(|error| match error {
        OpenDirectPairError::Mint(failure) => failure,
        OpenDirectPairError::Identity(DirectPairMintIdentityMismatch) => {
            PersistenceFailure::from_source(ProvisioningRowBroken(
                "direct-pair mint selected endpoints differ from the agent and creator",
            ))
        }
        OpenDirectPairError::Announcement(never) => match never {},
    })?;

    let intent = OnboardingSystemMessage::agent_direct_line(
        direct_line.clone(),
        onboarding_message_id,
        &agent_handle,
        &creator_handle,
        now_ms,
    );
    let onboarding = crate::conversation_messaging::messaging::append_onboarding_system_message_on(
        conn, &intent,
    )
    .await?;

    Ok(RawAgentArmGraph {
        flip,
        creator: creator_identity,
        direct_line,
        direct_line_participants: participants,
        pair_created: recorder.pair_created(),
        onboarding,
    })
}

/// Provision the CONVERSATION-scoped dependent graph on an already-flipped, conversation-scoped
/// reservation. The conversation-arm twin of [`write_agent_graph_on`]: a pure WRITER (the flip
/// fired in the dispatcher). It writes, all on `now_ms`: the agent's personal home workspace,
/// its `Onboarding` DM channel, the three roster edges (agent-in-DM, creator-owns-workspace,
/// creator-in-DM), and the onboarding message on the DM. Every row is read back raw and
/// returned; the core use case seals every touched column against intent.
async fn write_conversation_graph_on(
    conn: &mut SqliteConnection,
    flip: ActivationFlipRow,
    dm_channel_id: ConversationId,
    onboarding_message_id: MessageId,
    now_ms: i64,
) -> Result<RawConversationArmGraph, PersistenceFailure> {
    let creator = decode::user_id(flip.created_by_user_id.clone())
        .map_err(PersistenceFailure::from_source)?;
    let agent_handle = ParticipantHandle::parse(flip.name.as_str()).map_err(|_| {
        PersistenceFailure::from_source(ProvisioningRowBroken(
            "activated agent's stored handle is not addressable",
        ))
    })?;
    // The one derivation both this write and the core seal compute, so neither recites the
    // other's literal id or description string.
    let home_id = home_workspace_id_for(&flip.agent_id);
    let description = agent_home_workspace_description(agent_handle.as_str());

    sqlx::query(INSERT_HOME_WORKSPACE)
        .bind(home_id.as_str())
        .bind(agent_handle.as_str())
        .bind(description)
        .bind(flip.agent_id.as_str())
        .bind(now_ms)
        .bind(now_ms)
        .bind(creator.as_str())
        .execute(&mut *conn)
        .await
        .map_err(PersistenceFailure::from_source)?;

    sqlx::query(INSERT_DM_CHANNEL)
        .bind(dm_channel_id.as_str())
        .bind(home_id.as_str())
        .bind(ONBOARDING_DM_TITLE)
        .bind(ONBOARDING_DM_TITLE_KEY)
        .bind(now_ms)
        .bind(now_ms)
        .execute(&mut *conn)
        .await
        .map_err(PersistenceFailure::from_source)?;

    sqlx::query(INSERT_CONVERSATION_AGENT)
        .bind(dm_channel_id.as_str())
        .bind(flip.agent_id.as_str())
        .bind(now_ms)
        .execute(&mut *conn)
        .await
        .map_err(PersistenceFailure::from_source)?;
    sqlx::query(INSERT_WORKSPACE_OWNER)
        .bind(home_id.as_str())
        .bind(creator.as_str())
        .bind(now_ms)
        .execute(&mut *conn)
        .await
        .map_err(PersistenceFailure::from_source)?;
    sqlx::query(INSERT_CONVERSATION_USER)
        .bind(dm_channel_id.as_str())
        .bind(creator.as_str())
        .bind(now_ms)
        .execute(&mut *conn)
        .await
        .map_err(PersistenceFailure::from_source)?;

    let intent = OnboardingSystemMessage::conversation_dm(
        dm_channel_id.clone(),
        onboarding_message_id,
        &agent_handle,
        now_ms,
    );
    let onboarding = crate::conversation_messaging::messaging::append_onboarding_system_message_on(
        conn, &intent,
    )
    .await?;

    let home_row = raw_home_workspace_row(
        &sqlx::query(READ_HOME_WORKSPACE_BACK)
            .bind(home_id.as_str())
            .fetch_one(&mut *conn)
            .await
            .map_err(PersistenceFailure::from_source)?,
    )
    .map_err(PersistenceFailure::from_source)?;
    let dm_row = raw_onboarding_dm_row(
        &sqlx::query(READ_DM_CHANNEL_BACK)
            .bind(dm_channel_id.as_str())
            .fetch_one(&mut *conn)
            .await
            .map_err(PersistenceFailure::from_source)?,
    )
    .map_err(PersistenceFailure::from_source)?;
    let agent_edge = {
        let row = sqlx::query(READ_CONVERSATION_AGENT_BACK)
            .bind(dm_channel_id.as_str())
            .bind(flip.agent_id.as_str())
            .fetch_one(&mut *conn)
            .await
            .map_err(PersistenceFailure::from_source)?;
        RawConversationAgentEdge {
            conversation_id: row
                .try_get::<String, _>("conversation_id")
                .map_err(PersistenceFailure::from_source)?,
            agent_id: row
                .try_get::<String, _>("agent_id")
                .map_err(PersistenceFailure::from_source)?,
            joined_at: row
                .try_get::<i64, _>("joined_at")
                .map_err(PersistenceFailure::from_source)?,
        }
    };
    let owner_edge = {
        let row = sqlx::query(READ_WORKSPACE_OWNER_BACK)
            .bind(home_id.as_str())
            .bind(creator.as_str())
            .fetch_one(&mut *conn)
            .await
            .map_err(PersistenceFailure::from_source)?;
        RawWorkspaceOwnerEdge {
            workspace_id: row
                .try_get::<String, _>("workspace_id")
                .map_err(PersistenceFailure::from_source)?,
            user_id: row
                .try_get::<String, _>("user_id")
                .map_err(PersistenceFailure::from_source)?,
            role: row
                .try_get::<String, _>("role")
                .map_err(PersistenceFailure::from_source)?,
            joined_at: row
                .try_get::<i64, _>("joined_at")
                .map_err(PersistenceFailure::from_source)?,
        }
    };
    let user_edge = {
        let row = sqlx::query(READ_CONVERSATION_USER_BACK)
            .bind(dm_channel_id.as_str())
            .bind(creator.as_str())
            .fetch_one(&mut *conn)
            .await
            .map_err(PersistenceFailure::from_source)?;
        RawConversationUserEdge {
            conversation_id: row
                .try_get::<String, _>("conversation_id")
                .map_err(PersistenceFailure::from_source)?,
            user_id: row
                .try_get::<String, _>("user_id")
                .map_err(PersistenceFailure::from_source)?,
            joined_at: row
                .try_get::<i64, _>("joined_at")
                .map_err(PersistenceFailure::from_source)?,
        }
    };

    Ok(RawConversationArmGraph {
        flip,
        home_workspace: home_row,
        dm_channel: dm_row,
        agent_on_dm: agent_edge,
        creator_workspace_owner: owner_edge,
        creator_on_dm: user_edge,
        onboarding,
    })
}

impl AgentArmProvisioningPort for SqliteHostAgentStore {
    async fn provision_and_activate(
        &self,
        agent: &AgentId,
        candidate_direct_line: ConversationId,
        candidate_dm_channel: ConversationId,
        onboarding_message_id: MessageId,
        now_ms: i64,
    ) -> Result<RawProvisionOutcome, PersistenceFailure> {
        let mut tx = self
            .begin_root_write()
            .await
            .map_err(PersistenceFailure::from_source)?;

        // Flip ONCE. A guard miss wrote nothing; drop the empty transaction and report `Lost`.
        let Some(flip) = flip_to_active_on(&mut tx, agent, now_ms).await? else {
            drop(tx);
            return Ok(RawProvisionOutcome::Lost);
        };

        // The row's OWN session scope — read from the flip's `RETURNING`, never chosen by the
        // caller — selects the arm to write. A scope this provisioner cannot classify writes
        // NOTHING: roll the flip back so the row stays pending, and report `UnrecognizedScope`.
        let outcome = match AgentSessionScope::parse(&flip.session_scope) {
            Some(AgentSessionScope::Agent) => write_agent_graph_on(
                &mut tx,
                flip,
                candidate_direct_line,
                onboarding_message_id,
                now_ms,
            )
            .await
            .map(|graph| RawProvisionOutcome::Agent(Box::new(graph))),
            Some(AgentSessionScope::Conversation) => write_conversation_graph_on(
                &mut tx,
                flip,
                candidate_dm_channel,
                onboarding_message_id,
                now_ms,
            )
            .await
            .map(|graph| RawProvisionOutcome::Conversation(Box::new(graph))),
            None => {
                drop(tx);
                return Ok(RawProvisionOutcome::UnrecognizedScope);
            }
        };

        match outcome {
            Ok(provisioned) => {
                tx.commit().await.map_err(PersistenceFailure::from_source)?;
                Ok(provisioned)
            }
            // A dependent-write failure: roll the whole flip-and-graph back and surface the
            // cause. The core use case drives the pending-only compensation.
            Err(error) => {
                drop(tx);
                Err(error)
            }
        }
    }

    async fn fail_after_rolled_back_provision(
        &self,
        agent: &AgentId,
        failed_at: i64,
        now_ms: i64,
    ) -> Result<Option<MaterializationFlipRow>, PersistenceFailure> {
        let mut tx = self
            .begin_root_write()
            .await
            .map_err(PersistenceFailure::from_source)?;
        let returned = sqlx::query(FAIL_PENDING_ONLY)
            .bind(DEPENDENT_GRAPH_FAILED_CODE)
            .bind(failed_at)
            .bind(now_ms)
            .bind(agent.as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(PersistenceFailure::from_source)?;
        let witness = match returned {
            Some(row) => Some(flip_row(&row).map_err(PersistenceFailure::from_source)?),
            None => None,
        };
        tx.commit().await.map_err(PersistenceFailure::from_source)?;
        Ok(witness)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use silverhand_backend_core::{
        application_orchestration::agent_materialization::{
            DEPENDENT_GRAPH_FAILED_CODE, FailOutcome, IdempotencyKey, MaterializationFault,
            ProvisionedActivation, ReopenOutcome, ReserveOutcome, fail_materialization,
            find_idempotent_reservation, mark_stale_materializations_failed,
            provision_and_activate, reopen_failed_materialization, reserve_agent,
        },
        foundation::identity::HostId,
        identity_access::agent_lifecycle::MaterializationStatus,
        identity_access::agents::runtime::RuntimeKind,
        identity_access::agents::session_scope::AgentSessionScope,
        identity_access::user_identity::ParticipantHandle,
    };
    use tempfile::TempDir;

    use super::*;

    fn agent(id: &str) -> AgentId {
        AgentId::parse(id).expect("agent id")
    }

    fn user(id: &str) -> UserId {
        UserId::parse(id).expect("user id")
    }

    fn idem(key: &str) -> IdempotencyKey {
        IdempotencyKey::parse(key).expect("idempotency key")
    }

    async fn seed_schema(dir: &TempDir, name: &str, seed: &[String]) -> PathBuf {
        let path = dir.path().join(name);
        let mut connection = crate::current_schema::create_current_schema(&path).await;
        for statement in seed {
            sqlx::query(sqlx::AssertSqlSafe(statement.as_str()))
                .execute(&mut connection)
                .await
                .expect("seed statement");
        }
        use sqlx::Connection;
        connection.close().await.expect("close seed connection");
        path
    }

    const USER: &str = "INSERT INTO users (id, name, display_name, created_at, updated_at) \
         VALUES ('u1', 'u1', 'U1', 1, 1)";
    const HOST: &str =
        "INSERT INTO hosts (id, name, kind, created_at) VALUES ('h1', 'h1', 'local', 1)";

    /// Seed one agent row in an arbitrary status with the given create timestamp and key.
    fn seed_agent(
        id: &str,
        status: &str,
        created_at: i64,
        idempotency_key: Option<&str>,
    ) -> String {
        let key = match idempotency_key {
            Some(k) => format!("'{k}'"),
            None => "NULL".to_string(),
        };
        format!(
            "INSERT INTO agents \
             (id, name, display_name, runtime, host_id, created_by_user_id, created_at, \
              updated_at, materialization_status, session_scope, idempotency_key) \
             VALUES ('{id}', '{id}', '{id}', 'claude', 'h1', 'u1', {created_at}, {created_at}, \
                     '{status}', 'conversation', {key})"
        )
    }

    fn base_seed() -> Vec<String> {
        vec![USER.to_string(), HOST.to_string()]
    }

    fn input(id: &str, key: &str) -> ReserveAgentInput {
        ReserveAgentInput {
            agent_id: agent(id),
            name: ParticipantHandle::parse(id).unwrap(),
            runtime: RuntimeKind::Codex,
            model: Some("gpt".to_string()),
            reasoning_effort: Some("high".to_string()),
            fast_mode: Some(true),
            host_id: HostId::parse("h1").unwrap(),
            description: Some("d".to_string()),
            session_scope: AgentSessionScope::Conversation,
            created_by_user_id: user("u1"),
            idempotency_key: idem(key),
        }
    }

    #[tokio::test]
    async fn reserve_inserts_a_pending_row_and_seals_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = seed_schema(&dir, "reserve.db", &base_seed()).await;
        let store = SqliteHostAgentStore::open(&path).await.unwrap();
        assert!(
            find_idempotent_reservation(&store, &user("u1"), &idem("nope"))
                .await
                .unwrap()
                .is_none()
        );

        let ReserveOutcome::Reserved(reserved) = reserve_agent(&store, &input("a1", "idem-1"), 42)
            .await
            .unwrap()
        else {
            panic!("expected a reserved agent");
        };
        assert_eq!(reserved.agent_id(), &agent("a1"));
        assert_eq!(reserved.created_by_user_id(), &user("u1"));
        assert_eq!(reserved.idempotency_key(), &idem("idem-1"));
        assert_eq!(reserved.created_at(), 42);
        assert_eq!(reserved.updated_at(), 42);

        // The lookup sees the pending row with the metadata the seal proved and the handle
        // mirrored into display_name.
        let hit = find_idempotent_reservation(&store, &user("u1"), &idem("idem-1"))
            .await
            .unwrap()
            .expect("a hit");
        assert_eq!(
            hit.materialization_status(),
            MaterializationStatus::PendingMaterialization
        );
        assert_eq!(hit.runtime(), RuntimeKind::Codex);
        assert_eq!(hit.name(), "a1");
        assert_eq!(hit.host_id(), &HostId::parse("h1").unwrap());
        assert_eq!(hit.session_scope(), AgentSessionScope::Conversation);
        for (creator, key) in [("u1", "nope"), ("u2", "idem-1")] {
            assert!(
                find_idempotent_reservation(&store, &user(creator), &idem(key))
                    .await
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[tokio::test]
    async fn reserve_reports_a_handle_held_by_a_user_as_a_constraint_conflict() {
        // The cross-namespace collision: a user already holds `clash`. The name UNIQUE on
        // `agents` would not fire (no agent holds it), but the `agents_name_unique_against_
        // users_insert` trigger would `RAISE(ABORT)` as an opaque `SQLITE_CONSTRAINT_TRIGGER`
        // — not a unique violation. The preflight catches it as an honest conflict instead
        // of letting it drift to a 500.
        let dir = tempfile::tempdir().unwrap();
        let mut seed = base_seed();
        seed.push(
            "INSERT INTO users (id, name, display_name, created_at, updated_at) \
             VALUES ('u2', 'clash', 'Clash', 1, 1)"
                .to_string(),
        );
        let path = seed_schema(&dir, "reserve-user-clash.db", &seed).await;
        let store = SqliteHostAgentStore::open(&path).await.unwrap();

        assert_eq!(
            reserve_agent(&store, &input("clash", "fresh-key"), 42)
                .await
                .unwrap(),
            ReserveOutcome::ConstraintConflict
        );
    }

    #[tokio::test]
    async fn reserve_reports_a_duplicate_handle_as_a_constraint_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let mut seed = base_seed();
        seed.push(seed_agent("a1", "active", 1, Some("other-key")));
        let path = seed_schema(&dir, "reserve-name.db", &seed).await;
        let store = SqliteHostAgentStore::open(&path).await.unwrap();

        // A different id but the same handle `a1` trips the `name` unique constraint.
        let mut candidate = input("different", "fresh-key");
        candidate.name = ParticipantHandle::parse("a1").unwrap();
        assert_eq!(
            reserve_agent(&store, &candidate, 42).await.unwrap(),
            ReserveOutcome::ConstraintConflict
        );
    }

    #[tokio::test]
    async fn reserve_reports_a_duplicate_idempotency_key_as_a_constraint_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let mut seed = base_seed();
        seed.push(
            "INSERT INTO users (id,name,display_name,created_at,updated_at) VALUES ('u2','u2','Second user',1,1)".to_owned(),
        );
        seed.push(seed_agent(
            "existing",
            "pending_materialization",
            1,
            Some("idem-1"),
        ));
        let path = seed_schema(&dir, "reserve-idem.db", &seed).await;
        let store = SqliteHostAgentStore::open(&path).await.unwrap();

        // A fresh handle but the same `(u1, idem-1)` trips the per-creator index.
        assert_eq!(
            reserve_agent(&store, &input("a2", "idem-1"), 42)
                .await
                .unwrap(),
            ReserveOutcome::ConstraintConflict
        );
        let mut other_creator = input("a3", "idem-1");
        other_creator.created_by_user_id = user("u2");
        assert!(matches!(
            reserve_agent(&store, &other_creator, 42).await.unwrap(),
            ReserveOutcome::Reserved(_)
        ));
    }

    #[tokio::test]
    async fn fail_flips_a_pending_row_and_reflips_a_failed_one() {
        let dir = tempfile::tempdir().unwrap();
        let mut seed = base_seed();
        seed.push(seed_agent(
            "a1",
            "pending_materialization",
            1,
            Some("idem-1"),
        ));
        let path = seed_schema(&dir, "fail.db", &seed).await;
        let store = SqliteHostAgentStore::open(&path).await.unwrap();

        let FailOutcome::Failed(failed) =
            fail_materialization(&store, &agent("a1"), "boom", 10, 11)
                .await
                .unwrap()
        else {
            panic!("expected a failed transition");
        };
        assert_eq!(failed.failed_code(), "boom");
        assert_eq!(failed.failed_at(), 10);
        assert_eq!(failed.updated_at(), 11);

        // failed -> failed re-stamps (the guard admits an already-failed row).
        let FailOutcome::Failed(refailed) =
            fail_materialization(&store, &agent("a1"), "again", 20, 21)
                .await
                .unwrap()
        else {
            panic!("expected a re-stamped failure");
        };
        assert_eq!(refailed.failed_code(), "again");
        assert_eq!(refailed.failed_at(), 20);
    }

    #[tokio::test]
    async fn fail_is_lost_against_an_active_or_absent_row() {
        let dir = tempfile::tempdir().unwrap();
        let mut seed = base_seed();
        seed.push(seed_agent("a1", "active", 1, Some("idem-1")));
        let path = seed_schema(&dir, "fail-lost.db", &seed).await;
        let store = SqliteHostAgentStore::open(&path).await.unwrap();

        assert_eq!(
            fail_materialization(&store, &agent("a1"), "boom", 10, 11)
                .await
                .unwrap(),
            FailOutcome::Lost
        );
        assert_eq!(
            fail_materialization(&store, &agent("missing"), "boom", 10, 11)
                .await
                .unwrap(),
            FailOutcome::Lost
        );
    }

    #[tokio::test]
    async fn reopen_flips_a_failed_row_and_clears_the_failure() {
        let dir = tempfile::tempdir().unwrap();
        let mut seed = base_seed();
        // A failed row carrying a stale failure stamp, to prove reopen clears it.
        seed.push(
            "INSERT INTO agents \
             (id, name, display_name, runtime, host_id, created_by_user_id, created_at, \
              updated_at, materialization_status, session_scope, idempotency_key, \
              materialization_failed_code, materialization_failed_at) \
             VALUES ('a1', 'a1', 'a1', 'claude', 'h1', 'u1', 1, 1, 'failed', 'conversation', \
                     'idem-1', 'stale', 9)"
                .to_string(),
        );
        let path = seed_schema(&dir, "reopen.db", &seed).await;
        let store = SqliteHostAgentStore::open(&path).await.unwrap();

        let ReopenOutcome::Reopened(reopened) =
            reopen_failed_materialization(&store, &agent("a1"), 30)
                .await
                .unwrap()
        else {
            panic!("expected a reopened reservation");
        };
        assert_eq!(reopened.updated_at(), 30);

        let hit = find_idempotent_reservation(&store, &user("u1"), &idem("idem-1"))
            .await
            .unwrap()
            .expect("a hit");
        assert_eq!(
            hit.materialization_status(),
            MaterializationStatus::PendingMaterialization
        );
        assert_eq!(hit.materialization_failed_code(), None);
        assert_eq!(hit.materialization_failed_at(), None);
    }

    #[tokio::test]
    async fn reopen_is_lost_against_pending_active_or_absent() {
        let dir = tempfile::tempdir().unwrap();
        for status in ["pending_materialization", "active"] {
            let mut seed = base_seed();
            seed.push(seed_agent("a1", status, 1, Some("idem-1")));
            let path = seed_schema(&dir, &format!("reopen-{status}.db"), &seed).await;
            let store = SqliteHostAgentStore::open(&path).await.unwrap();
            assert_eq!(
                reopen_failed_materialization(&store, &agent("a1"), 30)
                    .await
                    .unwrap(),
                ReopenOutcome::Lost
            );
        }

        let path = seed_schema(&dir, "reopen-missing.db", &base_seed()).await;
        let store = SqliteHostAgentStore::open(&path).await.unwrap();
        assert_eq!(
            reopen_failed_materialization(&store, &agent("missing"), 30)
                .await
                .unwrap(),
            ReopenOutcome::Lost
        );
    }

    #[tokio::test]
    async fn stale_sweep_fails_only_the_old_pending_rows() {
        let dir = tempfile::tempdir().unwrap();
        let mut seed = base_seed();
        seed.push(seed_agent(
            "old-pending",
            "pending_materialization",
            5,
            Some("k-old"),
        ));
        seed.push(seed_agent(
            "new-pending",
            "pending_materialization",
            50,
            Some("k-new"),
        ));
        seed.push(seed_agent("old-active", "active", 5, Some("k-active")));
        seed.push(seed_agent(
            "at-cutoff",
            "pending_materialization",
            25,
            Some("k-cutoff"),
        ));
        seed.push(seed_agent("old-failed", "failed", 5, Some("k-failed")));
        let path = seed_schema(&dir, "stale.db", &seed).await;
        let store = SqliteHostAgentStore::open(&path).await.unwrap();

        // cutoff strictly greater than old (5), strictly not greater than new (50).
        let swept = mark_stale_materializations_failed(&store, 25, 25, 26)
            .await
            .unwrap();
        assert_eq!(swept, vec![agent("old-pending")]);
        let boundary = find_idempotent_reservation(&store, &user("u1"), &idem("k-cutoff"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            boundary.materialization_status(),
            MaterializationStatus::PendingMaterialization
        );

        // The new pending row is untouched.
        let new_hit = find_idempotent_reservation(&store, &user("u1"), &idem("k-new"))
            .await
            .unwrap()
            .expect("a hit");
        assert_eq!(
            new_hit.materialization_status(),
            MaterializationStatus::PendingMaterialization
        );

        // The swept row carries the canonical code.
        let old_hit = find_idempotent_reservation(&store, &user("u1"), &idem("k-old"))
            .await
            .unwrap()
            .expect("a hit");
        assert_eq!(
            old_hit.materialization_status(),
            MaterializationStatus::Failed
        );
        assert_eq!(
            old_hit.materialization_failed_code(),
            Some("backend_crash_during_materialize")
        );
        assert_eq!(old_hit.materialization_failed_at(), Some(25));
    }

    /// A pending agent-scoped row of the given id, seeded with the given create timestamp so
    /// the activation flip can win it and the direct line it opens can ride the row's OWN
    /// `created_at`.
    fn seed_scoped_agent(id: &str, status: &str, scope: &str, created_at: i64) -> String {
        format!(
            "INSERT INTO agents \
             (id, name, display_name, runtime, host_id, created_by_user_id, created_at, \
              updated_at, materialization_status, session_scope) \
             VALUES ('{id}', '{id}', '{id}', 'claude', 'h1', 'u1', {created_at}, {created_at}, \
                     '{status}', '{scope}')"
        )
    }

    /// A direct line between the very same participants provisioning would open (agent `a1`
    /// and its creator `u1`), so get-or-create resolves it rather than minting the candidate.
    const EXISTING_PAIR: &str = "INSERT INTO conversations \
         (id, placement_kind, roster_policy, title, endpoint_a_kind, endpoint_a_id, \
          endpoint_b_kind, endpoint_b_id, created_at, updated_at) \
         VALUES ('existing-pair', 'direct_pair', 'fixed', 'DM', 'agent', 'a1', 'human', \
                 'u1', 1, 1)";

    /// A message already holding the id the onboarding write will mint, so the dependent insert
    /// collides on the primary key only after the flip has won.
    const CLASH_MESSAGE: &str = "INSERT INTO messages \
         (id, conversation_id, from_kind, content, seq, created_at) \
         VALUES ('msg-onboard', 'existing-pair', 'system', 'prior', 1, 1)";

    #[tokio::test]
    async fn activation_provisions_the_agent_arm_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let mut seed = base_seed();
        // Reserve clock 10, activation clock 99 — the discriminator that pins WHICH clock each
        // dependent fact rides: the direct line is the row's own artefact (created_at 10), the
        // activation is the flip's stamp (updated_at 99).
        seed.push(seed_scoped_agent(
            "a1",
            "pending_materialization",
            "agent",
            10,
        ));
        let path = seed_schema(&dir, "activate.db", &seed).await;
        let store = SqliteHostAgentStore::open(&path).await.unwrap();

        let outcome = provision_and_activate(
            &store,
            &agent("a1"),
            ConversationId::parse("pair-new").unwrap(),
            ConversationId::parse("dm-unused").unwrap(),
            MessageId::parse("msg-onboard").unwrap(),
            99,
        )
        .await
        .unwrap();

        let ProvisionedActivation::Agent(fact) = outcome else {
            panic!("expected a provisioned agent arm");
        };
        assert_eq!(fact.agent_id(), &agent("a1"));
        assert_eq!(
            fact.direct_line(),
            &ConversationId::parse("pair-new").unwrap()
        );
        assert_eq!(
            fact.onboarding_message_id(),
            &MessageId::parse("msg-onboard").unwrap()
        );
        assert!(fact.pair_created());

        // The row is active, stamped with the request clock.
        let status: String =
            sqlx::query_scalar("SELECT materialization_status FROM agents WHERE id = 'a1'")
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(status, "active");
        let updated_at: i64 = sqlx::query_scalar("SELECT updated_at FROM agents WHERE id = 'a1'")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(updated_at, 99);

        // P0-2 discriminator: the freshly minted direct line rides the agent row's OWN
        // created_at (10), NOT the activation clock (99). Both pair timestamps are the row's
        // created_at — the line is a durable artefact of the row, retry-stable — while the
        // agent row's updated_at alone carries the activation stamp.
        let (pair_created_at, pair_updated_at): (i64, i64) = sqlx::query_as(
            "SELECT created_at, updated_at FROM conversations WHERE id = 'pair-new'",
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(
            pair_created_at, 10,
            "direct line rides the row's created_at, not the flip clock"
        );
        assert_eq!(
            pair_updated_at, 10,
            "direct line updated_at is the row's created_at too"
        );

        // The onboarding system message landed on the freshly minted line as its first ordinal.
        let from_kind: String =
            sqlx::query_scalar("SELECT from_kind FROM messages WHERE id = 'msg-onboard'")
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(from_kind, "system");
        // The onboarding message, by contrast, rides the activation clock (99): it is a freshly
        // authored event, not an artefact of the row, so its created_at is the flip's now_ms.
        let message_created_at: i64 =
            sqlx::query_scalar("SELECT created_at FROM messages WHERE id = 'msg-onboard'")
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(message_created_at, 99);
        let seq: i64 = sqlx::query_scalar("SELECT seq FROM messages WHERE id = 'msg-onboard'")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(seq, 1);
        let conversation: String =
            sqlx::query_scalar("SELECT conversation_id FROM messages WHERE id = 'msg-onboard'")
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(conversation, "pair-new");
    }

    #[tokio::test]
    async fn activation_is_lost_and_writes_nothing_against_a_non_pending_row() {
        let dir = tempfile::tempdir().unwrap();
        for status in ["active", "failed"] {
            let mut seed = base_seed();
            seed.push(seed_scoped_agent("a1", status, "agent", 1));
            let path = seed_schema(&dir, &format!("lost-{status}.db"), &seed).await;
            let store = SqliteHostAgentStore::open(&path).await.unwrap();

            let outcome = provision_and_activate(
                &store,
                &agent("a1"),
                ConversationId::parse("pair-new").unwrap(),
                ConversationId::parse("dm-unused").unwrap(),
                MessageId::parse("msg-onboard").unwrap(),
                99,
            )
            .await
            .unwrap();
            assert_eq!(outcome, ProvisionedActivation::Lost);

            // The guarded flip changed no row: the status is untouched, no line was minted, no
            // onboarding message written, and no compensating fail was stamped.
            let after: String =
                sqlx::query_scalar("SELECT materialization_status FROM agents WHERE id = 'a1'")
                    .fetch_one(store.pool())
                    .await
                    .unwrap();
            assert_eq!(after, status);
            let convs: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM conversations WHERE id = 'pair-new'")
                    .fetch_one(store.pool())
                    .await
                    .unwrap();
            assert_eq!(convs, 0);
            let msgs: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE id = 'msg-onboard'")
                    .fetch_one(store.pool())
                    .await
                    .unwrap();
            assert_eq!(msgs, 0);
        }

        // An absent row is likewise `Lost`, with nothing written.
        let path = seed_schema(&dir, "lost-absent.db", &base_seed()).await;
        let store = SqliteHostAgentStore::open(&path).await.unwrap();
        let outcome = provision_and_activate(
            &store,
            &agent("missing"),
            ConversationId::parse("pair-new").unwrap(),
            ConversationId::parse("dm-unused").unwrap(),
            MessageId::parse("msg-onboard").unwrap(),
            99,
        )
        .await
        .unwrap();
        assert_eq!(outcome, ProvisionedActivation::Lost);
    }

    #[tokio::test]
    async fn a_dependent_graph_failure_rolls_back_and_stamps_a_separate_fail() {
        let dir = tempfile::tempdir().unwrap();
        let mut seed = base_seed();
        seed.push(seed_scoped_agent(
            "a1",
            "pending_materialization",
            "agent",
            1,
        ));
        // The line already exists and already holds the id the onboarding write will mint, so the
        // dependent insert collides only after the flip has won.
        seed.push(EXISTING_PAIR.to_string());
        seed.push(CLASH_MESSAGE.to_string());
        let path = seed_schema(&dir, "dependent-fail.db", &seed).await;
        let store = SqliteHostAgentStore::open(&path).await.unwrap();

        let fault = provision_and_activate(
            &store,
            &agent("a1"),
            ConversationId::parse("pair-new").unwrap(),
            ConversationId::parse("dm-unused").unwrap(),
            MessageId::parse("msg-onboard").unwrap(),
            99,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(fault, MaterializationFault::Persistence(_)),
            "expected a dependent-write persistence fault, got {fault:?}"
        );

        // The whole transaction rolled back — had the flip stuck as `active`, the compensating
        // fail (guarded to pending only) would have been Lost and the status stayed `active`.
        // Its landing as `failed` with the dependent code proves rollback AND compensation.
        let status: String =
            sqlx::query_scalar("SELECT materialization_status FROM agents WHERE id = 'a1'")
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(status, "failed");
        let code: Option<String> =
            sqlx::query_scalar("SELECT materialization_failed_code FROM agents WHERE id = 'a1'")
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(code.as_deref(), Some(DEPENDENT_GRAPH_FAILED_CODE));

        // The pre-existing line still holds exactly its one seeded message; the candidate line
        // was never minted (get-or-create resolved the existing one).
        let existing_msgs: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM messages WHERE conversation_id = 'existing-pair'",
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(existing_msgs, 1);
        let candidate: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM conversations WHERE id = 'pair-new'")
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(candidate, 0);
    }

    #[tokio::test]
    async fn the_compensating_fail_stamps_only_a_still_pending_row() {
        // The pending-only compensation CAS, exercised directly in both directions. It is the
        // fence that lets the core use case drive compensation after a rolled-back provisioning
        // without ever clobbering a concurrent transition that already reached a terminal state.
        let dir = tempfile::tempdir().unwrap();

        // Direction 1 — still pending: the compensation wins, stamping the dependent code and
        // the clocks it was handed, and returns the row it touched.
        {
            let mut seed = base_seed();
            seed.push(seed_scoped_agent(
                "a1",
                "pending_materialization",
                "agent",
                1,
            ));
            let path = seed_schema(&dir, "compensate-pending.db", &seed).await;
            let store = SqliteHostAgentStore::open(&path).await.unwrap();

            let touched = store
                .fail_after_rolled_back_provision(&agent("a1"), 500, 600)
                .await
                .unwrap();
            assert!(
                touched.is_some(),
                "a still-pending row must be stamped by the compensating fail"
            );

            let (status, code, failed_at, updated_at): (String, Option<String>, Option<i64>, i64) =
                sqlx::query_as(
                    "SELECT materialization_status, materialization_failed_code, \
                            materialization_failed_at, updated_at FROM agents WHERE id = 'a1'",
                )
                .fetch_one(store.pool())
                .await
                .unwrap();
            assert_eq!(status, "failed");
            assert_eq!(code.as_deref(), Some(DEPENDENT_GRAPH_FAILED_CODE));
            assert_eq!(failed_at, Some(500));
            assert_eq!(updated_at, 600);
        }

        // Direction 2a — a concurrent winner reached `active`: the CAS matches nothing, returns
        // None, and leaves the winner's status and clean failure code untouched.
        {
            let mut seed = base_seed();
            seed.push(
                "INSERT INTO agents \
                 (id, name, display_name, runtime, host_id, created_by_user_id, created_at, \
                  updated_at, materialization_status, session_scope) \
                 VALUES ('a1', 'a1', 'a1', 'claude', 'h1', 'u1', 1, 200, 'active', 'agent')"
                    .to_string(),
            );
            let path = seed_schema(&dir, "compensate-active-winner.db", &seed).await;
            let store = SqliteHostAgentStore::open(&path).await.unwrap();

            let touched = store
                .fail_after_rolled_back_provision(&agent("a1"), 500, 600)
                .await
                .unwrap();
            assert!(
                touched.is_none(),
                "an active winner must not be matched by the compensating fail"
            );
            let (status, code, updated_at): (String, Option<String>, i64) = sqlx::query_as(
                "SELECT materialization_status, materialization_failed_code, updated_at \
                   FROM agents WHERE id = 'a1'",
            )
            .fetch_one(store.pool())
            .await
            .unwrap();
            assert_eq!(status, "active", "the winner's status must survive");
            assert_eq!(code, None, "the winner's clean code must survive");
            assert_eq!(updated_at, 200, "the winner's clock must survive");
        }

        // Direction 2b — a concurrent winner already `failed` under a DIFFERENT code and clock:
        // the CAS still matches nothing (it is pending-only, NOT `failed`-restamping), so the
        // prior terminal failure is authoritative and is left exactly as it stood.
        {
            let mut seed = base_seed();
            seed.push(
                "INSERT INTO agents \
                 (id, name, display_name, runtime, host_id, created_by_user_id, created_at, \
                  updated_at, materialization_status, session_scope, \
                  materialization_failed_code, materialization_failed_at) \
                 VALUES ('a1', 'a1', 'a1', 'claude', 'h1', 'u1', 1, 250, 'failed', 'agent', \
                         'some_other_code', 250)"
                    .to_string(),
            );
            let path = seed_schema(&dir, "compensate-failed-winner.db", &seed).await;
            let store = SqliteHostAgentStore::open(&path).await.unwrap();

            let touched = store
                .fail_after_rolled_back_provision(&agent("a1"), 500, 600)
                .await
                .unwrap();
            assert!(
                touched.is_none(),
                "a row already failed under another code must not be restamped by graph compensation"
            );
            let (code, failed_at, updated_at): (Option<String>, Option<i64>, i64) = sqlx::query_as(
                "SELECT materialization_failed_code, materialization_failed_at, updated_at \
                   FROM agents WHERE id = 'a1'",
            )
            .fetch_one(store.pool())
            .await
            .unwrap();
            assert_eq!(
                code.as_deref(),
                Some("some_other_code"),
                "the prior terminal code is authoritative and must not be overwritten"
            );
            assert_eq!(failed_at, Some(250), "the prior failure clock must survive");
            assert_eq!(updated_at, 250, "the prior row clock must survive");
        }
    }

    #[tokio::test]
    async fn activation_provisions_the_conversation_arm_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let mut seed = base_seed();
        // A pending conversation-scoped reservation, created at clock 10; the activation fires at
        // 99. Every dependent fact of this arm — the home workspace, its DM, the roster edges,
        // and the onboarding message — rides the activation clock, so 99 is the discriminator.
        seed.push(seed_scoped_agent(
            "a1",
            "pending_materialization",
            "conversation",
            10,
        ));
        let path = seed_schema(&dir, "activate-conversation.db", &seed).await;
        let store = SqliteHostAgentStore::open(&path).await.unwrap();

        let outcome = provision_and_activate(
            &store,
            &agent("a1"),
            ConversationId::parse("pair-unused").unwrap(),
            ConversationId::parse("dm-onboard").unwrap(),
            MessageId::parse("msg-onboard").unwrap(),
            99,
        )
        .await
        .unwrap();

        let ProvisionedActivation::Conversation(fact) = outcome else {
            panic!("expected a provisioned conversation arm");
        };
        assert_eq!(fact.agent_id(), &agent("a1"));
        assert_eq!(fact.home_workspace(), &home_workspace_id_for(&agent("a1")));
        assert_eq!(
            fact.dm_channel(),
            &ConversationId::parse("dm-onboard").unwrap()
        );
        assert_eq!(
            fact.onboarding_message_id(),
            &MessageId::parse("msg-onboard").unwrap()
        );

        // The agent row is active, stamped with the activation clock.
        let (status, updated_at): (String, i64) =
            sqlx::query_as("SELECT materialization_status, updated_at FROM agents WHERE id = 'a1'")
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(status, "active");
        assert_eq!(updated_at, 99);

        // The home workspace: owned by the agent, its human-owner pointer the creator, a NULL
        // name_key (the arc CHECK), riding the activation clock.
        let (name, name_key, owner_agent, owner_user, ws_created): (
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            i64,
        ) = sqlx::query_as(
            "SELECT name, name_key, owner_agent_id, owner_user_id, created_at \
               FROM workspaces WHERE id = 'wsah_a1'",
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(name, "a1");
        assert_eq!(name_key, None);
        assert_eq!(owner_agent.as_deref(), Some("a1"));
        assert_eq!(owner_user.as_deref(), Some("u1"));
        assert_eq!(ws_created, 99);

        // The home DM: a fixed-roster, private workspace channel carrying the canonical title
        // and folded key, riding the activation clock.
        let (ws_id, placement, roster, title, title_key, visibility, dm_created): (
            Option<String>,
            String,
            String,
            String,
            Option<String>,
            String,
            i64,
        ) = sqlx::query_as(
            "SELECT workspace_id, placement_kind, roster_policy, title, title_key, \
                    visibility, created_at FROM conversations WHERE id = 'dm-onboard'",
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(ws_id.as_deref(), Some("wsah_a1"));
        assert_eq!(placement, "workspace");
        assert_eq!(roster, "fixed");
        assert_eq!(title, ONBOARDING_DM_TITLE);
        assert_eq!(title_key.as_deref(), Some(ONBOARDING_DM_TITLE_KEY));
        assert_eq!(visibility, "private");
        assert_eq!(dm_created, 99);

        // The three roster edges: the agent in its DM, the creator OWNER of the home workspace,
        // the creator in the DM.
        let agent_edge: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM conversation_agents \
               WHERE conversation_id = 'dm-onboard' AND agent_id = 'a1'",
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(agent_edge, 1);
        let owner_role: String = sqlx::query_scalar(
            "SELECT role FROM workspace_users WHERE workspace_id = 'wsah_a1' AND user_id = 'u1'",
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(owner_role, "owner");
        let user_edge: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM conversation_users \
               WHERE conversation_id = 'dm-onboard' AND user_id = 'u1'",
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(user_edge, 1);

        // The onboarding message landed on the DM as its first ordinal, riding the activation
        // clock.
        let (msg_conv, from_kind, msg_created, seq): (String, String, i64, i64) = sqlx::query_as(
            "SELECT conversation_id, from_kind, created_at, seq \
               FROM messages WHERE id = 'msg-onboard'",
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(msg_conv, "dm-onboard");
        assert_eq!(from_kind, "system");
        assert_eq!(msg_created, 99);
        assert_eq!(seq, 1);
    }

    #[tokio::test]
    async fn conversation_activation_is_lost_and_writes_nothing_against_a_non_pending_row() {
        let dir = tempfile::tempdir().unwrap();
        let mut seed = base_seed();
        seed.push(seed_scoped_agent("a1", "active", "conversation", 1));
        let path = seed_schema(&dir, "conversation-lost.db", &seed).await;
        let store = SqliteHostAgentStore::open(&path).await.unwrap();

        let outcome = provision_and_activate(
            &store,
            &agent("a1"),
            ConversationId::parse("pair-unused").unwrap(),
            ConversationId::parse("dm-onboard").unwrap(),
            MessageId::parse("msg-onboard").unwrap(),
            99,
        )
        .await
        .unwrap();
        assert_eq!(outcome, ProvisionedActivation::Lost);

        // The guarded flip changed no row: no home workspace, DM, or onboarding message written.
        let workspaces: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM workspaces WHERE id = 'wsah_a1'")
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(workspaces, 0);
        let convs: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM conversations WHERE id = 'dm-onboard'")
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(convs, 0);
        let msgs: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE id = 'msg-onboard'")
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(msgs, 0);
    }

    #[tokio::test]
    async fn the_raw_port_dispatches_the_arm_from_the_flipped_rows_own_scope() {
        // The caller does not choose the arm — the port reads the flipped row's OWN session_scope
        // inside the transaction and dispatches accordingly. The same call, differing only in the
        // seeded row's scope, produces different arms and touches different tables.
        let dir = tempfile::tempdir().unwrap();

        // An agent-scoped row yields the agent arm: a direct pair is minted, and NO home
        // workspace is created.
        {
            let mut seed = base_seed();
            seed.push(seed_scoped_agent(
                "a1",
                "pending_materialization",
                "agent",
                10,
            ));
            let path = seed_schema(&dir, "dispatch-agent.db", &seed).await;
            let store = SqliteHostAgentStore::open(&path).await.unwrap();

            let outcome = store
                .provision_and_activate(
                    &agent("a1"),
                    ConversationId::parse("pair-new").unwrap(),
                    ConversationId::parse("dm-unused").unwrap(),
                    MessageId::parse("msg-onboard").unwrap(),
                    99,
                )
                .await
                .unwrap();
            assert!(
                matches!(outcome, RawProvisionOutcome::Agent(_)),
                "an agent-scoped row must dispatch the agent arm, got {outcome:?}"
            );
            let pairs: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM conversations WHERE id = 'pair-new'")
                    .fetch_one(store.pool())
                    .await
                    .unwrap();
            assert_eq!(pairs, 1, "the agent arm mints the direct pair");
            let workspaces: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM workspaces WHERE id = 'wsah_a1'")
                    .fetch_one(store.pool())
                    .await
                    .unwrap();
            assert_eq!(workspaces, 0, "the agent arm writes no home workspace");
        }

        // A conversation-scoped row yields the conversation arm: a home workspace is created, and
        // the candidate direct pair is never minted.
        {
            let mut seed = base_seed();
            seed.push(seed_scoped_agent(
                "a1",
                "pending_materialization",
                "conversation",
                10,
            ));
            let path = seed_schema(&dir, "dispatch-conversation.db", &seed).await;
            let store = SqliteHostAgentStore::open(&path).await.unwrap();

            let outcome = store
                .provision_and_activate(
                    &agent("a1"),
                    ConversationId::parse("pair-unused").unwrap(),
                    ConversationId::parse("dm-onboard").unwrap(),
                    MessageId::parse("msg-onboard").unwrap(),
                    99,
                )
                .await
                .unwrap();
            assert!(
                matches!(outcome, RawProvisionOutcome::Conversation(_)),
                "a conversation-scoped row must dispatch the conversation arm, got {outcome:?}"
            );
            let workspaces: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM workspaces WHERE id = 'wsah_a1'")
                    .fetch_one(store.pool())
                    .await
                    .unwrap();
            assert_eq!(
                workspaces, 1,
                "the conversation arm mints the home workspace"
            );
            let pairs: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM conversations WHERE id = 'pair-unused'")
                    .fetch_one(store.pool())
                    .await
                    .unwrap();
            assert_eq!(pairs, 0, "the conversation arm never mints the direct pair");
        }
    }

    #[tokio::test]
    async fn the_raw_port_rolls_a_dependent_failure_back_to_a_still_pending_row() {
        // The port's transaction is atomic and it does NOT itself stamp the failure — that is the
        // core use case's compensating step. On a dependent-write collision the whole transaction
        // rolls back: the flip is undone (the row is STILL pending, not `active`, not `failed`)
        // and zero dependent rows survive.
        let dir = tempfile::tempdir().unwrap();
        let mut seed = base_seed();
        seed.push(seed_scoped_agent(
            "a1",
            "pending_materialization",
            "agent",
            1,
        ));
        seed.push(EXISTING_PAIR.to_string());
        seed.push(CLASH_MESSAGE.to_string());
        let path = seed_schema(&dir, "raw-rollback.db", &seed).await;
        let store = SqliteHostAgentStore::open(&path).await.unwrap();

        let err = store
            .provision_and_activate(
                &agent("a1"),
                ConversationId::parse("pair-new").unwrap(),
                ConversationId::parse("dm-unused").unwrap(),
                MessageId::parse("msg-onboard").unwrap(),
                99,
            )
            .await
            .unwrap_err();
        let _ = err; // a PersistenceFailure — the caller (core) decides how to compensate.

        // The flip rolled back: the row is left exactly pending, with no failure stamp. Had the
        // port committed the flip before the dependent write failed, this would read `active`.
        let (status, code): (String, Option<String>) = sqlx::query_as(
            "SELECT materialization_status, materialization_failed_code FROM agents WHERE id = 'a1'",
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(status, "pending_materialization");
        assert_eq!(code, None);

        // Zero dependent rows survive: the candidate line was never minted and the pre-existing
        // line still holds exactly its one seeded message.
        let candidate: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM conversations WHERE id = 'pair-new'")
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(candidate, 0);
        let existing_msgs: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM messages WHERE conversation_id = 'existing-pair'",
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(existing_msgs, 1);
    }
}
