//! One-snapshot SQLite projections for the three roll-up message-unread reads.

use std::collections::BTreeMap;

use silverhand_backend_core::{
    conversation_messaging::messaging::collaboration::message_unread::{
        ConversationMessageUnread, ConversationMessageUnreadReadPort,
        ConversationMessageUnreadRefusal, LatestUnreadPointer, NonZeroUnreadCount, UnreadCount,
        UnreadRollup, ViewerMessageUnreadSummary, ViewerMessageUnreadSummaryReadPort,
        WorkspaceConversationUnread, WorkspaceMessageUnread, WorkspaceMessageUnreadReadPort,
        WorkspaceMessageUnreadRefusal, WorkspaceUnreadTotal,
    },
    conversation_messaging::messaging::{MessageId, MessageSeq, ThreadRootId},
    foundation::identity::{ConversationId, UserId, WorkspaceId},
    foundation::persistence::PersistenceFailure,
};
use sqlx::{Row, sqlite::SqliteRow};
use thiserror::Error;

use crate::{
    conversation_messaging::messaging::MessagingStore,
    viewer_read_model::unread_read::rollup_unread,
};

use super::conversation_participation::{
    ConversationParticipationStoreError, Placement, decode_conversation, decode_placement,
    participates,
};

macro_rules! conversation_columns {
    () => {
        "c.id,c.workspace_id,c.placement_kind,c.roster_policy,c.title,\
         c.title_key,c.status,c.visibility,c.endpoint_a_kind,c.endpoint_a_id,c.endpoint_a_deleted_at,\
         c.endpoint_b_kind,c.endpoint_b_id,c.endpoint_b_deleted_at"
    };
}

macro_rules! root_valid {
    () => {
        "CASE WHEN m.thread_root_id IS NULL THEN 1 ELSE EXISTS(SELECT 1 FROM messages root WHERE \
         root.id=m.thread_root_id AND root.conversation_id=m.conversation_id AND \
         root.thread_root_id IS NULL) END"
    };
}

macro_rules! author_valid {
    () => {
        "CASE WHEN m.from_kind='user' THEN EXISTS(SELECT 1 FROM users u WHERE u.id=m.from_user_id) \
         WHEN m.from_kind='agent' THEN EXISTS(SELECT 1 FROM agents a WHERE a.id=m.from_agent_id) \
         ELSE 0 END"
    };
}

macro_rules! latest_columns {
    () => {
        concat!(
            "m.conversation_id AS message_conversation_id,m.id AS message_id,m.seq,\
         m.thread_root_id,m.from_kind,m.from_user_id,m.from_agent_id,",
            root_valid!(),
            " AS root_valid,",
            author_valid!(),
            " AS author_valid"
        )
    };
}

const WORKSPACE_ROLLUPS: &str = concat!(
    "WITH p(viewer,ws) AS (SELECT ?,?), totals AS (SELECT c.id AS conversation_id,\
     COUNT(*) AS unread_count,MAX(m.seq) AS latest_seq",
    " FROM conversations c JOIN conversation_users cu ON cu.conversation_id=c.id AND \
     cu.user_id=(SELECT viewer FROM p) JOIN messages m ON m.conversation_id=c.id AND ",
    rollup_unread!(),
    " WHERE c.workspace_id=(SELECT ws FROM p) GROUP BY c.id) SELECT ",
    conversation_columns!(),
    ",r.unread_count,",
    latest_columns!(),
    " FROM totals r JOIN conversations c ON c.id=r.conversation_id \
     LEFT JOIN messages m ON m.conversation_id=r.conversation_id AND m.seq=r.latest_seq ORDER BY c.id ASC"
);

const SUMMARY_COUNTS: &str = concat!(
    "WITH p(viewer) AS (SELECT ?) SELECT ",
    conversation_columns!(),
    ",cu.user_id AS source_viewer,COUNT(*) AS unread_count",
    " FROM conversations c JOIN conversation_users cu ON cu.conversation_id=c.id AND \
     cu.user_id=(SELECT viewer FROM p) JOIN messages m ON m.conversation_id=c.id AND ",
    rollup_unread!(),
    " GROUP BY c.id,cu.user_id ORDER BY c.workspace_id ASC,c.id ASC"
);

