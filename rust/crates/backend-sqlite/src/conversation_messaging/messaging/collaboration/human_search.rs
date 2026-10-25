//! One-snapshot SQLite producer for the public human collaboration search.
//!
//! `search.*` diagnostics are elapsed business stages, not SQL-only timers:
//! resolve includes participation checks; identities includes ACL/matching/counting;
//! messages includes author reads and projection. Never sum nested business and
//! statement phases as request latency. `db.statement:search.*` measures only
//! the named query/fetch on the already-acquired transaction and uses the existing
//! bounded statement histogram. No input or result fields are attached to spans.
//! Skipped branches emit no SQL spans. Dropping a request closes its entered phases
//! with elapsed time but does not fabricate a successful statement histogram sample;
//! existing HTTP diagnostics classify cancellation. SQLx retains transaction rollback.

use silverhand_backend_core::{
    conversation_messaging::messaging::{
        FromKind, MessageAuthor, MessageId, MessageSeq, PrincipalIdentity, ThreadRootId,
        collaboration::{
            conversation_message_window::{
                MessageWindowCorruption, SelectedWindowMessage, WindowMessageMetadata,
                WindowMessageProjection,
            },
            conversation_ref::ConversationRosterPolicy,
            human_search::{
                HumanSearchConversationPlacementKind, HumanSearchConversationRef,
                HumanSearchCorruption, HumanSearchMessageCountFact, HumanSearchOrder,
                HumanSearchPage, HumanSearchQuery, HumanSearchReadPort, HumanSearchRefusal,
                HumanSearchStream, HumanSearchWorkspaceRef, IdentitySearchGroups,
                IdentitySearchStatus, ResolvedHumanSearchQuery, SearchIdentity,
                SelectedHumanSearchConversation, SelectedHumanSearchMessage,
                SelectedHumanSearchThreadAnchor, SelectedHumanSearchWorkspace,
            },
        },
    },
    foundation::identity::{AgentId, ConversationId, UserId, WorkspaceId},
    foundation::persistence::PersistenceFailure,
};
use silverhand_backend_observability::Observe;
use sqlx::{
    Row, SqlSafeStr, Sqlite, Transaction,
    sqlite::{SqliteArguments, SqliteRow},
};
use thiserror::Error;

use crate::conversation_messaging::messaging::{
    MessagingStore,
    author::{AuthorEvidence, resolve_authors},
};
use crate::foundation::statement_metrics::statement;

use super::{
    conversation_message_window::{author_fact, decode_attachments},
    conversation_participation::{
        ConversationParticipationStoreError, SELECT_CONVERSATION, decode_conversation,
        human_participation_source, participating_viewer,
    },
};

const SELECT_WORKSPACES: &str = "WITH p(viewer,needle,workspace,has_workspace,from_mode,\
     from_value,to_mode,to_value) AS (SELECT ?,?,?,?,?,?,?,?) SELECT wu.user_id AS \
     source_viewer,w.id AS workspace_id,w.name AS workspace_name,w.owner_agent_id AS \
     workspace_owner_agent_id,w.description,w.archived_at,w.created_at,w.updated_at FROM p JOIN \
     workspaces w JOIN workspace_users wu ON wu.workspace_id=w.id AND wu.user_id=p.viewer WHERE \
     (w.name LIKE p.needle ESCAPE '\\' OR w.description LIKE p.needle ESCAPE '\\') AND \
     (p.has_workspace=0 OR w.id=p.workspace) AND (p.from_mode=0 OR \
     (p.from_mode=1 AND w.created_at>=p.from_value)) AND (p.to_mode!=1 OR \
     w.created_at<=p.to_value) ORDER BY w.updated_at DESC,w.id ASC LIMIT ?";

