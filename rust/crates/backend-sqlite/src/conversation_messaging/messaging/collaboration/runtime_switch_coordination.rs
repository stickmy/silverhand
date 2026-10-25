//! The conversation-inbox reads the runtime-switch saga runs POST-COMMIT to plan
//! its best-effort re-drive.
//!
//! The saga's [`SwitchConversationCoordination`] port lives in the messaging bounded
//! context, not the host-agent one: both the pending-inbox enumerations and the
//! placement-aware participation check are messaging-owned reads. Reusing the send
//! gate's [`SELECT_AGENT_PARTICIPATES`] fragment keeps the participation authority
//! single — a focus captured before clear is reused only while that one predicate
//! still accepts it, never against a paraphrase.
//!
//! Each enumeration mirrors its TypeScript repository statement verbatim:
//! `list_pending_for_agent` is the AgentSlot restart's oldest-first origin scan
//! (`in_flight_owner IS NULL`, `GROUP BY conversation_id ORDER BY MIN(id)`), and
//! `list_pending_for_agent_since` is the ConversationSlot's during-transition nudge
//! (`created_at >= ?`, in-flight rows INCLUDED, distinct ids ordered by id).

use silverhand_backend_core::{
    foundation::identity::{AgentId, ConversationId},
    foundation::persistence::PersistenceFailure,
    host_runtime::agent_runtime_switch::SwitchConversationCoordination,
};
use sqlx::{Row, sqlite::SqliteRow};

use crate::conversation_messaging::messaging::{MessagingStore, SELECT_AGENT_PARTICIPATES};

/// Every origin conversation with pending custody for one AgentSlot, ordered by
/// first inbox id so reconnect replay preserves global arrival order across
/// targets. `in_flight_owner IS NULL` is the drain-side narrowing: a claimed lease
/// is not a fresh restart candidate until it is rolled back under the lock.
const SELECT_PENDING_FOR_AGENT: &str = "SELECT conversation_id \
       FROM conversation_inboxes \
      WHERE agent_id = ? \
        AND delivered_at IS NULL \
        AND failed_at IS NULL \
        AND in_flight_owner IS NULL \
      GROUP BY conversation_id \
      ORDER BY MIN(id) ASC";

/// Pending pairs that gained undelivered input at or after `since_ms`, excluding
/// historical backlog. In-flight rows are DELIBERATELY included here (no
/// `in_flight_owner` narrowing): a config change nudges only what arrived while the
/// spawn gate was held, and a lease taken in that window is still such a message.
const SELECT_PENDING_FOR_AGENT_SINCE: &str = "SELECT DISTINCT conversation_id \
       FROM conversation_inboxes \
      WHERE agent_id = ? \
        AND delivered_at IS NULL \
        AND failed_at IS NULL \
        AND created_at >= ? \
      ORDER BY conversation_id ASC";

/// A pool-scoped read adapter: the saga's re-drive plan runs after the transition
/// commits and its lock releases, so it reads outside any write reservation.
#[derive(Clone)]
pub struct SqliteSwitchConversationCoordination {
    pool: crate::foundation::database::SqliteDatabase,
}

impl<E> MessagingStore<E> {
    pub fn switch_conversation_coordination(&self) -> SqliteSwitchConversationCoordination {
        SqliteSwitchConversationCoordination {
            pool: self.pool.clone(),
        }
    }
}

fn decode_conversation_id(row: &SqliteRow) -> Result<ConversationId, PersistenceFailure> {
    let raw: String = row
        .try_get("conversation_id")
        .map_err(PersistenceFailure::from_source)?;
    ConversationId::parse(raw).map_err(PersistenceFailure::from_source)
}

impl SwitchConversationCoordination for SqliteSwitchConversationCoordination {
    async fn list_pending_for_agent(
        &self,
        agent: &AgentId,
    ) -> Result<Vec<ConversationId>, PersistenceFailure> {
        let rows = sqlx::query(SELECT_PENDING_FOR_AGENT)
            .bind(agent.as_str())
            .fetch_all(&self.pool)
            .await
            .map_err(PersistenceFailure::from_source)?;
        rows.iter().map(decode_conversation_id).collect()
    }

    async fn list_pending_for_agent_since(
        &self,
        agent: &AgentId,
        since_ms: i64,
    ) -> Result<Vec<ConversationId>, PersistenceFailure> {
        let rows = sqlx::query(SELECT_PENDING_FOR_AGENT_SINCE)
            .bind(agent.as_str())
            .bind(since_ms)
            .fetch_all(&self.pool)
            .await
            .map_err(PersistenceFailure::from_source)?;
        rows.iter().map(decode_conversation_id).collect()
    }

