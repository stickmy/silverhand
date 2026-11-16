//! SQLite Activity cold projection.
//!
//! Agent failures and streams are read in one deferred snapshot. A stream enters when
//! the viewer ambiently enters its unread or has an unread mention in it. Existing
//! unread, attention-tier, author, and direct-pair authorities are composed rather
//! than transcribed.

use crate::foundation::conversation_lifecycle_sql::conversation_open;
use crate::{
    conversation_messaging::direct_pair_read::{DirectPairReadError, LivePrincipals, PairEvidence},
    conversation_messaging::messaging::{
        MessagingStore,
        author::{AuthorEvidence, resolve_authors},
    },
    viewer_read_model::unread_read::{
        CHANNEL_DEFAULT_MODE, channel_ambient_enters, effective_directed_mention,
        message_unread_guards,
    },
};
use silverhand_backend_core::{
    conversation_messaging::messaging::{
        FromKind, MessageId, NotificationId, ThreadRootId,
        collaboration::activity::{
            ActivityItem, ActivityList, ActivityLocation, ActivityPreview, ActivityReadModel,
            ActivitySalience, ActivityUnreadCount,
        },
    },
    foundation::identity::{ConversationId, UserId, WorkspaceId},
    foundation::persistence::PersistenceFailure,
};
use silverhand_backend_observability::Observe;
use sqlx::{Row, sqlite::SqliteRow};
use thiserror::Error;

pub mod observe;

/// The viewer can see this conversation: open, a workspace channel or a direct pair
/// they are an endpoint of, inside the requested workspace if any.
macro_rules! activity_visible {
    () => {
        concat!(
            conversation_open!("c."),
            " AND (c.placement_kind='workspace' OR (c.placement_kind='direct_pair' AND \
             ((c.endpoint_a_kind='human' AND c.endpoint_a_id=(SELECT viewer FROM p)) OR \
             (c.endpoint_b_kind='human' AND c.endpoint_b_id=(SELECT viewer FROM p))))) AND \
             ((SELECT ws FROM p) IS NULL OR c.workspace_id=(SELECT ws FROM p))"
        )
    };
}

macro_rules! conversation_columns {
    () => {
        "c.id AS id, c.created_at AS created_at, c.placement_kind, c.workspace_id, \
         c.roster_policy, c.title AS conversation_title, c.title_key AS conversation_title_key, \
         c.endpoint_a_kind, c.endpoint_a_id, c.endpoint_a_deleted_at, c.endpoint_b_kind, \
         c.endpoint_b_id, c.endpoint_b_deleted_at, "
    };
}

macro_rules! author_columns {
    () => {
        "m.from_kind, m.from_user_id, m.from_agent_id, m.author_id, m.author_name, \
         m.author_display_name "
    };
}

const AGENT_FAILURES: &str = concat!(
    "WITH p(viewer, ws) AS (SELECT ?, ?) SELECT n.id AS notification_id, \
     c.id AS conversation_id, NULL AS thread_root_id, NULL AS root_content, n.payload, \
     n.created_at AS item_created_at, ",
    conversation_columns!(),
    "NULL AS from_kind, NULL AS from_user_id, NULL AS from_agent_id, NULL AS author_id, \
     NULL AS author_name, NULL AS author_display_name FROM notifications n JOIN conversations c \
     ON c.id=n.conversation_id WHERE n.user_id=(SELECT viewer FROM p) AND \
     n.kind='agent_start_failed' AND n.read_at IS NULL AND ",
    activity_visible!(),
    " ORDER BY n.created_at DESC, n.id ASC"
);

