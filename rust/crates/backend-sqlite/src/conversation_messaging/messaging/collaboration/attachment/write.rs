//! Writer-reserved SQLite attachment units of work.
//!
//! Each operation reserves SQLite's writer before its first authority read.
//! Refusals are therefore based on the row, membership, creator, reference set,
//! and revision that the same transaction either mutates or leaves untouched.

use serde_json::Value;
use silverhand_backend_core::{
    conversation_messaging::messaging::{
        AttachmentId, PrincipalIdentity,
        collaboration::{
            attachment::{
                AttachmentContentUpdateCommand, AttachmentContentUpdateRefusal,
                AttachmentContentUpdateUnitOfWork, AttachmentContentUpdatedFact,
                AttachmentCreateAdmission, AttachmentCreateCommand, AttachmentCreateRefusal,
                AttachmentCreateUnitOfWork, AttachmentCreatedFact, AttachmentCreator,
                AttachmentDeleteCommand, AttachmentDeleteRefusal, AttachmentDeleteUnitOfWork,
                AttachmentDeletedFact, AttachmentMutationAdmission, AttachmentWriteAdmissionPort,
                AttachmentWriteAdmissionRefusal, AttachmentWriteAuthority,
                ChannelAttachmentReadRefusal,
            },
            channel_lifecycle::ChannelLifecyclePlacement,
        },
    },
    foundation::identity::{ConversationId, UserId},
    foundation::persistence::PersistenceFailure,
    foundation::wire_number::JsSafeInteger,
};
use sqlx::{Row, Sqlite, Transaction};
use thiserror::Error;

use crate::conversation_messaging::messaging::{
    MessagingStore,
    collaboration::{
        attachment::read::{AttachmentReadStoreError, load_attachment},
        channel_lifecycle::{
            ChannelLifecycleStoreError, load_channel_row, load_human_members,
            load_workspace_role_fact,
        },
    },
};

const INSERT_ATTACHMENT: &str = "INSERT INTO attachments(\
    id,channel_id,source_conversation_id,filename,size,content_type,storage,storage_key,\
    created_by_kind,created_by_id,created_at,content_revision)\
    VALUES(?,?,?,?,?,?,?,?,?,?,?,?)";

const UPDATE_CONTENT: &str = r#"UPDATE attachments SET storage_key=?,size=?,content_revision=?
    WHERE id=? AND storage=? AND storage_key=? AND content_revision=?"#;

const DELETE_ATTACHMENT: &str = r#"DELETE FROM attachments
    WHERE id=? AND storage=? AND storage_key=? AND content_revision=?"#;

const ALL_MESSAGE_METADATA: &str =
    "SELECT rowid AS message_rowid,metadata FROM messages ORDER BY rowid ASC";

const UPLOAD_PLACEMENT_LIVENESS: &str = r#"SELECT id,placement_kind,
    endpoint_a_deleted_at,endpoint_b_deleted_at,
    CASE endpoint_a_kind
      WHEN 'human' THEN EXISTS(SELECT 1 FROM users WHERE id=endpoint_a_id)
      WHEN 'agent' THEN EXISTS(SELECT 1 FROM agents WHERE id=endpoint_a_id)
      ELSE 0 END AS endpoint_a_exists,
    CASE endpoint_b_kind
      WHEN 'human' THEN EXISTS(SELECT 1 FROM users WHERE id=endpoint_b_id)
      WHEN 'agent' THEN EXISTS(SELECT 1 FROM agents WHERE id=endpoint_b_id)
      ELSE 0 END AS endpoint_b_exists
    FROM conversations WHERE id=?"#;

#[derive(Clone)]
pub struct SqliteAttachmentWrite {
    pool: crate::foundation::database::SqliteDatabase,
}

impl<E> MessagingStore<E> {
    pub fn attachment_writes(&self) -> SqliteAttachmentWrite {
        SqliteAttachmentWrite {
            pool: self.pool.clone(),
        }
    }
}

