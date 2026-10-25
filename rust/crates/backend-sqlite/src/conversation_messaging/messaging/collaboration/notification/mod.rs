use crate::conversation_messaging::messaging::{
    HostEnvironment, MessagingStore,
    author::{AuthorEvidence, resolve_authors},
};
use silverhand_backend_core::{
    conversation_messaging::messaging::{
        FromKind, MessageId, NotificationId, SendClock, ThreadRootId,
        collaboration::notification::*,
    },
    foundation::identity::{AgentId, ConversationId, UserId, WorkspaceId},
    foundation::persistence::PersistenceFailure,
};
use sqlx::{Row, sqlite::SqliteRow};
use std::num::NonZeroU64;
use thiserror::Error;

#[derive(Clone)]
pub struct SqliteNotificationStore<E = HostEnvironment> {
    pool: crate::foundation::database::SqliteDatabase,
    environment: E,
}
impl<E: Clone> MessagingStore<E> {
    pub fn notifications(&self) -> SqliteNotificationStore<E> {
        SqliteNotificationStore {
            pool: self.pool.clone(),
            environment: self.environment.clone(),
        }
    }
}

#[derive(Debug, Error)]
pub enum NotificationStoreError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("stored notification identity is malformed")]
    MalformedIdentity,
    #[error("stored notification kind is malformed")]
    MalformedKind,
    #[error("stored notification timestamp is malformed")]
    MalformedTimestamp,
    #[error("stored notification payload is not a JSON object")]
    MalformedPayload,
    #[error("stored anchor message author kind is outside the closed set")]
    MalformedAuthorKind,
    #[error("clock failed")]
    Clock(
        #[source]
        silverhand_backend_core::conversation_messaging::messaging::EnvironmentUnavailable,
    ),
}
impl From<NotificationStoreError> for PersistenceFailure {
    fn from(value: NotificationStoreError) -> Self {
        Self::from_source(value)
    }
}

fn opt<T>(
    raw: Option<String>,
    parse: impl FnOnce(String) -> Option<T>,
) -> Result<Option<T>, NotificationStoreError> {
    match raw {
        Some(raw) => parse(raw)
            .map(Some)
            .ok_or(NotificationStoreError::MalformedIdentity),
        None => Ok(None),
    }
}
fn decode(row: SqliteRow) -> Result<Notification, NotificationStoreError> {
    let payload_json: Option<String> = row.try_get("payload")?;
    if let Some(raw) = payload_json.as_deref()
        && !matches!(
            serde_json::from_str::<serde_json::Value>(raw),
            Ok(serde_json::Value::Object(_))
        )
    {
        return Err(NotificationStoreError::MalformedPayload);
    }
    Ok(Notification {
        id: NotificationId::parse(row.try_get::<String, _>("id")?)
            .map_err(|_| NotificationStoreError::MalformedIdentity)?,
        kind: NotificationKind::parse(row.try_get::<String, _>("kind")?.as_str())
            .ok_or(NotificationStoreError::MalformedKind)?,
        user: opt(row.try_get("user_id")?, |v| UserId::parse(v).ok())?,
        agent: opt(row.try_get("agent_id")?, |v| AgentId::parse(v).ok())?,
        workspace: opt(row.try_get("workspace_id")?, |v| WorkspaceId::parse(v).ok())?,
        conversation: opt(row.try_get("conversation_id")?, |v| {
            ConversationId::parse(v).ok()
        })?,
        message: opt(row.try_get("message_id")?, |v| MessageId::parse(v).ok())?,
        thread_root: opt(row.try_get("thread_root_id")?, |v| {
            ThreadRootId::parse(v).ok()
        })?,
        source_workspace: opt(row.try_get("source_workspace_id")?, |v| {
            WorkspaceId::parse(v).ok()
        })?,
        source_conversation: opt(row.try_get("source_conversation_id")?, |v| {
            ConversationId::parse(v).ok()
        })?,
        source_message: opt(row.try_get("source_message_id")?, |v| {
            MessageId::parse(v).ok()
        })?,
        payload_json,
        read_at: match row.try_get::<Option<i64>, _>("read_at")? {
            Some(value) => Some(
                NotificationTimestamp::new(value)
                    .ok_or(NotificationStoreError::MalformedTimestamp)?,
            ),
            None => None,
        },
        created_at: NotificationTimestamp::new(row.try_get("created_at")?)
            .ok_or(NotificationStoreError::MalformedTimestamp)?,
    })
}

/// The list read also carries the anchor message's author columns, so the sender is
/// resolved by the same author authority Activity uses (live row over snapshot).
macro_rules! list_select {
    () => {
        "SELECT n.id,n.kind,n.user_id,n.agent_id,n.workspace_id,n.conversation_id,n.message_id,m.thread_root_id,n.source_workspace_id,n.source_conversation_id,n.source_message_id,n.payload,n.read_at,n.created_at,m.from_kind,m.from_user_id,m.from_agent_id,m.author_id,m.author_name,m.author_display_name FROM notifications n LEFT JOIN messages m ON m.id=n.message_id WHERE n.user_id=?"
    };
}

