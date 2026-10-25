//! SQLite unit of work for the viewer-owned Activity observation batch.

use std::collections::{HashMap, HashSet};

use silverhand_backend_core::{
    conversation_messaging::messaging::{
        MessageSeq, NotificationId, ThreadRootId,
        collaboration::{
            activity::observe::{
                ObservationChanges, ObservationPort, ObserveBatch, ObserveRefusal, ObserveSource,
                ObservedConversation, ObservedSeq, StreamPosition,
            },
            notification::NotificationKind,
        },
        environment::SendClock,
    },
    foundation::identity::{ConversationId, UserId, WorkspaceId},
};
use sqlx::{Row, Sqlite, Transaction};
use thiserror::Error;

use super::valid_title_key_spelling;
use crate::conversation_messaging::messaging::{HostEnvironment, MessagingStore, ROOT_WRITE_BEGIN};

#[derive(Clone)]
pub struct SqliteObservationStore<E = HostEnvironment> {
    pool: crate::foundation::database::SqliteDatabase,
    environment: E,
}

impl<E: Clone> MessagingStore<E> {
    pub fn observation(&self) -> SqliteObservationStore<E> {
        SqliteObservationStore {
            pool: self.pool.clone(),
            environment: self.environment.clone(),
        }
    }
}

#[derive(Debug, Error)]
pub enum ObservationStoreError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("the observation clock could not answer")]
    Environment(
        #[source]
        silverhand_backend_core::conversation_messaging::messaging::EnvironmentUnavailable,
    ),
    #[error("stored observation coordinate {0} is malformed")]
    MalformedIdentity(&'static str),
    #[error("stored observation ordinal is outside its domain")]
    MalformedOrdinal,
    #[error("stored conversation placement is outside the closed schema domain")]
    MalformedPlacement,
    #[error("stored notification kind is outside the closed product domain")]
    MalformedNotificationKind,
}

#[derive(Clone)]
enum Placement {
    Workspace(WorkspaceId),
    DirectPair { viewer_is_endpoint: bool },
}

#[derive(Clone)]
struct ResolvedStream {
    conversation: ConversationId,
    thread_root: Option<ThreadRootId>,
    target: MessageSeq,
    placement: Placement,
}

enum ResolvedSource {
    Stream(ResolvedStream),
    Notification(NotificationId),
}

impl<E> ObservationPort for SqliteObservationStore<E>
where
    E: SendClock + Send + Sync,
{
    type Error = ObservationStoreError;

    async fn observe(
        &self,
        viewer: UserId,
        batch: ObserveBatch,
    ) -> Result<Result<ObservationChanges, ObserveRefusal>, Self::Error> {
        // This is a write unit, even though source validation comes first. Acquire
        // the existing writer boundary before reading mutable ACL/head facts: a
        // deferred snapshot cannot safely upgrade while another writer is active.
        let mut tx = self.pool.begin_with(ROOT_WRITE_BEGIN).await?;
        let mut resolved = Vec::with_capacity(batch.sources().len());
        for source in batch.sources() {
            match resolve_source(&mut tx, &viewer, source).await? {
                Ok(source) => resolved.push(source),
                Err(refusal) => {
                    tx.rollback().await?;
                    return Ok(Err(refusal));
                }
            }
        }

        // The clock is a write authority, so validation/refusal never depends on it.
        let now = match self.environment.now_unix_ms() {
            Ok(now) => now,
            Err(error) => {
                tx.rollback().await?;
                return Err(ObservationStoreError::Environment(error));
            }
        };
        let mut conversations = HashMap::<String, ObservedConversation>::new();
        let mut directed_badges = HashSet::<String>::new();
        let mut dismissed = HashMap::<String, NotificationId>::new();

        for source in resolved {
            match source {
                ResolvedSource::Stream(stream) => {
                    let cursor_changed = advance_cursor(&mut tx, &viewer, &stream, now).await?;
                    let cleared = clear_stream_mentions(&mut tx, &viewer, &stream, now).await?;
                    if cursor_changed || !cleared.is_empty() {
                        conversations
                            .entry(stream.conversation.as_str().to_owned())
                            .or_insert_with(|| match &stream.placement {
                                Placement::Workspace(workspace) => ObservedConversation::workspace(
                                    stream.conversation.clone(),
                                    workspace.clone(),
                                ),
                                Placement::DirectPair { .. } => {
                                    ObservedConversation::direct_pair(stream.conversation.clone())
                                }
                            });
                    }
                    for notification in cleared {
                        dismissed
                            .entry(notification.as_str().to_owned())
                            .or_insert(notification);
                    }
                }
                ResolvedSource::Notification(notification) => {
                    if let Some(cleared) =
                        clear_notification(&mut tx, &viewer, &notification, now).await?
                    {
                        dismissed
                            .entry(notification.as_str().to_owned())
                            .or_insert(notification);
                        if cleared.kind == NotificationKind::MentionSelf
                            && let Some(workspace) = cleared.workspace
                        {
                            directed_badges.insert(workspace.into_inner());
                        }
                    }
                }
            }
        }
        tx.commit().await?;

        if conversations.is_empty() && dismissed.is_empty() {
            return Ok(Ok(ObservationChanges::unchanged()));
        }
        Ok(Ok(ObservationChanges::changed(
            conversations.into_values().collect(),
            directed_badges
                .into_iter()
                .map(WorkspaceId::parse)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| ObservationStoreError::MalformedIdentity("workspace_id"))?,
            dismissed.into_values().collect(),
        )))
    }
}

