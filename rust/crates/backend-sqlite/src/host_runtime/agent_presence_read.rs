//! SQLite adapter for the agent-presence cold read.
//!
//! It supplies the facts [`silverhand_backend_core::host_runtime::agent_presence_read`] declares:
//! the agent's `last_activity_at` and the two set-shaped ACL arms, resolved together
//! on ONE `BEGIN DEFERRED` snapshot so the visibility a caller is judged against and
//! the activity stamp the away deadline is projected from come from one view of the
//! database.
//!
//! ## The ACL arms are the SAME SHARED authority as availability and detail
//!
//! The direct-pair and workspace arms run [`crate::identity_access::agent_view_acl`], the SAME SQL
//! `GET /api/agents/{id}/availability` and `GET /api/agents/{id}` decide against, so
//! the three faces cannot drift on who may see an agent. The creator arm is the agent
//! row's `created_by_user_id`.
//!
//! ## The away deadline is not read here
//!
//! This adapter reads only the STORED `last_activity_at`; the wire `away_at` is the
//! clock-free [`silverhand_backend_core::host_runtime::agent_presence`] policy the core
//! bundle applies, the SAME boundary the runtime-delivery writer uses. No TTL
//! arithmetic lives in this adapter.
//!
//! ## Every committed value is decoded strictly
//!
//! Ids go through the domain's fallible parsers and the stage through
//! [`MaterializationStatus::parse`]: a stored value that cannot be a legal identity or
//! a known stage is corruption of a committed row and surfaces as a
//! [`PersistenceFailure`] (a safe `500`), never coerced to a default.
//! `last_activity_at` is an `i64` column; a non-numeric text there is a decode error,
//! never a silent `None`.

use std::{path::Path, time::Duration};

use silverhand_backend_core::{
    foundation::identity::{AgentId, UserId},
    foundation::persistence::PersistenceFailure,
    host_runtime::agent_presence::ActivityInstant,
    host_runtime::agent_presence_read::{AgentPresenceFacts, AgentPresenceReadModel},
    identity_access::agent_lifecycle::MaterializationStatus,
    identity_access::capability::agent_view::{AgentViewerArms, AgentVisibility},
};
use sqlx::Row;
use thiserror::Error;

use crate::identity_access::agent_view_acl::{AgentViewAclError, read_agent_audience};

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const DEFERRED_READ_BEGIN: &str = "BEGIN DEFERRED";

/// The one activity stamp the deadline is projected from, plus the two id columns the
/// creator arm and source-bind need. The runtime-transition columns and the DTO-only
/// fields are deliberately unselected: presence reads the away axis, nothing else.
const SELECT_AGENT_PRESENCE_ROW: &str = "SELECT id, created_by_user_id, \
     materialization_status, last_activity_at FROM agents WHERE id = ?";

#[derive(Clone)]
pub struct SqliteAgentPresenceColdRead {
    pool: crate::foundation::database::SqliteDatabase,
}

/// Read failures surfaced only as the source of a [`PersistenceFailure`]. Every
/// variant is a driver fault or corruption of a committed row, never a product-level
/// refusal — those are decisions the core capability makes from the facts this returns.
#[derive(Debug, Error)]
pub enum AgentPresenceColdReadError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("stored column {column} does not name a usable identity")]
    MalformedIdentity { column: &'static str },
    #[error("stored agents.materialization_status holds {value:?}, which is not a known stage")]
    MalformedStatus { value: String },
    #[error("stored agents.last_activity_at holds {value}, which is not a usable presence instant")]
    MalformedActivityStamp { value: i64 },
}

impl SqliteAgentPresenceColdRead {
    pub fn from_pool(pool: impl Into<crate::foundation::database::SqliteDatabase>) -> Self {
        let pool = pool.into();
        Self { pool }
    }

    pub async fn open(path: impl AsRef<Path>) -> Result<Self, AgentPresenceColdReadError> {
        let pool = crate::foundation::database::store_pool(path.as_ref(), BUSY_TIMEOUT).await?;
        Ok(Self { pool })
    }

    async fn read_agent_presence_facts(
        &self,
        agent: &AgentId,
    ) -> Result<Option<AgentPresenceFacts>, AgentPresenceColdReadError> {
        let mut tx = self.pool.begin_with(DEFERRED_READ_BEGIN).await?;

        let row = sqlx::query(SELECT_AGENT_PRESENCE_ROW)
            .bind(agent.as_str())
            .fetch_optional(&mut *tx)
            .await?;
        let Some(row) = row else {
            tx.commit().await?;
            return Ok(None);
        };

        let source: String = row.try_get("id")?;
        let source =
            AgentId::parse(source).map_err(|_| AgentPresenceColdReadError::MalformedIdentity {
                column: "agents.id",
            })?;
        let creator: String = row.try_get("created_by_user_id")?;
        let creator =
            UserId::parse(creator).map_err(|_| AgentPresenceColdReadError::MalformedIdentity {
                column: "agents.created_by_user_id",
            })?;
        let status_raw: String = row.try_get("materialization_status")?;
        let materialization_status = MaterializationStatus::parse(&status_raw)
            .map_err(|_| AgentPresenceColdReadError::MalformedStatus { value: status_raw })?;
        // Strict-decode the stamp BEFORE the ACL arms and any product projection: a
        // negative or beyond-wire-safe committed value is corruption, surfaced as a
        // named read error (a safe 500), never saturated into a legal deadline.
        let last_activity_at: Option<i64> = row.try_get("last_activity_at")?;
        let last_activity_at = last_activity_at
            .map(ActivityInstant::new)
            .transpose()
            .map_err(
                |out_of_range| AgentPresenceColdReadError::MalformedActivityStamp {
                    value: out_of_range.0,
                },
            )?;

        let audience = read_agent_audience(&mut tx, agent).await?;

        tx.commit().await?;

        let viewers = AgentViewerArms::new(
            Some(creator),
            audience.direct_pair_human_endpoints,
            audience.shared_workspace_members,
        );
        Ok(Some(AgentPresenceFacts::from_source(
            source,
            AgentVisibility {
                viewers,
                materialization_status,
            },
            last_activity_at,
        )))
    }
}

impl From<AgentViewAclError> for AgentPresenceColdReadError {
    fn from(error: AgentViewAclError) -> Self {
        match error {
            AgentViewAclError::Sql(error) => Self::Sql(error),
            AgentViewAclError::MalformedIdentity { column } => Self::MalformedIdentity { column },
        }
    }
}

impl AgentPresenceReadModel for SqliteAgentPresenceColdRead {
    async fn read_agent_presence(
        &self,
        agent: &AgentId,
    ) -> Result<Option<AgentPresenceFacts>, PersistenceFailure> {
        self.read_agent_presence_facts(agent)
            .await
            .map_err(PersistenceFailure::from_source)
    }
}
