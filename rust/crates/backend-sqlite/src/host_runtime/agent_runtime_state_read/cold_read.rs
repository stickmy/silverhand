//! SQLite adapter for the agent runtime-state cold read.
//!
//! It supplies the facts [`silverhand_backend_core::host_runtime::agent_runtime_state_read`]
//! declares, resolved together on ONE `BEGIN DEFERRED` snapshot: the agent's ACL
//! arms + stage, its COMPLETE participation set (each row carrying its archived flag
//! and this viewer's readability), and its durable seats with per-scope undelivered
//! queue depth. Reading the visibility authority and the payload on one snapshot is
//! what closes the authority↔payload TOCTOU — an agent reparented or a conversation
//! archived between two reads can never admit a caller one read would hide.
//!
//! ## The ACL and redaction authorities are the SHARED ones
//!
//! The visibility arms come from [`crate::identity_access::agent_view_acl::read_agent_audience`] — the
//! SAME relation `GET /api/agents/{id}/availability` and `GET /api/agents/{id}` decide
//! against — so this face cannot drift on who may see an agent. Per-conversation
//! redaction rides [`human_participation_source!`], the SAME strict human-participation
//! authority the conversation-participation read owns; the participation set is filtered
//! by [`agent_participation_predicate!`], the SAME predicate the send gate and task-read
//! set use. Composing both macros here — never a second SQL spelling — is why an ACL edit
//! reaches this face or no face.
//!
//! ## Every decision-driving value is decoded strictly
//!
//! Ids go through the domain's fallible parsers, the stage through
//! [`MaterializationStatus::parse`], the seat status through
//! [`RuntimeObservedStatus::parse`], the roster policy through
//! [`ConversationRosterPolicy::parse`], and the generation through
//! [`RuntimeGeneration::new`]: a stored value outside its set is corruption of a
//! committed row and surfaces as a [`PersistenceFailure`] (a safe `500`), never a
//! default. The wire-echo strings the DTO passes through verbatim — `workspace_id`,
//! `workspace_name`, `placement_kind`, `channel_title`, `pid`, `cli_session_id` — carry
//! no decode of their own, matching the detail face's treatment of echoed columns.

use std::{path::Path, time::Duration};

use silverhand_backend_core::{
    conversation_messaging::messaging::collaboration::conversation_ref::ConversationRosterPolicy,
    foundation::identity::{AgentId, ConversationId, UserId},
    foundation::persistence::PersistenceFailure,
    host_runtime::agent_runtime_state_read::{
        AgentRuntimeStateFacts, AgentRuntimeStateReadModel, RuntimeParticipationFact,
        RuntimeSeatFact, RuntimeSeatScopeFact,
    },
    host_runtime::runtime_credential::RuntimeGeneration,
    host_runtime::runtime_event::RuntimeObservedStatus,
    identity_access::agent_lifecycle::MaterializationStatus,
};
use sqlx::{Row, SqlitePool, sqlite::SqliteRow};
use thiserror::Error;

use crate::{
    conversation_messaging::messaging::agent_participation_predicate,
    conversation_messaging::messaging::collaboration::conversation_participation::human_participation_source,
    identity_access::agent_view_acl::{AgentViewAclError, read_agent_audience},
};

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const DEFERRED_READ_BEGIN: &str = "BEGIN DEFERRED";

/// The agent-row facts the ACL and the source-bind seal need: the row's OWN `id`,
/// the creator arm, and the materialization stage. Selecting `agents.id` is what
/// makes the bundle SOURCE-BOUND — the seal coordinate is the id the row was read
/// back with (strictly decoded below), never the request echo, so a future query
/// that joined or spliced a different row than the one asked for is caught by the
/// route's `facts.source()` guard instead of silently projecting the wrong agent.
/// `None` from this read is the agent's absence arm; the participation and seat reads
/// never run for a row that does not exist.
const SELECT_AGENT_ACL_ROW: &str =
    "SELECT id, created_by_user_id, materialization_status FROM agents WHERE id = ?";