/// One row per stream that waits on the viewer: ambiently entered unread, or an
/// unread mention of the viewer. `unread_count` is every unread message of an entered
/// stream, else just its mentions; the anchor is the first unread mention, else the
/// first unread message.
const STREAMS: &str = concat!(
    "WITH p(viewer, ws, cdm) AS (SELECT ?, ?, ?), viewer_conversation AS (\
       SELECT cu.conversation_id FROM conversation_users cu JOIN conversations c_seat ON \
       c_seat.id=cu.conversation_id WHERE cu.user_id=(SELECT viewer FROM p) AND \
       c_seat.placement_kind='workspace' UNION ALL SELECT c.id FROM conversations c WHERE \
       c.placement_kind='direct_pair' AND ((c.endpoint_a_kind='human' AND \
       c.endpoint_a_id=(SELECT viewer FROM p)) OR (c.endpoint_b_kind='human' AND \
       c.endpoint_b_id=(SELECT viewer FROM p)))\
     ), entering_stream AS (\
       SELECT c.id AS conversation_id, sh.thread_root_id AS root, \
       COALESCE(cr.last_read_seq,0) AS cursor_seq FROM viewer_conversation vc \
       JOIN conversations c ON c.id=vc.conversation_id JOIN conversation_stream_heads sh ON \
       sh.conversation_id=c.id LEFT JOIN conversation_reads cr ON cr.user_id=(SELECT viewer FROM p) \
       AND cr.conversation_id=c.id AND cr.thread_root_id IS sh.thread_root_id WHERE ",
    conversation_open!("c."),
    " AND ((SELECT ws FROM p) IS NULL OR \
       c.workspace_id=(SELECT ws FROM p)) AND ((sh.thread_root_id IS NULL AND \
       c.roster_policy='fixed') OR (sh.thread_root_id IS NULL AND c.roster_policy='open' AND ",
    channel_ambient_enters!(),
    ") OR (sh.thread_root_id IS NOT NULL AND EXISTS (SELECT 1 FROM thread_followers tf WHERE \
       tf.thread_root_id=sh.thread_root_id AND tf.principal_kind='human' AND \
       tf.principal_id=(SELECT viewer FROM p)) AND ",
    channel_ambient_enters!(),
    "))\
     ), unread_tail AS (\
       SELECT es.conversation_id, es.root, m.seq FROM entering_stream es CROSS JOIN messages m ON \
       m.conversation_id=es.conversation_id AND m.thread_root_id IS es.root AND \
       m.seq>es.cursor_seq WHERE ",
    // entering_stream already excludes archived conversations in this snapshot.
    // Do not look up that same conversation again for every unread message.
    message_unread_guards!(),
    "), entered AS (\
       SELECT conversation_id, root, COUNT(*) AS unread_count, MIN(seq) AS first_unread_seq \
       FROM unread_tail GROUP BY conversation_id, root\
     ), mentioned AS (\
       SELECT c.id AS conversation_id, m.thread_root_id AS root, \
       COUNT(DISTINCT n.message_id) AS mention_count, MIN(m.seq) AS first_mention_seq \
       FROM notifications n JOIN messages m ON m.id=n.message_id JOIN conversations c ON \
       c.id=n.conversation_id WHERE n.user_id=(SELECT viewer FROM p) AND ",
    effective_directed_mention!(),
    " AND ",
    activity_visible!(),
    " GROUP BY c.id, m.thread_root_id\
     ), stream AS (\
       SELECT conversation_id, root FROM entered UNION SELECT conversation_id, root FROM mentioned\
     ) SELECT s.conversation_id, s.root AS thread_root_id, root.content AS root_content, \
       m.id AS message_id, m.content AS message_content, m.created_at AS item_created_at, \
       COALESCE(e.unread_count, mn.mention_count) AS unread_count, \
       COALESCE(mn.mention_count, 0) AS mention_count, ",
    conversation_columns!(),
    author_columns!(),
    "FROM stream s LEFT JOIN entered e ON e.conversation_id=s.conversation_id AND e.root IS s.root \
       LEFT JOIN mentioned mn ON mn.conversation_id=s.conversation_id AND mn.root IS s.root \
       JOIN messages m ON m.conversation_id=s.conversation_id AND \
       m.seq=COALESCE(mn.first_mention_seq, e.first_unread_seq) JOIN conversations c ON \
       c.id=s.conversation_id LEFT JOIN messages root ON root.id=s.root \
       ORDER BY s.conversation_id ASC, s.root IS NOT NULL ASC, s.root ASC"
);

#[derive(Clone)]
pub struct SqliteActivityReadModel {
    pool: crate::foundation::database::SqliteDatabase,
}

impl<E> MessagingStore<E> {
    pub fn activity_read_model(&self) -> SqliteActivityReadModel {
        SqliteActivityReadModel {
            pool: self.pool.clone(),
        }
    }
}

