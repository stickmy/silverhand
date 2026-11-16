//! SQLite adapter for generation-bound runtime custody.
//!
//! Slot SQL, transaction modes, persistence decoding and runtime-event writes
//! stay here. Callers receive only typed core facts or `PersistenceFailure`.

use std::{path::Path, time::Duration};

use crate::foundation::agent_lifecycle_sql::agent_transition_clear;
use silverhand_backend_core::{
    foundation::identity::{AgentId, ConversationId, DaemonInstanceId},
    foundation::persistence::PersistenceFailure,
    host_runtime::runtime_credential::{
        ClaimRuntimeCustody, ClaimedRuntime, FinalizeRuntimeCustody, RuntimeCredentialHash,
        RuntimeCustody, RuntimeCustodyPort, RuntimeFinalStatus, RuntimeGeneration,
        RuntimePrincipal, RuntimeScope,
    },
};
use sqlx::{Row, SqliteConnection};
use thiserror::Error;

const ROOT_WRITE_BEGIN: &str = "BEGIN IMMEDIATE";

#[derive(Clone)]
pub struct SqliteRuntimeCustodyStore {
    pool: crate::foundation::database::SqliteDatabase,
}

#[derive(Debug, Error)]
pub enum SqliteRuntimeCustodyStoreError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
}

impl SqliteRuntimeCustodyStore {
    pub fn from_pool(pool: impl Into<crate::foundation::database::SqliteDatabase>) -> Self {
        let pool = pool.into();
        Self { pool }
    }

    pub async fn open(path: impl AsRef<Path>) -> Result<Self, SqliteRuntimeCustodyStoreError> {
        let pool =
            crate::foundation::database::store_pool(path.as_ref(), Duration::from_secs(5)).await?;
        Ok(Self { pool })
    }