/// Only mention kinds name a sender; a missing anchor message reads as system, so
/// it resolves to no sender rather than a fabricated author.
fn sender_evidence(row: &SqliteRow) -> Result<AuthorEvidence, NotificationStoreError> {
    let kind: String = row.try_get("kind")?;
    let from_kind = match (
        kind.as_str(),
        row.try_get::<Option<String>, _>("from_kind")?.as_deref(),
    ) {
        ("mention_self" | "mention_external", Some("user")) => FromKind::User,
        ("mention_self" | "mention_external", Some("agent")) => FromKind::Agent,
        (_, None | Some("system" | "user" | "agent")) => FromKind::System,
        (_, Some(_)) => return Err(NotificationStoreError::MalformedAuthorKind),
    };
    if from_kind == FromKind::System {
        return Ok(AuthorEvidence {
            from_kind,
            from_user_id: None,
            from_agent_id: None,
            author_id: None,
            author_name: None,
            author_display_name: None,
        });
    }
    Ok(AuthorEvidence {
        from_kind,
        from_user_id: row.try_get("from_user_id")?,
        from_agent_id: row.try_get("from_agent_id")?,
        author_id: row.try_get("author_id")?,
        author_name: row.try_get("author_name")?,
        author_display_name: row.try_get("author_display_name")?,
    })
}

impl<E: Clone + Send + Sync + 'static> NotificationReadPort for SqliteNotificationStore<E> {
    type Error = PersistenceFailure;
    async fn list(
        &self,
        viewer: &UserId,
        query: NotificationListQuery,
    ) -> Result<Vec<ListedNotification>, Self::Error> {
        let mut conn = self
            .pool
            .acquire()
            .await
            .map_err(NotificationStoreError::from)?;
        let rows = if query.unread_only {
            sqlx::query(concat!(
                list_select!(),
                " AND n.read_at IS NULL ORDER BY n.created_at DESC,n.id DESC LIMIT ?"
            ))
            .bind(viewer.as_str())
            .bind(i64::from(query.limit))
            .fetch_all(&mut *conn)
            .await
        } else {
            sqlx::query(concat!(
                list_select!(),
                " ORDER BY n.created_at DESC,n.id DESC LIMIT ?"
            ))
            .bind(viewer.as_str())
            .bind(i64::from(query.limit))
            .fetch_all(&mut *conn)
            .await
        }
        .map_err(NotificationStoreError::from)?;
        let evidence = rows
            .iter()
            .map(sender_evidence)
            .collect::<Result<Vec<_>, _>>()?;
        let notifications = rows
            .into_iter()
            .map(decode)
            .collect::<Result<Vec<_>, _>>()?;
        let senders = resolve_authors(&mut conn, &evidence).await?;
        Ok(notifications
            .into_iter()
            .zip(senders)
            .map(|(notification, sender)| ListedNotification {
                notification,
                sender,
            })
            .collect())
    }
    async fn unread_summary(&self, viewer: &UserId) -> Result<NotificationSummary, Self::Error> {
        let mut tx = self
            .pool
            .begin_with("BEGIN DEFERRED")
            .await
            .map_err(NotificationStoreError::from)?;
        let total: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM notifications WHERE user_id=? AND read_at IS NULL",
        )
        .bind(viewer.as_str())
        .fetch_one(&mut *tx)
        .await
        .map_err(NotificationStoreError::from)?;
        let counts=sqlx::query("SELECT COALESCE(n.workspace_id,c.workspace_id) scope_id,COUNT(*) c FROM notifications n LEFT JOIN conversations c ON c.id=n.conversation_id WHERE n.user_id=? AND n.read_at IS NULL AND COALESCE(n.workspace_id,c.workspace_id) IS NOT NULL GROUP BY scope_id ORDER BY scope_id").bind(viewer.as_str()).fetch_all(&mut *tx).await.map_err(NotificationStoreError::from)?;
        let latest=sqlx::query("SELECT scope_id,conversation_id,message_id,thread_root_id FROM (SELECT COALESCE(n.workspace_id,c.workspace_id) scope_id,n.conversation_id,m.id message_id,m.thread_root_id,ROW_NUMBER() OVER(PARTITION BY COALESCE(n.workspace_id,c.workspace_id) ORDER BY n.created_at DESC,n.id DESC) rn FROM notifications n JOIN conversations c ON c.id=n.conversation_id JOIN messages m ON m.id=n.message_id WHERE n.user_id=? AND n.read_at IS NULL AND c.archived_at IS NULL) WHERE rn=1 ORDER BY scope_id").bind(viewer.as_str()).fetch_all(&mut *tx).await.map_err(NotificationStoreError::from)?;
        let by_workspace = counts
            .into_iter()
            .map(|r| {
                Ok((
                    WorkspaceId::parse(r.try_get::<String, _>("scope_id")?)
                        .map_err(|_| NotificationStoreError::MalformedIdentity)?,
                    u64::try_from(r.try_get::<i64, _>("c")?)
                        .map_err(|_| NotificationStoreError::MalformedTimestamp)?,
                ))
            })
            .collect::<Result<_, NotificationStoreError>>()?;
        let latest_by_workspace = latest
            .into_iter()
            .map(|r| {
                Ok((
                    WorkspaceId::parse(r.try_get::<String, _>("scope_id")?)
                        .map_err(|_| NotificationStoreError::MalformedIdentity)?,
                    UnreadAnchor {
                        conversation: ConversationId::parse(
                            r.try_get::<String, _>("conversation_id")?,
                        )
                        .map_err(|_| NotificationStoreError::MalformedIdentity)?,
                        message: MessageId::parse(r.try_get::<String, _>("message_id")?)
                            .map_err(|_| NotificationStoreError::MalformedIdentity)?,
                        thread_root: opt(r.try_get("thread_root_id")?, |v| {
                            ThreadRootId::parse(v).ok()
                        })?,
                    },
                ))
            })
            .collect::<Result<_, NotificationStoreError>>()?;
        tx.commit().await.map_err(NotificationStoreError::from)?;
        Ok(NotificationSummary {
            total: u64::try_from(total).map_err(|_| NotificationStoreError::MalformedTimestamp)?,
            by_workspace,
            latest_by_workspace,
        })
    }
}

