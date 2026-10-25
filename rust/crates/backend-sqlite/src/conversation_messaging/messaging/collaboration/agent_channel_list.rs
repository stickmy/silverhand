//! The agent-scoped channel-list read over `conversations` + `conversation_agents`.
//!
//! Member arm ∪ discovery arm, `is_member` computed independently, ordered
//! `(archived_at IS NOT NULL) ASC, updated_at DESC` — the exact
//! `listAgentVisibleChannels` query.

use silverhand_backend_core::{
    conversation_messaging::messaging::collaboration::agent_channel_list::{
        AgentChannelListRead, AgentListedChannel, ListChannelsRefusal, ListChannelsResult,
    },
    conversation_messaging::messaging::collaboration::channel_directory::{
        ChannelVisibility, DirectoryRoster,
    },
    conversation_messaging::messaging::collaboration::message_target_coordinates::ConversationPlacementKind,
    foundation::identity::{AgentId, ConversationId, WorkspaceId},
    foundation::persistence::PersistenceFailure,
};
use sqlx::{Row, Sqlite, Transaction};
use thiserror::Error;

use crate::conversation_messaging::messaging::MessagingStore;

/// The agent channel-list read authority — a shared-pool clone of the ONE store.
#[derive(Clone)]
pub struct SqliteAgentChannelList {
    pool: crate::foundation::database::SqliteDatabase,
}

impl<E> MessagingStore<E> {
    pub fn agent_channel_list(&self) -> SqliteAgentChannelList {
        SqliteAgentChannelList {
            pool: self.pool().clone(),
        }
    }
}

impl SqliteAgentChannelList {
    pub fn from_pool(pool: impl Into<crate::foundation::database::SqliteDatabase>) -> Self {
        let pool = pool.into();
        Self { pool }
    }
}

#[derive(Debug, Error)]
pub enum AgentChannelListError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("stored conversation identity is malformed")]
    MalformedIdentity,
    #[error("stored conversation roster policy is outside the closed set: {0}")]
    UnknownRoster(String),
    #[error("stored conversation visibility is outside the closed set: {0}")]
    UnknownVisibility(String),
    #[error("stored conversation placement_kind is outside the closed set")]
    UnknownPlacement,
}

impl From<AgentChannelListError> for PersistenceFailure {
    fn from(value: AgentChannelListError) -> Self {
        Self::from_source(value)
    }
}

const SELECT_AGENT_VISIBLE_CHANNELS: &str = "\
    SELECT c.id AS id, c.title AS title, c.roster_policy AS roster_policy, \
           c.visibility AS visibility, c.archived_at AS archived_at, c.updated_at AS updated_at, \
           EXISTS (SELECT 1 FROM conversation_agents ca \
                    WHERE ca.conversation_id = c.id AND ca.agent_id = ?) AS is_member \
      FROM conversations c \
     WHERE c.workspace_id = ? \
       AND ( \
         EXISTS (SELECT 1 FROM conversation_agents ca2 \
                  WHERE ca2.conversation_id = c.id AND ca2.agent_id = ?) \
         OR (c.placement_kind = 'workspace' AND c.roster_policy = 'open' \
             AND c.visibility = 'public' AND c.archived_at IS NULL) \
       ) \
     ORDER BY (c.archived_at IS NOT NULL) ASC, c.updated_at DESC";

impl AgentChannelListRead for SqliteAgentChannelList {
    async fn list_channels(
        &self,
        agent: &AgentId,
        source: Option<&ConversationId>,
        explicit_workspace: Option<&WorkspaceId>,
    ) -> Result<Result<ListChannelsResult, ListChannelsRefusal>, PersistenceFailure> {
        let mut tx = self
            .pool
            .begin_with("BEGIN DEFERRED")
            .await
            .map_err(AgentChannelListError::from)?;

        // Resolve the target workspace + authorize, IN this snapshot.
        let target = match resolve_target(&mut tx, agent, source, explicit_workspace).await? {
            Ok(target) => target,
            Err(refusal) => {
                tx.commit().await.map_err(AgentChannelListError::from)?;
                return Ok(Err(refusal));
            }
        };

        let rows = sqlx::query(SELECT_AGENT_VISIBLE_CHANNELS)
            .bind(agent.as_str())
            .bind(target.as_str())
            .bind(agent.as_str())
            .fetch_all(&mut *tx)
            .await
            .map_err(AgentChannelListError::from)?;
        let mut channels = Vec::with_capacity(rows.len());
        for row in &rows {
            channels.push(decode_channel(row)?);
        }
        tx.commit().await.map_err(AgentChannelListError::from)?;
        Ok(Ok(ListChannelsResult {
            workspace: target,
            channels,
        }))
    }
}