const SELECT_CONVERSATIONS: &str = concat!(
    "WITH p(viewer,needle,workspace,has_workspace,from_mode,from_value,to_mode,to_value) AS \
     (SELECT ?,?,?,?,?,?,?,?), candidates AS (SELECT ",
    human_participation_source!("c", "p.viewer"),
    " AS source_viewer,c.id AS conversation_id,c.placement_kind,c.roster_policy,c.title,\
     c.workspace_id AS conversation_workspace_id,c.closed_at,c.archived_at,c.created_at AS \
     conversation_created_at,c.updated_at,w.id AS joined_workspace_id,w.name AS workspace_name,\
     w.owner_agent_id AS workspace_owner_agent_id FROM p JOIN conversations c LEFT \
     JOIN workspaces w ON w.id=c.workspace_id WHERE c.title LIKE p.needle ESCAPE '\\' AND \
     (p.has_workspace=0 OR c.workspace_id=p.workspace) AND (p.from_mode=0 OR \
     (p.from_mode=1 AND c.created_at>=p.from_value)) AND (p.to_mode!=1 OR \
     c.created_at<=p.to_value)) SELECT * FROM candidates WHERE source_viewer IS NOT NULL ORDER BY \
     updated_at DESC,conversation_id ASC LIMIT ?"
);

macro_rules! message_candidates {
    () => {
        concat!(
            "WITH p(viewer,needle,has_text,workspace,has_workspace,conversation,has_conversation,\
             stream,thread_root,sender_kind,sender_id,has_sender,from_mode,from_value,to_mode,\
             to_value) AS (SELECT ?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?), candidates AS \
             (SELECT ",
            human_participation_source!("c", "p.viewer"),
            " AS source_viewer,m.id AS message_id,m.conversation_id AS \
             message_conversation_id,m.from_kind,m.from_agent_id,m.from_user_id,m.author_id,\
             m.client_message_id,m.author_name,m.author_display_name,m.content,m.metadata,m.seq,m.created_at AS \
             message_created_at,m.thread_root_id,c.id AS conversation_id,c.placement_kind,\
             c.roster_policy,c.title,c.workspace_id AS conversation_workspace_id,c.closed_at,\
             c.created_at AS conversation_created_at,w.id AS joined_workspace_id,w.name AS \
             workspace_name,w.owner_agent_id AS workspace_owner_agent_id FROM p JOIN \
             messages m JOIN conversations c ON c.id=m.conversation_id LEFT JOIN workspaces w ON \
             w.id=c.workspace_id WHERE (p.has_text=0 OR m.content LIKE p.needle ESCAPE '\\') AND \
             (p.has_workspace=0 OR c.workspace_id=p.workspace) AND (p.has_conversation=0 OR \
             c.id=p.conversation) AND (p.stream='all' OR (p.stream='mainline' AND \
             m.thread_root_id IS NULL) OR (p.stream='thread' AND m.thread_root_id=p.thread_root)) \
             AND (p.has_sender=0 OR (m.from_kind=p.sender_kind AND m.author_id=p.sender_id)) AND \
             (p.from_mode=0 OR (p.from_mode=1 AND m.created_at>=p.from_value)) AND \
             (p.to_mode!=1 OR m.created_at<=p.to_value)) "
        )
    };
}

const COUNT_MESSAGES: &str = concat!(
    message_candidates!(),
    "SELECT COUNT(*) AS total,MIN(source_viewer) AS min_source_viewer,MAX(source_viewer) AS \
     max_source_viewer FROM candidates WHERE source_viewer IS NOT NULL"
);
const SELECT_MESSAGES_RECENCY: &str = concat!(
    message_candidates!(),
    "SELECT * FROM candidates WHERE source_viewer IS NOT NULL ORDER BY message_created_at DESC,\
     seq DESC,conversation_id ASC,message_id ASC LIMIT ?"
);
const SELECT_MESSAGES_SEQUENCE: &str = concat!(
    message_candidates!(),
    "SELECT * FROM candidates WHERE source_viewer IS NOT NULL ORDER BY seq ASC LIMIT ?"
);
const SELECT_THREAD_ANCHOR: &str =
    "SELECT id,conversation_id,thread_root_id FROM messages WHERE id=?";

#[derive(Clone)]
pub struct SqliteHumanSearchRead {
    pool: crate::foundation::database::SqliteDatabase,
}

impl<E> MessagingStore<E> {
    pub fn human_search(&self) -> SqliteHumanSearchRead {
        SqliteHumanSearchRead::from_pool(self.pool.clone())
    }
}

