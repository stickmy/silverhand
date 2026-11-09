//! Turning stored rows into the narrow facts the application teardown phases read.
//!
//! The pre-lock [`AgentTeardownReadPort`] answers hints outside any transaction;
//! the write paths call [`capture_agent`] and the `select_*` helpers INSIDE the one
//! write lock to take the authoritative facts and the post-commit capture before
//! the delete erases the rows they derive from. Runtime-session projection remains
//! a C2 responsibility and is consumed through its narrow connection-scoped helper.
//!
//! Each `select_*` helper takes a `&mut SqliteConnection`, which is what both a
//! pooled connection and a live transaction deref to, so the two audiences run the
//! identical statement without a second code path.

use silverhand_backend_core::{
    application_orchestration::teardown::{
        facts::{
            AgentRuntimeSessionFacts, AgentTeardownCapture, AgentTeardownFacts,
            ComputerTeardownFacts, StoredBlobRef,
        },
        read::AgentTeardownReadPort,
    },
    foundation::identity::AgentId,
    foundation::persistence::PersistenceFailure,
    identity_access::capability::computer_access::ComputerOwnershipFacts,
};
use sqlx::{Row, SqliteConnection, sqlite::SqliteRow};

use crate::{
    foundation::host_agent_store::{SqliteHostAgentStore, decode},
    host_runtime::agent_bulk_stop_read::select_runtime_sessions,
};

const SELECT_AGENT_FACTS: &str = "SELECT id, name, display_name, runtime, host_id, \
     materialization_status, materialization_failed_code, deletion_started_at \
     FROM agents WHERE id = ?";
const SELECT_AGENTS_ON_HOST: &str = "SELECT id, name, display_name, runtime, host_id, \
     materialization_status, materialization_failed_code, deletion_started_at \
     FROM agents WHERE host_id = ? ORDER BY id ASC";
const SELECT_IS_DELETION_PENDING: &str =
    "SELECT EXISTS(SELECT 1 FROM agents WHERE id = ? AND deletion_started_at IS NOT NULL)";
const SELECT_COMPUTER_OWNERSHIP: &str = "SELECT id, user_id FROM hosts WHERE id = ?";
const SELECT_COMPUTER_LIFECYCLE: &str =
    "SELECT id, kind, provisioning_source FROM hosts WHERE id = ?";
const SELECT_HOME_WORKSPACE: &str = "SELECT id FROM workspaces WHERE owner_agent_id = ?";
const COUNT_REGULAR_MEMBERSHIPS: &str = "SELECT count(*) FROM workspace_members WHERE agent_id = ?";
/// The home workspace's fixed-roster channels, ordered exactly as the summary log
/// reads them: onboarding first, then the agent's own dm channel, then the rest by
/// creation. Only the first id is logged, but the whole ordered set drives the blob
/// sweep so the order is fixed for both.
const SELECT_HOME_DM_CHANNELS: &str = "SELECT c.id \
       FROM conversations c \
       JOIN workspaces w ON w.id = c.workspace_id \
       LEFT JOIN agents owner ON owner.id = w.owner_agent_id \
      WHERE c.workspace_id = ? \
        AND c.roster_policy = 'fixed' \
      ORDER BY CASE \
          WHEN c.title = 'Onboarding' THEN 0 \
          WHEN owner.id IS NOT NULL AND c.title = 'dm-' || owner.name THEN 1 \
          ELSE 2 \
        END, c.created_at ASC";
const SELECT_CHANNEL_BLOBS: &str = "SELECT storage, storage_key FROM attachments \
     WHERE channel_id = ? ORDER BY created_at ASC, id ASC";

fn agent_facts(row: &SqliteRow) -> Result<AgentTeardownFacts, sqlx::Error> {
    Ok(AgentTeardownFacts {
        id: decode::agent_id(row.try_get("id")?)?,
        name: row.try_get("name")?,
        display_name: row.try_get("display_name")?,
        runtime: row.try_get("runtime")?,
        host_id: decode::host_id(row.try_get("host_id")?)?,
        materialization_status: decode::materialization_status(
            &row.try_get::<String, _>("materialization_status")?,
        )?,
        failed_code: row.try_get("materialization_failed_code")?,
        deletion_started_at: row.try_get("deletion_started_at")?,
    })
}

fn computer_ownership(row: &SqliteRow) -> Result<ComputerOwnershipFacts, sqlx::Error> {
    let owner = row
        .try_get::<Option<String>, _>("user_id")?
        .map(decode::user_id)
        .transpose()?;
    Ok(ComputerOwnershipFacts {
        id: decode::host_id(row.try_get("id")?)?,
        owner,
    })
}

