//! SQLite cold-read adapter for the authenticated `GET /api/workspaces` envelope.
//!
//! Workspace identities are read from one deferred snapshot.
//! Sidebar ordering belongs to GET /api/sort/sidebar.
//!
//! Every committed row is decoded through the domain's typed identities and
//! closed enums. A stored value that cannot be a legal domain value — an empty or
//! malformed identity, a
//! non-null column holding the wrong type, unreadable JSON — is corruption of a
//! committed row and surfaces as a `PersistenceFailure` (a safe `500`), never a
//! product `4xx` and never coerced to a default. `name` is the one non-identity
//! string and is carried verbatim: the schema places no non-empty invariant on it.

use std::{path::Path, time::Duration};

use silverhand_backend_core::{
    foundation::identity::{AgentId, ConversationId, UserId, WorkspaceId},
    foundation::persistence::PersistenceFailure,
    identity_access::workspace_cold_read::{
        WorkspaceColdRead, WorkspaceColdReadUnitOfWork, compose_workspace_cold_read,
    },
    identity_access::workspace_face::{WorkspaceFace, WorkspaceFaceReadPort},
};
use sqlx::{Row, Sqlite, Transaction};
use thiserror::Error;

const DEFERRED_READ_BEGIN: &str = "BEGIN DEFERRED";
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// The visible-workspace list for one human viewer.
///
/// The projected column set and the two DM subqueries are the published contract
/// reverse derived from the TypeScript `WORKSPACE_SELECT` / `toViewerWorkspace`,
/// not from any single consumer's read subset. Membership is visibility for a
/// human principal (`canViewWorkspace` reduces to `isWorkspaceMember`), so the
/// filter is pushed into SQL as an `EXISTS` over `workspace_users`; the TS route
/// reads all rows then app-filters, but the surviving set and its order are
/// identical. The ORDER BY replicates ONLY the two published keys — home-agent
/// workspaces last, then `updated_at DESC` — with no `id` tie-break, so the
/// parity oracle keeps its timestamps mutually unequal.
const SELECT_VISIBLE_WORKSPACES: &str = "\
SELECT \
    w.id, w.name, w.description, w.owner_agent_id, w.owner_user_id, \
    w.archived_at, w.created_at, w.updated_at, \
    CASE WHEN w.owner_agent_id IS NOT NULL THEN ( \
        SELECT c.id \
          FROM conversations c \
          LEFT JOIN agents owner ON owner.id = w.owner_agent_id \
         WHERE c.workspace_id = w.id \
           AND c.roster_policy = 'fixed' \
         ORDER BY CASE \
                    WHEN c.title = 'Onboarding' THEN 0 \
                    WHEN owner.id IS NOT NULL AND c.title = 'dm-' || owner.name THEN 1 \
                    ELSE 2 \
                  END, \
                  c.created_at ASC \
         LIMIT 1 \
    ) ELSE NULL END AS dm_channel_id, \
    CASE WHEN w.owner_agent_id IS NOT NULL THEN ( \
        SELECT COALESCE(json_group_array(id), '[]') \
          FROM ( \
            SELECT c.id \
              FROM conversations c \
              LEFT JOIN agents owner ON owner.id = w.owner_agent_id \
             WHERE c.workspace_id = w.id \
               AND c.roster_policy = 'fixed' \
             ORDER BY CASE \
                        WHEN c.title = 'Onboarding' THEN 0 \
                        WHEN owner.id IS NOT NULL AND c.title = 'dm-' || owner.name THEN 1 \
                        ELSE 2 \
                      END, \
                      c.created_at ASC \
          ) \
    ) ELSE '[]' END AS dm_channel_ids_json \
  FROM workspaces w \
 WHERE EXISTS ( \
        SELECT 1 FROM workspace_users wu \
         WHERE wu.workspace_id = w.id AND wu.user_id = ? \
      ) \
 ORDER BY CASE WHEN w.owner_agent_id IS NULL THEN 0 ELSE 1 END, \
          w.updated_at DESC";

#[derive(Clone)]
pub struct SqliteWorkspaceColdRead {
    pool: crate::foundation::database::SqliteDatabase,
}

/// Read failures surfaced only as the source of a [`PersistenceFailure`]. Every
/// variant here is corruption of a committed row or a driver fault, never a
/// product-level refusal.
#[derive(Debug, Error)]
pub enum WorkspaceColdReadError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("stored dm-channel id list is not readable JSON")]
    MalformedDmChannelIds(#[source] serde_json::Error),
    #[error("stored column {field} does not name a usable identity")]
    MalformedIdentity { field: &'static str },
}

impl SqliteWorkspaceColdRead {
    pub fn from_pool(pool: impl Into<crate::foundation::database::SqliteDatabase>) -> Self {
        let pool = pool.into();
        Self { pool }
    }

    pub async fn open(path: impl AsRef<Path>) -> Result<Self, WorkspaceColdReadError> {
        let pool = crate::foundation::database::store_pool(path.as_ref(), BUSY_TIMEOUT).await?;
        Ok(Self { pool })
    }

    async fn begin_snapshot(&self) -> Result<ColdReadSnapshot<'_>, WorkspaceColdReadError> {
        let tx = self.pool.begin_with(DEFERRED_READ_BEGIN).await?;
        Ok(ColdReadSnapshot { tx })
    }
}

