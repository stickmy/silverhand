//! SQLite persistence for the remaining host-management faces.
//!
//! Update takes `BEGIN IMMEDIATE` before its authoritative read. Status reads the
//! computer and registrations on one deferred snapshot. Both reuse the pool held by
//! [`SqliteHostRuntimeStore`]; neither opens a second store or connection authority.

use silverhand_backend_core::{
    foundation::identity::{AgentId, HostId},
    foundation::persistence::PersistenceFailure,
    host_runtime::{
        status::{HostStatusAgentFact, HostStatusStoredFacts, HostStatusStoredReadModel},
        update::{UpdateComputerDecision, UpdateComputerTransaction, UpdateComputerUnitOfWork},
    },
    identity_access::agent_lifecycle::MaterializationStatus,
    identity_access::agents::runtime::RuntimeKind,
};
use sqlx::Row;
use thiserror::Error;

use super::{SqliteHostRuntimeStore, row_to_computer_record};

const SELECT_COMPUTER: &str = "SELECT id, name, kind, created_at, user_id, installation_id, \
     last_seen_at, activated_at, finalized_at, revoked_at, provisioning_source, display_label \
     FROM hosts WHERE id = ?";

const SELECT_STATUS_AGENTS: &str = "SELECT id, host_id, name, display_name, runtime, \
     model, reasoning_effort, fast_mode, avatar_storage_key, updated_at, materialization_status, created_at \
     FROM agents WHERE host_id = ? ORDER BY created_at ASC";

#[derive(Debug, Error)]
enum HostManagementStoreError {
    #[error("computer disappeared while its update transaction held the write lock")]
    ComputerVanished,
    #[error("stored agent identity is malformed")]
    MalformedAgentIdentity,
    #[error("stored agent host identity is malformed")]
    MalformedHostIdentity,
    #[error("stored agent runtime is outside the closed set")]
    MalformedRuntime,
    #[error("stored materialization status is outside the closed set")]
    MalformedMaterializationStatus,
}

impl UpdateComputerUnitOfWork for SqliteHostRuntimeStore {
    async fn update_computer<D>(
        &self,
        computer: HostId,
        decide: D,
    ) -> Result<UpdateComputerTransaction, PersistenceFailure>
    where
        D: FnOnce(
                Option<silverhand_backend_core::host_runtime::computer::ComputerRecord>,
            ) -> UpdateComputerDecision
            + Send,
    {
        // The lock is acquired at BEGIN, before the read the decision consumes.
        let mut tx = self
            .begin_root_write()
            .await
            .map_err(PersistenceFailure::from_source)?;
        let row = crate::foundation::statement_metrics::statement(
            "hosts.get_by_id",
            "get",
            sqlx::query(SELECT_COMPUTER)
                .bind(computer.as_str())
                .fetch_optional(&mut *tx),
        )
        .await
        .map_err(PersistenceFailure::from_source)?;
        let record = row
            .as_ref()
            .map(row_to_computer_record)
            .transpose()
            .map_err(PersistenceFailure::from_source)?;

        let before = record.clone();
        let decision = decide(record);
        let result = match decision {
            UpdateComputerDecision::Refuse(_) => UpdateComputerTransaction::Unchanged { before },
            UpdateComputerDecision::Write {
                display_label,
                finalized_at,
            } => {
                crate::foundation::statement_metrics::statement(
                    "hosts.update_display_label",
                    "run",
                    sqlx::query(
                        "UPDATE hosts SET display_label = ?, \
                     finalized_at = COALESCE(finalized_at, ?) WHERE id = ?",
                    )
                    .bind(display_label)
                    .bind(finalized_at)
                    .bind(computer.as_str())
                    .execute(&mut *tx),
                )
                .await
                .map_err(PersistenceFailure::from_source)?;
                let row = crate::foundation::statement_metrics::statement(
                    "hosts.get_by_id",
                    "get",
                    sqlx::query(SELECT_COMPUTER)
                        .bind(computer.as_str())
                        .fetch_optional(&mut *tx),
                )
                .await
                .map_err(PersistenceFailure::from_source)?
                .ok_or_else(|| {
                    PersistenceFailure::from_source(HostManagementStoreError::ComputerVanished)
                })?;
                let committed =
                    row_to_computer_record(&row).map_err(PersistenceFailure::from_source)?;
                UpdateComputerTransaction::Committed {
                    before: before.ok_or_else(|| {
                        PersistenceFailure::from_source(HostManagementStoreError::ComputerVanished)
                    })?,
                    committed,
                }
            }
        };
        tx.commit().await.map_err(PersistenceFailure::from_source)?;
        Ok(result)
    }
}

impl HostStatusStoredReadModel for SqliteHostRuntimeStore {
    async fn read_host_status(
        &self,
        computer: &HostId,
    ) -> Result<Option<HostStatusStoredFacts>, PersistenceFailure> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(PersistenceFailure::from_source)?;
        let row = crate::foundation::statement_metrics::statement(
            "hosts.get_by_id",
            "get",
            sqlx::query(SELECT_COMPUTER)
                .bind(computer.as_str())
                .fetch_optional(&mut *tx),
        )
        .await
        .map_err(PersistenceFailure::from_source)?;
        let Some(row) = row else {
            tx.commit().await.map_err(PersistenceFailure::from_source)?;
            return Ok(None);
        };
        let stored_computer =
            row_to_computer_record(&row).map_err(PersistenceFailure::from_source)?;
        let rows = crate::foundation::statement_metrics::statement(
            "hosts.list_status_agents",
            "all",
            sqlx::query(SELECT_STATUS_AGENTS)
                .bind(computer.as_str())
                .fetch_all(&mut *tx),
        )
        .await
        .map_err(PersistenceFailure::from_source)?;
        let agents = rows
            .iter()
            .map(decode_status_agent)
            .collect::<Result<Vec<_>, _>>()
            .map_err(PersistenceFailure::from_source)?;
        tx.commit().await.map_err(PersistenceFailure::from_source)?;
        Ok(Some(HostStatusStoredFacts::new(stored_computer, agents)))
    }
}