fn computer_lifecycle(row: &SqliteRow) -> Result<ComputerTeardownFacts, sqlx::Error> {
    Ok(ComputerTeardownFacts {
        id: decode::host_id(row.try_get("id")?)?,
        kind: decode::host_kind(&row.try_get::<String, _>("kind")?)?,
        provisioning_source: decode::provisioning_source(
            &row.try_get::<String, _>("provisioning_source")?,
        )?,
    })
}

pub(super) async fn select_agent_facts(
    conn: &mut SqliteConnection,
    id: &AgentId,
) -> Result<Option<AgentTeardownFacts>, sqlx::Error> {
    let row = sqlx::query(SELECT_AGENT_FACTS)
        .bind(id.as_str())
        .fetch_optional(&mut *conn)
        .await?;
    row.as_ref().map(agent_facts).transpose()
}

pub(super) async fn select_agents_on_host(
    conn: &mut SqliteConnection,
    host_id: &silverhand_backend_core::foundation::identity::HostId,
) -> Result<Vec<AgentTeardownFacts>, sqlx::Error> {
    let rows = sqlx::query(SELECT_AGENTS_ON_HOST)
        .bind(host_id.as_str())
        .fetch_all(&mut *conn)
        .await?;
    rows.iter().map(agent_facts).collect()
}

pub(super) async fn select_is_deletion_pending(
    conn: &mut SqliteConnection,
    id: &AgentId,
) -> Result<bool, sqlx::Error> {
    let pending: i64 = sqlx::query_scalar(SELECT_IS_DELETION_PENDING)
        .bind(id.as_str())
        .fetch_one(&mut *conn)
        .await?;
    Ok(pending != 0)
}

pub(super) async fn select_computer_ownership(
    conn: &mut SqliteConnection,
    id: &silverhand_backend_core::foundation::identity::HostId,
) -> Result<Option<ComputerOwnershipFacts>, sqlx::Error> {
    let row = sqlx::query(SELECT_COMPUTER_OWNERSHIP)
        .bind(id.as_str())
        .fetch_optional(&mut *conn)
        .await?;
    row.as_ref().map(computer_ownership).transpose()
}

pub(super) async fn select_computer_lifecycle(
    conn: &mut SqliteConnection,
    id: &silverhand_backend_core::foundation::identity::HostId,
) -> Result<Option<ComputerTeardownFacts>, sqlx::Error> {
    let row = sqlx::query(SELECT_COMPUTER_LIFECYCLE)
        .bind(id.as_str())
        .fetch_optional(&mut *conn)
        .await?;
    row.as_ref().map(computer_lifecycle).transpose()
}

/// The post-commit facts, taken inside the write scope because the same
/// transaction deletes every row they derive from. The blob refs are why this
/// cannot be recomputed afterwards: the storage keys live on attachment rows that
/// cascade away with the agent's home workspace.
pub(super) async fn capture_agent(
    conn: &mut SqliteConnection,
    id: &AgentId,
) -> Result<AgentTeardownCapture, sqlx::Error> {
    let runtime_sessions = select_runtime_sessions(&mut *conn, id).await?;

    let home: Option<String> = sqlx::query_scalar(SELECT_HOME_WORKSPACE)
        .bind(id.as_str())
        .fetch_optional(&mut *conn)
        .await?;

    let mut dm_channels: Vec<String> = Vec::new();
    if let Some(home_id) = &home {
        let rows = sqlx::query(SELECT_HOME_DM_CHANNELS)
            .bind(home_id)
            .fetch_all(&mut *conn)
            .await?;
        for row in &rows {
            dm_channels.push(row.try_get("id")?);
        }
    }
    let dm_channel_id = dm_channels
        .first()
        .cloned()
        .map(decode::conversation_id)
        .transpose()?;

    let mut dm_blob_refs: Vec<StoredBlobRef> = Vec::new();
    for channel in &dm_channels {
        let rows = sqlx::query(SELECT_CHANNEL_BLOBS)
            .bind(channel)
            .fetch_all(&mut *conn)
            .await?;
        for row in &rows {
            dm_blob_refs.push(StoredBlobRef {
                storage: row.try_get("storage")?,
                storage_key: row.try_get("storage_key")?,
            });
        }
    }

    let regular_workspace_count: i64 = sqlx::query_scalar(COUNT_REGULAR_MEMBERSHIPS)
        .bind(id.as_str())
        .fetch_one(&mut *conn)
        .await?;
    let regular_workspace_count =
        u32::try_from(regular_workspace_count).map_err(decode::decode_error)?;

    Ok(AgentTeardownCapture {
        agent_id: id.clone(),
        runtime_sessions,
        dm_blob_refs,
        dm_channel_id,
        regular_workspace_count,
    })
}

