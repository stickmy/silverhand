//! Startup-only database authority. Shared SQL is embedded at build time; no
//! runtime Node, Cargo, TS fallback, or second migration ledger.
use chrono::Utc;
use serde::Deserialize;
use sqlx::{
    Connection, Executor, Row, SqliteConnection,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteLockingMode, SqliteSynchronous},
};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use thiserror::Error;

mod preconditions;
#[cfg(test)]
mod tests;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Plan {
    minimum_supported_step: i64,
    one_to_one_ceiling: i64,
    epochs: Vec<Epoch>,
    steps: Vec<Step>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Epoch {
    epoch: i64,
    last_step: i64,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Step {
    version: i64,
    name: String,
    up: String,
    foreign_key_escape: bool,
    precondition: bool,
}
fn plan() -> Plan {
    serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../packages/shared/src/db/rust-migrations.generated.json"
    )))
    .expect("build-verified shared migration projection")
}
fn now() -> chrono::DateTime<Utc> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before epoch");
    chrono::DateTime::from_timestamp(elapsed.as_secs() as i64, elapsed.subsec_nanos())
        .expect("system clock in chrono range")
}
impl Plan {
    fn epoch(&self, step: i64) -> i64 {
        if step <= self.one_to_one_ceiling {
            return step.max(0);
        }
        self.epochs
            .iter()
            .filter(|e| step >= e.last_step)
            .map(|e| e.epoch)
            .next_back()
            .unwrap_or(self.one_to_one_ceiling)
    }
}

#[derive(Debug, Error)]
pub enum DatabaseStartupError {
    #[error("schema_too_old: database step {0} is below the supported baseline")]
    TooOld(i64),
    #[error("schema_too_new: database step {0} is newer than this binary")]
    TooNew(i64),
    #[error("schema_ledger_inconsistent: migration ledger is not a prefix of the shared plan")]
    Ledger,
    #[error(
        "schema_upgrade_disabled: step {from} -> {to}; set SILVERHAND_MIGRATE=1 to back up and upgrade"
    )]
    UpgradeDisabled { from: i64, to: i64 },
    #[error("migration_backup_failed")]
    Backup(#[source] std::io::Error),
    #[error("database startup filesystem operation failed")]
    Io(#[from] std::io::Error),
    #[error("database startup SQL failed")]
    Sql(#[from] sqlx::Error),
    #[error("migration precondition/integrity failure: {0}")]
    Integrity(String),
}

#[derive(Debug)]
pub struct DatabasePreparation {
    pub before_step: i64,
    pub after_step: i64,
    pub before_epoch: i64,
    pub after_epoch: i64,
    pub applied_steps: Vec<i64>,
    pub backup_path: Option<PathBuf>,
}

pub async fn prepare_database(
    path: &Path,
    allow_upgrade: bool,
) -> Result<DatabasePreparation, DatabaseStartupError> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    fs::create_dir_all(
        path.parent()
            .ok_or_else(|| std::io::Error::other("missing data directory"))?,
    )?;
    let options = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true)
        // Retain the database file lock between step commits and during backup.
        // This connection closes before runtime pools; process death releases its
        // OS locks without stale-directory deletion or PID ownership guessing.
        .locking_mode(SqliteLockingMode::Exclusive)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Full)
        .foreign_keys(true)
        .busy_timeout(Duration::ZERO);
    let mut db = SqliteConnection::connect_with(&options).await?;
    let result = async {
        db.execute("BEGIN EXCLUSIVE; COMMIT;").await?;
        prepare_connected(&mut db, &path, allow_upgrade, &plan()).await
    }
    .await;
    let closed = db.close().await;
    match result {
        Ok(value) => {
            closed?;
            Ok(value)
        }
        Err(error) => {
            if let Err(close) = closed {
                tracing::error!(%close, "database close after startup failure");
            }
            Err(error)
        }
    }
}

