//! Single-statement scope reader over the production SQLite schema.
//!
//! It reads a workspace's existence, ownership, and BOTH membership edges (the
//! human's `workspace_users` row and the agent's `workspace_members` row) in ONE
//! statement, then returns them as [`AgentWorkspaceScopeFacts`] source-bound to the
//! FULL triple it read for — the workspace id, the viewing human, and the subject
//! agent.
//!
//! The three coordinates the facts carry are SELECTed back out of the SAME `request`
//! CTE row the two membership subqueries filter on, then strictly decoded — they are
//! not re-cloned from the method parameters. This makes the route's coordinate compare
//! load-bearing against a `.bind()` order slip or a query keyed on the wrong bound
//! coordinate: such a drift changes a projected coordinate, which no longer equals what
//! the route asked, so the route rejects it as a resolver defect (a safe `500`) before
//! any daemon round trip.
//!
//! That compare does NOT, on its own, catch an edge wired to the wrong `request` column
//! (a subquery projecting the right coordinate but filtering on another) — the coordinate
//! would still match. The real-SQL workspace_data_plane_scope tests vary the requested
//! viewer and agent independently to guard the resulting membership decisions.
//!
//! It still decides nothing: which edge counts (home-owner identity vs the explicit
//! `workspace_members` edge) is the pure resolver's choice, so no role or membership
//! rule is duplicated in SQL here.
//!
//! `owner_agent_id` present marks a private agent-home container. A committed owner id
//! — or an echoed request coordinate — the domain refuses is corruption of the read,
//! not a product answer, so it fails the read as a `PersistenceFailure` (a safe `500`)
//! rather than resolving to an unusable value.

use std::{path::Path, time::Duration};

use silverhand_backend_core::{
    foundation::identity::{AgentId, UserId, WorkspaceId},
    foundation::persistence::PersistenceFailure,
    workspace_data_plane::{AgentWorkspaceScopeFacts, AgentWorkspaceScopeRead},
};
use thiserror::Error;

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// ONE statement over ONE snapshot: a `request` CTE pins the three requested
/// coordinates as columns, and every downstream fact keys off that single source —
/// existence and ownership from the LEFT-JOINed workspace row, and both membership
/// edges from subqueries that reference `request.workspace_id` / `request.viewer_id`
/// / `request.agent_id`, never a re-bound literal. The same three coordinate columns
/// are projected back so the facts are bound to exactly what the edges were read for.
/// `?1`/`?2`/`?3` are reused by index so each is bound exactly once.
const READ_SCOPE_FACTS: &str = "\
WITH request AS (SELECT ?1 AS workspace_id, ?2 AS viewer_id, ?3 AS agent_id) \
SELECT \
    request.workspace_id AS requested_workspace_id, \
    request.viewer_id AS read_viewer_id, \
    request.agent_id AS read_agent_id, \
    (w.id IS NOT NULL) AS present, \
    w.owner_agent_id AS owner_agent_id, \
    EXISTS( \
        SELECT 1 FROM workspace_users wu \
        WHERE wu.workspace_id = request.workspace_id AND wu.user_id = request.viewer_id \
    ) AS human_is_member, \
    EXISTS( \
        SELECT 1 FROM workspace_members wm \
        WHERE wm.workspace_id = request.workspace_id AND wm.agent_id = request.agent_id \
    ) AS agent_is_workspace_member \
FROM request \
LEFT JOIN workspaces w ON w.id = request.workspace_id";

/// The row shape [`READ_SCOPE_FACTS`] returns: the three echoed request coordinates,
/// existence, the nullable owner, and the two membership edges as `0`/`1`.
type ScopeRow = (String, String, String, i64, Option<String>, i64, i64);

/// Reads the workspace-scope facts behind the agent-pane data plane. Holds only a
/// pool; every read is one statement over the current schema.
#[derive(Clone)]
pub struct SqliteWorkspaceScopeStore {
    pool: crate::foundation::database::SqliteDatabase,
}

/// A pool that could not be opened as a consumer of an already-migrated database.
#[derive(Debug, Error)]
pub enum SqliteWorkspaceScopeStoreError {
    #[error(transparent)]
    Open(#[from] sqlx::Error),
}

/// A committed `workspaces.owner_agent_id`, or an echoed request coordinate, the
/// identity domain refuses. Reaches the port's caller as the source of a
/// `PersistenceFailure` (a safe `500`), never coerced to a sentinel and never a
/// product answer.
#[derive(Debug, Error)]
#[error("scope read returned a coordinate the identity domain refuses")]
struct CorruptScopeCoordinate;

impl SqliteWorkspaceScopeStore {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, SqliteWorkspaceScopeStoreError> {
        Self::open_with_busy_timeout(path, BUSY_TIMEOUT).await
    }

    /// Opens the pool as a consumer: `create_if_missing` is off, so a database
    /// that has never been migrated is a hard open error rather than a silently
    /// created empty file this adapter has no schema to fill.
    pub async fn open_with_busy_timeout(
        path: impl AsRef<Path>,
        busy_timeout: Duration,
    ) -> Result<Self, SqliteWorkspaceScopeStoreError> {
        let pool = crate::foundation::database::store_pool(path.as_ref(), busy_timeout).await?;

        Ok(Self { pool })
    }

    /// Share an already-open pool, for a composition root that keeps one pool per
    /// database rather than opening a second for this reader.
    pub fn from_pool(pool: impl Into<crate::foundation::database::SqliteDatabase>) -> Self {
        let pool = pool.into();
        Self { pool }
    }
}

impl AgentWorkspaceScopeRead for SqliteWorkspaceScopeStore {
    async fn read_agent_workspace_scope(
        &self,
        workspace: &WorkspaceId,
        viewer: &UserId,
        agent: &AgentId,
    ) -> Result<AgentWorkspaceScopeFacts, PersistenceFailure> {
        // The single statement already reads existence, ownership and both edges
        // consistently; an outer BEGIN/COMMIT adds no cross-query guarantee.
        let (
            requested_workspace_id,
            read_viewer_id,
            read_agent_id,
            present,
            owner_column,
            human_is_member,
            agent_is_workspace_member,
        ): ScopeRow = sqlx::query_as(READ_SCOPE_FACTS)
            .bind(workspace.as_str())
            .bind(viewer.as_str())
            .bind(agent.as_str())
            .fetch_one(&self.pool)
            .await
            .map_err(PersistenceFailure::from_source)?;

        // Decode the three coordinates the edges were read against back into the
        // domain. These are the SAME columns the membership subqueries filter on, so
        // the facts are bound to what was actually queried; a value the domain
        // refuses is a corrupt read, a safe 500, never a product answer.
        let requested = WorkspaceId::parse(requested_workspace_id)
            .map_err(|_| PersistenceFailure::from_source(CorruptScopeCoordinate))?;
        let read_viewer = UserId::parse(read_viewer_id)
            .map_err(|_| PersistenceFailure::from_source(CorruptScopeCoordinate))?;
        let read_agent = AgentId::parse(read_agent_id)
            .map_err(|_| PersistenceFailure::from_source(CorruptScopeCoordinate))?;
        let owner_agent_id = match owner_column {
            Some(raw) => Some(
                AgentId::parse(raw)
                    .map_err(|_| PersistenceFailure::from_source(CorruptScopeCoordinate))?,
            ),
            None => None,
        };

        Ok(AgentWorkspaceScopeFacts::new(
            requested,
            read_viewer,
            read_agent,
            present != 0,
            owner_agent_id,
            human_is_member != 0,
            agent_is_workspace_member != 0,
        ))
    }
}
