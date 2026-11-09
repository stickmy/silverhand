//! The durable deletion-intent lifecycle: the fail-forward marker write, the
//! restart inventory, and the keyed completing write.
//!
//! All three run under the same root-write reservation the other axes use.
//! `begin` is a compare-and-set that turns a null marker into a timestamp; the
//! `WHERE` re-states every precondition the pre-read observed, so a row that moved
//! out from under the lock changes nothing and surfaces as a lost serialized CAS
//! rather than a silent no-op. `complete` re-reads the attempt's own agent under
//! the lock and captures before the delete, exactly as the single-agent axis does;
//! the marker's timestamp is the token it matches on, so another attempt taking the
//! row is `AttemptLost`, not a second teardown.

use silverhand_backend_core::{
    application_orchestration::teardown::{
        contract::TeardownFailure,
        eligibility::{AgentTeardownEligibility, EligibilityVerdict, hold_eligibility},
        intent::{
            AgentDeletionAttempt, AgentDeletionLifecyclePort, BeginDeletionRecord,
            CompleteDeletion, PersistedDeletionIntent,
        },
        write::AgentTornDown,
    },
    foundation::identity::{AgentId, HostId},
    foundation::persistence::PersistenceFailure,
    identity_access::agent_lifecycle::{
        AgentLifecycle, AgentLifecycleStamps, MaterializationStatus, agent_lifecycle,
    },
};
use sqlx::{Row, sqlite::SqliteRow};
use thiserror::Error;

use crate::foundation::{
    agent_lifecycle_sql::agent_active,
    host_agent_store::{SqliteHostAgentStore, decode},
};

use super::{
    facts::{capture_agent, select_agent_facts},
    unit_of_work::DELETE_AGENT,
};

/// The pre-read the marker write decides from. It reads `runtime_transition_target`,
/// which the shared agent-facts projection does not carry, because the ONE stage a
/// deletion may not begin from beyond "not active" is a runtime transition holding
/// the row.
const SELECT_BEGIN_PRECHECK: &str = "SELECT host_id, materialization_status, deletion_started_at, \
     runtime_transition_target FROM agents WHERE id = ?";

/// The marker write. `updated_at` is bumped with the marker so the row's mtime
/// reflects the transition, matching the TypeScript authority. The `WHERE` is the
/// serialized compare-and-set: still active, still unmarked, no transition.
const BEGIN_DELETION_CAS: &str = concat!(
    "UPDATE agents SET deletion_started_at = ?, updated_at = ? WHERE id = ? AND ",
    agent_active!("")
);

/// The restart inventory, ordered oldest-marker-first so a resumer drains them in
/// the order they were condemned.
const SELECT_PENDING_DELETIONS: &str = "SELECT id, host_id, deletion_started_at \
       FROM agents WHERE deletion_started_at IS NOT NULL \
      ORDER BY deletion_started_at ASC, id ASC";

/// The writer was reserved and every precondition the pre-read observed still held,
/// yet the CAS changed no row. Under `BEGIN IMMEDIATE` nothing else can have written
/// the row, so this is a broken invariant, not a product answer.
#[derive(Debug, Error)]
#[error("begin agent deletion lost its serialized compare-and-set")]
struct DeletionCasLost;

/// What the pre-read observed, decoded through the same authorities the facts
/// projection uses.
struct BeginPrecheck {
    host_id: HostId,
    stamps: AgentLifecycleStamps,
}

fn begin_precheck(row: &SqliteRow) -> Result<BeginPrecheck, sqlx::Error> {
    Ok(BeginPrecheck {
        host_id: decode::host_id(row.try_get("host_id")?)?,
        stamps: AgentLifecycleStamps {
            materialization: decode::materialization_status(
                &row.try_get::<String, _>("materialization_status")?,
            )?,
            deletion_started_at: row.try_get("deletion_started_at")?,
            transition_held: row
                .try_get::<Option<String>, _>("runtime_transition_target")?
                .is_some(),
        },
    })
}

fn pending_intent(row: &SqliteRow) -> Result<PersistedDeletionIntent, sqlx::Error> {
    Ok(PersistedDeletionIntent {
        agent_id: decode::agent_id(row.try_get("id")?)?,
        host_id: decode::host_id(row.try_get("host_id")?)?,
        deletion_started_at: row.try_get("deletion_started_at")?,
    })
}

