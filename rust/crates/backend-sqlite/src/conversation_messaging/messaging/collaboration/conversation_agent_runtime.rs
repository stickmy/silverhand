//! One-snapshot SQLite projection for committed-roster runtime state.

use serde_json::json;
use silverhand_backend_core::{
    conversation_messaging::messaging::{
        PrincipalIdentity,
        collaboration::{
            CliSessionId,
            channel_directory::{ChannelVisibility, DirectoryRoster},
            channel_lifecycle::ChannelLifecyclePlacement,
            conversation_agent_directory::JoinedAt,
            conversation_agent_runtime::{
                ConversationAgentRuntimeProjection, ConversationAgentRuntimeReadPort,
                ConversationAgentRuntimeRefusal, ConversationAgentRuntimeRow, RuntimePid,
                RuntimeProjectionStatus,
            },
        },
    },
    foundation::identity::{AgentId, ConversationId, UserId},
    foundation::persistence::PersistenceFailure,
    host_runtime::runtime_credential::{RuntimeGeneration, RuntimeScope},
    identity_access::agents::session_scope::AgentSessionScope,
};
use sqlx::{Row, Sqlite, Transaction};
use thiserror::Error;

use crate::conversation_messaging::messaging::MessagingStore;

use super::channel_lifecycle::{
    ChannelLifecycleStoreError, load_channel_row, load_human_members, load_workspace_role_fact,
};

#[derive(Clone)]
pub struct SqliteConversationAgentRuntimeRead {
    pool: crate::foundation::database::SqliteDatabase,
}

impl<E> MessagingStore<E> {
    pub fn conversation_agent_runtime(&self) -> SqliteConversationAgentRuntimeRead {
        SqliteConversationAgentRuntimeRead {
            pool: self.pool.clone(),
        }
    }
}

#[derive(Debug, Error)]
pub enum ConversationAgentRuntimeStoreError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("conversation access snapshot is malformed")]
    Access(#[from] ChannelLifecycleStoreError),
    #[error("stored runtime population identity is malformed")]
    PopulationIdentity,
    #[error("stored runtime population membership timestamp is malformed")]
    PopulationJoinedAt,
    #[error("stored runtime slot identity is malformed")]
    SlotIdentity,
    #[error("stored runtime slot scope is malformed")]
    SlotScope,
    #[error("stored runtime slot generation is malformed")]
    Generation,
    #[error("stored runtime CLI session identity is malformed")]
    CliSession,
    #[error("stored runtime pid is malformed")]
    Pid,
    #[error("stored runtime status is outside the closed set")]
    Status,
    #[error("stored runtime projection is not coordinate-closed")]
    Projection,
}

impl From<ConversationAgentRuntimeStoreError> for PersistenceFailure {
    fn from(value: ConversationAgentRuntimeStoreError) -> Self {
        Self::from_source(value)
    }
}

impl ConversationAgentRuntimeReadPort for SqliteConversationAgentRuntimeRead {
    type Error = PersistenceFailure;

    async fn read(
        &self,
        viewer: &UserId,
        conversation: &ConversationId,
    ) -> Result<
        Result<ConversationAgentRuntimeProjection, ConversationAgentRuntimeRefusal>,
        Self::Error,
    > {
        self.read_inner(viewer, conversation)
            .await
            .map_err(PersistenceFailure::from)
    }
}

