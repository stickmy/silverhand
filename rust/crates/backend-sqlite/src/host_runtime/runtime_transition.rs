//! The durable runtime-transition lock: acquire, release, and commit-and-release,
//! each a compare-and-set under the same root-write reservation the other axes use.
//!
//! This adapter WITNESSES; it never mints the domain verdict. Each write projects the
//! row it actually touched from its own `RETURNING` clause — the acquired lock's
//! `id`/`target`/`started_at`, or the committed after-image's every column — and the
//! core free functions
//! ([`begin_runtime_transition`](silverhand_backend_core::host_runtime::runtime_transition::begin_runtime_transition),
//! [`complete_runtime_transition`](silverhand_backend_core::host_runtime::runtime_transition::complete_runtime_transition))
//! bind the subject and seal it against the request. The adapter cannot claim success
//! for a row it did not touch: no `RETURNING` row means no witness.
//!
//! `begin` is a pre-read then a compare-and-set that turns a null lock into a target;
//! the `WHERE` re-states every precondition the pre-read observed, so a row that moved
//! out from under the lock changes nothing and surfaces as a lost serialized CAS rather
//! than a silent no-op. `abort` and `complete` are keyed on the ownership token minted
//! at `begin`: the guard is the token's target AND its `started_at`, so a row a *new*
//! attempt reacquired at a *different* instant (same target, fresh `started_at`) matches
//! nothing for the *old* token — a different-started-at fence, not a full ABA defense
//! (a same-millisecond reacquire is out of scope, as `started_at` is a wall clock).
//! `complete` writes the `runtime` + tuning and clears the lock in the SAME statement,
//! so the ownership guard gates the tuning write itself. The committed `runtime` is bound
//! from the token's own `target`, the same value the `WHERE` guards on, so the SET and
//! the guard can never name two different runtimes.
//!
//! The `(runtime_transition_target, runtime_transition_started_at)` pair is set and
//! cleared together in every statement, satisfying the schema's paired CHECK and its
//! insert/update triggers.

use silverhand_backend_core::{
    foundation::identity::{AgentId, ConversationId},
    foundation::persistence::PersistenceFailure,
    host_runtime::runtime_credential::RuntimeScope,
    host_runtime::runtime_event::RuntimeObservedStatus,
    host_runtime::runtime_transition::{
        AbortTransition, BeginTransitionRecord, CompleteTransitionRecord,
        RuntimeTransitionAfterImage, RuntimeTransitionCommit, RuntimeTransitionInboxReclaimRecord,
        RuntimeTransitionInboxReclaimRow, RuntimeTransitionInboxWrite, RuntimeTransitionLock,
        RuntimeTransitionLockRow, RuntimeTransitionSeatRead, RuntimeTransitionSeatRow,
        RuntimeTransitionSeatSnapshotRecord, RuntimeTransitionSeatSnapshotRow,
        RuntimeTransitionWrite,
    },
    identity_access::agents::runtime::RuntimeKind,
};
use sqlx::{Row, sqlite::SqliteRow};
use thiserror::Error;

use crate::foundation::host_agent_store::{SqliteHostAgentStore, decode};

/// The pre-read `begin` decides from: enough to answer `NotFound` / `Deleting` /
/// `InProgress` before the acquiring compare-and-set runs.
const SELECT_BEGIN_PRECHECK: &str =
    "SELECT deletion_started_at, runtime_transition_target FROM agents WHERE id = ?";

/// Acquire: a null lock becomes `target` with its paired started-at. The `WHERE`
/// re-states the preconditions the pre-read observed as the serialized compare-and-set,
/// and `RETURNING` hands back the row the write touched so core can prove the subject.
/// It does NOT bump `updated_at` — the runtime has not changed yet, matching the
/// TypeScript `beginRuntimeTransition`.
const BEGIN_TRANSITION_CAS: &str = "UPDATE agents \
        SET runtime_transition_target = ?, runtime_transition_started_at = ? \
      WHERE id = ? \
        AND runtime_transition_target IS NULL \
        AND deletion_started_at IS NULL \
    RETURNING id, runtime_transition_target, runtime_transition_started_at";

