//! SQLite adapter for the session context's `auth_sessions` authority.
//!
//! [`SqliteSessionStore`] offers all three faces of that one authority: reading a
//! bearer with throttled activity renewal ([`SessionValidationPort`]), persisting a freshly minted
//! coordinate at signup/login ([`SessionIssue`]), and retiring one at logout
//! ([`SessionRevoke`]). Only the plaintext bearer's SHA-256 coordinate is ever
//! persisted — never the plaintext itself.
//!
//! The session-row primary key is an opaque persistence surrogate the core
//! deliberately does not model (see `session_write`); the adapter allocates it
//! here, mirroring the product's `randomBytes(12).toString('base64url')` — twelve
//! entropy bytes rendered base64url without padding (exactly sixteen characters).
//! The store therefore owns an [`EntropySource`], injected so tests are
//! deterministic; the clock stays in the application (`now_ms` is passed in).

use std::{path::Path, time::Duration};

use silverhand_backend_core::{
    foundation::identity::UserId,
    foundation::persistence::PersistenceFailure,
    host_runtime::environment::{EntropySource, OsEntropy},
    identity_access::auth::{
        SessionAudit, SessionIssue, SessionRevoke, SessionTokenHash, SessionValidation,
        SessionValidationPort,
    },
};
use silverhand_backend_observability::Observe;
use sqlx::Row;
use thiserror::Error;

/// The product's `randomBytes(12)`: twelve entropy bytes → sixteen base64url
/// characters (12 is a multiple of 3, so the encoding never pads).
const SESSION_ID_BYTES: usize = 12;

const SESSION_TOUCH_INTERVAL_MS: i64 = 5 * 60 * 1_000;

async fn read_valid_session<'e>(
    executor: impl sqlx::Executor<'e, Database = sqlx::Sqlite>,
    token_hash: &SessionTokenHash,
    validation: SessionValidation,
) -> Result<Option<(UserId, i64)>, PersistenceFailure> {
    let row = crate::foundation::statement_metrics::statement(
        "auth.verify",
        "get",
        sqlx::query(
            "SELECT s.user_id, s.last_used_at, u.id AS existing_user
            FROM auth_sessions s LEFT JOIN users u ON u.id = s.user_id
            WHERE s.token_hash = ? AND s.revoked_at IS NULL AND s.last_used_at + ? >= ?",
        )
        .bind(token_hash.as_str())
        .bind(validation.ttl_ms())
        .bind(validation.now_ms())
        .fetch_optional(executor),
    )
    .await
    .map_err(persistence)?;
    let Some(row) = row else {
        return Ok(None);
    };

    if row
        .try_get::<Option<String>, _>("existing_user")
        .map_err(persistence)?
        .is_none()
    {
        return Err(PersistenceFailure::from_source(DanglingSessionUser));
    }
    let raw: String = row.try_get("user_id").map_err(persistence)?;
    let user = UserId::parse(raw)
        .map_err(|error| PersistenceFailure::from_source(InvalidPersistedUserId(error)))?;
    let last_used = row.try_get("last_used_at").map_err(persistence)?;
    Ok(Some((user, last_used)))
}

async fn read_from_pool(
    pool: &crate::foundation::database::SqliteDatabase,
    token_hash: &SessionTokenHash,
    validation: SessionValidation,
) -> Result<Option<(UserId, i64)>, PersistenceFailure> {
    let mut connection = pool
        .acquire()
        .observe(silverhand_backend_observability::operation_span!(
            "db.pool.acquire"
        ))
        .await
        .map_err(persistence)?;
    read_valid_session(&mut *connection, token_hash, validation).await
}

fn renewal_due(last_used: i64, now: i64) -> bool {
    now.saturating_sub(last_used) >= SESSION_TOUCH_INTERVAL_MS
}

const INSERT_SESSION: &str = "\
INSERT INTO auth_sessions \
  (id, user_id, token_hash, created_at, last_used_at, revoked_at, user_agent, ip_first_seen) \
VALUES (?, ?, ?, ?, ?, NULL, ?, ?)";

const REVOKE_BY_TOKEN_HASH: &str = "\
UPDATE auth_sessions SET revoked_at = ? WHERE token_hash = ? AND revoked_at IS NULL";