#[derive(Debug, Error)]
pub enum AttachmentWriteStoreError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("attachment read authority failed")]
    Read(#[from] AttachmentReadStoreError),
    #[error("attachment write authority snapshot is malformed")]
    Authority(#[from] ChannelLifecycleStoreError),
    #[error("attachment row points at a missing owning conversation")]
    MissingConversation,
    #[error("replacement bytes were validated against a different locked content type")]
    LockedContentTypeChanged,
    #[error("replacement blob storage differs from the committed attachment storage")]
    ReplacementStorageChanged,
    #[error("replacement blob key is not fresh")]
    ReplacementKeyReused,
    #[error("attachment content revision reached the exact JSON integer ceiling")]
    RevisionExhausted,
    #[error("the selected row changed inside the writer-reserved unit")]
    Coordinate,
    #[error("a committed attachment fact could not seal its complete row")]
    Fact,
    #[error("message row {row} has malformed attachment metadata")]
    MalformedMessageMetadata { row: i64 },
    #[error("message row {row} has a malformed attachment reference")]
    MalformedMessageAttachmentReference { row: i64 },
    #[error("selected upload conversation has malformed placement liveness")]
    UploadPlacement,
}

impl From<AttachmentWriteStoreError> for PersistenceFailure {
    fn from(value: AttachmentWriteStoreError) -> Self {
        Self::from_source(value)
    }
}

impl AttachmentCreateUnitOfWork for SqliteAttachmentWrite {
    type Error = PersistenceFailure;

    async fn create(
        &self,
        command: AttachmentCreateCommand,
    ) -> Result<Result<AttachmentCreatedFact, AttachmentCreateRefusal>, Self::Error> {
        self.create_inner(command)
            .await
            .map_err(PersistenceFailure::from)
    }
}

impl AttachmentWriteAdmissionPort for SqliteAttachmentWrite {
    type Error = PersistenceFailure;

    async fn admit_create(
        &self,
        viewer: &UserId,
        conversation: &ConversationId,
    ) -> Result<Result<AttachmentCreateAdmission, AttachmentWriteAdmissionRefusal>, Self::Error>
    {
        self.admit_create_inner(viewer, conversation)
            .await
            .map_err(PersistenceFailure::from)
    }

    async fn admit_mutation(
        &self,
        viewer: &UserId,
        attachment: &AttachmentId,
    ) -> Result<Result<AttachmentMutationAdmission, AttachmentWriteAdmissionRefusal>, Self::Error>
    {
        self.admit_mutation_inner(viewer, attachment)
            .await
            .map_err(PersistenceFailure::from)
    }
}

impl AttachmentContentUpdateUnitOfWork for SqliteAttachmentWrite {
    type Error = PersistenceFailure;

    async fn update_content(
        &self,
        command: AttachmentContentUpdateCommand,
    ) -> Result<Result<AttachmentContentUpdatedFact, AttachmentContentUpdateRefusal>, Self::Error>
    {
        self.update_content_inner(command)
            .await
            .map_err(PersistenceFailure::from)
    }
}

impl AttachmentDeleteUnitOfWork for SqliteAttachmentWrite {
    type Error = PersistenceFailure;

    async fn delete(
        &self,
        command: AttachmentDeleteCommand,
    ) -> Result<Result<AttachmentDeletedFact, AttachmentDeleteRefusal>, Self::Error> {
        self.delete_inner(command)
            .await
            .map_err(PersistenceFailure::from)
    }
}

impl SqliteAttachmentWrite {
    /// The single reservation seam shared by every attachment mutation. Callers
    /// perform their first authority read only after this future resolves.
    async fn begin_write(&self) -> Result<Transaction<'_, Sqlite>, AttachmentWriteStoreError> {
        Ok(self.pool.begin_with("BEGIN IMMEDIATE").await?)
    }

    async fn admit_create_inner(
        &self,
        viewer: &UserId,
        conversation: &ConversationId,
    ) -> Result<
        Result<AttachmentCreateAdmission, AttachmentWriteAdmissionRefusal>,
        AttachmentWriteStoreError,
    > {
        let mut tx = self.pool.begin_with("BEGIN DEFERRED").await?;
        let result = match require_upload_authority(&mut tx, viewer, conversation).await? {
            Ok(authority) => Ok(AttachmentCreateAdmission::selected(authority)),
            Err(ChannelAttachmentReadRefusal::NotFound) => {
                Err(AttachmentWriteAdmissionRefusal::NotFound)
            }
            Err(ChannelAttachmentReadRefusal::Forbidden) => {
                Err(AttachmentWriteAdmissionRefusal::Forbidden)
            }
        };
        tx.commit().await?;
        Ok(result)
    }

    async fn admit_mutation_inner(
        &self,
        viewer: &UserId,
        attachment: &AttachmentId,
    ) -> Result<
        Result<AttachmentMutationAdmission, AttachmentWriteAdmissionRefusal>,
        AttachmentWriteStoreError,
    > {
        let mut tx = self.pool.begin_with("BEGIN DEFERRED").await?;
        let Some(record) = load_attachment(&mut tx, attachment).await? else {
            tx.commit().await?;
            return Ok(Err(AttachmentWriteAdmissionRefusal::NotFound));
        };
        let authority = match select_write_authority(&mut tx, viewer, record.channel()).await? {
            Ok(authority) if matches!(record.creator(), AttachmentCreator::Human(owner) if owner == authority.participant()) => {
                authority
            }
            Ok(_) | Err(ChannelAttachmentReadRefusal::Forbidden) => {
                tx.commit().await?;
                return Ok(Err(AttachmentWriteAdmissionRefusal::Forbidden));
            }
            Err(ChannelAttachmentReadRefusal::NotFound) => {
                return Err(AttachmentWriteStoreError::MissingConversation);
            }
        };
        let admission = AttachmentMutationAdmission::selected(authority, record)
            .map_err(|_| AttachmentWriteStoreError::Fact)?;
        tx.commit().await?;
        Ok(Ok(admission))
    }

    async fn create_inner(
        &self,
        command: AttachmentCreateCommand,
    ) -> Result<Result<AttachmentCreatedFact, AttachmentCreateRefusal>, AttachmentWriteStoreError>
    {
        let mut tx = self.begin_write().await?;
        let authority =
            match require_upload_authority(&mut tx, command.viewer(), command.requested()).await? {
                Ok(authority) => authority,
                Err(ChannelAttachmentReadRefusal::NotFound) => {
                    tx.commit().await?;
                    return Ok(Err(AttachmentCreateRefusal::NotFound));
                }
                Err(ChannelAttachmentReadRefusal::Forbidden) => {
                    tx.commit().await?;
                    return Ok(Err(AttachmentCreateRefusal::Forbidden));
                }
            };
        let record = command.record();
        insert_attachment_row(&mut tx, record).await?;
        let committed = load_attachment(&mut tx, record.id())
            .await?
            .ok_or(AttachmentWriteStoreError::Coordinate)?;
        let fact = AttachmentCreatedFact::committed(&command, &authority, committed)
            .map_err(|_| AttachmentWriteStoreError::Fact)?;
        tx.commit().await?;
        Ok(Ok(fact))
    }

    async fn update_content_inner(
        &self,
        command: AttachmentContentUpdateCommand,
    ) -> Result<
        Result<AttachmentContentUpdatedFact, AttachmentContentUpdateRefusal>,
        AttachmentWriteStoreError,
    > {
        let mut tx = self.begin_write().await?;
        let Some(before) = load_attachment(&mut tx, command.requested()).await? else {
            tx.commit().await?;
            return Ok(Err(AttachmentContentUpdateRefusal::NotFound));
        };
        let authority = match select_write_authority(&mut tx, command.viewer(), before.channel())
            .await?
        {
            Ok(authority) if matches!(before.creator(), AttachmentCreator::Human(owner) if owner == authority.participant()) => {
                authority
            }
            Ok(_) | Err(ChannelAttachmentReadRefusal::Forbidden) => {
                tx.commit().await?;
                return Ok(Err(AttachmentContentUpdateRefusal::Forbidden));
            }
            Err(ChannelAttachmentReadRefusal::NotFound) => {
                return Err(AttachmentWriteStoreError::MissingConversation);
            }
        };
        if before.content_type() != command.expected_content_type() {
            return Err(AttachmentWriteStoreError::LockedContentTypeChanged);
        }
        if before.blob().storage() != command.replacement_blob().storage() {
            return Err(AttachmentWriteStoreError::ReplacementStorageChanged);
        }
        if before.blob().key() == command.replacement_blob().key() {
            return Err(AttachmentWriteStoreError::ReplacementKeyReused);
        }
        let next_revision = before
            .content_revision()
            .get()
            .checked_add(1)
            .and_then(|value| JsSafeInteger::new(value).ok())
            .ok_or(AttachmentWriteStoreError::RevisionExhausted)?;
        if swap_attachment_content_row(
            &mut tx,
            &before,
            command.replacement_blob(),
            command.replacement_size(),
            next_revision,
        )
        .await?
            != 1
        {
            return Err(AttachmentWriteStoreError::Coordinate);
        }
        let after = load_attachment(&mut tx, command.requested())
            .await?
            .ok_or(AttachmentWriteStoreError::Coordinate)?;
        if after.blob() != command.replacement_blob()
            || after.size() != command.replacement_size()
            || after.content_revision() != next_revision
        {
            return Err(AttachmentWriteStoreError::Coordinate);
        }
        let fact = AttachmentContentUpdatedFact::committed(&command, &authority, before, after)
            .map_err(|_| AttachmentWriteStoreError::Fact)?;
        tx.commit().await?;
        Ok(Ok(fact))
    }

    async fn delete_inner(
        &self,
        command: AttachmentDeleteCommand,
    ) -> Result<Result<AttachmentDeletedFact, AttachmentDeleteRefusal>, AttachmentWriteStoreError>
    {
        let mut tx = self.begin_write().await?;
        let Some(selected) = load_attachment(&mut tx, command.requested()).await? else {
            tx.commit().await?;
            return Ok(Err(AttachmentDeleteRefusal::NotFound));
        };
        let authority = match select_write_authority(&mut tx, command.viewer(), selected.channel())
            .await?
        {
            Ok(authority) if matches!(selected.creator(), AttachmentCreator::Human(owner) if owner == authority.participant()) => {
                authority
            }
            Ok(_) | Err(ChannelAttachmentReadRefusal::Forbidden) => {
                tx.commit().await?;
                return Ok(Err(AttachmentDeleteRefusal::Forbidden));
            }
            Err(ChannelAttachmentReadRefusal::NotFound) => {
                return Err(AttachmentWriteStoreError::MissingConversation);
            }
        };
        if has_matching_reference(&mut tx, command.requested()).await? {
            tx.commit().await?;
            return Ok(Err(AttachmentDeleteRefusal::InUse));
        }
        let deleted = sqlx::query(DELETE_ATTACHMENT)
            .bind(command.requested().as_str())
            .bind(selected.blob().storage().as_str())
            .bind(selected.blob().key().as_str())
            .bind(selected.content_revision().get())
            .execute(&mut *tx)
            .await?;
        if deleted.rows_affected() != 1 {
            return Err(AttachmentWriteStoreError::Coordinate);
        }
        let fact = AttachmentDeletedFact::committed(&command, &authority, selected)
            .map_err(|_| AttachmentWriteStoreError::Fact)?;
        tx.commit().await?;
        Ok(Ok(fact))
    }
}

