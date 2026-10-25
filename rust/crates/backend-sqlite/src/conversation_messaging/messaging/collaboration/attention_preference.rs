use crate::conversation_messaging::messaging::{HostEnvironment, MessagingStore};
use silverhand_backend_core::{
    conversation_messaging::messaging::{
        SendClock,
        collaboration::attention_preference::{
            AttentionPreferencePort, ChannelAttentionAdmissionRefusal, ChannelAttentionCommit,
            ChannelAttentionMode, ChannelAttentionWriteRefusal, WorkspaceAttentionAdmissionRefusal,
            WorkspaceAttentionCommit, WorkspaceAttentionPreference, WorkspaceAttentionReadRefusal,
            WorkspaceAttentionWriteRefusal,
        },
    },
    foundation::identity::{ConversationId, UserId, WorkspaceId},
    foundation::persistence::PersistenceFailure,
};
use thiserror::Error;

#[derive(Clone)]
pub struct SqliteAttentionPreferenceStore<E = HostEnvironment> {
    pool: crate::foundation::database::SqliteDatabase,
    environment: E,
}
impl<E: Clone> MessagingStore<E> {
    pub fn attention_preferences(&self) -> SqliteAttentionPreferenceStore<E> {
        SqliteAttentionPreferenceStore {
            pool: self.pool.clone(),
            environment: self.environment.clone(),
        }
    }
}

/// Opaque preflight proof. Only this adapter can mint one, and commit consumes
/// it while repeating the authority in the writer transaction.
pub struct SqliteChannelAttentionTarget {
    viewer: UserId,
    conversation: ConversationId,
    workspace: WorkspaceId,
}
pub struct SqliteWorkspaceAttentionTarget {
    viewer: UserId,
    workspace: WorkspaceId,
}
#[derive(Debug)]
pub struct SqliteCommittedChannelAttentionPreference {
    viewer: UserId,
    workspace: WorkspaceId,
    conversation: ConversationId,
    mode: ChannelAttentionMode,
}
impl ChannelAttentionCommit for SqliteCommittedChannelAttentionPreference {
    fn viewer(&self) -> &UserId {
        &self.viewer
    }
    fn workspace(&self) -> &WorkspaceId {
        &self.workspace
    }
    fn conversation(&self) -> &ConversationId {
        &self.conversation
    }
    fn mode(&self) -> ChannelAttentionMode {
        self.mode
    }
}
#[derive(Debug)]
pub struct SqliteCommittedWorkspaceAttentionPreference {
    viewer: UserId,
    workspace: WorkspaceId,
    muted: bool,
}
impl WorkspaceAttentionCommit for SqliteCommittedWorkspaceAttentionPreference {
    fn viewer(&self) -> &UserId {
        &self.viewer
    }
    fn workspace(&self) -> &WorkspaceId {
        &self.workspace
    }
    fn muted(&self) -> bool {
        self.muted
    }
}

#[derive(Debug, Error)]
pub enum AttentionPreferenceStoreError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("stored attention preference identity is malformed")]
    MalformedIdentity,
    #[error("stored channel attention mode is malformed")]
    MalformedMode,
    #[error("stored workspace mute flag is malformed")]
    MalformedMute,
    #[error("clock failed")]
    Clock(
        #[source]
        silverhand_backend_core::conversation_messaging::messaging::EnvironmentUnavailable,
    ),
}
impl From<AttentionPreferenceStoreError> for PersistenceFailure {
    fn from(value: AttentionPreferenceStoreError) -> Self {
        Self::from_source(value)
    }
}

async fn channel_workspace(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    conversation: &ConversationId,
) -> Result<Option<WorkspaceId>, AttentionPreferenceStoreError> {
    let raw: Option<String> = sqlx::query_scalar("SELECT workspace_id FROM conversations WHERE id=? AND placement_kind='workspace' AND roster_policy='open'")
        .bind(conversation.as_str()).fetch_optional(&mut **tx).await?.flatten();
    raw.map(|value| {
        WorkspaceId::parse(value).map_err(|_| AttentionPreferenceStoreError::MalformedIdentity)
    })
    .transpose()
}
async fn channel_visible(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    viewer: &UserId,
    workspace: &WorkspaceId,
    conversation: &ConversationId,
) -> Result<bool, AttentionPreferenceStoreError> {
    Ok(sqlx::query_scalar::<_, i64>("SELECT 1 WHERE EXISTS(SELECT 1 FROM workspace_users WHERE workspace_id=? AND user_id=?) AND EXISTS(SELECT 1 FROM conversation_users WHERE conversation_id=? AND user_id=?)")
        .bind(workspace.as_str()).bind(viewer.as_str()).bind(conversation.as_str()).bind(viewer.as_str())
        .fetch_optional(&mut **tx).await?.is_some())
}
async fn workspace_kind(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    viewer: &UserId,
    workspace: &WorkspaceId,
) -> Result<Result<(), bool>, AttentionPreferenceStoreError> {
    let visible: Option<i64> = sqlx::query_scalar("SELECT 1 FROM workspaces w JOIN workspace_users wu ON wu.workspace_id=w.id AND wu.user_id=? WHERE w.id=?")
        .bind(viewer.as_str()).bind(workspace.as_str()).fetch_optional(&mut **tx).await?;
    if visible.is_none() {
        return Ok(Err(false));
    }
    let owner: Option<String> =
        sqlx::query_scalar("SELECT owner_agent_id FROM workspaces WHERE id=?")
            .bind(workspace.as_str())
            .fetch_one(&mut **tx)
            .await?;
    if owner.is_some() {
        Ok(Err(true))
    } else {
        Ok(Ok(()))
    }
}