impl<E: SendClock + Clone + Send + Sync + 'static> NotificationClearPort
    for SqliteNotificationStore<E>
{
    type Error = PersistenceFailure;
    async fn clear(
        &self,
        viewer: UserId,
        selector: NotificationClearSelector,
    ) -> Result<CommittedNotificationClear, Self::Error> {
        let now = self
            .environment
            .now_unix_ms()
            .map_err(NotificationStoreError::Clock)?;
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(NotificationStoreError::from)?;
        let rows = match &selector {
            NotificationClearSelector::Id(v) => sqlx::query("SELECT n.id,n.kind,n.user_id,n.agent_id,n.workspace_id,n.conversation_id,n.message_id,m.thread_root_id,n.source_workspace_id,n.source_conversation_id,n.source_message_id,n.payload,n.read_at,n.created_at FROM notifications n LEFT JOIN messages m ON m.id=n.message_id WHERE n.user_id=? AND n.read_at IS NULL AND n.id=? ORDER BY n.id").bind(viewer.as_str()).bind(v.as_str()).fetch_all(&mut *tx).await,
            NotificationClearSelector::Workspace(v) => sqlx::query("SELECT n.id,n.kind,n.user_id,n.agent_id,n.workspace_id,n.conversation_id,n.message_id,m.thread_root_id,n.source_workspace_id,n.source_conversation_id,n.source_message_id,n.payload,n.read_at,n.created_at FROM notifications n LEFT JOIN messages m ON m.id=n.message_id WHERE n.user_id=? AND n.read_at IS NULL AND n.workspace_id=? ORDER BY n.id").bind(viewer.as_str()).bind(v.as_str()).fetch_all(&mut *tx).await,
            NotificationClearSelector::Conversation(v) => sqlx::query("SELECT n.id,n.kind,n.user_id,n.agent_id,n.workspace_id,n.conversation_id,n.message_id,m.thread_root_id,n.source_workspace_id,n.source_conversation_id,n.source_message_id,n.payload,n.read_at,n.created_at FROM notifications n LEFT JOIN messages m ON m.id=n.message_id WHERE n.user_id=? AND n.read_at IS NULL AND n.conversation_id=? ORDER BY n.id").bind(viewer.as_str()).bind(v.as_str()).fetch_all(&mut *tx).await,
            NotificationClearSelector::All => sqlx::query("SELECT n.id,n.kind,n.user_id,n.agent_id,n.workspace_id,n.conversation_id,n.message_id,m.thread_root_id,n.source_workspace_id,n.source_conversation_id,n.source_message_id,n.payload,n.read_at,n.created_at FROM notifications n LEFT JOIN messages m ON m.id=n.message_id WHERE n.user_id=? AND n.read_at IS NULL ORDER BY n.id").bind(viewer.as_str()).fetch_all(&mut *tx).await,
        }.map_err(NotificationStoreError::from)?;
        // Decode every selected row before the first mutation. A malformed id,
        // scope, kind, timestamp or payload is corruption, not a partially
        // committed clear followed by an error.
        let selected = rows
            .into_iter()
            .map(decode)
            .collect::<Result<Vec<_>, _>>()?;
        let mut marked = 0_u64;
        for notification in &selected {
            sqlx::query(
                "UPDATE notifications SET read_at=? WHERE id=? AND user_id=? AND read_at IS NULL",
            )
            .bind(now)
            .bind(notification.id.as_str())
            .bind(viewer.as_str())
            .execute(&mut *tx)
            .await
            .map_err(NotificationStoreError::from)
            .map(|done| marked += done.rows_affected())?;
        }
        tx.commit().await.map_err(NotificationStoreError::from)?;
        // The witness carries only the target result. No absolute badge projection is
        // sealed here: the client refetches the authoritative roll-up on the thin unread
        // signal, so an out-of-order effect cannot write back a stale count.
        Ok(match NonZeroU64::new(marked) {
            Some(marked) => CommittedNotificationClear::changed(viewer, marked),
            None => CommittedNotificationClear::unchanged(viewer),
        })
    }
}