/// A pooled connection to an existing product database offering the session
/// authority. Owns an [`EntropySource`] for the opaque session-row surrogate;
/// holds no clock (`now_ms` is passed into the writes by the application).
#[derive(Clone)]
pub struct SqliteSessionStore<E = OsEntropy> {
    pool: crate::foundation::database::SqliteDatabase,
    entropy: E,
}

#[derive(Debug, Error)]
pub enum SqliteSessionStoreError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
}

impl SqliteSessionStore<OsEntropy> {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, SqliteSessionStoreError> {
        Self::open_with_entropy(path, OsEntropy).await
    }

    /// Construct over an already-open pool with the OS entropy source.
    pub fn from_pool(pool: impl Into<crate::foundation::database::SqliteDatabase>) -> Self {
        let pool = pool.into();
        Self {
            pool,
            entropy: OsEntropy,
        }
    }
}

impl<E> SqliteSessionStore<E>
where
    E: EntropySource,
{
    /// Open with an injected entropy source — tests use a deterministic one so the
    /// allocated session-row id is reproducible.
    pub async fn open_with_entropy(
        path: impl AsRef<Path>,
        entropy: E,
    ) -> Result<Self, SqliteSessionStoreError> {
        let pool =
            crate::foundation::database::store_pool(path.as_ref(), Duration::from_secs(5)).await?;
        Ok(Self { pool, entropy })
    }

    /// Construct over an already-open pool with an injected entropy source.
    pub fn from_pool_with_entropy(
        pool: impl Into<crate::foundation::database::SqliteDatabase>,
        entropy: E,
    ) -> Self {
        let pool = pool.into();
        Self { pool, entropy }
    }

    fn allocate_session_id(&self) -> Result<String, PersistenceFailure> {
        let mut bytes = [0u8; SESSION_ID_BYTES];
        self.entropy
            .fill(&mut bytes)
            .map_err(PersistenceFailure::from_source)?;
        Ok(base64url_no_pad(&bytes))
    }

    #[tracing::instrument(
        target = "silverhand::diagnostics",
        name = "db.auth.verify_and_touch",
        skip_all,
        fields(
            elapsed_ms = tracing::field::Empty,
            outcome = tracing::field::Empty,
            operation_finished = tracing::field::Empty,
        )
    )]
    async fn verify_with_renewal(
        &self,
        token_hash: SessionTokenHash,
        validation: SessionValidation,
    ) -> Result<Option<UserId>, PersistenceFailure> {
        let observation = silverhand_backend_observability::Operation::current();
        let result = async {
            // WAL readers do not queue behind an unrelated writer. This snapshot is
            // the admission point for recent activity, including uncommitted revokes.
            let Some((user, last_used)) =
                read_from_pool(&self.pool, &token_hash, validation).await?
            else {
                return Ok(None);
            };
            if !renewal_due(last_used, validation.now_ms()) {
                return Ok(Some(user));
            }

            // Acquire the writer before re-reading: concurrent renewals or a revoke
            // committed while we waited must win over the first snapshot.
            let mut transaction =
                crate::foundation::statement_metrics::begin(&self.pool, Some("BEGIN IMMEDIATE"))
                    .await
                    .map_err(persistence)?;
            let current = read_valid_session(&mut *transaction, &token_hash, validation).await?;
            if let Some((_, last_used)) = &current
                && renewal_due(*last_used, validation.now_ms())
            {
                crate::foundation::statement_metrics::statement(
                    "auth.touch",
                    "run",
                    sqlx::query("UPDATE auth_sessions SET last_used_at = ? WHERE token_hash = ?")
                        .bind(validation.now_ms())
                        .bind(token_hash.as_str())
                        .execute(&mut *transaction),
                )
                .await
                .map_err(persistence)?;
            }
            crate::foundation::statement_metrics::statement(
                "auth.commit",
                "run",
                transaction.commit(),
            )
            .await
            .map_err(persistence)?;
            Ok(current.map(|(user, _)| user))
        }
        .await;
        observation.finish(&result);
        result
    }
}

