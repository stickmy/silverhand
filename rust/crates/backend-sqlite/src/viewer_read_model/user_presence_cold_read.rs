//! SQLite adapter for the stored half of the `user_presence` cold read.
//!
//! One statement reads every viewer's id, last-seen stamp, and creation instant
//! in creation order, the same set the TypeScript `users.list()` (`SELECT * FROM
//! users ORDER BY created_at ASC`) hands its presence map. Where the TypeScript
//! leaves a `created_at` tie to the engine's incidental order, this adapter
//! refines that underdetermined boundary with an explicit `id ASC` tiebreak, so
//! the loaded rows stand in ONE total order the sealed snapshot can verify.
//! Because it is a SINGLE statement, SQLite already runs it in one implicit
//! transaction — there is no second read to keep consistent with it, so no
//! explicit deferred snapshot is taken. The live presence half is NOT read here:
//! it comes from the process registry behind `OnlineViewers`, which the boundary
//! composes with these facts.
//!
//! `id` is decoded through the domain's fallible [`UserId`] parser: a stored id
//! that cannot be a legal identity is corruption of a committed row and surfaces
//! as a `PersistenceFailure` (a safe `500`), never coerced to a default. The
//! nullable `last_seen_at` is decoded the same way through [`LastSeenAt`]: a
//! stored stamp that is negative or beyond the JS safe-integer range the wire
//! carries losslessly is corruption of a committed row, refused HERE at decode —
//! before liveness is consulted — so an online viewer whose stamp would be
//! suppressed still cannot carry a corrupt one to the wire. `created_at` is read
//! only as the ordering key: `ORDER BY created_at ASC, id ASC` fixes ONE total
//! order over the set so a tie cannot depend on the engine's incidental order,
//! and the sealed `PresenceSnapshot` the boundary builds refuses anything else.

use std::{path::Path, time::Duration};

use silverhand_backend_core::{
    foundation::identity::UserId,
    foundation::persistence::PersistenceFailure,
    viewer_read_model::presence::{LastSeenAt, LastSeenOutOfRange, UserPresenceFacts},
    viewer_read_model::user_presence_cold_read::UserPresenceReadModel,
};
use sqlx::{Row, SqlitePool};
use thiserror::Error;

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

const SELECT_USER_PRESENCE_FACTS: &str =
    "SELECT id, last_seen_at, created_at FROM users ORDER BY created_at ASC, id ASC";

#[derive(Clone)]
pub struct SqliteUserPresenceColdRead {
    pool: SqlitePool,
}

/// Read failures surfaced only as the source of a [`PersistenceFailure`]. Both
/// variants are a driver fault or corruption of a committed row, never a
/// product-level refusal.
#[derive(Debug, Error)]
pub enum UserPresenceColdReadError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("stored column users.id does not name a usable identity")]
    MalformedIdentity,
    #[error(
        "stored column users.last_seen_at holds {value}, which is not a wire-safe non-negative instant"
    )]
    MalformedStamp { value: i64 },
}

impl SqliteUserPresenceColdRead {
    pub fn from_pool(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn open(path: impl AsRef<Path>) -> Result<Self, UserPresenceColdReadError> {
        let pool = crate::foundation::database::store_pool(path.as_ref(), BUSY_TIMEOUT).await?;
        Ok(Self { pool })
    }

    async fn read_facts(&self) -> Result<Vec<UserPresenceFacts>, UserPresenceColdReadError> {
        let rows = sqlx::query(SELECT_USER_PRESENCE_FACTS)
            .fetch_all(&self.pool)
            .await?;
        let mut facts = Vec::with_capacity(rows.len());
        for row in &rows {
            let id: String = row.try_get("id")?;
            let id = UserId::parse(id).map_err(|_| UserPresenceColdReadError::MalformedIdentity)?;
            let stored: Option<i64> = row.try_get("last_seen_at")?;
            let last_seen_at = stored.map(LastSeenAt::from_stored).transpose().map_err(
                |LastSeenOutOfRange(value)| UserPresenceColdReadError::MalformedStamp { value },
            )?;
            let created_at: i64 = row.try_get("created_at")?;
            facts.push(UserPresenceFacts {
                id,
                last_seen_at,
                created_at,
            });
        }
        Ok(facts)
    }
}

impl UserPresenceReadModel for SqliteUserPresenceColdRead {
    async fn list_user_presence_facts(&self) -> Result<Vec<UserPresenceFacts>, PersistenceFailure> {
        self.read_facts()
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

    #[tokio::test]
    async fn reads_every_viewer_in_creation_order_with_the_nullable_stamp_verbatim() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("presence-facts.sqlite");
        seed(
            &path,
            "INSERT INTO users (id, name, display_name, created_at, updated_at, last_seen_at) VALUES \
             ('user-late', 'late', 'Late', 200, 200, 1700), \
             ('user-early', 'early', 'Early', 100, 100, NULL)",
        )
        .await;

        let adapter = SqliteUserPresenceColdRead::open(&path).await.unwrap();
        let facts = adapter.list_user_presence_facts().await.unwrap();

        assert_eq!(
            facts,
            vec![
                UserPresenceFacts {
                    id: UserId::parse("user-early").unwrap(),
                    last_seen_at: None,
                    created_at: 100,
                },
                UserPresenceFacts {
                    id: UserId::parse("user-late").unwrap(),
                    last_seen_at: Some(LastSeenAt::from_stored(1700).unwrap()),
                    created_at: 200,
                },
            ]
        );
    }

