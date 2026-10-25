//! The one place the product database's connection options live.
//!
//! Every runtime connection runs WAL with `synchronous=FULL` (an acknowledged write
//! must survive power loss; NORMAL skips the commit fsync), enforces foreign keys, and
//! never creates the file — `database_startup` owns creation and migration.

use std::path::Path;
use std::time::Duration;

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};

pub const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Pool size for a store opened on its own (tests, tools).
pub(crate) const STORE_POOL_CONNECTIONS: u32 = 4;

/// Pool size for the one pool a composition root shares across every store.
const SHARED_POOL_CONNECTIONS: u32 = 16;

pub fn connect_options(path: &Path, busy_timeout: Duration) -> SqliteConnectOptions {
    SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Full)
        .busy_timeout(busy_timeout)
}

/// A small pool for one store opened directly on `path`.
pub(crate) async fn store_pool(
    path: &Path,
    busy_timeout: Duration,
) -> Result<SqlitePool, sqlx::Error> {
    SqlitePoolOptions::new()
        .max_connections(STORE_POOL_CONNECTIONS)
        .connect_with(connect_options(path, busy_timeout))
        .await
}

/// The shared runtime pool: open once per process and hand to every store's
/// `from_pool`.
pub async fn open_shared_pool(path: &Path) -> Result<SqlitePool, sqlx::Error> {
    SqlitePoolOptions::new()
        .max_connections(SHARED_POOL_CONNECTIONS)
        .connect_with(connect_options(path, BUSY_TIMEOUT))
        .await
}
