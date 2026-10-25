use std::{path::Path, time::Duration};

use silverhand_backend_core::{
    foundation::identity::{HostId, UserId},
    foundation::persistence::PersistenceFailure,
    host_runtime::{
        claim::{
            ClaimLocalComputerUnitOfWork, LocalClaimAttempt, LocalClaimFacts, LocalClaimWrite,
            LocalClaimWriteError,
        },
        computer::{ComputerRecord, HostKind, ProvisioningSource},
        custody::{
            CredentialAuthState, CredentialIssueRefusal, CredentialIssueUnitOfWork, CredentialKind,
            CredentialSweep, HostCredentialEligibilityFacts, HostCredentialInventory,
            HostCredentialLookup, NewLongLivedCredential, judge_credential_eligibility,
        },
        invite::{HostInviteUnitOfWork, HostInviteWriteError, NewUserInviteHost},
        read::ComputerReadModel,
        revoke::{RevokeComputerAttempt, RevokeComputerUnitOfWork, RevokedComputer},
    },
};
use sqlx::{Row, Sqlite, Transaction};
use thiserror::Error;

mod management;

// #task:2294 — four low-fanout read adapters relocated under this self-named
// `host_runtime` context (dual role: real context + relocation namespace). Declared
// here only; their owners/targets live in the organization map/gate.
pub mod agent_availability_read;
pub(crate) mod agent_bulk_stop_read;
pub mod agent_presence_read;
pub mod agent_runtime_state_read;
pub mod computer_agents_read;
pub mod host_resources_read;
pub mod runtime_credential;
mod runtime_switch;
mod runtime_transition;

const ROOT_WRITE_BEGIN: &str = "BEGIN IMMEDIATE";

#[derive(Clone)]
pub struct SqliteHostRuntimeStore {
    pool: crate::foundation::database::SqliteDatabase,
}

#[derive(Debug, Error)]
pub enum SqliteHostRuntimeStoreError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
}

impl SqliteHostRuntimeStore {
    pub fn from_pool(pool: impl Into<crate::foundation::database::SqliteDatabase>) -> Self {
        let pool = pool.into();
        Self { pool }
    }

    pub async fn open(path: impl AsRef<Path>) -> Result<Self, SqliteHostRuntimeStoreError> {
        Self::open_with_busy_timeout(path, Duration::from_secs(5)).await
    }

    async fn open_with_busy_timeout(
        path: impl AsRef<Path>,
        busy_timeout: Duration,
    ) -> Result<Self, SqliteHostRuntimeStoreError> {
        let pool = crate::foundation::database::store_pool(path.as_ref(), busy_timeout).await?;
        Ok(Self { pool })
    }

    /// The single place a write axis reserves the one writer before its first read,
    /// so an eligibility read and the write it guards cannot straddle another writer's
    /// commit.
    async fn begin_root_write(&self) -> Result<Transaction<'_, Sqlite>, sqlx::Error> {
        crate::foundation::statement_metrics::statement(
            "hosts.begin_immediate",
            "run",
            self.pool.begin_with(ROOT_WRITE_BEGIN),
        )
        .await
    }

    async fn create_transaction(
        &self,
        host: NewUserInviteHost,
        credential: NewLongLivedCredential,
    ) -> Result<(), HostInviteWriteError> {
        let mut tx = self.begin_root_write().await.map_err(persistence)?;
        let name_exists: i64 = crate::foundation::statement_metrics::statement(
            "hosts.name_exists",
            "get",
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM hosts WHERE name = ?)")
                .bind(host.name())
                .fetch_one(&mut *tx),
        )
        .await
        .map_err(persistence)?;
        if name_exists != 0 {
            return Err(HostInviteWriteError::DuplicateHostName);
        }

        crate::foundation::statement_metrics::statement(
            "hosts.create",
            "run",
            sqlx::query(
                "INSERT INTO hosts
                (id, name, kind, user_id, installation_id, last_seen_at, revoked_at,
                 created_at, display_label, activated_at, finalized_at, provisioning_source)
             VALUES (?, ?, 'remote', ?, NULL, NULL, NULL, ?, NULL, NULL, NULL, 'user_invite')",
            )
            .bind(host.id())
            .bind(host.name())
            .bind(host.user_id().as_str())
            .bind(host.created_at())
            .execute(&mut *tx),
        )
        .await
        .map_err(persistence)?;

        crate::foundation::statement_metrics::statement(
            "host_credentials.create",
            "run",
            sqlx::query(
                "INSERT INTO host_credentials
                (id, host_id, token_hash, kind, expires_at, consumed_at, revoked_at)
             VALUES (?, ?, ?, 'long_lived', NULL, NULL, NULL)",
            )
            .bind(credential.id())
            .bind(credential.host_id())
            .bind(credential.token_hash())
            .execute(&mut *tx),
        )
        .await
        .map_err(persistence)?;

        tx.commit().await.map_err(persistence)?;
        Ok(())
    }
}

impl HostInviteUnitOfWork for SqliteHostRuntimeStore {
    async fn create_host_and_credential(
        &self,
        host: NewUserInviteHost,
        credential: NewLongLivedCredential,
    ) -> Result<(), HostInviteWriteError> {
        self.create_transaction(host, credential).await
    }
}

impl HostCredentialLookup for SqliteHostRuntimeStore {
    async fn lookup_by_token_hash(
        &self,
        token_hash: String,
    ) -> Result<Option<CredentialAuthState>, PersistenceFailure> {
        let row = crate::foundation::statement_metrics::statement(
            "host_credentials.lookup_by_token_hash",
            "get",
            sqlx::query(
                "SELECT c.id AS credential_id, c.host_id, c.kind, c.expires_at,
                    c.consumed_at, c.revoked_at AS credential_revoked_at,
                    h.user_id AS host_user_id, h.revoked_at AS host_revoked_at
               FROM host_credentials c
               JOIN hosts h ON h.id = c.host_id
              WHERE c.token_hash = ?",
            )
            .bind(token_hash)
            .fetch_optional(&self.pool),
        )
        .await
        .map_err(PersistenceFailure::from_source)?;
        row.map(|row| -> Result<CredentialAuthState, sqlx::Error> {
            let kind: String = row.try_get("kind")?;
            let host_user_id = row
                .try_get::<Option<String>, _>("host_user_id")?
                .map(|raw| {
                    UserId::parse(raw).map_err(|source| sqlx::Error::Decode(Box::new(source)))
                })
                .transpose()?;
            Ok(CredentialAuthState {
                credential_id: row.try_get("credential_id")?,
                host_id: row.try_get("host_id")?,
                host_user_id,
                kind: decode_credential_kind(&kind)?,
                expires_at: row.try_get("expires_at")?,
                consumed_at: row.try_get("consumed_at")?,
                credential_revoked_at: row.try_get("credential_revoked_at")?,
                host_revoked_at: row.try_get("host_revoked_at")?,
            })
        })
        .transpose()
        .map_err(PersistenceFailure::from_source)
    }
}

fn persistence(error: sqlx::Error) -> HostInviteWriteError {
    HostInviteWriteError::Persistence(PersistenceFailure::from_source(error))
}

/// Every column [`row_to_computer_record`] reads, spelled out in each query so the
/// decode does not depend on the order `SELECT *` happens to yield and a new column
/// cannot silently shift it.
impl ComputerReadModel for SqliteHostRuntimeStore {
    async fn list_for_owner(
        &self,
        owner: UserId,
    ) -> Result<Vec<ComputerRecord>, PersistenceFailure> {
        let rows = crate::foundation::statement_metrics::statement(
            "hosts.list_for_user",
            "all",
            sqlx::query(
                "SELECT id, name, kind, created_at, user_id, installation_id, last_seen_at,
                    revoked_at, display_label, activated_at, provisioning_source, finalized_at
               FROM hosts
              WHERE user_id = ?
                AND NOT (provisioning_source = 'user_invite' AND finalized_at IS NULL)
              ORDER BY (CASE WHEN revoked_at IS NULL THEN 0 ELSE 1 END) ASC,
                       created_at ASC",
            )
            .bind(owner.as_str())
            .fetch_all(&self.pool),
        )
        .await
        .map_err(PersistenceFailure::from_source)?;
        rows.iter()
            .map(row_to_computer_record)
            .collect::<Result<Vec<_>, _>>()
            .map_err(PersistenceFailure::from_source)
    }