#[derive(Debug, Error)]
pub enum ActivityStoreError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("stored Activity identity is malformed in {0}")]
    MalformedIdentity(&'static str),
    #[error("stored Activity placement is outside the closed set")]
    MalformedPlacement,
    #[error("stored Activity author kind is outside the closed set")]
    MalformedAuthorKind,
    #[error("stored Activity roster policy is outside the closed set")]
    MalformedRoster,
    #[error("stored Activity unread count is negative")]
    NegativeUnreadCount,
    #[error("direct-pair counterpart projection failed")]
    DirectPair(#[source] DirectPairReadError),
    #[error("author projection failed")]
    Author(#[source] PersistenceFailure),
}

impl ActivityReadModel for SqliteActivityReadModel {
    type Error = PersistenceFailure;

    async fn list_activity(
        &self,
        viewer: &UserId,
        workspace: Option<&WorkspaceId>,
    ) -> Result<ActivityList, Self::Error> {
        self.list(viewer, workspace)
            .await
            .map_err(PersistenceFailure::from_source)
    }
}

impl SqliteActivityReadModel {
    #[tracing::instrument(
        target = "silverhand::diagnostics",
        name = "db.activity",
        skip_all,
        fields(
            elapsed_ms = tracing::field::Empty,
            outcome = tracing::field::Empty,
            operation_finished = tracing::field::Empty,
        )
    )]
    async fn list(
        &self,
        viewer: &UserId,
        workspace: Option<&WorkspaceId>,
    ) -> Result<ActivityList, ActivityStoreError> {
        let observation = silverhand_backend_observability::Operation::current();
        let result = async {
            let mut tx =
                crate::foundation::statement_metrics::begin(&self.pool, Some("BEGIN DEFERRED"))
                    .await?;
            let ws = workspace.map(WorkspaceId::as_str);
            let failure_rows = sqlx::query(AGENT_FAILURES)
                .bind(viewer.as_str())
                .bind(ws)
                .fetch_all(&mut *tx)
                .observe(silverhand_backend_observability::operation_span!(
                    "db.activity.agent_failures"
                ))
                .await?;
            let mut projected = failure_rows
                .iter()
                .map(RawItem::agent_failed)
                .collect::<Result<Vec<_>, _>>()?;
            let stream_rows = sqlx::query(STREAMS)
                .bind(viewer.as_str())
                .bind(ws)
                .bind(CHANNEL_DEFAULT_MODE)
                .fetch_all(&mut *tx)
                .observe(silverhand_backend_observability::operation_span!(
                    "db.activity.streams"
                ))
                .await?;
            for row in stream_rows {
                projected.push(RawItem::stream(&row)?);
            }

            let evidence = projected
                .iter()
                .map(|item| item.author.clone())
                .collect::<Vec<_>>();
            let authors = resolve_authors(&mut tx, &evidence)
                .await
                .map_err(ActivityStoreError::Author)?;

            let pair_values = projected
                .iter()
                .filter_map(|item| match &item.placement {
                    RawPlacement::DirectPair(pair) => Some(pair.clone()),
                    RawPlacement::Workspace { .. } => None,
                })
                .collect::<Vec<_>>();
            let principals = LivePrincipals::load(&mut tx, &pair_values)
                .await
                .map_err(ActivityStoreError::DirectPair)?;

            let mut items = Vec::with_capacity(projected.len());
            for (raw, author) in projected.into_iter().zip(authors) {
                items.push(raw.finish(viewer, &principals, author)?);
            }
            tx.commit().await?;
            Ok(ActivityList::projected(items))
        }
        .await;
        observation.finish(&result);
        result
    }
}

#[derive(Clone)]
enum RawSource {
    AgentFailed {
        notification: NotificationId,
    },
    Stream {
        message: MessageId,
        unread: ActivityUnreadCount,
        mentions: ActivityUnreadCount,
        salience: ActivitySalience,
    },
}

#[derive(Clone)]
struct RawItem {
    source: RawSource,
    conversation: ConversationId,
    thread_root: Option<ThreadRootId>,
    placement: RawPlacement,
    root_preview: Option<ActivityPreview>,
    preview: Option<ActivityPreview>,
    author: AuthorEvidence,
    timestamp_ms: i64,
}

#[derive(Clone)]
enum RawPlacement {
    Workspace {
        workspace: WorkspaceId,
        title: String,
    },
    DirectPair(PairEvidence),
}

