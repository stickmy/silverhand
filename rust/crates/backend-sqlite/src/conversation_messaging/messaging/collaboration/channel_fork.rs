//! The internal agent channel-fork write authority.
//!
//! ONE `BEGIN IMMEDIATE` transaction that re-validates every gate under the lock
//! (workspace exists + regular, each member agent is a workspace member, the
//! fork-source conversation exists + the acting agent is a member + its workspace
//! matches, the title is free), inserts the channel (`spawned_by_agent_id` = the
//! actor) + member agents + the source's INHERITED human roster, and appends the
//! two seed messages — a `channel_context` system message then the agent's initial
//! prompt — through the canonical seq/head primitives so the new channel's
//! `latest_message_seq`/stream head advance exactly as a normal send's. Any refusal
//! rolls the whole transaction back (no channel, no members, no messages).

use silverhand_backend_core::{
    conversation_messaging::messaging::collaboration::channel_fork::{
        ChannelForkRefusal, ChannelForkRequest, ChannelForkWrite, CommittedChannelFork,
    },
    foundation::persistence::PersistenceFailure,
};
use sqlx::{Row, Sqlite, Transaction};
use thiserror::Error;

use crate::conversation_messaging::messaging::MessagingStore;
use crate::conversation_messaging::messaging::{
    FROM_KIND_SYSTEM, INSERT_MESSAGE, ReserveSeqFault, bump_stream_head_on, reserve_next_seq_on,
};

const ROOT_WRITE_BEGIN: &str = "BEGIN IMMEDIATE";
const FROM_KIND_AGENT: &str = "agent";

#[derive(Clone)]
pub struct SqliteChannelFork {
    pool: crate::foundation::database::SqliteDatabase,
}

impl<E> MessagingStore<E> {
    pub fn channel_fork(&self) -> SqliteChannelFork {
        SqliteChannelFork {
            pool: self.pool().clone(),
        }
    }
}

impl SqliteChannelFork {
    pub fn from_pool(pool: impl Into<crate::foundation::database::SqliteDatabase>) -> Self {
        let pool = pool.into();
        Self { pool }
    }
}

