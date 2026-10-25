//! SQLite prerequisites consumed by the workspace-rename unit owned by #2193.
//!
//! The read side selects only the committed open-roster, non-closed workspace
//! channel population. The write side owns one atomic sequence reservation and
//! system-message insert. Delivery and publication deliberately remain outside
//! this adapter.

use silverhand_backend_core::{
    conversation_messaging::messaging::{
        MessageId, MessageSeq, SendClock, SendEntropy,
        collaboration::workspace_rename_messaging::{
            CommittedWorkspaceRenameMessage, ConversationAgentsReadPort, WorkspaceRenameChannel,
            WorkspaceRenameChannelProjectionError, WorkspaceRenameChannels,
            WorkspaceRenameChannelsReadPort, WorkspaceRenameMessageAuthorPort,
            WorkspaceRenameMessageCreatedAt, WorkspaceRenameMessageDraft,
        },
        environment::EnvironmentUnavailable,
    },
    foundation::identity::{AgentId, ConversationId, WorkspaceId},
    foundation::persistence::PersistenceFailure,
};
use sqlx::{Row, Sqlite, Transaction};
use thiserror::Error;

use crate::conversation_messaging::messaging::{
    MessagingStore, RESERVE_NEXT_SEQ, SELECT_LATEST_SEQ,
};

#[derive(Clone)]
pub struct SqliteWorkspaceRenameMessaging<E> {
    pool: crate::foundation::database::SqliteDatabase,
    environment: E,
}

impl<E: Clone> MessagingStore<E> {
    pub fn workspace_rename_messaging(&self) -> SqliteWorkspaceRenameMessaging<E> {
        SqliteWorkspaceRenameMessaging {
            pool: self.pool.clone(),
            environment: self.environment.clone(),
        }
    }
}

