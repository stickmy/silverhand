//! SQLite adapter for the password-credential context's `user_password_credentials`
//! authority.
//!
//! Two responsibilities meet here, both persistence-representation concerns the
//! core deliberately does not own:
//!
//! * [`SqliteCredentialStore`] answers the login lookup ([`LoginCredentialLookup`]):
//!   an email resolves to `(user_id, password_hash?)`. It is a pure consumer of an
//!   existing product database.
//! * [`serialize_password_hash_params`] is the write half of the persisted
//!   `password_hash_params` codec. Core produces only the Rust provenance arm, so
//!   the serializer emits exactly one shape — a discriminated member of the SAME
//!   backward-compatible producer union the TS `parsePasswordHashParams` reads
//!   (discriminated by `library`). The camelCase field names and the shared cost
//!   fields (`memoryCostKiB`/`timeCost`/`parallelism`/`outputLen`, which the TS
//!   `Argon2idCostCandidate` base declares) are the wire contract both producers
//!   speak; a checked-in fixture (`tests/fixtures/password-hash-params-rust.json`)
//!   pins the exact bytes so the two implementations cannot drift.

use std::{path::Path, time::Duration};

use serde::Serialize;
use silverhand_backend_core::{
    foundation::identity::UserId,
    foundation::persistence::PersistenceFailure,
    identity_access::credential::{
        LoginCandidate, LoginCredentialLookup, PasswordHashParams, PendingRecoveryCode,
        RECOVERY_CODE_MAX_ATTEMPTS, RECOVERY_CODE_TTL_MS, RECOVERY_RESEND_INTERVAL_MS,
        RECOVERY_WINDOW_MAX_ATTEMPTS, RECOVERY_WINDOW_MAX_SENDS, RECOVERY_WINDOW_MS, RUST_PRODUCER,
    },
};
use sqlx::{Row, SqlitePool};
use thiserror::Error;

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// The provenance algorithm tag, spelled the way it is persisted.
const ALGORITHM: &str = "argon2id";
/// The Argon2 version tag, matching the core `ARGON2ID_VERSION`.
const VERSION: &str = "0x13";

const LOOKUP_LOGIN: &str = "\
SELECT u.id AS user_id, c.password_hash AS password_hash \
  FROM users u \
  LEFT JOIN user_password_credentials c ON c.user_id = u.id \
 WHERE u.email = ? COLLATE NOCASE";

/// A pooled connection to an existing product database offering the login-lookup
/// read. Holds no clock: the login flow spends no write here.
#[derive(Clone)]
pub struct SqliteCredentialStore {
    pool: SqlitePool,
}

/// Startup/driver failures surfaced only to the composition root that opens the
/// store; the port method hands the driver error over as a `PersistenceFailure`.
#[derive(Debug, Error)]
pub enum SqliteCredentialStoreError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
}

/// A committed `users.id` that decodes to a value the domain refuses (the empty
/// string). The product schema's ids are non-empty by construction, so this is a
/// persistence-integrity fault, never coerced to a sentinel.
#[derive(Debug, Error)]
#[error("users.id holds an unusable identifier")]
struct CorruptStoredUserId;

impl SqliteCredentialStore {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, SqliteCredentialStoreError> {
        let pool = crate::foundation::database::store_pool(path.as_ref(), BUSY_TIMEOUT).await?;
        Ok(Self { pool })
    }