const CONVERSATION_ROLLUP: &str = concat!(
    "WITH p(viewer,conv) AS (SELECT ?,?), totals AS (SELECT COUNT(*) AS unread_count,MAX(m.seq) AS latest_seq",
    " FROM messages m WHERE m.conversation_id=(SELECT conv FROM p) AND ",
    rollup_unread!(),
    ") SELECT r.unread_count,",
    latest_columns!(),
    " FROM totals r LEFT JOIN messages m ON m.conversation_id=(SELECT conv FROM p) AND m.seq=r.latest_seq"
);

#[derive(Clone)]
pub struct SqliteMessageUnreadRead {
    pool: crate::foundation::database::SqliteDatabase,
}

impl<E> MessagingStore<E> {
    pub fn message_unread(&self) -> SqliteMessageUnreadRead {
        SqliteMessageUnreadRead {
            pool: self.pool.clone(),
        }
    }
}

#[derive(Debug, Error)]
pub enum MessageUnreadStoreError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("conversation access snapshot is malformed")]
    Access(#[from] ConversationParticipationStoreError),
    #[error("stored message-unread identity is malformed")]
    MalformedIdentity,
    #[error("stored message-unread ordinal or count is malformed")]
    MalformedUnread,
    #[error("stored unread latest pointer has an invalid author or thread root")]
    MalformedCandidate,
    #[error("stored unread aggregate does not form a closed projection")]
    MalformedProjection,
}

impl From<MessageUnreadStoreError> for PersistenceFailure {
    fn from(value: MessageUnreadStoreError) -> Self {
        Self::from_source(value)
    }
}

impl WorkspaceMessageUnreadReadPort for SqliteMessageUnreadRead {
    type Error = PersistenceFailure;

    async fn read_workspace(
        &self,
        viewer: &UserId,
        workspace: &WorkspaceId,
    ) -> Result<Result<WorkspaceMessageUnread, WorkspaceMessageUnreadRefusal>, Self::Error> {
        self.read_workspace_inner(viewer, workspace)
            .await
            .map_err(PersistenceFailure::from)
    }
}

impl ViewerMessageUnreadSummaryReadPort for SqliteMessageUnreadRead {
    type Error = PersistenceFailure;

    async fn read_summary(
        &self,
        viewer: &UserId,
    ) -> Result<ViewerMessageUnreadSummary, Self::Error> {
        self.read_summary_inner(viewer)
            .await
            .map_err(PersistenceFailure::from)
    }
}

impl ConversationMessageUnreadReadPort for SqliteMessageUnreadRead {
    type Error = PersistenceFailure;

    async fn read_conversation(
        &self,
        viewer: &UserId,
        conversation: &ConversationId,
    ) -> Result<Result<ConversationMessageUnread, ConversationMessageUnreadRefusal>, Self::Error>
    {
        self.read_conversation_inner(viewer, conversation)
            .await
            .map_err(PersistenceFailure::from)
    }
}