/// Release a lock the caller owns, keyed on BOTH its target and its started-at so a
/// reacquired row (fresh started-at) is not the caller's to release.
const ABORT_TRANSITION_CAS: &str = "UPDATE agents \
        SET runtime_transition_target = NULL, runtime_transition_started_at = NULL \
      WHERE id = ? \
        AND runtime_transition_target = ? \
        AND runtime_transition_started_at = ?";

/// Commit runtime + tuning and release the lock, guarded by the owned target AND
/// started-at, returning the after-image core seals field by field. The `runtime` set and
/// the target guarded are bound from the SAME lock-sourced string, so the pair CHECK, the
/// ownership guard, and the committed runtime cannot disagree.
const COMPLETE_TRANSITION_CAS: &str = "UPDATE agents \
        SET runtime = ?, model = ?, reasoning_effort = ?, fast_mode = ?, \
            runtime_transition_target = NULL, runtime_transition_started_at = NULL, \
            updated_at = ? \
      WHERE id = ? \
        AND runtime_transition_target = ? \
        AND runtime_transition_started_at = ? \
    RETURNING id, runtime, model, reasoning_effort, fast_mode, updated_at, \
              runtime_transition_target, runtime_transition_started_at";

const SELECT_HELD_TRANSITION: &str = "SELECT id, runtime_transition_target, \
        runtime_transition_started_at FROM agents WHERE id = ?";

const RECLAIM_INBOX_LEASES: &str = "UPDATE conversation_inboxes \
        SET in_flight_owner = NULL, in_flight_at = NULL \
      WHERE agent_id = ? \
        AND in_flight_owner IS NOT NULL \
        AND EXISTS ( \
            SELECT 1 FROM agents \
             WHERE id = ? \
               AND runtime_transition_target = ? \
               AND runtime_transition_started_at = ? \
        ) \
    RETURNING conversation_id";

/// The writer was reserved and every precondition the pre-read observed still held, yet
/// the acquiring compare-and-set returned no row. Under `BEGIN IMMEDIATE` nothing else
/// can have written the row, so this is a broken invariant, not a product answer.
#[derive(Debug, Error)]
#[error("begin runtime transition lost its serialized compare-and-set")]
struct BeginTransitionCasLost;

/// What the pre-read observed of the lock columns.
struct BeginPrecheck {
    deletion_started_at: Option<i64>,
    runtime_transition_target: Option<String>,
}

fn begin_precheck(row: &SqliteRow) -> Result<BeginPrecheck, sqlx::Error> {
    Ok(BeginPrecheck {
        deletion_started_at: row.try_get("deletion_started_at")?,
        runtime_transition_target: row.try_get::<Option<String>, _>("runtime_transition_target")?,
    })
}

/// The stored `fast_mode` boolean as its `0`/`1`/null column, matching the
/// `fast_mode === null ? null : fast_mode ? 1 : 0` the TypeScript writer applies.
fn fast_mode_column(fast_mode: Option<bool>) -> Option<i64> {
    fast_mode.map(|on| if on { 1 } else { 0 })
}

/// Project the acquired lock from the `begin` `RETURNING` row. The `AgentId` is decoded
/// from the row's OWN `id` column, never back-filled from the query parameter; the lock
/// columns are non-null because this statement just set them.
fn begin_lock_row(row: &SqliteRow) -> Result<RuntimeTransitionLockRow, sqlx::Error> {
    Ok(RuntimeTransitionLockRow {
        agent_id: decode::agent_id(row.try_get::<String, _>("id")?)?,
        target: row.try_get::<String, _>("runtime_transition_target")?,
        started_at: row.try_get::<i64, _>("runtime_transition_started_at")?,
    })
}