    /// Construct over an already-open pool so a composition root that shares one
    /// pool across adapters need not reopen the file.
    pub fn from_pool(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

impl LoginCredentialLookup for SqliteCredentialStore {
    async fn lookup_login(
        &self,
        email: &str,
    ) -> Result<Option<LoginCandidate>, PersistenceFailure> {
        let row = sqlx::query(LOOKUP_LOGIN)
            .bind(email)
            .fetch_optional(&self.pool)
            .await
            .map_err(PersistenceFailure::from_source)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let raw_user_id: String = row
            .try_get("user_id")
            .map_err(PersistenceFailure::from_source)?;
        let user_id = UserId::parse(raw_user_id)
            .map_err(|_| PersistenceFailure::from_source(CorruptStoredUserId))?;
        let password_hash: Option<String> = row
            .try_get("password_hash")
            .map_err(PersistenceFailure::from_source)?;
        Ok(Some(LoginCandidate {
            user_id,
            password_hash,
        }))
    }
}

// ===========================================================================
// The persisted `password_hash_params` codec — Rust producer arm.
// ===========================================================================

/// The persisted shape of the Rust producer arm. Field order and camelCase names
/// are the wire contract shared with the TS reader; the cost knobs sit at the top
/// level (the shared `Argon2idCostCandidate` base), and the honest Rust calibration
/// lives under `calibration` — never the Node-only `benchmark` fields.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RustParamsWire<'a> {
    algorithm: &'a str,
    library: &'a str,
    version: &'a str,
    // serde's camelCase lowercases the trailing "B" of "KiB"; the wire contract
    // (the TS `Argon2idCostCandidate`) spells it `memoryCostKiB`, so pin it.
    #[serde(rename = "memoryCostKiB")]
    memory_cost_kib: u32,
    time_cost: u32,
    parallelism: u32,
    output_len: u32,
    calibration: CalibrationWire,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CalibrationWire {
    target_ms: u64,
    measured_ms: u64,
    candidate_count: u32,
    #[serde(rename = "floorMemoryCostKiB")]
    floor_memory_cost_kib: u32,
    floor_time_cost: u32,
}

/// Serialize the structured Rust provenance into the `password_hash_params` JSON
/// column. The output is a member of the producer union discriminated by
/// `library == "silverhand/argon2-rustcrypto"`; the TS `parsePasswordHashParams`
/// admits it alongside the legacy `@node-rs/argon2` arm.
pub fn serialize_password_hash_params(params: &PasswordHashParams) -> String {
    let wire = RustParamsWire {
        algorithm: ALGORITHM,
        library: RUST_PRODUCER,
        version: VERSION,
        memory_cost_kib: params.cost.memory_cost_kib,
        time_cost: params.cost.time_cost,
        parallelism: params.cost.parallelism,
        output_len: params.cost.output_len,
        calibration: CalibrationWire {
            target_ms: params.calibration.target_millis,
            measured_ms: params.calibration.measured_millis,
            candidate_count: params.calibration.candidate_count,
            floor_memory_cost_kib: params.calibration.floor_memory_cost_kib,
            floor_time_cost: params.calibration.floor_time_cost,
        },
    };
    // The struct is total and every field a plain scalar, so serialization cannot
    // fail; a change that could fail must revisit this expectation deliberately.
    serde_json::to_string(&wire).expect("serializing password_hash_params cannot fail")
}

impl silverhand_backend_core::identity_access::credential::PasswordChangeStore
    for SqliteCredentialStore
{
    async fn password_for_user(&self, user: &UserId) -> Result<Option<String>, PersistenceFailure> {
        sqlx::query_scalar("SELECT password_hash FROM user_password_credentials WHERE user_id = ?")
            .bind(user.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(PersistenceFailure::from_source)
    }

    async fn replace_password(
        &self,
        user: &UserId,
        session: &silverhand_backend_core::identity_access::auth::SessionTokenHash,
        expected: &str,
        replacement: silverhand_backend_core::identity_access::credential::PreparedPasswordCredential,
        now_ms: i64,
    ) -> Result<bool, PersistenceFailure> {
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(PersistenceFailure::from_source)?;
        let changed = sqlx::query("UPDATE user_password_credentials
            SET password_hash = ?, password_hash_params = ?, password_updated_at = ?
            WHERE user_id = ? AND password_hash = ?
            AND EXISTS (SELECT 1 FROM auth_sessions WHERE user_id = ? AND token_hash = ? AND revoked_at IS NULL)")
            .bind(replacement.phc)
            .bind(serialize_password_hash_params(&replacement.params))
            .bind(now_ms)
            .bind(user.as_str())
            .bind(expected)
            .bind(user.as_str())
            .bind(session.as_str())
            .execute(&mut *tx)
            .await
            .map_err(PersistenceFailure::from_source)?;

        if changed.rows_affected() == 0 {
            return Ok(false);
        }
        sqlx::query(
            "UPDATE auth_sessions SET revoked_at = ? WHERE user_id = ? AND revoked_at IS NULL",
        )
        .bind(now_ms)
        .bind(user.as_str())
        .execute(&mut *tx)
        .await
        .map_err(PersistenceFailure::from_source)?;

        tx.commit().await.map_err(PersistenceFailure::from_source)?;
        Ok(true)
    }
}

impl silverhand_backend_core::identity_access::credential::PasswordRecoveryStore
    for SqliteCredentialStore
{
    async fn account_for_email(&self, email: &str) -> Result<Option<UserId>, PersistenceFailure> {
        let raw: Option<String> =
            sqlx::query_scalar("SELECT id FROM users WHERE email = ? COLLATE NOCASE")
                .bind(email)
                .fetch_optional(&self.pool)
                .await
                .map_err(PersistenceFailure::from_source)?;
        raw.map(|raw| {
            UserId::parse(raw).map_err(|_| PersistenceFailure::from_source(CorruptStoredUserId))
        })
        .transpose()
    }

    async fn issue_code(
        &self,
        user: &UserId,
        email: &str,
        code_id: &str,
        code_hash: &str,
        now_ms: i64,
    ) -> Result<bool, PersistenceFailure> {
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(PersistenceFailure::from_source)?;
        let owned: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM users WHERE id = ? AND email = ? COLLATE NOCASE)",
        )
        .bind(user.as_str())
        .bind(email)
        .fetch_one(&mut *tx)
        .await
        .map_err(PersistenceFailure::from_source)?;
        if !owned {
            return Ok(false);
        }
        let window = current_window(&mut tx, user, now_ms).await?;
        if window.sends >= RECOVERY_WINDOW_MAX_SENDS
            || window.attempts >= RECOVERY_WINDOW_MAX_ATTEMPTS
            || window
                .last_sent_at
                .is_some_and(|sent| now_ms - sent < RECOVERY_RESEND_INTERVAL_MS)
        {
            return Ok(false);
        }
        sqlx::query(
            "INSERT INTO password_recovery_state (user_id, email, code_id, code_hash, \
             code_expires_at, code_attempts, window_started_at, window_sends, window_attempts, \
             last_sent_at) VALUES (?, ?, ?, ?, ?, 0, ?, ?, ?, ?) \
             ON CONFLICT(user_id) DO UPDATE SET email = excluded.email, \
             code_id = excluded.code_id, code_hash = excluded.code_hash, \
             code_expires_at = excluded.code_expires_at, code_attempts = 0, \
             window_started_at = excluded.window_started_at, \
             window_sends = excluded.window_sends, window_attempts = excluded.window_attempts, \
             last_sent_at = excluded.last_sent_at",
        )
        .bind(user.as_str())
        .bind(email)
        .bind(code_id)
        .bind(code_hash)
        .bind(now_ms + RECOVERY_CODE_TTL_MS)
        .bind(window.started_at)
        .bind(window.sends + 1)
        .bind(window.attempts)
        .bind(now_ms)
        .execute(&mut *tx)
        .await
        .map_err(PersistenceFailure::from_source)?;
        tx.commit().await.map_err(PersistenceFailure::from_source)?;
        Ok(true)
    }

    async fn claim_attempt(
        &self,
        email: &str,
        now_ms: i64,
    ) -> Result<Option<PendingRecoveryCode>, PersistenceFailure> {
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(PersistenceFailure::from_source)?;
        let row = sqlx::query(
            "SELECT s.user_id, s.code_id, s.code_hash, s.code_expires_at, s.code_attempts \
               FROM password_recovery_state s JOIN users u ON u.id = s.user_id \
              WHERE s.email = ? COLLATE NOCASE AND u.email = s.email COLLATE NOCASE",
        )
        .bind(email)
        .fetch_optional(&mut *tx)
        .await
        .map_err(PersistenceFailure::from_source)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let raw_user: String = row
            .try_get("user_id")
            .map_err(PersistenceFailure::from_source)?;
        let user = UserId::parse(raw_user)
            .map_err(|_| PersistenceFailure::from_source(CorruptStoredUserId))?;
        let code_id: Option<String> = row
            .try_get("code_id")
            .map_err(PersistenceFailure::from_source)?;
        let code_hash: Option<String> = row
            .try_get("code_hash")
            .map_err(PersistenceFailure::from_source)?;
        let expires_at: Option<i64> = row
            .try_get("code_expires_at")
            .map_err(PersistenceFailure::from_source)?;
        let code_attempts: i64 = row
            .try_get("code_attempts")
            .map_err(PersistenceFailure::from_source)?;
        let window = current_window(&mut tx, &user, now_ms).await?;
        let (Some(code_id), Some(code_hash), Some(expires_at)) = (code_id, code_hash, expires_at)
        else {
            return Ok(None);
        };
        if expires_at <= now_ms
            || code_attempts >= RECOVERY_CODE_MAX_ATTEMPTS
            || window.attempts >= RECOVERY_WINDOW_MAX_ATTEMPTS
        {
            return Ok(None);
        }
        sqlx::query(
            "UPDATE password_recovery_state SET code_attempts = code_attempts + 1, \
             window_started_at = ?, window_sends = ?, window_attempts = ? WHERE user_id = ?",
        )
        .bind(window.started_at)
        .bind(window.sends)
        .bind(window.attempts + 1)
        .bind(user.as_str())
        .execute(&mut *tx)
        .await
        .map_err(PersistenceFailure::from_source)?;
        tx.commit().await.map_err(PersistenceFailure::from_source)?;
        Ok(Some(PendingRecoveryCode {
            user_id: user,
            code_id,
            code_hash,
        }))
    }

    async fn complete_reset(
        &self,
        code: &PendingRecoveryCode,
        email: &str,
        replacement: silverhand_backend_core::identity_access::credential::PreparedPasswordCredential,
        now_ms: i64,
    ) -> Result<bool, PersistenceFailure> {
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(PersistenceFailure::from_source)?;
        // The claimed attempt is already counted, hence `<=`.
        let consumed = sqlx::query(
            "UPDATE password_recovery_state \
                SET code_id = NULL, code_hash = NULL, code_expires_at = NULL, code_attempts = 0 \
              WHERE user_id = ? AND code_id = ? AND code_hash = ? AND code_expires_at > ? \
                AND code_attempts <= ? AND window_attempts <= ? \
                AND email = ? COLLATE NOCASE \
                AND EXISTS (SELECT 1 FROM users u WHERE u.id = user_id AND u.email = ? COLLATE NOCASE)",
        )
        .bind(code.user_id.as_str())
        .bind(&code.code_id)
        .bind(&code.code_hash)
        .bind(now_ms)
        .bind(RECOVERY_CODE_MAX_ATTEMPTS)
        .bind(RECOVERY_WINDOW_MAX_ATTEMPTS)
        .bind(email)
        .bind(email)
        .execute(&mut *tx)
        .await
        .map_err(PersistenceFailure::from_source)?;
        if consumed.rows_affected() == 0 {
            return Ok(false);
        }
        sqlx::query(
            "INSERT INTO user_password_credentials \
             (user_id, password_hash, password_hash_params, password_updated_at) VALUES (?, ?, ?, ?) \
             ON CONFLICT(user_id) DO UPDATE SET password_hash = excluded.password_hash, \
             password_hash_params = excluded.password_hash_params, \
             password_updated_at = excluded.password_updated_at",
        )
        .bind(code.user_id.as_str())
        .bind(replacement.phc)
        .bind(serialize_password_hash_params(&replacement.params))
        .bind(now_ms)
        .execute(&mut *tx)
        .await
        .map_err(PersistenceFailure::from_source)?;
        sqlx::query(
            "UPDATE auth_sessions SET revoked_at = ? WHERE user_id = ? AND revoked_at IS NULL",
        )
        .bind(now_ms)
        .bind(code.user_id.as_str())
        .execute(&mut *tx)
        .await
        .map_err(PersistenceFailure::from_source)?;
        tx.commit().await.map_err(PersistenceFailure::from_source)?;
        Ok(true)
    }
}

struct RecoveryWindow {
    started_at: i64,
    sends: i64,
    attempts: i64,
    last_sent_at: Option<i64>,
}

/// The account's hourly window, rolled over when it has elapsed.
async fn current_window(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    user: &UserId,
    now_ms: i64,
) -> Result<RecoveryWindow, PersistenceFailure> {
    let row: Option<(i64, i64, i64, Option<i64>)> = sqlx::query_as(
        "SELECT window_started_at, window_sends, window_attempts, last_sent_at \
           FROM password_recovery_state WHERE user_id = ?",
    )
    .bind(user.as_str())
    .fetch_optional(&mut **tx)
    .await
    .map_err(PersistenceFailure::from_source)?;
    Ok(match row {
        Some((started_at, sends, attempts, last_sent_at))
            if now_ms - started_at < RECOVERY_WINDOW_MS =>
        {
            RecoveryWindow {
                started_at,
                sends,
                attempts,
                last_sent_at,
            }
        }
        Some((_, _, _, last_sent_at)) => RecoveryWindow {
            started_at: now_ms,
            sends: 0,
            attempts: 0,
            last_sent_at,
        },
        None => RecoveryWindow {
            started_at: now_ms,
            sends: 0,
            attempts: 0,
            last_sent_at: None,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use silverhand_backend_core::identity_access::credential::{Argon2idCost, RustCalibration};
    use sqlx::sqlite::SqliteConnectOptions;
    use sqlx::{Connection, Executor, SqliteConnection};

    const CURRENT_SCHEMA: &str =
        include_str!("../../../../../rust/test-fixtures/migration-baseline/schema-current.sql");

    // A signup user with a credential, a profile-only user (no credential row), and
    // a stranger email that resolves to no row at all.
    const SEED_SQL: &str = "\
INSERT INTO users (id, name, display_name, email, created_at, updated_at) \
  VALUES ('user-1', 'alice', 'Alice', 'alice@example.com', 1, 1); \
INSERT INTO users (id, name, display_name, email, created_at, updated_at) \
  VALUES ('user-2', 'bob', 'Bob', 'bob@example.com', 1, 1); \
INSERT INTO user_password_credentials (user_id, password_hash, password_hash_params, password_updated_at) \
  VALUES ('user-1', '$argon2id$v=19$stored', '{\"algorithm\":\"argon2id\"}', 1);";

    async fn open_seeded() -> (tempfile::TempDir, SqliteCredentialStore) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("credential.sqlite");
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .foreign_keys(true);
        let mut db = SqliteConnection::connect_with(&options).await.unwrap();
        db.execute(sqlx::raw_sql(CURRENT_SCHEMA)).await.unwrap();
        db.execute(sqlx::raw_sql(SEED_SQL)).await.unwrap();
        db.close().await.unwrap();
        let store = SqliteCredentialStore::open(&path).await.unwrap();
        (directory, store)
    }

    #[tokio::test]
    async fn a_signup_user_resolves_to_its_user_and_stored_phc() {
        let (_dir, store) = open_seeded().await;
        let candidate = store
            .lookup_login("alice@example.com")
            .await
            .unwrap()
            .expect("a row for a known email");
        assert_eq!(candidate.user_id.as_str(), "user-1");
        assert_eq!(
            candidate.password_hash.as_deref(),
            Some("$argon2id$v=19$stored")
        );
    }

    #[tokio::test]
    async fn a_profile_only_user_resolves_with_a_null_hash() {
        let (_dir, store) = open_seeded().await;
        let candidate = store
            .lookup_login("bob@example.com")
            .await
            .unwrap()
            .expect("a row for a profile-only user");
        assert_eq!(candidate.user_id.as_str(), "user-2");
        assert_eq!(candidate.password_hash, None);
    }

    #[tokio::test]
    async fn the_lookup_is_case_insensitive_on_email() {
        let (_dir, store) = open_seeded().await;
        let candidate = store.lookup_login("ALICE@EXAMPLE.COM").await.unwrap();
        assert_eq!(candidate.unwrap().user_id.as_str(), "user-1");
    }

    #[tokio::test]
    async fn an_unknown_email_resolves_to_none() {
        let (_dir, store) = open_seeded().await;
        assert!(
            store
                .lookup_login("nobody@example.com")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn the_rust_arm_serializes_to_the_checked_in_fixture_bytes() {
        let params = PasswordHashParams {
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
        };
        let fixture = include_str!("../../tests/fixtures/password-hash-params-rust.json");
        assert_eq!(serialize_password_hash_params(&params), fixture.trim_end());
    }
}