#[derive(Debug, Error)]
pub enum HumanSearchStoreError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("conversation participation snapshot is malformed")]
    Participation(#[from] ConversationParticipationStoreError),
    #[error("stored human-search field {field} is malformed")]
    Field { field: &'static str },
    #[error("stored human-search author projection failed")]
    Author(#[source] PersistenceFailure),
    #[error("stored human-search identity candidates failed")]
    IdentityCandidates(#[source] PersistenceFailure),
    #[error("stored human-search message metadata is malformed")]
    Metadata,
    #[error("stored message projection is not closed")]
    MessageProjection(#[source] MessageWindowCorruption),
    #[error("stored human-search projection is not closed")]
    Projection(#[source] HumanSearchCorruption),
}

impl From<HumanSearchStoreError> for PersistenceFailure {
    fn from(value: HumanSearchStoreError) -> Self {
        Self::from_source(value)
    }
}

impl HumanSearchReadPort for SqliteHumanSearchRead {
    type Error = PersistenceFailure;

    async fn search(
        &self,
        viewer: &UserId,
        query: HumanSearchQuery,
    ) -> Result<Result<HumanSearchPage, HumanSearchRefusal>, Self::Error> {
        self.search_inner(viewer, query)
            .await
            .map_err(PersistenceFailure::from)
    }
}

impl SqliteHumanSearchRead {
    pub fn from_pool(pool: impl Into<crate::foundation::database::SqliteDatabase>) -> Self {
        let pool = pool.into();
        Self { pool }
    }

    async fn search_inner(
        &self,
        viewer: &UserId,
        requested: HumanSearchQuery,
    ) -> Result<Result<HumanSearchPage, HumanSearchRefusal>, HumanSearchStoreError> {
        // Acquire separately: search business pool wait must not include authentication
        // or be charged to BEGIN/SQL. SQLx still owns rollback on cancellation.
        let connection = self
            .pool
            .acquire()
            .observe(silverhand_backend_observability::operation_span!(
                "search.pool.acquire"
            ))
            .await?;
        let mut tx = Transaction::begin(connection, Some("BEGIN DEFERRED".into_sql_str()))
            .observe(silverhand_backend_observability::operation_span!(
                "search.transaction.begin"
            ))
            .await?;
        let resolved = match resolve_query(&mut tx, viewer, requested).await? {
            Ok(resolution) => resolution,
            Err(refusal) => {
                statement("search.commit", "commit", tx.commit()).await?;
                return Ok(Err(refusal));
            }
        };
        let effective = resolved.query.effective().clone();
        let identities = if effective.include_identities() {
            Some(load_identities(&mut tx, viewer, &effective).await?)
        } else {
            None
        };
        let active = effective.has_search_intent() && !resolved.unavailable;
        let (workspaces, conversations, messages, count) = if active {
            let workspaces = if effective.includes_named_entities() {
                load_workspaces(&mut tx, viewer, &effective).await?
            } else {
                Vec::new()
            };
            let conversations = if effective.includes_named_entities() {
                load_conversations(&mut tx, viewer, &effective).await?
            } else {
                Vec::new()
            };
            let (count_viewer, total) = count_messages(&mut tx, viewer, &effective).await?;
            let messages = load_messages(&mut tx, viewer, &effective).await?;
            let count = HumanSearchMessageCountFact::counted(count_viewer, effective, total)
                .map_err(HumanSearchStoreError::Projection)?;
            (workspaces, conversations, messages, count)
        } else {
            (
                Vec::new(),
                Vec::new(),
                Vec::new(),
                HumanSearchMessageCountFact::skipped(),
            )
        };
        let page = {
            let span = silverhand_backend_observability::operation_span!("search.assemble");
            span.in_scope(|| {
                let observation = silverhand_backend_observability::Operation::current();
                let result = (|| {
                    let page = HumanSearchPage::sealed(
                        viewer.clone(),
                        resolved.query,
                        workspaces,
                        conversations,
                        messages,
                        count,
                    )
                    .map_err(HumanSearchStoreError::Projection)?;
                    match identities {
                        Some(identities) => page
                            .with_identities(identities)
                            .map_err(HumanSearchStoreError::Projection),
                        None => Ok(page),
                    }
                })();
                observation.finish(&result);
                result
            })?
        };
        statement("search.commit", "commit", tx.commit()).await?;
        Ok(Ok(page))
    }
}

#[tracing::instrument(
    target = "silverhand::diagnostics",
    name = "search.resolve",
    skip_all,
    fields(
        elapsed_ms = tracing::field::Empty,
        outcome = tracing::field::Empty,
        operation_finished = tracing::field::Empty,
    )
)]
async fn resolve_query(
    tx: &mut Transaction<'_, Sqlite>,
    viewer: &UserId,
    requested: HumanSearchQuery,
) -> Result<Result<SqliteResolvedQuery, HumanSearchRefusal>, HumanSearchStoreError> {
    let observation = silverhand_backend_observability::Operation::current();
    let result = async {
        if !matches!(requested.stream(), HumanSearchStream::Thread) {
            return ResolvedHumanSearchQuery::direct(requested)
                .map(|query| {
                    Ok(SqliteResolvedQuery {
                        query,
                        unavailable: false,
                    })
                })
                .map_err(HumanSearchStoreError::Projection);
        }
        let conversation =
            requested
                .conversation()
                .cloned()
                .ok_or(HumanSearchStoreError::Projection(
                    HumanSearchCorruption::ThreadResolution,
                ))?;
        let Some(conversation_row) = sqlx::query(SELECT_CONVERSATION)
            .bind(conversation.as_str())
            .fetch_optional(&mut **tx)
            .await?
        else {
            return ResolvedHumanSearchQuery::thread_unavailable(viewer.clone(), requested)
                .map(|query| {
                    Ok(SqliteResolvedQuery {
                        query,
                        unavailable: true,
                    })
                })
                .map_err(HumanSearchStoreError::Projection);
        };
        let (stored, placement) = decode_conversation(&conversation_row, &conversation)?;
        if participating_viewer(tx, viewer, &stored, &placement)
            .await?
            .is_none()
        {
            return ResolvedHumanSearchQuery::thread_unavailable(viewer.clone(), requested)
                .map(|query| {
                    Ok(SqliteResolvedQuery {
                        query,
                        unavailable: true,
                    })
                })
                .map_err(HumanSearchStoreError::Projection);
        }
        let requested_anchor = requested
            .thread_root()
            .ok_or(HumanSearchStoreError::Projection(
                HumanSearchCorruption::ThreadResolution,
            ))?;
        let Some(anchor_row) = sqlx::query(SELECT_THREAD_ANCHOR)
            .bind(requested_anchor.as_str())
            .fetch_optional(&mut **tx)
            .await?
        else {
            return Ok(Err(HumanSearchRefusal::ThreadRootNotFound));
        };
        let anchor = message_id(&anchor_row, "id")?;
        let anchor_conversation = conversation_id(&anchor_row, "conversation_id")?;
        let stored_root = optional_root(&anchor_row, "thread_root_id")?;
        if anchor_conversation != stored {
            return Ok(Err(HumanSearchRefusal::ThreadRootNotFound));
        }
        let loaded_root = match stored_root.as_ref() {
            None => None,
            Some(root) => {
                let Some(row) = sqlx::query(SELECT_THREAD_ANCHOR)
                    .bind(root.as_str())
                    .fetch_optional(&mut **tx)
                    .await?
                else {
                    return Ok(Err(HumanSearchRefusal::ThreadRootNotFound));
                };
                Some((
                    message_id(&row, "id")?,
                    conversation_id(&row, "conversation_id")?,
                    optional_root(&row, "thread_root_id")?,
                ))
            }
        };
        let selected = match SelectedHumanSearchThreadAnchor::from_rows(
            anchor,
            anchor_conversation,
            stored_root,
            loaded_root,
        ) {
            Ok(selected) => selected,
            Err(refusal) => return Ok(Err(refusal)),
        };
        ResolvedHumanSearchQuery::thread_resolved(viewer.clone(), requested, selected)
            .map(|query| {
                Ok(SqliteResolvedQuery {
                    query,
                    unavailable: false,
                })
            })
            .map_err(HumanSearchStoreError::Projection)
    }
    .await;
    observation.finish(&result);
    result
}

struct SqliteResolvedQuery {
    query: ResolvedHumanSearchQuery,
    unavailable: bool,
}

#[tracing::instrument(
    target = "silverhand::diagnostics",
    name = "search.workspaces",
    skip_all,
    fields(
        elapsed_ms = tracing::field::Empty,
        outcome = tracing::field::Empty,
        operation_finished = tracing::field::Empty,
    )
)]
async fn load_workspaces(
    tx: &mut Transaction<'_, Sqlite>,
    viewer: &UserId,
    query: &HumanSearchQuery,
) -> Result<Vec<SelectedHumanSearchWorkspace>, HumanSearchStoreError> {
    let observation = silverhand_backend_observability::Operation::current();
    let result = async {
        let (from_mode, from_value) = millis(query.from());
        let (to_mode, to_value) = millis(query.to());
        let rows = statement(
            "search.workspaces",
            "select",
            sqlx::query(SELECT_WORKSPACES)
                .bind(viewer.as_str())
                .bind(like_needle(query.text().as_str()))
                .bind(query.workspace().map_or("", WorkspaceId::as_str))
                .bind(i64::from(query.workspace().is_some()))
                .bind(from_mode)
                .bind(from_value)
                .bind(to_mode)
                .bind(to_value)
                .bind(i64::from(query.limit().min(20)))
                .fetch_all(&mut **tx),
        )
        .await?;
        rows.iter().map(decode_workspace_result).collect()
    }
    .await;
    observation.finish(&result);
    result
}

#[tracing::instrument(
    target = "silverhand::diagnostics",
    name = "search.channels",
    skip_all,
    fields(
        elapsed_ms = tracing::field::Empty,
        outcome = tracing::field::Empty,
        operation_finished = tracing::field::Empty,
    )
)]
async fn load_conversations(
    tx: &mut Transaction<'_, Sqlite>,
    viewer: &UserId,
    query: &HumanSearchQuery,
) -> Result<Vec<SelectedHumanSearchConversation>, HumanSearchStoreError> {
    let observation = silverhand_backend_observability::Operation::current();
    let result = async {
        let (from_mode, from_value) = millis(query.from());
        let (to_mode, to_value) = millis(query.to());
        let rows = statement(
            "search.channels",
            "select",
            sqlx::query(SELECT_CONVERSATIONS)
                .bind(viewer.as_str())
                .bind(like_needle(query.text().as_str()))
                .bind(query.workspace().map_or("", WorkspaceId::as_str))
                .bind(i64::from(query.workspace().is_some()))
                .bind(from_mode)
                .bind(from_value)
                .bind(to_mode)
                .bind(to_value)
                .bind(i64::from(query.limit().min(30)))
                .fetch_all(&mut **tx),
        )
        .await?;
        rows.iter().map(decode_conversation_result).collect()
    }
    .await;
    observation.finish(&result);
    result
}

