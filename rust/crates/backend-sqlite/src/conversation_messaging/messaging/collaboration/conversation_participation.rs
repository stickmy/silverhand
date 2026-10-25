//! Single SQLite authority for strict human conversation participation.
//!
//! This is deliberately narrower than every product projection that consumes
//! it: no public visibility widening, unread state, roster payload or runtime
//! state is selected. Workspace participation is the intersection of the live
//! workspace, workspace seat and conversation seat; direct-pair participation
//! comes only from its committed endpoints.

use silverhand_backend_core::{
    conversation_messaging::messaging::PrincipalIdentity,
    foundation::identity::{AgentId, ConversationId, UserId, WorkspaceId},
    foundation::persistence::PersistenceFailure,
};
use sqlx::{Row, Sqlite, Transaction, sqlite::SqliteRow};
use thiserror::Error;

use crate::conversation_messaging::messaging::MessagingStore;

use super::activity::valid_title_key_spelling;

/// The single SQL spelling of strict human participation. It returns the
/// actual selected source identity (not a boolean) so every consumer can carry
/// provenance into its core fact. `$conversation` and `$viewer` are SQL
/// expressions supplied by the caller.
macro_rules! human_participation_source {
    ($conversation:literal, $viewer:literal) => {
        concat!(
            "CASE ",
            $conversation,
            ".placement_kind WHEN 'workspace' THEN (SELECT cu.user_id FROM workspaces pw ",
            "JOIN workspace_users wu ON wu.workspace_id=pw.id AND wu.user_id=",
            $viewer,
            " JOIN conversation_users cu ON cu.conversation_id=",
            $conversation,
            ".id AND cu.user_id=wu.user_id WHERE pw.id=",
            $conversation,
            ".workspace_id) WHEN 'direct_pair' THEN CASE WHEN ",
            $conversation,
            ".endpoint_a_kind='human' AND ",
            $conversation,
            ".endpoint_a_id=",
            $viewer,
            " THEN ",
            $conversation,
            ".endpoint_a_id WHEN ",
            $conversation,
            ".endpoint_b_kind='human' AND ",
            $conversation,
            ".endpoint_b_id=",
            $viewer,
            " THEN ",
            $conversation,
            ".endpoint_b_id END END"
        )
    };
}
// Re-exported at `pub(crate)` (not `pub(super)`) so the crate-root
// `agent_runtime_state_read` adapter composes its source-bound list projection
// from the SAME strict human-participation authority this module owns, rather
// than a second SQL spelling of who may read a conversation. Grammar unchanged;
// this only widens the reach of the existing rule.
pub(crate) use human_participation_source;

const SELECT_PARTICIPATING_VIEWER: &str = concat!(
    "WITH p(viewer,conversation) AS (SELECT ?,?) SELECT source_viewer FROM (SELECT ",
    human_participation_source!("c", "(SELECT viewer FROM p)"),
    " AS source_viewer FROM conversations c WHERE c.id=(SELECT conversation FROM p)) WHERE ",
    "source_viewer IS NOT NULL"
);

pub(super) const SELECT_CONVERSATION: &str = "SELECT id,workspace_id,placement_kind,roster_policy,title,title_key,\
     status,visibility,endpoint_a_kind,endpoint_a_id,endpoint_a_deleted_at,\
     endpoint_b_kind,endpoint_b_id,endpoint_b_deleted_at \
     FROM conversations WHERE id=?";

#[derive(Clone)]
pub struct SqliteConversationParticipationRead {
    pool: crate::foundation::database::SqliteDatabase,
}

