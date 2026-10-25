//! Shared SQLite facts for reaction mutation and cold-read adapters.

mod mutation;
mod read;
pub(super) use read::aggregate_facts_in_snapshot;

use silverhand_backend_core::{
    conversation_messaging::messaging::{MessageId, PrincipalIdentity},
    foundation::identity::{AgentId, ConversationId, UserId},
};
use sqlx::{Row, Transaction};
use thiserror::Error;

use crate::conversation_messaging::messaging::MessagingStore;

#[derive(Clone)]
pub struct SqliteReactionStore {
    pub(super) pool: crate::foundation::database::SqliteDatabase,
}

#[derive(Debug, Error)]
pub enum ReactionStoreError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("stored conversation placement is outside the reaction domain")]
    InvalidConversationPlacement,
    #[error("stored reaction identity is malformed")]
    MalformedIdentity,
    #[error("stored reaction emoji is outside the published grammar")]
    MalformedEmoji,
    #[error("stored reaction count is outside the published domain")]
    InvalidCount,
    #[error("stored message author kind is outside the published set")]
    InvalidMessageAuthor,
}

impl<E> MessagingStore<E> {
    pub fn reactions(&self) -> SqliteReactionStore {
        SqliteReactionStore {
            pool: self.pool.clone(),
        }
    }
}

pub(super) struct MessageFacts {
    pub(super) conversation: ConversationId,
    pub(super) from_kind: String,
    pub(super) thread_root: Option<MessageId>,
}

pub(super) struct ConversationFacts {
    pub(super) placement: String,
    pub(super) workspace: Option<String>,
    pub(super) closed: bool,
    pub(super) archived: bool,
    pub(super) endpoint_a: Option<Endpoint>,
    pub(super) endpoint_b: Option<Endpoint>,
}

pub(super) struct Endpoint {
    pub(super) principal: PrincipalIdentity,
    pub(super) deleted_at: Option<i64>,
}

pub(super) async fn message_facts(
    tx: &mut Transaction<'_, sqlx::Sqlite>,
    message: &MessageId,
) -> Result<Option<MessageFacts>, ReactionStoreError> {
    let Some(row) =
        sqlx::query("SELECT conversation_id, from_kind, thread_root_id FROM messages WHERE id = ?")
            .bind(message.as_str())
            .fetch_optional(&mut **tx)
            .await?
    else {
        return Ok(None);
    };
    Ok(Some(MessageFacts {
        conversation: ConversationId::parse(row.try_get::<String, _>("conversation_id")?)
            .map_err(|_| ReactionStoreError::MalformedIdentity)?,
        from_kind: row.try_get("from_kind")?,
        thread_root: row
            .try_get::<Option<String>, _>("thread_root_id")?
            .map(MessageId::parse)
            .transpose()
            .map_err(|_| ReactionStoreError::MalformedIdentity)?,
    }))
}

pub(super) async fn conversation_facts(
    tx: &mut Transaction<'_, sqlx::Sqlite>,
    conversation: &ConversationId,
) -> Result<Option<ConversationFacts>, ReactionStoreError> {
    let Some(row) = sqlx::query(
        "SELECT placement_kind, roster_policy, workspace_id, closed_at, archived_at, \
         endpoint_a_kind, endpoint_a_id, endpoint_a_deleted_at, \
         endpoint_b_kind, endpoint_b_id, endpoint_b_deleted_at \
         FROM conversations WHERE id = ?",
    )
    .bind(conversation.as_str())
    .fetch_optional(&mut **tx)
    .await?
    else {
        return Ok(None);
    };
    let placement: String = row.try_get("placement_kind")?;
    let roster: String = row.try_get("roster_policy")?;
    match (placement.as_str(), roster.as_str()) {
        ("workspace", "open" | "fixed") => Ok(Some(ConversationFacts {
            placement,
            workspace: row.try_get("workspace_id")?,
            closed: row.try_get::<Option<i64>, _>("closed_at")?.is_some(),
            archived: row.try_get::<Option<i64>, _>("archived_at")?.is_some(),
            endpoint_a: None,
            endpoint_b: None,
        })),
        ("direct_pair", "fixed") => Ok(Some(ConversationFacts {
            placement,
            workspace: None,
            closed: row.try_get::<Option<i64>, _>("closed_at")?.is_some(),
            archived: row.try_get::<Option<i64>, _>("archived_at")?.is_some(),
            endpoint_a: Some(decode_endpoint(&row, "endpoint_a")?),
            endpoint_b: Some(decode_endpoint(&row, "endpoint_b")?),
        })),
        _ => Err(ReactionStoreError::InvalidConversationPlacement),
    }
}

fn decode_endpoint(
    row: &sqlx::sqlite::SqliteRow,
    prefix: &str,
) -> Result<Endpoint, ReactionStoreError> {
    let (kind, id, deleted) = match prefix {
        "endpoint_a" => ("endpoint_a_kind", "endpoint_a_id", "endpoint_a_deleted_at"),
        "endpoint_b" => ("endpoint_b_kind", "endpoint_b_id", "endpoint_b_deleted_at"),
        _ => unreachable!("reaction owns two endpoint columns"),
    };
    let id: String = row
        .try_get::<Option<String>, _>(id)?
        .filter(|id| !id.is_empty())
        .ok_or(ReactionStoreError::MalformedIdentity)?;
    let principal = match row.try_get::<Option<String>, _>(kind)?.as_deref() {
        Some("human") => PrincipalIdentity::Human(
            UserId::parse(id).map_err(|_| ReactionStoreError::MalformedIdentity)?,
        ),
        Some("agent") => PrincipalIdentity::Agent(
            AgentId::parse(id).map_err(|_| ReactionStoreError::MalformedIdentity)?,
        ),
        _ => return Err(ReactionStoreError::InvalidConversationPlacement),
    };
    Ok(Endpoint {
        principal,
        deleted_at: row.try_get(deleted)?,
    })
}

pub(super) async fn endpoint_exists(
    tx: &mut Transaction<'_, sqlx::Sqlite>,
    principal: &PrincipalIdentity,
) -> Result<bool, ReactionStoreError> {
    let (query, id) = match principal {
        PrincipalIdentity::Human(id) => ("SELECT 1 FROM users WHERE id = ?", id.as_str()),
        PrincipalIdentity::Agent(id) => ("SELECT 1 FROM agents WHERE id = ?", id.as_str()),
    };
    Ok(sqlx::query(query)
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?
        .is_some())
}