#[tracing::instrument(
    target = "silverhand::diagnostics",
    name = "search.message_count",
    skip_all,
    fields(
        elapsed_ms = tracing::field::Empty,
        outcome = tracing::field::Empty,
        operation_finished = tracing::field::Empty,
    )
)]
async fn count_messages(
    tx: &mut Transaction<'_, Sqlite>,
    viewer: &UserId,
    query: &HumanSearchQuery,
) -> Result<(UserId, i64), HumanSearchStoreError> {
    let observation = silverhand_backend_observability::Operation::current();
    let result = async {
        let row = statement(
            "search.message_count",
            "select",
            bind_message_query(sqlx::query(COUNT_MESSAGES), viewer, query, false)
                .fetch_one(&mut **tx),
        )
        .await?;
        let total: i64 = row.try_get("total")?;
        let minimum: Option<String> = row.try_get("min_source_viewer")?;
        let maximum: Option<String> = row.try_get("max_source_viewer")?;
        let source = match (minimum, maximum) {
            (None, None) if total == 0 => viewer.clone(),
            (Some(minimum), Some(maximum)) if minimum == maximum => {
                UserId::parse(minimum).map_err(|_| field("message_count.source_viewer"))?
            }
            _ => return Err(field("message_count.source_viewer")),
        };
        Ok((source, total))
    }
    .await;
    observation.finish(&result);
    result
}