    async fn is_agent_participant(
        &self,
        conversation: &ConversationId,
        agent: &AgentId,
    ) -> Result<bool, PersistenceFailure> {
        let found: Option<i64> = sqlx::query_scalar(SELECT_AGENT_PARTICIPATES)
            .bind(conversation.as_str())
            .bind(agent.as_str())
            .bind(agent.as_str())
            .bind(agent.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(PersistenceFailure::from_source)?;
        Ok(found.is_some())
    }
}

#[cfg(test)]
mod tests {
    use silverhand_backend_core::foundation::identity::{AgentId, ConversationId};
    use sqlx::{Connection, Executor, SqliteConnection};

    use super::*;

    fn agent(id: &str) -> AgentId {
        AgentId::parse(id).expect("agent id")
    }
    fn conv(id: &str) -> ConversationId {
        ConversationId::parse(id).expect("conversation id")
    }

    /// The base graph every enumeration reads against: agent-1 rosters the `c-*` workspace
    /// conversations and is the agent endpoint of `pair-1`; agent-2 rosters only `c-other`, so
    /// agent-1 is a non-participant there. No inbox rows yet — each test seeds its own through a
    /// second writer connection so the pending-set shapes stay local to their assertion.
    async fn seeded() -> (tempfile::TempDir, MessagingStore) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("switch-coordination.sqlite");
        let mut db = crate::current_schema::create_current_schema(&path).await;
        db.execute(sqlx::raw_sql(
            "INSERT INTO users (id, name, display_name, created_at, updated_at) VALUES \
                 ('user-1', 'owner', 'Owner', 0, 0); \
             INSERT INTO hosts (id, name, kind, created_at) VALUES ('host-1', 'Host', 'local', 0); \
             INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, \
                 display_name, created_by_user_id, materialization_status, session_scope) VALUES \
                 ('agent-1', 'agent', 'codex', 'host-1', 0, 0, 'Agent', 'user-1', 'active', 'conversation'), \
                 ('agent-2', 'agent-two', 'codex', 'host-1', 0, 0, 'Agent Two', 'user-1', 'active', 'conversation'); \
             INSERT INTO workspaces (id, name, name_key, owner_user_id, created_at, updated_at) \
                 VALUES ('workspace-1', 'Space', 'space', 'user-1', 0, 0); \
             INSERT INTO workspace_users (workspace_id, user_id, joined_at, role) VALUES \
                 ('workspace-1', 'user-1', 0, 'owner'); \
             INSERT INTO conversations (id, workspace_id, placement_kind, roster_policy, title, \
                 title_key, status, visibility, created_at, updated_at) VALUES \
                 ('c-1', 'workspace-1', 'workspace', 'open', 'One', 'one', 'open', 'private', 0, 0), \
                 ('c-2', 'workspace-1', 'workspace', 'open', 'Two', 'two', 'open', 'private', 0, 0), \
                 ('c-3', 'workspace-1', 'workspace', 'open', 'Three', 'three', 'open', 'private', 0, 0), \
                 ('c-claimed', 'workspace-1', 'workspace', 'open', 'Claimed', 'claimed', 'open', 'private', 0, 0), \
                 ('c-delivered', 'workspace-1', 'workspace', 'open', 'Delivered', 'delivered', 'open', 'private', 0, 0), \
                 ('c-failed', 'workspace-1', 'workspace', 'open', 'Failed', 'failed', 'open', 'private', 0, 0), \
                 ('c-other', 'workspace-1', 'workspace', 'open', 'Other', 'other', 'open', 'private', 0, 0); \
             INSERT INTO conversations (id, workspace_id, endpoint_a_kind, endpoint_a_id, \
                 endpoint_b_kind, endpoint_b_id, placement_kind, roster_policy, title, title_key, \
                 status, visibility, created_at, updated_at) VALUES \
                 ('pair-1', NULL, 'agent', 'agent-1', 'human', 'user-1', 'direct_pair', 'fixed', \
                  'Direct Pair', NULL, 'idle', 'private', 0, 0); \
             INSERT INTO conversation_agents (conversation_id, agent_id, joined_at) VALUES \
                 ('c-1', 'agent-1', 0), ('c-2', 'agent-1', 0), ('c-3', 'agent-1', 0), \
                 ('c-claimed', 'agent-1', 0), ('c-delivered', 'agent-1', 0), \
                 ('c-failed', 'agent-1', 0), ('c-other', 'agent-2', 0);",
        ))
        .await
        .unwrap();
        db.close().await.unwrap();
        let store = MessagingStore::open(&path).await.unwrap();
        (directory, store)
    }

    async fn writer(directory: &tempfile::TempDir) -> SqliteConnection {
        let path = directory.path().join("switch-coordination.sqlite");
        SqliteConnection::connect(&format!("sqlite:{}", path.display()))
            .await
            .unwrap()
    }