impl<E: SendClock + Clone + Send + Sync + 'static> AttentionPreferencePort
    for SqliteAttentionPreferenceStore<E>
{
    type Error = PersistenceFailure;
    type ChannelTarget = SqliteChannelAttentionTarget;
    type WorkspaceTarget = SqliteWorkspaceAttentionTarget;
    type ChannelCommit = SqliteCommittedChannelAttentionPreference;
    type WorkspaceCommit = SqliteCommittedWorkspaceAttentionPreference;

    async fn admit_channel(
        &self,
        viewer: &UserId,
        conversation: &ConversationId,
    ) -> Result<Result<Self::ChannelTarget, ChannelAttentionAdmissionRefusal>, Self::Error> {
        let mut tx = self
            .pool
            .begin_with("BEGIN DEFERRED")
            .await
            .map_err(AttentionPreferenceStoreError::from)?;
        let Some(workspace) = channel_workspace(&mut tx, conversation).await? else {
            tx.rollback()
                .await
                .map_err(AttentionPreferenceStoreError::from)?;
            return Ok(Err(ChannelAttentionAdmissionRefusal::ChannelNotFound));
        };
        if !channel_visible(&mut tx, viewer, &workspace, conversation).await? {
            tx.rollback()
                .await
                .map_err(AttentionPreferenceStoreError::from)?;
            return Ok(Err(ChannelAttentionAdmissionRefusal::Forbidden));
        }
        tx.commit()
            .await
            .map_err(AttentionPreferenceStoreError::from)?;
        Ok(Ok(SqliteChannelAttentionTarget {
            viewer: viewer.clone(),
            conversation: conversation.clone(),
            workspace,
        }))
    }

    async fn commit_channel(
        &self,
        target: Self::ChannelTarget,
        mode: ChannelAttentionMode,
    ) -> Result<Result<Self::ChannelCommit, ChannelAttentionWriteRefusal>, Self::Error> {
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(AttentionPreferenceStoreError::from)?;
        let current = channel_workspace(&mut tx, &target.conversation).await?;
        if current.as_ref() != Some(&target.workspace) {
            tx.rollback()
                .await
                .map_err(AttentionPreferenceStoreError::from)?;
            return Ok(Err(ChannelAttentionWriteRefusal::ChannelNotFound));
        }
        if !channel_visible(
            &mut tx,
            &target.viewer,
            &target.workspace,
            &target.conversation,
        )
        .await?
        {
            tx.rollback()
                .await
                .map_err(AttentionPreferenceStoreError::from)?;
            return Ok(Err(ChannelAttentionWriteRefusal::Forbidden));
        }
        // The fallible clock is deliberately after every product refusal.
        let now = self
            .environment
            .now_unix_ms()
            .map_err(AttentionPreferenceStoreError::Clock)?;
        sqlx::query("INSERT INTO channel_attention_preferences(user_id,conversation_id,mode,updated_at) VALUES(?,?,?,?) ON CONFLICT(user_id,conversation_id) DO UPDATE SET mode=excluded.mode,updated_at=excluded.updated_at")
            .bind(target.viewer.as_str()).bind(target.conversation.as_str()).bind(mode.as_str()).bind(now).execute(&mut *tx).await.map_err(AttentionPreferenceStoreError::from)?;
        let stored: String = sqlx::query_scalar(
            "SELECT mode FROM channel_attention_preferences WHERE user_id=? AND conversation_id=?",
        )
        .bind(target.viewer.as_str())
        .bind(target.conversation.as_str())
        .fetch_one(&mut *tx)
        .await
        .map_err(AttentionPreferenceStoreError::from)?;
        let stored = ChannelAttentionMode::parse(&stored)
            .ok_or(AttentionPreferenceStoreError::MalformedMode)?;
        tx.commit()
            .await
            .map_err(AttentionPreferenceStoreError::from)?;
        Ok(Ok(SqliteCommittedChannelAttentionPreference {
            viewer: target.viewer,
            workspace: target.workspace,
            conversation: target.conversation,
            mode: stored,
        }))
    }

    async fn get_workspace(
        &self,
        viewer: &UserId,
        workspace: WorkspaceId,
    ) -> Result<Result<WorkspaceAttentionPreference, WorkspaceAttentionReadRefusal>, Self::Error>
    {
        let mut tx = self
            .pool
            .begin_with("BEGIN DEFERRED")
            .await
            .map_err(AttentionPreferenceStoreError::from)?;
        match workspace_kind(&mut tx, viewer, &workspace).await? {
            Err(false) => {
                tx.rollback()
                    .await
                    .map_err(AttentionPreferenceStoreError::from)?;
                return Ok(Err(WorkspaceAttentionReadRefusal::Forbidden));
            }
            Err(true) => {
                tx.rollback()
                    .await
                    .map_err(AttentionPreferenceStoreError::from)?;
                return Ok(Err(WorkspaceAttentionReadRefusal::WorkspaceNotFound));
            }
            Ok(()) => {}
        }
        let muted: Option<i64> = sqlx::query_scalar(
            "SELECT muted FROM workspace_attention_preferences WHERE user_id=? AND workspace_id=?",
        )
        .bind(viewer.as_str())
        .bind(workspace.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(AttentionPreferenceStoreError::from)?;
        let muted = match muted {
            None | Some(0) => false,
            Some(1) => true,
            Some(_) => return Err(AttentionPreferenceStoreError::MalformedMute.into()),
        };
        tx.commit()
            .await
            .map_err(AttentionPreferenceStoreError::from)?;
        Ok(Ok(WorkspaceAttentionPreference::projected(
            workspace, muted,
        )))
    }

    async fn admit_workspace_write(
        &self,
        viewer: &UserId,
        workspace: &WorkspaceId,
    ) -> Result<Result<Self::WorkspaceTarget, WorkspaceAttentionAdmissionRefusal>, Self::Error>
    {
        let mut tx = self
            .pool
            .begin_with("BEGIN DEFERRED")
            .await
            .map_err(AttentionPreferenceStoreError::from)?;
        match workspace_kind(&mut tx, viewer, workspace).await? {
            Err(false) => {
                tx.rollback()
                    .await
                    .map_err(AttentionPreferenceStoreError::from)?;
                return Ok(Err(WorkspaceAttentionAdmissionRefusal::Forbidden));
            }
            Err(true) => {
                tx.rollback()
                    .await
                    .map_err(AttentionPreferenceStoreError::from)?;
                return Ok(Err(WorkspaceAttentionAdmissionRefusal::WorkspaceNotFound));
            }
            Ok(()) => {}
        }
        tx.commit()
            .await
            .map_err(AttentionPreferenceStoreError::from)?;
        Ok(Ok(SqliteWorkspaceAttentionTarget {
            viewer: viewer.clone(),
            workspace: workspace.clone(),
        }))
    }

    async fn commit_workspace(
        &self,
        target: Self::WorkspaceTarget,
        muted: bool,
    ) -> Result<Result<Self::WorkspaceCommit, WorkspaceAttentionWriteRefusal>, Self::Error> {
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(AttentionPreferenceStoreError::from)?;
        match workspace_kind(&mut tx, &target.viewer, &target.workspace).await? {
            Err(false) => {
                tx.rollback()
                    .await
                    .map_err(AttentionPreferenceStoreError::from)?;
                return Ok(Err(WorkspaceAttentionWriteRefusal::Forbidden));
            }
            Err(true) => {
                tx.rollback()
                    .await
                    .map_err(AttentionPreferenceStoreError::from)?;
                return Ok(Err(WorkspaceAttentionWriteRefusal::WorkspaceNotFound));
            }
            Ok(()) => {}
        }
        let now = self
            .environment
            .now_unix_ms()
            .map_err(AttentionPreferenceStoreError::Clock)?;
        sqlx::query("INSERT INTO workspace_attention_preferences(user_id,workspace_id,muted,updated_at) VALUES(?,?,?,?) ON CONFLICT(user_id,workspace_id) DO UPDATE SET muted=excluded.muted,updated_at=excluded.updated_at")
            .bind(target.viewer.as_str()).bind(target.workspace.as_str()).bind(if muted {1_i64}else{0_i64}).bind(now).execute(&mut *tx).await.map_err(AttentionPreferenceStoreError::from)?;
        let stored: i64 = sqlx::query_scalar(
            "SELECT muted FROM workspace_attention_preferences WHERE user_id=? AND workspace_id=?",
        )
        .bind(target.viewer.as_str())
        .bind(target.workspace.as_str())
        .fetch_one(&mut *tx)
        .await
        .map_err(AttentionPreferenceStoreError::from)?;
        let stored = match stored {
            0 => false,
            1 => true,
            _ => return Err(AttentionPreferenceStoreError::MalformedMute.into()),
        };
        tx.commit()
            .await
            .map_err(AttentionPreferenceStoreError::from)?;
        Ok(Ok(SqliteCommittedWorkspaceAttentionPreference {
            viewer: target.viewer,
            workspace: target.workspace,
            muted: stored,
        }))
    }
}
