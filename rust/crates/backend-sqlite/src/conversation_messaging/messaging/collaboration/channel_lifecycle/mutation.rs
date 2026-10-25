//! Writer-reserved SQLite units for channel lifecycle mutations.
//!
//! The DEFERRED admission read is only an auth-first/body-late optimization.
//! Every operation starts `BEGIN IMMEDIATE`, strictly decodes the current row,
//! re-evaluates governance, writes (or decides its no-op), strictly decodes the
//! whole committed image and only then returns an operation-specific fact.

use silverhand_backend_core::{
    conversation_messaging::messaging::{
        MessageId, MessageSeq, SendClock, SendEntropy,
        collaboration::{
            channel_directory::{ChannelVisibility, DirectoryRoster},
            channel_lifecycle::{
                ChannelArchivePort, ChannelArchiveRefusal, ChannelMutationAdmissionPort,
                ChannelMutationAdmissionRefusal, ChannelMutationProjectionError, ChannelRenamePort,
                ChannelRenameRefusal, ChannelVisibilityPort, ChannelVisibilityRefusal,
                CommittedChannelArchive, CommittedChannelRename, CommittedChannelVisibility,
                CommittedRenameNotice, GovernedChannelTarget, RenameRecipient,
            },
        },
        conversation_title::CanonicalConversationTitle,
        environment::EnvironmentUnavailable,
    },
    foundation::identity::{AgentId, ConversationId, UserId, WorkspaceId},
    foundation::persistence::PersistenceFailure,
    identity_access::access::role_governs_workspace,
};
use sqlx::{Row, Sqlite, Transaction};
use thiserror::Error;

use crate::conversation_messaging::messaging::{
    MessagingStore, RESERVE_NEXT_SEQ, SELECT_LATEST_SEQ, bump_stream_head_on,
};

use super::{ChannelLifecycleStoreError, load_channel_row, load_workspace_role};

#[derive(Clone)]
pub struct SqliteChannelLifecycleMutation<E> {
    pool: crate::foundation::database::SqliteDatabase,
    environment: E,
}

impl<E: Clone> MessagingStore<E> {
    pub fn channel_lifecycle_mutation(&self) -> SqliteChannelLifecycleMutation<E> {
        SqliteChannelLifecycleMutation {
            pool: self.pool.clone(),
            environment: self.environment.clone(),
        }
    }
}