/// Every conversation the agent is a party to, each row carrying its archived flag and
/// THIS viewer's readability, on the snapshot. `agent_participation_predicate!` gates
/// membership (the SAME predicate the send gate uses); `human_participation_source!`
/// resolves redaction (the SAME strict human-participation authority the conversation
/// read owns), with the viewer bound ONCE through a CTE because the macro names it three
/// times. Complete set — the archived flag, not a second query, is the visible/complete
/// split. Binds, in order: the viewer (CTE), then the agent three times (predicate).
const SELECT_AGENT_PARTICIPATIONS: &str = concat!(
    "WITH v(viewer) AS (SELECT ?) \
     SELECT c.id AS conversation_id, c.workspace_id AS workspace_id, w.name AS workspace_name, \
        c.placement_kind AS placement_kind, c.roster_policy AS roster_policy, \
        c.title AS channel_title, c.closed_at AS closed_at, c.archived_at AS archived_at, ",
    human_participation_source!("c", "(SELECT viewer FROM v)"),
    " AS source_viewer \
       FROM conversations c \
       LEFT JOIN workspaces w ON w.id = c.workspace_id \
      WHERE ",
    agent_participation_predicate!(),
    " ORDER BY c.closed_at IS NULL DESC, c.updated_at DESC"
);

/// Every durable seat with its per-scope undelivered queue depth and — for a conversation
/// seat — this viewer's readability of the seat's conversation, all on the snapshot in ONE
/// statement. The undelivered depth is PRE-AGGREGATED once, not counted per seat: `q`
/// groups this agent's undelivered inbox rows by conversation ONCE, `t` sums that group set
/// to the agent-wide total ONCE, and each seat then reads its depth by a JOIN/lookup into
/// those materialized results (agent-scope seat ← the single `t` total via `CROSS JOIN`;
/// conversation seat ← its own `q` group via `LEFT JOIN`, absent group = 0). So N
/// conversation seats cost one grouped aggregation, not N correlated `COUNT(*)` scans.
/// Redaction reuses the SAME `human_participation_source!` authority correlated to the
/// seat's conversation (a bounded per-conversation membership fact, not an aggregation).
/// `t` derives from `q` (a `SUM` over the already-grouped set) and binds nothing, so this
/// statement carries exactly THREE placeholders. Binds, in order: the viewer (CTE), the
/// agent (`q`), then the agent (`WHERE`). Undelivered = `delivered_at IS NULL AND failed_at IS NULL`.
const SELECT_AGENT_SEATS: &str = concat!(
    "WITH v(viewer) AS (SELECT ?), \
       q(conversation_id, depth) AS ( \
         SELECT i.conversation_id, COUNT(*) FROM conversation_inboxes i \
          WHERE i.agent_id = ? AND i.delivered_at IS NULL AND i.failed_at IS NULL \
          GROUP BY i.conversation_id), \
       t(depth) AS (SELECT COALESCE(SUM(depth), 0) FROM q) \
     SELECT s.conversation_id AS conversation_id, s.observed_status AS observed_status, \
        s.observed_pid AS observed_pid, s.generation AS generation, \
        s.cli_session_id AS cli_session_id, \
        CASE WHEN s.conversation_id IS NULL THEN NULL ELSE (SELECT ",
    human_participation_source!("c", "(SELECT viewer FROM v)"),
    " FROM conversations c WHERE c.id = s.conversation_id) END AS source_viewer, \
        CASE WHEN s.conversation_id IS NULL THEN t.depth ELSE COALESCE(q.depth, 0) END AS queue_depth \
       FROM agent_runtime_slots s \
       CROSS JOIN t \
       LEFT JOIN q ON q.conversation_id = s.conversation_id \
      WHERE s.agent_id = ? \
      ORDER BY s.id ASC"
);

/// Whether ONE conversation is readable by the viewer under the SAME strict
/// human-participation authority — the post-snapshot question the live focus asks about a
/// conversation whose identity is only known after the read. Mirrors the participation
/// read's redaction column narrowed to one id. Binds: the viewer (CTE), then the
/// conversation id.
const SELECT_CONVERSATION_VIEWER: &str = concat!(
    "WITH v(viewer) AS (SELECT ?) \
     SELECT source_viewer FROM (SELECT ",
    human_participation_source!("c", "(SELECT viewer FROM v)"),
    " AS source_viewer FROM conversations c WHERE c.id = ?) WHERE source_viewer IS NOT NULL"
);

#[derive(Clone)]
pub struct SqliteAgentRuntimeStateColdRead {
    pool: SqlitePool,
}

/// Read failures surfaced only as the source of a [`PersistenceFailure`]. Every variant is
/// a driver fault or corruption of a committed row, never a product refusal — those are
/// decisions the core capability makes from the facts this returns.
#[derive(Debug, Error)]
pub enum AgentRuntimeStateColdReadError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("stored column {column} does not name a usable identity")]
    MalformedIdentity { column: &'static str },
    #[error("stored agents.materialization_status holds {value:?}, which is not a known stage")]
    MalformedStatus { value: String },
    #[error("stored agent_runtime_slots.observed_status holds {value:?}, outside the closed set")]
    MalformedObservedStatus { value: String },
    #[error("stored conversations.roster_policy holds {value:?}, outside the closed set")]
    MalformedRosterPolicy { value: String },
    #[error("stored agent_runtime_slots.generation holds {value}, which is not a valid generation")]
    MalformedGeneration { value: i64 },
}