    async fn claim_transaction(
        &self,
        claim: ClaimRuntimeCustody,
    ) -> Result<Option<ClaimedRuntime>, PersistenceFailure> {
        let mut transaction = self
            .pool
            .begin_with(ROOT_WRITE_BEGIN)
            .await
            .map_err(persistence)?;
        if !agent_transition_is_clear(&mut transaction, claim.scope().agent()).await? {
            transaction.commit().await.map_err(persistence)?;
            return Ok(None);
        }
        let Some(slot) = read_slot(&mut transaction, claim.scope()).await? else {
            transaction.commit().await.map_err(persistence)?;
            return Ok(None);
        };
        let slot = decode_slot(slot)?;

        if let Some(daemon) = slot.daemon_instance.as_ref()
            && daemon == claim.daemon_instance()
        {
            let principal = RuntimePrincipal::for_scope(
                claim.scope().clone(),
                RuntimeCustody::new(slot.generation, daemon.clone()),
            );
            insert_credential(
                &mut transaction,
                slot.id,
                slot.generation,
                claim.credential(),
            )
            .await?;
            transaction.commit().await.map_err(persistence)?;
            return Ok(Some(ClaimedRuntime::new(principal, false)));
        }

        let next_generation = slot
            .generation
            .checked_next()
            .map_err(PersistenceFailure::from_source)?;
        let updated = sqlx::query(concat!(
            "UPDATE agent_runtime_slots
                SET generation = ?,
                    claimed_by_daemon_instance_id = ?,
                    observed_status = 'starting',
                    observed_pid = NULL
              WHERE id = ? AND generation = ?
                AND EXISTS (SELECT 1 FROM agents WHERE id = ? AND ",
            agent_transition_clear!(""),
            ")
              RETURNING cli_session_id",
        ))
        .bind(next_generation.as_i64())
        .bind(claim.daemon_instance().as_str())
        .bind(slot.id)
        .bind(slot.generation.as_i64())
        .bind(claim.scope().agent().as_str())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(persistence)?
        .ok_or_else(|| PersistenceFailure::from_source(RuntimeSlotChangedDuringClaim))?;
        let cli_session_id: Option<String> =
            updated.try_get("cli_session_id").map_err(persistence)?;
        revoke_slot_credentials(&mut transaction, slot.id).await?;
        insert_credential(
            &mut transaction,
            slot.id,
            next_generation,
            claim.credential(),
        )
        .await?;

        insert_event(
            &mut transaction,
            EventRecord {
                slot_id: slot.id,
                scope: claim.scope(),
                generation: next_generation,
                event_kind: "generation_claimed",
                daemon_instance_id: Some(claim.daemon_instance().as_str()),
                observed_status: "starting",
                cli_session_id: cli_session_id.as_deref(),
                occurred_at: claim.observed_at().as_unix_millis(),
            },
        )
        .await?;
        transaction.commit().await.map_err(persistence)?;

        Ok(Some(ClaimedRuntime::new(
            RuntimePrincipal::for_scope(
                claim.scope().clone(),
                RuntimeCustody::new(next_generation, claim.daemon_instance().clone()),
            ),
            true,
        )))
    }

    async fn current_principal(
        &self,
        scope: RuntimeScope,
    ) -> Result<Option<RuntimePrincipal>, PersistenceFailure> {
        let mut connection = self.pool.acquire().await.map_err(persistence)?;
        let Some(slot) = read_slot(&mut connection, &scope).await? else {
            return Ok(None);
        };
        let slot = decode_slot(slot)?;
        let Some(daemon) = slot.daemon_instance else {
            return Ok(None);
        };
        Ok(Some(RuntimePrincipal::for_scope(
            scope,
            RuntimeCustody::new(slot.generation, daemon),
        )))
    }

    async fn verify_current(
        &self,
        principal: RuntimePrincipal,
    ) -> Result<bool, PersistenceFailure> {
        let scope = principal.scope();
        let mut connection = self.pool.acquire().await.map_err(persistence)?;
        let Some(slot) = read_slot(&mut connection, &scope).await? else {
            return Ok(false);
        };
        let slot = decode_slot(slot)?;
        Ok(slot.generation == principal.custody().generation()
            && slot.daemon_instance.as_ref() == Some(principal.custody().daemon_instance()))
    }

    async fn finalize_transaction(
        &self,
        finalization: FinalizeRuntimeCustody,
    ) -> Result<bool, PersistenceFailure> {
        let mut transaction = self
            .pool
            .begin_with(ROOT_WRITE_BEGIN)
            .await
            .map_err(persistence)?;
        let principal = finalization.principal();
        let scope = principal.scope();
        let status = match finalization.status() {
            RuntimeFinalStatus::Idle => "idle",
            RuntimeFinalStatus::Crashed => "crashed",
        };
        let Some(slot) = read_slot(&mut transaction, &scope).await? else {
            transaction.commit().await.map_err(persistence)?;
            return Ok(false);
        };
        let slot = decode_slot(slot)?;
        if slot.generation != principal.custody().generation()
            || slot.daemon_instance.as_ref() != Some(principal.custody().daemon_instance())
        {
            transaction.commit().await.map_err(persistence)?;
            return Ok(false);
        }
        let row = sqlx::query(
            "UPDATE agent_runtime_slots
                SET claimed_by_daemon_instance_id = NULL,
                    observed_status = ?,
                    observed_pid = NULL
              WHERE id = ?
                AND generation = ?
                AND claimed_by_daemon_instance_id = ?
              RETURNING cli_session_id",
        )
        .bind(status)
        .bind(slot.id)
        .bind(slot.generation.as_i64())
        .bind(principal.custody().daemon_instance().as_str())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(persistence)?
        .ok_or_else(|| PersistenceFailure::from_source(RuntimeSlotChangedDuringFinalize))?;
        let cli_session_id: Option<String> = row.try_get("cli_session_id").map_err(persistence)?;
        revoke_slot_credentials(&mut transaction, slot.id).await?;
        insert_event(
            &mut transaction,
            EventRecord {
                slot_id: slot.id,
                scope: &scope,
                generation: slot.generation,
                event_kind: "generation_finalized",
                daemon_instance_id: None,
                observed_status: status,
                cli_session_id: cli_session_id.as_deref(),
                occurred_at: finalization.observed_at().as_unix_millis(),
            },
        )
        .await?;
        transaction.commit().await.map_err(persistence)?;
        Ok(true)
    }

    async fn authenticate_credential(
        &self,
        credential: RuntimeCredentialHash,
    ) -> Result<Option<RuntimePrincipal>, PersistenceFailure> {
        let mut connection = self.pool.acquire().await.map_err(persistence)?;
        let row = sqlx::query(
            "SELECT s.agent_id, s.conversation_id, s.generation, s.claimed_by_daemon_instance_id
               FROM agent_runtime_credentials c
               JOIN agent_runtime_slots s ON s.id = c.slot_id
              WHERE c.token_hash = ?
                AND s.generation = c.generation
                AND s.claimed_by_daemon_instance_id IS NOT NULL",
        )
        .bind(credential.as_str())
        .fetch_optional(&mut *connection)
        .await
        .map_err(persistence)?;
        let Some(row) = row else { return Ok(None) };
        let agent = AgentId::parse(row.try_get::<String, _>("agent_id").map_err(persistence)?)
            .map_err(PersistenceFailure::from_source)?;
        let scope = match row
            .try_get::<Option<String>, _>("conversation_id")
            .map_err(persistence)?
        {
            Some(source) => RuntimeScope::ConversationSlot {
                agent,
                source: ConversationId::parse(source).map_err(PersistenceFailure::from_source)?,
            },
            None => RuntimeScope::AgentSlot { agent },
        };
        let slot = decode_slot(PersistedSlot {
            id: 0,
            generation: row.try_get("generation").map_err(persistence)?,
            claimed_by_daemon_instance_id: row
                .try_get("claimed_by_daemon_instance_id")
                .map_err(persistence)?,
        })?;
        Ok(slot.daemon_instance.map(|daemon| {
            RuntimePrincipal::for_scope(scope, RuntimeCustody::new(slot.generation, daemon))
        }))
    }

    async fn principal_holds_credential(
        &self,
        principal: RuntimePrincipal,
    ) -> Result<bool, PersistenceFailure> {
        let mut connection = self.pool.acquire().await.map_err(persistence)?;
        let Some(slot) = read_slot(&mut connection, &principal.scope()).await? else {
            return Ok(false);
        };
        let slot = decode_slot(slot)?;
        if slot.generation != principal.custody().generation()
            || slot.daemon_instance.as_ref() != Some(principal.custody().daemon_instance())
        {
            return Ok(false);
        }
        sqlx::query_scalar(
            "SELECT EXISTS (
               SELECT 1 FROM agent_runtime_credentials WHERE slot_id = ? AND generation = ?
             )",
        )
        .bind(slot.id)
        .bind(slot.generation.as_i64())
        .fetch_one(&mut *connection)
        .await
        .map_err(persistence)
    }
}