impl<E> SessionValidationPort for SqliteSessionStore<E>
where
    E: EntropySource + Clone + 'static,
{
    async fn verify(
        &self,
        token_hash: SessionTokenHash,
        validation: SessionValidation,
    ) -> Result<Option<UserId>, PersistenceFailure> {
        Ok(read_from_pool(&self.pool, &token_hash, validation)
            .await?
            .map(|(user, _)| user))
    }

    async fn verify_and_touch(
        &self,
        token_hash: SessionTokenHash,
        validation: SessionValidation,
    ) -> Result<Option<UserId>, PersistenceFailure> {
        self.verify_with_renewal(token_hash, validation).await
    }
}

impl<E> SessionIssue for SqliteSessionStore<E>
where
    E: EntropySource + Clone + 'static,
{
    async fn issue_verified(
        &self,
        user_id: &UserId,
        expected_phc: &str,
        token_hash: SessionTokenHash,
        audit: &SessionAudit,
        now_ms: i64,
    ) -> Result<bool, PersistenceFailure> {
        let id = self.allocate_session_id()?;
        let result = sqlx::query(
            "INSERT INTO auth_sessions
            (id,user_id,token_hash,created_at,last_used_at,revoked_at,user_agent,ip_first_seen)
            SELECT ?,?,?,?, ?,NULL,?,? WHERE EXISTS
            (SELECT 1 FROM user_password_credentials WHERE user_id = ? AND password_hash = ?)",
        )
        .bind(id)
        .bind(user_id.as_str())
        .bind(token_hash.as_str())
        .bind(now_ms)
        .bind(now_ms)
        .bind(audit.user_agent())
        .bind(audit.ip_first_seen())
        .bind(user_id.as_str())
        .bind(expected_phc)
        .execute(&self.pool)
        .await
        .map_err(persistence)?;
        Ok(result.rows_affected() == 1)
    }

    async fn issue(
        &self,
        user_id: &UserId,
        token_hash: SessionTokenHash,
        audit: &SessionAudit,
        now_ms: i64,
    ) -> Result<(), PersistenceFailure> {
        let id = self.allocate_session_id()?;
        // created_at and last_used_at start equal — a just-issued session has been
        // "used" exactly once, at issue time.
        sqlx::query(INSERT_SESSION)
            .bind(id)
            .bind(user_id.as_str())
            .bind(token_hash.as_str())
            .bind(now_ms)
            .bind(now_ms)
            .bind(audit.user_agent())
            .bind(audit.ip_first_seen())
            .execute(&self.pool)
            .await
            .map_err(persistence)?;
        Ok(())
    }
}

impl<E> SessionRevoke for SqliteSessionStore<E>
where
    E: EntropySource + Clone + 'static,
{
    async fn revoke(
        &self,
        token_hash: SessionTokenHash,
        now_ms: i64,
    ) -> Result<bool, PersistenceFailure> {
        // Only a LIVE row is retired; the `revoked_at IS NULL` guard makes a repeat
        // revoke affect zero rows, so an already-revoked (or unknown) coordinate is
        // `false`, never an error.
        let result = sqlx::query(REVOKE_BY_TOKEN_HASH)
            .bind(now_ms)
            .bind(token_hash.as_str())
            .execute(&self.pool)
            .await
            .map_err(persistence)?;
        Ok(result.rows_affected() > 0)
    }
}

/// Encode bytes as base64url without padding — the `toString('base64url')` the
/// product uses for the session-row surrogate. No external crate: the alphabet is
/// fixed and the only input is a fixed-width entropy buffer.
fn base64url_no_pad(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as usize;
        out.push(ALPHABET[b0 >> 2] as char);
        match chunk.len() {
            1 => {
                out.push(ALPHABET[(b0 & 0b11) << 4] as char);
            }
            2 => {
                let b1 = chunk[1] as usize;
                out.push(ALPHABET[((b0 & 0b11) << 4) | (b1 >> 4)] as char);
                out.push(ALPHABET[(b1 & 0b1111) << 2] as char);
            }
            _ => {
                let b1 = chunk[1] as usize;
                let b2 = chunk[2] as usize;
                out.push(ALPHABET[((b0 & 0b11) << 4) | (b1 >> 4)] as char);
                out.push(ALPHABET[((b1 & 0b1111) << 2) | (b2 >> 6)] as char);
                out.push(ALPHABET[b2 & 0b111111] as char);
            }
        }
    }
    out
}

