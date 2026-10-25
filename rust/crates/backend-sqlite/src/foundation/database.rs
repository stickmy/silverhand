//! The one place the product database's connection options and pools live.
//!
//! Every runtime connection runs WAL with `synchronous=FULL` (an acknowledged write
//! must survive power loss; NORMAL skips the commit fsync), enforces foreign keys, and
//! never creates the file — `database_startup` owns creation and migration.

use std::path::Path;
use std::time::Duration;

use sqlx::pool::PoolConnection;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Sqlite, SqlitePool, Transaction};

pub const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Pool size for a store opened on its own (tests, tools).
pub(crate) const STORE_POOL_CONNECTIONS: u32 = 4;

/// Reader pool size for the process-wide database handle.
const SHARED_READER_CONNECTIONS: u32 = 8;

/// A store's database handle: a reader pool and a writer pool.
///
/// `BEGIN IMMEDIATE`/`EXCLUSIVE` transactions take the writer. In the shared handle the
/// writer is one connection, so writers queue fairly in the pool instead of contending
/// for SQLite's write lock through busy-handler sleeps. A deferred transaction that
/// later writes still upgrades on a reader connection under `busy_timeout`.
#[derive(Clone, Debug)]
pub struct SqliteDatabase {
    reader: SqlitePool,
    writer: SqlitePool,
}

impl SqliteDatabase {
    pub fn reader(&self) -> &SqlitePool {
        &self.reader
    }

    pub fn writer(&self) -> &SqlitePool {
        &self.writer
    }

    /// The pool a transaction opened with `statement` (`None` = plain `BEGIN`) runs on.
    pub fn pool_for(&self, statement: Option<&str>) -> &SqlitePool {
        match statement {
            Some(statement) if takes_write_lock(statement) => &self.writer,
            _ => &self.reader,
        }
    }

    pub async fn begin_with(
        &self,
        statement: &'static str,
    ) -> Result<Transaction<'static, Sqlite>, sqlx::Error> {
        self.pool_for(Some(statement)).begin_with(statement).await
    }

    pub async fn begin(&self) -> Result<Transaction<'static, Sqlite>, sqlx::Error> {
        self.reader.begin().await
    }

    pub async fn close(&self) {
        self.writer.close().await;
        self.reader.close().await;
    }

    pub fn options(&self) -> &sqlx::pool::PoolOptions<Sqlite> {
        self.reader.options()
    }

    pub async fn acquire(&self) -> Result<PoolConnection<Sqlite>, sqlx::Error> {
        self.reader.acquire().await
    }
}

/// Autocommit statements run on the reader pool; a statement that writes takes
/// SQLite's write lock under `busy_timeout`, like a deferred transaction.
impl<'p> sqlx::Executor<'p> for &'_ SqliteDatabase {
    type Database = Sqlite;

    fn fetch_many<'e, 'q: 'e, E>(
        self,
        query: E,
    ) -> futures_util::stream::BoxStream<
        'e,
        Result<
            either::Either<sqlx::sqlite::SqliteQueryResult, sqlx::sqlite::SqliteRow>,
            sqlx::Error,
        >,
    >
    where
        E: 'q + sqlx::Execute<'q, Sqlite>,
    {
        (&self.reader).fetch_many(query)
    }

    fn fetch_optional<'e, 'q: 'e, E>(
        self,
        query: E,
    ) -> futures_util::future::BoxFuture<'e, Result<Option<sqlx::sqlite::SqliteRow>, sqlx::Error>>
    where
        E: 'q + sqlx::Execute<'q, Sqlite>,
    {
        (&self.reader).fetch_optional(query)
    }

    fn prepare_with<'e>(
        self,
        sql: sqlx::SqlStr,
        parameters: &'e [sqlx::sqlite::SqliteTypeInfo],
    ) -> futures_util::future::BoxFuture<'e, Result<sqlx::sqlite::SqliteStatement, sqlx::Error>>
    where
        'p: 'e,
    {
        (&self.reader).prepare_with(sql, parameters)
    }

    fn describe<'e>(
        self,
        sql: sqlx::SqlStr,
    ) -> futures_util::future::BoxFuture<'e, Result<sqlx::Describe<Sqlite>, sqlx::Error>>
    where
        'p: 'e,
    {
        (&self.reader).describe(sql)
    }
}

/// One pool serving both roles: the shape of a store opened directly and of tests.
impl From<SqlitePool> for SqliteDatabase {
    fn from(pool: SqlitePool) -> Self {
        Self {
            reader: pool.clone(),
            writer: pool,
        }
    }
}

fn takes_write_lock(statement: &str) -> bool {
    let statement = statement.trim_start();
    ["BEGIN IMMEDIATE", "BEGIN EXCLUSIVE"].iter().any(|prefix| {
        statement.len() >= prefix.len() && statement[..prefix.len()].eq_ignore_ascii_case(prefix)
    })
}

pub fn connect_options(path: &Path, busy_timeout: Duration) -> SqliteConnectOptions {
    SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Full)
        .busy_timeout(busy_timeout)
}

/// A small single pool for one store opened directly on `path`.
pub(crate) async fn store_pool(
    path: &Path,
    busy_timeout: Duration,
) -> Result<SqliteDatabase, sqlx::Error> {
    let pool = SqlitePoolOptions::new()
        .max_connections(STORE_POOL_CONNECTIONS)
        .connect_with(connect_options(path, busy_timeout))
        .await?;
    Ok(pool.into())
}

/// The process-wide handle: open once and hand to every store's `from_pool`.
pub async fn open_shared(path: &Path) -> Result<SqliteDatabase, sqlx::Error> {
    let options = connect_options(path, BUSY_TIMEOUT);
    let writer = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options.clone())
        .await?;
    let reader = SqlitePoolOptions::new()
        .max_connections(SHARED_READER_CONNECTIONS)
        .connect_with(options)
        .await?;
    Ok(SqliteDatabase { reader, writer })
}

#[cfg(test)]
mod tests {
    use super::takes_write_lock;

    #[test]
    fn only_lock_taking_begins_route_to_the_writer() {
        assert!(takes_write_lock("BEGIN IMMEDIATE"));
        assert!(takes_write_lock("begin exclusive"));
        assert!(!takes_write_lock("BEGIN DEFERRED"));
        assert!(!takes_write_lock("BEGIN"));
    }
}
