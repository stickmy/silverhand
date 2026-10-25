//! SQLite adapter for the single-workspace detail read behind
//! `GET /api/workspaces/:id`.
//!
//! The port reports the RAW read fact for the authenticated viewer — the row's
//! published face plus the `workspace_users.user_id` the viewer's membership row
//! matched (or `None`) — and never a verdict. Existence, visibility, and the
//! identity bind are the job of
//! [`classify_workspace_view`](silverhand_backend_core::identity_access::workspace_detail_read::classify_workspace_view)
//! at the authenticated boundary. The face and the membership subject are read on
//! ONE statement, so the two facts cannot sample different instants.
//!
//! The face projection and its two DM subqueries are the SAME published contract
//! the list read (`GET /api/workspaces`) emits, reverse derived from
//! `toViewerWorkspace`; this read simply scopes it to one id and adds the viewer's
//! membership subject. Every committed identity coordinate is decoded through the
//! domain's fallible parser, so a malformed stored id is corruption of a committed
//! row (a safe `500`), never a product `4xx`.

use std::{path::Path, time::Duration};

use silverhand_backend_core::{
    foundation::identity::{AgentId, ConversationId, UserId, WorkspaceId},
    foundation::persistence::PersistenceFailure,
    identity_access::workspace_detail_read::{
        WorkspaceDetailForViewer, WorkspaceDetailReadUnitOfWork,
    },
    identity_access::workspace_face::WorkspaceFace,
};
use sqlx::{Row, SqlitePool};
use thiserror::Error;

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// One workspace's published face for a viewer, with the membership subject the
/// viewer matched. The face columns and DM subqueries mirror the list read's
/// `SELECT_VISIBLE_WORKSPACES` exactly (the same wire contract); the difference is
/// this read keys on `w.id = ?` rather than a membership `EXISTS`, and projects the
/// membership subject as a nullable column so a non-member still yields a row (the
/// existence-before-visibility distinction the classifier needs). The two `?`
/// placeholders bind in text order: the membership subject first, then the id.
const SELECT_WORKSPACE_DETAIL: &str = "\
SELECT \
    w.id, w.name, w.description, w.owner_agent_id, w.owner_user_id, \
    w.archived_at, w.created_at, w.updated_at, \
    ( \
        SELECT wu.user_id FROM workspace_users wu \
         WHERE wu.workspace_id = w.id AND wu.user_id = ? \
    ) AS membership_subject, \
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
 WHERE w.id = ?";

#[derive(Clone)]
pub struct SqliteWorkspaceDetailRead {
    pool: SqlitePool,
}

/// Read failures surfaced only as the source of a [`PersistenceFailure`]. Every
/// variant is corruption of a committed row or a driver fault, never a product
/// refusal.
#[derive(Debug, Error)]
pub enum WorkspaceDetailReadError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("stored dm-channel id list is not readable JSON")]
    MalformedDmChannelIds(#[source] serde_json::Error),
    #[error("stored column {field} does not name a usable identity")]
    MalformedIdentity { field: &'static str },
}

impl SqliteWorkspaceDetailRead {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, WorkspaceDetailReadError> {
        let pool = crate::foundation::database::store_pool(path.as_ref(), BUSY_TIMEOUT).await?;
        Ok(Self { pool })
    }

    /// Construct over an already-open pool, so a composition root that shares one
    /// pool across adapters need not reopen the file.
    pub fn from_pool(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

impl WorkspaceDetailReadUnitOfWork for SqliteWorkspaceDetailRead {
    async fn load_workspace_detail(
        &self,
        viewer: &UserId,
        id: &WorkspaceId,
    ) -> Result<Option<WorkspaceDetailForViewer>, PersistenceFailure> {
        let row = sqlx::query(SELECT_WORKSPACE_DETAIL)
            .bind(viewer.as_str())
            .bind(id.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(|error| {
                PersistenceFailure::from_source(WorkspaceDetailReadError::Sql(error))
            })?;
        match row {
            None => Ok(None),
            Some(row) => decode_detail(&row)
                .map(Some)
                .map_err(PersistenceFailure::from_source),
        }
    }
}

fn decode_detail(
    row: &sqlx::sqlite::SqliteRow,
) -> Result<WorkspaceDetailForViewer, WorkspaceDetailReadError> {
    let dm_channel_ids_json: String = row.try_get("dm_channel_ids_json")?;
    let dm_channel_ids = decode_dm_channel_ids(&dm_channel_ids_json)?;
    let face = WorkspaceFace {
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
    };
    let membership_subject = parse_optional_identity::<UserId>(
        row.try_get("membership_subject")?,
        "workspace_users.user_id",
    )?;
    Ok(WorkspaceDetailForViewer {
        face,
        membership_subject,
    })
}

fn decode_dm_channel_ids(json: &str) -> Result<Vec<ConversationId>, WorkspaceDetailReadError> {
    let raw: Vec<String> =
        serde_json::from_str(json).map_err(WorkspaceDetailReadError::MalformedDmChannelIds)?;
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
) -> Result<T, WorkspaceDetailReadError> {
    T::parse_identity(raw).ok_or(WorkspaceDetailReadError::MalformedIdentity { field })
}

fn parse_optional_identity<T: ParsableIdentity>(
    raw: Option<String>,
    field: &'static str,
) -> Result<Option<T>, WorkspaceDetailReadError> {
    match raw {
        Some(raw) => parse_identity::<T>(raw, field).map(Some),
        None => Ok(None),
    }
}
