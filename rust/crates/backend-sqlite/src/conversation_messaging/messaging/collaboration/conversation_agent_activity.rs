//! One-snapshot SQLite projection for persisted conversation-agent activity.

use serde_json::Value;
use silverhand_backend_core::{
    conversation_messaging::messaging::{
        PrincipalIdentity,
        collaboration::{
            channel_directory::{ChannelVisibility, DirectoryRoster},
            channel_lifecycle::ChannelLifecyclePlacement,
            conversation_agent_activity::{
                ActivityCreatedAt, ActivityEventId, ActivityPageRequest, ActivityPayload,
                AgentActivityPage, AgentActivityReadPort, ConversationActivityEvent,
                ConversationActivityKind, ConversationAgentActivityPage,
                ConversationAgentActivityReadPort, ConversationAgentActivityRefusal,
            },
        },
    },
    foundation::identity::{AgentId, ConversationId, UserId},
    foundation::persistence::PersistenceFailure,
};
use sqlx::{Row, Sqlite, Transaction};
use thiserror::Error;

use crate::conversation_messaging::messaging::{MessagingStore, SELECT_AGENT_PARTICIPATES};

use super::channel_lifecycle::{
    ChannelLifecycleStoreError, load_channel_row, load_human_members, load_workspace_role_fact,
};

#[derive(Clone)]
pub struct SqliteConversationAgentActivityRead {
    pool: crate::foundation::database::SqliteDatabase,
}

impl<E> MessagingStore<E> {
    /// The only durable activity table serves both runtime scopes. Never touch
    /// messages, tasks, memory, generations or the AUTOINCREMENT ledger.
    pub async fn prune_expired_activity(&self, now_ms: i64) -> Result<u64, sqlx::Error> {
        use silverhand_backend_core::conversation_messaging::messaging::collaboration::conversation_agent_activity::ACTIVITY_RETENTION_MS;
        let cutoff = now_ms.saturating_sub(ACTIVITY_RETENTION_MS);
        Ok(
            sqlx::query("DELETE FROM conversation_events WHERE created_at < ?")
                .bind(cutoff)
                .execute(&self.pool)
                .await?
                .rows_affected(),
        )
    }

    pub fn conversation_agent_activity(&self) -> SqliteConversationAgentActivityRead {
        SqliteConversationAgentActivityRead {
            pool: self.pool.clone(),
        }
    }
}

#[derive(Debug, Error)]
pub enum ConversationAgentActivityStoreError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("conversation access snapshot is malformed")]
    Access(#[from] ChannelLifecycleStoreError),
    #[error("stored conversation activity identity is malformed")]
    Identity,
    #[error("stored conversation activity id is outside its domain")]
    EventId,
    #[error("stored conversation activity kind is outside the closed set")]
    Kind,
    #[error("stored conversation activity payload is not valid JSON")]
    Payload,
    #[error("stored conversation activity created_at is outside its wire domain")]
    CreatedAt,
    #[error("stored conversation activity page is not a closed ordered projection")]
    Page,
}

impl From<ConversationAgentActivityStoreError> for PersistenceFailure {
    fn from(value: ConversationAgentActivityStoreError) -> Self {
        Self::from_source(value)
    }
}

impl ConversationAgentActivityReadPort for SqliteConversationAgentActivityRead {
    type Error = PersistenceFailure;

    async fn admit(
        &self,
        viewer: &UserId,
        conversation: &ConversationId,
        agent: &AgentId,
    ) -> Result<Result<(), ConversationAgentActivityRefusal>, Self::Error> {
        self.admit_inner(viewer, conversation, agent)
            .await
            .map_err(PersistenceFailure::from)
    }

    async fn read(
        &self,
        viewer: &UserId,
        conversation: &ConversationId,
        agent: &AgentId,
        page: ActivityPageRequest,
    ) -> Result<Result<ConversationAgentActivityPage, ConversationAgentActivityRefusal>, Self::Error>
    {
        self.read_inner(viewer, conversation, agent, page)
            .await
            .map_err(PersistenceFailure::from)
    }
}

