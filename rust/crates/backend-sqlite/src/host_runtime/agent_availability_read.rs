//! SQLite adapter for the stored half of the availability cold read.
//!
//! It supplies the facts [`silverhand_backend_core::host_runtime::agent_availability_read`]
//! declares, in the two reads that module separates: the agent view before the
//! ACL gate, and the host row only after it passes.
//!
//! ## The agent view is ONE deferred snapshot
//!
//! [`read_agent_view`](AgentAvailabilityReadModel::read_agent_view) reads the
//! agent row and the two set-shaped ACL arms (the human endpoints of the agent's
//! direct pairs, and the members of every workspace the agent reaches) inside a
//! single `BEGIN DEFERRED` transaction, so the visibility a caller is judged
//! against and the reachability inputs a value is later computed from come from
//! ONE view of the database. A write landing between the arm reads cannot make an
//! agent pass the ACL as it stood and then be answered from a later row.
//!
//! ## The set-shaped arms come from the shared relation, not a private SQL
//!
//! The creator arm is the agent row's `created_by_user_id`, read once off the
//! agent row. The two set-shaped arms are read by
//! [`read_agent_audience`](crate::identity_access::agent_view_acl::read_agent_audience), the single
//! authority in [`crate::identity_access::agent_view_acl`] that both this availability read and the
//! agent-list read share so the ACL cannot drift between them: the direct-pair arm
//! gates the OTHER endpoint on `= 'human'` in both slot orders so an agent↔agent
//! pair contributes nobody, and the workspace arm unions the members of every
//! workspace the agent reaches PLUS the agent's own agent-home workspace. Dropping
//! any one arm hides a viewer admitted only by it — the differential the route
//! tests pin.
//!
//! ## Every committed value is decoded strictly
//!
//! Ids go through the domain's fallible parsers and the stage through
//! [`MaterializationStatus::parse`]: a stored value that cannot be a legal
//! identity or a known stage is corruption of a committed row and surfaces as a
//! [`PersistenceFailure`] (a safe `500`), never coerced to a default.

use std::{path::Path, time::Duration};

use silverhand_backend_core::{
    foundation::identity::{AgentId, HostId, UserId},
    foundation::persistence::PersistenceFailure,
    host_runtime::agent_availability_read::{AgentAvailabilityReadModel, AgentViewFacts},
    host_runtime::availability::HostReachabilityFacts,
    identity_access::agent_lifecycle::MaterializationStatus,
    identity_access::capability::agent_view::{AgentViewerArms, AgentVisibility},
};
use sqlx::{Row, SqlitePool};
use thiserror::Error;

use crate::identity_access::agent_view_acl::{AgentViewAclError, read_agent_audience};

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const DEFERRED_READ_BEGIN: &str = "BEGIN DEFERRED";

const SELECT_AGENT_ROW: &str = "SELECT id, host_id, created_by_user_id, materialization_status, \
     deletion_started_at FROM agents WHERE id = ?";

const SELECT_HOST_ROW: &str = "SELECT revoked_at FROM hosts WHERE id = ?";

#[derive(Clone)]
pub struct SqliteAgentAvailabilityColdRead {
    pool: SqlitePool,
}

/// Read failures surfaced only as the source of a [`PersistenceFailure`]. Every
/// variant is a driver fault or corruption of a committed row, never a
/// product-level refusal — those are decisions the core capabilities make from
/// the facts this returns.
#[derive(Debug, Error)]
pub enum AgentAvailabilityColdReadError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("stored column {column} does not name a usable identity")]
    MalformedIdentity { column: &'static str },
    #[error("stored agents.materialization_status holds {value:?}, which is not a known stage")]
    MalformedStatus { value: String },
}

impl SqliteAgentAvailabilityColdRead {
    pub fn from_pool(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn open(path: impl AsRef<Path>) -> Result<Self, AgentAvailabilityColdReadError> {
        let pool = crate::foundation::database::store_pool(path.as_ref(), BUSY_TIMEOUT).await?;
        Ok(Self { pool })
    }

    async fn read_agent_view_facts(
        &self,
        agent: &AgentId,
    ) -> Result<Option<AgentViewFacts>, AgentAvailabilityColdReadError> {
        let mut tx = self.pool.begin_with(DEFERRED_READ_BEGIN).await?;

        let row = sqlx::query(SELECT_AGENT_ROW)
            .bind(agent.as_str())
            .fetch_optional(&mut *tx)
            .await?;
        let Some(row) = row else {
            tx.commit().await?;
            return Ok(None);
        };

        let source: String = row.try_get("id")?;
        let source = AgentId::parse(source).map_err(|_| {
            AgentAvailabilityColdReadError::MalformedIdentity {
                column: "agents.id",
            }
        })?;
        let host_id: String = row.try_get("host_id")?;
        let host_id = HostId::parse(host_id).map_err(|_| {
            AgentAvailabilityColdReadError::MalformedIdentity {
                column: "agents.host_id",
            }
        })?;
        let creator: String = row.try_get("created_by_user_id")?;
        let creator = UserId::parse(creator).map_err(|_| {
            AgentAvailabilityColdReadError::MalformedIdentity {
                column: "agents.created_by_user_id",
            }
        })?;
        let status_raw: String = row.try_get("materialization_status")?;
        let materialization_status = MaterializationStatus::parse(&status_raw)
            .map_err(|_| AgentAvailabilityColdReadError::MalformedStatus { value: status_raw })?;
        let deletion_started_at: Option<i64> = row.try_get("deletion_started_at")?;

        let audience = read_agent_audience(&mut tx, agent).await?;

        tx.commit().await?;

        let viewers = AgentViewerArms::new(
            Some(creator),
            audience.direct_pair_human_endpoints,
            audience.shared_workspace_members,
        );
        Ok(Some(AgentViewFacts::from_source(
            source,
            AgentVisibility {
                viewers,
                materialization_status,
            },
            host_id,
            deletion_started_at,
        )))
    }

    async fn read_host_reachability_row(
        &self,
        host: &HostId,
    ) -> Result<Option<HostReachabilityFacts>, AgentAvailabilityColdReadError> {
        let Some(row) = sqlx::query(SELECT_HOST_ROW)
            .bind(host.as_str())
            .fetch_optional(&self.pool)
            .await?
        else {
            return Ok(None);
        };
        let revoked_at: Option<i64> = row.try_get("revoked_at")?;
        Ok(Some(HostReachabilityFacts {
            id: host.clone(),
            revoked_at,
        }))
    }
}

impl From<AgentViewAclError> for AgentAvailabilityColdReadError {
    fn from(error: AgentViewAclError) -> Self {
        match error {
            AgentViewAclError::Sql(error) => Self::Sql(error),
            AgentViewAclError::MalformedIdentity { column } => Self::MalformedIdentity { column },
        }
    }
}

impl AgentAvailabilityReadModel for SqliteAgentAvailabilityColdRead {
    async fn read_agent_view(
        &self,
        agent: &AgentId,
    ) -> Result<Option<AgentViewFacts>, PersistenceFailure> {
        self.read_agent_view_facts(agent)
            .await
            .map_err(PersistenceFailure::from_source)
    }

    async fn read_host_reachability(
        &self,
        host: &HostId,
    ) -> Result<Option<HostReachabilityFacts>, PersistenceFailure> {
        self.read_host_reachability_row(host)
            .await
            .map_err(PersistenceFailure::from_source)
    }
}
