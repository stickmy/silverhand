//! SQLite projection and mutation unit for viewer-private saved messages.

mod mutation;
mod read;

use silverhand_backend_core::{
    conversation_messaging::messaging::{
        collaboration::saved::SavedEntry,
        environment::{SendClock, SendEntropy},
    },
    foundation::persistence::PersistenceFailure,
};
use thiserror::Error;

use crate::conversation_messaging::messaging::{HostEnvironment, MessagingStore};

#[derive(Clone)]
pub struct SqliteSavedStore<E = HostEnvironment> {
    pub(super) pool: crate::foundation::database::SqliteDatabase,
    pub(super) environment: E,
}

impl<E: Clone> MessagingStore<E> {
    pub fn saved(&self) -> SqliteSavedStore<E> {
        SqliteSavedStore {
            pool: self.pool.clone(),
            environment: self.environment.clone(),
        }
    }
}

#[derive(Debug, Error)]
pub enum SavedStoreError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("stored saved-message identity is malformed")]
    MalformedIdentity,
    #[error("stored saved timestamp or message ordinal is outside its domain")]
    MalformedOrdinal,
    #[error("stored saved-message placement is outside the published domain")]
    MalformedPlacement,
    #[error("stored saved row and message disagree on conversation")]
    MismatchedConversation,
    #[error("stored message metadata is malformed")]
    MalformedMetadata(#[source] serde_json::Error),
    #[error("stored message metadata is not an object")]
    MalformedMetadataShape,
    #[error("stored message author kind is outside the published domain")]
    MalformedAuthor,
    #[error("stored linked-task projection field {field} is malformed")]
    MalformedLinkedTask { field: &'static str },
    #[error("environment could not mint the saved-message fact")]
    Environment(
        #[source]
        silverhand_backend_core::conversation_messaging::messaging::EnvironmentUnavailable,
    ),
}

impl From<SavedStoreError> for PersistenceFailure {
    fn from(value: SavedStoreError) -> Self {
        Self::from_source(value)
    }
}

impl<E> SqliteSavedStore<E>
where
    E: SendClock + SendEntropy + Send + Sync,
{
    fn new_entry_id(
        &self,
    ) -> Result<
        silverhand_backend_core::conversation_messaging::messaging::collaboration::saved::SavedEntryId,
        PersistenceFailure,
    >{
        silverhand_backend_core::conversation_messaging::messaging::collaboration::saved::SavedEntryId::mint(
            &self.environment,
        )
        .map_err(|error| SavedStoreError::Environment(error).into())
    }

    fn now(
        &self,
    ) -> Result<
        silverhand_backend_core::conversation_messaging::messaging::collaboration::saved::SavedAt,
        PersistenceFailure,
    > {
        let value = self
            .environment
            .now_unix_ms()
            .map_err(SavedStoreError::Environment)?;
        silverhand_backend_core::conversation_messaging::messaging::collaboration::saved::SavedAt::new(value)
            .map_err(|_| SavedStoreError::MalformedOrdinal.into())
    }
}

pub(super) const SELECT_JOINED: &str = "SELECT sm.id AS saved_id, sm.user_id AS saved_user_id, sm.saved_at, sm.conversation_id AS saved_conversation_id, m.id AS message_id, m.conversation_id AS message_conversation_id, m.from_kind, m.from_agent_id, m.from_user_id, m.author_id, m.author_name, m.author_display_name, m.content, m.metadata, m.seq, m.created_at, m.thread_root_id, c.placement_kind, c.workspace_id, CASE WHEN c.placement_kind = 'workspace' THEN c.title ELSE NULL END AS channel_name, lt.id AS task_id, lt.conversation_id AS task_conversation_id, lt.task_number, lt.title AS task_title, lt.status AS task_status, lt.assignee_agent_id, lt.assignee_user_id, lta.display_name AS assignee_agent_name, ltu.display_name AS assignee_user_name FROM saved_messages sm JOIN messages m ON m.id = sm.message_id JOIN conversations c ON c.id = sm.conversation_id LEFT JOIN tasks lt ON lt.source_message_id = m.id LEFT JOIN agents lta ON lta.id = lt.assignee_agent_id LEFT JOIN users ltu ON ltu.id = lt.assignee_user_id";

pub(super) async fn decode_entries(
    conn: &mut sqlx::SqliteConnection,
    rows: Vec<sqlx::sqlite::SqliteRow>,
) -> Result<Vec<SavedEntry>, PersistenceFailure> {
    read::decode_entries(conn, rows).await
}