    async fn read_computer(
        &self,
        computer: HostId,
    ) -> Result<Option<ComputerRecord>, PersistenceFailure> {
        let row = crate::foundation::statement_metrics::statement(
            "hosts.get_by_id",
            "get",
            sqlx::query(
                "SELECT id, name, kind, created_at, user_id, installation_id, last_seen_at,
                    revoked_at, display_label, activated_at, provisioning_source, finalized_at
               FROM hosts
              WHERE id = ?",
            )
            .bind(computer.as_str())
            .fetch_optional(&self.pool),
        )
        .await
        .map_err(PersistenceFailure::from_source)?;
        row.as_ref()
            .map(row_to_computer_record)
            .transpose()
            .map_err(PersistenceFailure::from_source)
    }
}

fn row_to_computer_record(row: &sqlx::sqlite::SqliteRow) -> Result<ComputerRecord, sqlx::Error> {
    let id = HostId::parse(row.try_get::<String, _>("id")?)
        .map_err(|source| sqlx::Error::Decode(Box::new(source)))?;
    let owner = row
        .try_get::<Option<String>, _>("user_id")?
        .map(|raw| UserId::parse(raw).map_err(|source| sqlx::Error::Decode(Box::new(source))))
        .transpose()?;
    Ok(ComputerRecord {
        id,
        name: row.try_get("name")?,
        kind: decode_host_kind(&row.try_get::<String, _>("kind")?)?,
        created_at: row.try_get("created_at")?,
        owner,
        installation_id: row.try_get("installation_id")?,
        last_seen_at: row.try_get("last_seen_at")?,
        activated_at: row.try_get("activated_at")?,
        finalized_at: row.try_get("finalized_at")?,
        revoked_at: row.try_get("revoked_at")?,
        provisioning_source: decode_provisioning_source(
            &row.try_get::<String, _>("provisioning_source")?,
        )?,
        display_label: row.try_get("display_label")?,
    })
}

fn decode_host_kind(raw: &str) -> Result<HostKind, sqlx::Error> {
    match raw {
        "local" => Ok(HostKind::Local),
        "remote" => Ok(HostKind::Remote),
        _ => Err(sqlx::Error::Decode(Box::new(InvalidHostKind))),
    }
}

fn decode_provisioning_source(raw: &str) -> Result<ProvisioningSource, sqlx::Error> {
    match raw {
        "app_install" => Ok(ProvisioningSource::AppInstall),
        "user_invite" => Ok(ProvisioningSource::UserInvite),
        "system" => Ok(ProvisioningSource::System),
        _ => Err(sqlx::Error::Decode(Box::new(InvalidProvisioningSource))),
    }
}

#[derive(Debug, Error)]
#[error("persisted host kind is outside the closed set")]
struct InvalidHostKind;

#[derive(Debug, Error)]
#[error("persisted provisioning source is outside the closed set")]
struct InvalidProvisioningSource;

fn decode_credential_kind(kind: &str) -> Result<CredentialKind, sqlx::Error> {
    match kind {
        "long_lived" => Ok(CredentialKind::LongLived),
        "bootstrap" => Ok(CredentialKind::Bootstrap),
        _ => Err(sqlx::Error::Decode(Box::new(InvalidCredentialKind))),
    }
}

#[derive(Debug, Error)]
#[error("persisted host credential kind is unsupported")]
struct InvalidCredentialKind;

impl RevokeComputerUnitOfWork for SqliteHostRuntimeStore {
    async fn revoke_computer(
        &self,
        attempt: RevokeComputerAttempt,
    ) -> Result<Option<RevokedComputer>, PersistenceFailure> {
        let computer = attempt.computer().as_str().to_owned();
        let at = attempt.at();
        let mut tx = self
            .begin_root_write()
            .await
            .map_err(PersistenceFailure::from_source)?;

        // The tombstone moves once: only a still-live row takes the stamp, and how many
        // rows that reached is whether THIS call ended custody.
        let tombstone = crate::foundation::statement_metrics::statement(
            "hosts.revoke",
            "run",
            sqlx::query("UPDATE hosts SET revoked_at = ? WHERE id = ? AND revoked_at IS NULL")
                .bind(at)
                .bind(&computer)
                .execute(&mut *tx),
        )
        .await
        .map_err(PersistenceFailure::from_source)?;
        let custody_ended_now = tombstone.rows_affected() > 0;

        // Unconditional, on a repeat revoke too: every live credential dies at `at`. Only
        // revoked_at is written — nothing here is superseded, so a consumed_at would
        // describe a dead credential to the verifier as one inside a grace window.
        crate::foundation::statement_metrics::statement(
            "host_credentials.revoke_for_host",
            "run",
            sqlx::query(
                "UPDATE host_credentials SET revoked_at = ? WHERE host_id = ? AND revoked_at IS NULL",
            )
            .bind(at)
            .bind(&computer)
            .execute(&mut *tx),
        )
        .await
        .map_err(PersistenceFailure::from_source)?;

        let row = crate::foundation::statement_metrics::statement(
            "hosts.get_by_id",
            "get",
            sqlx::query(
                "SELECT id, name, kind, created_at, user_id, installation_id, last_seen_at,
                    revoked_at, display_label, activated_at, provisioning_source, finalized_at
               FROM hosts
              WHERE id = ?",
            )
            .bind(&computer)
            .fetch_optional(&mut *tx),
        )
        .await
        .map_err(PersistenceFailure::from_source)?;

        let revoked = row
            .as_ref()
            .map(row_to_computer_record)
            .transpose()
            .map_err(PersistenceFailure::from_source)?
            .map(|computer| RevokedComputer {
                computer,
                custody_ended_now,
            });

        tx.commit().await.map_err(PersistenceFailure::from_source)?;
        Ok(revoked)
    }
}

impl CredentialIssueUnitOfWork for SqliteHostRuntimeStore {
    async fn issue_credential(
        &self,
        credential: NewLongLivedCredential,
        sweep: Option<CredentialSweep>,
    ) -> Result<Result<(), CredentialIssueRefusal>, PersistenceFailure> {
        let mut tx = self
            .begin_root_write()
            .await
            .map_err(PersistenceFailure::from_source)?;

        // The verdict is re-asked under the lock from a fresh read: a host revoked
        // between a route's pre-lock hint and this write is refused here, and the
        // authority for the refusal is the core rule, not a second copy of it.
        let facts = crate::foundation::statement_metrics::statement(
            "hosts.get_auth_state",
            "get",
            sqlx::query("SELECT user_id, revoked_at FROM hosts WHERE id = ?")
                .bind(credential.host_id())
                .fetch_optional(&mut *tx),
        )
        .await
        .map_err(PersistenceFailure::from_source)?
        .map(
            |row| -> Result<HostCredentialEligibilityFacts, sqlx::Error> {
                let user_id = row
                    .try_get::<Option<String>, _>("user_id")?
                    .map(|raw| {
                        UserId::parse(raw).map_err(|source| sqlx::Error::Decode(Box::new(source)))
                    })
                    .transpose()?;
                Ok(HostCredentialEligibilityFacts {
                    user_id,
                    revoked_at: row.try_get("revoked_at")?,
                })
            },
        )
        .transpose()
        .map_err(PersistenceFailure::from_source)?;

        if let Err(refusal) = judge_credential_eligibility(facts.as_ref()) {
            return Ok(Err(refusal));
        }

        crate::foundation::statement_metrics::statement(
            "host_credentials.create",
            "run",
            sqlx::query(
                "INSERT INTO host_credentials
                (id, host_id, token_hash, kind, expires_at, consumed_at, revoked_at)
             VALUES (?, ?, ?, 'long_lived', NULL, NULL, NULL)",
            )
            .bind(credential.id())
            .bind(credential.host_id())
            .bind(credential.token_hash())
            .execute(&mut *tx),
        )
        .await
        .map_err(PersistenceFailure::from_source)?;

        // The sweep keeps exactly the credential just inserted (id <> ?) and never
        // overwrites an earlier consumed_at, so a token already inside its grace window
        // keeps the instant it was first superseded.
        if let Some(sweep) = sweep {
            crate::foundation::statement_metrics::statement(
                "host_credentials.supersede",
                "run",
                sqlx::query(
                    "UPDATE host_credentials
                    SET revoked_at = ?, consumed_at = COALESCE(consumed_at, ?)
                  WHERE host_id = ?
                    AND id <> ?
                    AND kind = 'long_lived'
                    AND revoked_at IS NULL",
                )
                .bind(sweep.revoked_at())
                .bind(sweep.consumed_at())
                .bind(credential.host_id())
                .bind(credential.id())
                .execute(&mut *tx),
            )
            .await
            .map_err(PersistenceFailure::from_source)?;
        }

        tx.commit().await.map_err(PersistenceFailure::from_source)?;
        Ok(Ok(()))
    }
}

