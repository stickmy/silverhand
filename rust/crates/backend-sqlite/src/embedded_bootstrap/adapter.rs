//! SQLite adapter for the [`EmbeddedBootstrapUnitOfWork`] application-seam port —
//! the atomic single-viewer choose-or-create write.
//!
//! One attempt is one `BEGIN IMMEDIATE` transaction that reads the viewer count
//! under the write lock and, in that SAME transaction, either creates the
//! profile-only local viewer and backfills its ownership, selects the sole
//! existing viewer, or refuses an ambiguous multi-viewer database. The count and
//! the create can therefore never race apart: a second concurrent bootstrap blocks
//! on the write lock and, once it runs, observes the committed viewer and selects.
//! (This is stricter than the TS handler's `users.list()`-then-`register`, whose
//! read and write are two statements; the port contract mandates the single unit.)
//!
//! The surrogate `users.id` is allocated here (the product's `randomHex(8)`) and a
//! `users.id` PK collision is retried on a fresh id up to the product's twelve
//! attempts; exhausting them is an infrastructure fault. Every OTHER `users`-write
//! rejection — including a `users.name` collision on the fixed `local` handle —
//! passes through as a [`PersistenceFailure`] (a safe-500), never laundered into a
//! product answer: an ambiguous database is the sole product refusal, and it is
//! decided by the count, not by a driver error.

use std::{path::Path, time::Duration};

use silverhand_backend_core::{
    conversation_messaging::messaging::AvatarRevision,
    embedded_bootstrap::{BootstrapChoice, EmbeddedBootstrapUnitOfWork},
    foundation::identity::UserId,
    foundation::persistence::PersistenceFailure,
    host_runtime::environment::{EntropySource, OsEntropy},
    identity_access::user_identity::UserRecord,
};
use sqlx::{Row, SqlitePool};
use thiserror::Error;

const ROOT_WRITE_BEGIN: &str = "BEGIN IMMEDIATE";
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// The product's `randomHex(8)`: eight entropy bytes → sixteen lowercase hex digits.
const USER_ID_BYTES: usize = 8;
/// The product's id-allocation retry ceiling (`attempt < 12`).
const ID_ALLOCATION_ATTEMPTS: usize = 12;

/// The fixed identity of the embedded local viewer (`routes.ts`: `register({ name:
/// 'local', display_name: 'Local User' })`). Profile-only: no email, no credential.
const LOCAL_HANDLE: &str = "local";
const LOCAL_DISPLAY_NAME: &str = "Local User";

const COUNT_USERS: &str = "SELECT COUNT(*) FROM users";
const SELECT_SOLE_USER: &str = "\
SELECT id, name, display_name, email, avatar_storage_key, created_at, updated_at \
 FROM users LIMIT 1";
const INSERT_LOCAL_USER: &str = "\
INSERT INTO users (id, name, display_name, avatar_storage_key, avatar_content_type, email, created_at, updated_at, embedded_profile_pending) \
VALUES (?, ?, ?, NULL, NULL, NULL, ?, ?, 1) \
RETURNING id, name, display_name, email, avatar_storage_key, created_at, updated_at";
/// Owner membership for every pre-existing workspace, expressed set-based rather
/// than transliterating the TS per-id loop: the contract is "an owner row for every
/// workspace", and `INSERT OR IGNORE … SELECT` states exactly that in one statement.
/// `SELECT id FROM workspaces` spans every kind (not the UI read model), so no
/// pre-existing workspace can be silently left ungoverned.
const BACKFILL_WORKSPACE_OWNER: &str = "\
INSERT OR IGNORE INTO workspace_users (workspace_id, user_id, joined_at, role) \
SELECT id, ?, ?, 'owner' FROM workspaces";
/// Stamp the denormalized owner pointer on every still-unowned workspace. The
/// `owner_user_id IS NULL` guard provably cannot demote an already-owned workspace.
const CLAIM_UNOWNED_WORKSPACES: &str =
    "UPDATE workspaces SET owner_user_id = ? WHERE owner_user_id IS NULL";