    #[tokio::test]
    async fn a_negative_stored_stamp_is_refused_at_decode() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("presence-negative.sqlite");
        seed(
            &path,
            "INSERT INTO users (id, name, display_name, created_at, updated_at, last_seen_at) VALUES \
             ('user-1', 'one', 'One', 100, 100, -1)",
        )
        .await;

        let adapter = SqliteUserPresenceColdRead::open(&path).await.unwrap();
        let error = adapter
            .read_facts()
            .await
            .expect_err("a negative stamp is corruption of a committed row");
        assert!(
            matches!(
                error,
                UserPresenceColdReadError::MalformedStamp { value: -1 }
            ),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn a_stamp_beyond_the_js_safe_range_is_refused_at_decode() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("presence-huge.sqlite");
        // 9_007_199_254_740_992 == MAX_SAFE_INTEGER + 1: the first instant the
        // `number` wire could not carry losslessly.
        seed(
            &path,
            "INSERT INTO users (id, name, display_name, created_at, updated_at, last_seen_at) VALUES \
             ('user-1', 'one', 'One', 100, 100, 9007199254740992)",
        )
        .await;

        let adapter = SqliteUserPresenceColdRead::open(&path).await.unwrap();
        let error = adapter
            .read_facts()
            .await
            .expect_err("a stamp past MAX_SAFE is corruption of a committed row");
        assert!(
            matches!(
                error,
                UserPresenceColdReadError::MalformedStamp {
                    value: 9_007_199_254_740_992
                }
            ),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn a_stamp_at_the_js_safe_maximum_is_accepted() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("presence-max.sqlite");
        // 9_007_199_254_740_991 == MAX_SAFE_INTEGER: the last instant that DOES
        // round-trip, the positive control for the two refusals above.
        seed(
            &path,
            "INSERT INTO users (id, name, display_name, created_at, updated_at, last_seen_at) VALUES \
             ('user-1', 'one', 'One', 100, 100, 9007199254740991)",
        )
        .await;

        let adapter = SqliteUserPresenceColdRead::open(&path).await.unwrap();
        let facts = adapter.list_user_presence_facts().await.unwrap();
        assert_eq!(
            facts[0].last_seen_at,
            Some(LastSeenAt::from_stored(9_007_199_254_740_991).unwrap())
        );
    }

    #[tokio::test]
    async fn a_created_at_tie_is_broken_by_ascending_id() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("presence-tie.sqlite");
        // Same instant, inserted id-descending: only the `id ASC` tiebreak makes
        // the loaded order stable, and the sealed snapshot requires it.
        seed(
            &path,
            "INSERT INTO users (id, name, display_name, created_at, updated_at, last_seen_at) VALUES \
             ('user-b', 'b', 'B', 100, 100, NULL), \
             ('user-a', 'a', 'A', 100, 100, NULL)",
        )
        .await;

        let adapter = SqliteUserPresenceColdRead::open(&path).await.unwrap();
        let facts = adapter.list_user_presence_facts().await.unwrap();
        assert_eq!(
            facts
                .iter()
                .map(|fact| fact.id.as_str().to_owned())
                .collect::<Vec<_>>(),
            vec!["user-a".to_owned(), "user-b".to_owned()]
        );
    }
}