#[tracing::instrument(
    target = "silverhand::diagnostics",
    name = "search.messages",
    skip_all,
    fields(
        elapsed_ms = tracing::field::Empty,
        outcome = tracing::field::Empty,
        operation_finished = tracing::field::Empty,
    )
)]
async fn load_messages(
    tx: &mut Transaction<'_, Sqlite>,
    viewer: &UserId,
    query: &HumanSearchQuery,
) -> Result<Vec<SelectedHumanSearchMessage>, HumanSearchStoreError> {
    let observation = silverhand_backend_observability::Operation::current();
    let result = async {
        let sql = match query.order() {
            HumanSearchOrder::RecencyDesc => SELECT_MESSAGES_RECENCY,
            HumanSearchOrder::SequenceAsc => SELECT_MESSAGES_SEQUENCE,
        };
        let rows = statement(
            "search.messages",
            "select",
            bind_message_query(sqlx::query(sql), viewer, query, true).fetch_all(&mut **tx),
        )
        .await?;
        // Decode every selected row before the set-shaped live-author lookup.
        let raw = rows
            .iter()
            .map(RawSearchMessage::decode)
            .collect::<Result<Vec<_>, _>>()?;
        let evidence = raw.iter().map(|row| row.author.clone()).collect::<Vec<_>>();
        let authors = resolve_authors(tx, &evidence)
            .await
            .map_err(HumanSearchStoreError::Author)?;
        raw.into_iter()
            .zip(authors)
            .map(|(row, author)| row.seal(author))
            .collect()
    }
    .await;
    observation.finish(&result);
    result
}

