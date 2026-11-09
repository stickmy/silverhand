//! The `AgentGraphRead` adapter: the two post-commit reads an idempotent echo of a
//! conversation-scoped create needs to rebuild its wire coordinates.
//!
//! A fresh create carries the home DM channel and the onboarding message id out of
//! the provisioning seal, so these queries run ONLY on the echo path — when a caller
//! replays an idempotent create against an already-active conversation-scoped agent
//! and the route must re-derive the coordinates the original 201 returned. They are
//! single-statement reads, not the agent projection authority, so they live here as a
//! narrow port over the same store that owns the agents / workspaces / conversations
//! tables rather than borrowing the detail read model.

use silverhand_backend_core::{
    application_orchestration::agent_creation::AgentGraphRead,
    conversation_messaging::messaging::{MessageId, agent_onboarding::ONBOARDING_DM_TITLE},
    foundation::identity::{AgentId, ConversationId},
    foundation::persistence::PersistenceFailure,
};

use crate::foundation::host_agent_store::SqliteHostAgentStore;

/// The agent's home `Onboarding` DM: the one fixed-roster channel the conversation-scope
/// provision opened in the workspace this agent owns. Bound by `owner_agent_id` rather
/// than the derived `wsah_<agent>` id so the read never assumes the id-minting rule.
const SELECT_HOME_ONBOARDING_DM: &str = "SELECT c.id \
       FROM conversations c \
       JOIN workspaces w ON w.id = c.workspace_id \
      WHERE w.owner_agent_id = ? \
        AND c.roster_policy = 'fixed' \
        AND c.title = ? \
      ORDER BY c.created_at ASC \
      LIMIT 1";

/// The onboarding system message is the first line written to the DM (`seq = 1`); the
/// first row by `seq` is that message regardless of any later traffic.
const SELECT_FIRST_MESSAGE: &str =
    "SELECT id FROM messages WHERE conversation_id = ? ORDER BY seq ASC LIMIT 1";

impl AgentGraphRead for SqliteHostAgentStore {
    async fn find_home_dm_channel(
        &self,
        agent: &AgentId,
    ) -> Result<Option<ConversationId>, PersistenceFailure> {
        let raw: Option<String> = sqlx::query_scalar(SELECT_HOME_ONBOARDING_DM)
            .bind(agent.as_str())
            .bind(ONBOARDING_DM_TITLE)
            .fetch_optional(self.pool())
            .await
            .map_err(PersistenceFailure::from_source)?;
        raw.map(ConversationId::parse)
            .transpose()
            .map_err(PersistenceFailure::from_source)
    }

