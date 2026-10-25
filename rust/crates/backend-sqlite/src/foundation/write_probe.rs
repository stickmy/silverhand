//! SQLite adapter for the persistence write probe.
//!
//! ## `BEGIN IMMEDIATE` is the question; anything deferred is a different one
//!
//! The port asks whether this process can still write. `BEGIN IMMEDIATE` takes the
//! write lock at BEGIN, so a database that cannot be written refuses HERE — which is
//! the whole point. A deferred transaction, or an empty `db.transaction(|| {})`,
//! would acquire nothing and answer `Ok` on a database whose very next write fails.
//!
//! The rollback is part of the probe, not cleanup after it: the probe must leave the
//! store exactly as it found it, so it writes no row and commits nothing. A rollback
//! that itself fails is a probe failure — the process just failed to release a write
//! lock it took, which is not a writable store.
//!
//! ## The probe rides a store's handle; it never opens its own
//!
//! Readiness is about the handle the process actually writes through. A probe that
//! opened the same FILE would be a different writer: its own connections, its own
//! PRAGMAs, its own lifetime. It would answer `Ok` after the store it reports for
//! was closed, and it would answer for configuration the store does not have —
//! either way `/readyz` goes green for a process that cannot write.
//!
//! So there is no path constructor. [`SqliteWriteProbe::sharing`] takes a pool and is
//! `pub(crate)`, which leaves exactly one way to obtain a probe from outside this
//! crate: ask a store for one
//! ([`MessagingStore::write_probe`](crate::conversation_messaging::messaging::MessagingStore::write_probe)),
//! which hands over the handle it writes with. `SqlitePool` is a shared handle, so a
//! probe cannot outlive the store's usability — the gate for that lives beside the
//! store, where the handle is private.

use silverhand_backend_core::foundation::persistence::{PersistenceFailure, WriteProbe};
use thiserror::Error;

const IMMEDIATE_WRITE_BEGIN: &str = "BEGIN IMMEDIATE";

/// Takes and releases the write lock on the store's database, through the store's own
/// handle.
#[derive(Clone)]
pub struct SqliteWriteProbe {
    pool: crate::foundation::database::SqliteDatabase,
}

/// The only way this adapter can fail: the driver refused. There is no corruption
/// arm, because the probe decodes nothing — it reads no row and renders no column.
#[derive(Debug, Error)]
pub enum SqliteWriteProbeError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
}

impl SqliteWriteProbe {
    /// Probe the database behind an existing handle. Not public: a caller outside this
    /// crate must go through the store that owns the handle, which is what keeps the
    /// probe and the writer the same thing.
    pub(crate) fn sharing(pool: crate::foundation::database::SqliteDatabase) -> Self {
        Self { pool }
    }

    async fn take_and_release_write_lock(&self) -> Result<(), SqliteWriteProbeError> {
        let transaction = super::statement_metrics::statement(
            "readiness.begin_immediate",
            "run",
            self.pool.begin_with(IMMEDIATE_WRITE_BEGIN),
        )
        .await?;
        super::statement_metrics::statement("readiness.rollback", "run", transaction.rollback())
            .await?;
        Ok(())
    }
}

impl WriteProbe for SqliteWriteProbe {
    async fn probe_write(&self) -> Result<(), PersistenceFailure> {
        let started = std::time::Instant::now();
        let result = self.take_and_release_write_lock().await;
        let error = result
            .as_ref()
            .err()
            .map(|SqliteWriteProbeError::Sql(error)| error);
        metrics::histogram!(
            "silverhand_db_readiness_probe_duration_seconds",
            "outcome" => super::statement_metrics::outcome(error),
        )
        .record(started.elapsed().as_secs_f64());
        result.map_err(PersistenceFailure::from_source)
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, time::Duration};

    use sqlx::{
        Connection, Executor,
        sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
    };
    use tempfile::TempDir;

    use super::*;
    use crate::conversation_messaging::messaging::{HostEnvironment, MessagingStore};

    /// A product database plus the store the process writes through. Every probe under
    /// test is minted from that store, so these are tests of the production shape and
    /// not of a pool this module invented.
    async fn store_over_a_product_database(
        dir: &TempDir,
        busy_timeout: Duration,
    ) -> (std::path::PathBuf, MessagingStore) {
        let path = dir.path().join("silverhand.sqlite");
        crate::current_schema::create_current_schema(&path)
            .await
            .close()
            .await
            .expect("release the schema handle");
        let store = MessagingStore::open_with_busy_timeout(&path, HostEnvironment, busy_timeout)
            .await
            .expect("open the store the process writes through");
        (path, store)
    }

    /// Hold the write lock from a connection no store here owns — what a concurrent
    /// writer in another process looks like.
    async fn writer_holding_the_lock(path: &Path) -> sqlx::SqlitePool {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(false)
            .journal_mode(SqliteJournalMode::Wal);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .expect("open the competing writer");
        pool.execute(IMMEDIATE_WRITE_BEGIN)
            .await
            .expect("take the write lock");
        pool
    }

    #[tokio::test]
    async fn a_writable_store_answers_ok_and_stays_probeable() {
        let dir = TempDir::new().expect("temp dir");
        let (_, store) = store_over_a_product_database(&dir, Duration::from_secs(5)).await;
        let probe = store.write_probe();

        // Repeated calls remain usable through the store's shared pool. This does
        // not assert which pooled connection served either call.
        probe.probe_write().await.expect("first probe");
        probe.probe_write().await.expect("second probe");
    }

    /// The failure the probe exists to catch: the lock is held elsewhere, so the
    /// process cannot write right now. And the refusal must not poison the store's
    /// handle — once the lock is free the same probe answers `Ok` again, which is what
    /// proves the failed attempt left nothing open either.
    ///
    /// The store is opened with a short busy timeout so the refusal is prompt; the
    /// statement, the lock it takes and the rollback are the production ones.
    #[tokio::test]
    async fn a_held_write_lock_is_a_refusal_the_probe_recovers_from() {
        let dir = TempDir::new().expect("temp dir");
        let (path, store) = store_over_a_product_database(&dir, Duration::from_millis(25)).await;
        let probe = store.write_probe();
        let competitor = writer_holding_the_lock(&path).await;

        let refused = probe.probe_write().await.expect_err("the lock is taken");
        // The engine's own words stay reachable through the failure's source chain —
        // an operational surface has something to say beyond "not writable".
        let cause = std::error::Error::source(&refused).expect("the driver cause");
        assert!(
            cause.to_string().to_lowercase().contains("database"),
            "unexpected cause: {cause}"
        );

        competitor
            .execute("ROLLBACK")
            .await
            .expect("release the write lock");
        probe
            .probe_write()
            .await
            .expect("probe after the lock frees");
    }
}