#[derive(Debug, Error)]
pub enum ChannelLifecycleMutationStoreError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("host clock or entropy failed")]
    Environment(#[from] EnvironmentUnavailable),
    #[error("stored channel row is malformed")]
    Row(#[from] ChannelLifecycleStoreError),
    #[error("stored channel mutation fact is malformed at {field}")]
    Corrupt { field: &'static str },
    #[error("committed channel mutation projection is inconsistent")]
    Projection(#[from] ChannelMutationProjectionError),
}

impl From<ChannelLifecycleMutationStoreError> for PersistenceFailure {
    fn from(value: ChannelLifecycleMutationStoreError) -> Self {
        Self::from_source(value)
    }
}

fn corrupt(field: &'static str) -> ChannelLifecycleMutationStoreError {
    ChannelLifecycleMutationStoreError::Corrupt { field }
}

#[derive(Clone)]
struct GovernedRow {
    workspace: WorkspaceId,
    row: silverhand_backend_core::conversation_messaging::messaging::collaboration::channel_lifecycle::ChannelLifecycleRow,
}

#[derive(Clone, Copy)]
enum GovernedRefusal {
    NotFound,
    Forbidden,
}

async fn governed(
    tx: &mut Transaction<'_, Sqlite>,
    viewer: &UserId,
    conversation: &ConversationId,
) -> Result<Result<GovernedRow, GovernedRefusal>, ChannelLifecycleMutationStoreError> {
    let Some(row) = load_channel_row(tx, conversation).await? else {
        return Ok(Err(GovernedRefusal::NotFound));
    };
    let Some(workspace) = row.workspace().cloned() else {
        return Ok(Err(GovernedRefusal::Forbidden));
    };
    let workspace_exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workspaces WHERE id=?)")
            .bind(workspace.as_str())
            .fetch_one(&mut **tx)
            .await?;
    if !workspace_exists {
        return Ok(Err(GovernedRefusal::Forbidden));
    }
    let role = load_workspace_role(tx, viewer, &workspace).await?;
    if !role_governs_workspace(role) {
        return Ok(Err(GovernedRefusal::Forbidden));
    }
    Ok(Ok(GovernedRow { workspace, row }))
}

impl<E> ChannelMutationAdmissionPort for SqliteChannelLifecycleMutation<E>
where
    E: Clone + Send + Sync + 'static,
{
    type Error = PersistenceFailure;

    async fn admit(
        &self,
        viewer: &UserId,
        conversation: &ConversationId,
    ) -> Result<Result<GovernedChannelTarget, ChannelMutationAdmissionRefusal>, Self::Error> {
        let mut tx = self
            .pool
            .begin_with("BEGIN DEFERRED")
            .await
            .map_err(ChannelLifecycleMutationStoreError::from)
            .map_err(PersistenceFailure::from)?;
        let answer = governed(&mut tx, viewer, conversation)
            .await
            .map_err(PersistenceFailure::from)?;
        tx.commit().await.map_err(|error| {
            PersistenceFailure::from(ChannelLifecycleMutationStoreError::Sql(error))
        })?;
        Ok(match answer {
            Ok(value) => Ok(GovernedChannelTarget::resolved(
                viewer.clone(),
                conversation.clone(),
                value.workspace,
            )),
            Err(GovernedRefusal::NotFound) => Err(ChannelMutationAdmissionRefusal::NotFound),
            Err(GovernedRefusal::Forbidden) => Err(ChannelMutationAdmissionRefusal::Forbidden),
        })
    }
}

impl<E> ChannelArchivePort for SqliteChannelLifecycleMutation<E>
where
    E: SendClock + Clone + Send + Sync + 'static,
{
    type Error = PersistenceFailure;

    async fn set_archived(
        &self,
        viewer: UserId,
        conversation: ConversationId,
        archived: bool,
    ) -> Result<Result<CommittedChannelArchive, ChannelArchiveRefusal>, Self::Error> {
        self.archive_inner(viewer, conversation, archived)
            .await
            .map_err(PersistenceFailure::from)
    }
}

impl<E> SqliteChannelLifecycleMutation<E>
where
    E: SendClock + Clone + Send + Sync + 'static,
{
    async fn archive_inner(
        &self,
        viewer: UserId,
        conversation: ConversationId,
        archived: bool,
    ) -> Result<
        Result<CommittedChannelArchive, ChannelArchiveRefusal>,
        ChannelLifecycleMutationStoreError,
    > {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let authority = match governed(&mut tx, &viewer, &conversation).await? {
            Ok(value) => value,
            Err(GovernedRefusal::NotFound) => {
                tx.commit().await?;
                return Ok(Err(ChannelArchiveRefusal::NotFound));
            }
            Err(GovernedRefusal::Forbidden) => {
                tx.commit().await?;
                return Ok(Err(ChannelArchiveRefusal::Forbidden));
            }
        };
        let now = self.environment.now_unix_ms()?;
        let changed = sqlx::query("UPDATE conversations SET archived_at=?,updated_at=? WHERE id=?")
            .bind(archived.then_some(now))
            .bind(now)
            .bind(conversation.as_str())
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if changed != 1 {
            return Err(corrupt("conversations.id:archive_update"));
        }
        let row = load_channel_row(&mut tx, &conversation)
            .await?
            .ok_or_else(|| corrupt("conversations.id:archive_readback"))?;
        let committed =
            CommittedChannelArchive::committed(viewer, authority.workspace, row, archived)?;
        tx.commit().await?;
        Ok(Ok(committed))
    }
}

impl<E> ChannelVisibilityPort for SqliteChannelLifecycleMutation<E>
where
    E: SendClock + Clone + Send + Sync + 'static,
{
    type Error = PersistenceFailure;

    async fn set_visibility(
        &self,
        viewer: UserId,
        conversation: ConversationId,
        visibility: ChannelVisibility,
    ) -> Result<Result<CommittedChannelVisibility, ChannelVisibilityRefusal>, Self::Error> {
        self.visibility_inner(viewer, conversation, visibility)
            .await
            .map_err(PersistenceFailure::from)
    }
}

impl<E> SqliteChannelLifecycleMutation<E>
where
    E: SendClock + Clone + Send + Sync + 'static,
{
    async fn visibility_inner(
        &self,
        viewer: UserId,
        conversation: ConversationId,
        visibility: ChannelVisibility,
    ) -> Result<
        Result<CommittedChannelVisibility, ChannelVisibilityRefusal>,
        ChannelLifecycleMutationStoreError,
    > {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let authority = match governed(&mut tx, &viewer, &conversation).await? {
            Ok(value) => value,
            Err(GovernedRefusal::NotFound) => {
                tx.commit().await?;
                return Ok(Err(ChannelVisibilityRefusal::NotFound));
            }
            Err(GovernedRefusal::Forbidden) => {
                tx.commit().await?;
                return Ok(Err(ChannelVisibilityRefusal::Forbidden));
            }
        };
        if authority.row.roster() != DirectoryRoster::Open {
            tx.commit().await?;
            return Ok(Err(ChannelVisibilityRefusal::NotFound));
        }
        if authority.row.visibility() == visibility {
            let committed = CommittedChannelVisibility::unchanged(
                viewer,
                authority.workspace,
                authority.row,
                visibility,
            )?;
            tx.commit().await?;
            return Ok(Ok(committed));
        }
        let now = self.environment.now_unix_ms()?;
        let changed = sqlx::query(
            "UPDATE conversations SET visibility=?,updated_at=? \
             WHERE id=? AND placement_kind='workspace' AND roster_policy='open'",
        )
        .bind(visibility.as_str())
        .bind(now)
        .bind(conversation.as_str())
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if changed != 1 {
            return Err(corrupt("conversations.id:visibility_update"));
        }
        let row = load_channel_row(&mut tx, &conversation)
            .await?
            .ok_or_else(|| corrupt("conversations.id:visibility_readback"))?;
        let committed =
            CommittedChannelVisibility::changed(viewer, authority.workspace, row, visibility)?;
        tx.commit().await?;
        Ok(Ok(committed))
    }
}

impl<E> ChannelRenamePort for SqliteChannelLifecycleMutation<E>
where
    E: SendClock + SendEntropy + Clone + Send + Sync + 'static,
{
    type Error = PersistenceFailure;

    async fn rename(
        &self,
        viewer: UserId,
        conversation: ConversationId,
        requested: CanonicalConversationTitle,
    ) -> Result<Result<CommittedChannelRename, ChannelRenameRefusal>, Self::Error> {
        self.rename_inner(viewer, conversation, requested)
            .await
            .map_err(PersistenceFailure::from)
    }
}

impl<E> SqliteChannelLifecycleMutation<E>
where
    E: SendClock + SendEntropy + Clone + Send + Sync + 'static,
{
    async fn rename_inner(
        &self,
        viewer: UserId,
        conversation: ConversationId,
        requested: CanonicalConversationTitle,
    ) -> Result<
        Result<CommittedChannelRename, ChannelRenameRefusal>,
        ChannelLifecycleMutationStoreError,
    > {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let authority = match governed(&mut tx, &viewer, &conversation).await? {
            Ok(value) => value,
            Err(GovernedRefusal::NotFound) => {
                tx.commit().await?;
                return Ok(Err(ChannelRenameRefusal::NotFound));
            }
            Err(GovernedRefusal::Forbidden) => {
                tx.commit().await?;
                return Ok(Err(ChannelRenameRefusal::Forbidden));
            }
        };
        let old = CanonicalConversationTitle::parse(authority.row.title())
            .map_err(|_| corrupt("conversations.title:rename_old"))?;
        if authority.row.title_key() != Some(old.key()) {
            return Err(corrupt("conversations.title_key:rename_old"));
        }
        if old.title() == requested.title() {
            let committed = CommittedChannelRename::unchanged(
                viewer,
                authority.workspace,
                authority.row,
                requested,
            )?;
            tx.commit().await?;
            return Ok(Ok(committed));
        }

        let holders = sqlx::query(
            "SELECT id FROM conversations WHERE placement_kind='workspace' \
             AND workspace_id=? AND title_key=? AND id<>?",
        )
        .bind(authority.workspace.as_str())
        .bind(requested.key())
        .bind(conversation.as_str())
        .fetch_all(&mut *tx)
        .await?;
        for holder in &holders {
            ConversationId::parse(holder.try_get::<String, _>("id")?)
                .map_err(|_| corrupt("conversations.id:title_holder"))?;
        }
        if !holders.is_empty() {
            tx.commit().await?;
            return Ok(Err(ChannelRenameRefusal::TitleTaken));
        }

        let now = self.environment.now_unix_ms()?;
        let message = MessageId::mint(&self.environment)?;
        let changed =
            sqlx::query("UPDATE conversations SET title=?,title_key=?,updated_at=? WHERE id=?")
                .bind(requested.title())
                .bind(requested.key())
                .bind(now)
                .bind(conversation.as_str())
                .execute(&mut *tx)
                .await?
                .rows_affected();
        if changed != 1 {
            return Err(corrupt("conversations.id:rename_update"));
        }

        let reserved = sqlx::query(RESERVE_NEXT_SEQ)
            .bind(conversation.as_str())
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if reserved != 1 {
            return Err(corrupt("conversations.latest_message_seq:reserve"));
        }
        let seq = MessageSeq::try_from(
            sqlx::query_scalar::<_, i64>(SELECT_LATEST_SEQ)
                .bind(conversation.as_str())
                .fetch_one(&mut *tx)
                .await?,
        )
        .map_err(|_| corrupt("conversations.latest_message_seq"))?;
        let notice = CommittedRenameNotice::for_titles(
            conversation.clone(),
            message.clone(),
            seq,
            now,
            &old,
            &requested,
        );
        let metadata = serde_json::to_string(&serde_json::json!({
            "subtype": "rename",
            "old_name": old.title(),
            "new_name": requested.title(),
            "scope": "conversation",
        }))
        .map_err(|_| corrupt("messages.metadata:serialize"))?;
        sqlx::query(
            "INSERT INTO messages(id,conversation_id,from_kind,from_agent_id,from_user_id,content,metadata,seq,created_at,thread_root_id,author_id,author_name,author_display_name,cc_source_message_id) \
             VALUES(?,?,'system',NULL,NULL,?,?,?,?,NULL,NULL,NULL,NULL,NULL)",
        )
        .bind(message.as_str())
        .bind(conversation.as_str())
        .bind(notice.content())
        .bind(&metadata)
        .bind(seq.get())
        .bind(now)
        .execute(&mut *tx)
        .await?;
        bump_stream_head_on(&mut tx, &conversation, None, seq).await?;

        assert_notice(&mut tx, &notice, &metadata).await?;
        let recipients = load_rename_recipients(&mut tx, &conversation).await?;
        let row = load_channel_row(&mut tx, &conversation)
            .await?
            .ok_or_else(|| corrupt("conversations.id:rename_readback"))?;
        if row.latest_message_seq() != seq.get() {
            return Err(corrupt("conversations.latest_message_seq:rename_notice"));
        }
        let committed = CommittedChannelRename::changed(
            viewer,
            authority.workspace,
            row,
            requested,
            old,
            notice,
            recipients,
        )?;
        tx.commit().await?;
        Ok(Ok(committed))
    }
}

async fn assert_notice(
    tx: &mut Transaction<'_, Sqlite>,
    notice: &CommittedRenameNotice,
    expected_metadata: &str,
) -> Result<(), ChannelLifecycleMutationStoreError> {
    let row = sqlx::query(
        "SELECT id,conversation_id,from_kind,from_agent_id,from_user_id,content,metadata,seq,created_at,thread_root_id,author_id,author_name,author_display_name,cc_source_message_id \
         FROM messages WHERE id=?",
    )
    .bind(notice.message().as_str())
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| corrupt("messages.id:rename_notice"))?;
    let stored_id =
        MessageId::parse(row.try_get::<String, _>("id")?).map_err(|_| corrupt("messages.id"))?;
    let stored_conversation = ConversationId::parse(row.try_get::<String, _>("conversation_id")?)
        .map_err(|_| corrupt("messages.conversation_id"))?;
    let stored_metadata: String = row.try_get("metadata")?;
    let metadata: serde_json::Value =
        serde_json::from_str(&stored_metadata).map_err(|_| corrupt("messages.metadata"))?;
    let expected: serde_json::Value = serde_json::from_str(expected_metadata)
        .map_err(|_| corrupt("messages.metadata:expected"))?;
    if &stored_id != notice.message()
        || &stored_conversation != notice.conversation()
        || row.try_get::<String, _>("from_kind")? != "system"
        || row.try_get::<Option<String>, _>("from_agent_id")?.is_some()
        || row.try_get::<Option<String>, _>("from_user_id")?.is_some()
        || row.try_get::<String, _>("content")? != notice.content()
        || metadata != expected
        || row.try_get::<i64, _>("seq")? != notice.seq().get()
        || row.try_get::<i64, _>("created_at")? != notice.created_at()
        || row
            .try_get::<Option<String>, _>("thread_root_id")?
            .is_some()
        || row.try_get::<Option<String>, _>("author_id")?.is_some()
        || row.try_get::<Option<String>, _>("author_name")?.is_some()
        || row
            .try_get::<Option<String>, _>("author_display_name")?
            .is_some()
        || row
            .try_get::<Option<String>, _>("cc_source_message_id")?
            .is_some()
    {
        return Err(corrupt("messages:rename_notice"));
    }
    Ok(())
}

async fn load_rename_recipients(
    tx: &mut Transaction<'_, Sqlite>,
    conversation: &ConversationId,
) -> Result<Vec<RenameRecipient>, ChannelLifecycleMutationStoreError> {
    let rows = sqlx::query(
        "SELECT ca.conversation_id,ca.agent_id,ca.joined_at,a.id IS NOT NULL AS agent_exists \
         FROM conversation_agents ca LEFT JOIN agents a ON a.id=ca.agent_id \
         WHERE ca.conversation_id=?",
    )
    .bind(conversation.as_str())
    .fetch_all(&mut **tx)
    .await?;
    let mut recipients = Vec::with_capacity(rows.len());
    for row in rows {
        let source = ConversationId::parse(row.try_get::<String, _>("conversation_id")?)
            .map_err(|_| corrupt("conversation_agents.conversation_id"))?;
        let agent = AgentId::parse(row.try_get::<String, _>("agent_id")?)
            .map_err(|_| corrupt("conversation_agents.agent_id"))?;
        let _: i64 = row.try_get("joined_at")?;
        if row.try_get::<i64, _>("agent_exists")? != 1 {
            return Err(corrupt("conversation_agents.agent_id:missing"));
        }
        recipients.push(RenameRecipient::selected(source, agent));
    }
    Ok(recipients)
}