impl<E> MessagingStore<E> {
    pub fn conversation_participation(&self) -> SqliteConversationParticipationRead {
        SqliteConversationParticipationRead {
            pool: self.pool.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversationParticipationSource {
    viewer: UserId,
    conversation: ConversationId,
}

impl ConversationParticipationSource {
    pub fn viewer(&self) -> &UserId {
        &self.viewer
    }

    pub fn conversation(&self) -> &ConversationId {
        &self.conversation
    }

    pub fn bind(
        self,
        viewer: &UserId,
        conversation: &ConversationId,
    ) -> Result<Self, ConversationParticipationIdentityMismatch> {
        if &self.viewer != viewer || &self.conversation != conversation {
            return Err(ConversationParticipationIdentityMismatch);
        }
        Ok(self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
#[error("conversation participation source differs from the authenticated request")]
pub struct ConversationParticipationIdentityMismatch;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConversationParticipationRefusal {
    NotFound,
    Forbidden,
}

#[derive(Debug, Error)]
pub enum ConversationParticipationStoreError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("stored identity in the conversation-participation snapshot is malformed")]
    MalformedIdentity,
    #[error("stored conversation placement contradicts the current schema")]
    MalformedPlacement,
    #[error("stored conversation status is outside the published closed set")]
    MalformedStatus,
}

impl From<ConversationParticipationStoreError> for PersistenceFailure {
    fn from(value: ConversationParticipationStoreError) -> Self {
        Self::from_source(value)
    }
}

impl SqliteConversationParticipationRead {
    pub async fn read(
        &self,
        viewer: &UserId,
        conversation: &ConversationId,
    ) -> Result<
        Result<ConversationParticipationSource, ConversationParticipationRefusal>,
        PersistenceFailure,
    > {
        self.read_inner(viewer, conversation)
            .await
            .map_err(PersistenceFailure::from)
    }

    async fn read_inner(
        &self,
        viewer: &UserId,
        requested: &ConversationId,
    ) -> Result<
        Result<ConversationParticipationSource, ConversationParticipationRefusal>,
        ConversationParticipationStoreError,
    > {
        let mut tx = self.pool.begin_with("BEGIN DEFERRED").await?;
        let Some(row) = sqlx::query(SELECT_CONVERSATION)
            .bind(requested.as_str())
            .fetch_optional(&mut *tx)
            .await?
        else {
            tx.commit().await?;
            return Ok(Err(ConversationParticipationRefusal::NotFound));
        };
        let (stored, placement) = decode_conversation(&row, requested)?;
        let Some(source_viewer) =
            participating_viewer(&mut tx, viewer, &stored, &placement).await?
        else {
            tx.commit().await?;
            return Ok(Err(ConversationParticipationRefusal::Forbidden));
        };
        let source = ConversationParticipationSource {
            viewer: source_viewer,
            conversation: stored,
        };
        tx.commit().await?;
        Ok(Ok(source))
    }
}

#[derive(Debug)]
pub(super) enum Placement {
    Workspace(WorkspaceId),
    DirectPair(PrincipalIdentity, PrincipalIdentity),
}

pub(super) fn decode_conversation(
    row: &SqliteRow,
    requested: &ConversationId,
) -> Result<(ConversationId, Placement), ConversationParticipationStoreError> {
    let stored = ConversationId::parse(row.try_get::<String, _>("id")?)
        .map_err(|_| ConversationParticipationStoreError::MalformedIdentity)?;
    if &stored != requested {
        return Err(ConversationParticipationStoreError::MalformedIdentity);
    }
    Ok((stored, decode_placement(row)?))
}

pub(super) fn decode_placement(
    row: &SqliteRow,
) -> Result<Placement, ConversationParticipationStoreError> {
    let status: String = row.try_get("status")?;
    if !matches!(
        status.as_str(),
        "starting" | "active" | "idle" | "archived" | "crashed"
    ) {
        return Err(ConversationParticipationStoreError::MalformedStatus);
    }
    let placement: String = row.try_get("placement_kind")?;
    let roster: String = row.try_get("roster_policy")?;
    let visibility: String = row.try_get("visibility")?;
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
            if !matches!(roster.as_str(), "open" | "fixed")
                || !matches!(visibility.as_str(), "private" | "public")
                || (visibility == "public" && roster != "open")
                || title.is_empty()
                || !title_key.as_deref().is_some_and(valid_title_key_spelling)
                || a_kind.is_some()
                || a_id.is_some()
                || a_deleted.is_some()
                || b_kind.is_some()
                || b_id.is_some()
                || b_deleted.is_some()
            {
                return Err(ConversationParticipationStoreError::MalformedPlacement);
            }
            let workspace = WorkspaceId::parse(
                workspace.ok_or(ConversationParticipationStoreError::MalformedPlacement)?,
            )
            .map_err(|_| ConversationParticipationStoreError::MalformedIdentity)?;
            Ok(Placement::Workspace(workspace))
        }
        "direct_pair" => {
            if workspace.is_some()
                || title_key.is_some()
                || roster != "fixed"
                || visibility != "private"
            {
                return Err(ConversationParticipationStoreError::MalformedPlacement);
            }
            let a_coordinate = a_kind
                .as_ref()
                .zip(a_id.as_ref())
                .ok_or(ConversationParticipationStoreError::MalformedPlacement)?;
            let b_coordinate = b_kind
                .as_ref()
                .zip(b_id.as_ref())
                .ok_or(ConversationParticipationStoreError::MalformedPlacement)?;
            if a_coordinate >= b_coordinate {
                return Err(ConversationParticipationStoreError::MalformedPlacement);
            }
            Ok(Placement::DirectPair(
                decode_endpoint(a_kind, a_id)?,
                decode_endpoint(b_kind, b_id)?,
            ))
        }
        _ => Err(ConversationParticipationStoreError::MalformedPlacement),
    }
}

fn decode_endpoint(
    kind: Option<String>,
    id: Option<String>,
) -> Result<PrincipalIdentity, ConversationParticipationStoreError> {
    let id = id
        .filter(|value| !value.is_empty())
        .ok_or(ConversationParticipationStoreError::MalformedIdentity)?;
    match kind.as_deref() {
        Some("human") => UserId::parse(id)
            .map(PrincipalIdentity::Human)
            .map_err(|_| ConversationParticipationStoreError::MalformedIdentity),
        Some("agent") => AgentId::parse(id)
            .map(PrincipalIdentity::Agent)
            .map_err(|_| ConversationParticipationStoreError::MalformedIdentity),
        _ => Err(ConversationParticipationStoreError::MalformedPlacement),
    }
}

pub(super) async fn participating_viewer(
    tx: &mut Transaction<'_, Sqlite>,
    viewer: &UserId,
    conversation: &ConversationId,
    placement: &Placement,
) -> Result<Option<UserId>, ConversationParticipationStoreError> {
    // The already-decoded placement remains a strict precondition; the SQL
    // expression below is the shared population authority used by point and
    // set-shaped consumers alike.
    let _ = placement;
    sqlx::query_scalar::<_, String>(SELECT_PARTICIPATING_VIEWER)
        .bind(viewer.as_str())
        .bind(conversation.as_str())
        .fetch_optional(&mut **tx)
        .await?
        .map(UserId::parse)
        .transpose()
        .map_err(|_| ConversationParticipationStoreError::MalformedIdentity)
}

pub(super) async fn participates(
    tx: &mut Transaction<'_, Sqlite>,
    viewer: &UserId,
    conversation: &ConversationId,
    placement: &Placement,
) -> Result<bool, ConversationParticipationStoreError> {
    Ok(participating_viewer(tx, viewer, conversation, placement)
        .await?
        .is_some())
}
