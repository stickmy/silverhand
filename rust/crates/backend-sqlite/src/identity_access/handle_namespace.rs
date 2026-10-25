//! SQLite adapter for the application-seam [`HandleNamespaceRead`] port.
//!
//! A participant @-handle lives in ONE global namespace shared by agents and
//! human users, enforced by `agents.name`/`users.name` UNIQUE plus the two
//! cross-namespace triggers. A handle is taken iff an agent OR a user already
//! holds it — checking one table alone would report a colliding handle as free and
//! then trip a trigger at insert time. This is the exact same question the
//! product's `isParticipantHandleAvailable` answers (matched case-sensitively, as
//! the underlying `getByName` reads do), so no bounded context owns it: it is an
//! application-level read the composition root wires here.

use std::{path::Path, time::Duration};

use silverhand_backend_core::{
    foundation::persistence::PersistenceFailure,
    identity_access::human_auth_flows::HandleNamespaceRead,
    identity_access::user_identity::ParticipantHandle,
};
use sqlx::SqlitePool;
use thiserror::Error;

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

// A handle is taken iff either half of the namespace already holds it. Both reads
// are the exact-match `name = ?` the product's `getByName` uses (no COLLATE), so
// the predicate mirrors the DB-enforced uniqueness the create paths collide
// against.
const HANDLE_TAKEN: &str = "\
SELECT EXISTS(SELECT 1 FROM users WHERE name = ?) \
    OR EXISTS(SELECT 1 FROM agents WHERE name = ?)";

/// Whether the shared @-handle namespace already holds `handle`, over any executor so the
/// pooled availability read and an in-flight write transaction ask the one canonical
/// question. A create reserve preflights this under its write reservation, before its
/// INSERT, so the cross-namespace user/agent collision surfaces as an honest conflict
/// rather than a trigger `RAISE(ABORT)` the driver reports as an opaque constraint error.
pub(crate) async fn handle_taken<'e, E>(executor: E, handle: &str) -> Result<bool, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let taken: i64 = sqlx::query_scalar(HANDLE_TAKEN)
        .bind(handle)
        .bind(handle)
        .fetch_one(executor)
        .await?;
    Ok(taken != 0)
}

/// A pooled connection to an existing product database answering the shared
/// @-handle availability question. Read-only; holds no clock.
#[derive(Clone)]
pub struct SqliteHandleNamespace {
    pool: SqlitePool,
}

/// Startup/driver failures surfaced only to the composition root that opens the
/// store; the port method hands the driver error over as a `PersistenceFailure`.
#[derive(Debug, Error)]
pub enum SqliteHandleNamespaceError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
}

impl SqliteHandleNamespace {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, SqliteHandleNamespaceError> {
        let pool = crate::foundation::database::store_pool(path.as_ref(), BUSY_TIMEOUT).await?;
        Ok(Self { pool })
    }

    /// Construct over an already-open pool so a composition root that shares one
    /// pool across adapters need not reopen the file.
    pub fn from_pool(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

impl HandleNamespaceRead for SqliteHandleNamespace {
    async fn is_handle_taken(
        &self,
        handle: &ParticipantHandle,
    ) -> Result<bool, PersistenceFailure> {
        handle_taken(&self.pool, handle.as_str())
            .await
            .map_err(PersistenceFailure::from_source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqliteConnectOptions;
    use sqlx::{Connection, Executor, SqliteConnection};

    const CURRENT_SCHEMA: &str =
        include_str!("../../../../../rust/test-fixtures/migration-baseline/schema-current.sql");

    // A user holding `alice`, a host + agent holding `bender`. `charlie` is free.
    const SEED_SQL: &str = "\
INSERT INTO users (id, name, display_name, created_at, updated_at) \
  VALUES ('user-1', 'alice', 'Alice', 1, 1); \
INSERT INTO hosts (id, name, kind, created_at) VALUES ('host-1', 'Host', 'local', 1); \
INSERT INTO agents (id, name, display_name, runtime, host_id, created_by_user_id, created_at, updated_at) \
  VALUES ('agent-1', 'bender', 'Bender', 'claude', 'host-1', 'user-1', 1, 1);";

    async fn open_seeded() -> (tempfile::TempDir, SqliteHandleNamespace) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("handle.sqlite");
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .foreign_keys(true);
        let mut db = SqliteConnection::connect_with(&options).await.unwrap();
        db.execute(sqlx::raw_sql(CURRENT_SCHEMA)).await.unwrap();
        db.execute(sqlx::raw_sql(SEED_SQL)).await.unwrap();
        db.close().await.unwrap();
        let store = SqliteHandleNamespace::open(&path).await.unwrap();
        (directory, store)
    }

    async fn taken(store: &SqliteHandleNamespace, handle: &str) -> bool {
        store
            .is_handle_taken(&ParticipantHandle::parse(handle).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_handle_held_by_a_user_or_an_agent_is_taken_but_a_free_one_is_not() {
        let (_dir, store) = open_seeded().await;
        assert!(taken(&store, "alice").await, "held by a user");
        assert!(taken(&store, "bender").await, "held by an agent");
        assert!(!taken(&store, "charlie").await, "held by no one");
    }

    #[tokio::test]
    async fn the_namespace_check_is_case_sensitive() {
        // `getByName` matches exactly, so a different case is a different handle.
        let (_dir, store) = open_seeded().await;
        assert!(!taken(&store, "Alice").await);
    }
}