pub(super) async fn insert_attachment_row(
    tx: &mut Transaction<'_, Sqlite>,
    record: &silverhand_backend_core::conversation_messaging::messaging::collaboration::attachment::AttachmentRecord,
) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query(INSERT_ATTACHMENT)
        .bind(record.id().as_str())
        .bind(record.channel().as_str())
        .bind(record.source_conversation().map(ConversationId::as_str))
        .bind(record.filename().as_str())
        .bind(record.size().get())
        .bind(record.content_type().as_str())
        .bind(record.blob().storage().as_str())
        .bind(record.blob().key().as_str())
        .bind(record.creator().kind())
        .bind(record.creator().id())
        .bind(record.created_at().get())
        .bind(record.content_revision().get())
        .execute(&mut **tx)
        .await?
        .rows_affected())
}

pub(super) async fn swap_attachment_content_row(
    tx: &mut Transaction<'_, Sqlite>,
    before: &silverhand_backend_core::conversation_messaging::messaging::collaboration::attachment::AttachmentRecord,
    replacement: &silverhand_backend_core::foundation::blob_store::StoredBlob,
    replacement_size: JsSafeInteger,
    next_revision: JsSafeInteger,
) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query(UPDATE_CONTENT)
        .bind(replacement.key().as_str())
        .bind(replacement_size.get())
        .bind(next_revision.get())
        .bind(before.id().as_str())
        .bind(before.blob().storage().as_str())
        .bind(before.blob().key().as_str())
        .bind(before.content_revision().get())
        .execute(&mut **tx)
        .await?
        .rows_affected())
}