#[derive(Debug, Error)]
pub enum ChannelForkStoreError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("committed message ordinal is outside its domain")]
    Ordinal,
    #[error("seed message metadata could not be encoded")]
    Serialization(#[source] serde_json::Error),
    #[error("id-minting environment unavailable")]
    Environment,
    #[error("a committed row carried a malformed identifier: {0}")]
    CorruptRow(&'static str),
}

impl From<silverhand_backend_core::conversation_messaging::messaging::EnvironmentUnavailable>
    for ChannelForkStoreError
{
    fn from(
        _: silverhand_backend_core::conversation_messaging::messaging::EnvironmentUnavailable,
    ) -> Self {
        Self::Environment
    }
}

impl From<ChannelForkStoreError> for PersistenceFailure {
    fn from(value: ChannelForkStoreError) -> Self {
        Self::from_source(value)
    }
}

impl ChannelForkWrite for SqliteChannelFork {
    async fn fork_channel<
        B: silverhand_backend_core::conversation_messaging::messaging::environment::SendEntropy
            + Send
            + Sync,
        C: silverhand_backend_core::conversation_messaging::messaging::collaboration::channel_fork::ChannelForkClock,
    >(
        &self,
        request: &ChannelForkRequest,
        entropy: &B,
        clock: &C,
    ) -> Result<Result<CommittedChannelFork, ChannelForkRefusal>, PersistenceFailure>{
        use silverhand_backend_core::conversation_messaging::messaging::collaboration::channel_fork::SealedSeedMessage;
        use silverhand_backend_core::conversation_messaging::messaging::{
            MessageId, mint_conversation_id,
        };
        let mut tx = self
            .pool
            .begin_with(ROOT_WRITE_BEGIN)
            .await
            .map_err(ChannelForkStoreError::from)?;

        // Workspace exists + is regular (owner_agent_id IS NULL).
        let Some(owner_agent_id) = workspace_owner(&mut tx, request).await? else {
            return refuse(tx, ChannelForkRefusal::WorkspaceNotFound).await;
        };
        if owner_agent_id.is_some() {
            return refuse(tx, ChannelForkRefusal::WorkspaceNotRegular).await;
        }

        // Fresh `create_channels` capability replay: a precheck-then-revoke
        // must not still create a channel.
        let cap: Option<i64> = sqlx::query_scalar(
            "SELECT CASE WHEN w.owner_agent_id = ? OR EXISTS (SELECT 1 FROM \
             workspace_agent_capabilities c WHERE c.workspace_id = w.id AND c.agent_id = ? \
             AND c.capability = 'create_channels') THEN 1 ELSE 0 END FROM workspaces w WHERE w.id = ?",
        )
        .bind(request.actor_agent.as_str())
        .bind(request.actor_agent.as_str())
        .bind(request.workspace.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(ChannelForkStoreError::from)?;
        if !cap.is_some_and(|allowed| allowed != 0) {
            return refuse(tx, ChannelForkRefusal::CapabilityDenied).await;
        }

        // The fork-source conversation exists, the actor is a member, and it belongs
        // to the same workspace.
        let Some(source_workspace) = source_conversation_workspace(&mut tx, request).await? else {
            return refuse(tx, ChannelForkRefusal::SourceConversationNotFound).await;
        };
        let actor_in_source: i64 = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM conversation_agents WHERE conversation_id = ? AND agent_id = ?)",
        )
        .bind(request.source_conversation.as_str())
        .bind(request.actor_agent.as_str())
        .fetch_one(&mut *tx)
        .await
        .map_err(ChannelForkStoreError::from)?;
        if actor_in_source != 1 {
            return refuse(tx, ChannelForkRefusal::AgentNotMemberOfSource).await;
        }
        if source_workspace.as_deref() != Some(request.workspace.as_str()) {
            return refuse(tx, ChannelForkRefusal::AgentWorkspaceMismatch).await;
        }

        // The forked channel inherits the SOURCE conversation's member agents,
        // re-read under the lock (the reference `conversations.listAgents(source)`).
        // Each stored id is DECODED into a typed AgentId here; a malformed row is a
        // typed CorruptRow fault (whole rollback), never silently dropped nor swapped
        // for the actor — so the committed roster and the effect audience cannot
        // diverge.
        let member_agent_id_rows: Vec<String> = sqlx::query_scalar(
            "SELECT agent_id FROM conversation_agents WHERE conversation_id = ?",
        )
        .bind(request.source_conversation.as_str())
        .fetch_all(&mut *tx)
        .await
        .map_err(ChannelForkStoreError::from)?;
        let mut member_agent_ids: Vec<silverhand_backend_core::foundation::identity::AgentId> =
            Vec::with_capacity(member_agent_id_rows.len());
        for id in member_agent_id_rows {
            let parsed = silverhand_backend_core::foundation::identity::AgentId::parse(id)
                .map_err(|_| ChannelForkStoreError::CorruptRow("conversation_agents.agent_id"))?;
            member_agent_ids.push(parsed);
        }

        // Per-source-agent workspace-membership gate: each derived member
        // must EXIST and be a member of the target workspace. A conversation edge is
        // NOT workspace-membership authority; a stale source member is refused, whole
        // rollback. Checked BEFORE inserting anything.
        for agent in &member_agent_ids {
            let member: i64 = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM agents a JOIN workspace_members wm \
                 ON wm.agent_id = a.id WHERE a.id = ? AND wm.workspace_id = ?)",
            )
            .bind(agent.as_str())
            .bind(request.workspace.as_str())
            .fetch_one(&mut *tx)
            .await
            .map_err(ChannelForkStoreError::from)?;
            if member != 1 {
                let refusal = ChannelForkRefusal::AgentNotWorkspaceMember {
                    agent: agent.clone(),
                };
                return refuse(tx, refusal).await;
            }
        }

        // Per-WORKSPACE title-collision gate: a `(workspace_id, title_key)`
        // scope, matching the canonical unique index — NOT a global key.
        let title_taken: i64 = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM conversations WHERE workspace_id = ? AND title_key = ? \
             AND placement_kind = 'workspace')",
        )
        .bind(request.workspace.as_str())
        .bind(request.title.key())
        .fetch_one(&mut *tx)
        .await
        .map_err(ChannelForkStoreError::from)?;
        if title_taken == 1 {
            // Title-address REFUSED event, bound to the workspace + key.
            // WARN to match the canonical `title-address-log` family (default sink is
            // warn-only); an INFO would be invisible in production.
            tracing::warn!(
                title_address_event = "refused",
                workspace_id = request.workspace.as_str(),
                conversation_id = tracing::field::Empty,
                key = request.title.key(),
                "channel-fork title address refused"
            );
            return refuse(
                tx,
                ChannelForkRefusal::TitleTaken {
                    title: request.title.title().to_owned(),
                },
            )
            .await;
        }

        // All gates passed → NOW mint ids + sample the clock: a
        // failing entropy cannot mask a product refusal, and the clock is an injectable
        // writer input (not a bare SystemTime::now()).
        let new_channel_id = mint_conversation_id(entropy).map_err(ChannelForkStoreError::from)?;
        let context_message_id = MessageId::mint(entropy).map_err(ChannelForkStoreError::from)?;
        let prompt_message_id = MessageId::mint(entropy).map_err(ChannelForkStoreError::from)?;
        let now_ms = clock.now_ms();

        // Snapshot the source title + actor identity for the context metadata + the
        // seed body composition. The source conversation exists (gated
        // above) and the actor is a verified source member + capability holder, so a
        // MISSING title / MISSING actor row is a corrupt committed state — a typed
        // CorruptRow fault (whole rollback), NEVER a silent empty-string / null-null
        // fallback that would write a channel with a wrong-looking seed.
        let source_title: String =
            sqlx::query_scalar("SELECT title FROM conversations WHERE id = ?")
                .bind(request.source_conversation.as_str())
                .fetch_optional(&mut *tx)
                .await
                .map_err(ChannelForkStoreError::from)?
                .flatten()
                .ok_or(ChannelForkStoreError::CorruptRow("conversations.title"))?;
        let (actor_name, actor_display_name): (Option<String>, Option<String>) =
            sqlx::query_as("SELECT name, display_name FROM agents WHERE id = ?")
                .bind(request.actor_agent.as_str())
                .fetch_optional(&mut *tx)
                .await
                .map_err(ChannelForkStoreError::from)?
                .ok_or(ChannelForkStoreError::CorruptRow("agents.actor_row"))?;

        // Insert the channel (agent-forked → private, spawned_by the actor).
        sqlx::query(
            "INSERT INTO conversations (id, workspace_id, placement_kind, roster_policy, title, title_key, status, visibility, spawned_by_agent_id, created_at, updated_at) \
             VALUES (?, ?, 'workspace', 'open', ?, ?, 'idle', 'private', ?, ?, ?)",
        )
        .bind(new_channel_id.as_str())
        .bind(request.workspace.as_str())
        .bind(request.title.title())
        .bind(request.title.key())
        .bind(request.actor_agent.as_str())
        .bind(now_ms)
        .bind(now_ms)
        .execute(&mut *tx)
        .await
        .map_err(ChannelForkStoreError::from)?;

        // Member agents (the source's inherited roster, gated above).
        for agent in &member_agent_ids {
            sqlx::query(
                "INSERT OR IGNORE INTO conversation_agents (conversation_id, agent_id, joined_at) VALUES (?, ?, ?)",
            )
            .bind(new_channel_id.as_str())
            .bind(agent.as_str())
            .bind(now_ms)
            .execute(&mut *tx)
            .await
            .map_err(ChannelForkStoreError::from)?;
        }

        // Inherit the source conversation's human roster.
        let source_users: Vec<String> =
            sqlx::query_scalar("SELECT user_id FROM conversation_users WHERE conversation_id = ?")
                .bind(request.source_conversation.as_str())
                .fetch_all(&mut *tx)
                .await
                .map_err(ChannelForkStoreError::from)?;
        for user_id in &source_users {
            sqlx::query(
                "INSERT OR IGNORE INTO conversation_users (conversation_id, user_id, joined_at) VALUES (?, ?, ?)",
            )
            .bind(new_channel_id.as_str())
            .bind(user_id)
            .bind(now_ms)
            .execute(&mut *tx)
            .await
            .map_err(ChannelForkStoreError::from)?;
        }

        // The context (system) seed body composes the reference text:
        // `Channel created from '<source.title>'. Reason: <context_summary>`.
        let context_body = format!(
            "Channel created from '{}'. Reason: {}",
            source_title, request.context_summary
        );
        let context_metadata = serde_json::to_string(&serde_json::json!({
            "subtype": "channel_context",
            "source_conversation_id": request.source_conversation.as_str(),
            "source_conversation_title": source_title,
            "created_by_agent_id": request.actor_agent.as_str(),
            "created_by_agent_name": actor_name,
        }))
        .map_err(ChannelForkStoreError::Serialization)?;

        let context_seq = append_seed_message(
            &mut tx,
            &new_channel_id,
            context_message_id.as_str(),
            FROM_KIND_SYSTEM,
            None,
            None,
            None,
            None,
            &context_body,
            Some(&context_metadata),
            now_ms,
        )
        .await?;
        // The prompt is AGENT-authored and carries the send-time author snapshot from
        // the fresh actor row, so a later agent delete resolves `deleted`.
        let prompt_seq = append_seed_message(
            &mut tx,
            &new_channel_id,
            prompt_message_id.as_str(),
            FROM_KIND_AGENT,
            Some(request.actor_agent.as_str()),
            Some(request.actor_agent.as_str()),
            actor_name.as_deref(),
            actor_display_name.as_deref(),
            &request.initial_prompt,
            None,
            now_ms,
        )
        .await?;

        tx.commit().await.map_err(ChannelForkStoreError::from)?;

        // Title-address TAKEN event, emitted ONLY after the commit
        // succeeds — a commit fault must never falsely log a taken address. WARN so it
        // lands in the default `~/.silverhand/logs` warn sink, matching the canonical
        // `title-address-log` family (an INFO would vanish under the default config).
        tracing::warn!(
            title_address_event = "taken",
            workspace_id = request.workspace.as_str(),
            conversation_id = new_channel_id.as_str(),
            key = request.title.key(),
            "channel-fork title address taken"
        );

        // The committed roster is the member set, one-to-one with what was inserted
        // (each id was decoded fail-closed above), so the effect audience cannot
        // diverge from the durable roster.
        Ok(Ok(CommittedChannelFork {
            channel: new_channel_id.clone(),
            actor_agent: request.actor_agent.clone(),
            context_message: SealedSeedMessage {
                id: context_message_id,
                conversation: new_channel_id.clone(),
                seq: context_seq,
                content: context_body,
            },
            prompt_message: SealedSeedMessage {
                id: prompt_message_id,
                conversation: new_channel_id,
                seq: prompt_seq,
                content: request.initial_prompt.clone(),
            },
            member_agents: member_agent_ids,
            member_users: source_users,
        }))
    }
}