fn bind_message_query<'q>(
    query_builder: sqlx::query::Query<'q, Sqlite, SqliteArguments>,
    viewer: &'q UserId,
    query: &'q HumanSearchQuery,
    include_limit: bool,
) -> sqlx::query::Query<'q, Sqlite, SqliteArguments> {
    let (from_mode, from_value) = millis(query.from());
    let (to_mode, to_value) = millis(query.to());
    let (sender_kind, sender_id) = sender(query.sender());
    let query_builder = query_builder
        .bind(viewer.as_str())
        .bind(like_needle(query.text().as_str()))
        .bind(i64::from(!query.text().is_empty()))
        .bind(query.workspace().map_or("", WorkspaceId::as_str))
        .bind(i64::from(query.workspace().is_some()))
        .bind(query.conversation().map_or("", ConversationId::as_str))
        .bind(i64::from(query.conversation().is_some()))
        .bind(query.stream().as_str())
        .bind(query.thread_root().map_or("", MessageId::as_str))
        .bind(sender_kind)
        .bind(sender_id)
        .bind(i64::from(query.sender().is_some()))
        .bind(from_mode)
        .bind(from_value)
        .bind(to_mode)
        .bind(to_value);
    if include_limit {
        query_builder.bind(i64::from(query.limit()))
    } else {
        query_builder
    }
}

#[derive(Clone)]
struct RawSearchMessage {
    client_message_id: Option<String>,
    source_viewer: UserId,
    conversation: HumanSearchConversationRef,
    id: MessageId,
    message_conversation: ConversationId,
    author: AuthorEvidence,
    content: String,
    metadata: Option<String>,
    seq: MessageSeq,
    created_at: i64,
    thread_root: Option<ThreadRootId>,
}

impl RawSearchMessage {
    fn decode(row: &SqliteRow) -> Result<Self, HumanSearchStoreError> {
        Ok(Self {
            client_message_id: row.try_get("client_message_id")?,
            source_viewer: user_id(row, "source_viewer")?,
            conversation: decode_conversation_ref(row)?,
            id: message_id(row, "message_id")?,
            message_conversation: conversation_id(row, "message_conversation_id")?,
            author: decode_author(row)?,
            content: row.try_get("content")?,
            metadata: row.try_get("metadata")?,
            seq: MessageSeq::try_from(row.try_get::<i64, _>("seq")?)
                .map_err(|_| field("messages.seq"))?,
            created_at: row.try_get("message_created_at")?,
            thread_root: optional_root(row, "thread_root_id")?,
        })
    }

    fn seal(
        self,
        resolved: Option<MessageAuthor>,
    ) -> Result<SelectedHumanSearchMessage, HumanSearchStoreError> {
        let attachments = decode_attachments(self.metadata.as_deref())
            .map_err(|_| HumanSearchStoreError::Metadata)?;
        let author = author_fact(&self.author, resolved).map_err(|error| {
            HumanSearchStoreError::Author(PersistenceFailure::from_source(error))
        })?;
        let selected = SelectedWindowMessage::seal(WindowMessageProjection {
            client_message_id: self.client_message_id,
            id: self.id,
            conversation: self.message_conversation,
            author,
            content: self.content,
            metadata: WindowMessageMetadata::decoded(self.metadata, attachments),
            seq: self.seq,
            created_at_ms: self.created_at,
            thread_root: self.thread_root,
            linked_task: None,
            reactions: Vec::new(),
        })
        .map_err(HumanSearchStoreError::MessageProjection)?;
        SelectedHumanSearchMessage::projected(self.source_viewer, self.conversation, selected)
            .map_err(HumanSearchStoreError::Projection)
    }
}