impl AgentDeletionLifecyclePort for SqliteHostAgentStore {
    async fn begin(
        &self,
        agent_id: &AgentId,
        started_at: i64,
    ) -> Result<BeginDeletionRecord, PersistenceFailure> {
        let mut tx = self
            .begin_root_write()
            .await
            .map_err(PersistenceFailure::from_source)?;

        let Some(row) = sqlx::query(SELECT_BEGIN_PRECHECK)
            .bind(agent_id.as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(PersistenceFailure::from_source)?
        else {
            return Ok(BeginDeletionRecord::NotFound);
        };
        let pre = begin_precheck(&row).map_err(PersistenceFailure::from_source)?;

        // Precedence is the TypeScript authority's: an existing marker answers first
        // (this request changed nothing), then stage, then a runtime transition.
        match agent_lifecycle(&pre.stamps) {
            AgentLifecycle::Deleting { started_at } => {
                return Ok(BeginDeletionRecord::AlreadyPending(
                    PersistedDeletionIntent {
                        agent_id: agent_id.clone(),
                        host_id: pre.host_id,
                        deletion_started_at: started_at,
                    },
                ));
            }
            AgentLifecycle::Pending | AgentLifecycle::Failed => {
                return Ok(BeginDeletionRecord::NotActive);
            }
            AgentLifecycle::Transitioning => {
                return Ok(BeginDeletionRecord::RuntimeTransitionInProgress);
            }
            AgentLifecycle::Active => {}
        }

        let changed = sqlx::query(BEGIN_DELETION_CAS)
            .bind(started_at)
            .bind(started_at)
            .bind(agent_id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(PersistenceFailure::from_source)?
            .rows_affected();
        if changed != 1 {
            return Err(PersistenceFailure::from_source(DeletionCasLost));
        }
        tx.commit().await.map_err(PersistenceFailure::from_source)?;
        Ok(BeginDeletionRecord::Started(PersistedDeletionIntent {
            agent_id: agent_id.clone(),
            host_id: pre.host_id,
            deletion_started_at: started_at,
        }))
    }

    async fn list_pending(&self) -> Result<Vec<PersistedDeletionIntent>, PersistenceFailure> {
        let mut conn = self
            .pool()
            .acquire()
            .await
            .map_err(PersistenceFailure::from_source)?;
        let rows = sqlx::query(SELECT_PENDING_DELETIONS)
            .fetch_all(&mut *conn)
            .await
            .map_err(PersistenceFailure::from_source)?;
        rows.iter()
            .map(pending_intent)
            .collect::<Result<Vec<_>, sqlx::Error>>()
            .map_err(PersistenceFailure::from_source)
    }

    async fn complete(
        &self,
        attempt: &AgentDeletionAttempt,
    ) -> Result<CompleteDeletion, TeardownFailure> {
        let mut tx = self
            .begin_root_write()
            .await
            .map_err(PersistenceFailure::from_source)?;

        let facts = match select_agent_facts(&mut tx, attempt.agent_id())
            .await
            .map_err(PersistenceFailure::from_source)?
        {
            Some(facts) => facts,
            None => return Ok(CompleteDeletion::Gone),
        };

        // The marker's timestamp is the token: another attempt owning the row, a
        // stage change, or a host move all mean this attempt no longer owns the
        // deletion and must delete nothing.
        if facts.materialization_status != MaterializationStatus::Active
            || &facts.host_id != attempt.host_id()
            || facts.deletion_started_at != Some(attempt.deletion_started_at())
        {
            return Ok(CompleteDeletion::AttemptLost);
        }

        let expected = AgentTeardownEligibility::ActiveOn {
            host_id: attempt.host_id().clone(),
        };
        let EligibilityVerdict::Held(held) = hold_eligibility(&expected, facts) else {
            unreachable!("complete re-read the attempt's own agent as active on its own computer");
        };
        let capture = capture_agent(&mut tx, attempt.agent_id())
            .await
            .map_err(PersistenceFailure::from_source)?;
        sqlx::query(DELETE_AGENT)
            .bind(attempt.agent_id().as_str())
            .execute(&mut *tx)
            .await
            .map_err(PersistenceFailure::from_source)?;
        let torn = AgentTornDown::seal(attempt.agent_id(), held, capture)?;
        tx.commit().await.map_err(PersistenceFailure::from_source)?;
        Ok(CompleteDeletion::Committed(Box::new(torn)))
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, time::Duration};

    use silverhand_backend_core::application_orchestration::teardown::intent::{
        BeginDeletion, begin_agent_deletion, pending_agent_deletions,
    };
    use sqlx::Connection;
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

    /// The reservation fails a contended begin with SQLITE_BUSY (code 5), walking the
    /// source chain because the marker write hands the driver error over as
    /// `PersistenceFailure`'s source inside `TeardownFailure`.
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
    const ACTIVE_A1: &str = "INSERT INTO agents \
         (id, name, runtime, host_id, created_by_user_id, created_at, updated_at, \
          materialization_status, session_scope) \
         VALUES ('a1', 'a1', 'claude', 'h1', 'u1', 1, 1, 'active', 'conversation')";
    const ACTIVE_A2: &str = "INSERT INTO agents \
         (id, name, runtime, host_id, created_by_user_id, created_at, updated_at, \
          materialization_status, session_scope) \
         VALUES ('a2', 'a2', 'claude', 'h1', 'u1', 1, 1, 'active', 'conversation')";
    const ACTIVE_A3: &str = "INSERT INTO agents \
         (id, name, runtime, host_id, created_by_user_id, created_at, updated_at, \
          materialization_status, session_scope) \
         VALUES ('a3', 'a3', 'claude', 'h1', 'u1', 1, 1, 'active', 'conversation')";
    /// Present but not at a stage a deletion may begin from — chosen so the arm is
    /// decided by the stage alone, with no failed-code question in the fixture.
    const PENDING_A1: &str = "INSERT INTO agents \
         (id, name, runtime, host_id, created_by_user_id, created_at, updated_at, \
          materialization_status, session_scope) \
         VALUES ('a1', 'a1', 'claude', 'h1', 'u1', 1, 1, 'pending_materialization', 'conversation')";
    /// Active, but a runtime transition holds the row. The target/started-at pair is
    /// what the schema's paired CHECK and the begin pre-read both key on.
    const TRANSITION_A1: &str = "INSERT INTO agents \
         (id, name, runtime, host_id, created_by_user_id, created_at, updated_at, \
          materialization_status, session_scope, \
          runtime_transition_target, runtime_transition_started_at) \
         VALUES ('a1', 'a1', 'claude', 'h1', 'u1', 1, 1, 'active', 'conversation', 'codex', 1)";

    /// The deterministic writer-reservation gate for the marker write. A no-write
    /// answer (`NotFound`, from an absent agent) succeeds while nothing is held, then
    /// fails at `begin_root_write` with SQLITE_BUSY once the single writer is held.
    ///
    /// Falsifier: change the store's `BEGIN IMMEDIATE` to `BEGIN DEFERRED` and this
    /// test turns RED — a deferred begin takes no writer lock, so the no-write answer
    /// returns `Ok(NotFound)` even while a writer is held, and `unwrap_err` panics.
    #[tokio::test]
    async fn begin_reserves_the_writer_before_its_precheck() {
        let dir = tempfile::tempdir().unwrap();
        let path = seed_schema(&dir, "begin-reservation.db", &[USER, HOST]).await;
        let store = SqliteHostAgentStore::open_with_busy_timeout(&path, Duration::from_millis(50))
            .await
            .unwrap();

        let idle = begin_agent_deletion(&store, &agent("missing"), 1)
            .await
            .unwrap();
        assert!(matches!(idle, BeginDeletion::NotFound));

        let held = store.begin_root_write().await.unwrap();
        let contended = begin_agent_deletion(&store, &agent("missing"), 1)
            .await
            .unwrap_err();
        assert!(
            is_sqlite_busy(&contended),
            "the marker write did not reserve the writer before its pre-read"
        );
        held.rollback().await.unwrap();
    }

    #[tokio::test]
    async fn begin_writes_the_marker_and_bumps_updated_at() {
        let dir = tempfile::tempdir().unwrap();
        let path = seed_schema(&dir, "begin.db", &[USER, HOST, ACTIVE_A1]).await;
        let store = SqliteHostAgentStore::open(&path).await.unwrap();

        let started = begin_agent_deletion(&store, &agent("a1"), 5).await.unwrap();
        assert!(matches!(started, BeginDeletion::Started(_)));

        let row = sqlx::query("SELECT deletion_started_at, updated_at FROM agents WHERE id = 'a1'")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(row.get::<Option<i64>, _>("deletion_started_at"), Some(5));
        assert_eq!(row.get::<i64, _>("updated_at"), 5);
    }

    #[tokio::test]
    async fn begin_is_idempotent_and_leaves_the_first_marker() {
        let dir = tempfile::tempdir().unwrap();
        let path = seed_schema(&dir, "idempotent.db", &[USER, HOST, ACTIVE_A1]).await;
        let store = SqliteHostAgentStore::open(&path).await.unwrap();

        assert!(matches!(
            begin_agent_deletion(&store, &agent("a1"), 5).await.unwrap(),
            BeginDeletion::Started(_)
        ));
        let again = begin_agent_deletion(&store, &agent("a1"), 9).await.unwrap();
        assert!(matches!(again, BeginDeletion::AlreadyPending(_)));

        // The second request changed nothing: neither the marker it read back nor the
        // row's mtime moved off the first begin's timestamp.
        let row = sqlx::query("SELECT deletion_started_at, updated_at FROM agents WHERE id = 'a1'")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(row.get::<Option<i64>, _>("deletion_started_at"), Some(5));
        assert_eq!(row.get::<i64, _>("updated_at"), 5);
    }

    #[tokio::test]
    async fn begin_refuses_missing_nonactive_and_transitioning_agents() {
        let dir = tempfile::tempdir().unwrap();

        let missing = seed_schema(&dir, "missing.db", &[USER, HOST]).await;
        let store = SqliteHostAgentStore::open(&missing).await.unwrap();
        assert!(matches!(
            begin_agent_deletion(&store, &agent("a1"), 1).await.unwrap(),
            BeginDeletion::NotFound
        ));

        let pending = seed_schema(&dir, "pending.db", &[USER, HOST, PENDING_A1]).await;
        let store = SqliteHostAgentStore::open(&pending).await.unwrap();
        assert!(matches!(
            begin_agent_deletion(&store, &agent("a1"), 1).await.unwrap(),
            BeginDeletion::NotActive
        ));

        let transition = seed_schema(&dir, "transition.db", &[USER, HOST, TRANSITION_A1]).await;
        let store = SqliteHostAgentStore::open(&transition).await.unwrap();
        assert!(matches!(
            begin_agent_deletion(&store, &agent("a1"), 1).await.unwrap(),
            BeginDeletion::RuntimeTransitionInProgress
        ));
    }

    #[tokio::test]
    async fn pending_deletions_are_ordered_oldest_marker_then_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = seed_schema(
            &dir,
            "pending-order.db",
            &[USER, HOST, ACTIVE_A1, ACTIVE_A2, ACTIVE_A3],
        )
        .await;
        let store = SqliteHostAgentStore::open(&path).await.unwrap();

        // a3's marker is older; a1 and a2 share a timestamp, so the id breaks the tie.
        begin_agent_deletion(&store, &agent("a3"), 1).await.unwrap();
        begin_agent_deletion(&store, &agent("a1"), 5).await.unwrap();
        begin_agent_deletion(&store, &agent("a2"), 5).await.unwrap();

        let attempts = pending_agent_deletions(&store).await.unwrap();
        assert_eq!(
            attempts
                .iter()
                .map(|attempt| attempt.agent_id().as_str())
                .collect::<Vec<_>>(),
            vec!["a3", "a1", "a2"]
        );
    }

    #[tokio::test]
    async fn complete_deletes_the_marked_agent() {
        let dir = tempfile::tempdir().unwrap();
        let path = seed_schema(&dir, "complete.db", &[USER, HOST, ACTIVE_A1]).await;
        let store = SqliteHostAgentStore::open(&path).await.unwrap();

        let BeginDeletion::Started(attempt) =
            begin_agent_deletion(&store, &agent("a1"), 5).await.unwrap()
        else {
            panic!("expected a started attempt");
        };
        let outcome = store.complete(&attempt).await.unwrap();
        assert!(matches!(outcome, CompleteDeletion::Committed(_)));

        let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM agents WHERE id = 'a1'")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(remaining, 0);
    }

    #[tokio::test]
    async fn complete_is_attempt_lost_when_the_marker_moved() {
        let dir = tempfile::tempdir().unwrap();
        let path = seed_schema(&dir, "attempt-lost.db", &[USER, HOST, ACTIVE_A1]).await;
        let store = SqliteHostAgentStore::open(&path).await.unwrap();

        let BeginDeletion::Started(attempt) =
            begin_agent_deletion(&store, &agent("a1"), 5).await.unwrap()
        else {
            panic!("expected a started attempt");
        };
        // Another attempt takes the row: the marker's timestamp is the token, so this
        // attempt no longer owns the deletion and must delete nothing.
        sqlx::query("UPDATE agents SET deletion_started_at = 999 WHERE id = 'a1'")
            .execute(store.pool())
            .await
            .unwrap();

        let outcome = store.complete(&attempt).await.unwrap();
        assert!(matches!(outcome, CompleteDeletion::AttemptLost));
        let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM agents WHERE id = 'a1'")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(remaining, 1);
    }

    #[tokio::test]
    async fn complete_is_gone_when_the_row_vanished() {
        let dir = tempfile::tempdir().unwrap();
        let path = seed_schema(&dir, "complete-gone.db", &[USER, HOST, ACTIVE_A1]).await;
        let store = SqliteHostAgentStore::open(&path).await.unwrap();

        let BeginDeletion::Started(attempt) =
            begin_agent_deletion(&store, &agent("a1"), 5).await.unwrap()
        else {
            panic!("expected a started attempt");
        };
        sqlx::query("DELETE FROM agents WHERE id = 'a1'")
            .execute(store.pool())
            .await
            .unwrap();

        let outcome = store.complete(&attempt).await.unwrap();
        assert!(matches!(outcome, CompleteDeletion::Gone));
    }
}