async fn insert_credential(
    connection: &mut SqliteConnection,
    slot_id: i64,
    generation: RuntimeGeneration,
    credential: &RuntimeCredentialHash,
) -> Result<(), PersistenceFailure> {
    sqlx::query(
        "INSERT INTO agent_runtime_credentials (token_hash, slot_id, generation) VALUES (?, ?, ?)",
    )
    .bind(credential.as_str())
    .bind(slot_id)
    .bind(generation.as_i64())
    .execute(&mut *connection)
    .await
    .map_err(persistence)?;
    Ok(())
}

/// Runs in the same transaction that ends or replaces a slot's custody.
pub(crate) async fn revoke_slot_credentials(
    connection: &mut SqliteConnection,
    slot_id: i64,
) -> Result<(), PersistenceFailure> {
    sqlx::query("DELETE FROM agent_runtime_credentials WHERE slot_id = ?")
        .bind(slot_id)
        .execute(&mut *connection)
        .await
        .map_err(persistence)?;
    Ok(())
}

async fn agent_transition_is_clear(
    connection: &mut SqliteConnection,
    agent: &silverhand_backend_core::foundation::identity::AgentId,
) -> Result<bool, PersistenceFailure> {
    let row = sqlx::query(
        "SELECT runtime_transition_target, runtime_transition_started_at \
           FROM agents WHERE id = ?",
    )
    .bind(agent.as_str())
    .fetch_optional(&mut *connection)
    .await
    .map_err(persistence)?;
    let Some(row) = row else { return Ok(false) };
    let target: Option<String> = row
        .try_get("runtime_transition_target")
        .map_err(persistence)?;
    let started_at: Option<i64> = row
        .try_get("runtime_transition_started_at")
        .map_err(persistence)?;
    Ok(target.is_none() && started_at.is_none())
}

impl RuntimeCustodyPort for SqliteRuntimeCustodyStore {
    async fn claim_or_current(
        &self,
        claim: ClaimRuntimeCustody,
    ) -> Result<Option<ClaimedRuntime>, PersistenceFailure> {
        self.claim_transaction(claim).await
    }

    async fn current(
        &self,
        scope: RuntimeScope,
    ) -> Result<Option<RuntimePrincipal>, PersistenceFailure> {
        self.current_principal(scope).await
    }

    async fn is_current(&self, principal: RuntimePrincipal) -> Result<bool, PersistenceFailure> {
        self.verify_current(principal).await
    }

    async fn finalize(
        &self,
        finalization: FinalizeRuntimeCustody,
    ) -> Result<bool, PersistenceFailure> {
        self.finalize_transaction(finalization).await
    }

    async fn authenticate(
        &self,
        credential: RuntimeCredentialHash,
    ) -> Result<Option<RuntimePrincipal>, PersistenceFailure> {
        self.authenticate_credential(credential).await
    }

    async fn holds_credential(
        &self,
        principal: RuntimePrincipal,
    ) -> Result<bool, PersistenceFailure> {
        self.principal_holds_credential(principal).await
    }

    async fn revoke_credential(
        &self,
        credential: RuntimeCredentialHash,
    ) -> Result<(), PersistenceFailure> {
        sqlx::query("DELETE FROM agent_runtime_credentials WHERE token_hash = ?")
            .bind(credential.as_str())
            .execute(&self.pool)
            .await
            .map_err(persistence)?;
        Ok(())
    }
}