/// Membership in every pre-existing conversation, same set-based form.
const BACKFILL_CONVERSATION_MEMBER: &str = "\
INSERT OR IGNORE INTO conversation_users (conversation_id, user_id, joined_at) \
SELECT id, ?, ? FROM conversations";

/// A pooled connection to an existing product database offering the atomic
/// choose-or-create bootstrap write. Owns an [`EntropySource`] for surrogate id
/// allocation; holds no clock (the write stamps `now_ms`, passed in by the flow).
#[derive(Clone)]
pub struct SqliteEmbeddedBootstrap<E = OsEntropy> {
    pool: SqlitePool,
    entropy: E,
}

/// Startup/driver failures surfaced only to the composition root that opens the
/// store; the port method hands operational failures over as a `PersistenceFailure`.
#[derive(Debug, Error)]
pub enum SqliteEmbeddedBootstrapError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
}

/// Why one atomic attempt could not yield a trustworthy answer. No arm is a product
/// outcome (those are the [`BootstrapChoice`] arms); every arm reaches the caller as
/// the source of one [`PersistenceFailure`] (a safe-500).
#[derive(Debug, Error)]
enum BootstrapFailure {
    #[error(transparent)]
    Sql(#[from] sqlx::Error),
    #[error("secure random source failed")]
    Entropy(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("a committed users row holds an unusable value")]
    CorruptStoredUser,
    #[error("could not allocate a unique user id")]
    IdAllocationExhausted,
}

/// The outcome of one atomic attempt. `RetryNewId` is internal — a `users.id`
/// allocation collision on the create path, retried on a fresh id.
enum Attempt {
    Created(UserRecord),
    Selected(UserRecord),
    Conflict,
    RetryNewId,
}

impl<E> SqliteEmbeddedBootstrap<E>
where
    E: EntropySource,
{
    async fn attempt(&self, now_ms: i64) -> Result<Attempt, BootstrapFailure> {
        let mut tx = self.pool.begin_with(ROOT_WRITE_BEGIN).await?;

        // The count is read WHILE the write lock is held — this is the whole
        // single-viewer gate. A concurrent bootstrap either has not yet acquired the
        // lock (and will see our committed row) or committed before us (and we see
        // theirs); neither can both create.
        let count: i64 = sqlx::query_scalar(COUNT_USERS).fetch_one(&mut *tx).await?;

        if count >= 2 {
            // Ambiguous: refuse, writing nothing. Drop rolls the (read-only) tx back.
            // No id is allocated, so an unrelated entropy fault can never turn this
            // (or the Selected path below) into a 500.
            return Ok(Attempt::Conflict);
        }

        if count == 1 {
            let row = sqlx::query(SELECT_SOLE_USER).fetch_one(&mut *tx).await?;
            let user = decode_user(&row)?;
            // A pure read; commit is a no-op but keeps the lifecycle explicit.
            tx.commit().await?;
            return Ok(Attempt::Selected(user));
        }

        // count == 0: only NOW, having decided a create is required and still holding
        // the same write-lock snapshot, do we draw entropy for the surrogate id. This
        // keeps id allocation strictly on the create path — a Selected/Conflict answer
        // is entropy-independent.
        let id = self.allocate_id()?;
        let user_row = match sqlx::query(INSERT_LOCAL_USER)
            .bind(&id)
            .bind(LOCAL_HANDLE)
            .bind(LOCAL_DISPLAY_NAME)
            .bind(now_ms)
            .bind(now_ms)
            .fetch_one(&mut *tx)
            .await
        {
            Ok(row) => row,
            Err(error) => {
                // Only an internal id-PK collision is retried; every other rejection
                // (e.g. the fixed `local` handle colliding with an agent name) is a
                // genuine fault the caller must see as a safe-500, never a product
                // answer — an ambiguous DB is decided by the count, not a driver error.
                drop(tx);
                if is_user_id_conflict(&error) {
                    return Ok(Attempt::RetryNewId);
                }
                return Err(BootstrapFailure::Sql(error));
            }
        };
        let user = decode_user(&user_row)?;

        // Backfill the first viewer as owner of every pre-existing workspace and
        // member of every pre-existing conversation, all in THIS transaction.
        sqlx::query(BACKFILL_WORKSPACE_OWNER)
            .bind(user.id.as_str())
            .bind(now_ms)
            .execute(&mut *tx)
            .await?;
        sqlx::query(CLAIM_UNOWNED_WORKSPACES)
            .bind(user.id.as_str())
            .execute(&mut *tx)
            .await?;
        sqlx::query(BACKFILL_CONVERSATION_MEMBER)
            .bind(user.id.as_str())
            .bind(now_ms)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        Ok(Attempt::Created(user))
    }

    fn allocate_id(&self) -> Result<String, BootstrapFailure> {
        let mut bytes = [0u8; USER_ID_BYTES];
        self.entropy
            .fill(&mut bytes)
            .map_err(|error| BootstrapFailure::Entropy(Box::new(error)))?;
        Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
    }

    async fn choose_or_create_inner(
        &self,
        now_ms: i64,
    ) -> Result<BootstrapChoice, BootstrapFailure> {
        for _ in 0..ID_ALLOCATION_ATTEMPTS {
            // Each attempt allocates its own fresh id INSIDE the create branch, so a
            // Selected or Conflict attempt short-circuits without ever drawing entropy;
            // only a `users.id` PK collision on the create path loops for a new id.
            match self.attempt(now_ms).await? {
                Attempt::Created(user) => return Ok(BootstrapChoice::Created { user }),
                Attempt::Selected(user) => return Ok(BootstrapChoice::Selected { user }),
                Attempt::Conflict => return Ok(BootstrapChoice::Conflict),
                Attempt::RetryNewId => continue,
            }
        }
        Err(BootstrapFailure::IdAllocationExhausted)
    }
}

impl SqliteEmbeddedBootstrap<OsEntropy> {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, SqliteEmbeddedBootstrapError> {
        Self::open_with_entropy(path, OsEntropy).await
    }
}

impl<E> SqliteEmbeddedBootstrap<E>
where
    E: EntropySource,
{
    /// Open with an injected entropy source — tests use a deterministic one so the
    /// allocated ids (and thus the collision/retry path) are reproducible.
    pub async fn open_with_entropy(
        path: impl AsRef<Path>,
        entropy: E,
    ) -> Result<Self, SqliteEmbeddedBootstrapError> {
        let pool = crate::foundation::database::store_pool(path.as_ref(), BUSY_TIMEOUT).await?;
        Ok(Self { pool, entropy })
    }

    /// Construct over an already-open pool with an injected entropy source.
    pub fn from_pool_with_entropy(pool: SqlitePool, entropy: E) -> Self {
        Self { pool, entropy }
    }
}

/// Decode a `users` row (an INSERT RETURNING on the create path, or the sole-user
/// SELECT) into the projected [`UserRecord`]. The id is parsed through the domain's
/// fallible parser and the avatar is folded exactly as the user-identity read does;
/// a malformed committed row is corruption (a safe-500), never a product answer.
fn decode_user(row: &sqlx::sqlite::SqliteRow) -> Result<UserRecord, BootstrapFailure> {
    let id = UserId::parse(
        row.try_get::<String, _>("id")
            .map_err(|_| BootstrapFailure::CorruptStoredUser)?,
    )
    .map_err(|_| BootstrapFailure::CorruptStoredUser)?;
    let updated_at: i64 = row
        .try_get("updated_at")
        .map_err(|_| BootstrapFailure::CorruptStoredUser)?;
    let avatar_storage_key: Option<String> = row
        .try_get("avatar_storage_key")
        .map_err(|_| BootstrapFailure::CorruptStoredUser)?;
    let avatar = match avatar_storage_key.as_deref() {
        None | Some("") => None,
        Some(_) => Some(AvatarRevision::at(updated_at)),
    };
    Ok(UserRecord {
        id,
        handle: row
            .try_get("name")
            .map_err(|_| BootstrapFailure::CorruptStoredUser)?,
        display_name: row
            .try_get("display_name")
            .map_err(|_| BootstrapFailure::CorruptStoredUser)?,
        email: row
            .try_get("email")
            .map_err(|_| BootstrapFailure::CorruptStoredUser)?,
        avatar,
        created_at: row
            .try_get("created_at")
            .map_err(|_| BootstrapFailure::CorruptStoredUser)?,
        updated_at,
    })
}

/// Whether a rejection of the `users` insert is the internal `users.id` PK
/// collision (the only rule this write retries). Every other rejection is a fault.
fn is_user_id_conflict(error: &sqlx::Error) -> bool {
    let sqlx::Error::Database(db) = error else {
        return false;
    };
    db.message().contains("UNIQUE constraint failed: users.id")
}

impl<E> EmbeddedBootstrapUnitOfWork for SqliteEmbeddedBootstrap<E>
where
    E: EntropySource + Clone + 'static,
{
    async fn choose_or_create(&self, now_ms: i64) -> Result<BootstrapChoice, PersistenceFailure> {
        self.choose_or_create_inner(now_ms)
            .await
            .map_err(PersistenceFailure::from_source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::{
        SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous,
    };
    use sqlx::{Connection, Executor, SqliteConnection};
    use std::sync::Mutex;

    const CURRENT_SCHEMA: &str =
        include_str!("../../../../../rust/test-fixtures/migration-baseline/schema-current.sql");

    /// A host, one regular workspace (unowned), and two conversations in it — so
    /// the backfill has real workspace + conversation rows to claim.
    const SEED_TOPOLOGY: &str = "\
INSERT INTO hosts (id, name, kind, created_at) VALUES ('host-1', 'Host', 'local', 1); \
INSERT INTO workspaces (id, name, name_key, created_at, updated_at) \
  VALUES ('ws-regular', 'Team', 'team', 1, 1); \
INSERT INTO conversations \
  (id, workspace_id, placement_kind, roster_policy, title, title_key, status, created_at, updated_at) \
  VALUES ('conv-1', 'ws-regular', 'workspace', 'open', 'general', 'general', 'active', 1, 1); \
INSERT INTO conversations \
  (id, workspace_id, placement_kind, roster_policy, title, title_key, status, created_at, updated_at) \
  VALUES ('conv-2', 'ws-regular', 'workspace', 'open', 'random', 'random', 'active', 1, 1);";

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

    async fn seed_db(extra: &'static str) -> (tempfile::TempDir, std::path::PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("bootstrap.sqlite");
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .foreign_keys(true);
        let mut db = SqliteConnection::connect_with(&options).await.unwrap();
        db.execute(sqlx::raw_sql(CURRENT_SCHEMA)).await.unwrap();
        db.execute("ALTER TABLE users ADD COLUMN embedded_profile_pending INTEGER NOT NULL DEFAULT 0 CHECK (embedded_profile_pending IN (0, 1))").await.unwrap();
        db.execute(sqlx::raw_sql(SEED_TOPOLOGY)).await.unwrap();
        if !extra.is_empty() {
            db.execute(sqlx::raw_sql(extra)).await.unwrap();
        }
        db.close().await.unwrap();
        (directory, path)
    }

    async fn reopen(path: &std::path::Path) -> SqliteConnection {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .foreign_keys(true);
        SqliteConnection::connect_with(&options).await.unwrap()
    }

    #[tokio::test]
    async fn failed_bootstrap_does_not_leave_a_user_without_setup_state() {
        let (_dir, path) = seed_db("CREATE TRIGGER fail_owner BEFORE UPDATE OF owner_user_id ON workspaces BEGIN SELECT RAISE(ABORT, 'isolated owner failure'); END;").await;
        let store = SqliteEmbeddedBootstrap::open(&path).await.unwrap();
        assert!(store.choose_or_create(500).await.is_err());
        let mut db = reopen(&path).await;
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
            .fetch_one(&mut db)
            .await
            .unwrap();
        assert_eq!(count, 0);
        sqlx::query("DROP TRIGGER fail_owner")
            .execute(&mut db)
            .await
            .unwrap();
        let BootstrapChoice::Created { user } = store.choose_or_create(600).await.unwrap() else {
            panic!("retry creates the identity")
        };
        let pending: bool =
            sqlx::query_scalar("SELECT embedded_profile_pending FROM users WHERE id = ?")
                .bind(user.id.as_str())
                .fetch_one(&mut db)
                .await
                .unwrap();
        assert!(pending);
    }

    #[tokio::test]
    async fn zero_users_creates_the_local_viewer_and_backfills_ownership() {
        let (_dir, path) = seed_db("").await;
        let entropy = ScriptedEntropy::new(vec![0, 1, 2, 3, 4, 5, 6, 7]);
        let store = SqliteEmbeddedBootstrap::open_with_entropy(&path, entropy)
            .await
            .unwrap();

        let choice = store.choose_or_create(500).await.unwrap();
        let BootstrapChoice::Created { user } = choice else {
            panic!("expected Created, got {choice:?}");
        };
        assert_eq!(user.id.as_str(), "0001020304050607");
        assert_eq!(user.handle, "local");
        assert_eq!(user.display_name, "Local User");
        assert_eq!(user.email, None);
        assert!(user.avatar.is_none());
        assert_eq!((user.created_at, user.updated_at), (500, 500));

        let mut db = reopen(&path).await;
        let pending: bool =
            sqlx::query_scalar("SELECT embedded_profile_pending FROM users WHERE id = ?")
                .bind(user.id.as_str())
                .fetch_one(&mut db)
                .await
                .unwrap();
        assert!(pending);
        // Owner MEMBER row on the workspace, with role 'owner'.
        let role: String = sqlx::query_scalar(
            "SELECT role FROM workspace_users WHERE workspace_id = 'ws-regular' AND user_id = ?",
        )
        .bind(user.id.as_str())
        .fetch_one(&mut db)
        .await
        .unwrap();
        assert_eq!(role, "owner");
        // Denormalized owner pointer claimed.
        let owner: Option<String> =
            sqlx::query_scalar("SELECT owner_user_id FROM workspaces WHERE id = 'ws-regular'")
                .fetch_one(&mut db)
                .await
                .unwrap();
        assert_eq!(owner.as_deref(), Some(user.id.as_str()));
        // Member of every conversation.
        let conv_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM conversation_users WHERE user_id = ?")
                .bind(user.id.as_str())
                .fetch_one(&mut db)
                .await
                .unwrap();
        assert_eq!(conv_count, 2);
    }

    #[tokio::test]
    async fn one_user_selects_it_and_writes_nothing() {
        let (_dir, path) = seed_db(
            "INSERT INTO users (id, name, display_name, email, created_at, updated_at) \
               VALUES ('solo', 'solo', 'Solo', 'solo@example.com', 1, 1);",
        )
        .await;
        // Selection must not draw entropy even if the source would fail immediately.
        let entropy = ScriptedEntropy::new(vec![]);
        let store = SqliteEmbeddedBootstrap::open_with_entropy(&path, entropy)
            .await
            .unwrap();

        let choice = store.choose_or_create(500).await.unwrap();
        let BootstrapChoice::Selected { user } = choice else {
            panic!("expected Selected, got {choice:?}");
        };
        assert_eq!(user.id.as_str(), "solo");
        assert_eq!(user.email.as_deref(), Some("solo@example.com"));

        // No membership/ownership was written by the select.
        let mut db = reopen(&path).await;
        let backfills: (i64, i64, i64) = sqlx::query_as(
            "SELECT (SELECT COUNT(*) FROM workspace_users),
                    (SELECT COUNT(*) FROM workspaces WHERE owner_user_id IS NOT NULL),
                    (SELECT COUNT(*) FROM conversation_users)",
        )
        .fetch_one(&mut db)
        .await
        .unwrap();
        assert_eq!(backfills, (0, 0, 0));
    }

    #[tokio::test]
    async fn more_than_one_user_is_a_conflict_and_writes_nothing() {
        let (_dir, path) = seed_db(
            "INSERT INTO users (id, name, display_name, created_at, updated_at) \
               VALUES ('a', 'aa', 'A', 1, 1); \
             INSERT INTO users (id, name, display_name, created_at, updated_at) \
               VALUES ('b', 'bb', 'B', 1, 1);",
        )
        .await;
        // Ambiguity is a product refusal independent of entropy availability.
        let entropy = ScriptedEntropy::new(vec![]);
        let store = SqliteEmbeddedBootstrap::open_with_entropy(&path, entropy)
            .await
            .unwrap();

        let choice = store.choose_or_create(500).await.unwrap();
        assert!(matches!(choice, BootstrapChoice::Conflict));
        let mut db = reopen(&path).await;
        let backfills: (i64, i64, i64) = sqlx::query_as(
            "SELECT (SELECT COUNT(*) FROM workspace_users),
                    (SELECT COUNT(*) FROM workspaces WHERE owner_user_id IS NOT NULL),
                    (SELECT COUNT(*) FROM conversation_users)",
        )
        .fetch_one(&mut db)
        .await
        .unwrap();
        assert_eq!(backfills, (0, 0, 0));
    }

    #[tokio::test]
    async fn concurrent_bootstraps_create_exactly_one_and_select_the_other() {
        // Two bootstraps race on the SAME pool. BEGIN IMMEDIATE serializes them:
        // exactly one commits the create, the other blocks then observes the
        // committed viewer and selects it. Never two users.
        let (_dir, path) = seed_db("").await;
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&path)
                    .foreign_keys(true)
                    .journal_mode(SqliteJournalMode::Wal)
                    .synchronous(SqliteSynchronous::Full)
                    .busy_timeout(Duration::from_secs(5)),
            )
            .await
            .unwrap();
        let a = SqliteEmbeddedBootstrap::from_pool_with_entropy(
            pool.clone(),
            ScriptedEntropy::new(vec![0xaa; 32]),
        );
        let b = SqliteEmbeddedBootstrap::from_pool_with_entropy(
            pool.clone(),
            ScriptedEntropy::new(vec![0xbb; 32]),
        );

        let (ra, rb) = tokio::join!(a.choose_or_create(500), b.choose_or_create(500));
        let ra = ra.unwrap();
        let rb = rb.unwrap();

        let created = [&ra, &rb]
            .iter()
            .filter(|c| matches!(c, BootstrapChoice::Created { .. }))
            .count();
        let selected = [&ra, &rb]
            .iter()
            .filter(|c| matches!(c, BootstrapChoice::Selected { .. }))
            .count();
        assert_eq!(created, 1, "exactly one create");
        assert_eq!(selected, 1, "the loser selects");

        let mut db = reopen(&path).await;
        let user_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
            .fetch_one(&mut db)
            .await
            .unwrap();
        assert_eq!(user_count, 1, "never two users");
    }

    #[tokio::test]
    async fn only_the_zero_user_create_surfaces_an_entropy_fault() {
        // The create path is the ONLY one that draws entropy; an empty source there is
        // a genuine infrastructure fault (a safe-500), not a product answer.
        let (_dir, path) = seed_db("").await;
        let store = SqliteEmbeddedBootstrap::open_with_entropy(&path, ScriptedEntropy::new(vec![]))
            .await
            .unwrap();

        let error = store
            .choose_or_create(500)
            .await
            .expect_err("0-user create must fail closed when entropy fails");
        // No user was created despite the failed draw.
        let mut db = reopen(&path).await;
        let user_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
            .fetch_one(&mut db)
            .await
            .unwrap();
        assert_eq!(user_count, 0, "a failed create writes nothing");
        // It surfaces as a persistence failure, not a product refusal.
        let _ = error;
    }
}
