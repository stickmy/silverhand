//! SQLite adapter for the agent-detail cold read.
//!
//! It supplies the facts [`silverhand_backend_core::identity_access::agent_detail_read`] declares:
//! the agent row, its host `LEFT JOIN`, and the two set-shaped ACL arms, resolved
//! together on ONE `BEGIN DEFERRED` snapshot so the visibility a caller is judged
//! against and the values projected to the wire come from one view of the database.
//!
//! ## The ACL arms are the SHARED authority
//!
//! The direct-pair and workspace arms run [`crate::identity_access::agent_view_acl`], the SAME SQL
//! `GET /api/agents/{id}/availability` decides against, so the two faces cannot
//! drift on who may see an agent. The creator arm is the agent row's
//! `created_by_user_id`, decoded ONCE and used both as the ACL creator and as the
//! wire's `created_by_user_id` — the gate input and the wire echo can never differ.
//!
//! ## Every committed value is decoded strictly
//!
//! Ids go through the domain's fallible parsers, the stage through
//! [`MaterializationStatus::parse`], the runtime through
//! [`RuntimeKind::parse`], and the reachable integer columns through an
//! `i64` decode: a stored value that cannot be a legal identity, a known stage, a
//! runtime in the closed set, or a number is corruption of a committed row and
//! surfaces as a [`PersistenceFailure`] (a safe `500`), never coerced to a default.
//! The remaining wire-shaped strings the DTO passes through verbatim (name, model, …)
//! carry no decode of their own.
//!
//! `runtime` is decoded and not passed through because it is a CLOSED wire
//! vocabulary: the same token the dispatch and roster faces refuse when it is outside
//! the set. Both the detail and the list face project through
//! [`decode_agent_row`], so neither can hand the wire a runtime the rest of the
//! system would reject.

use std::{path::Path, time::Duration};

use silverhand_backend_core::{
    foundation::identity::{AgentId, UserId},
    foundation::persistence::PersistenceFailure,
    identity_access::agent_detail_read::{
        AgentDetailFacts, AgentDetailProjection, AgentDetailReadModel,
    },
    identity_access::agent_lifecycle::MaterializationStatus,
    identity_access::agents::runtime::RuntimeKind,
};
use sqlx::{Row, sqlite::SqliteRow};
use thiserror::Error;

use crate::identity_access::agent_view_acl::{AgentViewAclError, read_agent_audience};

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const DEFERRED_READ_BEGIN: &str = "BEGIN DEFERRED";

/// The agent row plus its host `LEFT JOIN` — every column the detail DTO projects,
/// with NO predicate of its own. The detail read appends `WHERE a.id = ?`; the list
/// read appends its candidate predicate and `ORDER BY`. Sharing the column list and
/// join here is what keeps the two reads on ONE row shape: a column added for detail
/// cannot miss the list, and [`decode_agent_row`] decodes either identically.
/// `idempotency_key`, the runtime-transition columns and `last_activity_at` are
/// deliberately unselected: they are not on the DTO.
///
/// It is a `macro_rules!` fragment, not a `const`, because sqlx accepts only a
/// `&'static str`: each face composes this literal with its own predicate through
/// `concat!` to get one at compile time, never a runtime `format!`.
macro_rules! agent_detail_select_from {
    () => {
        "SELECT a.id, a.name, a.display_name, a.runtime, a.model, \
           a.reasoning_effort, a.fast_mode, a.host_id, a.description, a.session_scope, \
           a.created_by_user_id, a.avatar_storage_key, a.materialization_status, \
           a.materialization_failed_code, a.materialization_failed_at, a.deletion_started_at, \
           a.created_at, a.updated_at, h.display_label AS host_display_label, h.name AS host_name \
           FROM agents a LEFT JOIN hosts h ON h.id = a.host_id"
    };
}
pub(crate) use agent_detail_select_from;

/// The single-agent detail read: the shared projection narrowed to ONE id, composed
/// at compile time so sqlx sees a `&'static str`.
const SELECT_AGENT_DETAIL_ROW: &str = concat!(agent_detail_select_from!(), " WHERE a.id = ?");

#[derive(Clone)]
pub struct SqliteAgentDetailColdRead {
    pool: crate::foundation::database::SqliteDatabase,
}

/// Read failures surfaced only as the source of a [`PersistenceFailure`]. Every
/// variant is a driver fault or corruption of a committed row, never a
/// product-level refusal — those are decisions the core capability makes from the
/// facts this returns.
#[derive(Debug, Error)]
pub enum AgentDetailColdReadError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("stored column {column} does not name a usable identity")]
    MalformedIdentity { column: &'static str },
    #[error("stored agents.materialization_status holds {value:?}, which is not a known stage")]
    MalformedStatus { value: String },
    #[error("stored agents.runtime holds {value:?}, which is not a runtime in the closed set")]
    MalformedRuntime { value: String },
}

impl SqliteAgentDetailColdRead {
    pub fn from_pool(pool: impl Into<crate::foundation::database::SqliteDatabase>) -> Self {
        let pool = pool.into();
        Self { pool }
    }

    pub async fn open(path: impl AsRef<Path>) -> Result<Self, AgentDetailColdReadError> {
        let pool = crate::foundation::database::store_pool(path.as_ref(), BUSY_TIMEOUT).await?;
        Ok(Self { pool })
    }