impl AgentTeardownReadPort for SqliteHostAgentStore {
    async fn read_agent_facts(
        &self,
        agent_id: &AgentId,
    ) -> Result<Option<AgentTeardownFacts>, PersistenceFailure> {
        let mut conn = self
            .pool()
            .acquire()
            .await
            .map_err(PersistenceFailure::from_source)?;
        select_agent_facts(&mut conn, agent_id)
            .await
            .map_err(PersistenceFailure::from_source)
    }

    async fn is_deletion_pending(&self, agent_id: &AgentId) -> Result<bool, PersistenceFailure> {
        let mut conn = self
            .pool()
            .acquire()
            .await
            .map_err(PersistenceFailure::from_source)?;
        select_is_deletion_pending(&mut conn, agent_id)
            .await
            .map_err(PersistenceFailure::from_source)
    }

    async fn list_runtime_sessions(
        &self,
        agent_id: &AgentId,
    ) -> Result<Vec<AgentRuntimeSessionFacts>, PersistenceFailure> {
        let mut conn = self
            .pool()
            .acquire()
            .await
            .map_err(PersistenceFailure::from_source)?;
        select_runtime_sessions(&mut conn, agent_id)
            .await
            .map_err(PersistenceFailure::from_source)
    }

    async fn read_computer_ownership(
        &self,
        host_id: &silverhand_backend_core::foundation::identity::HostId,
    ) -> Result<Option<ComputerOwnershipFacts>, PersistenceFailure> {
        let mut conn = self
            .pool()
            .acquire()
            .await
            .map_err(PersistenceFailure::from_source)?;
        select_computer_ownership(&mut conn, host_id)
            .await
            .map_err(PersistenceFailure::from_source)
    }

