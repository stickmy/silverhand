//! The list direction of the agent-visibility relation, for `GET /api/agents`.
//!
//! Availability and detail each ask that relation about ONE agent; the list face
//! asks it about ONE VIEWER. Both directions read the same rows, so they belong to
//! one authority in [`crate::identity_access::agent_view_acl`] rather than to two SQL texts that can
//! drift — a drift in an ACL is a security bug, not a style nit.
//!
//! ## One relation, two directions, arms still separate
//!
//! The set-shaped arms become one `(agent_id, user_id, arm)` relation with
//! `arm` in `{direct_pair, workspace}`:
//!
//!  - the AUDIENCE direction selects by agent and PARTITIONS by tag, so it keeps
//!    feeding `AgentViewerArms::new` two separate iterators exactly as today. That
//!    constructor obliges every fact source to supply every arm, and a merged
//!    UNTAGGED relation would have made the obligation vacuous while still
//!    compiling;
//!  - the LIST direction selects by viewer and needs no partition at all: it
//!    produces candidate agent ids, and what admits them is the capability, not this
//!    query.
//!
//! The CREATOR arm stays entirely outside the relation. It is one column on the
//! agent row, read once and driving the ACL arm, `?scope=owned` and the wire echo
//! from a single decoded value.
//!
//! ## What the tag can and cannot prove
//!
//! Strict decode makes an UNKNOWN tag fail loudly. It does NOT prove which legal
//! branch a row came from: `AgentViewerArms` unions its arms immediately, so
//! swapping `direct_pair` for `workspace` on a row is invisible to every product
//! judgement downstream. Arm attribution is therefore pinned at the RELATION level,
//! by focused per-direction queries and mutations; a fixture that can only observe
//! the union cannot stand in for that proof. The tag buys a single relation, a
//! preserved two-arm supply structure and a named decode — not a provenance proof it
//! cannot carry.

use std::{path::Path, time::Duration};

use silverhand_backend_core::{
    foundation::identity::UserId, foundation::persistence::PersistenceFailure,
    identity_access::agent_detail_read::AgentDetailFacts,
    identity_access::agent_list_read::AgentListReadModel,
};
use sqlx::SqlitePool;

use crate::{
    identity_access::agent_detail_read::{
        AgentDetailColdReadError, agent_detail_select_from, decode_agent_row,
    },
    identity_access::agent_view_acl::{read_candidate_audiences, viewer_candidate_predicate},
};

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const DEFERRED_READ_BEGIN: &str = "BEGIN DEFERRED";

/// The list read's row query, composed at compile time so sqlx sees a `&'static str`.
/// The shared detail projection is narrowed to the agents a viewer may see through the
/// shared [`viewer_candidate_predicate`] — creator-owned OR relation-admitted — in the
/// port's `created_at ASC` order with NO tiebreak (the TypeScript face has none; ties are
/// undefined). Sharing the predicate with the bounded audience batch keeps "which agents
/// are candidates" spelled once. Both `?` binds take the viewer id.
const SELECT_VISIBLE_AGENT_ROWS: &str = concat!(
    agent_detail_select_from!(),
    " WHERE ",
    viewer_candidate_predicate!(),
    " ORDER BY a.created_at ASC"
);

#[derive(Clone)]
pub struct SqliteAgentListColdRead {
    pool: SqlitePool,
}

impl SqliteAgentListColdRead {
    pub fn from_pool(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn open(path: impl AsRef<Path>) -> Result<Self, AgentDetailColdReadError> {
        let pool = crate::foundation::database::store_pool(path.as_ref(), BUSY_TIMEOUT).await?;
        Ok(Self { pool })
    }

    async fn read_agent_candidates_facts(
        &self,
        viewer: &UserId,
    ) -> Result<Vec<AgentDetailFacts>, AgentDetailColdReadError> {
        let mut tx = self.pool.begin_with(DEFERRED_READ_BEGIN).await?;

        let candidates = read_agent_candidates_in_tx(&mut tx, viewer).await?;
        tx.commit().await?;
        Ok(candidates)
    }
}

impl AgentListReadModel for SqliteAgentListColdRead {
    async fn read_agent_candidates(
        &self,
        viewer: &UserId,
    ) -> Result<Vec<AgentDetailFacts>, PersistenceFailure> {
        self.read_agent_candidates_facts(viewer)
            .await
            .map_err(PersistenceFailure::from_source)
    }
}

/// Reuse the directory visibility facts inside another one-snapshot read.
pub(crate) async fn read_agent_candidates_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    viewer: &UserId,
) -> Result<Vec<AgentDetailFacts>, AgentDetailColdReadError> {
    let rows = sqlx::query(SELECT_VISIBLE_AGENT_ROWS)
        .bind(viewer.as_str())
        .bind(viewer.as_str())
        .fetch_all(&mut **tx)
        .await?;

    // The audience for EVERY candidate is read once per arm on the SAME snapshot,
    // keyed by the agent each arm describes — a fixed statement count no matter how
    // many candidates the viewer has. `agent_list_read::visible_agents` re-decides
    // each row from these arms, so the narrowing above only offers a superset.
    let mut audiences = read_candidate_audiences(tx, viewer).await?;

    let mut candidates = Vec::with_capacity(rows.len());
    for row in &rows {
        let decoded = decode_agent_row(row)?;
        // Take the audience keyed to the row's OWN id, so the arms describe the row
        // they travel with; a creator-only candidate has an EMPTY (not absent)
        // audience — nobody but its creator can see it.
        let audience = audiences
            .remove(decoded.source.as_str())
            .unwrap_or_default();
        candidates.push(AgentDetailFacts::from_source(
            decoded.source,
            decoded.creator,
            audience.direct_pair_human_endpoints,
            audience.shared_workspace_members,
            decoded.materialization_status,
            decoded.projection,
        ));
    }

    Ok(candidates)
}