async fn resolve_source(
    tx: &mut Transaction<'_, Sqlite>,
    viewer: &UserId,
    source: &ObserveSource,
) -> Result<Result<ResolvedSource, ObserveRefusal>, ObservationStoreError> {
    let ObserveSource::Stream {
        conversation,
        thread_root,
        position,
    } = source
    else {
        let ObserveSource::Notification(notification) = source else {
            unreachable!()
        };
        return Ok(Ok(ResolvedSource::Notification(notification.clone())));
    };

    let row = sqlx::query(
        "SELECT c.placement_kind,c.workspace_id,c.roster_policy,c.title_key,c.endpoint_a_kind, \
         c.endpoint_a_id,c.endpoint_a_deleted_at,c.endpoint_b_kind,c.endpoint_b_id,c.endpoint_b_deleted_at, \
         EXISTS(SELECT 1 FROM workspace_users wu WHERE wu.workspace_id=c.workspace_id AND wu.user_id=?) AS in_workspace, \
         EXISTS(SELECT 1 FROM conversation_users cu WHERE cu.conversation_id=c.id AND cu.user_id=?) AS in_conversation \
         FROM conversations c WHERE c.id=?",
    )
    .bind(viewer.as_str())
    .bind(viewer.as_str())
    .bind(conversation.as_str())
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row else {
        return Ok(Err(ObserveRefusal::ConversationNotFound));
    };
    let placement = decode_placement(&row, viewer)?;
    let allowed = match &placement {
        Placement::Workspace(_) => {
            row.try_get::<i64, _>("in_workspace")? == 1
                && row.try_get::<i64, _>("in_conversation")? == 1
        }
        Placement::DirectPair { viewer_is_endpoint } => *viewer_is_endpoint,
    };
    if !allowed {
        return Ok(Err(ObserveRefusal::Forbidden));
    }

    if let Some(root) = thread_root {
        let root_row =
            sqlx::query("SELECT conversation_id,thread_root_id FROM messages WHERE id=?")
                .bind(root.as_str())
                .fetch_optional(&mut **tx)
                .await?;
        let Some(root_row) = root_row else {
            return Ok(Err(ObserveRefusal::ThreadRootNotFound));
        };
        let root_conversation: String = root_row.try_get("conversation_id")?;
        ConversationId::parse(root_conversation.clone())
            .map_err(|_| ObservationStoreError::MalformedIdentity("messages.conversation_id"))?;
        if root_conversation != conversation.as_str() {
            return Ok(Err(ObserveRefusal::ThreadRootNotFound));
        }
        let parent: Option<String> = root_row.try_get("thread_root_id")?;
        if let Some(parent) = parent {
            ThreadRootId::parse(parent)
                .map_err(|_| ObservationStoreError::MalformedIdentity("messages.thread_root_id"))?;
            return Ok(Err(ObserveRefusal::ThreadRootIsReply));
        }
    }

    let head = sqlx::query_scalar::<_, i64>(
        "SELECT latest_seq FROM conversation_stream_heads WHERE conversation_id=? AND thread_root_id IS ?",
    )
    .bind(conversation.as_str())
    .bind(thread_root.as_ref().map(ThreadRootId::as_str))
    .fetch_optional(&mut **tx)
    .await?
    .unwrap_or(0);
    let head = MessageSeq::try_from(head).map_err(|_| ObservationStoreError::MalformedOrdinal)?;
    let target = match position {
        StreamPosition::AtSeq(ObservedSeq::BeyondPersistedRange) => {
            return Ok(Err(ObserveRefusal::PositionFuture));
        }
        StreamPosition::AtSeq(ObservedSeq::Persisted(target)) if target > &head => {
            return Ok(Err(ObserveRefusal::PositionFuture));
        }
        StreamPosition::AtSeq(ObservedSeq::Persisted(target)) => *target,
        StreamPosition::StreamHead => head,
    };
    Ok(Ok(ResolvedSource::Stream(ResolvedStream {
        conversation: conversation.clone(),
        thread_root: thread_root.clone(),
        target,
        placement,
    })))
}

