//! One-snapshot SQLite attachment reads.
//!
//! Admission, canonical message-reference selection, row decoding, and public
//! reference proof all happen inside one deferred transaction. Filtering and
//! page trimming happen only after every selected row has been strictly decoded,
//! so a malformed row cannot disappear behind a product predicate.

use silverhand_backend_core::{
    conversation_messaging::messaging::{
        AttachmentId, FromKind, MessageId, MessageSeq, PrincipalIdentity, ThreadRootId,
        collaboration::{
            attachment::{
                AttachmentContentType, AttachmentCreator, AttachmentDownloadFact,
                AttachmentDownloadReadPort, AttachmentDownloadRefusal, AttachmentFilename,
                AttachmentListCursor, AttachmentListItem, AttachmentListPage, AttachmentListQuery,
                AttachmentListReadPort, AttachmentListSource, AttachmentRecord,
                AttachmentSenderFacet, AttachmentSenderFacetReadPort, AttachmentSenderFacets,
                AttachmentSenderFilter, AttachmentSourceActor, ChannelAttachmentReadRefusal,
            },
            channel_directory::{ChannelVisibility, DirectoryRoster},
            channel_lifecycle::{ChannelLifecyclePlacement, ChannelLifecycleRow},
        },
    },
    foundation::blob_store::{BlobStorageKey, StorageKind, StoredBlob},
    foundation::identity::{AgentId, ConversationId, UserId},
    foundation::persistence::PersistenceFailure,
    foundation::wire_number::JsSafeInteger,
};
use sqlx::{Row, Sqlite, Transaction, sqlite::SqliteRow};
use thiserror::Error;

use crate::conversation_messaging::messaging::{
    MessagingStore,
    author::{AuthorEvidence, resolve_authors},
    collaboration::channel_lifecycle::{
        ChannelLifecycleStoreError, load_channel_row, load_human_members, load_workspace_role,
    },
};

macro_rules! canonical_refs_sql {
    () => {
        r#"raw AS (
    SELECT m.id AS message_id,m.conversation_id AS message_conversation_id,
           m.thread_root_id AS thread_root_id,m.seq AS message_seq,
           m.from_kind AS from_kind,m.from_user_id AS from_user_id,
           m.from_agent_id AS from_agent_id,m.author_id AS author_id,
           m.author_name AS author_name,m.author_display_name AS author_display_name,
           m.created_at AS appeared_at,
           CASE WHEN je.type='object' THEN json_extract(je.value,'$.id') ELSE NULL END AS attachment_id
      FROM messages m,
           json_each(CASE WHEN json_valid(m.metadata) THEN
             CASE WHEN json_type(m.metadata,'$.attachments')='array'
               THEN json_extract(m.metadata,'$.attachments') ELSE '[]' END
             ELSE '[]' END) je
     WHERE m.conversation_id=?1
),refs AS (
    SELECT attachment_id,message_id,message_conversation_id,thread_root_id,message_seq,
           from_kind,from_user_id,from_agent_id,author_id,author_name,author_display_name,appeared_at,
           ROW_NUMBER() OVER (PARTITION BY attachment_id ORDER BY appeared_at ASC,message_id ASC) AS rn
      FROM raw WHERE typeof(attachment_id)='text' AND attachment_id<>''
)"#
    };
}

const LIST_SQL: &str = concat!(
    "WITH ",
    canonical_refs_sql!(),
    r#" SELECT r.attachment_id,r.message_id,r.message_conversation_id,r.thread_root_id,
             r.message_seq,r.from_kind,r.from_user_id,r.from_agent_id,r.author_id,
             r.author_name,r.author_display_name,r.appeared_at,
             a.id AS a_id,a.channel_id AS a_channel_id,
             a.source_conversation_id AS a_source_conversation_id,a.filename AS a_filename,
             a.size AS a_size,a.content_type AS a_content_type,a.storage AS a_storage,
             a.storage_key AS a_storage_key,a.content_revision AS a_content_revision,
             a.created_by_kind AS a_created_by_kind,a.created_by_id AS a_created_by_id,
             a.created_at AS a_created_at
        FROM refs r JOIN attachments a ON a.id=r.attachment_id AND a.channel_id=?1
       WHERE r.rn=1
         AND (?2 IS NULL OR (r.from_kind=?2
              AND (?3 IS NULL OR r.from_user_id=?3)
              AND (?4 IS NULL OR r.from_agent_id=?4)))
         AND (?5 IS NULL OR r.appeared_at>=?5)
         AND (?6 IS NULL OR r.appeared_at<?6)
         AND (?7 IS NULL OR r.appeared_at<?7
              OR (r.appeared_at=?7 AND r.attachment_id<?8))
       ORDER BY r.appeared_at DESC,r.attachment_id DESC LIMIT ?9"#
);