#[derive(Debug, Error)]
pub enum WorkspaceRenameMessagingStoreError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("host clock or entropy failed")]
    Environment(#[from] EnvironmentUnavailable),
    #[error("stored workspace-rename fact is malformed at {field}")]
    Corrupt { field: &'static str },
    #[error("stored workspace-rename channel population is inconsistent")]
    Projection(#[from] WorkspaceRenameChannelProjectionError),
}

impl From<WorkspaceRenameMessagingStoreError> for PersistenceFailure {
    fn from(value: WorkspaceRenameMessagingStoreError) -> Self {
        Self::from_source(value)
    }
}

fn corrupt(field: &'static str) -> WorkspaceRenameMessagingStoreError {
    WorkspaceRenameMessagingStoreError::Corrupt { field }
}

impl<E> WorkspaceRenameChannelsReadPort for SqliteWorkspaceRenameMessaging<E>
where
    E: Send + Sync,
{
    type Error = PersistenceFailure;

    async fn list_non_closed(
        &self,
        workspace: &WorkspaceId,
    ) -> Result<WorkspaceRenameChannels, Self::Error> {
        self.list_non_closed_inner(workspace)
            .await
            .map_err(PersistenceFailure::from)
    }
}

impl<E> SqliteWorkspaceRenameMessaging<E> {
    async fn list_non_closed_inner(
        &self,
        requested: &WorkspaceId,
    ) -> Result<WorkspaceRenameChannels, WorkspaceRenameMessagingStoreError> {
        let mut tx = self.pool.begin_with("BEGIN DEFERRED").await?;
        let stored_workspace =
            sqlx::query_scalar::<_, String>("SELECT id FROM workspaces WHERE id=?")
                .bind(requested.as_str())
                .fetch_optional(&mut *tx)
                .await?
                .ok_or_else(|| corrupt("workspaces.id:workspace_rename_population"))?;
        let stored_workspace = WorkspaceId::parse(stored_workspace)
            .map_err(|_| corrupt("workspaces.id:workspace_rename_population"))?;
        if &stored_workspace != requested {
            return Err(corrupt("workspaces.id:workspace_rename_population"));
        }

        let selected = sqlx::query(
            "SELECT c.workspace_id,c.id AS conversation_id FROM conversations c \
             WHERE c.workspace_id=? AND c.placement_kind='workspace' \
             AND c.roster_policy='open' AND c.closed_at IS NULL \
             ORDER BY c.id ASC",
        )
        .bind(requested.as_str())
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        let mut channels = Vec::with_capacity(selected.len());
        for row in selected {
            let workspace = WorkspaceId::parse(
                row.try_get::<String, _>("workspace_id")
                    .map_err(|_| corrupt("conversations.workspace_id"))?,
            )
            .map_err(|_| corrupt("conversations.workspace_id"))?;
            let conversation = ConversationId::parse(
                row.try_get::<String, _>("conversation_id")
                    .map_err(|_| corrupt("conversations.id"))?,
            )
            .map_err(|_| corrupt("conversations.id"))?;
            channels.push(WorkspaceRenameChannel::selected(workspace, conversation));
        }
        let population = WorkspaceRenameChannels::selected(stored_workspace, channels)?;
        Ok(population)
    }
}

impl<E> ConversationAgentsReadPort for SqliteWorkspaceRenameMessaging<E>
where
    E: Send + Sync,
{
    type Error = PersistenceFailure;

    async fn agent_ids(&self, conversation: &ConversationId) -> Result<Vec<AgentId>, Self::Error> {
        self.agent_ids_inner(conversation)
            .await
            .map_err(PersistenceFailure::from)
    }
}

impl<E> SqliteWorkspaceRenameMessaging<E> {
    /// The agents joined to one conversation — the ungated delivery audience for
    /// the rename fan-out. Ordered by `agent_id` so the fan-out adapter's own tests
    /// observe a deterministic sequence; delivery itself is per-agent and
    /// order-independent. An absent or empty conversation is the empty audience,
    /// never an error: there is simply no one to push to.
    async fn agent_ids_inner(
        &self,
        conversation: &ConversationId,
    ) -> Result<Vec<AgentId>, WorkspaceRenameMessagingStoreError> {
        let rows = sqlx::query_scalar::<_, String>(
            "SELECT agent_id FROM conversation_agents WHERE conversation_id=? ORDER BY agent_id ASC",
        )
        .bind(conversation.as_str())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|raw| AgentId::parse(raw).map_err(|_| corrupt("conversation_agents.agent_id")))
            .collect()
    }
}

impl<E> WorkspaceRenameMessageAuthorPort for SqliteWorkspaceRenameMessaging<E>
where
    E: SendClock + SendEntropy + Send + Sync,
{
    type Error = PersistenceFailure;

    async fn author(
        &self,
        draft: WorkspaceRenameMessageDraft,
    ) -> Result<CommittedWorkspaceRenameMessage, Self::Error> {
        self.author_inner(draft)
            .await
            .map_err(PersistenceFailure::from)
    }
}

impl<E> SqliteWorkspaceRenameMessaging<E>
where
    E: SendClock + SendEntropy,
{
    async fn author_inner(
        &self,
        requested: WorkspaceRenameMessageDraft,
    ) -> Result<CommittedWorkspaceRenameMessage, WorkspaceRenameMessagingStoreError> {
        // Host work precedes the writer reservation. The committed fact below is
        // still reconstructed exclusively from rows inside the transaction.
        let created_at = self.environment.now_unix_ms()?;
        let message = MessageId::mint(&self.environment)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;

        let source_workspace = load_authoring_target(&mut tx, &requested).await?;
        let reserved = sqlx::query(RESERVE_NEXT_SEQ)
            .bind(requested.conversation().as_str())
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if reserved != 1 {
            return Err(corrupt("conversations.latest_message_seq:reserve"));
        }
        let seq = MessageSeq::try_from(
            sqlx::query_scalar::<_, i64>(SELECT_LATEST_SEQ)
                .bind(requested.conversation().as_str())
                .fetch_one(&mut *tx)
                .await?,
        )
        .map_err(|_| corrupt("conversations.latest_message_seq"))?;
        let metadata = serde_json::to_string(&serde_json::json!({
            "subtype": requested.subtype(),
            "old_name": requested.old_name(),
            "new_name": requested.new_name(),
            "scope": requested.scope(),
        }))
        .map_err(|_| corrupt("messages.metadata:serialize"))?;
        sqlx::query(
            "INSERT INTO messages(id,conversation_id,from_kind,from_agent_id,from_user_id,content,metadata,seq,created_at,thread_root_id,author_id,author_name,author_display_name,cc_source_message_id) \
             VALUES(?,?,'system',NULL,NULL,?,?,?,?,NULL,NULL,NULL,NULL,NULL)",
        )
        .bind(message.as_str())
        .bind(requested.conversation().as_str())
        .bind(requested.content())
        .bind(metadata)
        .bind(seq.get())
        .bind(created_at)
        .execute(&mut *tx)
        .await?;

        let committed = load_committed_message(
            &mut tx,
            &requested,
            source_workspace,
            &message,
            seq,
            created_at,
        )
        .await?;
        let final_head = sqlx::query_scalar::<_, i64>(SELECT_LATEST_SEQ)
            .bind(requested.conversation().as_str())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| corrupt("conversations.latest_message_seq:readback"))?;
        let final_head = MessageSeq::try_from(final_head)
            .map_err(|_| corrupt("conversations.latest_message_seq:readback"))?;
        if final_head != seq {
            return Err(corrupt("conversations.latest_message_seq:readback"));
        }
        tx.commit().await?;
        Ok(committed)
    }
}

