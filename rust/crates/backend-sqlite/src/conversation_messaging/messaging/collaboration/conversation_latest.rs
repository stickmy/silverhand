//! SQLite projection for the viewer's per-conversation latest message summaries.

use silverhand_backend_core::{
    conversation_messaging::messaging::{
        FromKind, MessageId, MessageSeq, ThreadRootId,
        collaboration::{
            activity::ActivityPreview,
            conversation_latest::{ConversationLatest, ConversationLatestReadPort},
        },
    },
    foundation::identity::{ConversationId, UserId},
    foundation::persistence::PersistenceFailure,
};
use sqlx::Row;
use thiserror::Error;

use crate::conversation_messaging::messaging::{
    MessagingStore,
    author::{AuthorEvidence, resolve_authors},
};

use super::{
    conversation_message_window::{ConversationMessageWindowStoreError, decode_attachments},
    conversation_participation::human_participation_source,
};

// Two index probes on `(conversation_id, from_kind, seq)` instead of `IN`, which
// SQLite cannot answer with a single min/max lookup.
const LIST_LATEST: &str = concat!(
    "WITH p(viewer) AS (SELECT ?), candidate(id) AS (\
       SELECT cu.conversation_id FROM conversation_users cu WHERE cu.user_id=(SELECT viewer FROM p) \
       UNION SELECT c.id FROM conversations c WHERE c.placement_kind='direct_pair' AND \
       ((c.endpoint_a_kind='human' AND c.endpoint_a_id=(SELECT viewer FROM p)) OR \
        (c.endpoint_b_kind='human' AND c.endpoint_b_id=(SELECT viewer FROM p)))\
     ), visible(id) AS (\
       SELECT c.id FROM candidate JOIN conversations c ON c.id=candidate.id WHERE (",
    human_participation_source!("c", "(SELECT viewer FROM p)"),
    ") IS NOT NULL\
     ), head(id,seq) AS (\
       SELECT v.id, MAX(\
         COALESCE((SELECT MAX(seq) FROM messages WHERE conversation_id=v.id AND from_kind='user'),-1),\
         COALESCE((SELECT MAX(seq) FROM messages WHERE conversation_id=v.id AND from_kind='agent'),-1)\
       ) FROM visible v\
     ) SELECT m.conversation_id,m.id,m.seq,m.created_at,m.thread_root_id,m.content,m.metadata,\
       m.from_kind,m.from_agent_id,m.from_user_id,m.author_id,m.author_name,m.author_display_name \
     FROM head h JOIN messages m ON m.conversation_id=h.id AND m.seq=h.seq \
     WHERE h.seq>=0 ORDER BY m.created_at DESC, m.conversation_id ASC"
);

#[derive(Clone)]
pub struct SqliteConversationLatestRead {
    pool: crate::foundation::database::SqliteDatabase,
}

impl<E> MessagingStore<E> {
    pub fn conversation_latest(&self) -> SqliteConversationLatestRead {
        SqliteConversationLatestRead {
            pool: self.pool().clone(),
        }
    }
}

#[derive(Debug, Error)]
pub enum ConversationLatestStoreError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("stored latest-message identity is malformed in {0}")]
    MalformedIdentity(&'static str),
    #[error("stored latest-message author is not a user or agent")]
    MalformedAuthor,
    #[error("stored latest-message attachments are malformed")]
    Attachments(#[source] ConversationMessageWindowStoreError),
    #[error("author projection failed")]
    Author(#[source] PersistenceFailure),
}

impl From<ConversationLatestStoreError> for PersistenceFailure {
    fn from(value: ConversationLatestStoreError) -> Self {
        Self::from_source(value)
    }
}

impl ConversationLatestReadPort for SqliteConversationLatestRead {
    type Error = PersistenceFailure;

    async fn list_latest(&self, viewer: &UserId) -> Result<Vec<ConversationLatest>, Self::Error> {
        self.list_inner(viewer)
            .await
            .map_err(PersistenceFailure::from)
    }
}

struct Raw {
    conversation: ConversationId,
    message: MessageId,
    seq: MessageSeq,
    created_at_ms: i64,
    thread_root: Option<ThreadRootId>,
    content: String,
    attachment_count: usize,
}

impl SqliteConversationLatestRead {
    async fn list_inner(
        &self,
        viewer: &UserId,
    ) -> Result<Vec<ConversationLatest>, ConversationLatestStoreError> {
        let mut tx = self.pool.begin_with("BEGIN DEFERRED").await?;
        let rows = sqlx::query(LIST_LATEST)
            .bind(viewer.as_str())
            .fetch_all(&mut *tx)
            .await?;
        let mut raws = Vec::with_capacity(rows.len());
        let mut evidence = Vec::with_capacity(rows.len());
        for row in rows {
            let from_kind = match row.try_get::<String, _>("from_kind")?.as_str() {
                "user" => FromKind::User,
                "agent" => FromKind::Agent,
                _ => return Err(ConversationLatestStoreError::MalformedAuthor),
            };
            evidence.push(AuthorEvidence {
                from_kind,
                from_agent_id: row.try_get("from_agent_id")?,
                from_user_id: row.try_get("from_user_id")?,
                author_id: row.try_get("author_id")?,
                author_name: row.try_get("author_name")?,
                author_display_name: row.try_get("author_display_name")?,
            });
            let metadata: Option<String> = row.try_get("metadata")?;
            raws.push(Raw {
                conversation: ConversationId::parse(row.try_get::<String, _>("conversation_id")?)
                    .map_err(|_| {
                    ConversationLatestStoreError::MalformedIdentity("messages.conversation_id")
                })?,
                message: MessageId::parse(row.try_get::<String, _>("id")?)
                    .map_err(|_| ConversationLatestStoreError::MalformedIdentity("messages.id"))?,
                seq: MessageSeq::try_from(row.try_get::<i64, _>("seq")?)
                    .map_err(|_| ConversationLatestStoreError::MalformedIdentity("messages.seq"))?,
                created_at_ms: row.try_get("created_at")?,
                thread_root: row
                    .try_get::<Option<String>, _>("thread_root_id")?
                    .map(ThreadRootId::parse)
                    .transpose()
                    .map_err(|_| {
                        ConversationLatestStoreError::MalformedIdentity("messages.thread_root_id")
                    })?,
                content: row.try_get("content")?,
                attachment_count: decode_attachments(metadata.as_deref())
                    .map_err(ConversationLatestStoreError::Attachments)?
                    .len(),
            });
        }
        let authors = resolve_authors(&mut tx, &evidence)
            .await
            .map_err(ConversationLatestStoreError::Author)?;
        tx.commit().await?;

        raws.into_iter()
            .zip(authors)
            .map(|(raw, author)| {
                Ok(ConversationLatest {
                    conversation: raw.conversation,
                    message: raw.message,
                    seq: raw.seq,
                    created_at_ms: raw.created_at_ms,
                    thread_root: raw.thread_root,
                    sender: author.ok_or(ConversationLatestStoreError::MalformedAuthor)?,
                    preview: ActivityPreview::from_source(Some(&raw.content)),
                    attachment_count: raw.attachment_count,
                })
            })
            .collect()
    }
}