impl RawItem {
    fn agent_failed(row: &SqliteRow) -> Result<Self, ActivityStoreError> {
        let payload: Option<String> = row.try_get("payload")?;
        let summary = payload
            .as_deref()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
            .and_then(|value| {
                let name = value.get("agent_name").and_then(serde_json::Value::as_str);
                let error = value.get("error").and_then(serde_json::Value::as_str);
                let joined = [name, error]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(" — ");
                (!joined.is_empty()).then_some(joined)
            });
        let mut item = Self::common(row)?;
        item.source = RawSource::AgentFailed {
            notification: notification_id(row)?,
        };
        item.thread_root = None;
        item.root_preview = None;
        item.preview = ActivityPreview::from_source(summary.as_deref());
        item.author = system_author();
        Ok(item)
    }

    fn stream(row: &SqliteRow) -> Result<Self, ActivityStoreError> {
        let count = |column| {
            ActivityUnreadCount::new(row.try_get::<i64, _>(column)?)
                .map_err(|_| ActivityStoreError::NegativeUnreadCount)
        };
        let unread = count("unread_count")?;
        let mentions = count("mention_count")?;
        let roster: String = row.try_get("roster_policy")?;
        let salience = match roster.as_str() {
            _ if mentions.get() > 0 => ActivitySalience::Directed,
            "fixed" => ActivitySalience::Directed,
            "open" => ActivitySalience::Ambient,
            _ => return Err(ActivityStoreError::MalformedRoster),
        };
        let message: String = row.try_get("message_id")?;
        Ok(Self {
            source: RawSource::Stream {
                message: message_id(message, "messages.id")?,
                unread,
                mentions,
                salience,
            },
            preview: ActivityPreview::from_source(
                row.try_get::<Option<String>, _>("message_content")?
                    .as_deref(),
            ),
            ..Self::common(row)?
        })
    }

    fn common(row: &SqliteRow) -> Result<Self, ActivityStoreError> {
        let conversation = conversation_id(row.try_get("conversation_id")?, "conversations.id")?;
        let placement_kind: String = row.try_get("placement_kind")?;
        let roster: String = row.try_get("roster_policy")?;
        let workspace: Option<String> = row.try_get("workspace_id")?;
        let title: String = row.try_get("conversation_title")?;
        let title_key: Option<String> = row.try_get("conversation_title_key")?;
        let endpoint_a_kind: Option<String> = row.try_get("endpoint_a_kind")?;
        let endpoint_a_id: Option<String> = row.try_get("endpoint_a_id")?;
        let endpoint_b_kind: Option<String> = row.try_get("endpoint_b_kind")?;
        let endpoint_b_id: Option<String> = row.try_get("endpoint_b_id")?;
        let endpoint_a_deleted_at: Option<i64> = row.try_get("endpoint_a_deleted_at")?;
        let endpoint_b_deleted_at: Option<i64> = row.try_get("endpoint_b_deleted_at")?;
        let placement = match placement_kind.as_str() {
            "workspace" => {
                if !matches!(roster.as_str(), "open" | "fixed")
                    || title_key
                        .as_deref()
                        .is_none_or(|key| !valid_title_key_spelling(key))
                    || endpoint_a_kind.is_some()
                    || endpoint_a_id.is_some()
                    || endpoint_b_kind.is_some()
                    || endpoint_b_id.is_some()
                    || endpoint_a_deleted_at.is_some()
                    || endpoint_b_deleted_at.is_some()
                {
                    return Err(ActivityStoreError::MalformedPlacement);
                }
                let workspace =
                    WorkspaceId::parse(workspace.ok_or(ActivityStoreError::MalformedPlacement)?)
                        .map_err(|_| {
                            ActivityStoreError::MalformedIdentity("conversations.workspace_id")
                        })?;
                RawPlacement::Workspace { workspace, title }
            }
            "direct_pair" => {
                let endpoints = match (
                    endpoint_a_kind.as_deref(),
                    endpoint_a_id.as_deref(),
                    endpoint_b_kind.as_deref(),
                    endpoint_b_id.as_deref(),
                ) {
                    (Some(a_kind), Some(a_id), Some(b_kind), Some(b_id)) => {
                        ((a_kind, a_id), (b_kind, b_id))
                    }
                    _ => return Err(ActivityStoreError::MalformedPlacement),
                };
                if workspace.is_some()
                    || roster != "fixed"
                    || title_key.is_some()
                    || endpoints.0 >= endpoints.1
                {
                    return Err(ActivityStoreError::MalformedPlacement);
                }
                RawPlacement::DirectPair(
                    PairEvidence::decode(row).map_err(ActivityStoreError::DirectPair)?,
                )
            }
            _ => return Err(ActivityStoreError::MalformedPlacement),
        };
        Ok(Self {
            source: RawSource::AgentFailed {
                notification: NotificationId::parse("placeholder")
                    .expect("static non-empty notification id"),
            },
            conversation,
            thread_root: optional_thread_root(row.try_get("thread_root_id")?)?,
            placement,
            root_preview: ActivityPreview::from_source(
                row.try_get::<Option<String>, _>("root_content")?.as_deref(),
            ),
            preview: None,
            author: author_evidence(row)?,
            timestamp_ms: row.try_get("item_created_at")?,
        })
    }

