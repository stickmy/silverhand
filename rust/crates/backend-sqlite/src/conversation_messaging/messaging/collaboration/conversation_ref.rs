//! One-snapshot SQLite projection for `GET /api/conversations/:id/ref`.

use silverhand_backend_core::{
    conversation_messaging::messaging::collaboration::conversation_ref::{
        ConversationRefReadPort, ConversationRefRefusal, ConversationRosterPolicy,
        WorkspaceConversationRef,
    },
    foundation::identity::{ConversationId, UserId, WorkspaceId},
    foundation::persistence::PersistenceFailure,
};
use sqlx::{Row, sqlite::SqliteRow};
use thiserror::Error;

use crate::conversation_messaging::messaging::MessagingStore;

use super::activity::valid_title_key_spelling;

#[derive(Clone)]
pub struct SqliteConversationRefRead {
    pool: crate::foundation::database::SqliteDatabase,
}

impl<E> MessagingStore<E> {
    pub fn conversation_refs(&self) -> SqliteConversationRefRead {
        SqliteConversationRefRead {
            pool: self.pool.clone(),
        }
    }
}

#[derive(Debug, Error)]
pub enum ConversationRefStoreError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("stored conversation identity is malformed")]
    MalformedIdentity,
    #[error("stored conversation placement contradicts the current schema")]
    MalformedPlacement,
    #[error("stored conversation visibility is outside the published closed set")]
    MalformedVisibility,
    #[error("stored conversation status is outside the published closed set")]
    MalformedStatus,
    #[error("stored workspace conversation title is not usable")]
    MalformedTitle,
}