impl SqliteConversationAgentActivityRead {
    async fn admit_inner(
        &self,
        viewer: &UserId,
        requested: &ConversationId,
        target: &AgentId,
    ) -> Result<Result<(), ConversationAgentActivityRefusal>, ConversationAgentActivityStoreError>
    {
        let mut tx = self.pool.begin_with("BEGIN DEFERRED").await?;
        let outcome = admit_in_snapshot(&mut tx, viewer, requested, target).await?;
        tx.commit().await?;
        Ok(outcome.map(|_| ()))
    }

    async fn read_inner(
        &self,
        viewer: &UserId,
        requested: &ConversationId,
        target: &AgentId,
        page: ActivityPageRequest,
    ) -> Result<
        Result<ConversationAgentActivityPage, ConversationAgentActivityRefusal>,
        ConversationAgentActivityStoreError,
    > {
        let mut tx = self.pool.begin_with("BEGIN DEFERRED").await?;
        let source_viewer = match admit_in_snapshot(&mut tx, viewer, requested, target).await? {
            Ok(value) => value,
            Err(refusal) => {
                tx.commit().await?;
                return Ok(Err(refusal));
            }
        };

        let selected = sqlx::query(
            "SELECT id,conversation_id,agent_id,kind,payload,created_at \
             FROM conversation_events \
             WHERE conversation_id=? AND agent_id=? AND (? IS NULL OR id < ?) \
             ORDER BY id DESC LIMIT ?",
        )
        .bind(requested.as_str())
        .bind(target.as_str())
        .bind(page.before().and_then(|value| value.persisted_value()))
        .bind(page.before().and_then(|value| value.persisted_value()))
        .bind(i64::from(page.limit()) + 1)
        .fetch_all(&mut *tx)
        .await?;

        let mut decoded = decode_rows(selected)?;

        let limit = usize::from(page.limit());
        let has_more = decoded.len() > limit;
        decoded.truncate(limit);
        decoded.reverse();
        let page = ConversationAgentActivityPage::projected(
            source_viewer,
            requested.clone(),
            target.clone(),
            decoded,
            has_more,
        )
        .map_err(|_| ConversationAgentActivityStoreError::Page)?;
        tx.commit().await?;
        Ok(Ok(page))
    }
}

async fn admit_in_snapshot(
    tx: &mut Transaction<'_, Sqlite>,
    viewer: &UserId,
    requested: &ConversationId,
    target: &AgentId,
) -> Result<Result<UserId, ConversationAgentActivityRefusal>, ConversationAgentActivityStoreError> {
    let Some(channel) = load_channel_row(tx, requested).await? else {
        return Ok(Err(ConversationAgentActivityRefusal::NotFound));
    };

    // Reuse the detail face's complete selected membership and role decode.
    // Public visibility may widen the viewer gate, but never hides a broken
    // membership row or weakens target-agent participation below.
    let human_members = load_human_members(tx, requested).await?;
    let stored_member = human_members.iter().any(|member| member == viewer);
    let source_viewer =
        match channel.placement() {
            ChannelLifecyclePlacement::Workspace(workspace) => {
                let workspace_exists: bool =
                    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workspaces w WHERE w.id=?)")
                        .bind(workspace.workspace().as_str())
                        .fetch_one(&mut **tx)
                        .await?;
                let role_fact = if workspace_exists {
                    load_workspace_role_fact(tx, viewer, workspace.workspace()).await?
                } else {
                    None
                };
                let stored_conversation_viewer = human_members
                    .iter()
                    .find(|member| *member == viewer)
                    .cloned();
                let is_member = role_fact.is_some() && stored_member;
                let public_widen = role_fact.is_some()
                    && workspace.roster() == DirectoryRoster::Open
                    && channel.visibility() == ChannelVisibility::Public
                    && channel.archived_at().is_none();
                if is_member {
                    stored_conversation_viewer
                } else if public_widen {
                    role_fact.map(|fact| fact.viewer)
                } else {
                    None
                }
            }
            ChannelLifecyclePlacement::DirectPair(participants) => participants
                .both()
                .iter()
                .find_map(|principal| match principal {
                    PrincipalIdentity::Human(source) if source == viewer => Some(source.clone()),
                    _ => None,
                }),
        };
    let Some(source_viewer) = source_viewer else {
        return Ok(Err(ConversationAgentActivityRefusal::Forbidden));
    };

    let target_participates: Option<i64> = sqlx::query_scalar(SELECT_AGENT_PARTICIPATES)
        .bind(requested.as_str())
        .bind(target.as_str())
        .bind(target.as_str())
        .bind(target.as_str())
        .fetch_optional(&mut **tx)
        .await?;
    if target_participates.is_none() {
        return Ok(Err(ConversationAgentActivityRefusal::TargetNotParticipant));
    }
    Ok(Ok(source_viewer))
}