const FACETS_SQL: &str = concat!(
    "WITH ",
    canonical_refs_sql!(),
    r#" SELECT DISTINCT r.from_kind,r.from_user_id,r.from_agent_id
        FROM refs r JOIN attachments a ON a.id=r.attachment_id AND a.channel_id=?1
       WHERE r.rn=1
         AND ((r.from_kind='user' AND r.from_user_id IS NOT NULL AND r.from_user_id<>''
               AND r.from_agent_id IS NULL)
           OR (r.from_kind='agent' AND r.from_agent_id IS NOT NULL AND r.from_agent_id<>''
               AND r.from_user_id IS NULL)
           OR (r.from_kind='system' AND r.from_user_id IS NULL AND r.from_agent_id IS NULL))
       ORDER BY r.from_kind ASC,COALESCE(r.from_user_id,r.from_agent_id) ASC"#
);

pub(super) const ATTACHMENT_SQL: &str = r#"SELECT id AS a_id,channel_id AS a_channel_id,
    source_conversation_id AS a_source_conversation_id,filename AS a_filename,size AS a_size,
    content_type AS a_content_type,storage AS a_storage,storage_key AS a_storage_key,
    content_revision AS a_content_revision,created_by_kind AS a_created_by_kind,
    created_by_id AS a_created_by_id,created_at AS a_created_at FROM attachments WHERE id=?"#;

const CHANNEL_REFERENCES_SQL: &str = r#"SELECT m.conversation_id,
    CASE WHEN je.type='object' THEN json_extract(je.value,'$.id') ELSE NULL END AS attachment_id
    FROM messages m,json_each(CASE WHEN json_valid(m.metadata) THEN
      CASE WHEN json_type(m.metadata,'$.attachments')='array'
        THEN json_extract(m.metadata,'$.attachments') ELSE '[]' END ELSE '[]' END) je
    WHERE m.conversation_id=?1
      AND CASE WHEN je.type='object' THEN json_extract(je.value,'$.id') ELSE NULL END=?2"#;

#[derive(Clone)]
pub struct SqliteAttachmentRead {
    pool: crate::foundation::database::SqliteDatabase,
}

impl<E> MessagingStore<E> {
    pub fn attachment_reads(&self) -> SqliteAttachmentRead {
        SqliteAttachmentRead {
            pool: self.pool.clone(),
        }
    }
}