async fn load_authoring_target(
    tx: &mut Transaction<'_, Sqlite>,
    requested: &WorkspaceRenameMessageDraft,
) -> Result<WorkspaceId, WorkspaceRenameMessagingStoreError> {
    let row = sqlx::query(
        "SELECT id,workspace_id,placement_kind,roster_policy,closed_at \
         FROM conversations WHERE id=?",
    )
    .bind(requested.conversation().as_str())
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| corrupt("conversations.id:workspace_rename_message"))?;
    let conversation = ConversationId::parse(
        row.try_get::<String, _>("id")
            .map_err(|_| corrupt("conversations.id"))?,
    )
    .map_err(|_| corrupt("conversations.id"))?;
    let workspace = WorkspaceId::parse(
        row.try_get::<String, _>("workspace_id")
            .map_err(|_| corrupt("conversations.workspace_id"))?,
    )
    .map_err(|_| corrupt("conversations.workspace_id"))?;
    let placement: String = row
        .try_get("placement_kind")
        .map_err(|_| corrupt("conversations.placement_kind"))?;
    let roster: String = row
        .try_get("roster_policy")
        .map_err(|_| corrupt("conversations.roster_policy"))?;
    let closed_at: Option<i64> = row
        .try_get("closed_at")
        .map_err(|_| corrupt("conversations.closed_at"))?;
    if &conversation != requested.conversation()
        || &workspace != requested.workspace()
        || placement != "workspace"
        || roster != "open"
        || closed_at.is_some()
    {
        return Err(corrupt("conversations:workspace_rename_message_target"));
    }
    Ok(workspace)
}

