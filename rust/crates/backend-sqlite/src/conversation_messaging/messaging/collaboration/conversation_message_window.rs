//! One-snapshot SQLite projection for a conversation message window.

use std::collections::{HashMap, HashSet};

use silverhand_backend_core::{
    conversation_messaging::messaging::{
        AttachmentId, FromKind, MessageAuthor, MessageId, MessageSeq, PrincipalIdentity,
        ThreadRootId,
        attachments::{AttachmentLabel, AttachmentLabelKind, AttachmentSnapshot},
        collaboration::{
            channel_directory::{ChannelVisibility, DirectoryRoster},
            channel_lifecycle::ChannelLifecyclePlacement,
            conversation_message_window::{
                ConversationMessageWindow, ConversationMessageWindowReadPort,
                ConversationMessageWindowRequest, MessageWindowAnchor, MessageWindowAudience,
                MessageWindowAudienceFact, MessageWindowCorruption, MessageWindowHead,
                MessageWindowRefusal, MessageWindowStreamRequest, ReplyBadgeProjection,
                ResolvedMessageWindowStream, SelectedWindowMessage, WindowAuthorFact,
                WindowBoundaryDirection, WindowBoundaryFact, WindowCount, WindowLinkedTask,
                WindowMessageMetadata, WindowMessageProjection, WindowReaction,
                WindowReactionViewer, WindowReplier, WindowReplyPreview, WindowRootReplies,
                WindowTaskAssignee,
            },
            reaction::Reactor,
        },
        task::{TaskId, TaskNumber, TaskStatus, TaskTitle},
    },
    foundation::identity::{AgentId, ConversationId, UserId},
    foundation::persistence::PersistenceFailure,
};
use sqlx::{QueryBuilder, Row, Sqlite, Transaction, sqlite::SqliteRow};
use thiserror::Error;

use crate::conversation_messaging::messaging::{
    MessagingStore,
    author::{AuthorEvidence, resolve_authors},
};

use super::{
    channel_lifecycle::{ChannelLifecycleStoreError, load_channel_row, load_workspace_role_fact},
    reaction::aggregate_facts_in_snapshot,
};

// `pub(crate)`: the message column list and its linked-task joins are reused verbatim
// by the agent-audit read adapter, which selects rows in `mode=all` (no mainline/thread
// filter) but decodes them through the same [`RawMessage::decode`].
pub(crate) const MESSAGE_COLUMNS: &str = "m.client_message_id,m.id AS message_id,m.conversation_id AS message_conversation_id,\
     m.from_kind,m.from_agent_id,m.from_user_id,m.author_id,m.author_name,\
     m.author_display_name,m.content,m.metadata,m.seq,m.created_at,m.thread_root_id,\
     lt.id AS task_id,lt.conversation_id AS task_conversation_id,lt.task_number,\
     lt.title AS task_title,lt.status AS task_status,\
     lt.assignee_agent_id,lt.assignee_user_id,lta.display_name AS assignee_agent_name,\
     ltu.display_name AS assignee_user_name";
pub(crate) const MESSAGE_JOINS: &str = " LEFT JOIN tasks lt ON lt.source_message_id=m.id \
     LEFT JOIN agents lta ON lta.id=lt.assignee_agent_id \
     LEFT JOIN users ltu ON ltu.id=lt.assignee_user_id ";

#[derive(Clone)]
pub struct SqliteConversationMessageWindowRead {
    pool: crate::foundation::database::SqliteDatabase,
}

impl<E> MessagingStore<E> {
    pub fn conversation_message_window(&self) -> SqliteConversationMessageWindowRead {
        SqliteConversationMessageWindowRead {
            pool: self.pool.clone(),
        }
    }
}