#[derive(Debug, Error)]
pub enum AttachmentReadStoreError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("channel admission row is malformed")]
    Channel(#[from] ChannelLifecycleStoreError),
    #[error("stored column {field} does not contain a usable identity")]
    Identity { field: &'static str },
    #[error("stored attachment storage kind is outside the closed set")]
    StorageKind,
    #[error("stored attachment storage key is malformed")]
    StorageKey,
    #[error("stored attachment filename is malformed")]
    Filename,
    #[error("stored attachment content type is malformed")]
    ContentType,
    #[error("stored attachment creator shape is malformed")]
    Creator,
    #[error("stored attachment row violates its scalar invariants")]
    Attachment,
    #[error("stored scalar {field} is outside the exact JSON integer domain")]
    Scalar { field: &'static str },
    #[error("stored message sender shape is malformed")]
    Sender,
    #[error("stored message ordinal is malformed")]
    MessageSeq,
    #[error("stored canonical attachment source is malformed")]
    Source,
    #[error("stored attachment/reference coordinates disagree")]
    Coordinate,
    #[error("message author resolution failed")]
    Author(#[source] PersistenceFailure),
    #[error("attachment row points at a missing conversation")]
    MissingConversation,
}

impl From<AttachmentReadStoreError> for PersistenceFailure {
    fn from(value: AttachmentReadStoreError) -> Self {
        Self::from_source(value)
    }
}

impl AttachmentListReadPort for SqliteAttachmentRead {
    type Error = PersistenceFailure;

    async fn list(
        &self,
        viewer: &UserId,
        conversation: &ConversationId,
        query: &AttachmentListQuery,
    ) -> Result<Result<AttachmentListPage, ChannelAttachmentReadRefusal>, Self::Error> {
        self.list_inner(viewer, conversation, query)
            .await
            .map_err(PersistenceFailure::from)
    }
}

impl AttachmentSenderFacetReadPort for SqliteAttachmentRead {
    type Error = PersistenceFailure;

    async fn sender_facets(
        &self,
        viewer: &UserId,
        conversation: &ConversationId,
    ) -> Result<Result<AttachmentSenderFacets, ChannelAttachmentReadRefusal>, Self::Error> {
        self.sender_facets_inner(viewer, conversation)
            .await
            .map_err(PersistenceFailure::from)
    }
}

impl AttachmentDownloadReadPort for SqliteAttachmentRead {
    type Error = PersistenceFailure;

    async fn download(
        &self,
        viewer: &UserId,
        attachment: &AttachmentId,
    ) -> Result<Result<AttachmentDownloadFact, AttachmentDownloadRefusal>, Self::Error> {
        self.download_inner(viewer, attachment)
            .await
            .map_err(PersistenceFailure::from)
    }
}

impl SqliteAttachmentRead {
    async fn list_inner(
        &self,
        viewer: &UserId,
        requested: &ConversationId,
        query: &AttachmentListQuery,
    ) -> Result<Result<AttachmentListPage, ChannelAttachmentReadRefusal>, AttachmentReadStoreError>
    {
        let mut tx = self.pool.begin_with("BEGIN DEFERRED").await?;
        if let Err(refusal) = require_member(&mut tx, viewer, requested).await? {
            tx.commit().await?;
            return Ok(Err(refusal));
        }

        let (sender_kind, sender_user, sender_agent) = sender_binds(query.sender());
        let rows = sqlx::query(LIST_SQL)
            .bind(requested.as_str())
            .bind(sender_kind)
            .bind(sender_user)
            .bind(sender_agent)
            .bind(query.since().map(JsSafeInteger::get))
            .bind(query.until().map(JsSafeInteger::get))
            .bind(
                query
                    .cursor()
                    .map(AttachmentListCursor::appeared_at)
                    .map(JsSafeInteger::get),
            )
            .bind(query.cursor().map(|cursor| cursor.attachment().as_str()))
            .bind(i64::from(query.limit()) + 1)
            .fetch_all(&mut *tx)
            .await?;

        let decoded = rows
            .iter()
            .map(|row| decode_list_row(row, requested))
            .collect::<Result<Vec<_>, _>>()?;
        let evidence = decoded
            .iter()
            .map(|row| row.author_evidence.clone())
            .collect::<Vec<_>>();
        let authors = resolve_authors(&mut tx, &evidence)
            .await
            .map_err(AttachmentReadStoreError::Author)?;
        let mut items = decoded
            .into_iter()
            .zip(authors)
            .map(|(row, author)| row.project(author, requested))
            .collect::<Result<Vec<_>, _>>()?;

        let has_more = items.len() > usize::from(query.limit());
        if has_more {
            items.truncate(usize::from(query.limit()));
        }
        let next_cursor = has_more
            .then(|| {
                let tail = items.last().expect("positive list limit guarantees a tail");
                AttachmentListCursor::new_safe(
                    tail.source().appeared_at(),
                    tail.attachment().id().clone(),
                )
            })
            .transpose()
            .map_err(|_| AttachmentReadStoreError::Coordinate)?;
        let page = AttachmentListPage::projected(
            viewer.clone(),
            requested.clone(),
            query.clone(),
            items,
            next_cursor,
        )
        .map_err(|_| AttachmentReadStoreError::Coordinate)?;
        tx.commit().await?;
        Ok(Ok(page))
    }

    async fn sender_facets_inner(
        &self,
        viewer: &UserId,
        requested: &ConversationId,
    ) -> Result<
        Result<AttachmentSenderFacets, ChannelAttachmentReadRefusal>,
        AttachmentReadStoreError,
    > {
        let mut tx = self.pool.begin_with("BEGIN DEFERRED").await?;
        if let Err(refusal) = require_member(&mut tx, viewer, requested).await? {
            tx.commit().await?;
            return Ok(Err(refusal));
        }
        let rows = sqlx::query(FACETS_SQL)
            .bind(requested.as_str())
            .fetch_all(&mut *tx)
            .await?;
        let mut facets = Vec::with_capacity(rows.len());
        for row in &rows {
            match decode_actor(row)? {
                AttachmentSourceActor::Human(Some(id)) => {
                    facets.push(AttachmentSenderFacet::Human(id));
                }
                AttachmentSourceActor::Agent(Some(id)) => {
                    facets.push(AttachmentSenderFacet::Agent(id));
                }
                AttachmentSourceActor::System => facets.push(AttachmentSenderFacet::System),
                AttachmentSourceActor::Human(None) | AttachmentSourceActor::Agent(None) => {}
            }
        }
        let fact = AttachmentSenderFacets::projected(viewer.clone(), requested.clone(), facets);
        tx.commit().await?;
        Ok(Ok(fact))
    }

    async fn download_inner(
        &self,
        viewer: &UserId,
        requested: &AttachmentId,
    ) -> Result<Result<AttachmentDownloadFact, AttachmentDownloadRefusal>, AttachmentReadStoreError>
    {
        let mut tx = self.pool.begin_with("BEGIN DEFERRED").await?;
        let Some(record) = load_attachment(&mut tx, requested).await? else {
            tx.commit().await?;
            return Ok(Err(AttachmentDownloadRefusal));
        };
        let row = load_channel_row(&mut tx, record.channel())
            .await?
            .ok_or(AttachmentReadStoreError::MissingConversation)?;
        let human_members = load_human_members(&mut tx, record.channel()).await?;
        let (member, public_candidate) =
            viewer_admission(&mut tx, viewer, &row, &human_members).await?;

        let fact = if member {
            AttachmentDownloadFact::member(viewer.clone(), requested.clone(), record)
                .map_err(|_| AttachmentReadStoreError::Coordinate)?
        } else if public_candidate {
            let reference = find_channel_reference(&mut tx, requested, row.conversation()).await?;
            let Some((referenced_attachment, referenced_conversation)) = reference else {
                tx.commit().await?;
                return Ok(Err(AttachmentDownloadRefusal));
            };
            AttachmentDownloadFact::public_persisted_reference(
                viewer.clone(),
                requested.clone(),
                record,
                &referenced_attachment,
                &referenced_conversation,
            )
            .map_err(|_| AttachmentReadStoreError::Coordinate)?
        } else {
            tx.commit().await?;
            return Ok(Err(AttachmentDownloadRefusal));
        };
        tx.commit().await?;
        Ok(Ok(fact))
    }
}

pub(super) async fn require_member(
    tx: &mut Transaction<'_, Sqlite>,
    viewer: &UserId,
    requested: &ConversationId,
) -> Result<Result<(), ChannelAttachmentReadRefusal>, AttachmentReadStoreError> {
    let Some(row) = load_channel_row(tx, requested).await? else {
        return Ok(Err(ChannelAttachmentReadRefusal::NotFound));
    };
    let human_members = load_human_members(tx, requested).await?;
    let (member, _) = viewer_admission(tx, viewer, &row, &human_members).await?;
    if member {
        Ok(Ok(()))
    } else {
        Ok(Err(ChannelAttachmentReadRefusal::Forbidden))
    }
}

async fn viewer_admission(
    tx: &mut Transaction<'_, Sqlite>,
    viewer: &UserId,
    row: &ChannelLifecycleRow,
    human_members: &[UserId],
) -> Result<(bool, bool), AttachmentReadStoreError> {
    match row.placement() {
        ChannelLifecyclePlacement::Workspace(workspace) => {
            let workspace_member = load_workspace_role(tx, viewer, workspace.workspace())
                .await?
                .is_some();
            let seated = human_members.iter().any(|candidate| candidate == viewer);
            let member = workspace_member && seated;
            let public_candidate = workspace_member
                && workspace.roster() == DirectoryRoster::Open
                && row.visibility() == ChannelVisibility::Public
                && row.archived_at().is_none();
            Ok((member, public_candidate))
        }
        ChannelLifecyclePlacement::DirectPair(participants) => {
            let identity = PrincipalIdentity::Human(viewer.clone());
            Ok((
                participants.first() == &identity || participants.second() == &identity,
                false,
            ))
        }
    }
}

fn sender_binds(
    filter: Option<&AttachmentSenderFilter>,
) -> (Option<&'static str>, Option<&str>, Option<&str>) {
    match filter {
        None => (None, None, None),
        Some(AttachmentSenderFilter::Human(id)) => (Some("user"), Some(id.as_str()), None),
        Some(AttachmentSenderFilter::Agent(id)) => (Some("agent"), None, Some(id.as_str())),
        Some(AttachmentSenderFilter::System) => (Some("system"), None, None),
    }
}

struct DecodedListRow {
    referenced_attachment: AttachmentId,
    attachment: AttachmentRecord,
    message: MessageId,
    conversation: ConversationId,
    thread_root: Option<ThreadRootId>,
    message_seq: MessageSeq,
    actor: AttachmentSourceActor,
    appeared_at: JsSafeInteger,
    author_evidence: AuthorEvidence,
}

impl DecodedListRow {
    fn project(
        self,
        author: Option<silverhand_backend_core::conversation_messaging::messaging::MessageAuthor>,
        requested: &ConversationId,
    ) -> Result<AttachmentListItem, AttachmentReadStoreError> {
        let source = AttachmentListSource::projected_safe(
            self.message,
            self.conversation,
            self.thread_root,
            self.message_seq,
            author,
            self.actor,
            self.appeared_at,
        )
        .map_err(|_| AttachmentReadStoreError::Source)?;
        AttachmentListItem::projected(
            self.referenced_attachment,
            self.attachment,
            source,
            requested,
        )
        .map_err(|_| AttachmentReadStoreError::Coordinate)
    }
}

fn decode_list_row(
    row: &SqliteRow,
    requested: &ConversationId,
) -> Result<DecodedListRow, AttachmentReadStoreError> {
    let referenced_attachment = attachment_id(row.try_get("attachment_id")?, "refs.attachment_id")?;
    let message = message_id(row.try_get("message_id")?, "messages.id")?;
    let conversation = conversation_id(
        row.try_get("message_conversation_id")?,
        "messages.conversation_id",
    )?;
    if &conversation != requested {
        return Err(AttachmentReadStoreError::Coordinate);
    }
    let thread_root = optional_thread_root(row.try_get("thread_root_id")?)?;
    let message_seq = MessageSeq::try_from(row.try_get::<i64, _>("message_seq")?)
        .map_err(|_| AttachmentReadStoreError::MessageSeq)?;
    let actor = decode_actor(row)?;
    let appeared_at = wire_nonnegative(row.try_get("appeared_at")?, "messages.created_at")?;
    let from_kind = from_kind(row.try_get::<String, _>("from_kind")?.as_str())?;
    let author_evidence = AuthorEvidence {
        from_kind,
        from_agent_id: row.try_get("from_agent_id")?,
        from_user_id: row.try_get("from_user_id")?,
        author_id: row.try_get("author_id")?,
        author_name: row.try_get("author_name")?,
        author_display_name: row.try_get("author_display_name")?,
    };
    let attachment = decode_attachment(row)?;
    Ok(DecodedListRow {
        referenced_attachment,
        attachment,
        message,
        conversation,
        thread_root,
        message_seq,
        actor,
        appeared_at,
        author_evidence,
    })
}

/// Loads one attachment row by id, strictly decoded. `pub(crate)`: the agent-audit
/// attachment face reuses this exact decode, then binds the row to its ONE audited
/// conversation by comparing [`AttachmentRecord::channel`] — a row in another
/// conversation is treated as absent, and the blob store is never touched here.
pub(crate) async fn load_attachment(
    tx: &mut Transaction<'_, Sqlite>,
    requested: &AttachmentId,
) -> Result<Option<AttachmentRecord>, AttachmentReadStoreError> {
    let raw = sqlx::query(ATTACHMENT_SQL)
        .bind(requested.as_str())
        .fetch_optional(&mut **tx)
        .await?;
    raw.map(|row| {
        let record = decode_attachment(&row)?;
        if record.id() != requested {
            return Err(AttachmentReadStoreError::Coordinate);
        }
        Ok(record)
    })
    .transpose()
}

fn decode_attachment(row: &SqliteRow) -> Result<AttachmentRecord, AttachmentReadStoreError> {
    let id = attachment_id(row.try_get("a_id")?, "attachments.id")?;
    let channel = conversation_id(row.try_get("a_channel_id")?, "attachments.channel_id")?;
    let source_conversation = row
        .try_get::<Option<String>, _>("a_source_conversation_id")?
        .map(|raw| conversation_id(raw, "attachments.source_conversation_id"))
        .transpose()?;
    let filename = AttachmentFilename::parse(row.try_get::<String, _>("a_filename")?)
        .map_err(|_| AttachmentReadStoreError::Filename)?;
    let content_type = AttachmentContentType::parse(row.try_get::<String, _>("a_content_type")?)
        .map_err(|_| AttachmentReadStoreError::ContentType)?;
    let storage = StorageKind::parse(row.try_get::<String, _>("a_storage")?.as_str())
        .map_err(|_| AttachmentReadStoreError::StorageKind)?;
    let key = BlobStorageKey::parse(row.try_get::<String, _>("a_storage_key")?)
        .map_err(|_| AttachmentReadStoreError::StorageKey)?;
    let creator_kind: String = row.try_get("a_created_by_kind")?;
    let creator_id: String = row.try_get("a_created_by_id")?;
    let creator = match creator_kind.as_str() {
        "user" => AttachmentCreator::Human(
            UserId::parse(creator_id).map_err(|_| AttachmentReadStoreError::Creator)?,
        ),
        "agent" => AttachmentCreator::Agent(
            AgentId::parse(creator_id).map_err(|_| AttachmentReadStoreError::Creator)?,
        ),
        _ => return Err(AttachmentReadStoreError::Creator),
    };
    let size = wire_nonnegative(row.try_get("a_size")?, "attachments.size")?;
    let content_revision = wire_positive(
        row.try_get("a_content_revision")?,
        "attachments.content_revision",
    )?;
    let created_at = wire_nonnegative(row.try_get("a_created_at")?, "attachments.created_at")?;
    AttachmentRecord::projected_safe(
        id,
        channel,
        source_conversation,
        filename,
        size,
        content_type,
        StoredBlob::new(storage, key),
        content_revision,
        creator,
        created_at,
    )
    .map_err(|_| AttachmentReadStoreError::Attachment)
}

fn decode_actor(row: &SqliteRow) -> Result<AttachmentSourceActor, AttachmentReadStoreError> {
    let kind: String = row.try_get("from_kind")?;
    let user = row
        .try_get::<Option<String>, _>("from_user_id")?
        .map(|raw| UserId::parse(raw).map_err(|_| AttachmentReadStoreError::Sender))
        .transpose()?;
    let agent = row
        .try_get::<Option<String>, _>("from_agent_id")?
        .map(|raw| AgentId::parse(raw).map_err(|_| AttachmentReadStoreError::Sender))
        .transpose()?;
    match (kind.as_str(), user, agent) {
        ("user", user, None) => Ok(AttachmentSourceActor::Human(user)),
        ("agent", None, agent) => Ok(AttachmentSourceActor::Agent(agent)),
        ("system", None, None) => Ok(AttachmentSourceActor::System),
        _ => Err(AttachmentReadStoreError::Sender),
    }
}

async fn find_channel_reference(
    tx: &mut Transaction<'_, Sqlite>,
    requested: &AttachmentId,
    channel: &ConversationId,
) -> Result<Option<(AttachmentId, ConversationId)>, AttachmentReadStoreError> {
    let rows = sqlx::query(CHANNEL_REFERENCES_SQL)
        .bind(channel.as_str())
        .bind(requested.as_str())
        .fetch_all(&mut **tx)
        .await?;
    let mut found = None;
    for row in &rows {
        let source = conversation_id(row.try_get("conversation_id")?, "messages.conversation_id")?;
        if &source != channel {
            return Err(AttachmentReadStoreError::Coordinate);
        }
        let Some(raw) = row.try_get::<Option<String>, _>("attachment_id")? else {
            continue;
        };
        if raw.is_empty() {
            continue;
        }
        let referenced = attachment_id(raw, "messages.metadata.attachments.id")?;
        if &referenced == requested {
            found = Some((referenced, source));
        }
    }
    Ok(found)
}

fn from_kind(raw: &str) -> Result<FromKind, AttachmentReadStoreError> {
    match raw {
        "user" => Ok(FromKind::User),
        "agent" => Ok(FromKind::Agent),
        "system" => Ok(FromKind::System),
        _ => Err(AttachmentReadStoreError::Sender),
    }
}

fn wire_nonnegative(
    value: i64,
    field: &'static str,
) -> Result<JsSafeInteger, AttachmentReadStoreError> {
    let value =
        JsSafeInteger::new(value).map_err(|_| AttachmentReadStoreError::Scalar { field })?;
    if value.get() >= 0 {
        Ok(value)
    } else {
        Err(AttachmentReadStoreError::Scalar { field })
    }
}

fn wire_positive(
    value: i64,
    field: &'static str,
) -> Result<JsSafeInteger, AttachmentReadStoreError> {
    let value =
        JsSafeInteger::new(value).map_err(|_| AttachmentReadStoreError::Scalar { field })?;
    if value.get() >= 1 {
        Ok(value)
    } else {
        Err(AttachmentReadStoreError::Scalar { field })
    }
}

fn attachment_id(
    raw: String,
    field: &'static str,
) -> Result<AttachmentId, AttachmentReadStoreError> {
    AttachmentId::parse(raw).map_err(|_| AttachmentReadStoreError::Identity { field })
}

fn message_id(raw: String, field: &'static str) -> Result<MessageId, AttachmentReadStoreError> {
    MessageId::parse(raw).map_err(|_| AttachmentReadStoreError::Identity { field })
}

fn conversation_id(
    raw: String,
    field: &'static str,
) -> Result<ConversationId, AttachmentReadStoreError> {
    ConversationId::parse(raw).map_err(|_| AttachmentReadStoreError::Identity { field })
}

fn optional_thread_root(
    raw: Option<String>,
) -> Result<Option<ThreadRootId>, AttachmentReadStoreError> {
    raw.map(|value| {
        ThreadRootId::parse(value).map_err(|_| AttachmentReadStoreError::Identity {
            field: "messages.thread_root_id",
        })
    })
    .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::{Executor, sqlite::SqlitePoolOptions};

    async fn store() -> SqliteAttachmentRead {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        pool.execute(
            "CREATE TABLE agents(\
               id TEXT PRIMARY KEY,name TEXT NOT NULL DEFAULT '',display_name TEXT NOT NULL DEFAULT '',\
               avatar_storage_key TEXT,updated_at INTEGER NOT NULL DEFAULT 0,runtime TEXT NOT NULL DEFAULT 'codex'\
             );\
             CREATE TABLE users(\
               id TEXT PRIMARY KEY,name TEXT NOT NULL DEFAULT '',display_name TEXT NOT NULL DEFAULT '',\
               avatar_storage_key TEXT,updated_at INTEGER NOT NULL DEFAULT 0\
             );\
             CREATE TABLE conversations(\
               id TEXT PRIMARY KEY,workspace_id TEXT,placement_kind TEXT NOT NULL,\
               roster_policy TEXT NOT NULL,title TEXT NOT NULL,title_key TEXT,status TEXT NOT NULL,\
               spawned_by_agent_id TEXT,closed_at INTEGER,close_summary TEXT,archived_at INTEGER,\
               latest_message_seq INTEGER NOT NULL DEFAULT 0,created_at INTEGER NOT NULL,\
               updated_at INTEGER NOT NULL,visibility TEXT NOT NULL DEFAULT 'private',\
               endpoint_a_kind TEXT,endpoint_a_id TEXT,endpoint_a_deleted_at INTEGER,\
               endpoint_b_kind TEXT,endpoint_b_id TEXT,endpoint_b_deleted_at INTEGER\
             );\
             CREATE TABLE workspace_users(\
               workspace_id TEXT NOT NULL,user_id TEXT NOT NULL,role TEXT NOT NULL\
             );\
             CREATE TABLE conversation_users(\
               conversation_id TEXT NOT NULL,user_id TEXT NOT NULL,joined_at INTEGER NOT NULL\
             );\
             CREATE TABLE attachments(\
               id TEXT PRIMARY KEY,channel_id TEXT NOT NULL,source_conversation_id TEXT,\
               filename TEXT NOT NULL,size INTEGER NOT NULL,content_type TEXT NOT NULL,\
               storage TEXT NOT NULL,storage_key TEXT NOT NULL,created_by_kind TEXT NOT NULL,\
               created_by_id TEXT NOT NULL,created_at INTEGER NOT NULL,\
               content_revision INTEGER NOT NULL DEFAULT 1\
             );\
             CREATE TABLE messages(\
               id TEXT PRIMARY KEY,conversation_id TEXT NOT NULL,thread_root_id TEXT,\
               seq INTEGER NOT NULL,from_kind TEXT NOT NULL,from_user_id TEXT,from_agent_id TEXT,\
               author_id TEXT,author_name TEXT,author_display_name TEXT,metadata TEXT NOT NULL,\
               created_at INTEGER NOT NULL\
             );",
        )
        .await
        .unwrap();
        SqliteAttachmentRead { pool: pool.into() }
    }

    async fn workspace_channel(
        store: &SqliteAttachmentRead,
        channel: &str,
        visibility: &str,
        archived_at: Option<i64>,
    ) {
        sqlx::query(
            "INSERT INTO conversations(\
               id,workspace_id,placement_kind,roster_policy,title,title_key,status,archived_at,\
               created_at,updated_at,visibility\
             ) VALUES(?,'workspace-1','workspace','open','General','general','idle',?,1,1,?)",
        )
        .bind(channel)
        .bind(archived_at)
        .bind(visibility)
        .execute(&store.pool)
        .await
        .unwrap();
    }

    async fn add_workspace_user(store: &SqliteAttachmentRead, user: &str) {
        sqlx::query("INSERT INTO users(id,name,display_name) VALUES(?,?,?)")
            .bind(user)
            .bind(user)
            .bind(user)
            .execute(&store.pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO workspace_users(workspace_id,user_id,role) VALUES('workspace-1',?,'member')",
        )
        .bind(user)
        .execute(&store.pool)
        .await
        .unwrap();
    }

    async fn seat(store: &SqliteAttachmentRead, channel: &str, user: &str) {
        sqlx::query(
            "INSERT INTO conversation_users(conversation_id,user_id,joined_at) VALUES(?,?,1)",
        )
        .bind(channel)
        .bind(user)
        .execute(&store.pool)
        .await
        .unwrap();
    }

    async fn attachment(store: &SqliteAttachmentRead, id: &str, channel: &str, storage: &str) {
        sqlx::query(
            "INSERT INTO attachments(\
               id,channel_id,filename,size,content_type,storage,storage_key,\
               created_by_kind,created_by_id,created_at\
             ) VALUES(?,?,'note.txt',4,'text/plain',?,'attachments/blob.bin','user','member',1)",
        )
        .bind(id)
        .bind(channel)
        .bind(storage)
        .execute(&store.pool)
        .await
        .unwrap();
    }

    async fn system_message(
        store: &SqliteAttachmentRead,
        id: &str,
        channel: &str,
        seq: i64,
        appeared_at: i64,
        attachment_ids: &[&str],
    ) {
        let metadata = serde_json::json!({
            "attachments": attachment_ids
                .iter()
                .map(|id| serde_json::json!({"id": id}))
                .collect::<Vec<_>>()
        });
        sqlx::query(
            "INSERT INTO messages(\
               id,conversation_id,seq,from_kind,metadata,created_at\
             ) VALUES(?,?,?,'system',?,?)",
        )
        .bind(id)
        .bind(channel)
        .bind(seq)
        .bind(metadata.to_string())
        .bind(appeared_at)
        .execute(&store.pool)
        .await
        .unwrap();
    }

    fn viewer() -> UserId {
        UserId::parse("member").unwrap()
    }

    fn channel() -> ConversationId {
        ConversationId::parse("channel-1").unwrap()
    }

    #[tokio::test]
    async fn list_uses_earliest_canonical_reference_and_descending_keyset() {
        let store = store().await;
        workspace_channel(&store, "channel-1", "private", None).await;
        add_workspace_user(&store, "member").await;
        seat(&store, "channel-1", "member").await;
        attachment(&store, "a", "channel-1", "local").await;
        attachment(&store, "b", "channel-1", "local").await;
        system_message(&store, "m-z", "channel-1", 1, 5, &["a"]).await;
        system_message(&store, "m-a", "channel-1", 2, 5, &["a"]).await;
        system_message(&store, "m-b", "channel-1", 3, 20, &["b"]).await;

        let first_query = AttachmentListQuery::new(1, None, None, None, None).unwrap();
        let first = store
            .list(&viewer(), &channel(), &first_query)
            .await
            .unwrap()
            .unwrap()
            .bind(&viewer(), &channel(), &first_query)
            .unwrap();
        assert_eq!(first.items()[0].attachment().id().as_str(), "b");
        let cursor = first.next_cursor().cloned().unwrap();

        let second_query = AttachmentListQuery::new(1, Some(cursor), None, None, None).unwrap();
        let second = store
            .list(&viewer(), &channel(), &second_query)
            .await
            .unwrap()
            .unwrap()
            .bind(&viewer(), &channel(), &second_query)
            .unwrap();
        assert_eq!(second.items()[0].attachment().id().as_str(), "a");
        assert_eq!(second.items()[0].source().message().as_str(), "m-a");
        assert!(second.next_cursor().is_none());

        let facets = store
            .sender_facets(&viewer(), &channel())
            .await
            .unwrap()
            .unwrap()
            .bind(&viewer(), &channel())
            .unwrap();
        assert_eq!(facets.senders(), &[AttachmentSenderFacet::System]);
    }

    #[tokio::test]
    async fn list_strictly_decodes_limit_sentinel_before_trimming() {
        let store = store().await;
        workspace_channel(&store, "channel-1", "private", None).await;
        add_workspace_user(&store, "member").await;
        seat(&store, "channel-1", "member").await;
        attachment(&store, "good", "channel-1", "local").await;
        attachment(&store, "bad", "channel-1", "unknown").await;
        system_message(&store, "m-good", "channel-1", 1, 20, &["good"]).await;
        system_message(&store, "m-bad", "channel-1", 2, 10, &["bad"]).await;

        let query = AttachmentListQuery::new(1, None, None, None, None).unwrap();
        assert!(store.list(&viewer(), &channel(), &query).await.is_err());
    }

    #[tokio::test]
    async fn unsafe_canonical_timestamp_is_named_corruption_and_repair_restores_page() {
        let store = store().await;
        workspace_channel(&store, "channel-1", "private", None).await;
        add_workspace_user(&store, "member").await;
        seat(&store, "channel-1", "member").await;
        attachment(&store, "a", "channel-1", "local").await;
        system_message(&store, "m", "channel-1", 1, JsSafeInteger::MAX + 1, &["a"]).await;
        let query = AttachmentListQuery::new(1, None, None, None, None).unwrap();

        assert!(matches!(
            store.list_inner(&viewer(), &channel(), &query).await,
            Err(AttachmentReadStoreError::Scalar {
                field: "messages.created_at"
            })
        ));
        sqlx::query("UPDATE messages SET created_at=8 WHERE id='m'")
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(
            store
                .list_inner(&viewer(), &channel(), &query)
                .await
                .unwrap()
                .is_ok()
        );
    }

    #[tokio::test]
    async fn refusal_precedes_malformed_attachment_projection() {
        let store = store().await;
        workspace_channel(&store, "channel-1", "private", None).await;
        add_workspace_user(&store, "member").await;
        attachment(&store, "bad", "channel-1", "unknown").await;
        system_message(&store, "m-bad", "channel-1", 1, 10, &["bad"]).await;
        let query = AttachmentListQuery::new(50, None, None, None, None).unwrap();

        assert_eq!(
            store.list(&viewer(), &channel(), &query).await.unwrap(),
            Err(ChannelAttachmentReadRefusal::Forbidden)
        );
    }

    #[tokio::test]
    async fn public_download_requires_same_channel_persisted_reference() {
        let store = store().await;
        workspace_channel(&store, "channel-1", "public", None).await;
        add_workspace_user(&store, "member").await;
        attachment(&store, "a", "channel-1", "local").await;
        system_message(&store, "foreign-ref", "foreign", 1, 5, &["a"]).await;
        let requested = AttachmentId::parse("a").unwrap();

        assert_eq!(
            store.download(&viewer(), &requested).await.unwrap(),
            Err(AttachmentDownloadRefusal)
        );

        system_message(&store, "unrelated-bad-ref", "channel-1", 1, 5, &[""]).await;
        system_message(&store, "local-ref", "channel-1", 1, 6, &["a"]).await;
        let fact = store
            .download(&viewer(), &requested)
            .await
            .unwrap()
            .unwrap()
            .bind(&viewer(), &requested)
            .unwrap();
        assert!(fact.is_public_reference());
        assert_eq!(fact.record().channel().as_str(), "channel-1");
    }

    #[tokio::test]
    async fn facets_exclude_cross_shaped_actor_and_keep_legal_canonical_actor() {
        let store = store().await;
        workspace_channel(&store, "channel-1", "private", None).await;
        add_workspace_user(&store, "member").await;
        seat(&store, "channel-1", "member").await;
        attachment(&store, "a", "channel-1", "local").await;
        attachment(&store, "b", "channel-1", "local").await;
        sqlx::query(
            "INSERT INTO messages(\
               id,conversation_id,seq,from_kind,from_user_id,from_agent_id,metadata,created_at\
             ) VALUES('m','channel-1',1,'user','member','agent-splice',?,5)",
        )
        .bind(serde_json::json!({"attachments":[{"id":"a"}]}).to_string())
        .execute(&store.pool)
        .await
        .unwrap();
        system_message(&store, "legal", "channel-1", 2, 6, &["b"]).await;

        let facets = store
            .sender_facets(&viewer(), &channel())
            .await
            .unwrap()
            .unwrap()
            .bind(&viewer(), &channel())
            .unwrap();
        assert_eq!(facets.senders(), &[AttachmentSenderFacet::System]);
    }

    #[tokio::test]
    async fn unsafe_attachment_scalars_are_named_and_repair_restores_download() {
        let store = store().await;
        workspace_channel(&store, "channel-1", "private", None).await;
        add_workspace_user(&store, "member").await;
        seat(&store, "channel-1", "member").await;
        attachment(&store, "a", "channel-1", "local").await;
        let requested = AttachmentId::parse("a").unwrap();

        for (sql, field) in [
            (
                "UPDATE attachments SET size=? WHERE id='a'",
                "attachments.size",
            ),
            (
                "UPDATE attachments SET content_revision=? WHERE id='a'",
                "attachments.content_revision",
            ),
            (
                "UPDATE attachments SET created_at=? WHERE id='a'",
                "attachments.created_at",
            ),
        ] {
            sqlx::query(sql)
                .bind(JsSafeInteger::MAX + 1)
                .execute(&store.pool)
                .await
                .unwrap();
            assert!(matches!(
                store.download_inner(&viewer(), &requested).await,
                Err(AttachmentReadStoreError::Scalar { field: actual }) if actual == field
            ));
            sqlx::query(sql)
                .bind(1_i64)
                .execute(&store.pool)
                .await
                .unwrap();
        }
        assert!(
            store
                .download_inner(&viewer(), &requested)
                .await
                .unwrap()
                .is_ok()
        );
    }
}
