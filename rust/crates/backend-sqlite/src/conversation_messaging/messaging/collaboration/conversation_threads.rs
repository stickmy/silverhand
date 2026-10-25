//! One-snapshot SQLite projection for a conversation's reply-cluster list.

use std::collections::HashMap;

use silverhand_backend_core::{
    conversation_messaging::messaging::{
        FromKind, MessageAuthor, MessageId, MessageSeq, ThreadRootId,
        collaboration::conversation_threads::{
            ConversationThreadsPage, ConversationThreadsReadPort, ConversationThreadsRefusal,
            SelectedConversationThread, ThreadListFilter, ThreadListQuery, ThreadReadCorruption,
        },
    },
    foundation::identity::{ConversationId, UserId},
    foundation::persistence::PersistenceFailure,
};
use sqlx::{Row, Sqlite, Transaction, sqlite::SqliteRow};
use thiserror::Error;

use crate::{
    conversation_messaging::messaging::{
        MessagingStore,
        author::{AuthorEvidence, resolve_authors},
    },
    viewer_read_model::unread_read::followed_thread_unread,
};

use super::conversation_participation::{
    ConversationParticipationStoreError, SELECT_CONVERSATION, decode_conversation,
    participating_viewer,
};

macro_rules! select_threads {
    ($($filter:tt)*) => {
        concat!(
            "WITH p(viewer,conv,has_cursor,cursor_at,cursor_seq) AS (SELECT ?,?,?,?,?), \
             agg AS (SELECT thread_root_id AS root_id,COUNT(*) AS reply_count,MAX(seq) AS \
             last_reply_seq FROM messages WHERE conversation_id=(SELECT conv FROM p) AND \
             thread_root_id IS NOT NULL AND from_kind!='system' GROUP BY thread_root_id) \
             SELECT c.id AS row_conversation_id,root.conversation_id AS root_conversation_id, \
             root.id AS root_message_id,root.thread_root_id AS root_own_thread_root_id, \
             root.from_kind AS root_from_kind,root.from_agent_id AS root_from_agent_id, \
             root.from_user_id AS root_from_user_id,root.author_id AS root_author_id, \
             root.author_name AS root_author_name,root.author_display_name AS \
             root_author_display_name,root.content AS root_content,root.created_at AS \
             root_created_at,root.seq AS root_seq,agg.reply_count,agg.last_reply_seq,lr.id AS \
             last_reply_message_id,lr.conversation_id AS last_reply_conversation_id, \
             lr.thread_root_id AS last_reply_thread_root_id,lr.created_at AS last_reply_at, \
             EXISTS(SELECT 1 FROM thread_followers tf WHERE tf.thread_root_id=root.id AND \
             tf.conversation_id=c.id AND tf.principal_kind='human' AND \
             tf.principal_id=(SELECT viewer FROM p)) AS viewer_following FROM agg JOIN messages lr \
             ON lr.conversation_id=(SELECT conv FROM p) AND lr.thread_root_id=agg.root_id AND \
             lr.seq=agg.last_reply_seq JOIN messages root ON root.id=agg.root_id AND \
             root.conversation_id=(SELECT conv FROM p) JOIN conversations c ON \
             c.id=root.conversation_id WHERE ((SELECT has_cursor FROM p)=0 OR \
             lr.created_at<(SELECT cursor_at FROM p) OR (lr.created_at=(SELECT cursor_at FROM p) \
             AND root.seq<(SELECT cursor_seq FROM p))) ",
            $($filter)*,
            " ORDER BY lr.created_at DESC,root.seq DESC LIMIT ?"
        )
    };
}

const SELECT_ALL_THREADS: &str = select_threads!("");
const SELECT_FOLLOWED_UNREAD_THREADS: &str = select_threads!(
    "AND EXISTS(SELECT 1 FROM messages m WHERE m.thread_root_id=root.id AND ",
    followed_thread_unread!(),
    ")"
);