async fn load_committed_message(
    tx: &mut Transaction<'_, Sqlite>,
    requested: &WorkspaceRenameMessageDraft,
    source_workspace: WorkspaceId,
    expected_message: &MessageId,
    expected_seq: MessageSeq,
    expected_created_at: i64,
) -> Result<CommittedWorkspaceRenameMessage, WorkspaceRenameMessagingStoreError> {
    let row = sqlx::query(
        "SELECT id,conversation_id,from_kind,from_agent_id,from_user_id,content,metadata,seq,created_at,thread_root_id,author_id,author_name,author_display_name,cc_source_message_id \
         FROM messages WHERE id=?",
    )
    .bind(expected_message.as_str())
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| corrupt("messages.id:workspace_rename_message"))?;

    let message = MessageId::parse(
        row.try_get::<String, _>("id")
            .map_err(|_| corrupt("messages.id"))?,
    )
    .map_err(|_| corrupt("messages.id"))?;
    let conversation = ConversationId::parse(
        row.try_get::<String, _>("conversation_id")
            .map_err(|_| corrupt("messages.conversation_id"))?,
    )
    .map_err(|_| corrupt("messages.conversation_id"))?;
    if &message != expected_message {
        return Err(corrupt("messages.id"));
    }
    if &conversation != requested.conversation() {
        return Err(corrupt("messages.conversation_id"));
    }
    if row
        .try_get::<String, _>("from_kind")
        .map_err(|_| corrupt("messages.from_kind"))?
        != "system"
        || row
            .try_get::<Option<String>, _>("from_agent_id")
            .map_err(|_| corrupt("messages.from_agent_id"))?
            .is_some()
        || row
            .try_get::<Option<String>, _>("from_user_id")
            .map_err(|_| corrupt("messages.from_user_id"))?
            .is_some()
        || row
            .try_get::<Option<String>, _>("thread_root_id")
            .map_err(|_| corrupt("messages.thread_root_id"))?
            .is_some()
        || row
            .try_get::<Option<String>, _>("author_id")
            .map_err(|_| corrupt("messages.author_id"))?
            .is_some()
        || row
            .try_get::<Option<String>, _>("author_name")
            .map_err(|_| corrupt("messages.author_name"))?
            .is_some()
        || row
            .try_get::<Option<String>, _>("author_display_name")
            .map_err(|_| corrupt("messages.author_display_name"))?
            .is_some()
        || row
            .try_get::<Option<String>, _>("cc_source_message_id")
            .map_err(|_| corrupt("messages.cc_source_message_id"))?
            .is_some()
    {
        return Err(corrupt("messages:workspace_rename_system_shape"));
    }

    let stored_seq = MessageSeq::try_from(
        row.try_get::<i64, _>("seq")
            .map_err(|_| corrupt("messages.seq"))?,
    )
    .map_err(|_| corrupt("messages.seq"))?;
    if stored_seq != expected_seq {
        return Err(corrupt("messages.seq"));
    }
    let stored_created_at = row
        .try_get::<i64, _>("created_at")
        .map_err(|_| corrupt("messages.created_at"))?;
    let created_at = WorkspaceRenameMessageCreatedAt::new(stored_created_at)
        .map_err(|_| corrupt("messages.created_at"))?;
    if stored_created_at != expected_created_at {
        return Err(corrupt("messages.created_at"));
    }

    let raw_metadata = row
        .try_get::<String, _>("metadata")
        .map_err(|_| corrupt("messages.metadata"))?;
    let metadata: serde_json::Value =
        serde_json::from_str(&raw_metadata).map_err(|_| corrupt("messages.metadata"))?;
    let object = metadata
        .as_object()
        .filter(|object| object.len() == 4)
        .ok_or_else(|| corrupt("messages.metadata"))?;
    let string_field = |key: &str| {
        object
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned)
            .ok_or_else(|| corrupt("messages.metadata"))
    };
    let subtype = string_field("subtype")?;
    let old_name = string_field("old_name")?;
    let new_name = string_field("new_name")?;
    let scope = string_field("scope")?;
    if subtype != requested.subtype()
        || old_name != requested.old_name()
        || new_name != requested.new_name()
        || scope != requested.scope()
    {
        return Err(corrupt("messages.metadata:workspace_rename"));
    }
    let actual_draft =
        WorkspaceRenameMessageDraft::for_names(source_workspace, conversation, old_name, new_name);
    let content = row
        .try_get::<String, _>("content")
        .map_err(|_| corrupt("messages.content"))?;
    if content != actual_draft.content() || &actual_draft != requested {
        return Err(corrupt("messages.content:workspace_rename"));
    }

    Ok(CommittedWorkspaceRenameMessage::committed(
        actual_draft,
        message,
        stored_seq,
        created_at,
    ))
}