fn decode_placement(
    row: &sqlx::sqlite::SqliteRow,
    viewer: &UserId,
) -> Result<Placement, ObservationStoreError> {
    let kind: String = row.try_get("placement_kind")?;
    let workspace: Option<String> = row.try_get("workspace_id")?;
    let roster: String = row.try_get("roster_policy")?;
    let title_key: Option<String> = row.try_get("title_key")?;
    let a_kind: Option<String> = row.try_get("endpoint_a_kind")?;
    let a_id: Option<String> = row.try_get("endpoint_a_id")?;
    let a_deleted: Option<i64> = row.try_get("endpoint_a_deleted_at")?;
    let b_kind: Option<String> = row.try_get("endpoint_b_kind")?;
    let b_id: Option<String> = row.try_get("endpoint_b_id")?;
    let b_deleted: Option<i64> = row.try_get("endpoint_b_deleted_at")?;
    match kind.as_str() {
        "workspace"
            if workspace.is_some()
                && title_key.as_deref().is_some_and(valid_title_key_spelling)
                && a_kind.is_none()
                && a_id.is_none()
                && a_deleted.is_none()
                && b_kind.is_none()
                && b_id.is_none()
                && b_deleted.is_none()
                && matches!(roster.as_str(), "open" | "fixed") =>
        {
            Ok(Placement::Workspace(
                WorkspaceId::parse(workspace.unwrap()).map_err(|_| {
                    ObservationStoreError::MalformedIdentity("conversations.workspace_id")
                })?,
            ))
        }
        "direct_pair"
            if workspace.is_none()
                && title_key.is_none()
                && roster == "fixed"
                && matches!(a_kind.as_deref(), Some("human" | "agent"))
                && matches!(b_kind.as_deref(), Some("human" | "agent"))
                && a_id.as_deref().is_some_and(|value| !value.is_empty())
                && b_id.as_deref().is_some_and(|value| !value.is_empty())
                && (a_kind.as_deref(), a_id.as_deref()) < (b_kind.as_deref(), b_id.as_deref()) =>
        {
            let viewer_is_endpoint = (a_kind.as_deref() == Some("human")
                && a_id.as_deref() == Some(viewer.as_str()))
                || (b_kind.as_deref() == Some("human") && b_id.as_deref() == Some(viewer.as_str()));
            if viewer_is_endpoint {
                Ok(Placement::DirectPair {
                    viewer_is_endpoint: true,
                })
            } else {
                Ok(Placement::DirectPair {
                    viewer_is_endpoint: false,
                })
            }
        }
        _ => Err(ObservationStoreError::MalformedPlacement),
    }
}