/// Append one seed message via the canonical seq/head primitives, returning its seq.
#[allow(clippy::too_many_arguments)]
async fn append_seed_message(
    tx: &mut Transaction<'_, Sqlite>,
    channel: &silverhand_backend_core::foundation::identity::ConversationId,
    message_id: &str,
    from_kind: &str,
    from_agent_id: Option<&str>,
    author_id: Option<&str>,
    author_name: Option<&str>,
    author_display_name: Option<&str>,
    content: &str,
    metadata: Option<&str>,
    now_ms: i64,
) -> Result<i64, PersistenceFailure> {
    let seq = reserve_next_seq_on(&mut *tx, channel)
        .await
        .map_err(|fault| match fault {
            ReserveSeqFault::Driver(error) => {
                PersistenceFailure::from_source(ChannelForkStoreError::Sql(error))
            }
            ReserveSeqFault::Ordinal(_) => {
                PersistenceFailure::from_source(ChannelForkStoreError::Ordinal)
            }
        })?;
    sqlx::query(INSERT_MESSAGE)
        .bind(message_id)
        .bind(channel.as_str())
        .bind(from_kind)
        .bind(from_agent_id)
        .bind(None::<&str>)
        .bind(content)
        .bind(metadata)
        .bind(seq.get())
        .bind(now_ms)
        .bind(None::<&str>)
        .bind(author_id)
        .bind(author_name)
        .bind(author_display_name)
        .bind(None::<&str>)
        .execute(&mut **tx)
        .await
        .map_err(ChannelForkStoreError::from)?;
    bump_stream_head_on(&mut *tx, channel, None, seq)
        .await
        .map_err(|error| PersistenceFailure::from_source(ChannelForkStoreError::Sql(error)))?;
    Ok(seq.get())
}