    async fn read_computer_lifecycle(
        &self,
        host_id: &silverhand_backend_core::foundation::identity::HostId,
    ) -> Result<Option<ComputerTeardownFacts>, PersistenceFailure> {
        let mut conn = self
            .pool()
            .acquire()
            .await
            .map_err(PersistenceFailure::from_source)?;
        select_computer_lifecycle(&mut conn, host_id)
            .await
            .map_err(PersistenceFailure::from_source)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use silverhand_backend_core::{
        foundation::identity::HostId,
        host_runtime::computer::{HostKind, ProvisioningSource},
        host_runtime::runtime_credential::RuntimeScope,
        identity_access::agent_lifecycle::MaterializationStatus,
    };
    use sqlx::Connection;
    use tempfile::TempDir;

    use super::*;

    fn agent(id: &str) -> AgentId {
        AgentId::parse(id).expect("agent id")
    }

    fn host(id: &str) -> HostId {
        HostId::parse(id).expect("host id")
    }

    /// Materializes the checked-in current schema, applies the seed statements one
    /// at a time, closes the seeding connection, and returns the path so a test can
    /// open the store with whatever configuration it needs.
    async fn seed_schema(dir: &TempDir, name: &str, seed: &[&'static str]) -> PathBuf {
        let path = dir.path().join(name);
        let mut connection = crate::current_schema::create_current_schema(&path).await;
        for statement in seed {
            sqlx::query(*statement)
                .execute(&mut connection)
                .await
                .expect("seed statement");
        }
        connection.close().await.expect("close seed connection");
        path
    }

    /// A corrupt token reaches the caller as `sqlx::Error::Decode` wrapped in the
    /// port error, never as a default. Walks the source chain because the port
    /// hands the driver error over as `PersistenceFailure`'s source.
    fn is_decode_failure(error: &(dyn std::error::Error + 'static)) -> bool {
        let mut current: Option<&(dyn std::error::Error + 'static)> = Some(error);
        while let Some(err) = current {
            if let Some(sqlx_error) = err.downcast_ref::<sqlx::Error>()
                && matches!(sqlx_error, sqlx::Error::Decode(_))
            {
                return true;
            }
            current = err.source();
        }
        false
    }

    /// One agent per session scope, so both `RuntimeScope` arms are produced from
    /// real rows rather than a hand-built slot: `a-agent` gets its agent-scoped slot
    /// from the `agents_create_agent_runtime_slot` trigger, and `a-conv` gets its
    /// conversation-scoped slot from the `conversation_agents_create_runtime_slot`
    /// trigger once it joins the workspace conversation.
    async fn seed_read_fixture(dir: &TempDir) -> SqliteHostAgentStore {
        let path = seed_schema(
            dir,
            "read.db",
            &[
                "INSERT INTO users (id, name, display_name, created_at, updated_at) \
                 VALUES ('u1', 'u1', 'U1', 1, 1)",
                "INSERT INTO hosts (id, name, kind, created_at, user_id, provisioning_source) \
                 VALUES ('h-good', 'h-good', 'remote', 1, 'u1', 'system')",
                // hosts.kind carries no CHECK, so a corrupt token can be planted and
                // read back through the decoder.
                "INSERT INTO hosts (id, name, kind, created_at) \
                 VALUES ('h-bad', 'h-bad', 'orbital', 1)",
                "INSERT INTO agents \
                 (id, name, runtime, host_id, created_by_user_id, created_at, updated_at, \
                  materialization_status, session_scope) \
                 VALUES ('a-agent', 'a-agent', 'claude', 'h-good', 'u1', 1, 1, 'active', 'agent')",
                "INSERT INTO agents \
                 (id, name, runtime, host_id, created_by_user_id, created_at, updated_at, \
                  materialization_status, session_scope) \
                 VALUES ('a-conv', 'a-conv', 'claude', 'h-good', 'u1', 1, 1, 'active', \
                  'conversation')",
                "INSERT INTO workspaces (id, name, name_key, created_at, updated_at) \
                 VALUES ('w1', 'w1', 'w1', 1, 1)",
                "INSERT INTO conversations \
                 (id, workspace_id, placement_kind, roster_policy, title, title_key, \
                  created_at, updated_at) \
                 VALUES ('c1', 'w1', 'workspace', 'open', 'chan', 'chan', 1, 1)",
                "INSERT INTO conversation_agents (conversation_id, agent_id, joined_at) \
                 VALUES ('c1', 'a-conv', 1)",
            ],
        )
        .await;
        SqliteHostAgentStore::open(&path).await.expect("open store")
    }

    #[tokio::test]
    async fn agent_facts_project_the_stored_row() {
        let dir = tempfile::tempdir().unwrap();
        let store = seed_read_fixture(&dir).await;

        let facts = store
            .read_agent_facts(&agent("a-agent"))
            .await
            .expect("read facts")
            .expect("agent present");

        assert_eq!(facts.id, agent("a-agent"));
        assert_eq!(facts.host_id, host("h-good"));
        assert_eq!(facts.materialization_status, MaterializationStatus::Active);
    }

    #[tokio::test]
    async fn both_runtime_scope_arms_decode_from_real_rows() {
        let dir = tempfile::tempdir().unwrap();
        let store = seed_read_fixture(&dir).await;

        let agent_sessions = store
            .list_runtime_sessions(&agent("a-agent"))
            .await
            .expect("agent sessions");
        assert!(matches!(
            agent_sessions.as_slice(),
            [session] if matches!(&session.scope, RuntimeScope::AgentSlot { agent } if agent == &self::agent("a-agent"))
        ));

        let conversation_sessions = store
            .list_runtime_sessions(&agent("a-conv"))
            .await
            .expect("conversation sessions");
        assert!(matches!(
            conversation_sessions.as_slice(),
            [session] if matches!(
                &session.scope,
                RuntimeScope::ConversationSlot { agent, source }
                    if agent == &self::agent("a-conv") && source.as_str() == "c1"
            )
        ));
    }

    #[tokio::test]
    async fn a_valid_computer_lifecycle_decodes() {
        let dir = tempfile::tempdir().unwrap();
        let store = seed_read_fixture(&dir).await;

        let facts = store
            .read_computer_lifecycle(&host("h-good"))
            .await
            .expect("read lifecycle")
            .expect("host present");

        assert_eq!(facts.id, host("h-good"));
        assert_eq!(facts.kind, HostKind::Remote);
        assert_eq!(facts.provisioning_source, ProvisioningSource::System);
    }

    /// The decode-failure gate. `hosts.kind` is the only vehicle: `observed_status`
    /// and `provisioning_source` both carry a CHECK, so an out-of-set token is
    /// rejected at insert and can never be read back, while `hosts.kind` has no CHECK
    /// and so a corrupt value survives to reach the decoder. Read back through the
    /// port it becomes a `Decode` failure rather than a default.
    #[tokio::test]
    async fn a_corrupt_computer_kind_surfaces_as_a_read_failure() {
        let dir = tempfile::tempdir().unwrap();
        let store = seed_read_fixture(&dir).await;

        let failure = store
            .read_computer_lifecycle(&host("h-bad"))
            .await
            .expect_err("a corrupt token is not a lifecycle answer");

        assert!(
            is_decode_failure(&failure),
            "corrupt storage did not surface as a decode failure"
        );
    }
}