fn decode_status_agent(row: &sqlx::sqlite::SqliteRow) -> Result<HostStatusAgentFact, sqlx::Error> {
    let id = AgentId::parse(row.try_get::<String, _>("id")?).map_err(|_| {
        sqlx::Error::Decode(Box::new(HostManagementStoreError::MalformedAgentIdentity))
    })?;
    let computer = HostId::parse(row.try_get::<String, _>("host_id")?).map_err(|_| {
        sqlx::Error::Decode(Box::new(HostManagementStoreError::MalformedHostIdentity))
    })?;
    let runtime = RuntimeKind::parse(&row.try_get::<String, _>("runtime")?)
        .ok_or_else(|| sqlx::Error::Decode(Box::new(HostManagementStoreError::MalformedRuntime)))?;
    let materialization_status =
        MaterializationStatus::parse(&row.try_get::<String, _>("materialization_status")?)
            .map_err(|_| {
                sqlx::Error::Decode(Box::new(
                    HostManagementStoreError::MalformedMaterializationStatus,
                ))
            })?;
    Ok(HostStatusAgentFact {
        id,
        computer,
        name: row.try_get("name")?,
        display_name: row.try_get("display_name")?,
        runtime,
        model: row.try_get("model")?,
        reasoning_effort: row.try_get("reasoning_effort")?,
        fast_mode: row
            .try_get::<Option<i64>, _>("fast_mode")?
            .map(|value| value != 0),
        avatar_storage_key: row.try_get("avatar_storage_key")?,
        updated_at: row.try_get("updated_at")?,
        materialization_status,
    })
}

#[cfg(test)]
mod tests {
    use silverhand_backend_core::foundation::identity::UserId;
    use silverhand_backend_core::host_runtime::{
        computer::ComputerDisplayLabel,
        status::{HostStatusStoredReadModel, readable_host_status},
        update::{
            UpdateComputer, UpdateComputerCommand, UpdateComputerInput, UpdateComputerOutcome,
        },
    };
    use sqlx::{Connection, Executor, SqliteConnection, sqlite::SqliteConnectOptions};

    use super::*;

    const CURRENT_SCHEMA: &str =
        include_str!("../../../../../rust/test-fixtures/migration-baseline/schema-current.sql");

    async fn fixture() -> (tempfile::TempDir, SqliteHostRuntimeStore) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("management.sqlite");
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .foreign_keys(true);
        let mut db = SqliteConnection::connect_with(&options).await.unwrap();
        db.execute(sqlx::raw_sql(CURRENT_SCHEMA)).await.unwrap();
        db.execute(sqlx::raw_sql(
            concat!(
                "INSERT INTO users (id,name,display_name,created_at,updated_at) VALUES ('owner','owner','Owner',1,1); ",
                "INSERT INTO hosts (id,name,kind,user_id,created_at,activated_at,finalized_at,provisioning_source) VALUES ('host-1','stable','remote','owner',1,2,NULL,'user_invite'); ",
                seed_agent!(
                    id = "agent-1",
                    name = "helper",
                    display_name = "Helper",
                    runtime = "codex",
                    host_id = "host-1",
                    created_by_user_id = "owner",
                    created_at = 3,
                    updated_at = 4,
                ),
                "; ",
            ),
        ))
        .await
        .unwrap();
        db.close().await.unwrap();
        let store = SqliteHostRuntimeStore::open(&path).await.unwrap();
        (directory, store)
    }

    #[tokio::test]
    async fn update_persists_the_label_and_finalization_stamp() {
        let (_directory, store) = fixture().await;
        let outcome = UpdateComputer::new(store.clone())
            .execute(UpdateComputerCommand {
                computer: HostId::parse("host-1").unwrap(),
                requester: UserId::parse("owner").unwrap(),
                input: UpdateComputerInput::Valid {
                    display_label: Some(ComputerDisplayLabel::parse(" Studio ").unwrap()),
                },
                at: 7,
            })
            .await
            .unwrap();
        let UpdateComputerOutcome::Finalized(computer) = outcome else {
            panic!("the attached draft finalizes")
        };
        assert_eq!(computer.display_label.as_deref(), Some("Studio"));
        assert_eq!(computer.finalized_at, Some(7));
    }

    #[tokio::test]
    async fn status_resolves_computer_and_registrations_on_the_store_pool() {
        let (_directory, store) = fixture().await;
        let computer = HostId::parse("host-1").unwrap();
        let facts = store.read_host_status(&computer).await.unwrap().unwrap();
        let read = readable_host_status(&UserId::parse("owner").unwrap(), &computer, Some(&facts))
            .unwrap()
            .unwrap();
        assert_eq!(read.computer.name, "stable");
        assert_eq!(read.agents.len(), 1);
        assert_eq!(read.agents[0].id.as_str(), "agent-1");
        assert_eq!(read.agents[0].runtime, RuntimeKind::Codex);
    }
}
