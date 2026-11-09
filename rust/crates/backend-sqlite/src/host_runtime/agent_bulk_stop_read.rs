//! SQLite projection for the host-runtime bulk-stop use case.
//!
//! The agent row's host and every runtime seat are captured on one connection so
//! dispatch coordinates cannot come from different snapshots. The narrow
//! [`select_runtime_sessions`] helper lets the application teardown unit of work
//! capture the same C2-owned projection inside its existing write transaction;
//! it never opens, begins, commits, or rolls back a transaction itself.

use silverhand_backend_core::{
    application_orchestration::teardown::facts::AgentRuntimeSessionFacts,
    foundation::identity::{AgentId, HostId},
    foundation::persistence::PersistenceFailure,
    host_runtime::{
        agent_bulk_stop::{AgentBulkStopReadPort, AgentBulkStopSnapshot},
        runtime_credential::RuntimeScope,
    },
};
use sqlx::{Row, SqliteConnection, sqlite::SqliteRow};

use crate::foundation::host_agent_store::{SqliteHostAgentStore, decode};

const SELECT_RUNTIME_SESSIONS: &str = "SELECT agent_id, conversation_id, observed_status, \
     generation FROM agent_runtime_slots WHERE agent_id = ? ORDER BY id ASC";

/// The bulk stop's host capture: the agent row's own id and the host every seat is
/// dispatched to, read on the same connection as the seats.
const SELECT_AGENT_HOST: &str = "SELECT id, host_id FROM agents WHERE id = ?";

fn runtime_session(row: &SqliteRow) -> Result<AgentRuntimeSessionFacts, sqlx::Error> {
    let agent = decode::agent_id(row.try_get("agent_id")?)?;
    let scope = match row.try_get::<Option<String>, _>("conversation_id")? {
        Some(raw) => RuntimeScope::ConversationSlot {
            agent,
            source: decode::conversation_id(raw)?,
        },
        None => RuntimeScope::AgentSlot { agent },
    };
    Ok(AgentRuntimeSessionFacts {
        scope,
        status: decode::observed_status(&row.try_get::<String, _>("observed_status")?)?,
        generation: decode::generation(row.try_get("generation")?)?,
    })
}

fn agent_host(row: &SqliteRow) -> Result<(AgentId, HostId), sqlx::Error> {
    Ok((
        decode::agent_id(row.try_get("id")?)?,
        decode::host_id(row.try_get("host_id")?)?,
    ))
}

pub(crate) async fn select_runtime_sessions(
    conn: &mut SqliteConnection,
    id: &AgentId,
) -> Result<Vec<AgentRuntimeSessionFacts>, sqlx::Error> {
    let rows = sqlx::query(SELECT_RUNTIME_SESSIONS)
        .bind(id.as_str())
        .fetch_all(&mut *conn)
        .await?;
    rows.iter().map(runtime_session).collect()
}

async fn select_bulk_stop_snapshot(
    conn: &mut SqliteConnection,
    id: &AgentId,
) -> Result<Option<AgentBulkStopSnapshot>, sqlx::Error> {
    let Some(row) = sqlx::query(SELECT_AGENT_HOST)
        .bind(id.as_str())
        .fetch_optional(&mut *conn)
        .await?
    else {
        return Ok(None);
    };
    let (source, host) = agent_host(&row)?;
    let sessions = select_runtime_sessions(&mut *conn, id).await?;
    Ok(Some(AgentBulkStopSnapshot {
        source,
        host,
        sessions,
    }))
}

