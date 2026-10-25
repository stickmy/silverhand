//! The connection the host-agent ports share, and the one place a required
//! transaction capability becomes a begin statement.
//!
//! This store opens a database it does NOT own: the product database whose schema
//! and migration ledger are created elsewhere. It therefore creates no table and
//! records no version, and `create_if_missing(false)` turns opening an
//! unprovisioned database into a startup failure rather than a silent empty one.

use std::{path::Path, time::Duration};

use sqlx::{Sqlite, SqlitePool, Transaction};
use thiserror::Error;

pub(crate) mod decode;

pub(crate) const ROOT_WRITE_BEGIN: &str = "BEGIN IMMEDIATE";
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct SqliteHostAgentStore {
    pool: SqlitePool,
}

/// Startup and driver failures, surfaced only to the composition root that opens
/// the store. The ports never expose it.
#[derive(Debug, Error)]
pub enum SqliteHostAgentStoreError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
}

impl SqliteHostAgentStore {
    pub fn from_pool(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn open(path: impl AsRef<Path>) -> Result<Self, SqliteHostAgentStoreError> {
        Self::open_with_busy_timeout(path, BUSY_TIMEOUT).await
    }

    pub(crate) async fn open_with_busy_timeout(
        path: impl AsRef<Path>,
        busy_timeout: Duration,
    ) -> Result<Self, SqliteHostAgentStoreError> {
        let pool = crate::foundation::database::store_pool(path.as_ref(), busy_timeout).await?;
        Ok(Self { pool })
    }

    /// The connection for single-statement reads that need no multi-statement
    /// snapshot.
    pub(crate) fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// The single place that reserves the one writer before the first statement of
    /// a write axis.
    pub(crate) async fn begin_root_write(&self) -> Result<Transaction<'_, Sqlite>, sqlx::Error> {
        self.pool.begin_with(ROOT_WRITE_BEGIN).await
    }
}