impl HostCredentialInventory for SqliteHostRuntimeStore {
    async fn has_active_long_lived_credential(
        &self,
        computer: HostId,
        now: i64,
    ) -> Result<bool, PersistenceFailure> {
        let count: i64 = crate::foundation::statement_metrics::statement(
            "host_credentials.has_active_long_lived",
            "get",
            sqlx::query_scalar(
                "SELECT COUNT(*)
               FROM host_credentials cred
               JOIN hosts h ON h.id = cred.host_id
              WHERE cred.host_id = ?
                AND cred.kind = 'long_lived'
                AND cred.consumed_at IS NULL
                AND cred.revoked_at IS NULL
                AND (cred.expires_at IS NULL OR cred.expires_at > ?)
                AND h.revoked_at IS NULL
                AND h.user_id IS NOT NULL",
            )
            .bind(computer.as_str())
            .bind(now)
            .fetch_one(&self.pool),
        )
        .await
        .map_err(PersistenceFailure::from_source)?;
        Ok(count > 0)
    }
}

fn claim_persistence(error: sqlx::Error) -> LocalClaimWriteError {
    LocalClaimWriteError::Persistence(PersistenceFailure::from_source(error))
}

impl ClaimLocalComputerUnitOfWork for SqliteHostRuntimeStore {
    async fn claim_local_computer(
        &self,
        attempt: LocalClaimAttempt,
    ) -> Result<ComputerRecord, LocalClaimWriteError> {
        let mut tx = self.begin_root_write().await.map_err(claim_persistence)?;

        // Both facts are read under the writer lock, so a row a concurrent claim from the
        // same installation could create cannot appear between a read and the write it
        // conditions — the difference between one computer for a machine and two.
        let claimed: Option<String> = crate::foundation::statement_metrics::statement(
            "hosts.get_by_user_installation",
            "get",
            sqlx::query_scalar(
                "SELECT id FROM hosts
              WHERE user_id = ? AND installation_id = ? AND revoked_at IS NULL",
            )
            .bind(attempt.owner().as_str())
            .bind(attempt.installation_id())
            .fetch_optional(&mut *tx),
        )
        .await
        .map_err(claim_persistence)?;
        let unclaimed_local: Option<String> = crate::foundation::statement_metrics::statement(
            "hosts.get_unclaimed_local",
            "get",
            sqlx::query_scalar(
                "SELECT id FROM hosts
              WHERE name = 'local'
                AND kind = 'local'
                AND user_id IS NULL
                AND installation_id IS NULL
                AND revoked_at IS NULL",
            )
            .fetch_optional(&mut *tx),
        )
        .await
        .map_err(claim_persistence)?;

        let to_host_id = |raw: Option<String>| -> Result<Option<HostId>, sqlx::Error> {
            raw.map(|raw| {
                HostId::parse(raw).map_err(|source| sqlx::Error::Decode(Box::new(source)))
            })
            .transpose()
        };
        let facts = LocalClaimFacts {
            claimed: to_host_id(claimed).map_err(claim_persistence)?,
            unclaimed_local: to_host_id(unclaimed_local).map_err(claim_persistence)?,
        };

        let target = match attempt.decide(&facts)? {
            // Reuse touches nothing but the last-seen stamp: custody, activation and the
            // chosen name all stay as this installation already left them.
            LocalClaimWrite::TouchLastSeen(touch) => {
                crate::foundation::statement_metrics::statement(
                    "hosts.touch_last_seen",
                    "run",
                    sqlx::query("UPDATE hosts SET last_seen_at = ? WHERE id = ?")
                        .bind(attempt.at())
                        .bind(touch.computer().as_str())
                        .execute(&mut *tx),
                )
                .await
                .map_err(claim_persistence)?;
                touch.computer().as_str().to_owned()
            }
            // Adoption binds owner and installation and finishes activation, but COALESCE
            // keeps any stamp the placeholder already carried rather than resetting it to
            // this instant. The label write is separate and conditional: `None` means the
            // caller sent no label, so a reclaim never blanks a name the user chose.
            LocalClaimWrite::Adopt(adopt) => {
                crate::foundation::statement_metrics::statement(
                    "hosts.bind_to_user_installation",
                    "run",
                    sqlx::query(
                        "UPDATE hosts
                        SET user_id = ?,
                            installation_id = ?,
                            last_seen_at = ?,
                            activated_at = COALESCE(activated_at, ?),
                            finalized_at = COALESCE(finalized_at, ?),
                            provisioning_source = 'app_install'
                      WHERE id = ?",
                    )
                    .bind(attempt.owner().as_str())
                    .bind(attempt.installation_id())
                    .bind(attempt.at())
                    .bind(attempt.at())
                    .bind(attempt.at())
                    .bind(adopt.computer().as_str())
                    .execute(&mut *tx),
                )
                .await
                .map_err(claim_persistence)?;
                if let Some(label) = adopt.display_label() {
                    crate::foundation::statement_metrics::statement(
                        "hosts.update_display_label",
                        "run",
                        sqlx::query("UPDATE hosts SET display_label = ? WHERE id = ?")
                            .bind(label)
                            .bind(adopt.computer().as_str())
                            .execute(&mut *tx),
                    )
                    .await
                    .map_err(claim_persistence)?;
                }
                adopt.computer().as_str().to_owned()
            }
            // A first claim registers the computer under the name the ladder chose. The
            // name is unique across every row, live or not, so the pre-check carries no
            // revoked filter; a taken name is the one outcome reported back rather than
            // resolved, letting the ladder try its next candidate.
            LocalClaimWrite::Register(register) => {
                let taken: i64 = crate::foundation::statement_metrics::statement(
                    "hosts.name_exists",
                    "get",
                    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM hosts WHERE name = ?)")
                        .bind(register.name())
                        .fetch_one(&mut *tx),
                )
                .await
                .map_err(claim_persistence)?;
                if taken != 0 {
                    return Err(LocalClaimWriteError::DuplicateComputerName);
                }
                crate::foundation::statement_metrics::statement(
                    "hosts.create",
                    "run",
                    sqlx::query(
                        "INSERT INTO hosts
                        (id, name, kind, user_id, installation_id, last_seen_at, revoked_at,
                         created_at, display_label, activated_at, finalized_at, provisioning_source)
                     VALUES (?, ?, 'local', ?, ?, ?, NULL, ?, ?, ?, ?, 'app_install')",
                    )
                    .bind(register.id())
                    .bind(register.name())
                    .bind(attempt.owner().as_str())
                    .bind(attempt.installation_id())
                    .bind(attempt.at())
                    .bind(attempt.at())
                    .bind(register.display_label())
                    .bind(attempt.at())
                    .bind(attempt.at())
                    .execute(&mut *tx),
                )
                .await
                .map_err(claim_persistence)?;
                register.id().to_owned()
            }
        };

        // The computer as it stands AFTER the write, re-read in the same transaction:
        // adoption keeps stamps this attempt did not set, so projecting from the attempt
        // would report a shape the row does not have.
        let row = crate::foundation::statement_metrics::statement(
            "hosts.get_by_id",
            "get",
            sqlx::query(
                "SELECT id, name, kind, created_at, user_id, installation_id, last_seen_at,
                    revoked_at, display_label, activated_at, provisioning_source, finalized_at
               FROM hosts
              WHERE id = ?",
            )
            .bind(&target)
            .fetch_one(&mut *tx),
        )
        .await
        .map_err(claim_persistence)?;
        let record = row_to_computer_record(&row).map_err(claim_persistence)?;

        tx.commit().await.map_err(claim_persistence)?;
        Ok(record)
    }
}