impl From<AgentViewAclError> for AgentRuntimeStateColdReadError {
    fn from(error: AgentViewAclError) -> Self {
        match error {
            AgentViewAclError::Sql(error) => Self::Sql(error),
            AgentViewAclError::MalformedIdentity { column } => Self::MalformedIdentity { column },
        }
    }
}

impl SqliteAgentRuntimeStateColdRead {
    pub fn from_pool(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn open(path: impl AsRef<Path>) -> Result<Self, AgentRuntimeStateColdReadError> {
        let pool = crate::foundation::database::store_pool(path.as_ref(), BUSY_TIMEOUT).await?;
        Ok(Self { pool })
    }

    async fn read_facts(
        &self,
        agent: &AgentId,
        viewer: &UserId,
    ) -> Result<Option<AgentRuntimeStateFacts>, AgentRuntimeStateColdReadError> {
        let mut tx = self.pool.begin_with(DEFERRED_READ_BEGIN).await?;

        let Some(acl_row) = sqlx::query(SELECT_AGENT_ACL_ROW)
            .bind(agent.as_str())
            .fetch_optional(&mut *tx)
            .await?
        else {
            tx.commit().await?;
            return Ok(None);
        };
        let creator = decode_creator(&acl_row)?;
        let materialization_status = decode_materialization_status(&acl_row)?;
        // The seal coordinate is the id the ROW carries, strictly decoded — never the
        // request echo. This is what gives the route's `facts.source()` guard something
        // to bite on: a query that ever resolved a different row than the one asked for
        // would land a mismatched id here.
        let source = decode_source(&acl_row)?;

        // The ACL arms land on the SAME snapshot as the agent row they describe.
        let audience = read_agent_audience(&mut tx, agent).await?;

        let participation_rows = sqlx::query(SELECT_AGENT_PARTICIPATIONS)
            .bind(viewer.as_str())
            .bind(agent.as_str())
            .bind(agent.as_str())
            .bind(agent.as_str())
            .fetch_all(&mut *tx)
            .await?;
        let mut participations = Vec::with_capacity(participation_rows.len());
        for row in &participation_rows {
            participations.push(decode_participation(row)?);
        }

        let seat_rows = sqlx::query(SELECT_AGENT_SEATS)
            .bind(viewer.as_str())
            .bind(agent.as_str())
            .bind(agent.as_str())
            .fetch_all(&mut *tx)
            .await?;
        let mut seats = Vec::with_capacity(seat_rows.len());
        for row in &seat_rows {
            seats.push(decode_seat(row)?);
        }

        tx.commit().await?;

        Ok(Some(AgentRuntimeStateFacts::from_source(
            source,
            creator,
            audience.direct_pair_human_endpoints,
            audience.shared_workspace_members,
            materialization_status,
            participations,
            seats,
        )))
    }

    async fn read_conversation_readable(
        &self,
        viewer: &UserId,
        conversation: &ConversationId,
    ) -> Result<bool, AgentRuntimeStateColdReadError> {
        let source_viewer: Option<String> = sqlx::query_scalar(SELECT_CONVERSATION_VIEWER)
            .bind(viewer.as_str())
            .bind(conversation.as_str())
            .fetch_optional(&self.pool)
            .await?;
        Ok(source_viewer.is_some())
    }
}

fn decode_source(row: &SqliteRow) -> Result<AgentId, AgentRuntimeStateColdReadError> {
    let raw: String = row.try_get("id")?;
    AgentId::parse(raw).map_err(|_| AgentRuntimeStateColdReadError::MalformedIdentity {
        column: "agents.id",
    })
}

fn decode_creator(row: &SqliteRow) -> Result<UserId, AgentRuntimeStateColdReadError> {
    let raw: String = row.try_get("created_by_user_id")?;
    UserId::parse(raw).map_err(|_| AgentRuntimeStateColdReadError::MalformedIdentity {
        column: "agents.created_by_user_id",
    })
}

fn decode_materialization_status(
    row: &SqliteRow,
) -> Result<MaterializationStatus, AgentRuntimeStateColdReadError> {
    let raw: String = row.try_get("materialization_status")?;
    MaterializationStatus::parse(&raw)
        .map_err(|_| AgentRuntimeStateColdReadError::MalformedStatus { value: raw })
}

fn decode_participation(
    row: &SqliteRow,
) -> Result<RuntimeParticipationFact, AgentRuntimeStateColdReadError> {
    let conversation_raw: String = row.try_get("conversation_id")?;
    let conversation = ConversationId::parse(conversation_raw).map_err(|_| {
        AgentRuntimeStateColdReadError::MalformedIdentity {
            column: "conversations.id",
        }
    })?;
    let roster_raw: String = row.try_get("roster_policy")?;
    let roster_policy = ConversationRosterPolicy::parse(&roster_raw)
        .map_err(|_| AgentRuntimeStateColdReadError::MalformedRosterPolicy { value: roster_raw })?;
    let archived_at: Option<i64> = row.try_get("archived_at")?;
    let source_viewer: Option<String> = row.try_get("source_viewer")?;
    Ok(RuntimeParticipationFact {
        conversation,
        workspace_id: row.try_get("workspace_id")?,
        workspace_name: row.try_get("workspace_name")?,
        placement_kind: row.try_get("placement_kind")?,
        roster_policy,
        channel_title: row.try_get("channel_title")?,
        closed_at: row.try_get("closed_at")?,
        archived: archived_at.is_some(),
        viewer_can_read: source_viewer.is_some(),
    })
}

fn decode_seat(row: &SqliteRow) -> Result<RuntimeSeatFact, AgentRuntimeStateColdReadError> {
    let conversation_raw: Option<String> = row.try_get("conversation_id")?;
    let source_viewer: Option<String> = row.try_get("source_viewer")?;
    let scope = match conversation_raw {
        None => RuntimeSeatScopeFact::Agent,
        Some(raw) => {
            let conversation = ConversationId::parse(raw).map_err(|_| {
                AgentRuntimeStateColdReadError::MalformedIdentity {
                    column: "agent_runtime_slots.conversation_id",
                }
            })?;
            RuntimeSeatScopeFact::Conversation {
                conversation,
                viewer_can_read: source_viewer.is_some(),
            }
        }
    };
    let status_raw: String = row.try_get("observed_status")?;
    let status = RuntimeObservedStatus::parse(&status_raw).map_err(|_| {
        AgentRuntimeStateColdReadError::MalformedObservedStatus { value: status_raw }
    })?;
    let generation_raw: i64 = row.try_get("generation")?;
    let generation = RuntimeGeneration::new(generation_raw).map_err(|_| {
        AgentRuntimeStateColdReadError::MalformedGeneration {
            value: generation_raw,
        }
    })?;
    Ok(RuntimeSeatFact {
        scope,
        generation,
        status,
        pid: row.try_get("observed_pid")?,
        cli_session_id: row.try_get("cli_session_id")?,
        queue_depth: row.try_get("queue_depth")?,
    })
}

impl AgentRuntimeStateReadModel for SqliteAgentRuntimeStateColdRead {
    async fn read_agent_runtime_state(
        &self,
        agent: &AgentId,
        viewer: &UserId,
    ) -> Result<Option<AgentRuntimeStateFacts>, PersistenceFailure> {
        self.read_facts(agent, viewer)
            .await
            .map_err(PersistenceFailure::from_source)
    }