    async fn find_first_onboarding_message(
        &self,
        dm: &ConversationId,
    ) -> Result<Option<MessageId>, PersistenceFailure> {
        let raw: Option<String> = sqlx::query_scalar(SELECT_FIRST_MESSAGE)
            .bind(dm.as_str())
            .fetch_optional(self.pool())
            .await
            .map_err(PersistenceFailure::from_source)?;
        raw.map(MessageId::parse)
            .transpose()
            .map_err(PersistenceFailure::from_source)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use sqlx::Connection;
    use tempfile::TempDir;

    use super::*;

    async fn seed_schema(dir: &TempDir, name: &str, seed: &[String]) -> PathBuf {
        let path = dir.path().join(name);
        let mut connection = crate::current_schema::create_current_schema(&path).await;
        for statement in seed {
            sqlx::query(sqlx::AssertSqlSafe(statement.as_str()))
                .execute(&mut connection)
                .await
                .expect("seed statement");
        }
        connection.close().await.expect("close seed connection");
        path
    }

    async fn open(dir: &TempDir, name: &str, seed: &[String]) -> SqliteHostAgentStore {
        let path = seed_schema(dir, name, seed).await;
        SqliteHostAgentStore::open(&path).await.unwrap()
    }

    fn base() -> Vec<String> {
        vec![
            "INSERT INTO users (id, name, display_name, created_at, updated_at) \
             VALUES ('u1', 'u1', 'U1', 1, 1)"
                .to_owned(),
            "INSERT INTO hosts (id, name, kind, created_at) VALUES ('h1', 'h1', 'local', 1)"
                .to_owned(),
        ]
    }

    fn seed_agent(id: &str) -> String {
        format!(
            "INSERT INTO agents (id, name, display_name, runtime, host_id, \
             materialization_status, session_scope, created_by_user_id, created_at, updated_at) \
             VALUES ('{id}', '{id}', '{id}', 'codex', 'h1', 'active', 'conversation', 'u1', 1, 1)"
        )
    }

    /// An agent-home workspace: `owner_agent_id` set forces `name_key` NULL by the
    /// schema's XOR check.
    fn seed_home_workspace(ws: &str, owner_agent: &str) -> String {
        format!(
            "INSERT INTO workspaces (id, name, owner_agent_id, owner_user_id, created_at, updated_at) \
             VALUES ('{ws}', '{owner_agent}', '{owner_agent}', 'u1', 1, 1)"
        )
    }

    fn seed_conversation(id: &str, ws: &str, title: &str, created_at: i64) -> String {
        format!(
            "INSERT INTO conversations (id, workspace_id, placement_kind, roster_policy, title, \
             title_key, created_at, updated_at) \
             VALUES ('{id}', '{ws}', 'workspace', 'fixed', '{title}', '{id}', {created_at}, 1)"
        )
    }

    fn seed_message(id: &str, conversation: &str, seq: i64, created_at: i64) -> String {
        format!(
            "INSERT INTO messages (id, conversation_id, from_kind, content, seq, created_at) \
             VALUES ('{id}', '{conversation}', 'system', 'onboarding', {seq}, {created_at})"
        )
    }

    #[tokio::test]
    async fn finds_the_onboarding_dm_in_the_agents_home_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let mut seed = base();
        seed.push(seed_agent("a1"));
        seed.push(seed_agent("other"));
        seed.push(seed_agent("empty"));
        // Ownership is stored, not inferred from a wsah_<agent> naming convention.
        seed.push(seed_home_workspace("home-one", "a1"));
        seed.push(seed_home_workspace("home-other", "other"));
        seed.push(seed_home_workspace("home-empty", "empty"));
        seed.push(seed_conversation("foreign", "home-other", "Onboarding", 0));
        // A non-onboarding fixed channel that must NOT be picked.
        seed.push(seed_conversation("dm-a1", "home-one", "dm-a1", 1));
        seed.push(
            "INSERT INTO conversations (id, workspace_id, placement_kind, roster_policy, \
                   title, title_key, created_at, updated_at) VALUES \
                   ('open-onb', 'home-one', 'workspace', 'open', 'Onboarding', 'open-onb', 2, 1)"
                .into(),
        );
        seed.push(seed_conversation("onb-a1", "home-one", "Onboarding", 3));
        seed.push(seed_conversation("later-onb", "home-one", "Onboarding", 4));
        let store = open(&dir, "home-dm.db", &seed).await;

        for (id, expected) in [("a1", Some("onb-a1")), ("empty", None)] {
            let dm = store
                .find_home_dm_channel(&AgentId::parse(id).unwrap())
                .await
                .unwrap();
            assert_eq!(
                dm,
                expected.map(|id| ConversationId::parse(id).unwrap()),
                "{id}"
            );
        }
    }

    #[tokio::test]
    async fn finds_the_first_message_by_seq() {
        let dir = tempfile::tempdir().unwrap();
        let mut seed = base();
        seed.push(seed_agent("a1"));
        seed.push(seed_home_workspace("wsah_a1", "a1"));
        seed.push(seed_conversation("onb-a1", "wsah_a1", "Onboarding", 3));
        seed.push(seed_conversation("empty", "wsah_a1", "Empty", 4));
        seed.push(seed_conversation("other", "wsah_a1", "Other", 5));
        seed.push(seed_message("foreign", "other", 1, 1));
        // Insertion and timestamp order disagree with the authoritative sequence.
        seed.push(seed_message("msg-2", "onb-a1", 2, 10));
        seed.push(seed_message("msg-1", "onb-a1", 1, 20));
        let store = open(&dir, "first-msg.db", &seed).await;

        for (id, expected) in [("onb-a1", Some("msg-1")), ("empty", None)] {
            let msg = store
                .find_first_onboarding_message(&ConversationId::parse(id).unwrap())
                .await
                .unwrap();
            assert_eq!(
                msg,
                expected.map(|id| MessageId::parse(id).unwrap()),
                "{id}"
            );
        }
    }
}
