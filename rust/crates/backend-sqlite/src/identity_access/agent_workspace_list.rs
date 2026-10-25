//! The agent-scoped workspace-list read over `workspaces` + `workspace_members`.
//!
//! Membership union, mirroring the reference `listForAgent`: the regular
//! workspaces the agent is a member of (`workspace_members`, `owner_agent_id IS
//! NULL`) plus the agent's OWN agent-home (`owner_agent_id = <agent>`). The
//! per-workspace `agent_dir` is NOT read here — it is a daemon fact the route
//! stamps.

use silverhand_backend_core::{
    foundation::identity::{AgentId, WorkspaceId},
    foundation::persistence::PersistenceFailure,
    identity_access::agent_workspace_list::{AgentListedWorkspace, AgentWorkspaceListRead},
};
use sqlx::Row;
use thiserror::Error;

use crate::conversation_messaging::messaging::MessagingStore;

/// The agent workspace-list read authority — a shared-pool clone of the ONE
/// messaging store pool.
#[derive(Clone)]
pub struct SqliteAgentWorkspaceList {
    pool: crate::foundation::database::SqliteDatabase,
}

impl<E> MessagingStore<E> {
    /// The agent workspace-list read authority, sharing this store's pool.
    pub fn agent_workspace_list(&self) -> SqliteAgentWorkspaceList {
        SqliteAgentWorkspaceList {
            pool: self.pool().clone(),
        }
    }
}

impl SqliteAgentWorkspaceList {
    pub fn from_pool(pool: impl Into<crate::foundation::database::SqliteDatabase>) -> Self {
        let pool = pool.into();
        Self { pool }
    }
}

#[derive(Debug, Error)]
pub enum AgentWorkspaceListError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("stored workspace identity {field} is malformed")]
    MalformedIdentity { field: &'static str },
}

impl From<AgentWorkspaceListError> for PersistenceFailure {
    fn from(value: AgentWorkspaceListError) -> Self {
        Self::from_source(value)
    }
}

/// The membership union, mirroring `listForAgent`: regular workspaces the agent is
/// a member of, then the agent's own agent-home. Ordered `(owner_agent_id IS NULL
/// Regular workspaces (member) as `sort_group=0`, the agent's own home as
/// `sort_group=1`, then `ORDER BY sort_group ASC, sort_name COLLATE NOCASE ASC` —
/// the exact total order of the reference `WorkspaceRepo.listForAgent` (regular
/// before own home, each group by name case-insensitively). `sort_name` is `w.name`.
/// `UNION ALL` (not `UNION`): the two arms are disjoint by construction (a regular
/// membership row has `owner_agent_id IS NULL`, the home arm requires it non-null),
/// and the reference uses `UNION ALL`.
const SELECT_AGENT_WORKSPACES: &str = "\
    SELECT w.id AS id, w.name AS name, w.owner_agent_id AS owner_agent_id, \
           w.archived_at AS archived_at, w.updated_at AS updated_at, \
           0 AS sort_group, w.name AS sort_name \
      FROM workspaces w \
      JOIN workspace_members wm ON wm.workspace_id = w.id \
     WHERE wm.agent_id = ? AND w.owner_agent_id IS NULL \
    UNION ALL \
    SELECT w.id AS id, w.name AS name, w.owner_agent_id AS owner_agent_id, \
           w.archived_at AS archived_at, w.updated_at AS updated_at, \
           1 AS sort_group, w.name AS sort_name \
      FROM workspaces w \
     WHERE w.owner_agent_id = ? \
    ORDER BY sort_group ASC, sort_name COLLATE NOCASE ASC";

impl AgentWorkspaceListRead for SqliteAgentWorkspaceList {
    async fn list_agent_workspaces(
        &self,
        agent: &AgentId,
    ) -> Result<Vec<AgentListedWorkspace>, PersistenceFailure> {
        let rows = sqlx::query(SELECT_AGENT_WORKSPACES)
            .bind(agent.as_str())
            .bind(agent.as_str())
            .fetch_all(&self.pool)
            .await
            .map_err(AgentWorkspaceListError::from)?;
        let mut workspaces = Vec::with_capacity(rows.len());
        for row in &rows {
            let id: String = row.try_get("id").map_err(AgentWorkspaceListError::from)?;
            let id =
                WorkspaceId::parse(id).map_err(|_| AgentWorkspaceListError::MalformedIdentity {
                    field: "workspaces.id",
                })?;
            let owner_agent_id: Option<String> = row
                .try_get("owner_agent_id")
                .map_err(AgentWorkspaceListError::from)?;
            let owner_agent_id = match owner_agent_id {
                None => None,
                Some(raw) => Some(AgentId::parse(raw).map_err(|_| {
                    AgentWorkspaceListError::MalformedIdentity {
                        field: "workspaces.owner_agent_id",
                    }
                })?),
            };
            let archived_at: Option<i64> = row
                .try_get("archived_at")
                .map_err(AgentWorkspaceListError::from)?;
            workspaces.push(AgentListedWorkspace {
                id,
                name: row.try_get("name").map_err(AgentWorkspaceListError::from)?,
                owner_agent_id,
                archived: archived_at.is_some(),
                updated_at: silverhand_backend_core::foundation::wire_number::JsSafeInteger::new(
                    row.try_get("updated_at")
                        .map_err(AgentWorkspaceListError::from)?,
                )
                .map(|v| v.get())
                .map_err(|_| AgentWorkspaceListError::MalformedIdentity {
                    field: "workspaces.updated_at",
                })?,
            });
        }
        Ok(workspaces)
    }
}