impl silverhand_backend_core::host_runtime::lifecycle::HostActivationPort
    for SqliteHostRuntimeStore
{
    async fn activate_attachment(
        &self,
        host: &str,
        owner: &UserId,
        credential_id: &str,
        now: i64,
    ) -> Result<bool, PersistenceFailure> {
        let result = crate::foundation::statement_metrics::statement(
            "hosts.activate_attachment",
            "run",
            sqlx::query(
                "UPDATE hosts SET activated_at = CASE
                WHEN provisioning_source = 'user_invite' THEN COALESCE(activated_at, ?) ELSE activated_at END
                WHERE id = ? AND user_id = ? AND revoked_at IS NULL
                AND EXISTS (SELECT 1 FROM host_credentials c WHERE c.id = ? AND c.host_id = hosts.id
                    AND c.kind = 'long_lived'
                    AND (c.expires_at IS NULL OR c.expires_at > ?)
                    AND ((c.revoked_at IS NULL AND c.consumed_at IS NULL)
                        OR (c.revoked_at > ? AND c.consumed_at IS NOT NULL)))",
            )
            .bind(now)
            .bind(host)
            .bind(owner.as_str())
            .bind(credential_id)
            .bind(now)
            .bind(now)
            .execute(&self.pool),
        )
        .await
        .map_err(PersistenceFailure::from_source)?;
        Ok(result.rows_affected() == 1)
    }
}

#[cfg(test)]
mod tests {
    use silverhand_backend_core::foundation::identity::UserId;
    use silverhand_backend_core::host_runtime::{
        claim::{ClaimLocalComputer, LocalClaimRequest},
        computer::ComputerDisplayLabel,
        custody::{HostTokenRejection, VerifyHostTokenFact, mint_credential, verify_host_token},
        environment::{Clock, EntropySource, OsEntropy, SystemClock},
        invite::CreateHostInvite,
        read::find_owned_computer,
        revoke::AdmittedRevoke,
    };
    use sqlx::Connection;
    use std::{collections::VecDeque, convert::Infallible, sync::Mutex};
    use tempfile::TempDir;

    use super::*;