impl SqliteMessageUnreadRead {
    #[tracing::instrument(
        target = "silverhand::diagnostics",
        name = "db.unread.read_workspace_inner",
        skip_all,
        fields(
            elapsed_ms = tracing::field::Empty,
            outcome = tracing::field::Empty,
            operation_finished = tracing::field::Empty,
        )
    )]
    async fn read_workspace_inner(
        &self,
        viewer: &UserId,
        requested: &WorkspaceId,
    ) -> Result<
        Result<WorkspaceMessageUnread, WorkspaceMessageUnreadRefusal>,
        MessageUnreadStoreError,
    > {
        let observation = silverhand_backend_observability::Operation::current();
        let result = async {
        let mut tx =
            crate::foundation::statement_metrics::begin(&self.pool, Some("BEGIN DEFERRED")).await?;
        let Some(raw_workspace) =
            sqlx::query_scalar::<_, String>("SELECT id FROM workspaces WHERE id=?")
                .bind(requested.as_str())
                .fetch_optional(&mut *tx)
                .await?
        else {
            tx.commit().await?;
            return Ok(Err(WorkspaceMessageUnreadRefusal::NotFound));
        };
        let workspace = WorkspaceId::parse(raw_workspace)
            .map_err(|_| MessageUnreadStoreError::MalformedIdentity)?;
        if &workspace != requested {
            return Err(MessageUnreadStoreError::MalformedIdentity);
        }
        let member: i64 = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM workspace_users WHERE workspace_id=? AND user_id=?)",
        )
        .bind(workspace.as_str())
        .bind(viewer.as_str())
        .fetch_one(&mut *tx)
        .await?;
        if member != 1 {
            tx.commit().await?;
            return Ok(Err(WorkspaceMessageUnreadRefusal::Forbidden));
        }

        let rows = sqlx::query(WORKSPACE_ROLLUPS)
            .bind(viewer.as_str())
            .bind(workspace.as_str())
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;
        let mut projected = Vec::with_capacity(rows.len());
        for row in rows {
            if !matches!(decode_placement(&row)?, Placement::Workspace(ref found) if found == &workspace)
            {
                return Err(MessageUnreadStoreError::MalformedProjection);
            }
            let conversation = ConversationId::parse(row.try_get::<String, _>("id")?)
                .map_err(|_| MessageUnreadStoreError::MalformedIdentity)?;
            let UnreadRollup::Unread(unread) = decode_rollup(&row, &conversation)? else {
                return Err(MessageUnreadStoreError::MalformedProjection);
            };
            projected.push(
                WorkspaceConversationUnread::projected(workspace.clone(), conversation, unread)
                    .map_err(|_| MessageUnreadStoreError::MalformedProjection)?,
            );
        }
        let projection = WorkspaceMessageUnread::projected(workspace, projected)
            .map_err(|_| MessageUnreadStoreError::MalformedProjection)?;
        Ok(Ok(projection))

        }.await;
        observation.finish(&result);
        result
    }

    #[tracing::instrument(
        target = "silverhand::diagnostics",
        name = "db.unread.read_summary_inner",
        skip_all,
        fields(
            elapsed_ms = tracing::field::Empty,
            outcome = tracing::field::Empty,
            operation_finished = tracing::field::Empty,
        )
    )]
    async fn read_summary_inner(
        &self,
        viewer: &UserId,
    ) -> Result<ViewerMessageUnreadSummary, MessageUnreadStoreError> {
        let observation = silverhand_backend_observability::Operation::current();
        let result = async {
            // One aggregate statement already provides a complete snapshot.
            let rows = sqlx::query(SUMMARY_COUNTS)
                .bind(viewer.as_str())
                .fetch_all(&self.pool)
                .await?;
            let mut totals = BTreeMap::<String, (UserId, WorkspaceId, i64)>::new();
            for row in rows {
                let source_viewer = UserId::parse(row.try_get::<String, _>("source_viewer")?)
                    .map_err(|_| MessageUnreadStoreError::MalformedIdentity)?;
                ConversationId::parse(row.try_get::<String, _>("id")?)
                    .map_err(|_| MessageUnreadStoreError::MalformedIdentity)?;
                let count = UnreadCount::new(row.try_get("unread_count")?)
                    .map_err(|_| MessageUnreadStoreError::MalformedUnread)?;
                let Placement::Workspace(workspace) = decode_placement(&row)? else {
                    return Err(MessageUnreadStoreError::MalformedProjection);
                };
                let entry = totals.entry(workspace.as_str().to_owned()).or_insert((
                    source_viewer.clone(),
                    workspace,
                    0,
                ));
                if entry.0 != source_viewer {
                    return Err(MessageUnreadStoreError::MalformedProjection);
                }
                entry.2 = entry
                    .2
                    .checked_add(count.get())
                    .ok_or(MessageUnreadStoreError::MalformedUnread)?;
            }
            let projected = totals
                .into_values()
                .map(|(source_viewer, workspace, count)| {
                    let count = UnreadCount::new(count)
                        .map_err(|_| MessageUnreadStoreError::MalformedUnread)?;
                    let count = NonZeroUnreadCount::new(count)
                        .map_err(|_| MessageUnreadStoreError::MalformedUnread)?;
                    Ok(WorkspaceUnreadTotal::projected(
                        source_viewer,
                        workspace,
                        count,
                    ))
                })
                .collect::<Result<Vec<_>, MessageUnreadStoreError>>()?;
            let summary = ViewerMessageUnreadSummary::projected(viewer.clone(), projected)
                .map_err(|_| MessageUnreadStoreError::MalformedProjection)?;
            Ok(summary)
        }
        .await;
        observation.finish(&result);
        result
    }

    #[tracing::instrument(
        target = "silverhand::diagnostics",
        name = "db.unread.read_conversation_inner",
        skip_all,
        fields(
            elapsed_ms = tracing::field::Empty,
            outcome = tracing::field::Empty,
            operation_finished = tracing::field::Empty,
        )
    )]
    async fn read_conversation_inner(
        &self,
        viewer: &UserId,
        requested: &ConversationId,
    ) -> Result<
        Result<ConversationMessageUnread, ConversationMessageUnreadRefusal>,
        MessageUnreadStoreError,
    > {
        let observation = silverhand_backend_observability::Operation::current();
        let result = async {
            let mut tx =
                crate::foundation::statement_metrics::begin(&self.pool, Some("BEGIN DEFERRED"))
                    .await?;
            let Some(row) = sqlx::query(
            "SELECT id,workspace_id,placement_kind,roster_policy,title,title_key,status,visibility,\
             endpoint_a_kind,endpoint_a_id,endpoint_a_deleted_at,\
             endpoint_b_kind,endpoint_b_id,endpoint_b_deleted_at FROM conversations WHERE id=?",
        )
        .bind(requested.as_str())
        .fetch_optional(&mut *tx)
        .await?
        else {
            tx.commit().await?;
            return Ok(Err(ConversationMessageUnreadRefusal::NotFound));
        };
            let (stored, placement) = decode_conversation(&row, requested)?;
            if !participates(&mut tx, viewer, requested, &placement).await? {
                tx.commit().await?;
                return Ok(Err(ConversationMessageUnreadRefusal::Forbidden));
            }
            let row = sqlx::query(CONVERSATION_ROLLUP)
                .bind(viewer.as_str())
                .bind(stored.as_str())
                .fetch_one(&mut *tx)
                .await?;
            tx.commit().await?;
            let rollup = decode_rollup(&row, &stored)?;
            let projection = ConversationMessageUnread::projected(stored, rollup)
                .map_err(|_| MessageUnreadStoreError::MalformedProjection)?;
            Ok(Ok(projection))
        }
        .await;
        observation.finish(&result);
        result
    }
}