fn held_lock_row(row: &SqliteRow) -> Result<Option<RuntimeTransitionLockRow>, sqlx::Error> {
    let Some(target) = row.try_get::<Option<String>, _>("runtime_transition_target")? else {
        return Ok(None);
    };
    let Some(started_at) = row.try_get::<Option<i64>, _>("runtime_transition_started_at")? else {
        return Ok(None);
    };
    Ok(Some(RuntimeTransitionLockRow {
        agent_id: decode::agent_id(row.try_get::<String, _>("id")?)?,
        target,
        started_at,
    }))
}

fn marker_matches(lock: &RuntimeTransitionLock, row: &RuntimeTransitionLockRow) -> bool {
    &row.agent_id == lock.agent_id()
        && row.target == lock.target().as_str()
        && row.started_at == lock.started_at()
}

/// Project the committed after-image from the `complete` `RETURNING` row. The id rides
/// the row's OWN `id`; the lock pair is read back so core can prove it cleared.
fn complete_after_image(row: &SqliteRow) -> Result<RuntimeTransitionAfterImage, sqlx::Error> {
    Ok(RuntimeTransitionAfterImage {
        agent_id: decode::agent_id(row.try_get::<String, _>("id")?)?,
        runtime: row.try_get::<String, _>("runtime")?,
        model: row.try_get::<Option<String>, _>("model")?,
        reasoning_effort: row.try_get::<Option<String>, _>("reasoning_effort")?,
        fast_mode: row.try_get::<Option<i64>, _>("fast_mode")?,
        updated_at: row.try_get::<i64, _>("updated_at")?,
        transition_target: row.try_get::<Option<String>, _>("runtime_transition_target")?,
        transition_started_at: row.try_get::<Option<i64>, _>("runtime_transition_started_at")?,
    })
}