fn decode_workspace_result(
    row: &SqliteRow,
) -> Result<SelectedHumanSearchWorkspace, HumanSearchStoreError> {
    let viewer = user_id(row, "source_viewer")?;
    let workspace =
        decode_workspace_ref(row, "workspace_id")?.ok_or_else(|| field("workspaces.id"))?;
    SelectedHumanSearchWorkspace::projected(
        viewer,
        workspace,
        row.try_get("description")?,
        row.try_get("archived_at")?,
        row.try_get("created_at")?,
        row.try_get("updated_at")?,
    )
    .map_err(HumanSearchStoreError::Projection)
}

fn decode_conversation_result(
    row: &SqliteRow,
) -> Result<SelectedHumanSearchConversation, HumanSearchStoreError> {
    SelectedHumanSearchConversation::projected(
        user_id(row, "source_viewer")?,
        decode_conversation_ref(row)?,
        row.try_get("updated_at")?,
        row.try_get("archived_at")?,
    )
    .map_err(HumanSearchStoreError::Projection)
}

fn decode_conversation_ref(
    row: &SqliteRow,
) -> Result<HumanSearchConversationRef, HumanSearchStoreError> {
    let kind = match row.try_get::<String, _>("placement_kind")?.as_str() {
        "workspace" => HumanSearchConversationPlacementKind::Workspace,
        "direct_pair" => HumanSearchConversationPlacementKind::DirectPair,
        _ => return Err(field("conversations.placement_kind")),
    };
    let roster = ConversationRosterPolicy::parse(&row.try_get::<String, _>("roster_policy")?)
        .map_err(|_| field("conversations.roster_policy"))?;
    let stored_workspace = row
        .try_get::<Option<String>, _>("conversation_workspace_id")?
        .map(WorkspaceId::parse)
        .transpose()
        .map_err(|_| field("conversations.workspace_id"))?;
    HumanSearchConversationRef::from_selected_row(
        conversation_id(row, "conversation_id")?,
        kind,
        roster,
        row.try_get("title")?,
        stored_workspace,
        decode_workspace_ref(row, "joined_workspace_id")?,
        row.try_get("closed_at")?,
        row.try_get("conversation_created_at")?,
    )
    .map_err(HumanSearchStoreError::Projection)
}

fn decode_workspace_ref(
    row: &SqliteRow,
    id_field: &'static str,
) -> Result<Option<HumanSearchWorkspaceRef>, HumanSearchStoreError> {
    let Some(raw_id) = row.try_get::<Option<String>, _>(id_field)? else {
        return Ok(None);
    };
    let id = WorkspaceId::parse(raw_id).map_err(|_| field(id_field))?;
    let owner = row
        .try_get::<Option<String>, _>("workspace_owner_agent_id")?
        .map(AgentId::parse)
        .transpose()
        .map_err(|_| field("workspaces.owner_agent_id"))?;
    HumanSearchWorkspaceRef::projected(id, row.try_get("workspace_name")?, owner)
        .map(Some)
        .map_err(HumanSearchStoreError::Projection)
}

fn decode_author(row: &SqliteRow) -> Result<AuthorEvidence, HumanSearchStoreError> {
    let from_kind = match row.try_get::<String, _>("from_kind")?.as_str() {
        "agent" => FromKind::Agent,
        "user" => FromKind::User,
        "system" => FromKind::System,
        _ => return Err(field("messages.from_kind")),
    };
    Ok(AuthorEvidence {
        from_kind,
        from_agent_id: row.try_get("from_agent_id")?,
        from_user_id: row.try_get("from_user_id")?,
        author_id: row.try_get("author_id")?,
        author_name: row.try_get("author_name")?,
        author_display_name: row.try_get("author_display_name")?,
    })
}

fn sender(sender: Option<&PrincipalIdentity>) -> (&str, &str) {
    match sender {
        None => ("", ""),
        Some(PrincipalIdentity::Human(id)) => ("user", id.as_str()),
        Some(PrincipalIdentity::Agent(id)) => ("agent", id.as_str()),
    }
}