async fn prepare_connected(
    db: &mut SqliteConnection,
    path: &Path,
    allow: bool,
    plan: &Plan,
) -> Result<DatabasePreparation, DatabaseStartupError> {
    let ledger_exists: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_schema WHERE type='table' AND name='_migrations'",
    )
    .fetch_one(&mut *db)
    .await?;
    let versions: Vec<i64> = if ledger_exists == 0 {
        vec![]
    } else {
        sqlx::query_scalar("SELECT version FROM _migrations ORDER BY version")
            .fetch_all(&mut *db)
            .await?
    };
    let before = versions.iter().copied().max().unwrap_or(0);
    let current = plan.steps.last().expect("nonempty plan").version;
    if before > current {
        return Err(DatabaseStartupError::TooNew(before));
    }
    let populated: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_schema WHERE type IN ('table','index','trigger','view') AND name NOT LIKE 'sqlite_%' AND name!='_migrations'").fetch_one(&mut *db).await?;
    if !(before == 0 && populated == 0) && before < plan.minimum_supported_step {
        return Err(DatabaseStartupError::TooOld(before));
    }
    let floor = plan.steps[0].version;
    let actual: Vec<_> = versions.iter().copied().filter(|v| *v >= floor).collect();
    let prefix: Vec<_> = plan
        .steps
        .iter()
        .filter(|s| s.version <= before)
        .map(|s| s.version)
        .collect();
    if actual != prefix {
        return Err(DatabaseStartupError::Ledger);
    }
    let pending: Vec<_> = plan.steps.iter().filter(|s| s.version > before).collect();
    if before > 0 && !pending.is_empty() && !allow {
        return Err(DatabaseStartupError::UpgradeDisabled {
            from: before,
            to: current,
        });
    }
    db.execute("CREATE TABLE IF NOT EXISTS _migrations(version INTEGER PRIMARY KEY,name TEXT NOT NULL,applied_at INTEGER NOT NULL)").await?;
    let backup_path = if before > 0 && !pending.is_empty() {
        Some(backup(db, path).await?)
    } else {
        None
    };
    let mut applied_steps = vec![];
    for step in pending {
        if step.foreign_key_escape {
            db.execute("PRAGMA foreign_keys=OFF; PRAGMA legacy_alter_table=ON")
                .await?;
        }
        let applied = apply_step(db, step).await;
        if step.foreign_key_escape {
            db.execute("PRAGMA legacy_alter_table=OFF; PRAGMA foreign_keys=ON")
                .await?;
        }
        applied?;
        applied_steps.push(step.version);
    }
    if applied_steps.is_empty() {
        db.begin_with("BEGIN EXCLUSIVE").await?.commit().await?;
    }
    preconditions::audit_addresses(db, true, true).await?;
    Ok(DatabasePreparation {
        before_step: before,
        after_step: current,
        before_epoch: plan.epoch(before),
        after_epoch: plan.epoch(current),
        applied_steps,
        backup_path,
    })
}

async fn apply_step(db: &mut SqliteConnection, step: &Step) -> Result<(), DatabaseStartupError> {
    let mut tx = db.begin_with("BEGIN EXCLUSIVE").await?;
    if step.precondition {
        preconditions::run(&mut tx, step.version).await?;
    }
    sqlx::raw_sql(sqlx::AssertSqlSafe(step.up.as_str()))
        .execute(&mut *tx)
        .await?;
    if step.foreign_key_escape {
        let violations = sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(&mut *tx)
            .await?;
        if !violations.is_empty() {
            return Err(DatabaseStartupError::Integrity(format!(
                "step {} foreign_key_check: {} violations",
                step.version,
                violations.len()
            )));
        }
    }
    sqlx::query("INSERT INTO _migrations(version,name,applied_at) VALUES(?,?,?)")
        .bind(step.version)
        .bind(&step.name)
        .bind(now().timestamp_millis())
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

async fn backup(db: &mut SqliteConnection, path: &Path) -> Result<PathBuf, DatabaseStartupError> {
    let checkpoint = sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .fetch_one(&mut *db)
        .await?;
    let busy: i64 = checkpoint.try_get(0)?;
    if busy != 0 {
        return Err(DatabaseStartupError::Backup(std::io::Error::other(
            "WAL checkpoint busy; no backup or migration attempted",
        )));
    }
    let dir = path.parent().expect("validated parent");
    let dest = dir.with_file_name(format!(
        "{}-backup-{}",
        dir.file_name().unwrap().to_string_lossy(),
        now().format("%Y%m%dT%H%M%S%.9fZ")
    ));
    let mut partial_name = dest.as_os_str().to_owned();
    partial_name.push(".partial");
    let partial = PathBuf::from(partial_name);
    let name = path.file_name().expect("database filename");
    // Startup backs up only SQLite. Attachment backups are an explicit deployment
    // operation, independent of where the database happens to live.
    fs::create_dir(&partial).map_err(DatabaseStartupError::Backup)?;
    fs::set_permissions(&partial, fs::metadata(dir)?.permissions())
        .map_err(DatabaseStartupError::Backup)?;
    let output = partial.join(name);
    // Never open/close the source file outside SQLite while it owns POSIX locks.
    // The output is a standalone consistent database, without source WAL/SHM.
    sqlx::query("VACUUM INTO ?")
        .bind(
            output
                .to_str()
                .ok_or_else(|| std::io::Error::other("backup path is not UTF-8"))?,
        )
        .execute(&mut *db)
        .await?;
    fs::set_permissions(&output, fs::metadata(path)?.permissions())?;
    fs::rename(&partial, &dest).map_err(DatabaseStartupError::Backup)?;
    Ok(dest)
}