fn decode_payload(
    raw: Option<String>,
) -> Result<ActivityPayload, ConversationAgentActivityStoreError> {
    let Some(raw) = raw else {
        return Ok(ActivityPayload::Null);
    };
    let parsed: Value =
        serde_json::from_str(&raw).map_err(|_| ConversationAgentActivityStoreError::Payload)?;
    match parsed {
        Value::Object(object) => Ok(ActivityPayload::object_json(
            Value::Object(object).to_string(),
        )),
        _ => Ok(ActivityPayload::Null),
    }
}

impl AgentActivityReadPort for SqliteConversationAgentActivityRead {
    async fn read_agent_activity(
        &self,
        viewer: &UserId,
        target: &AgentId,
        page: ActivityPageRequest,
    ) -> Result<Option<AgentActivityPage>, PersistenceFailure> {
        self.read_agent_inner(viewer, target, page)
            .await
            .map_err(PersistenceFailure::from)
    }
}
impl SqliteConversationAgentActivityRead {
    async fn read_agent_inner(
        &self,
        viewer: &UserId,
        target: &AgentId,
        page: ActivityPageRequest,
    ) -> Result<Option<AgentActivityPage>, ConversationAgentActivityStoreError> {
        let mut tx = self.pool.begin_with("BEGIN DEFERRED").await?;
        if !crate::identity_access::agent_view_acl::may_read_agent_stream(&mut tx, viewer, target)
            .await?
        {
            tx.commit().await?;
            return Ok(None);
        }
        let selected = sqlx::query("SELECT id,conversation_id,agent_id,kind,payload,created_at FROM conversation_events WHERE agent_id=? AND (? IS NULL OR id < ?) ORDER BY id DESC LIMIT ?")
            .bind(target.as_str()).bind(page.before().and_then(|v| v.persisted_value()))
            .bind(page.before().and_then(|v| v.persisted_value())).bind(i64::from(page.limit())+1)
            .fetch_all(&mut *tx).await?;
        let mut decoded = decode_rows(selected)?;

        let has_more = decoded.len() > usize::from(page.limit());
        decoded.truncate(usize::from(page.limit()));
        decoded.reverse();
        let result =
            AgentActivityPage::projected(viewer.clone(), target.clone(), decoded, has_more)
                .map_err(|_| ConversationAgentActivityStoreError::Page)?;
        tx.commit().await?;
        Ok(Some(result))
    }
}

fn decode_rows(
    selected: Vec<sqlx::sqlite::SqliteRow>,
) -> Result<Vec<ConversationActivityEvent>, ConversationAgentActivityStoreError> {
    // Decode every selected row, including the look-ahead row, before
    // slicing or reversing. A malformed has-more candidate cannot vanish
    // behind pagination.
    let mut decoded = Vec::with_capacity(selected.len());
    for row in selected {
        let id = ActivityEventId::new(row.try_get("id")?)
            .map_err(|_| ConversationAgentActivityStoreError::EventId)?;
        let conversation = ConversationId::parse(row.try_get::<String, _>("conversation_id")?)
            .map_err(|_| ConversationAgentActivityStoreError::Identity)?;
        let agent = row
            .try_get::<Option<String>, _>("agent_id")?
            .map(AgentId::parse)
            .transpose()
            .map_err(|_| ConversationAgentActivityStoreError::Identity)?;
        let kind = ConversationActivityKind::parse(&row.try_get::<String, _>("kind")?)
            .map_err(|_| ConversationAgentActivityStoreError::Kind)?;
        let payload = decode_payload(row.try_get("payload")?)?;
        let created_at = ActivityCreatedAt::new(row.try_get("created_at")?)
            .map_err(|_| ConversationAgentActivityStoreError::CreatedAt)?;
        decoded.push(ConversationActivityEvent::projected(
            id,
            conversation,
            agent,
            kind,
            payload,
            created_at,
        ));
    }

    Ok(decoded)
}