    /// Seed one inbox row (and the message it references) with explicit id/created_at and lifecycle
    /// flags. `id` doubles as the message `seq`, so it is globally unique and controls both the
    /// `MIN(id)` oldest-first ordering and the `conversation_id` distinctness.
    #[allow(clippy::too_many_arguments)]
    async fn add_inbox(
        conn: &mut SqliteConnection,
        id: i64,
        conversation: &str,
        agent: &str,
        created_at: i64,
        delivered: bool,
        failed: bool,
        in_flight: bool,
    ) {
        let message_id = format!("m{id}");
        sqlx::query(
            "INSERT INTO messages (id, conversation_id, from_kind, from_user_id, content, seq, \
                 created_at) VALUES (?, ?, 'user', 'user-1', 'hi', ?, ?)",
        )
        .bind(&message_id)
        .bind(conversation)
        .bind(id)
        .bind(created_at)
        .execute(&mut *conn)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO conversation_inboxes (id, conversation_id, agent_id, message_id, \
                 delivered_at, created_at, in_flight_owner, failed_at, delivery_kind) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'turn')",
        )
        .bind(id)
        .bind(conversation)
        .bind(agent)
        .bind(&message_id)
        .bind(delivered.then_some(created_at))
        .bind(created_at)
        .bind(in_flight.then_some("owner-1"))
        .bind(failed.then_some(created_at))
        .execute(&mut *conn)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn pending_for_agent_dedups_per_conversation_oldest_first_and_excludes_the_settled() {
        let (directory, store) = seeded().await;
        let mut conn = writer(&directory).await;
        // c-1 has two pending rows → one entry, ordered by its MIN(id)=10, ahead of c-2's id=20.
        add_inbox(&mut conn, 30, "c-1", "agent-1", 30, false, false, false).await;
        add_inbox(&mut conn, 10, "c-1", "agent-1", 10, false, false, false).await;
        add_inbox(&mut conn, 20, "c-2", "agent-1", 20, false, false, false).await;
        // A claimed lease is not a fresh restart candidate (`in_flight_owner IS NULL` narrows it out).
        add_inbox(&mut conn, 5, "c-claimed", "agent-1", 5, false, false, true).await;
        // Delivered and failed rows are terminal — never pending.
        add_inbox(
            &mut conn,
            6,
            "c-delivered",
            "agent-1",
            6,
            true,
            false,
            false,
        )
        .await;
        add_inbox(&mut conn, 7, "c-failed", "agent-1", 7, false, true, false).await;
        // Another agent's pending row is not agent-1's.
        add_inbox(&mut conn, 8, "c-other", "agent-2", 8, false, false, false).await;
        conn.close().await.unwrap();

        let pending = store
            .switch_conversation_coordination()
            .list_pending_for_agent(&agent("agent-1"))
            .await
            .unwrap();
        assert_eq!(pending, vec![conv("c-1"), conv("c-2")]);
    }

    #[tokio::test]
    async fn pending_since_includes_in_flight_honors_the_boundary_and_excludes_the_settled() {
        let (directory, store) = seeded().await;
        let mut conn = writer(&directory).await;
        // At the boundary (created_at == since) → included.
        add_inbox(&mut conn, 10, "c-1", "agent-1", 100, false, false, false).await;
        // Below the boundary → historical backlog, excluded.
        add_inbox(&mut conn, 20, "c-2", "agent-1", 99, false, false, false).await;
        // In-flight rows are DELIBERATELY included here (no `in_flight_owner` narrowing).
        add_inbox(&mut conn, 30, "c-3", "agent-1", 150, false, false, true).await;
        // Delivered and failed are still terminal, boundary or not.
        add_inbox(
            &mut conn,
            40,
            "c-delivered",
            "agent-1",
            200,
            true,
            false,
            false,
        )
        .await;
        add_inbox(
            &mut conn, 50, "c-failed", "agent-1", 200, false, true, false,
        )
        .await;
        conn.close().await.unwrap();

        let since = store
            .switch_conversation_coordination()
            .list_pending_for_agent_since(&agent("agent-1"), 100)
            .await
            .unwrap();
        // Distinct ids, ordered by conversation_id ASC.
        assert_eq!(since, vec![conv("c-1"), conv("c-3")]);
    }

    #[tokio::test]
    async fn is_agent_participant_accepts_both_placement_arms_and_rejects_a_non_member() {
        let (_directory, store) = seeded().await;
        let coordination = store.switch_conversation_coordination();
        let agent_1 = agent("agent-1");
        // Roster arm: agent-1 is a workspace-conversation member.
        assert!(
            coordination
                .is_agent_participant(&conv("c-1"), &agent_1)
                .await
                .unwrap()
        );
        // Direct-pair arm: agent-1 is the agent endpoint of `pair-1`.
        assert!(
            coordination
                .is_agent_participant(&conv("pair-1"), &agent_1)
                .await
                .unwrap()
        );
        // Neither arm: agent-1 does not roster `c-other` and is no endpoint there.
        assert!(
            !coordination
                .is_agent_participant(&conv("c-other"), &agent_1)
                .await
                .unwrap()
        );
    }
}