impl From<ConversationRefStoreError> for PersistenceFailure {
    fn from(value: ConversationRefStoreError) -> Self {
        Self::from_source(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Visibility {
    Private,
    Public,
}

#[derive(Debug)]
struct WorkspacePlacement {
    workspace: WorkspaceId,
    roster: ConversationRosterPolicy,
    visibility: Visibility,
    title: String,
    closed_at: Option<i64>,
    archived_at: Option<i64>,
}

#[derive(Debug)]
struct DirectPairPlacement {
    endpoint_a_kind: String,
    endpoint_a_id: String,
    endpoint_b_kind: String,
    endpoint_b_id: String,
}

#[derive(Debug)]
enum Placement {
    Workspace(WorkspacePlacement),
    DirectPair(DirectPairPlacement),
}

impl ConversationRefReadPort for SqliteConversationRefRead {
    type Error = PersistenceFailure;

    async fn resolve(
        &self,
        viewer: &UserId,
        conversation: &ConversationId,
    ) -> Result<Result<WorkspaceConversationRef, ConversationRefRefusal>, Self::Error> {
        self.resolve_inner(viewer, conversation)
            .await
            .map_err(PersistenceFailure::from)
    }
}

impl SqliteConversationRefRead {
    async fn resolve_inner(
        &self,
        viewer: &UserId,
        conversation: &ConversationId,
    ) -> Result<Result<WorkspaceConversationRef, ConversationRefRefusal>, ConversationRefStoreError>
    {
        let mut tx = self.pool.begin_with("BEGIN DEFERRED").await?;
        let Some(row) = sqlx::query(
            "SELECT id,workspace_id,placement_kind,roster_policy,title,title_key,status,visibility,\
             closed_at,archived_at,endpoint_a_kind,endpoint_a_id,endpoint_a_deleted_at,\
             endpoint_b_kind,endpoint_b_id,endpoint_b_deleted_at FROM conversations WHERE id=?",
        )
        .bind(conversation.as_str())
        .fetch_optional(&mut *tx)
        .await?
        else {
            tx.commit().await?;
            return Ok(Err(ConversationRefRefusal::NotFound));
        };
        let stored_id = ConversationId::parse(row.try_get::<String, _>("id")?)
            .map_err(|_| ConversationRefStoreError::MalformedIdentity)?;
        if &stored_id != conversation {
            return Err(ConversationRefStoreError::MalformedIdentity);
        }
        let placement = decode_placement(&row)?;

        let admitted = match &placement {
            Placement::DirectPair(pair) => {
                (pair.endpoint_a_kind == "human" && pair.endpoint_a_id == viewer.as_str())
                    || (pair.endpoint_b_kind == "human" && pair.endpoint_b_id == viewer.as_str())
            }
            Placement::Workspace(workspace) => {
                let workspace_exists: bool =
                    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workspaces WHERE id=?)")
                        .bind(workspace.workspace.as_str())
                        .fetch_one(&mut *tx)
                        .await?;
                if !workspace_exists {
                    false
                } else {
                    let workspace_member: bool = sqlx::query_scalar(
                        "SELECT EXISTS(SELECT 1 FROM workspace_users WHERE workspace_id=? AND user_id=?)",
                    )
                    .bind(workspace.workspace.as_str())
                    .bind(viewer.as_str())
                    .fetch_one(&mut *tx)
                    .await?;
                    if !workspace_member {
                        false
                    } else {
                        let conversation_member: bool = sqlx::query_scalar(
                            "SELECT EXISTS(SELECT 1 FROM conversation_users WHERE conversation_id=? AND user_id=?)",
                        )
                        .bind(conversation.as_str())
                        .bind(viewer.as_str())
                        .fetch_one(&mut *tx)
                        .await?;
                        conversation_member
                            || (workspace.roster == ConversationRosterPolicy::Open
                                && workspace.visibility == Visibility::Public
                                && workspace.archived_at.is_none())
                    }
                }
            }
        };
        if !admitted {
            tx.commit().await?;
            return Ok(Err(ConversationRefRefusal::Forbidden));
        }
        let result = match placement {
            Placement::DirectPair(_) => Err(ConversationRefRefusal::NotFound),
            Placement::Workspace(workspace) => Ok(WorkspaceConversationRef::projected(
                stored_id,
                workspace.roster,
                workspace.title,
                workspace.workspace,
                workspace.closed_at,
                workspace.archived_at,
            )),
        };
        tx.commit().await?;
        Ok(result)
    }
}

fn decode_placement(row: &SqliteRow) -> Result<Placement, ConversationRefStoreError> {
    let placement: String = row.try_get("placement_kind")?;
    let roster =
        ConversationRosterPolicy::parse(row.try_get::<String, _>("roster_policy")?.as_str())
            .map_err(|_| ConversationRefStoreError::MalformedPlacement)?;
    let status: String = row.try_get("status")?;
    if !matches!(
        status.as_str(),
        "starting" | "active" | "idle" | "archived" | "crashed"
    ) {
        return Err(ConversationRefStoreError::MalformedStatus);
    }
    let visibility = match row.try_get::<String, _>("visibility")?.as_str() {
        "private" => Visibility::Private,
        "public" => Visibility::Public,
        _ => return Err(ConversationRefStoreError::MalformedVisibility),
    };
    let workspace: Option<String> = row.try_get("workspace_id")?;
    let title: String = row.try_get("title")?;
    let title_key: Option<String> = row.try_get("title_key")?;
    let a_kind: Option<String> = row.try_get("endpoint_a_kind")?;
    let a_id: Option<String> = row.try_get("endpoint_a_id")?;
    let a_deleted: Option<i64> = row.try_get("endpoint_a_deleted_at")?;
    let b_kind: Option<String> = row.try_get("endpoint_b_kind")?;
    let b_id: Option<String> = row.try_get("endpoint_b_id")?;
    let b_deleted: Option<i64> = row.try_get("endpoint_b_deleted_at")?;
    match placement.as_str() {
        "workspace" => {
            let workspace =
                WorkspaceId::parse(workspace.ok_or(ConversationRefStoreError::MalformedPlacement)?)
                    .map_err(|_| ConversationRefStoreError::MalformedIdentity)?;
            if title.is_empty()
                || !title_key.as_deref().is_some_and(valid_title_key_spelling)
                || a_kind.is_some()
                || a_id.is_some()
                || a_deleted.is_some()
                || b_kind.is_some()
                || b_id.is_some()
                || b_deleted.is_some()
                || (visibility == Visibility::Public && roster != ConversationRosterPolicy::Open)
            {
                return Err(ConversationRefStoreError::MalformedPlacement);
            }
            Ok(Placement::Workspace(WorkspacePlacement {
                workspace,
                roster,
                visibility,
                title,
                closed_at: row.try_get("closed_at")?,
                archived_at: row.try_get("archived_at")?,
            }))
        }
        "direct_pair" => {
            if workspace.is_some()
                || title_key.is_some()
                || roster != ConversationRosterPolicy::Fixed
                || visibility != Visibility::Private
            {
                return Err(ConversationRefStoreError::MalformedPlacement);
            }
            let a_kind = endpoint_kind(a_kind)?;
            let b_kind = endpoint_kind(b_kind)?;
            let a_id = endpoint_id(a_id)?;
            let b_id = endpoint_id(b_id)?;
            if (a_kind.as_str(), a_id.as_str()) >= (b_kind.as_str(), b_id.as_str()) {
                return Err(ConversationRefStoreError::MalformedPlacement);
            }
            Ok(Placement::DirectPair(DirectPairPlacement {
                endpoint_a_kind: a_kind,
                endpoint_a_id: a_id,
                endpoint_b_kind: b_kind,
                endpoint_b_id: b_id,
            }))
        }
        _ => Err(ConversationRefStoreError::MalformedPlacement),
    }
}

fn endpoint_kind(value: Option<String>) -> Result<String, ConversationRefStoreError> {
    match value.as_deref() {
        Some("human" | "agent") => Ok(value.expect("matched Some")),
        _ => Err(ConversationRefStoreError::MalformedPlacement),
    }
}

fn endpoint_id(value: Option<String>) -> Result<String, ConversationRefStoreError> {
    value
        .filter(|value| !value.is_empty())
        .ok_or(ConversationRefStoreError::MalformedIdentity)
}
