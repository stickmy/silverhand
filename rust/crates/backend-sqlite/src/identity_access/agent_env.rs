//! SQLite adapter for per-agent custom environment variables (`agent_env_vars`).

use silverhand_backend_core::{
    foundation::{identity::AgentId, persistence::PersistenceFailure},
    identity_access::agent_env::{AgentEnvStore, AgentEnvVar, AgentEnvWriteOutcome},
};
use sqlx::{Executor, Row};

const ROOT_WRITE_BEGIN: &str = "BEGIN IMMEDIATE";

#[derive(Clone)]
pub struct SqliteAgentEnvStore {
    pool: crate::foundation::database::SqliteDatabase,
}

impl SqliteAgentEnvStore {
    pub fn from_pool(pool: impl Into<crate::foundation::database::SqliteDatabase>) -> Self {
        Self { pool: pool.into() }
    }
}

pub(crate) async fn read_agent_env_on_conn(
    conn: &mut sqlx::sqlite::SqliteConnection,
    agent: &AgentId,
) -> Result<Vec<AgentEnvVar>, PersistenceFailure> {
    let rows = sqlx::query(
        "SELECT name, value
           FROM agent_env_vars
          WHERE agent_id = ?
          ORDER BY position ASC",
    )
    .bind(agent.as_str())
    .fetch_all(conn)
    .await
    .map_err(PersistenceFailure::from_source)?;

    let mut vars = Vec::with_capacity(rows.len());
    for row in rows {
        let name: String = row
            .try_get("name")
            .map_err(PersistenceFailure::from_source)?;
        let value: String = row
            .try_get("value")
            .map_err(PersistenceFailure::from_source)?;
        vars.push(AgentEnvVar { name, value });
    }
    Ok(vars)
}

impl AgentEnvStore for SqliteAgentEnvStore {
    async fn read_agent_env(
        &self,
        agent: &AgentId,
    ) -> Result<Vec<AgentEnvVar>, PersistenceFailure> {
        let mut conn = self
            .pool
            .acquire()
            .await
            .map_err(PersistenceFailure::from_source)?;
        read_agent_env_on_conn(&mut conn, agent).await
    }

    async fn replace_agent_env(
        &self,
        agent: &AgentId,
        vars: &[AgentEnvVar],
    ) -> Result<AgentEnvWriteOutcome, PersistenceFailure> {
        let mut conn = self
            .pool
            .acquire()
            .await
            .map_err(PersistenceFailure::from_source)?;
        conn.execute(ROOT_WRITE_BEGIN)
            .await
            .map_err(PersistenceFailure::from_source)?;

        let result = async {
            let exists: Option<i64> = sqlx::query_scalar(
                "SELECT 1 FROM agents WHERE id = ? AND materialization_status = 'active'",
            )
            .bind(agent.as_str())
            .fetch_optional(&mut *conn)
            .await
            .map_err(PersistenceFailure::from_source)?;
            if exists.is_none() {
                return Ok(AgentEnvWriteOutcome::NoSuchAgent);
            }

            sqlx::query("DELETE FROM agent_env_vars WHERE agent_id = ?")
                .bind(agent.as_str())
                .execute(&mut *conn)
                .await
                .map_err(PersistenceFailure::from_source)?;

            for (index, var) in vars.iter().enumerate() {
                sqlx::query(
                    "INSERT INTO agent_env_vars (agent_id, position, name, value)
                     VALUES (?, ?, ?, ?)",
                )
                .bind(agent.as_str())
                .bind(index as i64)
                .bind(&var.name)
                .bind(&var.value)
                .execute(&mut *conn)
                .await
                .map_err(PersistenceFailure::from_source)?;
            }

            Ok(AgentEnvWriteOutcome::Replaced(vars.to_vec()))
        }
        .await;

        match result {
            Ok(outcome) => {
                conn.execute("COMMIT")
                    .await
                    .map_err(PersistenceFailure::from_source)?;
                Ok(outcome)
            }
            Err(err) => {
                let _ = conn.execute("ROLLBACK").await;
                Err(err)
            }
        }
    }
}