impl WorkspaceColdReadUnitOfWork for SqliteWorkspaceColdRead {
    async fn load_workspace_cold_read(
        &self,
        viewer: &UserId,
    ) -> Result<WorkspaceColdRead, PersistenceFailure> {
        let mut snapshot = self
            .begin_snapshot()
            .await
            .map_err(PersistenceFailure::from_source)?;
        // Workspace identity is read through one snapshot; sidebar ordering is
        // a separate endpoint and is not included in this read.
        let read = compose_workspace_cold_read(&mut snapshot, viewer).await?;
        snapshot
            .tx
            .commit()
            .await
            .map_err(|error| PersistenceFailure::from_source(WorkspaceColdReadError::Sql(error)))?;
        Ok(read)
    }
}

/// One deferred-read transaction for workspace identity. It is
/// private to the adapter: the application depends only on the unit of work.
struct ColdReadSnapshot<'p> {
    tx: Transaction<'p, Sqlite>,
}

impl WorkspaceFaceReadPort for ColdReadSnapshot<'_> {
    async fn visible_workspaces(
        &mut self,
        viewer: &UserId,
    ) -> Result<Vec<WorkspaceFace>, PersistenceFailure> {
        self.read_visible_workspaces(viewer)
            .await
            .map_err(PersistenceFailure::from_source)
    }
}

impl ColdReadSnapshot<'_> {
    async fn read_visible_workspaces(
        &mut self,
        viewer: &UserId,
    ) -> Result<Vec<WorkspaceFace>, WorkspaceColdReadError> {
        let rows = sqlx::query(SELECT_VISIBLE_WORKSPACES)
            .bind(viewer.as_str())
            .fetch_all(&mut *self.tx)
            .await?;
        let mut faces = Vec::with_capacity(rows.len());
        for row in &rows {
            faces.push(decode_face(row)?);
        }
        Ok(faces)
    }
}

fn decode_face(row: &sqlx::sqlite::SqliteRow) -> Result<WorkspaceFace, WorkspaceColdReadError> {
    let dm_channel_ids_json: String = row.try_get("dm_channel_ids_json")?;
    let dm_channel_ids = decode_dm_channel_ids(&dm_channel_ids_json)?;
    Ok(WorkspaceFace {
        id: parse_identity::<WorkspaceId>(row.try_get("id")?, "workspaces.id")?,
        name: row.try_get("name")?,
        description: row.try_get("description")?,
        owner_agent_id: parse_optional_identity::<AgentId>(
            row.try_get("owner_agent_id")?,
            "workspaces.owner_agent_id",
        )?,
        owner_user_id: parse_optional_identity::<UserId>(
            row.try_get("owner_user_id")?,
            "workspaces.owner_user_id",
        )?,
        dm_channel_id: parse_optional_identity::<ConversationId>(
            row.try_get("dm_channel_id")?,
            "workspaces.dm_channel_id",
        )?,
        dm_channel_ids,
        archived_at: row.try_get("archived_at")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

fn decode_dm_channel_ids(json: &str) -> Result<Vec<ConversationId>, WorkspaceColdReadError> {
    let raw: Vec<String> =
        serde_json::from_str(json).map_err(WorkspaceColdReadError::MalformedDmChannelIds)?;
    raw.into_iter()
        .map(|id| parse_identity::<ConversationId>(id, "workspaces.dm_channel_ids[]"))
        .collect()
}

/// Every committed identity coordinate is parsed through the domain's fallible
/// parser; an empty or otherwise unusable value is corruption of a committed row.
trait ParsableIdentity: Sized {
    fn parse_identity(raw: String) -> Option<Self>;
}

macro_rules! parsable_identity {
    ($ty:ty) => {
        impl ParsableIdentity for $ty {
            fn parse_identity(raw: String) -> Option<Self> {
                <$ty>::parse(raw).ok()
            }
        }
    };
}

parsable_identity!(WorkspaceId);
parsable_identity!(AgentId);
parsable_identity!(UserId);
parsable_identity!(ConversationId);

fn parse_identity<T: ParsableIdentity>(
    raw: String,
    field: &'static str,
) -> Result<T, WorkspaceColdReadError> {
    T::parse_identity(raw).ok_or(WorkspaceColdReadError::MalformedIdentity { field })
}

fn parse_optional_identity<T: ParsableIdentity>(
    raw: Option<String>,
    field: &'static str,
) -> Result<Option<T>, WorkspaceColdReadError> {
    match raw {
        Some(raw) => parse_identity::<T>(raw, field).map(Some),
        None => Ok(None),
    }
}
