//! SQLite placement projection for the agent-profile MCP data plane.
//!
//! The agent id, owning host, and deletion marker come from one row so the C1
//! decision never combines coordinates from different lifecycle snapshots.

use silverhand_backend_core::{
    foundation::{identity::AgentId, persistence::PersistenceFailure},
    identity_access::agent_profile_mcp::{AgentProfileMcpPlacement, AgentProfileMcpPlacementRead},
};
use sqlx::{Row, sqlite::SqliteRow};

use crate::foundation::host_agent_store::{SqliteHostAgentStore, decode};

const SELECT_AGENT_MCP_PLACEMENT: &str =
    "SELECT id, host_id, deletion_started_at FROM agents WHERE id = ?";

fn agent_mcp_placement(row: &SqliteRow) -> Result<AgentProfileMcpPlacement, sqlx::Error> {
    Ok(AgentProfileMcpPlacement::new(
        decode::agent_id(row.try_get("id")?)?,
        decode::host_id(row.try_get("host_id")?)?,
        row.try_get::<Option<i64>, _>("deletion_started_at")?
            .is_some(),
    ))
}

impl AgentProfileMcpPlacementRead for SqliteHostAgentStore {
    async fn read_mcp_placement(
        &self,
        agent: &AgentId,
    ) -> Result<Option<AgentProfileMcpPlacement>, PersistenceFailure> {
        let row = sqlx::query(SELECT_AGENT_MCP_PLACEMENT)
            .bind(agent.as_str())
            .fetch_optional(self.pool())
            .await
            .map_err(PersistenceFailure::from_source)?;
        row.as_ref()
            .map(agent_mcp_placement)
            .transpose()
            .map_err(PersistenceFailure::from_source)
    }
}

#[cfg(test)]
mod tests {
    use sqlx::Connection;

    use silverhand_backend_core::foundation::identity::HostId;

    use super::*;

    fn agent(id: &str) -> AgentId {
        AgentId::parse(id).expect("agent id")
    }

    fn host(id: &str) -> HostId {
        HostId::parse(id).expect("host id")
    }

    #[tokio::test]
    async fn placement_sources_host_and_deletion_from_one_row() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profile-mcp-placement.db");
        let mut connection = crate::current_schema::create_current_schema(&path).await;
        for statement in [
            "INSERT INTO users (id, name, display_name, created_at, updated_at) \
             VALUES ('u1', 'u1', 'U1', 1, 1)",
            "INSERT INTO hosts (id, name, kind, created_at, user_id, provisioning_source) \
             VALUES ('h-good', 'h-good', 'remote', 1, 'u1', 'system')",
            seed_agent!(
                id = "a-agent",
                host_id = "h-good",
                created_by_user_id = "u1",
                session_scope = "agent",
            ),
        ] {
            sqlx::query(statement)
                .execute(&mut connection)
                .await
                .expect("seed statement");
        }
        connection.close().await.expect("close seed connection");
        let store = SqliteHostAgentStore::open(&path).await.expect("open store");

        let active = store
            .read_mcp_placement(&agent("a-agent"))
            .await
            .expect("read placement")
            .expect("agent present");
        assert_eq!(active.source(), &agent("a-agent"));
        assert_eq!(active.host(), &host("h-good"));
        assert!(!active.deletion_pending());

        sqlx::query("UPDATE agents SET deletion_started_at = 7 WHERE id = 'a-agent'")
            .execute(store.pool())
            .await
            .unwrap();
        let deleting = store
            .read_mcp_placement(&agent("a-agent"))
            .await
            .expect("read deleting placement")
            .expect("agent present");
        assert_eq!(deleting.source(), &agent("a-agent"));
        assert_eq!(deleting.host(), &host("h-good"));
        assert!(deleting.deletion_pending());

        assert!(
            store
                .read_mcp_placement(&agent("nope"))
                .await
                .expect("read missing")
                .is_none()
        );
    }
}