impl RuntimeTransitionWrite for SqliteHostAgentStore {
    async fn begin(
        &self,
        agent: &AgentId,
        target: RuntimeKind,
        now_ms: i64,
    ) -> Result<BeginTransitionRecord, PersistenceFailure> {
        let mut tx = self
            .begin_root_write()
            .await
            .map_err(PersistenceFailure::from_source)?;

        let Some(row) = sqlx::query(SELECT_BEGIN_PRECHECK)
            .bind(agent.as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(PersistenceFailure::from_source)?
        else {
            return Ok(BeginTransitionRecord::NotFound);
        };
        let pre = begin_precheck(&row).map_err(PersistenceFailure::from_source)?;

        // Precedence is the TypeScript authority's re-read: an in-flight delete answers
        // before an in-flight transition.
        if pre.deletion_started_at.is_some() {
            return Ok(BeginTransitionRecord::Deleting);
        }
        if pre.runtime_transition_target.is_some() {
            return Ok(BeginTransitionRecord::InProgress);
        }

        let Some(returned) = sqlx::query(BEGIN_TRANSITION_CAS)
            .bind(target.as_str())
            .bind(now_ms)
            .bind(agent.as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(PersistenceFailure::from_source)?
        else {
            return Err(PersistenceFailure::from_source(BeginTransitionCasLost));
        };
        let witness = begin_lock_row(&returned).map_err(PersistenceFailure::from_source)?;
        tx.commit().await.map_err(PersistenceFailure::from_source)?;
        Ok(BeginTransitionRecord::Locked(witness))
    }

    async fn abort(
        &self,
        lock: &RuntimeTransitionLock,
    ) -> Result<AbortTransition, PersistenceFailure> {
        let mut tx = self
            .begin_root_write()
            .await
            .map_err(PersistenceFailure::from_source)?;
        let changed = sqlx::query(ABORT_TRANSITION_CAS)
            .bind(lock.agent_id().as_str())
            .bind(lock.target().as_str())
            .bind(lock.started_at())
            .execute(&mut *tx)
            .await
            .map_err(PersistenceFailure::from_source)?
            .rows_affected();
        tx.commit().await.map_err(PersistenceFailure::from_source)?;
        Ok(if changed > 0 {
            AbortTransition::Released
        } else {
            AbortTransition::NotHeld
        })
    }

    async fn complete(
        &self,
        lock: &RuntimeTransitionLock,
        commit: &RuntimeTransitionCommit,
        now_ms: i64,
    ) -> Result<CompleteTransitionRecord, PersistenceFailure> {
        let mut tx = self
            .begin_root_write()
            .await
            .map_err(PersistenceFailure::from_source)?;
        let returned = sqlx::query(COMPLETE_TRANSITION_CAS)
            .bind(lock.target().as_str())
            .bind(commit.model.as_deref())
            .bind(commit.reasoning_effort.as_deref())
            .bind(fast_mode_column(commit.fast_mode))
            .bind(now_ms)
            .bind(lock.agent_id().as_str())
            .bind(lock.target().as_str())
            .bind(lock.started_at())
            .fetch_optional(&mut *tx)
            .await
            .map_err(PersistenceFailure::from_source)?;
        let record = match returned {
            Some(row) => CompleteTransitionRecord::Committed(
                complete_after_image(&row).map_err(PersistenceFailure::from_source)?,
            ),
            None => CompleteTransitionRecord::Lost,
        };
        tx.commit().await.map_err(PersistenceFailure::from_source)?;
        Ok(record)
    }
}

impl RuntimeTransitionInboxWrite for SqliteHostAgentStore {
    async fn reclaim_transition_inboxes(
        &self,
        lock: &RuntimeTransitionLock,
    ) -> Result<RuntimeTransitionInboxReclaimRecord, PersistenceFailure> {
        let mut tx = self
            .begin_root_write()
            .await
            .map_err(PersistenceFailure::from_source)?;
        let Some(row) = sqlx::query(SELECT_HELD_TRANSITION)
            .bind(lock.agent_id().as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(PersistenceFailure::from_source)?
        else {
            tx.commit().await.map_err(PersistenceFailure::from_source)?;
            return Ok(RuntimeTransitionInboxReclaimRecord::Lost);
        };
        let Some(witness) = held_lock_row(&row).map_err(PersistenceFailure::from_source)? else {
            tx.commit().await.map_err(PersistenceFailure::from_source)?;
            return Ok(RuntimeTransitionInboxReclaimRecord::Lost);
        };
        if !marker_matches(lock, &witness) {
            tx.commit().await.map_err(PersistenceFailure::from_source)?;
            return Ok(RuntimeTransitionInboxReclaimRecord::Lost);
        }

        // The EXISTS guard is deliberately repeated on the mutation itself. The
        // BEGIN IMMEDIATE reservation makes the pre-read stable, while this final CAS
        // line is the mutation-killing proof that a stale token cannot clear leases.
        let rows = sqlx::query(RECLAIM_INBOX_LEASES)
            .bind(lock.agent_id().as_str())
            .bind(lock.agent_id().as_str())
            .bind(lock.target().as_str())
            .bind(lock.started_at())
            .fetch_all(&mut *tx)
            .await
            .map_err(PersistenceFailure::from_source)?;
        let mut conversations = rows
            .into_iter()
            .map(|row| {
                ConversationId::parse(row.try_get::<String, _>("conversation_id")?)
                    .map_err(decode::decode_error)
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(PersistenceFailure::from_source)?;
        conversations.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        conversations.dedup();
        tx.commit().await.map_err(PersistenceFailure::from_source)?;
        Ok(RuntimeTransitionInboxReclaimRecord::Reclaimed(
            RuntimeTransitionInboxReclaimRow {
                lock: witness,
                conversations,
            },
        ))
    }
}

impl RuntimeTransitionSeatRead for SqliteHostAgentStore {
    async fn read_transition_seats(
        &self,
        lock: &RuntimeTransitionLock,
    ) -> Result<RuntimeTransitionSeatSnapshotRecord, PersistenceFailure> {
        let mut tx = self
            .pool()
            .begin()
            .await
            .map_err(PersistenceFailure::from_source)?;
        let Some(row) = sqlx::query(SELECT_HELD_TRANSITION)
            .bind(lock.agent_id().as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(PersistenceFailure::from_source)?
        else {
            tx.commit().await.map_err(PersistenceFailure::from_source)?;
            return Ok(RuntimeTransitionSeatSnapshotRecord::Lost);
        };
        let Some(witness) = held_lock_row(&row).map_err(PersistenceFailure::from_source)? else {
            tx.commit().await.map_err(PersistenceFailure::from_source)?;
            return Ok(RuntimeTransitionSeatSnapshotRecord::Lost);
        };
        if !marker_matches(lock, &witness) {
            tx.commit().await.map_err(PersistenceFailure::from_source)?;
            return Ok(RuntimeTransitionSeatSnapshotRecord::Lost);
        }
        let rows = sqlx::query(
            "SELECT agent_id, conversation_id, observed_status, cli_session_id \
               FROM agent_runtime_slots WHERE agent_id = ? ORDER BY id",
        )
        .bind(lock.agent_id().as_str())
        .fetch_all(&mut *tx)
        .await
        .map_err(PersistenceFailure::from_source)?;
        let seats = rows
            .into_iter()
            .map(|row| {
                let agent = decode::agent_id(row.try_get::<String, _>("agent_id")?)?;
                let conversation = row.try_get::<Option<String>, _>("conversation_id")?;
                let scope = match conversation {
                    Some(source) => RuntimeScope::ConversationSlot {
                        agent,
                        source: ConversationId::parse(source).map_err(decode::decode_error)?,
                    },
                    None => RuntimeScope::AgentSlot { agent },
                };
                let observed_status =
                    RuntimeObservedStatus::parse(&row.try_get::<String, _>("observed_status")?)
                        .map_err(decode::decode_error)?;
                Ok(RuntimeTransitionSeatRow {
                    scope,
                    observed_status,
                    cli_session_id: row.try_get("cli_session_id")?,
                })
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()
            .map_err(PersistenceFailure::from_source)?;
        tx.commit().await.map_err(PersistenceFailure::from_source)?;
        Ok(RuntimeTransitionSeatSnapshotRecord::Snapshot(
            RuntimeTransitionSeatSnapshotRow {
                lock: witness,
                seats,
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, time::Duration};

    use silverhand_backend_core::host_runtime::runtime_transition::{
        BeginTransition, CompleteTransition, abort_runtime_transition, begin_runtime_transition,
        complete_runtime_transition,
    };
    use sqlx::{Connection, Row};
    use tempfile::TempDir;

    use super::*;

    fn agent(id: &str) -> AgentId {
        AgentId::parse(id).expect("agent id")
    }

    async fn seed_schema(dir: &TempDir, name: &str, seed: &[&'static str]) -> PathBuf {
        let path = dir.path().join(name);
        let mut connection = crate::current_schema::create_current_schema(&path).await;
        for statement in seed {
            sqlx::query(*statement)
                .execute(&mut connection)
                .await
                .expect("seed statement");
        }
        connection.close().await.expect("close seed connection");
        path
    }

    /// Walks the source chain for SQLITE_BUSY (code 5), the way the reservation gate
    /// hands a contended begin over as `PersistenceFailure`'s source.
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

    const USER: &str = "INSERT INTO users (id, name, display_name, created_at, updated_at) \
         VALUES ('u1', 'u1', 'U1', 1, 1)";
    const HOST: &str =
        "INSERT INTO hosts (id, name, kind, created_at) VALUES ('h1', 'h1', 'local', 1)";
    /// Active, runtime `claude`, no lock and no tuning columns set, mtime at 1.
    const ACTIVE_A1: &str = "INSERT INTO agents \
         (id, name, runtime, host_id, created_by_user_id, created_at, updated_at, \
          materialization_status, session_scope) \
         VALUES ('a1', 'a1', 'claude', 'h1', 'u1', 1, 1, 'active', 'conversation')";
    /// Active but a transition to `codex` already holds the lock.
    const TRANSITION_A1: &str = "INSERT INTO agents \
         (id, name, runtime, host_id, created_by_user_id, created_at, updated_at, \
          materialization_status, session_scope, \
          runtime_transition_target, runtime_transition_started_at) \
         VALUES ('a1', 'a1', 'claude', 'h1', 'u1', 1, 1, 'active', 'conversation', 'codex', 1)";
    /// Active but a deletion marker holds the row.
    const DELETING_A1: &str = "INSERT INTO agents \
         (id, name, runtime, host_id, created_by_user_id, created_at, updated_at, \
          materialization_status, session_scope, deletion_started_at) \
         VALUES ('a1', 'a1', 'claude', 'h1', 'u1', 1, 1, 'active', 'conversation', 7)";

    async fn lock_columns(store: &SqliteHostAgentStore) -> (Option<String>, Option<i64>) {
        let row = sqlx::query(
            "SELECT runtime_transition_target, runtime_transition_started_at \
               FROM agents WHERE id = 'a1'",
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        (
            row.get::<Option<String>, _>("runtime_transition_target"),
            row.get::<Option<i64>, _>("runtime_transition_started_at"),
        )
    }

    /// Acquire a lock through the full core+adapter stack, returning the sealed token.
    async fn acquire(
        store: &SqliteHostAgentStore,
        target: RuntimeKind,
        now: i64,
    ) -> RuntimeTransitionLock {
        match begin_runtime_transition(store, &agent("a1"), target, now)
            .await
            .unwrap()
        {
            BeginTransition::Started(lock) => lock,
            other => panic!("expected a sealed lock, got {other:?}"),
        }
    }

    /// The deterministic writer-reservation gate. A no-write answer (`NotFound`, from an
    /// absent agent) succeeds while nothing is held, then fails at `begin_root_write`
    /// with SQLITE_BUSY once the single writer is held.
    ///
    /// Falsifier: change the store's `BEGIN IMMEDIATE` to `BEGIN DEFERRED` and this test
    /// turns RED — a deferred begin takes no writer lock, so the no-write answer returns
    /// `Ok(NotFound)` even while a writer is held, and `unwrap_err` panics.
    #[tokio::test]
    async fn begin_reserves_the_writer_before_its_precheck() {
        let dir = tempfile::tempdir().unwrap();
        let path = seed_schema(&dir, "begin-reservation.db", &[USER, HOST]).await;
        let store = SqliteHostAgentStore::open_with_busy_timeout(&path, Duration::from_millis(50))
            .await
            .unwrap();

        let idle = store
            .begin(&agent("missing"), RuntimeKind::Codex, 1)
            .await
            .unwrap();
        assert!(matches!(idle, BeginTransitionRecord::NotFound));

        let held = store.begin_root_write().await.unwrap();
        let contended = store
            .begin(&agent("missing"), RuntimeKind::Codex, 1)
            .await
            .unwrap_err();
        assert!(
            is_sqlite_busy(&contended),
            "begin did not reserve the writer before its pre-read"
        );
        held.rollback().await.unwrap();
    }

    #[tokio::test]
    async fn begin_acquires_the_lock_without_touching_runtime_or_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let path = seed_schema(&dir, "begin.db", &[USER, HOST, ACTIVE_A1]).await;
        let store = SqliteHostAgentStore::open(&path).await.unwrap();

        let lock = acquire(&store, RuntimeKind::Codex, 5).await;
        // The sealed token carries the row the write actually touched.
        assert_eq!(lock.agent_id(), &agent("a1"));
        assert_eq!(lock.target(), RuntimeKind::Codex);
        assert_eq!(lock.started_at(), 5);

        let row = sqlx::query(
            "SELECT runtime, updated_at, runtime_transition_target, runtime_transition_started_at \
               FROM agents WHERE id = 'a1'",
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        // The lock is held for the target with its paired started-at...
        assert_eq!(
            row.get::<Option<String>, _>("runtime_transition_target"),
            Some("codex".to_string())
        );
        assert_eq!(
            row.get::<Option<i64>, _>("runtime_transition_started_at"),
            Some(5)
        );
        // ...but the runtime and the row's mtime are untouched until `complete`.
        assert_eq!(row.get::<String, _>("runtime"), "claude");
        assert_eq!(row.get::<i64, _>("updated_at"), 1);
    }

    #[tokio::test]
    async fn begin_refuses_missing_deleting_and_already_transitioning_agents() {
        let dir = tempfile::tempdir().unwrap();

        let missing = seed_schema(&dir, "missing.db", &[USER, HOST]).await;
        let store = SqliteHostAgentStore::open(&missing).await.unwrap();
        assert_eq!(
            begin_runtime_transition(&store, &agent("a1"), RuntimeKind::Codex, 1)
                .await
                .unwrap(),
            BeginTransition::NotFound
        );

        // A deletion marker refuses acquisition.
        let deleting = seed_schema(&dir, "deleting.db", &[USER, HOST, DELETING_A1]).await;
        let store = SqliteHostAgentStore::open(&deleting).await.unwrap();
        assert_eq!(
            begin_runtime_transition(&store, &agent("a1"), RuntimeKind::Codex, 1)
                .await
                .unwrap(),
            BeginTransition::Deleting
        );

        // A held lock refuses a second acquire, whatever the new target.
        let transition = seed_schema(&dir, "transition.db", &[USER, HOST, TRANSITION_A1]).await;
        let store = SqliteHostAgentStore::open(&transition).await.unwrap();
        assert_eq!(
            begin_runtime_transition(&store, &agent("a1"), RuntimeKind::Cursor, 1)
                .await
                .unwrap(),
            BeginTransition::InProgress
        );
    }

    #[tokio::test]
    async fn abort_releases_the_lock_it_owns_then_is_not_held() {
        let dir = tempfile::tempdir().unwrap();
        let path = seed_schema(&dir, "abort.db", &[USER, HOST, ACTIVE_A1]).await;
        let store = SqliteHostAgentStore::open(&path).await.unwrap();

        let lock = acquire(&store, RuntimeKind::Codex, 5).await;
        assert_eq!(
            abort_runtime_transition(&store, &lock).await.unwrap(),
            AbortTransition::Released
        );
        assert_eq!(lock_columns(&store).await, (None, None));

        // A second abort of the same token finds nothing to release.
        assert_eq!(
            abort_runtime_transition(&store, &lock).await.unwrap(),
            AbortTransition::NotHeld
        );
    }

    /// The different-started-at fence. A new attempt reacquires the same target with a
    /// fresh started-at; the OLD token can neither abort nor complete the new lock,
    /// because its guard binds the started-at it acquired. This fences a reacquire at a
    /// *different* instant only — a same-millisecond reacquire is out of scope, since
    /// `started_at` is a wall clock, not a unique attempt id.
    #[tokio::test]
    async fn a_reacquired_lock_at_a_different_started_at_is_not_the_old_tokens_to_touch() {
        let dir = tempfile::tempdir().unwrap();
        let path = seed_schema(&dir, "aba.db", &[USER, HOST, ACTIVE_A1]).await;
        let store = SqliteHostAgentStore::open(&path).await.unwrap();

        let stale = acquire(&store, RuntimeKind::Codex, 5).await;
        // Simulate a fresh attempt reacquiring the SAME target with a new started-at.
        sqlx::query("UPDATE agents SET runtime_transition_started_at = 9 WHERE id = 'a1'")
            .execute(store.pool())
            .await
            .unwrap();

        assert_eq!(
            abort_runtime_transition(&store, &stale).await.unwrap(),
            AbortTransition::NotHeld
        );
        let commit = RuntimeTransitionCommit {
            model: None,
            reasoning_effort: None,
            fast_mode: None,
        };
        assert_eq!(
            complete_runtime_transition(&store, &stale, &commit, 42)
                .await
                .unwrap(),
            CompleteTransition::Lost
        );
        // The new attempt's lock still stands, untouched.
        assert_eq!(
            lock_columns(&store).await,
            (Some("codex".to_string()), Some(9))
        );
    }

    #[tokio::test]
    async fn complete_writes_runtime_and_tuning_and_releases_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = seed_schema(&dir, "complete.db", &[USER, HOST, ACTIVE_A1]).await;
        let store = SqliteHostAgentStore::open(&path).await.unwrap();

        let lock = acquire(&store, RuntimeKind::Codex, 5).await;
        let commit = RuntimeTransitionCommit {
            model: Some("gpt".to_string()),
            reasoning_effort: Some("high".to_string()),
            fast_mode: Some(true),
        };
        let CompleteTransition::Committed(sealed) =
            complete_runtime_transition(&store, &lock, &commit, 42)
                .await
                .unwrap()
        else {
            panic!("expected a committed transition");
        };
        assert_eq!(sealed.agent_id(), &agent("a1"));
        assert_eq!(sealed.runtime(), RuntimeKind::Codex);
        assert_eq!(sealed.model(), Some("gpt"));
        assert_eq!(sealed.reasoning_effort(), Some("high"));
        assert_eq!(sealed.fast_mode(), Some(true));
        assert_eq!(sealed.updated_at(), 42);

        let row = sqlx::query(
            "SELECT runtime, model, reasoning_effort, fast_mode, updated_at, \
                    runtime_transition_target, runtime_transition_started_at \
               FROM agents WHERE id = 'a1'",
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        // The committed runtime is the lock target (`codex`), not the row's prior runtime
        // (`claude`) and not any commit field — the commit carries no runtime at all.
        assert_eq!(row.get::<String, _>("runtime"), "codex");
        assert_eq!(
            row.get::<Option<String>, _>("model"),
            Some("gpt".to_string())
        );
        assert_eq!(
            row.get::<Option<String>, _>("reasoning_effort"),
            Some("high".to_string())
        );
        assert_eq!(row.get::<Option<i64>, _>("fast_mode"), Some(1));
        assert_eq!(row.get::<i64, _>("updated_at"), 42);
        assert_eq!(
            row.get::<Option<String>, _>("runtime_transition_target"),
            None
        );
        assert_eq!(
            row.get::<Option<i64>, _>("runtime_transition_started_at"),
            None
        );
    }

    #[tokio::test]
    async fn complete_clears_tuning_columns_to_null() {
        let dir = tempfile::tempdir().unwrap();
        // Seed a row whose tuning columns are already set, to prove `complete` overwrites
        // them with NULLs rather than leaving the previous values.
        let seeded = "INSERT INTO agents \
             (id, name, runtime, host_id, created_by_user_id, created_at, updated_at, \
              materialization_status, session_scope, model, reasoning_effort, fast_mode) \
             VALUES ('a1', 'a1', 'claude', 'h1', 'u1', 1, 1, 'active', 'conversation', \
                     'old', 'low', 1)";
        let path = seed_schema(&dir, "complete-null.db", &[USER, HOST, seeded]).await;
        let store = SqliteHostAgentStore::open(&path).await.unwrap();

        let lock = acquire(&store, RuntimeKind::Codex, 5).await;
        let commit = RuntimeTransitionCommit {
            model: None,
            reasoning_effort: None,
            fast_mode: None,
        };
        let CompleteTransition::Committed(sealed) =
            complete_runtime_transition(&store, &lock, &commit, 42)
                .await
                .unwrap()
        else {
            panic!("expected a committed transition");
        };
        assert_eq!(sealed.model(), None);
        assert_eq!(sealed.reasoning_effort(), None);
        assert_eq!(sealed.fast_mode(), None);

        let row =
            sqlx::query("SELECT model, reasoning_effort, fast_mode FROM agents WHERE id = 'a1'")
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(row.get::<Option<String>, _>("model"), None);
        assert_eq!(row.get::<Option<String>, _>("reasoning_effort"), None);
        assert_eq!(row.get::<Option<i64>, _>("fast_mode"), None);
    }
}