    async fn read_agent_detail_facts(
        &self,
        agent: &AgentId,
    ) -> Result<Option<AgentDetailFacts>, AgentDetailColdReadError> {
        let mut tx = self.pool.begin_with(DEFERRED_READ_BEGIN).await?;

        let row = sqlx::query(SELECT_AGENT_DETAIL_ROW)
            .bind(agent.as_str())
            .fetch_optional(&mut *tx)
            .await?;
        let Some(row) = row else {
            tx.commit().await?;
            return Ok(None);
        };

        let decoded = decode_agent_row(&row)?;
        let audience = read_agent_audience(&mut tx, agent).await?;

        tx.commit().await?;

        // The creator is passed ONCE: `from_source` seeds the ACL creator arm from it
        // and the wire's `created_by_user_id` reads the same field, so the adapter
        // cannot admit one human and render another.
        Ok(Some(AgentDetailFacts::from_source(
            decoded.source,
            decoded.creator,
            audience.direct_pair_human_endpoints,
            audience.shared_workspace_members,
            decoded.materialization_status,
            decoded.projection,
        )))
    }
}

/// One decoded agent row, split into the coordinates the ACL binds (`source`,
/// `creator`), the stage the gate reads, and the wire-facing projection. Shared so
/// the list read decodes each candidate row byte-identically to the detail read.
pub(crate) struct DecodedAgentRow {
    pub(crate) source: AgentId,
    pub(crate) creator: UserId,
    pub(crate) materialization_status: MaterializationStatus,
    pub(crate) projection: AgentDetailProjection,
}

/// Decode one row of [`AGENT_DETAIL_SELECT_FROM`] strictly: ids through the domain's
/// fallible parsers, the stage through [`MaterializationStatus::parse`], the runtime
/// through [`RuntimeKind::parse`], and the reachable integer columns through an `i64`
/// decode. A stored value that cannot be a legal identity, a known stage, a runtime in
/// the closed set, or a number is corruption of a committed row and surfaces as an
/// error (a safe `500`), never coerced to a default. The remaining wire-shaped strings
/// the DTO passes through verbatim carry no decode of their own.
///
/// Shared by the detail and the list face, so the runtime gate holds for BOTH: there
/// is one decoder, not one per face.
pub(crate) fn decode_agent_row(
    row: &SqliteRow,
) -> Result<DecodedAgentRow, AgentDetailColdReadError> {
    let source: String = row.try_get("id")?;
    let source =
        AgentId::parse(source).map_err(|_| AgentDetailColdReadError::MalformedIdentity {
            column: "agents.id",
        })?;
    let created_by: String = row.try_get("created_by_user_id")?;
    let creator =
        UserId::parse(created_by).map_err(|_| AgentDetailColdReadError::MalformedIdentity {
            column: "agents.created_by_user_id",
        })?;
    let status_raw: String = row.try_get("materialization_status")?;
    let materialization_status = MaterializationStatus::parse(&status_raw)
        .map_err(|_| AgentDetailColdReadError::MalformedStatus { value: status_raw })?;

    // The runtime column has no CHECK, so an out-of-set token CAN be committed. Both
    // faces render it, so it fails closed here with the offending value attached
    // rather than being forwarded to the wire.
    let runtime_raw: String = row.try_get("runtime")?;
    let runtime = RuntimeKind::parse(&runtime_raw)
        .ok_or(AgentDetailColdReadError::MalformedRuntime { value: runtime_raw })?;

    // SQLite stores the boolean as 0/1; a non-numeric text in the affinity-only
    // column is a decode error above, never a silent `false`. `agentRowToDto` maps
    // null → null and any value → Boolean(fast), which for the stored 0/1 is exactly
    // this `!= 0`.
    let fast_mode: Option<i64> = row.try_get("fast_mode")?;
    let fast_mode = fast_mode.map(|value| value != 0);

    let projection = AgentDetailProjection {
        name: row.try_get("name")?,
        display_name: row.try_get("display_name")?,
        runtime,
        model: row.try_get("model")?,
        reasoning_effort: row.try_get("reasoning_effort")?,
        fast_mode,
        host_id: row.try_get("host_id")?,
        host_display_label: row.try_get("host_display_label")?,
        host_name: row.try_get("host_name")?,
        description: row.try_get("description")?,
        session_scope: row.try_get("session_scope")?,
        avatar_storage_key: row.try_get("avatar_storage_key")?,
        materialization_failed_code: row.try_get("materialization_failed_code")?,
        materialization_failed_at: row.try_get("materialization_failed_at")?,
        deletion_started_at: row.try_get("deletion_started_at")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    };

    Ok(DecodedAgentRow {
        source,
        creator,
        materialization_status,
        projection,
    })
}

impl From<AgentViewAclError> for AgentDetailColdReadError {
    fn from(error: AgentViewAclError) -> Self {
        match error {
            AgentViewAclError::Sql(error) => Self::Sql(error),
            AgentViewAclError::MalformedIdentity { column } => Self::MalformedIdentity { column },
        }
    }
}

impl AgentDetailReadModel for SqliteAgentDetailColdRead {
    async fn read_agent_detail(
        &self,
        agent: &AgentId,
    ) -> Result<Option<AgentDetailFacts>, PersistenceFailure> {
        self.read_agent_detail_facts(agent)
            .await
            .map_err(PersistenceFailure::from_source)
    }
}
