//! SQLite adapter for the [`HumanRegistrationUnitOfWork`] application-seam port —
//! the atomic first-time signup write.
//!
//! One attempt is one `BEGIN IMMEDIATE` transaction that creates the `users` row
//! AND upserts its `user_password_credentials` row, or neither. The surrogate
//! `users.id` is allocated here (the product's `randomHex(8)` — eight entropy
//! bytes rendered as sixteen lowercase hex digits) and a `users.id` PK collision
//! is retried on a fresh id, up to the same twelve attempts the product allows;
//! exhausting them is an infrastructure fault, not a product outcome.
//!
//! Only the handle and email conflicts the signup flow's security ordering must
//! arbitrate surface as outcomes ([`HumanRegistration::HandleConflict`] /
//! `EmailConflict`). Every OTHER driver rejection — a CHECK, a NOT NULL, a disk
//! error — passes through as a [`PersistenceFailure`] and is NEVER laundered into a
//! business answer: the conflict classifier decodes exactly the three uniqueness
//! rules the `users` write can trip and nothing else, mirroring the product's
//! single `throwAsUserWriteConflict` decoding site.

use std::{path::Path, time::Duration};

use silverhand_backend_core::{
    foundation::identity::UserId,
    foundation::persistence::PersistenceFailure,
    host_runtime::environment::{EntropySource, OsEntropy},
    identity_access::credential::PreparedPasswordCredential,
    identity_access::human_auth_flows::{HumanRegistration, HumanRegistrationUnitOfWork},
    identity_access::user_identity::{DisplayName, EmailAddress, ParticipantHandle, UserRecord},
};
use sqlx::Row;
use thiserror::Error;

use crate::identity_access::credential::serialize_password_hash_params;

const ROOT_WRITE_BEGIN: &str = "BEGIN IMMEDIATE";
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// The product's `randomHex(8)`: eight entropy bytes → sixteen lowercase hex digits.
const USER_ID_BYTES: usize = 8;
/// The product's id-allocation retry ceiling (`attempt < 12`).
const ID_ALLOCATION_ATTEMPTS: usize = 12;

const INSERT_USER: &str = "\
INSERT INTO users (id, name, display_name, avatar_storage_key, avatar_content_type, email, created_at, updated_at) \
VALUES (?, ?, ?, NULL, NULL, ?, ?, ?) \
RETURNING id, name, display_name, email, created_at, updated_at";
const UPSERT_CREDENTIAL: &str = "\
INSERT INTO user_password_credentials (user_id, password_hash, password_hash_params, password_updated_at) \
VALUES (?, ?, ?, ?) \
ON CONFLICT(user_id) DO UPDATE SET \
  password_hash = excluded.password_hash, \
  password_hash_params = excluded.password_hash_params, \
  password_updated_at = excluded.password_updated_at";

/// A pooled connection to an existing product database offering the atomic
/// signup write. Owns an [`EntropySource`] for surrogate id allocation; holds no
/// clock (the write stamps `now_ms`, passed in by the application).
#[derive(Clone)]
pub struct SqliteHumanRegistration<E = OsEntropy> {
    pool: crate::foundation::database::SqliteDatabase,
    entropy: E,
}

/// Startup/driver failures surfaced only to the composition root that opens the
/// store; the port method hands operational failures over as a `PersistenceFailure`.
#[derive(Debug, Error)]
pub enum SqliteHumanRegistrationError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
}