fn persistence(error: sqlx::Error) -> PersistenceFailure {
    PersistenceFailure::from_source(error)
}

#[derive(Debug, Error)]
#[error("persisted session user id is invalid")]
struct InvalidPersistedUserId(#[source] silverhand_backend_core::foundation::identity::EmptyUserId);

#[derive(Debug, Error)]
#[error("authenticated session references a missing user")]
struct DanglingSessionUser;

#[cfg(test)]
mod tests {
    use super::*;
    use silverhand_backend_core::identity_access::auth::{SessionTokenHash, SessionTtl};
    use sqlx::sqlite::SqliteConnectOptions;
    use sqlx::{Connection, Executor, SqliteConnection};
    use std::sync::Mutex;

    const CURRENT_SCHEMA: &str =
        include_str!("../../../../../rust/test-fixtures/migration-baseline/schema-current.sql");

    const SEED_SQL: &str = "\
INSERT INTO users (id, name, display_name, email, created_at, updated_at) \
  VALUES ('user-1', 'alice', 'Alice', 'alice@example.com', 1, 1);";

    /// A deterministic entropy source dispensing a scripted queue of bytes.
    #[derive(Clone)]
    struct ScriptedEntropy {
        bytes: std::sync::Arc<Mutex<std::collections::VecDeque<u8>>>,
    }

    impl ScriptedEntropy {
        fn new(bytes: Vec<u8>) -> Self {
            Self {
                bytes: std::sync::Arc::new(Mutex::new(bytes.into())),
            }
        }
    }

    #[derive(Debug, Error)]
    #[error("scripted entropy exhausted")]
    struct EntropyExhausted;

    impl EntropySource for ScriptedEntropy {
        type Error = EntropyExhausted;
        fn fill(&self, destination: &mut [u8]) -> Result<(), Self::Error> {
            let mut queue = self.bytes.lock().unwrap();
            for slot in destination.iter_mut() {
                *slot = queue.pop_front().ok_or(EntropyExhausted)?;
            }
            Ok(())
        }
    }

    async fn seed_db() -> (tempfile::TempDir, std::path::PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.sqlite");
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .foreign_keys(true);
        let mut db = SqliteConnection::connect_with(&options).await.unwrap();
        db.execute(sqlx::raw_sql(CURRENT_SCHEMA)).await.unwrap();
        db.execute(sqlx::raw_sql(SEED_SQL)).await.unwrap();
        db.close().await.unwrap();
        (directory, path)
    }

    #[tokio::test]
    async fn session_pool_retains_full_durability() {
        let (_directory, path) = seed_db().await;
        let store = SqliteSessionStore::open(&path).await.unwrap();
        let mut connections = Vec::new();
        for _ in 0..store.pool.options().get_max_connections() {
            connections.push(store.pool.acquire().await.unwrap());
        }
        for connection in &mut connections {
            let synchronous: i64 = sqlx::query_scalar("PRAGMA synchronous")
                .fetch_one(&mut **connection)
                .await
                .unwrap();
            let journal: String = sqlx::query_scalar("PRAGMA journal_mode")
                .fetch_one(&mut **connection)
                .await
                .unwrap();
            assert_eq!(synchronous, 2);
            assert_eq!(journal, "wal");
        }
        drop(connections);
        store.pool.close().await;
    }

    fn hash(plaintext: &str) -> SessionTokenHash {
        SessionTokenHash::of_plaintext(plaintext)
    }

    fn ttl() -> SessionTtl {
        SessionTtl::from_millis(100_000).unwrap()
    }

    #[test]
    fn base64url_encodes_twelve_bytes_to_sixteen_unpadded_chars() {
        // The 12-byte, 16-char, no-pad shape the product's session id relies on.
        let id = base64url_no_pad(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]);
        assert_eq!(id.len(), 16);
        assert!(!id.contains('='));
        // A known RFC 4648 §10 vector, translated to the url alphabet: "sure." is
        // "c3VyZS4" in base64url (no pad).
        assert_eq!(base64url_no_pad(b"sure."), "c3VyZS4");
        // The two url-safe substitutions actually occur (0xFB,0xFF -> '-','_').
        assert_eq!(base64url_no_pad(&[0xfb, 0xff]), "-_8");
    }

    #[tokio::test]
    async fn issue_commits_a_live_session_that_validates_then_revoke_retires_it() {
        let (_dir, path) = seed_db().await;
        let entropy = ScriptedEntropy::new(vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]);
        let store = SqliteSessionStore::open_with_entropy(&path, entropy)
            .await
            .unwrap();
        let token = hash("bearer-plaintext");
        let audit = SessionAudit::capture(Some("Mozilla/5.0"), Some("203.0.113.7"));

        store
            .issue(
                &UserId::parse("user-1").unwrap(),
                hash("bearer-plaintext"),
                &audit,
                500,
            )
            .await
            .unwrap();

        // The committed row carries the audit breadcrumbs and the allocated 16-char id.
        let reopen = SqliteConnectOptions::new()
            .filename(&path)
            .foreign_keys(true);
        let mut db = SqliteConnection::connect_with(&reopen).await.unwrap();
        let row = sqlx::query(
            "SELECT id, user_agent, ip_first_seen, created_at, last_used_at, revoked_at \
               FROM auth_sessions WHERE token_hash = ?",
        )
        .bind(token.as_str())
        .fetch_one(&mut db)
        .await
        .unwrap();
        let id: String = row.try_get("id").unwrap();
        assert_eq!(id.len(), 16);
        assert_eq!(
            row.try_get::<String, _>("user_agent").unwrap(),
            "Mozilla/5.0"
        );
        assert_eq!(
            row.try_get::<String, _>("ip_first_seen").unwrap(),
            "203.0.113.7"
        );
        assert_eq!(row.try_get::<i64, _>("created_at").unwrap(), 500);
        assert_eq!(row.try_get::<i64, _>("last_used_at").unwrap(), 500);
        assert_eq!(row.try_get::<Option<i64>, _>("revoked_at").unwrap(), None);
        db.close().await.unwrap();

        // It validates within its window, then a revoke retires it and a second
        // revoke of the same coordinate is idempotently `false`.
        let seen = store
            .verify_and_touch(
                hash("bearer-plaintext"),
                SessionValidation::new(1000, ttl()),
            )
            .await
            .unwrap();
        assert_eq!(seen.unwrap().as_str(), "user-1");

        assert!(
            store.revoke(hash("bearer-plaintext"), 2000).await.unwrap(),
            "a live session revokes"
        );
        assert!(
            !store.revoke(hash("bearer-plaintext"), 3000).await.unwrap(),
            "a second revoke is false"
        );

        // A revoked coordinate no longer validates.
        let after = store
            .verify_and_touch(
                hash("bearer-plaintext"),
                SessionValidation::new(4000, ttl()),
            )
            .await
            .unwrap();
        assert!(after.is_none());
    }

    #[tokio::test]
    async fn revoking_an_unknown_coordinate_is_false_not_an_error() {
        let (_dir, path) = seed_db().await;
        let store = SqliteSessionStore::open(&path).await.unwrap();
        assert!(!store.revoke(hash("never-issued"), 500).await.unwrap());
    }

    #[tokio::test]
    async fn a_null_audit_field_is_stored_as_null() {
        let (_dir, path) = seed_db().await;
        let entropy = ScriptedEntropy::new(vec![20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31]);
        let store = SqliteSessionStore::open_with_entropy(&path, entropy)
            .await
            .unwrap();
        let token = hash("bearer-2");
        store
            .issue(
                &UserId::parse("user-1").unwrap(),
                hash("bearer-2"),
                &SessionAudit::capture(None, None),
                500,
            )
            .await
            .unwrap();
        let reopen = SqliteConnectOptions::new()
            .filename(&path)
            .foreign_keys(true);
        let mut db = SqliteConnection::connect_with(&reopen).await.unwrap();
        let ua: Option<String> =
            sqlx::query_scalar("SELECT user_agent FROM auth_sessions WHERE token_hash = ?")
                .bind(token.as_str())
                .fetch_one(&mut db)
                .await
                .unwrap();
        assert_eq!(ua, None);
    }
}