async fn select_write_authority(
    tx: &mut Transaction<'_, Sqlite>,
    viewer: &UserId,
    requested: &ConversationId,
) -> Result<Result<AttachmentWriteAuthority, ChannelAttachmentReadRefusal>, AttachmentWriteStoreError>
{
    let Some(channel) = load_channel_row(tx, requested).await? else {
        return Ok(Err(ChannelAttachmentReadRefusal::NotFound));
    };
    let authority =
        match channel.placement() {
            ChannelLifecyclePlacement::Workspace(workspace) => {
                let members = load_human_members(tx, requested).await?;
                let selected_seat = members.into_iter().find(|candidate| candidate == viewer);
                let selected_workspace_member =
                    load_workspace_role_fact(tx, viewer, workspace.workspace())
                        .await?
                        .map(|fact| fact.viewer);
                match (selected_seat, selected_workspace_member) {
                    (Some(seat), Some(workspace_member)) => Some(
                        AttachmentWriteAuthority::workspace(
                            channel.conversation().clone(),
                            seat,
                            workspace_member,
                        )
                        .map_err(|_| AttachmentWriteStoreError::Coordinate)?,
                    ),
                    _ => None,
                }
            }
            ChannelLifecyclePlacement::DirectPair(participants) => participants
                .both()
                .iter()
                .find_map(|endpoint| match endpoint {
                    PrincipalIdentity::Human(source) if source == viewer => {
                        Some(AttachmentWriteAuthority::direct_pair(
                            channel.conversation().clone(),
                            source.clone(),
                        ))
                    }
                    _ => None,
                }),
        };
    Ok(authority.ok_or(ChannelAttachmentReadRefusal::Forbidden))
}