impl AgentBulkStopReadPort for SqliteHostAgentStore {
    async fn read_bulk_stop_snapshot(
        &self,
        agent_id: &AgentId,
    ) -> Result<Option<AgentBulkStopSnapshot>, PersistenceFailure> {
        // One transaction keeps the host row and seat set in one read snapshot.
        // Dropping the read-only transaction rolls it back without a write.
        let mut tx = self
            .pool()
            .begin()
            .await
            .map_err(PersistenceFailure::from_source)?;
        select_bulk_stop_snapshot(&mut tx, agent_id)
            .await
            .map_err(PersistenceFailure::from_source)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use sqlx::Connection;
    use tempfile::TempDir;

    use super::*;

    fn agent(id: &str) -> AgentId {
        AgentId::parse(id).expect("agent id")
    }

    fn host(id: &str) -> HostId {
        HostId::parse(id).expect("host id")
    }

    async fn seed_schema(dir: &TempDir, name: &str, seed: &[&'static str]) -> PathBuf {
        let path = dir.path().join(name);
        let mut connection = crate::current_schema::create_current_schema(&path).await;
        for statement in seed {
            sqlx::query(*statement)
                .execute(&mut connection)
                .await
                .expect("seed statement");
        }
        connection.close().await.expect("close seed connection");
        path
    }

    async fn seed_read_fixture(dir: &TempDir) -> SqliteHostAgentStore {
        let path = seed_schema(
            dir,
            "bulk-stop-read.db",
            &[
                "INSERT INTO users (id, name, display_name, created_at, updated_at) \
                 VALUES ('u1', 'u1', 'U1', 1, 1)",
                "INSERT INTO hosts (id, name, kind, created_at, user_id, provisioning_source) \
                 VALUES ('h-good', 'h-good', 'remote', 1, 'u1', 'system')",
                seed_agent!(
                    id = "a-agent",
                    host_id = "h-good",
                    created_by_user_id = "u1",
                    session_scope = "agent",
                ),
                seed_agent!(id = "a-conv", host_id = "h-good", created_by_user_id = "u1"),
                "INSERT INTO workspaces (id, name, name_key, created_at, updated_at) \
                 VALUES ('w1', 'w1', 'w1', 1, 1)",
                seed_channel!(
                    id = "c1",
                    workspace_id = "w1",
                    title = "chan",
                    title_key = "chan",
                ),
                "INSERT INTO conversation_agents (conversation_id, agent_id, joined_at) \
                 VALUES ('c1', 'a-conv', 1)",
                "UPDATE agent_runtime_slots SET observed_status='active', generation=1 WHERE agent_id='a-agent'",
                "UPDATE agent_runtime_slots SET observed_status='starting', generation=1 WHERE agent_id='a-conv'",
                "UPDATE agent_runtime_slots SET generation=2 WHERE agent_id='a-conv'",
            ],
        )
        .await;
        SqliteHostAgentStore::open(&path).await.expect("open store")
    }

    #[tokio::test]
    async fn a_bulk_stop_snapshot_binds_the_source_host_and_its_seat() {
        let dir = tempfile::tempdir().unwrap();
        let store = seed_read_fixture(&dir).await;

        let snapshot = store
            .read_bulk_stop_snapshot(&agent("a-agent"))
            .await
            .expect("read snapshot")
            .expect("agent present");
        assert_eq!(snapshot.source, agent("a-agent"));
        assert_eq!(snapshot.host, host("h-good"));
        assert_eq!(
            snapshot.sessions[0].status,
            silverhand_backend_core::host_runtime::runtime_event::RuntimeObservedStatus::Active
        );
        assert_eq!(snapshot.sessions[0].generation.as_i64(), 1);
        assert!(matches!(
            snapshot.sessions.as_slice(),
            [session] if matches!(
                &session.scope,
                RuntimeScope::AgentSlot { agent } if agent == &self::agent("a-agent")
            )
        ));

        let conversation = store
            .read_bulk_stop_snapshot(&agent("a-conv"))
            .await
            .expect("read snapshot")
            .expect("agent present");
        assert_eq!(conversation.source, agent("a-conv"));
        assert_eq!(conversation.host, host("h-good"));
        assert_eq!(
            conversation.sessions[0].status,
            silverhand_backend_core::host_runtime::runtime_event::RuntimeObservedStatus::Starting
        );
        assert_eq!(conversation.sessions[0].generation.as_i64(), 2);
        assert!(matches!(
            conversation.sessions.as_slice(),
            [session] if matches!(
                &session.scope,
                RuntimeScope::ConversationSlot { agent, source }
                    if agent == &self::agent("a-conv") && source.as_str() == "c1"
            )
        ));
        let snapshot = store
            .read_bulk_stop_snapshot(&agent("nope"))
            .await
            .expect("read snapshot");
        assert!(snapshot.is_none(), "an unknown agent has no snapshot");
    }
}