// Aggregate responsibility: only the returned latest row is decoded here.
// Count includes every row selected by the canonical unread predicate, including
// middle rows with unrelated malformed fields. This is not a database audit.
fn decode_rollup(
    row: &SqliteRow,
    source: &ConversationId,
) -> Result<UnreadRollup, MessageUnreadStoreError> {
    let count = UnreadCount::new(row.try_get("unread_count")?)
        .map_err(|_| MessageUnreadStoreError::MalformedUnread)?;
    if count.get() == 0 {
        if row.try_get::<Option<i64>, _>("seq")?.is_some() {
            return Err(MessageUnreadStoreError::MalformedProjection);
        }
        return Ok(UnreadRollup::CaughtUp);
    }
    let root_valid: i64 = row.try_get("root_valid")?;
    let author_valid: i64 = row.try_get("author_valid")?;
    let from_kind: String = row.try_get("from_kind")?;
    let from_user: Option<String> = row.try_get("from_user_id")?;
    let from_agent: Option<String> = row.try_get("from_agent_id")?;
    if root_valid != 1 || author_valid != 1 {
        return Err(MessageUnreadStoreError::MalformedCandidate);
    }
    match from_kind.as_str() {
        "user" if from_user.as_deref().is_some_and(|id| !id.is_empty()) && from_agent.is_none() => {
        }
        "agent"
            if from_agent.as_deref().is_some_and(|id| !id.is_empty()) && from_user.is_none() => {}
        _ => return Err(MessageUnreadStoreError::MalformedCandidate),
    }
    let conversation = ConversationId::parse(row.try_get::<String, _>("message_conversation_id")?)
        .map_err(|_| MessageUnreadStoreError::MalformedIdentity)?;
    if &conversation != source {
        return Err(MessageUnreadStoreError::MalformedIdentity);
    }
    let message = MessageId::parse(row.try_get::<String, _>("message_id")?)
        .map_err(|_| MessageUnreadStoreError::MalformedIdentity)?;
    let seq = MessageSeq::try_from(row.try_get::<i64, _>("seq")?)
        .map_err(|_| MessageUnreadStoreError::MalformedUnread)?;
    let thread_root = row
        .try_get::<Option<String>, _>("thread_root_id")?
        .map(ThreadRootId::parse)
        .transpose()
        .map_err(|_| MessageUnreadStoreError::MalformedIdentity)?;
    UnreadRollup::projected(
        count,
        Some(LatestUnreadPointer::projected(
            conversation,
            message,
            seq,
            thread_root,
        )),
    )
    .map_err(|_| MessageUnreadStoreError::MalformedProjection)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unread_aggregate_single_connection_errors_and_cancellation_reclaim() {
        use sqlx::{
            Connection, Executor,
            sqlite::{SqliteConnectOptions, SqlitePoolOptions},
        };
        use std::{
            sync::{
                Arc,
                atomic::{AtomicBool, Ordering},
                mpsc,
            },
            time::Duration,
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aggregate.sqlite");
        let mut seed = crate::current_schema::create_current_schema(&path).await;
        seed.execute(sqlx::raw_sql(
            "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES('v','v','V',1,1),('a','a','A',1,1);
             INSERT INTO workspaces(id,name,name_key,created_at,updated_at) VALUES('w','W','w',1,1);
             INSERT INTO workspace_users(workspace_id,user_id,joined_at) VALUES('w','v',1);
             WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<300)
             INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,status,visibility,created_at,updated_at)
             SELECT 'c'||i,'w','workspace','open','C'||i,'c'||i,'idle','private',1,1 FROM n;
             INSERT INTO conversation_users(conversation_id,user_id,joined_at) SELECT id,'v',1 FROM conversations;
             INSERT INTO messages(id,conversation_id,from_kind,from_user_id,content,seq,created_at)
             SELECT 'm'||id,id,'user','a','hello',1,1 FROM conversations;"
        )).await.unwrap();
        seed.close().await.unwrap();
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(SqliteConnectOptions::new().filename(path))
            .await
            .unwrap();
        let reader = SqliteMessageUnreadRead {
            pool: pool.clone().into(),
        };
        let viewer = UserId::parse("v").unwrap();
        let workspace = WorkspaceId::parse("w").unwrap();
        // Many conversations still produce only aggregate rows on one connection.
        tokio::time::timeout(
            Duration::from_secs(5),
            reader.read_workspace_inner(&viewer, &workspace),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();
        reader.read_summary_inner(&viewer).await.unwrap();
        sqlx::query("UPDATE messages SET id=CAST(x'80' AS TEXT) WHERE id='mc150'")
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            reader
                .read_workspace_inner(&viewer, &workspace)
                .await
                .is_err()
        );
        sqlx::query("UPDATE messages SET id='mc150' WHERE conversation_id='c150'")
            .execute(&pool)
            .await
            .unwrap();
        for summary in [false, true] {
            let entered = Arc::new(tokio::sync::Notify::new());
            let first = Arc::new(AtomicBool::new(true));
            let (release, receiver) = mpsc::channel();
            {
                let mut connection = pool.acquire().await.unwrap();
                let entered = entered.clone();
                connection
                    .lock_handle()
                    .await
                    .unwrap()
                    .set_progress_handler(1000, move || {
                        if first.swap(false, Ordering::SeqCst) {
                            entered.notify_one();
                            receiver.recv_timeout(Duration::from_secs(5)).unwrap();
                        }
                        true
                    });
            }
            let reader = reader.clone();
            let viewer = viewer.clone();
            let workspace = workspace.clone();
            let task = tokio::spawn(async move {
                if summary {
                    reader.read_summary_inner(&viewer).await.map(|_| ())
                } else {
                    reader
                        .read_workspace_inner(&viewer, &workspace)
                        .await
                        .map(|_| ())
                }
            });
            tokio::time::timeout(Duration::from_secs(3), entered.notified())
                .await
                .unwrap();
            task.abort();
            release.send(()).unwrap();
            assert!(task.await.unwrap_err().is_cancelled());
            tokio::time::timeout(Duration::from_secs(3), async {
                let mut connection = pool.acquire().await.unwrap();
                connection
                    .lock_handle()
                    .await
                    .unwrap()
                    .remove_progress_handler();
                let mut tx = connection.begin().await.unwrap();
                sqlx::query("UPDATE messages SET content='reused' WHERE id='mc1'")
                    .execute(&mut *tx)
                    .await
                    .unwrap();
                tx.commit().await.unwrap();
            })
            .await
            .unwrap();
        }
        reader
            .read_workspace_inner(&viewer, &workspace)
            .await
            .unwrap()
            .unwrap();
        pool.close().await;
    }
}