async fn require_upload_authority(
    tx: &mut Transaction<'_, Sqlite>,
    viewer: &UserId,
    requested: &ConversationId,
) -> Result<Result<AttachmentWriteAuthority, ChannelAttachmentReadRefusal>, AttachmentWriteStoreError>
{
    let authority = select_write_authority(tx, viewer, requested).await?;
    let Ok(authority) = authority else {
        return Ok(authority);
    };
    let row = sqlx::query(UPLOAD_PLACEMENT_LIVENESS)
        .bind(requested.as_str())
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(AttachmentWriteStoreError::MissingConversation)?;
    let stored = ConversationId::parse(row.try_get::<String, _>("id")?)
        .map_err(|_| AttachmentWriteStoreError::UploadPlacement)?;
    if &stored != requested {
        return Err(AttachmentWriteStoreError::Coordinate);
    }
    match row.try_get::<String, _>("placement_kind")?.as_str() {
        "workspace" => Ok(Ok(authority)),
        "direct_pair" => {
            let a_deleted: Option<i64> = row.try_get("endpoint_a_deleted_at")?;
            let b_deleted: Option<i64> = row.try_get("endpoint_b_deleted_at")?;
            let a_exists: i64 = row.try_get("endpoint_a_exists")?;
            let b_exists: i64 = row.try_get("endpoint_b_exists")?;
            if a_deleted.is_some() || b_deleted.is_some() || a_exists != 1 || b_exists != 1 {
                Ok(Err(ChannelAttachmentReadRefusal::Forbidden))
            } else {
                Ok(Ok(authority))
            }
        }
        _ => Err(AttachmentWriteStoreError::UploadPlacement),
    }
}

async fn has_matching_reference(
    tx: &mut Transaction<'_, Sqlite>,
    requested: &AttachmentId,
) -> Result<bool, AttachmentWriteStoreError> {
    let rows = sqlx::query(ALL_MESSAGE_METADATA)
        .fetch_all(&mut **tx)
        .await?;
    let mut found = false;
    for row in &rows {
        let row_id: i64 = row.try_get("message_rowid")?;
        let Some(raw) = row.try_get::<Option<String>, _>("metadata")? else {
            continue;
        };
        let metadata: Value = serde_json::from_str(&raw)
            .map_err(|_| AttachmentWriteStoreError::MalformedMessageMetadata { row: row_id })?;
        let object = metadata
            .as_object()
            .ok_or(AttachmentWriteStoreError::MalformedMessageMetadata { row: row_id })?;
        let Some(attachments) = object.get("attachments") else {
            continue;
        };
        let attachments = attachments
            .as_array()
            .ok_or(AttachmentWriteStoreError::MalformedMessageMetadata { row: row_id })?;
        for reference in attachments {
            let raw_id = reference
                .as_object()
                .and_then(|value| value.get("id"))
                .and_then(Value::as_str)
                .ok_or(
                    AttachmentWriteStoreError::MalformedMessageAttachmentReference { row: row_id },
                )?;
            let referenced = AttachmentId::parse(raw_id).map_err(|_| {
                AttachmentWriteStoreError::MalformedMessageAttachmentReference { row: row_id }
            })?;
            found |= &referenced == requested;
        }
    }
    Ok(found)
}