async fn advance_cursor(
    tx: &mut Transaction<'_, Sqlite>,
    viewer: &UserId,
    stream: &ResolvedStream,
    now: i64,
) -> Result<bool, ObservationStoreError> {
    let current = sqlx::query_scalar::<_, i64>(
        "SELECT last_read_seq FROM conversation_reads WHERE user_id=? AND conversation_id=? AND thread_root_id IS ?",
    )
    .bind(viewer.as_str())
    .bind(stream.conversation.as_str())
    .bind(stream.thread_root.as_ref().map(ThreadRootId::as_str))
    .fetch_optional(&mut **tx)
    .await?
    .unwrap_or(0);
    MessageSeq::try_from(current).map_err(|_| ObservationStoreError::MalformedOrdinal)?;
    if stream.target.get() <= current {
        return Ok(false);
    }
    if stream.thread_root.is_none() {
        sqlx::query(
            "INSERT INTO conversation_reads(user_id,conversation_id,thread_root_id,last_read_seq,read_at) VALUES(?,?,NULL,?,?) \
             ON CONFLICT(user_id,conversation_id) WHERE thread_root_id IS NULL DO UPDATE SET \
             last_read_seq=max(conversation_reads.last_read_seq,excluded.last_read_seq),read_at=excluded.read_at",
        )
        .bind(viewer.as_str())
        .bind(stream.conversation.as_str())
        .bind(stream.target.get())
        .bind(now)
        .execute(&mut **tx)
        .await?;
    } else {
        sqlx::query(
            "INSERT INTO conversation_reads(user_id,conversation_id,thread_root_id,last_read_seq,read_at) VALUES(?,?,?,?,?) \
             ON CONFLICT(user_id,conversation_id,thread_root_id) WHERE thread_root_id IS NOT NULL DO UPDATE SET \
             last_read_seq=max(conversation_reads.last_read_seq,excluded.last_read_seq),read_at=excluded.read_at",
        )
        .bind(viewer.as_str())
        .bind(stream.conversation.as_str())
        .bind(stream.thread_root.as_ref().map(ThreadRootId::as_str))
        .bind(stream.target.get())
        .bind(now)
        .execute(&mut **tx)
        .await?;
    }
    Ok(true)
}

async fn clear_stream_mentions(
    tx: &mut Transaction<'_, Sqlite>,
    viewer: &UserId,
    stream: &ResolvedStream,
    now: i64,
) -> Result<Vec<NotificationId>, ObservationStoreError> {
    let rows = sqlx::query(
        "UPDATE notifications SET read_at=? WHERE user_id=? AND kind='mention_self' AND read_at IS NULL \
         AND conversation_id=? AND message_id IN (SELECT id FROM messages WHERE conversation_id=? \
         AND thread_root_id IS ? AND seq<=?) RETURNING id",
    )
    .bind(now)
    .bind(viewer.as_str())
    .bind(stream.conversation.as_str())
    .bind(stream.conversation.as_str())
    .bind(stream.thread_root.as_ref().map(ThreadRootId::as_str))
    .bind(stream.target.get())
    .fetch_all(&mut **tx)
    .await?;
    rows.into_iter()
        .map(|row| {
            NotificationId::parse(row.try_get::<String, _>("id")?)
                .map_err(|_| ObservationStoreError::MalformedIdentity("notifications.id"))
        })
        .collect()
}

struct ClearedNotification {
    kind: NotificationKind,
    workspace: Option<WorkspaceId>,
}

async fn clear_notification(
    tx: &mut Transaction<'_, Sqlite>,
    viewer: &UserId,
    notification: &NotificationId,
    now: i64,
) -> Result<Option<ClearedNotification>, ObservationStoreError> {
    let row = sqlx::query(
        "SELECT n.id,n.kind,n.conversation_id,c.placement_kind,c.workspace_id,c.title_key, \
         c.roster_policy,c.endpoint_a_kind,c.endpoint_a_id,c.endpoint_a_deleted_at, \
         c.endpoint_b_kind,c.endpoint_b_id,c.endpoint_b_deleted_at \
         FROM notifications n LEFT JOIN conversations c ON c.id=n.conversation_id \
         WHERE n.id=? AND n.user_id=? AND n.read_at IS NULL",
    )
    .bind(notification.as_str())
    .bind(viewer.as_str())
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    NotificationId::parse(row.try_get::<String, _>("id")?)
        .map_err(|_| ObservationStoreError::MalformedIdentity("notifications.id"))?;
    let kind = NotificationKind::parse(&row.try_get::<String, _>("kind")?)
        .ok_or(ObservationStoreError::MalformedNotificationKind)?;
    let conversation: Option<String> = row.try_get("conversation_id")?;
    let workspace = if let Some(conversation) = conversation {
        ConversationId::parse(conversation).map_err(|_| {
            ObservationStoreError::MalformedIdentity("notifications.conversation_id")
        })?;
        match decode_placement(&row, viewer) {
            Ok(Placement::Workspace(workspace)) => Some(workspace),
            Ok(Placement::DirectPair { .. }) => None,
            Err(error) => return Err(error),
        }
    } else {
        None
    };
    let changed = sqlx::query(
        "UPDATE notifications SET read_at=? WHERE id=? AND user_id=? AND read_at IS NULL",
    )
    .bind(now)
    .bind(notification.as_str())
    .bind(viewer.as_str())
    .execute(&mut **tx)
    .await?
    .rows_affected()
        == 1;
    Ok(changed.then_some(ClearedNotification { kind, workspace }))
}
