//! One-snapshot SQLite projection for `GET /api/channels/:id/agents`.

use silverhand_backend_core::{
    conversation_messaging::messaging::collaboration::conversation_agent_directory::{
        ConversationAgentDirectory, ConversationAgentDirectoryReadPort,
        ConversationAgentDirectoryRefusal, ConversationAgentMember, JoinedAt,
    },
    foundation::identity::{AgentId, ConversationId, UserId},
    foundation::persistence::PersistenceFailure,
};
use sqlx::Row;
use thiserror::Error;

use crate::conversation_messaging::messaging::MessagingStore;

use super::conversation_participation::{
    ConversationParticipationStoreError, decode_conversation, participates,
};

#[derive(Clone)]
pub struct SqliteConversationAgentDirectoryRead {
    pool: crate::foundation::database::SqliteDatabase,
}

impl<E> MessagingStore<E> {
    pub fn conversation_agent_directory(&self) -> SqliteConversationAgentDirectoryRead {
        SqliteConversationAgentDirectoryRead {
            pool: self.pool.clone(),
        }
    }
}

#[derive(Debug, Error)]
pub enum ConversationAgentDirectoryStoreError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("conversation access snapshot is malformed")]
    Access(#[from] ConversationParticipationStoreError),
    #[error("stored conversation-agent identity is malformed")]
    MalformedIdentity,
    #[error("stored conversation-agent joined_at is outside its domain")]
    MalformedJoinedAt,
    #[error("stored conversation-agent row does not reference a live agent")]
    MissingAgent,
    #[error("stored conversation-agent rows are not a unique total order")]
    MalformedDirectory,
}

impl From<ConversationAgentDirectoryStoreError> for PersistenceFailure {
    fn from(value: ConversationAgentDirectoryStoreError) -> Self {
        Self::from_source(value)
    }
}

impl ConversationAgentDirectoryReadPort for SqliteConversationAgentDirectoryRead {
    type Error = PersistenceFailure;

    async fn read(
        &self,
        viewer: &UserId,
        conversation: &ConversationId,
    ) -> Result<Result<ConversationAgentDirectory, ConversationAgentDirectoryRefusal>, Self::Error>
    {
        self.read_inner(viewer, conversation)
            .await
            .map_err(PersistenceFailure::from)
    }
}

impl SqliteConversationAgentDirectoryRead {
    async fn read_inner(
        &self,
        viewer: &UserId,
        conversation: &ConversationId,
    ) -> Result<
        Result<ConversationAgentDirectory, ConversationAgentDirectoryRefusal>,
        ConversationAgentDirectoryStoreError,
    > {
        let mut tx = self.pool.begin_with("BEGIN DEFERRED").await?;
        let Some(row) = sqlx::query(
            "SELECT id,workspace_id,placement_kind,roster_policy,title,title_key,status,visibility,\
             endpoint_a_kind,endpoint_a_id,endpoint_a_deleted_at,\
             endpoint_b_kind,endpoint_b_id,endpoint_b_deleted_at \
             FROM conversations WHERE id=?",
        )
        .bind(conversation.as_str())
        .fetch_optional(&mut *tx)
        .await?
        else {
            tx.commit().await?;
            return Ok(Err(ConversationAgentDirectoryRefusal::NotFound));
        };
        let (stored, placement) = decode_conversation(&row, conversation)?;
        if !participates(&mut tx, viewer, conversation, &placement).await? {
            tx.commit().await?;
            return Ok(Err(ConversationAgentDirectoryRefusal::Forbidden));
        }

        let rows = sqlx::query(
            "SELECT ca.conversation_id,ca.agent_id,ca.joined_at,\
             EXISTS(SELECT 1 FROM agents a WHERE a.id=ca.agent_id) AS agent_exists \
             FROM conversation_agents ca WHERE ca.conversation_id=? \
             ORDER BY ca.joined_at ASC,ca.agent_id ASC",
        )
        .bind(conversation.as_str())
        .fetch_all(&mut *tx)
        .await?;
        let mut members = Vec::with_capacity(rows.len());
        for row in rows {
            let row_conversation =
                ConversationId::parse(row.try_get::<String, _>("conversation_id")?)
                    .map_err(|_| ConversationAgentDirectoryStoreError::MalformedIdentity)?;
            if row_conversation != stored {
                return Err(ConversationAgentDirectoryStoreError::MalformedIdentity);
            }
            let agent = AgentId::parse(row.try_get::<String, _>("agent_id")?)
                .map_err(|_| ConversationAgentDirectoryStoreError::MalformedIdentity)?;
            if row.try_get::<i64, _>("agent_exists")? != 1 {
                return Err(ConversationAgentDirectoryStoreError::MissingAgent);
            }
            let joined_at = JoinedAt::new(row.try_get("joined_at")?)
                .map_err(|_| ConversationAgentDirectoryStoreError::MalformedJoinedAt)?;
            members.push(ConversationAgentMember::projected(
                row_conversation,
                agent,
                joined_at,
            ));
        }
        let directory = ConversationAgentDirectory::projected(stored, members)
            .map_err(|_| ConversationAgentDirectoryStoreError::MalformedDirectory)?;
        tx.commit().await?;
        Ok(Ok(directory))
    }
}