const SELECT_REPLIERS: &str = "SELECT m.conversation_id AS replier_conversation_id,\
     m.thread_root_id AS replier_thread_root_id,m.from_kind,m.from_agent_id,m.from_user_id,\
     m.author_id,m.author_name,m.author_display_name,MAX(m.seq) AS last_seq FROM \
     json_each(?) requested JOIN messages m ON m.thread_root_id=requested.value WHERE \
     m.conversation_id=? AND m.from_kind!='system' GROUP BY m.thread_root_id,m.from_kind,m.author_id \
     ORDER BY last_seq DESC";

#[derive(Clone)]
pub struct SqliteConversationThreadsRead {
    pool: crate::foundation::database::SqliteDatabase,
}

impl<E> MessagingStore<E> {
    pub fn conversation_threads(&self) -> SqliteConversationThreadsRead {
        SqliteConversationThreadsRead {
            pool: self.pool.clone(),
        }
    }
}

#[derive(Debug, Error)]
pub enum ConversationThreadsStoreError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("conversation participation snapshot is malformed")]
    Access(#[from] ConversationParticipationStoreError),
    #[error("stored conversation thread field {field} is malformed")]
    MalformedIdentity { field: &'static str },
    #[error("stored conversation thread author kind is outside the closed set")]
    MalformedAuthorKind,
    #[error("stored conversation thread ordinal is outside its domain")]
    MalformedOrdinal,
    #[error("conversation thread author projection failed")]
    Author(#[source] PersistenceFailure),
    #[error("stored conversation thread projection is not closed")]
    Projection(#[source] ThreadReadCorruption),
    #[error("conversation thread replier source does not match its selected root")]
    ReplierCoordinate,
}

impl From<ConversationThreadsStoreError> for PersistenceFailure {
    fn from(value: ConversationThreadsStoreError) -> Self {
        Self::from_source(value)
    }
}

impl ConversationThreadsReadPort for SqliteConversationThreadsRead {
    type Error = PersistenceFailure;

    async fn admit(
        &self,
        viewer: &UserId,
        conversation: &ConversationId,
    ) -> Result<Result<(), ConversationThreadsRefusal>, Self::Error> {
        self.admit_inner(viewer, conversation)
            .await
            .map(|outcome| outcome.map(|_| ()))
            .map_err(PersistenceFailure::from)
    }

    async fn read(
        &self,
        viewer: &UserId,
        conversation: &ConversationId,
        query: ThreadListQuery,
    ) -> Result<Result<ConversationThreadsPage, ConversationThreadsRefusal>, Self::Error> {
        self.read_inner(viewer, conversation, query)
            .await
            .map_err(PersistenceFailure::from)
    }
}

impl SqliteConversationThreadsRead {
    async fn admit_inner(
        &self,
        viewer: &UserId,
        requested: &ConversationId,
    ) -> Result<
        Result<(UserId, ConversationId), ConversationThreadsRefusal>,
        ConversationThreadsStoreError,
    > {
        let mut tx = self.pool.begin_with("BEGIN DEFERRED").await?;
        let outcome = admit_in_snapshot(&mut tx, viewer, requested).await?;
        tx.commit().await?;
        Ok(outcome)
    }

    async fn read_inner(
        &self,
        viewer: &UserId,
        requested: &ConversationId,
        query: ThreadListQuery,
    ) -> Result<
        Result<ConversationThreadsPage, ConversationThreadsRefusal>,
        ConversationThreadsStoreError,
    > {
        let mut tx = self.pool.begin_with("BEGIN DEFERRED").await?;
        let (source_viewer, stored_conversation) =
            match admit_in_snapshot(&mut tx, viewer, requested).await? {
                Ok(source) => source,
                Err(refusal) => {
                    tx.commit().await?;
                    return Ok(Err(refusal));
                }
            };

        let sql = match query.filter() {
            ThreadListFilter::All => SELECT_ALL_THREADS,
            ThreadListFilter::FollowedUnread => SELECT_FOLLOWED_UNREAD_THREADS,
        };
        let before = query.before();
        let selected = sqlx::query(sql)
            .bind(source_viewer.as_str())
            .bind(stored_conversation.as_str())
            .bind(i64::from(before.is_some()))
            .bind(before.map_or(0, |cursor| cursor.last_reply_at()))
            .bind(before.map_or(0, |cursor| cursor.root_seq().get()))
            .bind(i64::from(query.limit()) + 1)
            .fetch_all(&mut *tx)
            .await?;

        // Decode every selected root, including the look-ahead row, before
        // slicing or applying any viewer projection.
        let raw_threads = selected
            .iter()
            .map(RawThread::decode)
            .collect::<Result<Vec<_>, _>>()?;
        let root_ids = raw_threads
            .iter()
            .map(|thread| thread.root_id.as_str())
            .collect::<Vec<_>>();
        let replier_rows = if root_ids.is_empty() {
            Vec::new()
        } else {
            sqlx::query(SELECT_REPLIERS)
                .bind(serde_json::to_string(&root_ids).map_err(|_| {
                    ConversationThreadsStoreError::MalformedIdentity {
                        field: "messages.thread_root_id",
                    }
                })?)
                .bind(stored_conversation.as_str())
                .fetch_all(&mut *tx)
                .await?
        };
        let raw_repliers = replier_rows
            .iter()
            .map(RawReplier::decode)
            .collect::<Result<Vec<_>, _>>()?;

        let mut evidence = raw_threads
            .iter()
            .map(|thread| thread.root_author.clone())
            .collect::<Vec<_>>();
        evidence.extend(raw_repliers.iter().map(|row| row.author.clone()));
        let resolved = resolve_authors(&mut tx, &evidence)
            .await
            .map_err(ConversationThreadsStoreError::Author)?;
        let (root_authors, replier_authors) = resolved.split_at(raw_threads.len());

        let mut repliers = HashMap::<ThreadRootId, Vec<(i64, MessageAuthor)>>::new();
        for (raw, author) in raw_repliers.iter().zip(replier_authors) {
            if raw.conversation != stored_conversation {
                return Err(ConversationThreadsStoreError::ReplierCoordinate);
            }
            let Some(author) = author.clone() else {
                return Err(ConversationThreadsStoreError::MalformedAuthorKind);
            };
            if author.stable_identity().is_none() {
                continue;
            }
            repliers
                .entry(raw.root.clone())
                .or_default()
                .push((raw.last_seq, author));
        }

        let mut decoded = Vec::with_capacity(raw_threads.len());
        for (raw, root_author) in raw_threads.into_iter().zip(root_authors.iter().cloned()) {
            let mut root_repliers = repliers.remove(&raw.root_id).unwrap_or_default();
            root_repliers.sort_by_key(|row| std::cmp::Reverse(row.0));
            let replier_total = i64::try_from(root_repliers.len())
                .map_err(|_| ConversationThreadsStoreError::MalformedOrdinal)?;
            let top_repliers = root_repliers
                .into_iter()
                .take(3)
                .map(|(_, author)| author)
                .collect();
            decoded.push(raw.seal(root_author, top_repliers, replier_total)?);
        }
        if !repliers.is_empty() {
            return Err(ConversationThreadsStoreError::ReplierCoordinate);
        }

        let limit = usize::from(query.limit());
        let has_more = decoded.len() > limit;
        decoded.truncate(limit);
        let page = ConversationThreadsPage::sealed(
            source_viewer,
            stored_conversation,
            query,
            decoded,
            has_more,
        )
        .map_err(ConversationThreadsStoreError::Projection)?;
        tx.commit().await?;
        Ok(Ok(page))
    }
}

async fn admit_in_snapshot(
    tx: &mut Transaction<'_, Sqlite>,
    viewer: &UserId,
    requested: &ConversationId,
) -> Result<
    Result<(UserId, ConversationId), ConversationThreadsRefusal>,
    ConversationThreadsStoreError,
> {
    let Some(row) = sqlx::query(SELECT_CONVERSATION)
        .bind(requested.as_str())
        .fetch_optional(&mut **tx)
        .await?
    else {
        return Ok(Err(ConversationThreadsRefusal::NotFound));
    };
    let (stored, placement) = decode_conversation(&row, requested)?;
    let Some(source_viewer) = participating_viewer(tx, viewer, &stored, &placement).await? else {
        return Ok(Err(ConversationThreadsRefusal::Forbidden));
    };
    Ok(Ok((source_viewer, stored)))
}

struct RawThread {
    conversation: ConversationId,
    root_conversation: ConversationId,
    root_id: ThreadRootId,
    root_author: AuthorEvidence,
    root_content: String,
    root_created_at: i64,
    root_seq: MessageSeq,
    reply_count: i64,
    last_reply_at: i64,
    last_reply_seq: MessageSeq,
    last_reply_conversation: ConversationId,
    last_reply_thread_root: ThreadRootId,
    last_reply_message_id: MessageId,
    viewer_following: bool,
}

impl RawThread {
    fn decode(row: &SqliteRow) -> Result<Self, ConversationThreadsStoreError> {
        if row
            .try_get::<Option<String>, _>("root_own_thread_root_id")?
            .is_some()
        {
            return Err(ConversationThreadsStoreError::Projection(
                ThreadReadCorruption::ReplyCoordinate,
            ));
        }
        Ok(Self {
            conversation: conversation_id(row, "row_conversation_id")?,
            root_conversation: conversation_id(row, "root_conversation_id")?,
            root_id: thread_root_id(row, "root_message_id")?,
            root_author: author_evidence(row, "root_")?,
            root_content: row.try_get("root_content")?,
            root_created_at: row.try_get("root_created_at")?,
            root_seq: message_seq(row, "root_seq")?,
            reply_count: row.try_get("reply_count")?,
            last_reply_at: row.try_get("last_reply_at")?,
            last_reply_seq: message_seq(row, "last_reply_seq")?,
            last_reply_conversation: conversation_id(row, "last_reply_conversation_id")?,
            last_reply_thread_root: thread_root_id(row, "last_reply_thread_root_id")?,
            last_reply_message_id: message_id(row, "last_reply_message_id")?,
            viewer_following: row.try_get::<i64, _>("viewer_following")? == 1,
        })
    }

    fn seal(
        self,
        root_author: Option<MessageAuthor>,
        repliers: Vec<MessageAuthor>,
        replier_total: i64,
    ) -> Result<SelectedConversationThread, ConversationThreadsStoreError> {
        SelectedConversationThread::seal(
            self.conversation,
            self.root_conversation,
            self.root_id,
            root_author,
            &self.root_content,
            self.root_created_at,
            self.root_seq,
            self.reply_count,
            self.last_reply_at,
            self.last_reply_seq,
            self.last_reply_conversation,
            self.last_reply_thread_root,
            self.last_reply_message_id,
            repliers,
            replier_total,
            self.viewer_following,
        )
        .map_err(ConversationThreadsStoreError::Projection)
    }
}

struct RawReplier {
    conversation: ConversationId,
    root: ThreadRootId,
    author: AuthorEvidence,
    last_seq: i64,
}

impl RawReplier {
    fn decode(row: &SqliteRow) -> Result<Self, ConversationThreadsStoreError> {
        let last_seq = message_seq(row, "last_seq")?.get();
        Ok(Self {
            conversation: conversation_id(row, "replier_conversation_id")?,
            root: thread_root_id(row, "replier_thread_root_id")?,
            author: author_evidence(row, "")?,
            last_seq,
        })
    }
}

fn author_evidence(
    row: &SqliteRow,
    prefix: &str,
) -> Result<AuthorEvidence, ConversationThreadsStoreError> {
    let column = |suffix: &str| format!("{prefix}{suffix}");
    let from_kind: String = row.try_get(column("from_kind").as_str())?;
    Ok(AuthorEvidence {
        from_kind: match from_kind.as_str() {
            "user" => FromKind::User,
            "agent" => FromKind::Agent,
            "system" => FromKind::System,
            _ => return Err(ConversationThreadsStoreError::MalformedAuthorKind),
        },
        from_agent_id: row.try_get(column("from_agent_id").as_str())?,
        from_user_id: row.try_get(column("from_user_id").as_str())?,
        author_id: row.try_get(column("author_id").as_str())?,
        author_name: row.try_get(column("author_name").as_str())?,
        author_display_name: row.try_get(column("author_display_name").as_str())?,
    })
}

fn conversation_id(
    row: &SqliteRow,
    field: &'static str,
) -> Result<ConversationId, ConversationThreadsStoreError> {
    ConversationId::parse(row.try_get::<String, _>(field)?)
        .map_err(|_| ConversationThreadsStoreError::MalformedIdentity { field })
}

fn thread_root_id(
    row: &SqliteRow,
    field: &'static str,
) -> Result<ThreadRootId, ConversationThreadsStoreError> {
    ThreadRootId::parse(row.try_get::<String, _>(field)?)
        .map_err(|_| ConversationThreadsStoreError::MalformedIdentity { field })
}

fn message_id(
    row: &SqliteRow,
    field: &'static str,
) -> Result<MessageId, ConversationThreadsStoreError> {
    MessageId::parse(row.try_get::<String, _>(field)?)
        .map_err(|_| ConversationThreadsStoreError::MalformedIdentity { field })
}

fn message_seq(
    row: &SqliteRow,
    field: &'static str,
) -> Result<MessageSeq, ConversationThreadsStoreError> {
    MessageSeq::try_from(row.try_get::<i64, _>(field)?)
        .map_err(|_| ConversationThreadsStoreError::MalformedOrdinal)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(
        root_conversation: &str,
        last_reply_conversation: &str,
        last_reply_thread_root: &str,
    ) -> RawThread {
        RawThread {
            conversation: ConversationId::parse("conversation-a").unwrap(),
            root_conversation: ConversationId::parse(root_conversation).unwrap(),
            root_id: ThreadRootId::parse("root-a").unwrap(),
            root_author: AuthorEvidence {
                from_kind: FromKind::System,
                from_agent_id: None,
                from_user_id: None,
                author_id: None,
                author_name: None,
                author_display_name: None,
            },
            root_content: "root".to_owned(),
            root_created_at: 1,
            root_seq: MessageSeq::try_from(1).unwrap(),
            reply_count: 1,
            last_reply_at: 2,
            last_reply_seq: MessageSeq::try_from(2).unwrap(),
            last_reply_conversation: ConversationId::parse(last_reply_conversation).unwrap(),
            last_reply_thread_root: ThreadRootId::parse(last_reply_thread_root).unwrap(),
            last_reply_message_id: MessageId::parse("reply").unwrap(),
            viewer_following: false,
        }
    }

    #[test]
    fn matching_actual_root_and_last_reply_aliases_are_accepted() {
        assert!(
            raw("conversation-a", "conversation-a", "root-a")
                .seal(None, vec![], 0)
                .is_ok()
        );
    }

    #[test]
    fn a_reply_from_another_root_cannot_be_projected_onto_the_selected_root() {
        assert!(matches!(
            raw("conversation-a", "conversation-a", "root-b").seal(None, vec![], 0),
            Err(ConversationThreadsStoreError::Projection(
                ThreadReadCorruption::ReplyCoordinate
            ))
        ));
    }

    #[test]
    fn a_reply_from_another_conversation_cannot_be_projected_onto_the_selected_root() {
        assert!(matches!(
            raw("conversation-a", "conversation-b", "root-a").seal(None, vec![], 0),
            Err(ConversationThreadsStoreError::Projection(
                ThreadReadCorruption::ReplyCoordinate
            ))
        ));
    }
}