#[derive(Debug, Error)]
pub enum ConversationMessageWindowStoreError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("channel access snapshot is malformed")]
    Access(#[source] PersistenceFailure),
    #[error("stored message-window identity field {field} is malformed")]
    Identity { field: &'static str },
    #[error("stored message-window author kind is outside the closed set")]
    AuthorKind,
    #[error("stored message-window metadata is malformed")]
    Metadata,
    #[error("stored linked-task projection field {field} is malformed")]
    LinkedTask { field: &'static str },
    #[error("stored message-window author projection failed")]
    Author(#[source] PersistenceFailure),
    #[error("stored message reaction projection failed")]
    Reaction(#[source] PersistenceFailure),
    #[error("stored message-window row coordinates are inconsistent")]
    Coordinate,
    #[error("stored message-window projection is not closed")]
    Projection(#[source] MessageWindowCorruption),
}

impl From<ConversationMessageWindowStoreError> for PersistenceFailure {
    fn from(value: ConversationMessageWindowStoreError) -> Self {
        Self::from_source(value)
    }
}

impl ConversationMessageWindowReadPort for SqliteConversationMessageWindowRead {
    type Error = PersistenceFailure;

    async fn read(
        &self,
        viewer: &UserId,
        conversation: &ConversationId,
        request: ConversationMessageWindowRequest,
    ) -> Result<Result<ConversationMessageWindow, MessageWindowRefusal>, Self::Error> {
        self.read_inner(viewer, conversation, request)
            .await
            .map_err(PersistenceFailure::from)
    }
}

impl SqliteConversationMessageWindowRead {
    #[tracing::instrument(
        target = "silverhand::diagnostics",
        name = "db.messages.read_inner",
        skip_all,
        fields(
            elapsed_ms = tracing::field::Empty,
            outcome = tracing::field::Empty,
            operation_finished = tracing::field::Empty,
        )
    )]
    async fn read_inner(
        &self,
        viewer: &UserId,
        requested: &ConversationId,
        request: ConversationMessageWindowRequest,
    ) -> Result<
        Result<ConversationMessageWindow, MessageWindowRefusal>,
        ConversationMessageWindowStoreError,
    > {
        let observation = silverhand_backend_observability::Operation::current();
        let result = async {
            let mut tx =
                crate::foundation::statement_metrics::begin(&self.pool, Some("BEGIN DEFERRED"))
                    .await?;
            let admitted = match admit_in_snapshot(&mut tx, viewer, requested).await? {
                Ok(fact) => fact,
                Err(refusal) => {
                    tx.commit().await?;
                    return Ok(Err(refusal));
                }
            };
            let stream = match resolve_stream(&mut tx, requested, request.stream()).await? {
                Some(stream) => stream,
                None => {
                    tx.commit().await?;
                    return Ok(Err(MessageWindowRefusal::ThreadRootNotFound));
                }
            };
            let head = load_stream_head(&mut tx, &stream).await?;
            let raw_messages = match select_window(&mut tx, &stream, &request).await? {
                Some(rows) => rows,
                None => {
                    tx.commit().await?;
                    return Ok(Err(MessageWindowRefusal::AnchorNotFound));
                }
            };

            let eligible_roots = raw_messages
                .iter()
                .filter(|row| row.thread_root.is_none() && row.author.from_kind != FromKind::System)
                .map(|row| {
                    ThreadRootId::parse(row.id.as_str()).map_err(|_| identity("messages.id"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let raw_replies = load_reply_sidecars(&mut tx, requested, &eligible_roots).await?;

            // Main rows, distinct-replier rows and preview rows share one author
            // batch. Every raw selected row is decoded before any product projection.
            let mut evidence = raw_messages
                .iter()
                .map(|row| row.author.clone())
                .collect::<Vec<_>>();
            evidence.extend(raw_replies.repliers.iter().map(|row| row.author.clone()));
            evidence.extend(raw_replies.previews.iter().map(|row| row.author.clone()));
            let resolved = resolve_authors(&mut tx, &evidence)
                .await
                .map_err(ConversationMessageWindowStoreError::Author)?;
            let message_len = raw_messages.len();
            let replier_len = raw_replies.repliers.len();
            let (message_authors, tail) = resolved.split_at(message_len);
            let (replier_authors, preview_authors) = tail.split_at(replier_len);

            let message_ids = raw_messages
                .iter()
                .map(|row| row.id.clone())
                .collect::<Vec<_>>();
            let reaction_viewer = matches!(admitted.kind, MessageWindowAudience::Participating)
                .then(|| Reactor::Human(admitted.viewer.clone()));
            let reaction_facts =
                aggregate_facts_in_snapshot(&mut tx, reaction_viewer.as_ref(), &message_ids)
                    .await
                    .map_err(|error| {
                        ConversationMessageWindowStoreError::Reaction(
                            PersistenceFailure::from_source(error),
                        )
                    })?;
            let selected_set = message_ids.iter().collect::<HashSet<_>>();
            let mut reactions = HashMap::<MessageId, Vec<WindowReaction>>::new();
            for fact in reaction_facts {
                if !selected_set.contains(&fact.message) {
                    return Err(ConversationMessageWindowStoreError::Coordinate);
                }
                let source = match admitted.kind {
                    MessageWindowAudience::Participating => {
                        WindowReactionViewer::Participating(admitted.viewer.clone())
                    }
                    MessageWindowAudience::PublicNonmember => WindowReactionViewer::Neutral,
                };
                let reaction = WindowReaction::projected(
                    fact.message.clone(),
                    source,
                    fact.aggregate,
                    fact.first_created_at,
                )
                .map_err(ConversationMessageWindowStoreError::Projection)?;
                reactions.entry(fact.message).or_default().push(reaction);
            }

            let mut messages = Vec::with_capacity(raw_messages.len());
            for (raw, author) in raw_messages
                .into_iter()
                .zip(message_authors.iter().cloned())
            {
                let row_reactions = reactions.remove(&raw.id).unwrap_or_default();
                messages.push(raw.seal(author, row_reactions)?);
            }
            if !reactions.is_empty() {
                return Err(ConversationMessageWindowStoreError::Coordinate);
            }

            let roots =
                raw_replies.seal(requested, &eligible_roots, replier_authors, preview_authors)?;
            let before = boundary_fact(
                &mut tx,
                &stream,
                messages.first(),
                WindowBoundaryDirection::Before,
            )
            .await?;
            let after = if matches!(request.query().anchor(), MessageWindowAnchor::Latest) {
                closed_false_boundary(&stream, messages.last(), WindowBoundaryDirection::After)?
            } else {
                boundary_fact(
                    &mut tx,
                    &stream,
                    messages.last(),
                    WindowBoundaryDirection::After,
                )
                .await?
            };
            let audience = MessageWindowAudienceFact::admitted(
                admitted.viewer,
                admitted.conversation,
                matches!(admitted.kind, MessageWindowAudience::Participating),
                matches!(admitted.kind, MessageWindowAudience::PublicNonmember),
            )
            .map_err(ConversationMessageWindowStoreError::Projection)?;
            let page = ConversationMessageWindow::sealed(
                audience, request, stream, head, messages, roots, before, after,
            )
            .map_err(ConversationMessageWindowStoreError::Projection)?;
            tx.commit().await?;
            Ok(Ok(page))
        }
        .await;
        observation.finish(&result);
        result
    }
}

struct AdmittedAudience {
    viewer: UserId,
    conversation: ConversationId,
    kind: MessageWindowAudience,
}

async fn admit_in_snapshot(
    tx: &mut Transaction<'_, Sqlite>,
    viewer: &UserId,
    requested: &ConversationId,
) -> Result<Result<AdmittedAudience, MessageWindowRefusal>, ConversationMessageWindowStoreError> {
    let row = load_channel_row(tx, requested).await.map_err(access)?;
    let Some(row) = row else {
        return Ok(Err(MessageWindowRefusal::NotFound));
    };
    let admitted = match row.placement() {
        ChannelLifecyclePlacement::Workspace(workspace) => {
            // Read only this viewer's seat, not every unrelated roster member.
            let member_row = sqlx::query(
                "SELECT user_id,joined_at FROM conversation_users WHERE conversation_id=? AND user_id=?",
            )
            .bind(requested.as_str())
            .bind(viewer.as_str())
            .fetch_optional(&mut **tx)
            .await?;
            let member = member_row
                .map(|row| {
                    let _: i64 = row.try_get("joined_at")?;
                    let id = UserId::parse(row.try_get::<String, _>("user_id")?)
                        .map_err(|_| ConversationMessageWindowStoreError::Coordinate)?;
                    if &id != viewer {
                        return Err(ConversationMessageWindowStoreError::Coordinate);
                    }
                    Ok(id)
                })
                .transpose()?;
            let role = load_workspace_role_fact(tx, viewer, workspace.workspace())
                .await
                .map_err(access)?;
            let member = role.as_ref().and(member);
            let public = role.is_some()
                && workspace.roster() == DirectoryRoster::Open
                && row.visibility() == ChannelVisibility::Public
                && row.archived_at().is_none();
            if let Some(source) = member {
                Some((source, MessageWindowAudience::Participating))
            } else {
                role.filter(|_| public)
                    .map(|fact| (fact.viewer, MessageWindowAudience::PublicNonmember))
            }
        }
        ChannelLifecyclePlacement::DirectPair(participants) => participants
            .both()
            .into_iter()
            .find_map(|endpoint| match endpoint {
                PrincipalIdentity::Human(id) if id == viewer => Some(id.clone()),
                _ => None,
            })
            .map(|source| (source, MessageWindowAudience::Participating)),
    };
    Ok(match admitted {
        Some((source_viewer, kind)) => Ok(AdmittedAudience {
            viewer: source_viewer,
            conversation: row.conversation().clone(),
            kind,
        }),
        None => Err(MessageWindowRefusal::Forbidden),
    })
}

fn access(error: ChannelLifecycleStoreError) -> ConversationMessageWindowStoreError {
    ConversationMessageWindowStoreError::Access(PersistenceFailure::from_source(error))
}

async fn resolve_stream(
    tx: &mut Transaction<'_, Sqlite>,
    conversation: &ConversationId,
    request: &MessageWindowStreamRequest,
) -> Result<Option<ResolvedMessageWindowStream>, ConversationMessageWindowStoreError> {
    match request {
        MessageWindowStreamRequest::Mainline => Ok(Some(ResolvedMessageWindowStream::mainline(
            conversation.clone(),
        ))),
        MessageWindowStreamRequest::Thread(requested) => {
            let row = sqlx::query(
                "SELECT id,conversation_id,thread_root_id FROM messages WHERE id=? AND conversation_id=?",
            )
            .bind(requested.as_str())
            .bind(conversation.as_str())
            .fetch_optional(&mut **tx)
            .await?;
            row.map(|row| {
                ResolvedMessageWindowStream::thread(
                    conversation.clone(),
                    requested.clone(),
                    message_id(&row, "id")?,
                    conversation_id(&row, "conversation_id")?,
                    optional_root(&row, "thread_root_id")?,
                )
                .map_err(ConversationMessageWindowStoreError::Projection)
            })
            .transpose()
        }
    }
}

async fn load_stream_head(
    tx: &mut Transaction<'_, Sqlite>,
    stream: &ResolvedMessageWindowStream,
) -> Result<MessageWindowHead, ConversationMessageWindowStoreError> {
    let rows = sqlx::query(
        "SELECT conversation_id,thread_root_id,latest_seq FROM conversation_stream_heads \
         WHERE conversation_id=? AND thread_root_id IS ?",
    )
    .bind(stream.conversation().as_str())
    .bind(stream.root().map(ThreadRootId::as_str))
    .fetch_all(&mut **tx)
    .await?;
    if rows.len() > 1 {
        return Err(ConversationMessageWindowStoreError::Coordinate);
    }
    let latest = if let Some(row) = rows.first() {
        let source = conversation_id(row, "conversation_id")?;
        let root = optional_root(row, "thread_root_id")?;
        if source != *stream.conversation() || root.as_ref() != stream.root() {
            return Err(ConversationMessageWindowStoreError::Coordinate);
        }
        message_seq(row, "latest_seq")?
    } else {
        MessageSeq::try_from(0).map_err(|_| identity("conversation_stream_heads.latest_seq"))?
    };
    MessageWindowHead::projected(
        stream.conversation().clone(),
        stream.root().cloned(),
        latest,
    )
    .map_err(ConversationMessageWindowStoreError::Projection)
}

#[tracing::instrument(
    target = "silverhand::diagnostics",
    name = "db.messages.select_window",
    skip_all,
    fields(
        elapsed_ms = tracing::field::Empty,
        outcome = tracing::field::Empty,
        operation_finished = tracing::field::Empty,
    )
)]
async fn select_window(
    tx: &mut Transaction<'_, Sqlite>,
    stream: &ResolvedMessageWindowStream,
    request: &ConversationMessageWindowRequest,
) -> Result<Option<Vec<RawMessage>>, ConversationMessageWindowStoreError> {
    let observation = silverhand_backend_observability::Operation::current();
    let result = async {
        let limit = usize::from(request.query().limit());
        match request.query().anchor() {
            MessageWindowAnchor::Latest => {
                let mut rows = fetch_messages(tx, stream, None, None, limit, false).await?;
                rows.reverse();
                Ok(Some(rows))
            }
            MessageWindowAnchor::Before(seq) => {
                let mut rows = fetch_messages(tx, stream, Some(*seq), None, limit, false).await?;
                rows.reverse();
                Ok(Some(rows))
            }
            MessageWindowAnchor::After(seq) => {
                fetch_messages(tx, stream, None, Some(*seq), limit, true)
                    .await
                    .map(Some)
            }
            MessageWindowAnchor::Around(anchor) => {
                let anchor_row = fetch_message_by_id(tx, stream, anchor).await?;
                let Some(anchor_row) = anchor_row else {
                    return Ok(None);
                };
                let before_limit = (limit.saturating_sub(1)) / 2;
                let mut before =
                    fetch_messages(tx, stream, Some(anchor_row.seq), None, before_limit, false)
                        .await?;
                before.reverse();
                let after_limit = limit.saturating_sub(before.len() + 1);
                let after =
                    fetch_messages(tx, stream, None, Some(anchor_row.seq), after_limit, true)
                        .await?;
                before.push(anchor_row);
                before.extend(after);
                Ok(Some(before))
            }
        }
    }
    .await;
    observation.finish(&result);
    result
}

async fn fetch_messages(
    tx: &mut Transaction<'_, Sqlite>,
    stream: &ResolvedMessageWindowStream,
    before: Option<MessageSeq>,
    after: Option<MessageSeq>,
    limit: usize,
    ascending: bool,
) -> Result<Vec<RawMessage>, ConversationMessageWindowStoreError> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let mut builder = QueryBuilder::<Sqlite>::new("SELECT ");
    builder
        .push(MESSAGE_COLUMNS)
        .push(" FROM messages m ")
        .push(MESSAGE_JOINS);
    builder
        .push(" WHERE m.conversation_id=")
        .push_bind(stream.conversation().as_str());
    push_stream(&mut builder, stream);
    if let Some(seq) = before {
        builder.push(" AND m.seq<").push_bind(seq.get());
    }
    if let Some(seq) = after {
        builder.push(" AND m.seq>").push_bind(seq.get());
    }
    builder
        .push(if ascending {
            " ORDER BY m.seq ASC LIMIT "
        } else {
            " ORDER BY m.seq DESC LIMIT "
        })
        .push_bind(i64::try_from(limit).map_err(|_| identity("messages.limit"))?);
    let rows = builder.build().fetch_all(&mut **tx).await?;
    rows.iter().map(RawMessage::decode).collect()
}

async fn fetch_message_by_id(
    tx: &mut Transaction<'_, Sqlite>,
    stream: &ResolvedMessageWindowStream,
    anchor: &MessageId,
) -> Result<Option<RawMessage>, ConversationMessageWindowStoreError> {
    let mut builder = QueryBuilder::<Sqlite>::new("SELECT ");
    builder
        .push(MESSAGE_COLUMNS)
        .push(" FROM messages m ")
        .push(MESSAGE_JOINS);
    builder
        .push(" WHERE m.conversation_id=")
        .push_bind(stream.conversation().as_str())
        .push(" AND m.id=")
        .push_bind(anchor.as_str());
    push_stream(&mut builder, stream);
    let row = builder.build().fetch_optional(&mut **tx).await?;
    row.as_ref().map(RawMessage::decode).transpose()
}

fn push_stream(builder: &mut QueryBuilder<Sqlite>, stream: &ResolvedMessageWindowStream) {
    match stream.root() {
        None => {
            builder.push(" AND m.thread_root_id IS NULL");
        }
        Some(root) => {
            builder
                .push(" AND (m.id=")
                .push_bind(root.as_str())
                .push(" OR m.thread_root_id=")
                .push_bind(root.as_str())
                .push(")");
        }
    }
}

async fn boundary_fact(
    tx: &mut Transaction<'_, Sqlite>,
    stream: &ResolvedMessageWindowStream,
    edge: Option<&SelectedWindowMessage>,
    direction: WindowBoundaryDirection,
) -> Result<WindowBoundaryFact, ConversationMessageWindowStoreError> {
    let Some(edge) = edge else {
        return Ok(empty_boundary(stream, direction));
    };
    let pivot = edge.projection().seq;
    let mut builder = QueryBuilder::<Sqlite>::new(
        "SELECT m.id,m.conversation_id,m.thread_root_id,m.seq FROM messages m WHERE m.conversation_id=",
    );
    builder.push_bind(stream.conversation().as_str());
    push_stream(&mut builder, stream);
    builder.push(match direction {
        WindowBoundaryDirection::Before => " AND m.seq<",
        WindowBoundaryDirection::After => " AND m.seq>",
    });
    builder.push_bind(pivot.get()).push(match direction {
        WindowBoundaryDirection::Before => " ORDER BY m.seq DESC LIMIT 1",
        WindowBoundaryDirection::After => " ORDER BY m.seq ASC LIMIT 1",
    });
    let candidate = builder.build().fetch_optional(&mut **tx).await?;
    let exists = if let Some(row) = candidate {
        let message = message_id(&row, "id")?;
        let source = conversation_id(&row, "conversation_id")?;
        let root = optional_root(&row, "thread_root_id")?;
        let candidate_seq = message_seq(&row, "seq")?;
        let in_stream = match stream.root() {
            None => root.is_none(),
            Some(expected) => {
                root.as_ref() == Some(expected)
                    || (root.is_none() && message.as_str() == expected.as_str())
            }
        };
        if source != *stream.conversation()
            || !in_stream
            || match direction {
                WindowBoundaryDirection::Before => candidate_seq >= pivot,
                WindowBoundaryDirection::After => candidate_seq <= pivot,
            }
        {
            return Err(ConversationMessageWindowStoreError::Coordinate);
        }
        true
    } else {
        false
    };
    WindowBoundaryFact::at(
        stream.conversation().clone(),
        stream.root().cloned(),
        direction,
        pivot,
        exists,
    )
    .map_err(ConversationMessageWindowStoreError::Projection)
}

fn empty_boundary(
    stream: &ResolvedMessageWindowStream,
    direction: WindowBoundaryDirection,
) -> WindowBoundaryFact {
    WindowBoundaryFact::empty(
        stream.conversation().clone(),
        stream.root().cloned(),
        direction,
    )
}

fn closed_false_boundary(
    stream: &ResolvedMessageWindowStream,
    edge: Option<&SelectedWindowMessage>,
    direction: WindowBoundaryDirection,
) -> Result<WindowBoundaryFact, ConversationMessageWindowStoreError> {
    match edge {
        None => Ok(empty_boundary(stream, direction)),
        Some(edge) => WindowBoundaryFact::at(
            stream.conversation().clone(),
            stream.root().cloned(),
            direction,
            edge.projection().seq,
            false,
        )
        .map_err(ConversationMessageWindowStoreError::Projection),
    }
}

/// One selected `messages` row, decoded but not yet author-resolved.
///
/// `pub(crate)`: the agent-audit read adapter selects its own `mode=all` rows and then
/// decodes/seals them through this same authority, so the audit transcript and the
/// authenticated window agree on what a row means. Only `author` (evidence to batch)
/// and `seq` (the keyset pivot) are read across the boundary; the rest stay private to
/// [`decode`](Self::decode)/[`seal`](Self::seal).
#[derive(Clone)]
pub(crate) struct RawMessage {
    client_message_id: Option<String>,
    id: MessageId,
    conversation: ConversationId,
    pub(crate) author: AuthorEvidence,
    content: String,
    metadata_json: Option<String>,
    attachments: Vec<AttachmentSnapshot>,
    pub(crate) seq: MessageSeq,
    created_at: i64,
    thread_root: Option<ThreadRootId>,
    linked_task: Option<WindowLinkedTask>,
}

impl RawMessage {
    pub(crate) fn decode(row: &SqliteRow) -> Result<Self, ConversationMessageWindowStoreError> {
        let metadata_json: Option<String> = row.try_get("metadata")?;
        Ok(Self {
            client_message_id: row.try_get("client_message_id")?,
            id: message_id(row, "message_id")?,
            conversation: conversation_id(row, "message_conversation_id")?,
            author: author_evidence(row, "")?,
            content: row.try_get("content")?,
            attachments: decode_attachments(metadata_json.as_deref())?,
            metadata_json,
            seq: message_seq(row, "seq")?,
            created_at: row.try_get("created_at")?,
            thread_root: optional_root(row, "thread_root_id")?,
            linked_task: decode_linked_task(row)?,
        })
    }

    pub(crate) fn seal(
        self,
        resolved: Option<MessageAuthor>,
        reactions: Vec<WindowReaction>,
    ) -> Result<SelectedWindowMessage, ConversationMessageWindowStoreError> {
        let author = author_fact(&self.author, resolved)?;
        SelectedWindowMessage::seal(WindowMessageProjection {
            client_message_id: self.client_message_id,
            id: self.id,
            conversation: self.conversation,
            author,
            content: self.content,
            metadata: WindowMessageMetadata::decoded(self.metadata_json, self.attachments),
            seq: self.seq,
            created_at_ms: self.created_at,
            thread_root: self.thread_root,
            linked_task: self.linked_task,
            reactions,
        })
        .map_err(ConversationMessageWindowStoreError::Projection)
    }
}

struct RawReplySidecars {
    summaries: HashMap<ThreadRootId, RawReplySummary>,
    repliers: Vec<RawReplier>,
    previews: Vec<RawPreview>,
}

impl RawReplySidecars {
    fn seal(
        self,
        conversation: &ConversationId,
        roots: &[ThreadRootId],
        replier_authors: &[Option<MessageAuthor>],
        preview_authors: &[Option<MessageAuthor>],
    ) -> Result<Vec<WindowRootReplies>, ConversationMessageWindowStoreError> {
        let root_set = roots.iter().collect::<HashSet<_>>();
        if self.summaries.keys().any(|root| !root_set.contains(root)) {
            return Err(ConversationMessageWindowStoreError::Coordinate);
        }
        let mut repliers = HashMap::<ThreadRootId, Vec<(MessageSeq, WindowAuthorFact)>>::new();
        for (raw, resolved) in self
            .repliers
            .into_iter()
            .zip(replier_authors.iter().cloned())
        {
            if raw.conversation != *conversation {
                return Err(ConversationMessageWindowStoreError::Coordinate);
            }
            repliers
                .entry(raw.root)
                .or_default()
                .push((raw.last_seq, author_fact(&raw.author, resolved)?));
        }
        let mut previews = HashMap::<ThreadRootId, Vec<WindowReplyPreview>>::new();
        for (raw, resolved) in self
            .previews
            .into_iter()
            .zip(preview_authors.iter().cloned())
        {
            if raw.conversation != *conversation {
                return Err(ConversationMessageWindowStoreError::Coordinate);
            }
            let author = author_fact(&raw.author, resolved)?;
            let labels = resolve_labels(&raw.attachments)?;
            let preview = WindowReplyPreview::sealed(
                raw.conversation,
                raw.root.clone(),
                raw.message,
                raw.seq,
                raw.created_at,
                author,
                &raw.content,
                WindowCount::new(
                    i64::try_from(raw.attachments.len())
                        .map_err(|_| identity("messages.metadata.attachments"))?,
                )
                .map_err(ConversationMessageWindowStoreError::Projection)?,
                labels.first().copied(),
            )
            .map_err(ConversationMessageWindowStoreError::Projection)?;
            previews.entry(raw.root).or_default().push(preview);
        }

        let mut out = Vec::with_capacity(roots.len());
        for root in roots {
            let summary = self.summaries.get(root);
            let count = WindowCount::new(summary.map_or(0, |value| value.count))
                .map_err(ConversationMessageWindowStoreError::Projection)?;
            let mut stable_repliers = Vec::new();
            for (last_seq, author) in repliers.remove(root).unwrap_or_default() {
                if author
                    .author()
                    .and_then(MessageAuthor::stable_identity)
                    .is_none()
                {
                    continue;
                }
                stable_repliers.push(
                    WindowReplier::projected(conversation.clone(), root.clone(), author, last_seq)
                        .map_err(ConversationMessageWindowStoreError::Projection)?,
                );
            }
            let total = WindowCount::new(
                i64::try_from(stable_repliers.len())
                    .map_err(|_| identity("messages.replier_total"))?,
            )
            .map_err(ConversationMessageWindowStoreError::Projection)?;
            stable_repliers.truncate(3);
            let badge = summary
                .map(|summary| {
                    if summary.conversation != *conversation || summary.root != *root {
                        return Err(ConversationMessageWindowStoreError::Coordinate);
                    }
                    ReplyBadgeProjection::sealed(
                        summary.conversation.clone(),
                        summary.root.clone(),
                        count,
                        summary.last_reply_at,
                        summary.last_reply_seq,
                        stable_repliers,
                        total,
                    )
                    .map_err(ConversationMessageWindowStoreError::Projection)
                })
                .transpose()?;
            out.push(
                WindowRootReplies::sealed(
                    conversation.clone(),
                    root.clone(),
                    count,
                    badge,
                    previews.remove(root).unwrap_or_default(),
                )
                .map_err(ConversationMessageWindowStoreError::Projection)?,
            );
        }
        if !repliers.is_empty() || !previews.is_empty() {
            return Err(ConversationMessageWindowStoreError::Coordinate);
        }
        Ok(out)
    }
}

struct RawReplySummary {
    conversation: ConversationId,
    root: ThreadRootId,
    count: i64,
    last_reply_at: i64,
    last_reply_seq: MessageSeq,
}

struct RawReplier {
    conversation: ConversationId,
    root: ThreadRootId,
    author: AuthorEvidence,
    last_seq: MessageSeq,
}

struct RawPreview {
    conversation: ConversationId,
    root: ThreadRootId,
    message: MessageId,
    seq: MessageSeq,
    created_at: i64,
    author: AuthorEvidence,
    content: String,
    attachments: Vec<AttachmentSnapshot>,
}

// Drive latest-row lookup from the selected-root aggregate. Reversing this
// inner join lets SQLite scan every message in the conversation, even when
// none of the requested roots has a reply.
const REPLY_SUMMARIES: &str = "SELECT lr.conversation_id AS reply_conversation_id,agg.root_id,agg.reply_count,\
         lr.created_at AS last_reply_at,lr.seq AS last_reply_seq FROM \
         (SELECT thread_root_id AS root_id,COUNT(*) AS reply_count,MAX(seq) AS last_reply_seq \
         FROM messages m WHERE m.thread_root_id IN (SELECT value FROM json_each(?)) \
         AND m.conversation_id=? AND m.from_kind!='system' GROUP BY m.thread_root_id) agg \
         CROSS JOIN messages lr ON lr.conversation_id=? AND lr.thread_root_id=agg.root_id \
         AND lr.seq=agg.last_reply_seq";

const REPLY_REPLIERS: &str = "SELECT m.conversation_id AS reply_conversation_id,m.thread_root_id AS root_id,\
         m.from_kind,m.from_agent_id,m.from_user_id,m.author_id,m.author_name,\
         m.author_display_name,MAX(m.seq) AS last_seq FROM messages m \
         WHERE m.thread_root_id IN (SELECT value FROM json_each(?)) AND m.conversation_id=? \
         AND m.from_kind!='system' GROUP BY m.thread_root_id,m.from_kind,m.author_id \
         ORDER BY m.thread_root_id, last_seq DESC";

const REPLY_PREVIEWS: &str = "SELECT reply_conversation_id,root_id,message_id,seq,created_at,from_kind,from_agent_id,\
         from_user_id,author_id,author_name,author_display_name,content,metadata FROM \
         (SELECT m.conversation_id AS reply_conversation_id,m.thread_root_id AS root_id,\
         m.id AS message_id,m.seq,m.created_at,m.from_kind,m.from_agent_id,m.from_user_id,\
         m.author_id,m.author_name,m.author_display_name,m.content,m.metadata,\
         ROW_NUMBER() OVER(PARTITION BY m.thread_root_id ORDER BY m.seq DESC) AS rn \
         FROM messages m WHERE m.thread_root_id IN (SELECT value FROM json_each(?)) \
         AND m.conversation_id=? AND m.from_kind!='system') WHERE rn<=3 \
         ORDER BY root_id,seq ASC";

async fn load_reply_sidecars(
    tx: &mut Transaction<'_, Sqlite>,
    conversation: &ConversationId,
    roots: &[ThreadRootId],
) -> Result<RawReplySidecars, ConversationMessageWindowStoreError> {
    if roots.is_empty() {
        return Ok(RawReplySidecars {
            summaries: HashMap::new(),
            repliers: Vec::new(),
            previews: Vec::new(),
        });
    }
    let root_json =
        serde_json::to_string(&roots.iter().map(ThreadRootId::as_str).collect::<Vec<_>>())
            .map_err(|_| ConversationMessageWindowStoreError::Metadata)?;
    let summary_rows = sqlx::query(REPLY_SUMMARIES)
        .bind(&root_json)
        .bind(conversation.as_str())
        .bind(conversation.as_str())
        .fetch_all(&mut **tx)
        .await?;
    let mut summaries = HashMap::new();
    for row in &summary_rows {
        let value = RawReplySummary {
            conversation: conversation_id(row, "reply_conversation_id")?,
            root: thread_root_id(row, "root_id")?,
            count: row.try_get("reply_count")?,
            last_reply_at: row.try_get("last_reply_at")?,
            last_reply_seq: message_seq(row, "last_reply_seq")?,
        };
        if summaries.insert(value.root.clone(), value).is_some() {
            return Err(ConversationMessageWindowStoreError::Coordinate);
        }
    }
    let replier_rows = sqlx::query(REPLY_REPLIERS)
        .bind(&root_json)
        .bind(conversation.as_str())
        .fetch_all(&mut **tx)
        .await?;
    let repliers = replier_rows
        .iter()
        .map(|row| {
            Ok(RawReplier {
                conversation: conversation_id(row, "reply_conversation_id")?,
                root: thread_root_id(row, "root_id")?,
                author: author_evidence(row, "")?,
                last_seq: message_seq(row, "last_seq")?,
            })
        })
        .collect::<Result<Vec<_>, ConversationMessageWindowStoreError>>()?;
    let preview_rows = sqlx::query(REPLY_PREVIEWS)
        .bind(root_json)
        .bind(conversation.as_str())
        .fetch_all(&mut **tx)
        .await?;
    let previews = preview_rows
        .iter()
        .map(|row| {
            let metadata: Option<String> = row.try_get("metadata")?;
            Ok(RawPreview {
                conversation: conversation_id(row, "reply_conversation_id")?,
                root: thread_root_id(row, "root_id")?,
                message: message_id(row, "message_id")?,
                seq: message_seq(row, "seq")?,
                created_at: row.try_get("created_at")?,
                author: author_evidence(row, "")?,
                content: row.try_get("content")?,
                attachments: decode_attachments(metadata.as_deref())?,
            })
        })
        .collect::<Result<Vec<_>, ConversationMessageWindowStoreError>>()?;
    Ok(RawReplySidecars {
        summaries,
        repliers,
        previews,
    })
}

fn author_evidence(
    row: &SqliteRow,
    prefix: &str,
) -> Result<AuthorEvidence, ConversationMessageWindowStoreError> {
    let column = |suffix: &str| format!("{prefix}{suffix}");
    let from_kind = match row
        .try_get::<String, _>(column("from_kind").as_str())?
        .as_str()
    {
        "agent" => FromKind::Agent,
        "user" => FromKind::User,
        "system" => FromKind::System,
        _ => return Err(ConversationMessageWindowStoreError::AuthorKind),
    };
    Ok(AuthorEvidence {
        from_kind,
        from_agent_id: row.try_get(column("from_agent_id").as_str())?,
        from_user_id: row.try_get(column("from_user_id").as_str())?,
        author_id: row.try_get(column("author_id").as_str())?,
        author_name: row.try_get(column("author_name").as_str())?,
        author_display_name: row.try_get(column("author_display_name").as_str())?,
    })
}

pub(super) fn author_fact(
    evidence: &AuthorEvidence,
    resolved: Option<MessageAuthor>,
) -> Result<WindowAuthorFact, ConversationMessageWindowStoreError> {
    let from_agent = evidence
        .from_agent_id
        .clone()
        .map(AgentId::parse)
        .transpose()
        .map_err(|_| identity("messages.from_agent_id"))?;
    let from_user = evidence
        .from_user_id
        .clone()
        .map(UserId::parse)
        .transpose()
        .map_err(|_| identity("messages.from_user_id"))?;
    let snapshot_identity = evidence
        .author_id
        .as_ref()
        .map(|id| match evidence.from_kind {
            FromKind::Agent => AgentId::parse(id.clone())
                .map(PrincipalIdentity::Agent)
                .map_err(|_| identity("messages.author_id")),
            FromKind::User => UserId::parse(id.clone())
                .map(PrincipalIdentity::Human)
                .map_err(|_| identity("messages.author_id")),
            FromKind::System => Err(identity("messages.author_id")),
        })
        .transpose()?;
    WindowAuthorFact::sealed(
        evidence.from_kind,
        from_agent,
        from_user,
        snapshot_identity,
        evidence.author_name.clone(),
        evidence.author_display_name.clone(),
        resolved,
    )
    .map_err(ConversationMessageWindowStoreError::Projection)
}

fn decode_linked_task(
    row: &SqliteRow,
) -> Result<Option<WindowLinkedTask>, ConversationMessageWindowStoreError> {
    let Some(id) = row.try_get::<Option<String>, _>("task_id")? else {
        return Ok(None);
    };
    let id = TaskId::parse(id).map_err(|_| linked_task("tasks.id"))?;
    let number = TaskNumber::new(
        row.try_get::<Option<i64>, _>("task_number")?
            .ok_or(linked_task("tasks.task_number:missing"))?,
    )
    .map_err(|_| linked_task("tasks.task_number"))?;
    // A correct writer persists a title already run through the create/edit
    // canonicalizer, and `tasks.title` carries no canonicity CHECK — so, exactly as
    // `TaskAggregate::decode` does, parse for the contract then require the stored
    // bytes to already equal the canonical form. Using the parsed (collapsed) value
    // would silently rewrite a persisted fact.
    let raw_title = row
        .try_get::<Option<String>, _>("task_title")?
        .ok_or(linked_task("tasks.title:missing"))?;
    let title = TaskTitle::parse(&raw_title).map_err(|_| linked_task("tasks.title"))?;
    if title.as_str() != raw_title {
        return Err(linked_task("tasks.title:non_canonical"));
    }
    let status = TaskStatus::parse(
        &row.try_get::<Option<String>, _>("task_status")?
            .ok_or(linked_task("tasks.status:missing"))?,
    )
    .map_err(|_| linked_task("tasks.status"))?;
    let agent: Option<String> = row.try_get("assignee_agent_id")?;
    let user: Option<String> = row.try_get("assignee_user_id")?;
    let assignee = match (agent, user) {
        (Some(id), None) => Some(WindowTaskAssignee::projected(
            PrincipalIdentity::Agent(
                AgentId::parse(id.clone()).map_err(|_| identity("tasks.assignee_agent_id"))?,
            ),
            row.try_get::<Option<String>, _>("assignee_agent_name")?
                .unwrap_or(id),
        )),
        (None, Some(id)) => Some(WindowTaskAssignee::projected(
            PrincipalIdentity::Human(
                UserId::parse(id.clone()).map_err(|_| identity("tasks.assignee_user_id"))?,
            ),
            row.try_get::<Option<String>, _>("assignee_user_name")?
                .unwrap_or(id),
        )),
        (None, None) => None,
        _ => return Err(linked_task("tasks.assignee")),
    };
    Ok(Some(WindowLinkedTask::projected(
        ConversationId::parse(
            row.try_get::<Option<String>, _>("task_conversation_id")?
                .ok_or(linked_task("tasks.conversation_id:missing"))?,
        )
        .map_err(|_| identity("tasks.conversation_id"))?,
        message_id(row, "message_id")?,
        id,
        number,
        title,
        status,
        assignee,
    )))
}

pub(crate) fn decode_attachments(
    raw: Option<&str>,
) -> Result<Vec<AttachmentSnapshot>, ConversationMessageWindowStoreError> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|_| ConversationMessageWindowStoreError::Metadata)?;
    let object = value
        .as_object()
        .ok_or(ConversationMessageWindowStoreError::Metadata)?;
    let Some(items) = object.get("attachments") else {
        return Ok(Vec::new());
    };
    let items = items
        .as_array()
        .ok_or(ConversationMessageWindowStoreError::Metadata)?;
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let object = item
            .as_object()
            .ok_or(ConversationMessageWindowStoreError::Metadata)?;
        let id = AttachmentId::parse(
            object
                .get("id")
                .and_then(serde_json::Value::as_str)
                .ok_or(ConversationMessageWindowStoreError::Metadata)?,
        )
        .map_err(|_| identity("messages.metadata.attachments.id"))?;
        let filename = object
            .get("filename")
            .and_then(serde_json::Value::as_str)
            .ok_or(ConversationMessageWindowStoreError::Metadata)?
            .to_owned();
        let size = object
            .get("size")
            .and_then(serde_json::Value::as_i64)
            .ok_or(ConversationMessageWindowStoreError::Metadata)?;
        let content_type = object
            .get("content_type")
            .and_then(serde_json::Value::as_str)
            .ok_or(ConversationMessageWindowStoreError::Metadata)?
            .to_owned();
        let label = object
            .get("label")
            .and_then(serde_json::Value::as_str)
            .and_then(AttachmentLabel::parse);
        out.push(AttachmentSnapshot {
            id,
            filename,
            size,
            content_type,
            label,
        });
    }
    Ok(out)
}

fn resolve_labels(
    attachments: &[AttachmentSnapshot],
) -> Result<Vec<AttachmentLabel>, ConversationMessageWindowStoreError> {
    let mut used_images = HashSet::new();
    let mut used_files = HashSet::new();
    for attachment in attachments {
        if let Some(label) = attachment.label {
            match label.kind() {
                AttachmentLabelKind::Image => {
                    used_images.insert(label.ordinal());
                }
                AttachmentLabelKind::File => {
                    used_files.insert(label.ordinal());
                }
            }
        }
    }
    let mut next_image = 1_u16;
    let mut next_file = 1_u16;
    attachments
        .iter()
        .map(|attachment| {
            if let Some(label) = attachment.label {
                return Ok(label);
            }
            let (kind, next, used) = if attachment.content_type.starts_with("image/") {
                (
                    AttachmentLabelKind::Image,
                    &mut next_image,
                    &mut used_images,
                )
            } else {
                (AttachmentLabelKind::File, &mut next_file, &mut used_files)
            };
            while used.contains(next) {
                *next = next.saturating_add(1);
            }
            let text = match kind {
                AttachmentLabelKind::Image => format!("Image #{}", *next),
                AttachmentLabelKind::File => format!("File #{}", *next),
            };
            used.insert(*next);
            *next = next.saturating_add(1);
            AttachmentLabel::parse(&text).ok_or(ConversationMessageWindowStoreError::Metadata)
        })
        .collect()
}

fn message_id(
    row: &SqliteRow,
    field: &'static str,
) -> Result<MessageId, ConversationMessageWindowStoreError> {
    MessageId::parse(row.try_get::<String, _>(field)?).map_err(|_| identity(field))
}

fn conversation_id(
    row: &SqliteRow,
    field: &'static str,
) -> Result<ConversationId, ConversationMessageWindowStoreError> {
    ConversationId::parse(row.try_get::<String, _>(field)?).map_err(|_| identity(field))
}

fn thread_root_id(
    row: &SqliteRow,
    field: &'static str,
) -> Result<ThreadRootId, ConversationMessageWindowStoreError> {
    ThreadRootId::parse(row.try_get::<String, _>(field)?).map_err(|_| identity(field))
}

fn optional_root(
    row: &SqliteRow,
    field: &'static str,
) -> Result<Option<ThreadRootId>, ConversationMessageWindowStoreError> {
    row.try_get::<Option<String>, _>(field)?
        .map(ThreadRootId::parse)
        .transpose()
        .map_err(|_| identity(field))
}

fn message_seq(
    row: &SqliteRow,
    field: &'static str,
) -> Result<MessageSeq, ConversationMessageWindowStoreError> {
    MessageSeq::try_from(row.try_get::<i64, _>(field)?).map_err(|_| identity(field))
}

fn identity(field: &'static str) -> ConversationMessageWindowStoreError {
    ConversationMessageWindowStoreError::Identity { field }
}

fn linked_task(field: &'static str) -> ConversationMessageWindowStoreError {
    ConversationMessageWindowStoreError::LinkedTask { field }
}

#[cfg(test)]
mod reply_lookup_tests {
    use super::*;
    use sqlx::{Connection, Executor};

    #[tokio::test]
    async fn a_queued_window_authorizes_after_access_changes_before_its_snapshot() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let mut db = pool.acquire().await.unwrap();
        db.execute(sqlx::raw_sql(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../rust/test-fixtures/migration-baseline/schema-current.sql"
        ))))
        .await
        .unwrap();
        for statement in [
            "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES('viewer','viewer','Viewer',1,1)",
            "INSERT INTO workspaces(id,name,name_key,created_at,updated_at) VALUES('w','W','w',1,1)",
            "INSERT INTO workspace_users(workspace_id,user_id,joined_at,role) VALUES('w','viewer',1,'member')",
            "INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,status,visibility,latest_message_seq,created_at,updated_at) VALUES('c','w','workspace','open','C','c','idle','private',0,1,1)",
            "INSERT INTO conversation_users(conversation_id,user_id,joined_at) VALUES('c','viewer',1)",
        ] {
            db.execute(statement).await.unwrap();
        }
        drop(db);
        let reader = SqliteConversationMessageWindowRead {
            pool: pool.clone().into(),
        };
        let viewer = UserId::parse("viewer").unwrap();
        let conversation = ConversationId::parse("c").unwrap();
        let request = ConversationMessageWindowRequest::new(
            MessageWindowStreamRequest::Mainline,
            50,
            MessageWindowAnchor::Latest,
        )
        .unwrap();
        assert!(
            reader
                .read(&viewer, &conversation, request.clone())
                .await
                .unwrap()
                .is_ok()
        );
        let mut db = pool.acquire().await.unwrap();
        let pending = reader.read(&viewer, &conversation, request);
        tokio::pin!(pending);
        tokio::select! {
            biased;
            result = &mut pending => panic!("read bypassed occupied snapshot connection: {result:?}"),
            () = tokio::task::yield_now() => {}
        }
        db.execute("DELETE FROM conversation_users WHERE conversation_id='c' AND user_id='viewer'")
            .await
            .unwrap();
        drop(db);
        assert!(matches!(
            pending.await.unwrap(),
            Err(MessageWindowRefusal::Forbidden)
        ));
        pool.close().await;
    }

    #[tokio::test]
    async fn all_reply_sidecars_seek_both_conversation_and_root_on_linked_sqlite() {
        let mut db = sqlx::SqliteConnection::connect("sqlite::memory:")
            .await
            .unwrap();
        db.execute(sqlx::raw_sql(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../rust/test-fixtures/migration-baseline/schema-current.sql"
        ))))
        .await
        .unwrap();
        let roots =
            serde_json::to_string(&(0..50).map(|n| format!("root-{n}")).collect::<Vec<_>>())
                .unwrap();
        for (name, statement, binds) in [
            ("summary", REPLY_SUMMARIES, 3),
            ("repliers", REPLY_REPLIERS, 2),
            ("previews", REPLY_PREVIEWS, 2),
        ] {
            let explain = format!("EXPLAIN QUERY PLAN {statement}");
            // Prefix only a compile-time production statement, never request text.
            let mut query = sqlx::query(sqlx::AssertSqlSafe(explain))
                .bind(&roots)
                .bind("c");
            if binds == 3 {
                query = query.bind("c");
            }
            let rows = query.fetch_all(&mut db).await.unwrap();
            let plan = rows
                .iter()
                .map(|row| row.get::<String, _>("detail"))
                .collect::<Vec<_>>();
            assert!(plan.iter().any(|step| step.contains("SEARCH m USING INDEX idx_messages_thread_root (conversation_id=? AND thread_root_id=?)")), "{name}: {plan:?}");
            if name == "summary" {
                assert!(plan.iter().any(|step| step.contains("SEARCH lr USING INDEX idx_messages_thread_root (conversation_id=? AND thread_root_id=? AND seq=?)")), "{plan:?}");
            }
            eprintln!("{name}: {plan:?}");
        }
        db.close().await.unwrap();
    }
}