async fn refuse(
    tx: Transaction<'_, Sqlite>,
    refusal: ChannelForkRefusal,
) -> Result<Result<CommittedChannelFork, ChannelForkRefusal>, PersistenceFailure> {
    tx.rollback().await.map_err(ChannelForkStoreError::from)?;
    Ok(Err(refusal))
}

/// The workspace's owner-agent arm (`Some(Some(agent))` = agent-home; `Some(None)` =
/// regular; `None` = missing).
async fn workspace_owner(
    tx: &mut Transaction<'_, Sqlite>,
    request: &ChannelForkRequest,
) -> Result<Option<Option<String>>, PersistenceFailure> {
    let row = sqlx::query("SELECT owner_agent_id FROM workspaces WHERE id = ?")
        .bind(request.workspace.as_str())
        .fetch_optional(&mut **tx)
        .await
        .map_err(ChannelForkStoreError::from)?;
    Ok(match row {
        None => None,
        Some(row) => Some(
            row.try_get::<Option<String>, _>("owner_agent_id")
                .map_err(ChannelForkStoreError::from)?,
        ),
    })
}

/// The source conversation's workspace, or `None` when the conversation is missing.
async fn source_conversation_workspace(
    tx: &mut Transaction<'_, Sqlite>,
    request: &ChannelForkRequest,
) -> Result<Option<Option<String>>, PersistenceFailure> {
    let row = sqlx::query("SELECT workspace_id FROM conversations WHERE id = ?")
        .bind(request.source_conversation.as_str())
        .fetch_optional(&mut **tx)
        .await
        .map_err(ChannelForkStoreError::from)?;
    Ok(match row {
        None => None,
        Some(row) => Some(
            row.try_get::<Option<String>, _>("workspace_id")
                .map_err(ChannelForkStoreError::from)?,
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use silverhand_backend_core::conversation_messaging::messaging::conversation_title::CanonicalConversationTitle;
    use silverhand_backend_core::conversation_messaging::messaging::environment::{
        EnvironmentUnavailable, SendEntropy,
    };
    use silverhand_backend_core::foundation::identity::{AgentId, ConversationId, WorkspaceId};
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use sqlx::{Connection, Executor, SqliteConnection};

    const CURRENT_SCHEMA: &str = include_str!(
        "../../../../../../../rust/test-fixtures/migration-baseline/schema-current.sql"
    );

    // ws-1 (agent-a member+create_channels, agent-b member) with source `src`
    // (agents a+b, user u1). ws-2 for the mismatch + per-ws-title arms. home-x agent-home.
    const SEED: &str = "\
INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES('u1','owner','Owner',1,1); \
INSERT INTO hosts(id,name,kind,created_at) VALUES('h1','H','local',1); \
INSERT INTO agents(id,name,display_name,runtime,host_id,created_by_user_id,created_at,updated_at,session_scope) VALUES('agent-a','alice','Alice','claude','h1','u1',1,1,'conversation'); \
INSERT INTO agents(id,name,display_name,runtime,host_id,created_by_user_id,created_at,updated_at,session_scope) VALUES('agent-b','bob','Bob','codex','h1','u1',1,1,'conversation'); \
INSERT INTO agents(id,name,display_name,runtime,host_id,created_by_user_id,created_at,updated_at,session_scope) VALUES('agent-x','carol','Carol','codex','h1','u1',1,1,'conversation'); \
INSERT INTO workspaces(id,name,name_key,owner_agent_id,created_at,updated_at,owner_user_id) VALUES('ws-1','Team','team',NULL,1,1,'u1'); \
INSERT INTO workspaces(id,name,name_key,owner_agent_id,created_at,updated_at,owner_user_id) VALUES('ws-2','Other','other',NULL,1,1,'u1'); \
INSERT INTO workspaces(id,name,name_key,owner_agent_id,created_at,updated_at) VALUES('home-x',' ',NULL,'agent-x',1,1); \
INSERT INTO workspace_members(workspace_id,agent_id,joined_at) VALUES('ws-1','agent-a',1); \
INSERT INTO workspace_members(workspace_id,agent_id,joined_at) VALUES('ws-1','agent-b',1); \
INSERT INTO workspace_members(workspace_id,agent_id,joined_at) VALUES('ws-2','agent-a',1); \
INSERT INTO workspace_agent_capabilities(workspace_id,agent_id,capability) VALUES('ws-1','agent-a','create_channels'); \
INSERT INTO workspace_agent_capabilities(workspace_id,agent_id,capability) VALUES('ws-2','agent-a','create_channels'); \
INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,status,visibility,created_at,updated_at) VALUES('src','ws-1','workspace','open','Source','source','idle','private',1,1); \
INSERT INTO conversation_agents(conversation_id,agent_id,joined_at) VALUES('src','agent-a',1); \
INSERT INTO conversation_agents(conversation_id,agent_id,joined_at) VALUES('src','agent-b',1); \
INSERT INTO conversation_users(conversation_id,user_id,joined_at) VALUES('src','u1',1); \
INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,status,visibility,created_at,updated_at) VALUES('src2','ws-2','workspace','open','Source2','source2','idle','private',1,1); \
INSERT INTO conversation_agents(conversation_id,agent_id,joined_at) VALUES('src2','agent-a',1);";

    // Deterministic entropy so ids are stable per call (distinct per invocation via a seed).
    struct SeededEntropy(std::sync::atomic::AtomicU8);
    impl SendEntropy for SeededEntropy {
        fn fill(&self, buffer: &mut [u8]) -> Result<(), EnvironmentUnavailable> {
            for byte in buffer.iter_mut() {
                *byte = self
                    .0
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                    .wrapping_add(1);
            }
            Ok(())
        }
    }
    // Always-failing entropy: proves id allocation is AFTER the gates (#31).
    struct FailingEntropy;
    impl SendEntropy for FailingEntropy {
        fn fill(&self, _: &mut [u8]) -> Result<(), EnvironmentUnavailable> {
            Err(EnvironmentUnavailable::from_source(std::io::Error::other(
                "no entropy",
            )))
        }
    }

    async fn open_seeded() -> (tempfile::TempDir, sqlx::SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cf.sqlite");
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
            .max_connections(4)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&path)
                    .foreign_keys(true),
            )
            .await
            .unwrap();
        (dir, pool)
    }
    fn req(source: &str, workspace: &str, title: &str) -> ChannelForkRequest {
        ChannelForkRequest {
            workspace: WorkspaceId::parse(workspace).unwrap(),
            source_conversation: ConversationId::parse(source).unwrap(),
            actor_agent: AgentId::parse("agent-a").unwrap(),
            title: CanonicalConversationTitle::parse(title).unwrap(),
            context_summary: "why".to_owned(),
            initial_prompt: "hello team".to_owned(),
        }
    }
    fn entropy() -> SeededEntropy {
        SeededEntropy(std::sync::atomic::AtomicU8::new(0))
    }

    struct TestClock(i64);
    impl silverhand_backend_core::conversation_messaging::messaging::collaboration::channel_fork::ChannelForkClock
        for TestClock
    {
        fn now_ms(&self) -> i64 {
            self.0
        }
    }
    fn clock() -> TestClock {
        TestClock(1000)
    }

    #[tokio::test]
    async fn fork_seals_post_images_with_prompt_author_snapshot_and_context_body() {
        let (_d, pool) = open_seeded().await;
        let w = SqliteChannelFork::from_pool(pool.clone());
        let committed = w
            .fork_channel(&req("src", "ws-1", "Design Review"), &entropy(), &clock())
            .await
            .unwrap()
            .unwrap();
        // Sealed seed post-images.
        assert_eq!(committed.context_message.seq, 1);
        assert_eq!(committed.prompt_message.seq, 2);
        assert_eq!(
            committed.context_message.content,
            "Channel created from 'Source'. Reason: why"
        );
        assert_eq!(committed.prompt_message.content, "hello team");
        // Members inherited from source.
        let mut agents: Vec<&str> = committed.member_agents.iter().map(|a| a.as_str()).collect();
        agents.sort();
        assert_eq!(agents, vec!["agent-a", "agent-b"]);
        assert_eq!(committed.member_users, vec!["u1".to_owned()]);
        // Prompt carries the send-time author snapshot (#34): delete actor → still resolvable.
        let (aid, aname): (Option<String>, Option<String>) =
            sqlx::query_as("SELECT author_id, author_name FROM messages WHERE id = ?")
                .bind(committed.prompt_message.id.as_str())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(aid.as_deref(), Some("agent-a"));
        assert_eq!(aname.as_deref(), Some("alice"));
    }

    #[tokio::test]
    async fn per_workspace_title_scope_and_collision() {
        let (_d, pool) = open_seeded().await;
        let w = SqliteChannelFork::from_pool(pool.clone());
        let e = entropy();
        // "Dup" in ws-1.
        w.fork_channel(&req("src", "ws-1", "Dup"), &e, &clock())
            .await
            .unwrap()
            .unwrap();
        // Same key in ws-2 SUCCEEDS (per-workspace scope #15).
        assert!(
            w.fork_channel(&req("src2", "ws-2", "Dup"), &e, &clock())
                .await
                .unwrap()
                .is_ok()
        );
        // Same key in ws-1 again → TitleTaken with the canonical title (#29).
        match w
            .fork_channel(&req("src", "ws-1", "Dup"), &e, &clock())
            .await
            .unwrap()
        {
            Err(ChannelForkRefusal::TitleTaken { title }) => assert_eq!(title, "Dup"),
            other => panic!("expected TitleTaken, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fresh_capability_and_source_member_gate_and_late_id_alloc() {
        let (dir, pool) = open_seeded().await;
        let w = SqliteChannelFork::from_pool(pool.clone());
        let e = entropy();
        // Revoke create_channels on ws-1 → CapabilityDenied.
        let side = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(dir.path().join("cf.sqlite"))
                    .foreign_keys(true),
            )
            .await
            .unwrap();
        sqlx::query("DELETE FROM workspace_agent_capabilities WHERE workspace_id='ws-1' AND agent_id='agent-a'")
            .execute(&side).await.unwrap();
        assert!(matches!(
            w.fork_channel(&req("src", "ws-1", "X"), &e, &clock())
                .await
                .unwrap(),
            Err(ChannelForkRefusal::CapabilityDenied)
        ));
        // Restore cap; make agent-b (a source member) NOT a ws member → AgentNotWorkspaceMember.
        sqlx::query("INSERT INTO workspace_agent_capabilities(workspace_id,agent_id,capability) VALUES('ws-1','agent-a','create_channels')")
            .execute(&side).await.unwrap();
        sqlx::query(
            "DELETE FROM workspace_members WHERE workspace_id='ws-1' AND agent_id='agent-b'",
        )
        .execute(&side)
        .await
        .unwrap();
        assert!(matches!(
            w.fork_channel(&req("src", "ws-1", "Y"), &e, &clock())
                .await
                .unwrap(),
            Err(ChannelForkRefusal::AgentNotWorkspaceMember { .. })
        ));
        // Failing entropy × a REFUSAL still returns the product refusal, not a 500
        // (#31: id alloc is after the gates). Title-taken with failing entropy:
        w.fork_channel(&req("src2", "ws-2", "Z"), &e, &clock())
            .await
            .unwrap()
            .unwrap();
        match w
            .fork_channel(&req("src2", "ws-2", "Z"), &FailingEntropy, &clock())
            .await
            .unwrap()
        {
            Err(ChannelForkRefusal::TitleTaken { .. }) => {}
            other => panic!("failing entropy must not mask TitleTaken, got {other:?}"),
        }
        // No orphan channel from the refused forks.
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM conversations WHERE title_key IN ('x','y')")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn the_injected_clock_stamps_the_committed_rows_not_a_wall_clock() {
        // The fork writer samples an INJECTABLE clock (not SystemTime::now),
        // so a held-writer / fixed clock deterministically stamps every committed row.
        let (_d, pool) = open_seeded().await;
        let w = SqliteChannelFork::from_pool(pool.clone());
        let committed = w
            .fork_channel(&req("src", "ws-1", "Clocked"), &entropy(), &TestClock(4242))
            .await
            .unwrap()
            .unwrap();
        let (created, updated): (i64, i64) =
            sqlx::query_as("SELECT created_at, updated_at FROM conversations WHERE id = ?")
                .bind(committed.channel.as_str())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!((created, updated), (4242, 4242));
        // The seed messages carry the same injected instant.
        let msg_created: i64 = sqlx::query_scalar("SELECT created_at FROM messages WHERE id = ?")
            .bind(committed.context_message.id.as_str())
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(msg_created, 4242);
    }

    #[tokio::test]
    async fn a_corrupt_source_roster_id_is_a_typed_fault_with_zero_writes() {
        // A source roster row whose agent_id cannot decode into a
        // typed AgentId (here an empty id) is a typed CorruptRow persistence fault that
        // rolls the whole tx back — never silently dropped nor swapped for the actor,
        // so the committed roster and the effect audience cannot diverge.
        let (_d, pool) = open_seeded().await;
        // Seed an agent with an EMPTY id (AgentId::parse rejects ""), make it a ws-1
        // member, and add it to the source roster so the writer reads it back.
        sqlx::query("INSERT INTO agents(id,name,display_name,runtime,host_id,created_by_user_id,created_at,updated_at,session_scope) VALUES('','ghost','Ghost','codex','h1','u1',1,1,'conversation')")
            .execute(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO workspace_members(workspace_id,agent_id,joined_at) VALUES('ws-1','',1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO conversation_agents(conversation_id,agent_id,joined_at) VALUES('src','',1)")
            .execute(&pool).await.unwrap();
        let w = SqliteChannelFork::from_pool(pool.clone());
        let outcome = w
            .fork_channel(&req("src", "ws-1", "Corrupt"), &entropy(), &clock())
            .await;
        assert!(
            outcome.is_err(),
            "a malformed roster id must be a typed fault, not a silent drop/actor-swap: {outcome:?}"
        );
        // Zero writes: no channel for this title.
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM conversations WHERE title_key = 'corrupt'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(count, 0, "the whole tx rolled back");
    }
}
