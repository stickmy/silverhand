//! SQLite adapter for the durable half of presence: persisting a viewer's
//! sealed last-seen instant.
//!
//! The write counterpart of [`crate::viewer_read_model::user_presence_cold_read`]. It mirrors the
//! TypeScript `UserRepo.touchLastSeen` exactly — `UPDATE users SET last_seen_at =
//! ? WHERE id = ?` — so the two implementations write the same column with the
//! same single statement. The value is a sealed
//! [`silverhand_backend_core::viewer_read_model::presence::LastSeenAt`] whose range was
//! enforced once, where the disconnect clock entered the domain, so this adapter
//! binds it verbatim and never re-validates.
//!
//! The addressed viewer is a registered user, so the row must exist. The
//! TypeScript returns `changes > 0` and its caller logs a lost persist without
//! blocking the offline fact; this adapter surfaces a zero-row update as a named
//! [`PersistenceFailure`] source, and the grace timer likewise audits it and
//! still publishes offline — the same observable behavior, with the missing row
//! made an explicit fault rather than a silent no-op. `id` is the primary key, so
//! a match can never touch more than one row: exactly one is the whole contract.

use std::{path::Path, time::Duration};

use silverhand_backend_core::{
    foundation::identity::UserId, foundation::persistence::PersistenceFailure,
    viewer_read_model::presence::LastSeenAt,
    viewer_read_model::user_presence_last_seen::LastSeenSink,
};
use sqlx::SqlitePool;
use thiserror::Error;

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

const UPDATE_LAST_SEEN: &str = "UPDATE users SET last_seen_at = ? WHERE id = ?";

#[derive(Clone)]
pub struct SqliteLastSeenSink {
    pool: SqlitePool,
}

/// Write failures surfaced only as the source of a [`PersistenceFailure`]. Both
/// variants are a driver fault or a broken row-count invariant, never a
/// product-level refusal.
#[derive(Debug, Error)]
pub enum SqliteLastSeenError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("last_seen_at update matched {matched} rows, not the one addressed viewer")]
    UnexpectedRowCount { matched: u64 },
}