/// Resolve the target workspace from source/explicit + gate cross-workspace reach +
/// `read_messages`, all on the tx.
async fn resolve_target(
    tx: &mut Transaction<'_, Sqlite>,
    agent: &AgentId,
    source: Option<&ConversationId>,
    explicit_workspace: Option<&WorkspaceId>,
) -> Result<Result<WorkspaceId, ListChannelsRefusal>, PersistenceFailure> {
    // The source conversation's placement (existence + workspace), if any.
    let (source_workspace, source_placement_kind) = match source {
        Some(src) => match conversation_placement(tx, src).await? {
            Some((kind, workspace)) => (workspace, Some(kind)),
            None => return Ok(Err(ListChannelsRefusal::SourceNotFound)),
        },
        None => (None, None),
    };

    let (target, cross_workspace) = match (explicit_workspace, source_workspace) {
        (Some(explicit), src) => (explicit.clone(), src.as_ref() != Some(explicit)),
        (None, Some(src)) => (src, false),
        (None, None) => {
            return Ok(Err(ListChannelsRefusal::WorkspaceScopeRequired {
                source_placement_kind,
            }));
        }
    };

    if cross_workspace && !agent_reaches_workspace(tx, agent, &target).await? {
        return Ok(Err(ListChannelsRefusal::WorkspaceForbidden));
    }
    if !may_read_messages(tx, agent, &target).await? {
        return Ok(Err(ListChannelsRefusal::CapabilityDenied));
    }
    Ok(Ok(target))
}

/// A conversation's `(placement_kind, workspace_id)`, or `None` if it does not exist.
async fn conversation_placement(
    tx: &mut Transaction<'_, Sqlite>,
    conversation: &ConversationId,
) -> Result<Option<(ConversationPlacementKind, Option<WorkspaceId>)>, PersistenceFailure> {
    let Some(row) =
        sqlx::query("SELECT placement_kind, workspace_id FROM conversations WHERE id = ?")
            .bind(conversation.as_str())
            .fetch_optional(&mut **tx)
            .await
            .map_err(AgentChannelListError::from)?
    else {
        return Ok(None);
    };
    let placement_raw: String = row
        .try_get("placement_kind")
        .map_err(AgentChannelListError::from)?;
    let kind = ConversationPlacementKind::parse(&placement_raw)
        .ok_or(AgentChannelListError::UnknownPlacement)?;
    let workspace_raw: Option<String> = row
        .try_get("workspace_id")
        .map_err(AgentChannelListError::from)?;
    let workspace = workspace_raw
        .map(WorkspaceId::parse)
        .transpose()
        .map_err(|_| AgentChannelListError::MalformedIdentity)?;
    Ok(Some((kind, workspace)))
}

async fn agent_reaches_workspace(
    tx: &mut Transaction<'_, Sqlite>,
    agent: &AgentId,
    workspace: &WorkspaceId,
) -> Result<bool, PersistenceFailure> {
    let reaches: Option<i64> = sqlx::query_scalar(
        "SELECT CASE WHEN w.owner_agent_id = ? OR (w.owner_agent_id IS NULL AND EXISTS (\
         SELECT 1 FROM workspace_members wm WHERE wm.workspace_id = w.id AND wm.agent_id = ?)) \
         THEN 1 ELSE 0 END FROM workspaces w WHERE w.id = ?",
    )
    .bind(agent.as_str())
    .bind(agent.as_str())
    .bind(workspace.as_str())
    .fetch_optional(&mut **tx)
    .await
    .map_err(AgentChannelListError::from)?;
    Ok(reaches.is_some_and(|allowed| allowed != 0))
}