    fn finish(
        self,
        viewer: &UserId,
        principals: &LivePrincipals,
        author: Option<silverhand_backend_core::conversation_messaging::messaging::MessageAuthor>,
    ) -> Result<ActivityItem, ActivityStoreError> {
        let location = match self.placement {
            RawPlacement::Workspace { workspace, title } => {
                ActivityLocation::workspace_channel(workspace, title)
            }
            RawPlacement::DirectPair(pair) => {
                let endpoint = pair
                    .counterpart(viewer)
                    .map_err(ActivityStoreError::DirectPair)?;
                let counterpart = principals
                    .counterpart(&endpoint)
                    .map_err(ActivityStoreError::DirectPair)?;
                ActivityLocation::direct_pair(counterpart)
            }
        };
        Ok(match self.source {
            RawSource::AgentFailed { notification } => ActivityItem::agent_failed(
                notification,
                self.conversation,
                location,
                self.preview,
                self.timestamp_ms,
            ),
            RawSource::Stream {
                message,
                unread,
                mentions,
                salience,
            } => ActivityItem::stream(
                message,
                unread,
                mentions,
                salience,
                self.conversation,
                self.thread_root,
                location,
                self.root_preview,
                self.preview,
                author,
                self.timestamp_ms,
            ),
        })
    }
}

/// Mirrors only the schema's row-local spelling guard. The terminal writer and
/// boot integrity audit own the stronger `title` ↔ normalized-key relation;
/// Activity must not create a third canonicalization authority for that pair.
pub(super) fn valid_title_key_spelling(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with(' ')
        && !value.ends_with(' ')
        && !value.bytes().any(|byte| byte.is_ascii_uppercase())
}

fn author_evidence(row: &SqliteRow) -> Result<AuthorEvidence, ActivityStoreError> {
    let raw: Option<String> = row.try_get("from_kind")?;
    let from_kind = match raw.as_deref() {
        None | Some("system") => FromKind::System,
        Some("user") => FromKind::User,
        Some("agent") => FromKind::Agent,
        Some(_) => return Err(ActivityStoreError::MalformedAuthorKind),
    };
    Ok(AuthorEvidence {
        from_kind,
        from_user_id: row.try_get("from_user_id")?,
        from_agent_id: row.try_get("from_agent_id")?,
        author_id: row.try_get("author_id")?,
        author_name: row.try_get("author_name")?,
        author_display_name: row.try_get("author_display_name")?,
    })
}

fn system_author() -> AuthorEvidence {
    AuthorEvidence {
        from_kind: FromKind::System,
        from_user_id: None,
        from_agent_id: None,
        author_id: None,
        author_name: None,
        author_display_name: None,
    }
}

fn notification_id(row: &SqliteRow) -> Result<NotificationId, ActivityStoreError> {
    NotificationId::parse(row.try_get::<String, _>("notification_id")?)
        .map_err(|_| ActivityStoreError::MalformedIdentity("notifications.id"))
}

fn conversation_id(raw: String, field: &'static str) -> Result<ConversationId, ActivityStoreError> {
    ConversationId::parse(raw).map_err(|_| ActivityStoreError::MalformedIdentity(field))
}

fn message_id(raw: String, field: &'static str) -> Result<MessageId, ActivityStoreError> {
    MessageId::parse(raw).map_err(|_| ActivityStoreError::MalformedIdentity(field))
}

fn optional_thread_root(raw: Option<String>) -> Result<Option<ThreadRootId>, ActivityStoreError> {
    raw.map(|value| {
        ThreadRootId::parse(value)
            .map_err(|_| ActivityStoreError::MalformedIdentity("messages.thread_root_id"))
    })
    .transpose()
}