    async fn fixture() -> (TempDir, SqliteHostRuntimeStore) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("host.sqlite");
        let mut connection = crate::current_schema::create_current_schema(&path).await;
        sqlx::query(
            "INSERT INTO users (id, name, display_name, created_at, updated_at)
             VALUES ('user-1', 'user-1', 'User One', 1, 1)",
        )
        .execute(&mut connection)
        .await
        .unwrap();
        connection.close().await.unwrap();
        let store = SqliteHostRuntimeStore::open(&path).await.unwrap();
        (directory, store)
    }

    #[tokio::test]
    async fn attachment_activation_matches_credential_authority_and_never_finalizes() {
        use silverhand_backend_core::host_runtime::lifecycle::HostActivationPort;
        for mutation in [
            "UPDATE host_credentials SET expires_at=100 WHERE host_id=?",
            "UPDATE host_credentials SET revoked_at=100 WHERE host_id=?",
            "UPDATE host_credentials SET consumed_at=50 WHERE host_id=?",
            "UPDATE host_credentials SET revoked_at=200,consumed_at=50 WHERE host_id=?",
            "UPDATE host_credentials SET revoked_at=100,consumed_at=50 WHERE host_id=?",
            "UPDATE hosts SET revoked_at=100 WHERE id=?",
            "DELETE FROM host_credentials WHERE host_id=?",
        ] {
            let (_directory, store) = fixture().await;
            let owner = UserId::parse("user-1").unwrap();
            let invite = CreateHostInvite::new(store.clone(), OsEntropy, SystemClock)
                .execute(owner.clone())
                .await
                .unwrap();
            let host = invite.host.id().to_owned();
            let token = invite.connect_token.reveal_once().to_owned();
            let VerifyHostTokenFact::Accepted { credential_id, .. } =
                verify_host_token(&store, token.clone(), 100).await.unwrap()
            else {
                panic!("fresh token rejected")
            };
            sqlx::query(mutation)
                .bind(&host)
                .execute(&store.pool)
                .await
                .unwrap();

            let eligible = matches!(
                verify_host_token(&store, token, 100).await.unwrap(),
                VerifyHostTokenFact::Accepted { .. }
            );
            assert_eq!(
                store
                    .activate_attachment(&host, &owner, &credential_id, 100)
                    .await
                    .unwrap(),
                eligible,
                "{mutation}"
            );
            let stamps: (Option<i64>, Option<i64>) =
                sqlx::query_as("SELECT activated_at, finalized_at FROM hosts WHERE id=?")
                    .bind(&host)
                    .fetch_one(&store.pool)
                    .await
                    .unwrap();
            assert_eq!(stamps, (eligible.then_some(100), None));
        }
    }

    #[tokio::test]
    async fn activation_is_idempotent_under_concurrency_and_does_not_recreate_deleted_hosts() {
        use silverhand_backend_core::host_runtime::lifecycle::HostActivationPort;
        let (_directory, store) = fixture().await;
        let owner = UserId::parse("user-1").unwrap();
        let invite = CreateHostInvite::new(store.clone(), OsEntropy, SystemClock)
            .execute(owner.clone())
            .await
            .unwrap();
        let host = invite.host.id().to_owned();
        let VerifyHostTokenFact::Accepted { credential_id, .. } =
            verify_host_token(&store, invite.connect_token.reveal_once().to_owned(), 100)
                .await
                .unwrap()
        else {
            panic!("fresh token rejected")
        };
        assert!(
            !store
                .activate_attachment(&host, &UserId::parse("other").unwrap(), &credential_id, 100)
                .await
                .unwrap()
        );

        let (first, second) = tokio::join!(
            store.activate_attachment(&host, &owner, &credential_id, 100),
            store.activate_attachment(&host, &owner, &credential_id, 200)
        );
        assert!(first.unwrap() && second.unwrap());
        let stamp: i64 = sqlx::query_scalar("SELECT activated_at FROM hosts WHERE id=?")
            .bind(&host)
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert!([100, 200].contains(&stamp));
        assert!(
            store
                .activate_attachment(&host, &owner, &credential_id, 300)
                .await
                .unwrap()
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT activated_at FROM hosts WHERE id=?")
                .bind(&host)
                .fetch_one(&store.pool)
                .await
                .unwrap(),
            stamp
        );

        sqlx::query("DELETE FROM host_credentials WHERE host_id=?")
            .bind(&host)
            .execute(&store.pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM hosts WHERE id=?")
            .bind(&host)
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(
            !store
                .activate_attachment(&host, &owner, &credential_id, 400)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn persists_only_the_hash_and_verifies_after_reopening() {
        let (directory, store) = fixture().await;
        let service = CreateHostInvite::new(store.clone(), OsEntropy, SystemClock);
        let fact = service
            .execute(UserId::parse("user-1").unwrap())
            .await
            .unwrap();
        let host_id = fact.host.id().to_owned();
        let redacted = format!("{:?}", fact.connect_token);
        let raw_token = fact.connect_token.reveal_once();
        drop(service);
        drop(store);

        let reopened = SqliteHostRuntimeStore::open(directory.path().join("host.sqlite"))
            .await
            .unwrap();
        let verified = verify_host_token(&reopened, format!("  {raw_token}  "), i64::MAX / 2)
            .await
            .unwrap();
        assert!(
            matches!(verified, VerifyHostTokenFact::Accepted { host_id: ref verified_id, ref user_id, .. } if verified_id == &host_id && user_id.as_str() == "user-1")
        );

        let row = sqlx::query("SELECT token_hash FROM host_credentials WHERE host_id = ?")
            .bind(&host_id)
            .fetch_one(&reopened.pool)
            .await
            .unwrap();
        let stored: String = row.try_get("token_hash").unwrap();
        assert_ne!(stored, raw_token);
        assert_eq!(redacted, "ConnectToken([REDACTED])");
        assert!(!redacted.contains(&raw_token));

        let row = sqlx::query(
            "SELECT h.kind, h.provisioning_source, h.installation_id, h.last_seen_at,
                    h.revoked_at, h.display_label, h.activated_at, h.finalized_at,
                    c.kind AS credential_kind, c.expires_at, c.consumed_at,
                    c.revoked_at AS credential_revoked_at
               FROM hosts h
               JOIN host_credentials c ON c.host_id = h.id
              WHERE h.id = ?",
        )
        .bind(&host_id)
        .fetch_one(&reopened.pool)
        .await
        .unwrap();
        assert_eq!(row.get::<String, _>("kind"), "remote");
        assert_eq!(row.get::<String, _>("provisioning_source"), "user_invite");
        assert_eq!(row.get::<String, _>("credential_kind"), "long_lived");
        for column in ["installation_id", "display_label"] {
            assert_eq!(
                row.get::<Option<String>, _>(column),
                None,
                "column {column}"
            );
        }
        for column in [
            "last_seen_at",
            "revoked_at",
            "activated_at",
            "finalized_at",
            "expires_at",
            "consumed_at",
            "credential_revoked_at",
        ] {
            assert_eq!(row.get::<Option<i64>, _>(column), None, "column {column}");
        }
    }

    #[tokio::test]
    async fn credential_failure_rolls_back_the_host_row() {
        let (_directory, store) = fixture().await;
        sqlx::query("CREATE TRIGGER refuse_credential BEFORE INSERT ON host_credentials BEGIN SELECT RAISE(ABORT, 'credential refused'); END")
            .execute(&store.pool)
            .await
            .unwrap();
        let service = CreateHostInvite::new(store.clone(), OsEntropy, SystemClock);

        assert!(
            service
                .execute(UserId::parse("user-1").unwrap())
                .await
                .is_err()
        );
        let host_count: i64 = sqlx::query_scalar("SELECT count(*) FROM hosts")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        let credential_count: i64 = sqlx::query_scalar("SELECT count(*) FROM host_credentials")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!((host_count, credential_count), (0, 0));
    }

    #[tokio::test]
    async fn unknown_and_blank_tokens_are_rejected_without_plaintext_comparison() {
        let (_directory, store) = fixture().await;
        assert_eq!(
            verify_host_token(&store, "   ".into(), 0).await.unwrap(),
            VerifyHostTokenFact::Rejected(HostTokenRejection::Missing)
        );
        assert_eq!(
            verify_host_token(&store, "unknown".into(), 0)
                .await
                .unwrap(),
            VerifyHostTokenFact::Rejected(HostTokenRejection::Missing)
        );
    }

    #[tokio::test]
    async fn real_schema_credential_predicates_match_the_ts_verdicts() {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Case {
            name: String,
            now: i64,
            state: serde_json::Map<String, serde_json::Value>,
            expected: String,
        }
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct State {
            kind: String,
            expires_at: Option<i64>,
            consumed_at: Option<i64>,
            revoked_at: Option<i64>,
            host_user_id: Option<String>,
            host_revoked_at: Option<i64>,
        }

        let cases: Vec<Case> = serde_json::from_str(include_str!(
            "../../tests/fixtures/host_credential_verdicts.json"
        ))
        .unwrap();
        for case in cases {
            let (_directory, store) = fixture().await;
            let fact = CreateHostInvite::new(store.clone(), OsEntropy, SystemClock)
                .execute(UserId::parse("user-1").unwrap())
                .await
                .unwrap();
            let host_id = fact.host.id().to_owned();
            let token = fact.connect_token.reveal_once();
            // Match the independent oracle's defaults; explicit null overrides
            // the bound owner instead of being mistaken for an absent field.
            let mut state = serde_json::json!({
                "kind": "long_lived",
                "expires_at": null,
                "consumed_at": null,
                "revoked_at": null,
                "host_user_id": "user-1",
                "host_revoked_at": null
            });
            state.as_object_mut().unwrap().extend(case.state);
            let state: State = serde_json::from_value(state).unwrap();
            sqlx::query(
                "UPDATE host_credentials SET kind=?, expires_at=?, consumed_at=?, revoked_at=?
                 WHERE host_id=?",
            )
            .bind(state.kind)
            .bind(state.expires_at)
            .bind(state.consumed_at)
            .bind(state.revoked_at)
            .bind(&host_id)
            .execute(&store.pool)
            .await
            .unwrap();
            sqlx::query("UPDATE hosts SET user_id=?, revoked_at=? WHERE id=?")
                .bind(state.host_user_id)
                .bind(state.host_revoked_at)
                .bind(&host_id)
                .execute(&store.pool)
                .await
                .unwrap();

            let actual = match verify_host_token(&store, token, case.now).await.unwrap() {
                VerifyHostTokenFact::Accepted { .. } => "accepted",
                VerifyHostTokenFact::Rejected(rejection) => rejection_code(rejection),
            };
            assert_eq!(actual, case.expected, "{}", case.name);
        }
    }

    fn rejection_code(rejection: HostTokenRejection) -> &'static str {
        match rejection {
            HostTokenRejection::Missing => "missing",
            HostTokenRejection::CredentialNotUsable => "credential_not_usable",
            HostTokenRejection::CredentialExpired => "credential_expired",
            HostTokenRejection::CredentialRevoked => "credential_revoked",
            HostTokenRejection::HostRevoked => "host_revoked",
            HostTokenRejection::HostUnbound => "host_unbound",
        }
    }

    #[test]
    fn credential_kind_is_decoded_at_the_sqlite_boundary() {
        assert_eq!(
            decode_credential_kind("long_lived").unwrap(),
            CredentialKind::LongLived
        );
        assert_eq!(
            decode_credential_kind("bootstrap").unwrap(),
            CredentialKind::Bootstrap
        );
        assert!(decode_credential_kind("future_kind").is_err());
    }

    async fn insert_user(store: &SqliteHostRuntimeStore, id: &str) {
        sqlx::query(
            "INSERT INTO users (id, name, display_name, created_at, updated_at)
             VALUES (?, ?, ?, 1, 1)",
        )
        .bind(id)
        .bind(id)
        .bind(id)
        .execute(&store.pool)
        .await
        .unwrap();
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert_host(
        store: &SqliteHostRuntimeStore,
        id: &str,
        owner: Option<&str>,
        created_at: i64,
        revoked_at: Option<i64>,
        finalized_at: Option<i64>,
        kind: &str,
        provisioning_source: &str,
    ) {
        sqlx::query(
            "INSERT INTO hosts
                (id, name, kind, user_id, installation_id, last_seen_at, revoked_at,
                 created_at, display_label, activated_at, finalized_at, provisioning_source)
             VALUES (?, ?, ?, ?, NULL, NULL, ?, ?, NULL, NULL, ?, ?)",
        )
        .bind(id)
        .bind(id)
        .bind(kind)
        .bind(owner)
        .bind(revoked_at)
        .bind(created_at)
        .bind(finalized_at)
        .bind(provisioning_source)
        .execute(&store.pool)
        .await
        .unwrap();
    }

    fn ids(records: &[ComputerRecord]) -> Vec<&str> {
        records.iter().map(|record| record.id.as_str()).collect()
    }

    #[tokio::test]
    async fn the_owner_list_hides_only_the_unfinalized_invite_draft() {
        let (_directory, store) = fixture().await;
        insert_host(
            &store,
            "draft",
            Some("user-1"),
            1,
            None,
            None,
            "remote",
            "user_invite",
        )
        .await;
        insert_host(
            &store,
            "invited",
            Some("user-1"),
            2,
            None,
            Some(2),
            "remote",
            "user_invite",
        )
        .await;
        insert_host(
            &store,
            "installing",
            Some("user-1"),
            3,
            None,
            None,
            "local",
            "app_install",
        )
        .await;

        let listed = store
            .list_for_owner(UserId::parse("user-1").unwrap())
            .await
            .unwrap();
        assert_eq!(ids(&listed), ["invited", "installing"]);
    }

    #[tokio::test]
    async fn the_owner_list_puts_live_rows_first_then_oldest() {
        let (_directory, store) = fixture().await;
        insert_host(
            &store,
            "live-late",
            Some("user-1"),
            4,
            None,
            Some(4),
            "remote",
            "user_invite",
        )
        .await;
        insert_host(
            &store,
            "dead-early",
            Some("user-1"),
            1,
            Some(9),
            Some(1),
            "remote",
            "user_invite",
        )
        .await;
        insert_host(
            &store,
            "live-early",
            Some("user-1"),
            2,
            None,
            Some(2),
            "remote",
            "user_invite",
        )
        .await;
        insert_host(
            &store,
            "dead-late",
            Some("user-1"),
            3,
            Some(9),
            Some(3),
            "remote",
            "user_invite",
        )
        .await;

        let listed = store
            .list_for_owner(UserId::parse("user-1").unwrap())
            .await
            .unwrap();
        assert_eq!(
            ids(&listed),
            ["live-early", "live-late", "dead-early", "dead-late"]
        );
    }

    #[tokio::test]
    async fn the_owner_list_is_only_this_owners_rows() {
        let (_directory, store) = fixture().await;
        insert_user(&store, "user-2").await;
        insert_host(
            &store,
            "mine",
            Some("user-1"),
            1,
            None,
            Some(1),
            "remote",
            "user_invite",
        )
        .await;
        insert_host(
            &store,
            "theirs",
            Some("user-2"),
            1,
            None,
            Some(1),
            "remote",
            "user_invite",
        )
        .await;

        let listed = store
            .list_for_owner(UserId::parse("user-1").unwrap())
            .await
            .unwrap();
        assert_eq!(ids(&listed), ["mine"]);
    }

    #[tokio::test]
    async fn read_computer_returns_a_row_whoever_holds_it() {
        let (_directory, store) = fixture().await;
        insert_user(&store, "user-2").await;
        insert_host(
            &store,
            "draft",
            Some("user-1"),
            1,
            None,
            None,
            "remote",
            "user_invite",
        )
        .await;
        insert_host(
            &store,
            "theirs",
            Some("user-2"),
            2,
            None,
            Some(2),
            "remote",
            "user_invite",
        )
        .await;
        insert_host(&store, "unclaimed", None, 3, None, None, "local", "system").await;

        let draft = store
            .read_computer(HostId::parse("draft").unwrap())
            .await
            .unwrap()
            .expect("the unfinalized invite draft the bulk list hides is still readable");
        assert_eq!(draft.provisioning_source, ProvisioningSource::UserInvite);
        assert_eq!(draft.finalized_at, None);

        let theirs = store
            .read_computer(HostId::parse("theirs").unwrap())
            .await
            .unwrap()
            .expect("a foreign owner's row is a fact this face still returns");
        assert_eq!(theirs.owner, Some(UserId::parse("user-2").unwrap()));

        let unclaimed = store
            .read_computer(HostId::parse("unclaimed").unwrap())
            .await
            .unwrap()
            .expect("an unclaimed row exists even with no owner");
        assert_eq!(unclaimed.owner, None);
    }

    #[tokio::test]
    async fn read_computer_is_none_for_an_absent_id() {
        let (_directory, store) = fixture().await;
        assert!(
            store
                .read_computer(HostId::parse("nobody").unwrap())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert_credential(
        store: &SqliteHostRuntimeStore,
        id: &str,
        host_id: &str,
        kind: &str,
        expires_at: Option<i64>,
        consumed_at: Option<i64>,
        revoked_at: Option<i64>,
    ) {
        sqlx::query(
            "INSERT INTO host_credentials
                (id, host_id, token_hash, kind, expires_at, consumed_at, revoked_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(host_id)
        .bind(format!("hash-{id}"))
        .bind(kind)
        .bind(expires_at)
        .bind(consumed_at)
        .bind(revoked_at)
        .execute(&store.pool)
        .await
        .unwrap();
    }

    /// (revoked_at, consumed_at) for one credential, the two columns every sweep touches.
    async fn credential_state(
        store: &SqliteHostRuntimeStore,
        id: &str,
    ) -> (Option<i64>, Option<i64>) {
        let row = sqlx::query("SELECT revoked_at, consumed_at FROM host_credentials WHERE id = ?")
            .bind(id)
            .fetch_one(&store.pool)
            .await
            .unwrap();
        (row.get("revoked_at"), row.get("consumed_at"))
    }

    /// An admitted revoke for an app-managed computer — the only shape `admit` accepts,
    /// carrying the id the write stamps and nothing a stale record could redirect.
    fn revoke_attempt(id: &str, owner: &str, at: i64) -> RevokeComputerAttempt {
        let owner_id = UserId::parse(owner).unwrap();
        let record = ComputerRecord {
            id: HostId::parse(id).unwrap(),
            name: id.to_owned(),
            kind: HostKind::Remote,
            created_at: 1,
            owner: Some(owner_id.clone()),
            installation_id: Some("i1".to_owned()),
            last_seen_at: Some(1),
            activated_at: Some(1),
            finalized_at: Some(1),
            revoked_at: None,
            provisioning_source: ProvisioningSource::AppInstall,
            display_label: None,
        };
        let owned =
            find_owned_computer(&owner_id, Some(record)).expect("the owner is admitted the row");
        AdmittedRevoke::admit(owned)
            .expect("an app-managed computer is revocable")
            .stamp(at)
    }

    /// Walks the source chain because the driver error is handed over as
    /// [`PersistenceFailure`]'s source (code 5 == SQLITE_BUSY).
    fn is_sqlite_busy(error: &(dyn std::error::Error + 'static)) -> bool {
        let mut current: Option<&(dyn std::error::Error + 'static)> = Some(error);
        while let Some(err) = current {
            if let Some(sqlx_error) = err.downcast_ref::<sqlx::Error>()
                && sqlx_error
                    .as_database_error()
                    .and_then(|db| db.code().map(|code| code.into_owned()))
                    .as_deref()
                    == Some("5")
            {
                return true;
            }
            current = err.source();
        }
        false
    }

    async fn empty_store_with_short_busy_timeout() -> (TempDir, SqliteHostRuntimeStore) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("host.sqlite");
        crate::current_schema::create_current_schema(&path)
            .await
            .close()
            .await
            .unwrap();
        let store =
            SqliteHostRuntimeStore::open_with_busy_timeout(&path, Duration::from_millis(50))
                .await
                .unwrap();
        (directory, store)
    }

    #[tokio::test]
    async fn revoke_stamps_the_tombstone_once_and_sweeps_credentials() {
        let (_directory, store) = fixture().await;
        insert_host(
            &store,
            "h1",
            Some("user-1"),
            1,
            None,
            Some(1),
            "remote",
            "app_install",
        )
        .await;
        insert_credential(&store, "c1", "h1", "long_lived", None, None, None).await;

        let revoked = store
            .revoke_computer(revoke_attempt("h1", "user-1", 900))
            .await
            .unwrap()
            .expect("the admitted row is under the lock");

        assert!(revoked.custody_ended_now);
        assert_eq!(revoked.computer.revoked_at, Some(900));
        // revoked_at only: the sweep leaves consumed_at NULL so the verifier never reads
        // a dead credential as one inside a grace window.
        assert_eq!(credential_state(&store, "c1").await, (Some(900), None));
    }

    #[tokio::test]
    async fn a_repeat_revoke_keeps_the_first_tombstone_but_still_sweeps() {
        let (_directory, store) = fixture().await;
        insert_host(
            &store,
            "h1",
            Some("user-1"),
            1,
            Some(100),
            Some(1),
            "remote",
            "app_install",
        )
        .await;
        // A live credential on an already-revoked computer — the stray token the repeat
        // revoke exists to reconcile.
        insert_credential(&store, "stray", "h1", "long_lived", None, None, None).await;

        let revoked = store
            .revoke_computer(revoke_attempt("h1", "user-1", 900))
            .await
            .unwrap()
            .expect("a repeat revoke is an answer, not an absence");

        assert!(!revoked.custody_ended_now);
        assert_eq!(revoked.computer.revoked_at, Some(100));
        assert_eq!(credential_state(&store, "stray").await, (Some(900), None));
    }

    #[tokio::test]
    async fn revoke_of_a_vanished_row_is_none() {
        let (_directory, store) = fixture().await;
        assert!(
            store
                .revoke_computer(revoke_attempt("ghost", "user-1", 900))
                .await
                .unwrap()
                .is_none()
        );
    }

    /// Falsifier: change `begin_root_write` to `BEGIN DEFERRED` and this turns RED — a
    /// deferred begin takes no writer lock, so the contended revoke proceeds instead of
    /// failing SQLITE_BUSY.
    #[tokio::test]
    async fn revoke_reserves_the_writer_before_it_touches_a_row() {
        let (_directory, store) = empty_store_with_short_busy_timeout().await;
        let held = store.begin_root_write().await.unwrap();

        let contended = store
            .revoke_computer(revoke_attempt("h1", "user-1", 900))
            .await
            .expect_err("a second writer cannot proceed while one is held");
        assert!(
            is_sqlite_busy(&contended),
            "revoke did not reserve the writer before its first statement"
        );
        held.rollback().await.unwrap();
    }

    #[tokio::test]
    async fn issue_inserts_a_non_expiring_long_lived_credential() {
        let (_directory, store) = fixture().await;
        insert_host(
            &store,
            "h1",
            Some("user-1"),
            1,
            None,
            Some(1),
            "remote",
            "app_install",
        )
        .await;
        let credential = mint_credential(&OsEntropy, "h1").unwrap().into_parts().0;
        let id = credential.id().to_owned();

        store
            .issue_credential(credential, None)
            .await
            .unwrap()
            .expect("an eligible host receives its credential");

        let row = sqlx::query(
            "SELECT kind, expires_at, consumed_at, revoked_at FROM host_credentials WHERE id = ?",
        )
        .bind(&id)
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!(row.get::<String, _>("kind"), "long_lived");
        for column in ["expires_at", "consumed_at", "revoked_at"] {
            assert_eq!(row.get::<Option<i64>, _>(column), None, "column {column}");
        }
    }

    #[tokio::test]
    async fn issue_refuses_under_the_lock_and_inserts_nothing() {
        let (_directory, store) = fixture().await;
        insert_host(
            &store,
            "unbound",
            None,
            1,
            None,
            Some(1),
            "remote",
            "app_install",
        )
        .await;
        insert_host(
            &store,
            "revoked",
            Some("user-1"),
            1,
            Some(5),
            Some(1),
            "remote",
            "app_install",
        )
        .await;

        for (host, expected) in [
            ("gone", CredentialIssueRefusal::HostMissing),
            ("unbound", CredentialIssueRefusal::HostUnbound),
            ("revoked", CredentialIssueRefusal::HostRevoked),
        ] {
            let credential = mint_credential(&OsEntropy, host).unwrap().into_parts().0;
            let refusal = store
                .issue_credential(credential, None)
                .await
                .unwrap()
                .expect_err("this host cannot receive a credential");
            assert_eq!(refusal, expected, "host {host}");
        }

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM host_credentials")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(count, 0, "a refused issue inserts nothing");
    }

    #[tokio::test]
    async fn the_sweep_supersedes_other_long_lived_tokens_and_keeps_the_new_one() {
        let (_directory, store) = fixture().await;
        insert_host(
            &store,
            "h1",
            Some("user-1"),
            1,
            None,
            Some(1),
            "remote",
            "app_install",
        )
        .await;
        insert_credential(&store, "old", "h1", "long_lived", None, None, None).await;
        insert_credential(&store, "boot", "h1", "bootstrap", None, None, None).await;

        let credential = mint_credential(&OsEntropy, "h1").unwrap().into_parts().0;
        let new_id = credential.id().to_owned();
        store
            .issue_credential(credential, Some(CredentialSweep::after_grace(1_000)))
            .await
            .unwrap()
            .expect("eligible");

        // Superseded now (consumed_at = issued), dead one grace window later.
        assert_eq!(
            credential_state(&store, "old").await,
            (Some(61_000), Some(1_000))
        );
        // The token just issued is kept, and a bootstrap credential is not this sweep's.
        assert_eq!(credential_state(&store, &new_id).await, (None, None));
        assert_eq!(credential_state(&store, "boot").await, (None, None));
    }

    #[tokio::test]
    async fn the_sweep_never_overwrites_an_earlier_consumed_at() {
        let (_directory, store) = fixture().await;
        insert_host(
            &store,
            "h1",
            Some("user-1"),
            1,
            None,
            Some(1),
            "remote",
            "app_install",
        )
        .await;
        // A credential superseded once (consumed_at set) but never given a death time.
        // This half-swept shape — NOT a genuine grace row, which carries a future
        // revoked_at and would be excluded by the sweep's `revoked_at IS NULL` filter — is
        // the only state the COALESCE guard can act on.
        insert_credential(
            &store,
            "half_swept",
            "h1",
            "long_lived",
            None,
            Some(50),
            None,
        )
        .await;

        let credential = mint_credential(&OsEntropy, "h1").unwrap().into_parts().0;
        store
            .issue_credential(credential, Some(CredentialSweep::immediately(900)))
            .await
            .unwrap()
            .expect("eligible");

        // revoked_at advances to the sweep instant; consumed_at keeps its first value.
        assert_eq!(
            credential_state(&store, "half_swept").await,
            (Some(900), Some(50))
        );
    }

    /// Falsifier: change `begin_root_write` to `BEGIN DEFERRED` and this turns RED — the
    /// eligibility read would proceed while a writer is held instead of failing BUSY,
    /// which is what makes the read and the insert it guards one indivisible decision.
    #[tokio::test]
    async fn issue_reserves_the_writer_before_it_reads_eligibility() {
        let (_directory, store) = empty_store_with_short_busy_timeout().await;
        let held = store.begin_root_write().await.unwrap();

        let credential = mint_credential(&OsEntropy, "h1").unwrap().into_parts().0;
        let contended = store
            .issue_credential(credential, None)
            .await
            .expect_err("eligibility cannot be read while a writer is held");
        assert!(
            is_sqlite_busy(&contended),
            "issue read eligibility outside the writer reservation"
        );
        held.rollback().await.unwrap();
    }

    async fn active(store: &SqliteHostRuntimeStore, id: &str, now: i64) -> bool {
        store
            .has_active_long_lived_credential(HostId::parse(id).unwrap(), now)
            .await
            .unwrap()
    }

    /// A live, user-1-owned, app-installed host — the shape whose only remaining
    /// variable is the credential attached to it.
    async fn live_owned_host(store: &SqliteHostRuntimeStore, id: &str) {
        insert_host(
            store,
            id,
            Some("user-1"),
            1,
            None,
            Some(1),
            "remote",
            "app_install",
        )
        .await;
    }

    #[tokio::test]
    async fn the_active_credential_hint_excludes_every_unusable_shape() {
        let (_directory, store) = fixture().await;

        live_owned_host(&store, "good").await;
        insert_credential(&store, "good-c", "good", "long_lived", None, None, None).await;

        live_owned_host(&store, "bootstrap").await;
        insert_credential(&store, "boot-c", "bootstrap", "bootstrap", None, None, None).await;
        live_owned_host(&store, "consumed").await;
        insert_credential(
            &store,
            "cons-c",
            "consumed",
            "long_lived",
            None,
            Some(50),
            None,
        )
        .await;
        live_owned_host(&store, "revoked-cred").await;
        insert_credential(
            &store,
            "rev-c",
            "revoked-cred",
            "long_lived",
            None,
            None,
            Some(50),
        )
        .await;
        live_owned_host(&store, "expired").await;
        insert_credential(
            &store,
            "exp-c",
            "expired",
            "long_lived",
            Some(100),
            None,
            None,
        )
        .await;

        // A usable credential, but the computer it belongs to cannot own one.
        insert_host(
            &store,
            "dead-host",
            Some("user-1"),
            1,
            Some(5),
            Some(1),
            "remote",
            "app_install",
        )
        .await;
        insert_credential(
            &store,
            "dead-c",
            "dead-host",
            "long_lived",
            None,
            None,
            None,
        )
        .await;
        insert_host(
            &store,
            "unbound-host",
            None,
            1,
            None,
            Some(1),
            "remote",
            "app_install",
        )
        .await;
        insert_credential(
            &store,
            "unbound-c",
            "unbound-host",
            "long_lived",
            None,
            None,
            None,
        )
        .await;

        assert!(active(&store, "good", 100).await);
        for dead in [
            "bootstrap",
            "consumed",
            "revoked-cred",
            "expired",
            "dead-host",
            "unbound-host",
        ] {
            assert!(
                !active(&store, dead, 100).await,
                "host {dead} must not count"
            );
        }
    }

    const INSTALLATION: &str = "11111111-2222-3333-4444-555555555555";
    const INSTALLATION_PREFIX: &str = "11111111";
    const NOW: i64 = 1_700_000_000_123;

    fn owner() -> UserId {
        UserId::parse("user-1").unwrap()
    }

    /// Entropy that hands back a fixed byte sequence, so a registered computer's id and
    /// the credential the claim then mints are the same on every run.
    struct FixedEntropy(Mutex<VecDeque<u8>>);

    impl FixedEntropy {
        fn bytes(count: usize) -> Self {
            Self(Mutex::new((0..count).map(|index| index as u8).collect()))
        }
    }

    impl EntropySource for FixedEntropy {
        type Error = Infallible;
        fn fill(&self, destination: &mut [u8]) -> Result<(), Self::Error> {
            let mut bytes = self.0.lock().unwrap();
            for slot in destination {
                *slot = bytes.pop_front().unwrap();
            }
            Ok(())
        }
    }

    struct FixedClock;

    impl Clock for FixedClock {
        type Error = Infallible;
        fn unix_time_ms(&self) -> Result<i64, Self::Error> {
            Ok(NOW)
        }
    }

    /// The same store plays all three write faces a claim uses: the local-claim unit of
    /// work, the credential inventory hint, and the credential issuer `finish` calls.
    fn claim_service(
        store: SqliteHostRuntimeStore,
    ) -> ClaimLocalComputer<
        SqliteHostRuntimeStore,
        SqliteHostRuntimeStore,
        SqliteHostRuntimeStore,
        FixedEntropy,
        FixedClock,
    > {
        ClaimLocalComputer::new(
            store.clone(),
            store.clone(),
            store,
            FixedEntropy::bytes(1024),
            FixedClock,
        )
    }

    fn claim_request(label: Option<&str>, current: Option<&str>) -> LocalClaimRequest {
        LocalClaimRequest {
            installation_id: INSTALLATION.to_owned(),
            label: label.map(|label| ComputerDisplayLabel::parse(label).unwrap()),
            current_computer: current.map(|current| HostId::parse(current).unwrap()),
        }
    }

    /// A host seeded with every column a claim branch reads or preserves, so a test can
    /// stand up a placeholder or a name-squatter with the exact stamps under test.
    #[allow(clippy::too_many_arguments)]
    async fn seed_host(
        store: &SqliteHostRuntimeStore,
        id: &str,
        name: &str,
        kind: &str,
        owner: Option<&str>,
        installation: Option<&str>,
        last_seen_at: Option<i64>,
        display_label: Option<&str>,
        activated_at: Option<i64>,
        finalized_at: Option<i64>,
        provisioning_source: &str,
    ) {
        sqlx::query(
            "INSERT INTO hosts
                (id, name, kind, user_id, installation_id, last_seen_at, revoked_at,
                 created_at, display_label, activated_at, finalized_at, provisioning_source)
             VALUES (?, ?, ?, ?, ?, ?, NULL, 1, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(name)
        .bind(kind)
        .bind(owner)
        .bind(installation)
        .bind(last_seen_at)
        .bind(display_label)
        .bind(activated_at)
        .bind(finalized_at)
        .bind(provisioning_source)
        .execute(&store.pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn a_reused_computer_moves_only_its_last_seen_stamp() {
        let (_directory, store) = fixture().await;
        seed_host(
            &store,
            "existing",
            "shiki-mac",
            "local",
            Some("user-1"),
            Some(INSTALLATION),
            Some(10),
            Some("Shiki's Mac"),
            Some(20),
            Some(30),
            "app_install",
        )
        .await;

        let claimed = claim_service(store.clone())
            .execute(owner(), claim_request(Some("A Brand New Name"), None))
            .await
            .expect("the existing computer is reused");

        let computer = &claimed.computer;
        assert_eq!(computer.id.as_str(), "existing");
        assert_eq!(computer.last_seen_at, Some(NOW));
        // Nothing else moved: reuse is a heartbeat, not a re-decision of custody, stamps
        // or the name the machine already carries.
        assert_eq!(computer.activated_at, Some(20));
        assert_eq!(computer.finalized_at, Some(30));
        assert_eq!(computer.name, "shiki-mac");
        assert_eq!(computer.display_label.as_deref(), Some("Shiki's Mac"));
    }

    #[tokio::test]
    async fn an_unclaimed_placeholder_is_adopted_and_finishes_activation() {
        let (_directory, store) = fixture().await;
        // A placeholder that already carries an activation stamp but no finalization.
        seed_host(
            &store,
            "placeholder",
            "local",
            "local",
            None,
            None,
            None,
            None,
            Some(20),
            None,
            "system",
        )
        .await;

        let claimed = claim_service(store.clone())
            .execute(owner(), claim_request(Some("  Shiki's Mac  "), None))
            .await
            .expect("the placeholder is adopted");

        let computer = &claimed.computer;
        assert_eq!(computer.id.as_str(), "placeholder");
        assert_eq!(computer.owner, Some(owner()));
        assert_eq!(computer.installation_id.as_deref(), Some(INSTALLATION));
        assert_eq!(computer.last_seen_at, Some(NOW));
        // COALESCE keeps the stamp the placeholder had...
        assert_eq!(computer.activated_at, Some(20));
        // ...and fills the one it lacked with this claim's instant.
        assert_eq!(computer.finalized_at, Some(NOW));
        assert_eq!(computer.provisioning_source, ProvisioningSource::AppInstall);
        // The supplied label is stored, trimmed at the boundary by `ComputerDisplayLabel`.
        assert_eq!(computer.display_label.as_deref(), Some("Shiki's Mac"));
    }

    #[tokio::test]
    async fn a_reclaim_without_a_label_leaves_the_stored_label_alone() {
        let (_directory, store) = fixture().await;
        seed_host(
            &store,
            "placeholder",
            "local",
            "local",
            None,
            None,
            None,
            Some("Chosen Name"),
            None,
            None,
            "system",
        )
        .await;

        let claimed = claim_service(store.clone())
            .execute(owner(), claim_request(None, None))
            .await
            .expect("the placeholder is adopted");

        assert_eq!(claimed.computer.owner, Some(owner()));
        // No label was sent, so the name the user chose earlier is untouched.
        assert_eq!(
            claimed.computer.display_label.as_deref(),
            Some("Chosen Name")
        );
    }

    #[tokio::test]
    async fn a_first_claim_registers_a_normalized_local_computer() {
        let (_directory, store) = fixture().await;

        let claimed = claim_service(store.clone())
            .execute(owner(), claim_request(Some("Shiki's MacBook Pro!"), None))
            .await
            .expect("a computer is registered");

        let computer = &claimed.computer;
        assert_eq!(computer.id.as_str(), "AAECAwQFBgcICQoL");
        assert_eq!(computer.name, "shiki-s-macbook-pro");
        assert_eq!(computer.kind, HostKind::Local);
        assert_eq!(computer.owner, Some(owner()));
        assert_eq!(computer.installation_id.as_deref(), Some(INSTALLATION));
        assert_eq!(computer.provisioning_source, ProvisioningSource::AppInstall);
        assert_eq!(
            computer.display_label.as_deref(),
            Some("Shiki's MacBook Pro!")
        );
        assert_eq!(computer.revoked_at, None);
        // Creation, activation, finalization and last-seen are one instant: a computer is
        // not seen before it exists.
        assert_eq!(computer.created_at, NOW);
        assert_eq!(computer.last_seen_at, Some(NOW));
        assert_eq!(computer.activated_at, Some(NOW));
        assert_eq!(computer.finalized_at, Some(NOW));
    }

    #[tokio::test]
    async fn a_taken_first_name_falls_through_to_the_disambiguated_candidate() {
        let (_directory, store) = fixture().await;
        // Any row already holding the first-choice name blocks it — the uniqueness is
        // over the whole table, whoever the row belongs to.
        seed_host(
            &store,
            "squatter",
            "shiki-s-macbook-pro",
            "remote",
            None,
            None,
            None,
            None,
            None,
            None,
            "system",
        )
        .await;

        let claimed = claim_service(store.clone())
            .execute(owner(), claim_request(Some("Shiki's MacBook Pro!"), None))
            .await
            .expect("the second candidate is free");

        // The store reported the first name taken, so the ladder registered the next.
        assert_eq!(
            claimed.computer.name,
            format!("shiki-s-macbook-pro-{INSTALLATION_PREFIX}")
        );
    }

    /// Falsifier: change `begin_root_write` to `BEGIN DEFERRED` and this turns RED — a
    /// deferred begin takes no writer lock, so the contended claim reads its facts
    /// instead of failing SQLITE_BUSY, which is what makes the two reads and the write
    /// they condition one indivisible decision.
    #[tokio::test]
    async fn claim_reserves_the_writer_before_it_reads_its_facts() {
        let (_directory, store) = empty_store_with_short_busy_timeout().await;
        let held = store.begin_root_write().await.unwrap();

        let Err(contended) = claim_service(store.clone())
            .execute(owner(), claim_request(None, None))
            .await
        else {
            panic!("a second writer cannot proceed while one is held");
        };
        assert!(
            is_sqlite_busy(&contended),
            "claim read its facts outside the writer reservation"
        );
        held.rollback().await.unwrap();
    }
}