impl SqliteLastSeenSink {
    pub fn from_pool(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn open(path: impl AsRef<Path>) -> Result<Self, SqliteLastSeenError> {
        let pool = crate::foundation::database::store_pool(path.as_ref(), BUSY_TIMEOUT).await?;
        Ok(Self { pool })
    }

    async fn write(
        &self,
        user_id: &UserId,
        last_seen_at: LastSeenAt,
    ) -> Result<(), SqliteLastSeenError> {
        let matched = sqlx::query(UPDATE_LAST_SEEN)
            .bind(last_seen_at.get())
            .bind(user_id.as_str())
            .execute(&self.pool)
            .await?
            .rows_affected();
        if matched != 1 {
            return Err(SqliteLastSeenError::UnexpectedRowCount { matched });
        }
        Ok(())
    }
}

impl LastSeenSink for SqliteLastSeenSink {
    async fn touch_last_seen(
        &self,
        user_id: UserId,
        last_seen_at: LastSeenAt,
    ) -> Result<(), PersistenceFailure> {
        self.write(&user_id, last_seen_at)
            .await
            .map_err(PersistenceFailure::from_source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use silverhand_backend_core::viewer_read_model::presence::MAX_SAFE_INTEGER;
    use sqlx::sqlite::SqliteConnectOptions;
    use sqlx::{Connection, Executor, Row, SqliteConnection};

    const CURRENT_SCHEMA: &str =
        include_str!("../../../../../rust/test-fixtures/migration-baseline/schema-current.sql");

    async fn seed(path: &Path, users_sql: &'static str) {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .foreign_keys(true);
        let mut db = SqliteConnection::connect_with(&options).await.unwrap();
        db.execute(sqlx::raw_sql(CURRENT_SCHEMA)).await.unwrap();
        db.execute(sqlx::raw_sql(users_sql)).await.unwrap();
        db.close().await.unwrap();
    }

    async fn stored_last_seen(path: &Path, id: &str) -> Option<i64> {
        let mut db = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
            .await
            .unwrap();
        let row = sqlx::query("SELECT last_seen_at FROM users WHERE id = ?")
            .bind(id)
            .fetch_one(&mut db)
            .await
            .unwrap();
        let value: Option<i64> = row.try_get("last_seen_at").unwrap();
        db.close().await.unwrap();
        value
    }

    #[tokio::test]
    async fn touching_a_viewer_writes_the_sealed_stamp_verbatim_on_exactly_one_row() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("touch.sqlite");
        // A viewer whose stamp starts NULL, so the write is observable end to end.
        seed(
            &path,
            "INSERT INTO users (id, name, display_name, created_at, updated_at, last_seen_at) VALUES \
             ('viewer-1', 'one', 'One', 100, 100, NULL), \
             ('viewer-other', 'other', 'Other', 200, 200, 42)",
        )
        .await;

        let sink = SqliteLastSeenSink::open(&path).await.unwrap();
        sink.touch_last_seen(
            UserId::parse("viewer-1").unwrap(),
            LastSeenAt::from_stored(1_700_000_000_123).unwrap(),
        )
        .await
        .unwrap();

        assert_eq!(
            stored_last_seen(&path, "viewer-1").await,
            Some(1_700_000_000_123)
        );
        assert_eq!(stored_last_seen(&path, "viewer-other").await, Some(42));
    }

    #[tokio::test]
    async fn the_maximum_wire_safe_stamp_round_trips_unchanged() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("touch-max.sqlite");
        seed(
            &path,
            "INSERT INTO users (id, name, display_name, created_at, updated_at, last_seen_at) VALUES \
             ('viewer-1', 'one', 'One', 100, 100, 42)",
        )
        .await;

        let sink = SqliteLastSeenSink::open(&path).await.unwrap();
        sink.touch_last_seen(
            UserId::parse("viewer-1").unwrap(),
            LastSeenAt::from_stored(MAX_SAFE_INTEGER).unwrap(),
        )
        .await
        .unwrap();

        assert_eq!(
            stored_last_seen(&path, "viewer-1").await,
            Some(MAX_SAFE_INTEGER)
        );
    }

    #[tokio::test]
    async fn touching_an_absent_viewer_is_a_named_zero_row_fault_not_a_silent_noop() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("touch-absent.sqlite");
        seed(&path, "SELECT 1").await;

        let sink = SqliteLastSeenSink::open(&path).await.unwrap();
        // The raw write names the broken invariant...
        let error = sink
            .write(
                &UserId::parse("ghost").unwrap(),
                LastSeenAt::from_stored(1_000).unwrap(),
            )
            .await
            .expect_err("no such viewer row must not be a silent success");
        assert!(
            matches!(
                error,
                SqliteLastSeenError::UnexpectedRowCount { matched: 0 }
            ),
            "{error:?}"
        );
        // ...and the port surfaces it as a persistence failure whose SOURCE is
        // that exact named row-count fault. Downcasting the chain (not just
        // `is_err`) is what makes this go RED if the mapping is ever weakened to
        // a generic failure that drops the SqliteLastSeenError.
        let failure = sink
            .touch_last_seen(
                UserId::parse("ghost").unwrap(),
                LastSeenAt::from_stored(1_000).unwrap(),
            )
            .await
            .expect_err("the port must surface the zero-row fault");
        let source = std::error::Error::source(&failure)
            .and_then(|source| source.downcast_ref::<SqliteLastSeenError>())
            .expect("the persistence failure must carry the sqlite last-seen error as its source");
        assert!(
            matches!(
                source,
                SqliteLastSeenError::UnexpectedRowCount { matched: 0 }
            ),
            "{source:?}"
        );
    }
}