impl SqliteConversationAgentRuntimeRead {
    async fn read_inner(
        &self,
        viewer: &UserId,
        requested: &ConversationId,
    ) -> Result<
        Result<ConversationAgentRuntimeProjection, ConversationAgentRuntimeRefusal>,
        ConversationAgentRuntimeStoreError,
    > {
        let mut tx = self.pool.begin_with("BEGIN DEFERRED").await?;
        let Some(channel) = load_channel_row(&mut tx, requested).await? else {
            tx.commit().await?;
            return Ok(Err(ConversationAgentRuntimeRefusal::NotFound));
        };

        // Decode the complete human membership face before visibility. Public
        // widening is not allowed to hide a malformed selected membership row.
        let human_members = load_human_members(&mut tx, requested).await?;
        let stored_member = human_members
            .iter()
            .find(|member| *member == viewer)
            .cloned();
        let source_viewer = match channel.placement() {
            ChannelLifecyclePlacement::Workspace(workspace) => {
                let workspace_exists: bool =
                    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workspaces WHERE id=?)")
                        .bind(workspace.workspace().as_str())
                        .fetch_one(&mut *tx)
                        .await?;
                let role = if workspace_exists {
                    load_workspace_role_fact(&mut tx, viewer, workspace.workspace()).await?
                } else {
                    None
                };
                let member = role.is_some() && stored_member.is_some();
                let public_widen = role.is_some()
                    && workspace.roster() == DirectoryRoster::Open
                    && channel.visibility() == ChannelVisibility::Public
                    && channel.archived_at().is_none();
                if member {
                    stored_member
                } else if public_widen {
                    role.map(|fact| fact.viewer)
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
            tx.commit().await?;
            return Ok(Err(ConversationAgentRuntimeRefusal::Forbidden));
        };

        // Population comes only from committed conversation_agents rows. A
        // direct pair has no such roster, so endpoint participation cannot
        // widen this list.
        let population = load_population(&mut tx, requested).await?;
        let rows = load_runtime_rows(&mut tx, requested, &population).await?;
        let projection =
            ConversationAgentRuntimeProjection::projected(source_viewer, requested.clone(), rows)
                .map_err(|_| ConversationAgentRuntimeStoreError::Projection)?;
        tx.commit().await?;
        Ok(Ok(projection))
    }
}

async fn load_population(
    tx: &mut Transaction<'_, Sqlite>,
    requested: &ConversationId,
) -> Result<Vec<PopulationMember>, ConversationAgentRuntimeStoreError> {
    let rows = sqlx::query(
        "SELECT ca.conversation_id,ca.agent_id,ca.joined_at,a.id AS stored_agent_id \
         FROM conversation_agents ca LEFT JOIN agents a ON a.id=ca.agent_id \
         WHERE ca.conversation_id=? ORDER BY ca.agent_id ASC",
    )
    .bind(requested.as_str())
    .fetch_all(&mut **tx)
    .await?;
    let mut population = Vec::with_capacity(rows.len());
    for row in rows {
        let source = ConversationId::parse(row.try_get::<String, _>("conversation_id")?)
            .map_err(|_| ConversationAgentRuntimeStoreError::PopulationIdentity)?;
        let agent = AgentId::parse(row.try_get::<String, _>("agent_id")?)
            .map_err(|_| ConversationAgentRuntimeStoreError::PopulationIdentity)?;
        let stored_agent = row
            .try_get::<Option<String>, _>("stored_agent_id")?
            .map(AgentId::parse)
            .transpose()
            .map_err(|_| ConversationAgentRuntimeStoreError::PopulationIdentity)?;
        JoinedAt::new(row.try_get("joined_at")?)
            .map_err(|_| ConversationAgentRuntimeStoreError::PopulationJoinedAt)?;
        if &source != requested || stored_agent.as_ref() != Some(&agent) {
            return Err(ConversationAgentRuntimeStoreError::PopulationIdentity);
        }
        if population
            .last()
            .is_some_and(|previous: &PopulationMember| previous.agent.as_str() >= agent.as_str())
        {
            return Err(ConversationAgentRuntimeStoreError::PopulationIdentity);
        }
        population.push(PopulationMember { source, agent });
    }
    Ok(population)
}

struct PopulationMember {
    source: ConversationId,
    agent: AgentId,
}

async fn load_runtime_rows(
    tx: &mut Transaction<'_, Sqlite>,
    requested: &ConversationId,
    population: &[PopulationMember],
) -> Result<Vec<ConversationAgentRuntimeRow>, ConversationAgentRuntimeStoreError> {
    if population.is_empty() {
        return Ok(Vec::new());
    }
    let population_json = json!(
        population
            .iter()
            .map(|member| member.agent.as_str())
            .collect::<Vec<_>>()
    );
    let selected = sqlx::query(
        "WITH population(agent_id) AS (SELECT value FROM json_each(?)) \
         SELECT p.agent_id AS population_agent_id,a.id AS stored_agent_id,a.session_scope,\
                s.agent_id AS slot_agent_id,s.conversation_id AS slot_conversation_id,\
                s.generation,s.observed_status,s.observed_pid,s.cli_session_id \
         FROM population p JOIN agents a ON a.id=p.agent_id \
         JOIN agent_runtime_slots s ON s.agent_id=a.id \
         WHERE s.conversation_id=? OR s.conversation_id IS NULL \
         ORDER BY p.agent_id ASC,s.id ASC",
    )
    .bind(population_json.to_string())
    .bind(requested.as_str())
    .fetch_all(&mut **tx)
    .await?;

    // SQL limits the read face to the requested ConversationSlot coordinate or
    // the roster agent's AgentSlot. Every row in that bounded candidate set is
    // then fully decoded; a scope/coordinate mismatch is corruption, never a
    // silent filter.
    let mut projected = Vec::new();
    for row in selected {
        let population_agent = AgentId::parse(row.try_get::<String, _>("population_agent_id")?)
            .map_err(|_| ConversationAgentRuntimeStoreError::SlotIdentity)?;
        let population_source = population
            .iter()
            .find(|member| member.agent == population_agent)
            .ok_or(ConversationAgentRuntimeStoreError::PopulationIdentity)?
            .source
            .clone();
        let stored_agent = AgentId::parse(row.try_get::<String, _>("stored_agent_id")?)
            .map_err(|_| ConversationAgentRuntimeStoreError::SlotIdentity)?;
        let slot_agent = AgentId::parse(row.try_get::<String, _>("slot_agent_id")?)
            .map_err(|_| ConversationAgentRuntimeStoreError::SlotIdentity)?;
        if population_agent != stored_agent || stored_agent != slot_agent {
            return Err(ConversationAgentRuntimeStoreError::SlotIdentity);
        }
        let slot_conversation = row
            .try_get::<Option<String>, _>("slot_conversation_id")?
            .map(ConversationId::parse)
            .transpose()
            .map_err(|_| ConversationAgentRuntimeStoreError::SlotIdentity)?;
        let session_scope: String = row.try_get("session_scope")?;
        let slot = match AgentSessionScope::parse(&session_scope)
            .ok_or(ConversationAgentRuntimeStoreError::SlotScope)?
        {
            AgentSessionScope::Agent => {
                if slot_conversation.is_some() {
                    return Err(ConversationAgentRuntimeStoreError::SlotScope);
                }
                RuntimeScope::AgentSlot {
                    agent: stored_agent.clone(),
                }
            }
            AgentSessionScope::Conversation => {
                let source =
                    slot_conversation.ok_or(ConversationAgentRuntimeStoreError::SlotScope)?;
                if &source != requested {
                    return Err(ConversationAgentRuntimeStoreError::SlotScope);
                }
                RuntimeScope::ConversationSlot {
                    agent: stored_agent.clone(),
                    source,
                }
            }
        };
        let generation = RuntimeGeneration::new(row.try_get("generation")?)
            .map_err(|_| ConversationAgentRuntimeStoreError::Generation)?;
        let status = RuntimeProjectionStatus::parse(&row.try_get::<String, _>("observed_status")?)
            .map_err(|_| ConversationAgentRuntimeStoreError::Status)?;
        let pid = row
            .try_get::<Option<i64>, _>("observed_pid")?
            .map(RuntimePid::new)
            .transpose()
            .map_err(|_| ConversationAgentRuntimeStoreError::Pid)?;
        let cli_session = row
            .try_get::<Option<String>, _>("cli_session_id")?
            .map(CliSessionId::parse)
            .transpose()
            .map_err(|_| ConversationAgentRuntimeStoreError::CliSession)?;
        projected.push(
            ConversationAgentRuntimeRow::projected(
                population_source,
                population_agent,
                slot,
                generation,
                cli_session,
                pid,
                status,
            )
            .map_err(|_| ConversationAgentRuntimeStoreError::Projection)?,
        );
    }
    Ok(projected)
}