    async fn read_conversation_visibility(
        &self,
        viewer: &UserId,
        conversation: &ConversationId,
    ) -> Result<bool, PersistenceFailure> {
        self.read_conversation_readable(viewer, conversation)
            .await
            .map_err(PersistenceFailure::from_source)
    }
}

#[cfg(test)]
mod shape {
    use super::SELECT_AGENT_SEATS;

    /// The seat query aggregates undelivered depth EXACTLY ONCE, not per seat. The old
    /// shape ran a correlated `COUNT(*)` in each `CASE` arm (two spellings, re-scanned
    /// per row); the grouped shape has a single `COUNT(*)` inside the `q` CTE and folds
    /// the agent-wide total with `SUM` over that same group. Pinning the aggregation
    /// count is the resident guard that a future edit cannot silently reintroduce the
    /// per-seat N+1 while still returning the right numbers.
    #[test]
    fn the_seat_query_pre_aggregates_undelivered_once() {
        assert_eq!(
            SELECT_AGENT_SEATS.matches("COUNT(*)").count(),
            1,
            "undelivered depth must be counted once in the `q` group, never per seat"
        );
        assert!(SELECT_AGENT_SEATS.contains("GROUP BY i.conversation_id"));
        assert!(SELECT_AGENT_SEATS.contains("CROSS JOIN t"));
        assert!(
            SELECT_AGENT_SEATS.contains("LEFT JOIN q ON q.conversation_id = s.conversation_id")
        );
    }
}