async fn may_read_messages(
    tx: &mut Transaction<'_, Sqlite>,
    agent: &AgentId,
    workspace: &WorkspaceId,
) -> Result<bool, PersistenceFailure> {
    let granted = crate::foundation::capability::agent_has_workspace_capability(
        &mut **tx,
        workspace.as_str(),
        agent.as_str(),
        "read_messages",
    )
    .await
    .map_err(AgentChannelListError::from)?;
    Ok(granted)
}

fn decode_channel(row: &sqlx::sqlite::SqliteRow) -> Result<AgentListedChannel, PersistenceFailure> {
    let id: String = row.try_get("id").map_err(AgentChannelListError::from)?;
    let id = ConversationId::parse(id).map_err(|_| AgentChannelListError::MalformedIdentity)?;
    let roster_raw: String = row
        .try_get("roster_policy")
        .map_err(AgentChannelListError::from)?;
    let roster_policy = DirectoryRoster::parse(&roster_raw)
        .map_err(|_| AgentChannelListError::UnknownRoster(roster_raw))?;
    let visibility_raw: String = row
        .try_get("visibility")
        .map_err(AgentChannelListError::from)?;
    let visibility = ChannelVisibility::parse(&visibility_raw)
        .map_err(|_| AgentChannelListError::UnknownVisibility(visibility_raw))?;
    let archived_at: Option<i64> = row
        .try_get("archived_at")
        .map_err(AgentChannelListError::from)?;
    let is_member: i64 = row
        .try_get("is_member")
        .map_err(AgentChannelListError::from)?;
    Ok(AgentListedChannel {
        id,
        title: row.try_get("title").map_err(AgentChannelListError::from)?,
        roster_policy,
        visibility,
        archived: archived_at.is_some(),
        is_member: is_member != 0,
        updated_at: silverhand_backend_core::foundation::wire_number::JsSafeInteger::new(
            row.try_get("updated_at")
                .map_err(AgentChannelListError::from)?,
        )
        .map(|v| v.get())
        .map_err(|_| AgentChannelListError::MalformedIdentity)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use sqlx::{Connection, Executor, SqliteConnection};

    const CURRENT_SCHEMA: &str = include_str!(
        "../../../../../../../rust/test-fixtures/migration-baseline/schema-current.sql"
    );

    // ws: agent-a member+cap; source conv `src` in ws (agent-a member). Channels:
    // `pub-open` (public open, non-member discovery), `priv-mem` (private, agent-a
    // member), `priv-other` (private, NOT agent-a — must not leak).
    const SEED: &str = "\
INSERT INTO users (id,name,display_name,created_at,updated_at) VALUES ('u1','u','U',1,1); \
INSERT INTO hosts (id,name,kind,created_at) VALUES ('h1','H','local',1); \
INSERT INTO agents (id,name,display_name,runtime,host_id,created_by_user_id,created_at,updated_at,session_scope) VALUES ('agent-a','alice','Alice','claude','h1','u1',1,1,'conversation'); \
INSERT INTO agents (id,name,display_name,runtime,host_id,created_by_user_id,created_at,updated_at,session_scope) VALUES ('agent-b','bob','Bob','codex','h1','u1',1,1,'conversation'); \
INSERT INTO workspaces (id,name,name_key,owner_agent_id,created_at,updated_at) VALUES ('ws','Team','team',NULL,1,1); \
INSERT INTO workspace_members (workspace_id,agent_id,joined_at) VALUES ('ws','agent-a',1); \
INSERT INTO workspace_agent_capabilities (workspace_id,agent_id,capability) VALUES ('ws','agent-a','read_messages'); \
INSERT INTO conversations (id,workspace_id,placement_kind,roster_policy,title,title_key,status,visibility,created_at,updated_at) VALUES ('src','ws','workspace','open','Src','src','idle','private',1,1); \
INSERT INTO conversation_agents (conversation_id,agent_id,joined_at) VALUES ('src','agent-a',1); \
INSERT INTO conversations (id,workspace_id,placement_kind,roster_policy,title,title_key,status,visibility,created_at,updated_at) VALUES ('pub-open','ws','workspace','open','Pub','pub','idle','public',1,20); \
INSERT INTO conversations (id,workspace_id,placement_kind,roster_policy,title,title_key,status,visibility,created_at,updated_at) VALUES ('priv-mem','ws','workspace','open','Mem','mem','idle','private',1,30); \
INSERT INTO conversation_agents (conversation_id,agent_id,joined_at) VALUES ('priv-mem','agent-a',1); \
INSERT INTO conversations (id,workspace_id,placement_kind,roster_policy,title,title_key,status,visibility,created_at,updated_at) VALUES ('priv-other','ws','workspace','open','Other','other','idle','private',1,40); \
INSERT INTO conversation_agents (conversation_id,agent_id,joined_at) VALUES ('priv-other','agent-b',1);";

    async fn seeded() -> (tempfile::TempDir, SqliteAgentChannelList) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lc.sqlite");
        let mut db = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&path)
                .create_if_missing(true)
                .foreign_keys(true),
        )
        .await
        .unwrap();
        db.execute(sqlx::raw_sql(CURRENT_SCHEMA)).await.unwrap();
        db.execute(sqlx::raw_sql(SEED)).await.unwrap();
        db.close().await.unwrap();
        let pool = SqlitePoolOptions::new()
            .max_connections(2)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&path)
                    .foreign_keys(true),
            )
            .await
            .unwrap();
        (dir, SqliteAgentChannelList::from_pool(pool))
    }
    fn agent(id: &str) -> AgentId {
        AgentId::parse(id).unwrap()
    }
    fn conv(id: &str) -> ConversationId {
        ConversationId::parse(id).unwrap()
    }
    fn ws(id: &str) -> WorkspaceId {
        WorkspaceId::parse(id).unwrap()
    }

    #[tokio::test]
    async fn defaults_to_source_workspace_and_returns_member_union_discovery_only() {
        let (_d, read) = seeded().await;
        // ConversationSlot source=src, no explicit workspace → source's ws.
        let result = read
            .list_channels(&agent("agent-a"), Some(&conv("src")), None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.workspace.as_str(), "ws");
        let ids: Vec<&str> = result.channels.iter().map(|c| c.id.as_str()).collect();
        // src (member) + pub-open (discovery) + priv-mem (member); NOT priv-other.
        assert!(ids.contains(&"src") && ids.contains(&"pub-open") && ids.contains(&"priv-mem"));
        assert!(
            !ids.contains(&"priv-other"),
            "a private channel the agent isn't in must not leak"
        );
    }

    #[tokio::test]
    async fn refusals() {
        let (_d, read) = seeded().await;
        // Missing source → SourceNotFound.
        assert!(matches!(
            read.list_channels(&agent("agent-a"), Some(&conv("nope")), None)
                .await
                .unwrap(),
            Err(ListChannelsRefusal::SourceNotFound)
        ));
        // AgentSlot (no source), no explicit → WorkspaceScopeRequired {None}.
        assert!(matches!(
            read.list_channels(&agent("agent-a"), None, None)
                .await
                .unwrap(),
            Err(ListChannelsRefusal::WorkspaceScopeRequired {
                source_placement_kind: None
            })
        ));
        // agent-b (member of priv-other but NOT ws member, no cap) → cross-ws forbidden
        // when naming ws explicitly with no source. agent-b is not a ws member.
        assert!(matches!(
            read.list_channels(&agent("agent-b"), None, Some(&ws("ws")))
                .await
                .unwrap(),
            Err(ListChannelsRefusal::WorkspaceForbidden)
        ));
    }

    #[tokio::test]
    async fn revoke_read_messages_flips_the_next_list_closed() {
        let (dir, read) = seeded().await;
        assert!(
            read.list_channels(&agent("agent-a"), Some(&conv("src")), None)
                .await
                .unwrap()
                .is_ok()
        );
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(dir.path().join("lc.sqlite"))
                    .foreign_keys(true),
            )
            .await
            .unwrap();
        sqlx::query("DELETE FROM workspace_agent_capabilities WHERE workspace_id='ws' AND agent_id='agent-a'")
            .execute(&pool).await.unwrap();
        assert!(matches!(
            read.list_channels(&agent("agent-a"), Some(&conv("src")), None)
                .await
                .unwrap(),
            Err(ListChannelsRefusal::CapabilityDenied)
        ));
    }
}