/// Why the atomic write could not yield a trustworthy answer. Neither arm is a
/// product outcome (those are the enum's conflict/registered arms): both reach the
/// caller as the source of one [`PersistenceFailure`].
#[derive(Debug, Error)]
enum RegistrationFailure {
    #[error(transparent)]
    Sql(#[from] sqlx::Error),
    #[error("secure random source failed")]
    Entropy(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("a committed users row holds an unusable identifier")]
    CorruptStoredUserId,
    #[error("could not allocate a unique user id")]
    IdAllocationExhausted,
}

/// The three `users`-write uniqueness rules the signup flow arbitrates. Any other
/// rejection is never one of these — it passes through as a persistence fault.
enum UsersConflict {
    /// `users.id` PK — an internal allocation collision, retried on a fresh id.
    UserId,
    /// `users.name` UNIQUE or the cross-namespace trigger — the shared @-handle.
    Name,
    /// The `uniq_users_email_when_set` partial unique index on `users.email`.
    Email,
}

/// The outcome of one atomic attempt.
enum Attempt {
    Registered(UserRecord),
    HandleConflict,
    EmailConflict,
    RetryNewId,
}

impl<E> SqliteHumanRegistration<E>
where
    E: EntropySource,
{
    async fn attempt(
        &self,
        id: &str,
        handle: &ParticipantHandle,
        display_name: &DisplayName,
        email: &EmailAddress,
        credential: &PreparedPasswordCredential,
        now_ms: i64,
    ) -> Result<Attempt, RegistrationFailure> {
        let mut tx = self.pool.begin_with(ROOT_WRITE_BEGIN).await?;

        let user_row = match sqlx::query(INSERT_USER)
            .bind(id)
            .bind(handle.as_str())
            .bind(display_name.as_str())
            .bind(email.as_str())
            .bind(now_ms)
            .bind(now_ms)
            .fetch_one(&mut *tx)
            .await
        {
            Ok(row) => row,
            Err(error) => {
                // The transaction rolls back on drop; classify the rejection. Only
                // the three known uniqueness rules become an outcome — anything else
                // is a genuine persistence fault the caller must see as a 500.
                drop(tx);
                return Ok(match classify_users_conflict(&error) {
                    Some(UsersConflict::UserId) => Attempt::RetryNewId,
                    Some(UsersConflict::Name) => Attempt::HandleConflict,
                    Some(UsersConflict::Email) => Attempt::EmailConflict,
                    None => return Err(RegistrationFailure::Sql(error)),
                });
            }
        };

        // The credential row keys on the brand-new user id, so its insert cannot
        // trip a `users` uniqueness rule; any failure here is a true fault.
        sqlx::query(UPSERT_CREDENTIAL)
            .bind(id)
            .bind(&credential.phc)
            .bind(serialize_password_hash_params(&credential.params))
            .bind(now_ms)
            .execute(&mut *tx)
            .await?;

        let user = decode_user(&user_row)?;
        tx.commit().await?;
        Ok(Attempt::Registered(user))
    }

    fn allocate_id(&self) -> Result<String, RegistrationFailure> {
        let mut bytes = [0u8; USER_ID_BYTES];
        self.entropy
            .fill(&mut bytes)
            .map_err(|error| RegistrationFailure::Entropy(Box::new(error)))?;
        Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
    }

    async fn register_inner(
        &self,
        handle: &ParticipantHandle,
        display_name: &DisplayName,
        email: &EmailAddress,
        credential: &PreparedPasswordCredential,
        now_ms: i64,
    ) -> Result<HumanRegistration, RegistrationFailure> {
        for _ in 0..ID_ALLOCATION_ATTEMPTS {
            let id = self.allocate_id()?;
            match self
                .attempt(&id, handle, display_name, email, credential, now_ms)
                .await?
            {
                Attempt::Registered(user) => return Ok(HumanRegistration::Registered { user }),
                Attempt::HandleConflict => return Ok(HumanRegistration::HandleConflict),
                Attempt::EmailConflict => return Ok(HumanRegistration::EmailConflict),
                Attempt::RetryNewId => continue,
            }
        }
        Err(RegistrationFailure::IdAllocationExhausted)
    }
}

impl SqliteHumanRegistration<OsEntropy> {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, SqliteHumanRegistrationError> {
        Self::open_with_entropy(path, OsEntropy).await
    }
}

impl<E> SqliteHumanRegistration<E>
where
    E: EntropySource,
{
    /// Open with an injected entropy source — tests use a deterministic one so the
    /// allocated ids (and thus the collision/retry path) are reproducible.
    pub async fn open_with_entropy(
        path: impl AsRef<Path>,
        entropy: E,
    ) -> Result<Self, SqliteHumanRegistrationError> {
        let pool = crate::foundation::database::store_pool(path.as_ref(), BUSY_TIMEOUT).await?;
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
}

/// Decode a `users` RETURNING row into the committed [`UserRecord`]. The id is
/// parsed through the domain's fallible parser: a malformed committed id is
/// corruption (a safe-500), never a product answer.
fn decode_user(row: &sqlx::sqlite::SqliteRow) -> Result<UserRecord, RegistrationFailure> {
    let raw_id: String = row.try_get("id")?;
    let id = UserId::parse(raw_id).map_err(|_| RegistrationFailure::CorruptStoredUserId)?;
    Ok(UserRecord {
        id,
        handle: row.try_get("name")?,
        display_name: row.try_get("display_name")?,
        email: row.try_get("email")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
        // A freshly registered user never has an avatar; the write does not
        // return the column and the row's key is always empty at this point.
        avatar: None,
    })
}

/// Decode a SQLite rejection of a `users` write into one of the three uniqueness
/// rules, or `None` for every other failure (which must pass through untouched).
/// Mirrors the product's `sqlite-user-conflicts.classify`, in the same priority
/// order. The `users` schema has no column whose name extends `id`/`name`/`email`,
/// so a prefix match is equivalent to the TS word-boundary anchors.
fn classify_users_conflict(error: &sqlx::Error) -> Option<UsersConflict> {
    let sqlx::Error::Database(db) = error else {
        return None;
    };
    let message = db.message();
    if message.contains("UNIQUE constraint failed: users.id") {
        return Some(UsersConflict::UserId);
    }
    if message.contains("users.name collides with an existing agent name") {
        return Some(UsersConflict::Name);
    }
    if message.contains("UNIQUE constraint failed: users.name") {
        return Some(UsersConflict::Name);
    }
    if message.contains("UNIQUE constraint failed: users.email") {
        return Some(UsersConflict::Email);
    }
    if message.contains("uniq_users_email_when_set") {
        return Some(UsersConflict::Email);
    }
    None
}

impl<E> HumanRegistrationUnitOfWork for SqliteHumanRegistration<E>
where
    E: EntropySource + Clone + 'static,
{
    async fn register(
        &self,
        handle: &ParticipantHandle,
        display_name: &DisplayName,
        email: &EmailAddress,
        credential: &PreparedPasswordCredential,
        now_ms: i64,
    ) -> Result<HumanRegistration, PersistenceFailure> {
        self.register_inner(handle, display_name, email, credential, now_ms)
            .await
            .map_err(PersistenceFailure::from_source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use silverhand_backend_core::identity_access::credential::{
        Argon2idCost, PasswordHashParams, RustCalibration,
    };
    use sqlx::sqlite::SqliteConnectOptions;
    use sqlx::{Connection, Executor, SqliteConnection};
    use std::sync::Mutex;

    const CURRENT_SCHEMA: &str =
        include_str!("../../../../../rust/test-fixtures/migration-baseline/schema-current.sql");

    // A user `carol` holding the email `carol@example.com`, a host + agent `bender`
    // occupying that handle, and a user occupying the id the first entropy draw
    // produces (to force the retry path). The agent occupies `bender` so a signup
    // for that handle is a HandleConflict even though no USER holds it.
    const SEED_SQL: &str = "\
INSERT INTO users (id, name, display_name, email, created_at, updated_at) \
  VALUES ('user-carol', 'carol', 'Carol', 'carol@example.com', 1, 1); \
INSERT INTO hosts (id, name, kind, created_at) VALUES ('host-1', 'Host', 'local', 1); \
INSERT INTO agents (id, name, display_name, runtime, host_id, created_by_user_id, created_at, updated_at) \
  VALUES ('agent-1', 'bender', 'Bender', 'claude', 'host-1', 'user-carol', 1, 1);";

    /// A deterministic entropy source dispensing a scripted queue of bytes so the
    /// allocated ids are exact and reproducible.
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
        let path = directory.path().join("registration.sqlite");
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

    fn credential() -> PreparedPasswordCredential {
        PreparedPasswordCredential {
            phc: "$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA".to_owned(),
            params: PasswordHashParams {
                cost: Argon2idCost {
                    memory_cost_kib: 19 * 1024,
                    time_cost: 2,
                    parallelism: 1,
                    output_len: 32,
                },
                calibration: RustCalibration {
                    target_millis: 250,
                    measured_millis: 262,
                    candidate_count: 3,
                    floor_memory_cost_kib: 19 * 1024,
                    floor_time_cost: 2,
                },
            },
        }
    }

    fn handle(raw: &str) -> ParticipantHandle {
        ParticipantHandle::parse(raw).unwrap()
    }
    fn display(raw: &str) -> DisplayName {
        DisplayName::parse(raw).unwrap()
    }
    fn email(raw: &str) -> EmailAddress {
        EmailAddress::parse(raw).unwrap()
    }

    #[tokio::test]
    async fn a_fresh_identity_returns_the_user_and_commits_its_credential() {
        let (_dir, path) = seed_db().await;
        // Eight bytes -> id "0001020304050607".
        let entropy = ScriptedEntropy::new(vec![0, 1, 2, 3, 4, 5, 6, 7]);
        let store = SqliteHumanRegistration::open_with_entropy(&path, entropy)
            .await
            .unwrap();
        let outcome = store
            .register(
                &handle("dave"),
                &display("Dave"),
                &email("dave@example.com"),
                &credential(),
                500,
            )
            .await
            .unwrap();
        let HumanRegistration::Registered { user } = outcome else {
            panic!("expected Registered, got {outcome:?}");
        };
        assert_eq!(user.id.as_str(), "0001020304050607");
        assert_eq!(user.handle, "dave");
        assert_eq!(user.display_name, "Dave");
        assert_eq!(user.email.as_deref(), Some("dave@example.com"));
        assert_eq!(user.created_at, 500);
        assert_eq!(user.updated_at, 500);

        // The credential row committed in the SAME transaction, carrying the Rust
        // producer arm of password_hash_params.
        let reopen = SqliteConnectOptions::new()
            .filename(&path)
            .foreign_keys(true);
        let mut db = SqliteConnection::connect_with(&reopen).await.unwrap();
        let (phc, params, updated_at): (String, String, i64) = sqlx::query_as(
            "SELECT password_hash, password_hash_params, password_updated_at FROM user_password_credentials WHERE user_id = ?",
        )
        .bind("0001020304050607")
        .fetch_one(&mut db)
        .await
        .unwrap();
        assert_eq!(phc, credential().phc);
        assert_eq!(updated_at, 500);
        assert!(params.contains("silverhand/argon2-rustcrypto"));
    }

    #[tokio::test]
    async fn a_handle_held_by_an_agent_is_a_handle_conflict_without_a_credential() {
        let (_dir, path) = seed_db().await;
        let entropy = ScriptedEntropy::new(vec![10, 11, 12, 13, 14, 15, 16, 17]);
        let store = SqliteHumanRegistration::open_with_entropy(&path, entropy)
            .await
            .unwrap();
        let outcome = store
            .register(
                &handle("bender"),
                &display("Imposter"),
                &email("new@example.com"),
                &credential(),
                500,
            )
            .await
            .unwrap();
        assert!(matches!(outcome, HumanRegistration::HandleConflict));

        // Nothing was written: the credential table has no row for the failed id.
        let reopen = SqliteConnectOptions::new()
            .filename(&path)
            .foreign_keys(true);
        let mut db = SqliteConnection::connect_with(&reopen).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM user_password_credentials")
            .fetch_one(&mut db)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn a_taken_email_with_a_free_handle_is_an_email_conflict() {
        let (_dir, path) = seed_db().await;
        let entropy = ScriptedEntropy::new(vec![20, 21, 22, 23, 24, 25, 26, 27]);
        let store = SqliteHumanRegistration::open_with_entropy(&path, entropy)
            .await
            .unwrap();
        let outcome = store
            .register(
                &handle("newhandle"),
                &display("New"),
                &email("carol@example.com"),
                &credential(),
                500,
            )
            .await
            .unwrap();
        assert!(matches!(outcome, HumanRegistration::EmailConflict));
    }

    #[tokio::test]
    async fn a_user_id_collision_retries_on_a_fresh_id_then_commits() {
        let (_dir, path) = seed_db().await;
        // Pre-occupy the id the FIRST draw produces, so attempt #1 trips users.id
        // and attempt #2 (a fresh draw) succeeds.
        let reopen = SqliteConnectOptions::new()
            .filename(&path)
            .foreign_keys(true);
        let mut db = SqliteConnection::connect_with(&reopen).await.unwrap();
        db.execute(sqlx::raw_sql(
            "INSERT INTO users (id, name, display_name, created_at, updated_at) \
               VALUES ('3030303030303030', 'occupier', 'Occupier', 1, 1);",
        ))
        .await
        .unwrap();
        db.close().await.unwrap();

        // First draw -> all 0x30 -> "3030303030303030" (collides); second draw ->
        // 0x31.. -> a fresh id that commits.
        let entropy = ScriptedEntropy::new(vec![
            0x30, 0x30, 0x30, 0x30, 0x30, 0x30, 0x30, 0x30, // attempt 1 (collides)
            0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, // attempt 2 (fresh)
        ]);
        let store = SqliteHumanRegistration::open_with_entropy(&path, entropy)
            .await
            .unwrap();
        let outcome = store
            .register(
                &handle("erin"),
                &display("Erin"),
                &email("erin@example.com"),
                &credential(),
                500,
            )
            .await
            .unwrap();
        let HumanRegistration::Registered { user } = outcome else {
            panic!("expected Registered after retry");
        };
        assert_eq!(user.id.as_str(), "3132333435363738");
    }
}