struct PersistedSlot {
    id: i64,
    generation: i64,
    claimed_by_daemon_instance_id: Option<String>,
}

struct DecodedSlot {
    id: i64,
    generation: RuntimeGeneration,
    daemon_instance: Option<DaemonInstanceId>,
}

fn decode_slot(slot: PersistedSlot) -> Result<DecodedSlot, PersistenceFailure> {
    let generation = RuntimeGeneration::new(slot.generation)
        .map_err(|error| PersistenceFailure::from_source(InvalidPersistedGeneration(error)))?;
    let daemon_instance = slot
        .claimed_by_daemon_instance_id
        .map(DaemonInstanceId::parse)
        .transpose()
        .map_err(|error| PersistenceFailure::from_source(InvalidPersistedDaemonInstance(error)))?;
    Ok(DecodedSlot {
        id: slot.id,
        generation,
        daemon_instance,
    })
}

async fn read_slot(
    connection: &mut SqliteConnection,
    scope: &RuntimeScope,
) -> Result<Option<PersistedSlot>, PersistenceFailure> {
    let row = match scope {
        RuntimeScope::ConversationSlot { agent, source } => sqlx::query(
            "SELECT id, generation, claimed_by_daemon_instance_id
                   FROM agent_runtime_slots
                  WHERE agent_id = ? AND conversation_id = ?",
        )
        .bind(agent.as_str())
        .bind(source.as_str())
        .fetch_optional(&mut *connection)
        .await
        .map_err(persistence)?,
        RuntimeScope::AgentSlot { agent } => sqlx::query(
            "SELECT id, generation, claimed_by_daemon_instance_id
                   FROM agent_runtime_slots
                  WHERE agent_id = ? AND conversation_id IS NULL",
        )
        .bind(agent.as_str())
        .fetch_optional(&mut *connection)
        .await
        .map_err(persistence)?,
    };
    row.map(|row| {
        Ok(PersistedSlot {
            id: row.try_get("id").map_err(persistence)?,
            generation: row.try_get("generation").map_err(persistence)?,
            claimed_by_daemon_instance_id: row
                .try_get("claimed_by_daemon_instance_id")
                .map_err(persistence)?,
        })
    })
    .transpose()
}

struct EventRecord<'a> {
    slot_id: i64,
    scope: &'a RuntimeScope,
    generation: RuntimeGeneration,
    event_kind: &'static str,
    daemon_instance_id: Option<&'a str>,
    observed_status: &'static str,
    cli_session_id: Option<&'a str>,
    occurred_at: i64,
}

async fn insert_event(
    connection: &mut SqliteConnection,
    event: EventRecord<'_>,
) -> Result<(), PersistenceFailure> {
    let (agent_id, conversation_id) = match event.scope {
        RuntimeScope::ConversationSlot { agent, source } => (agent.as_str(), Some(source.as_str())),
        RuntimeScope::AgentSlot { agent } => (agent.as_str(), None),
    };
    sqlx::query(
        "INSERT INTO agent_runtime_events (
            slot_id, agent_id, conversation_id, generation, event_kind,
            daemon_instance_id, observed_status, observed_pid, cli_session_id, occurred_at
         ) VALUES (?, ?, ?, ?, ?, ?, ?, NULL, ?, ?)",
    )
    .bind(event.slot_id)
    .bind(agent_id)
    .bind(conversation_id)
    .bind(event.generation.as_i64())
    .bind(event.event_kind)
    .bind(event.daemon_instance_id)
    .bind(event.observed_status)
    .bind(event.cli_session_id)
    .bind(event.occurred_at)
    .execute(&mut *connection)
    .await
    .map_err(persistence)?;
    Ok(())
}

fn persistence(error: sqlx::Error) -> PersistenceFailure {
    PersistenceFailure::from_source(error)
}

#[derive(Debug, Error)]
#[error("persisted runtime generation is invalid")]
struct InvalidPersistedGeneration(
    #[source] silverhand_backend_core::host_runtime::runtime_credential::InvalidRuntimeGeneration,
);

#[derive(Debug, Error)]
#[error("persisted daemon instance id is invalid")]
struct InvalidPersistedDaemonInstance(
    #[source] silverhand_backend_core::foundation::identity::EmptyDaemonInstanceId,
);

#[derive(Debug, Error)]
#[error("runtime slot changed while its generation claim held the writer reservation")]
struct RuntimeSlotChangedDuringClaim;

#[derive(Debug, Error)]
#[error("runtime slot changed after exact custody was decoded under the writer reservation")]
struct RuntimeSlotChangedDuringFinalize;