fn millis(
    value: Option<
        silverhand_backend_core::conversation_messaging::messaging::collaboration::human_search::HumanSearchMillis,
    >,
) -> (i64, i64) {
    match value {
        None => (0, 0),
        Some(value) if value.is_beyond_persisted_range() => (2, 0),
        Some(value) => (1, value.persisted_value().unwrap_or_default()),
    }
}

fn like_needle(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('%');
    for character in value.chars() {
        if matches!(character, '\\' | '%' | '_') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped.push('%');
    escaped
}

fn user_id(row: &SqliteRow, name: &'static str) -> Result<UserId, HumanSearchStoreError> {
    UserId::parse(row.try_get::<String, _>(name)?).map_err(|_| field(name))
}

fn conversation_id(
    row: &SqliteRow,
    name: &'static str,
) -> Result<ConversationId, HumanSearchStoreError> {
    ConversationId::parse(row.try_get::<String, _>(name)?).map_err(|_| field(name))
}

fn message_id(row: &SqliteRow, name: &'static str) -> Result<MessageId, HumanSearchStoreError> {
    MessageId::parse(row.try_get::<String, _>(name)?).map_err(|_| field(name))
}

fn optional_root(
    row: &SqliteRow,
    name: &'static str,
) -> Result<Option<ThreadRootId>, HumanSearchStoreError> {
    row.try_get::<Option<String>, _>(name)?
        .map(ThreadRootId::parse)
        .transpose()
        .map_err(|_| field(name))
}

fn field(name: &'static str) -> HumanSearchStoreError {
    HumanSearchStoreError::Field { field: name }
}

#[tracing::instrument(
    target = "silverhand::diagnostics",
    name = "search.identities",
    skip_all,
    fields(
        elapsed_ms = tracing::field::Empty,
        outcome = tracing::field::Empty,
        operation_finished = tracing::field::Empty,
    )
)]
async fn load_identities(
    tx: &mut Transaction<'_, Sqlite>,
    viewer: &UserId,
    query: &HumanSearchQuery,
) -> Result<IdentitySearchGroups, HumanSearchStoreError> {
    let observation = silverhand_backend_observability::Operation::current();
    let result = async {
        use silverhand_backend_core::{
            conversation_messaging::messaging::AvatarRevision,
            identity_access::{
                agent_list_read::{ListScope, visible_agents},
                capability::agent_view::VisibleStages,
            },
        };
        let mut candidates = Vec::new();
        if query.identity_search_status() == IdentitySearchStatus::Searched {
            let rows = sqlx::query(
            "SELECT id,name,display_name,avatar_storage_key,updated_at FROM users WHERE id != ?",
        )
        .bind(viewer.as_str())
        .fetch_all(&mut **tx)
        .await?;
            for row in rows {
                let avatar_key: Option<String> = row.try_get("avatar_storage_key")?;
                candidates.push(SearchIdentity {
                    identity: PrincipalIdentity::Human(user_id(&row, "id")?),
                    name: row.try_get("name")?,
                    display_name: row.try_get("display_name")?,
                    avatar: avatar_key
                        .map(|_| row.try_get::<i64, _>("updated_at").map(AvatarRevision::at))
                        .transpose()?,
                    runtime: None,
                });
            }

            let facts =
                crate::identity_access::agent_list_read::read_agent_candidates_in_tx(tx, viewer)
                    .await
                    .map_err(|error| {
                        HumanSearchStoreError::IdentityCandidates(PersistenceFailure::from_source(
                            error,
                        ))
                    })?;
            let visible = visible_agents(
                viewer,
                &facts,
                &VisibleStages::active_only(),
                ListScope::All,
            )
            .map_err(|_| HumanSearchStoreError::Projection(HumanSearchCorruption::Identity))?;
            for agent in visible {
                let projection = agent.projection();
                if projection.session_scope != "agent" {
                    continue;
                }
                candidates.push(SearchIdentity {
                    identity: PrincipalIdentity::Agent(agent.source().clone()),
                    name: projection.name.clone(),
                    display_name: projection.display_name.clone(),
                    avatar: projection
                        .avatar_storage_key
                        .as_ref()
                        .map(|_| AvatarRevision::at(projection.updated_at)),
                    runtime: Some(projection.runtime),
                });
            }
        }
        IdentitySearchGroups::from_visible(viewer.clone(), query.clone(), candidates)
            .map_err(HumanSearchStoreError::Projection)
    }
    .await;
    observation.finish(&result);
    result
}
