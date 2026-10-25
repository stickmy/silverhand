//! SQLite adapter for the task `status` write unit-of-work.
//!
//! Implements [`TaskStatusWrite`] as ONE `BEGIN IMMEDIATE` transaction that
//! re-reads the task's decision-relevant columns UNDER THE LOCK, runs the core
//! [`agent_status_write_decision`] on those fresh facts, and only then writes. The
//! concurrency contract: a write predicated only on `status` would let a
//! concurrent claim/unclaim move `assignee` without moving `status`, and a
//! stale-authorized change would still apply. Re-reading the full decision set
//! (`status`, both assignee columns, creator kind/id, `closed_at`) under the same
//! writer that will UPDATE forecloses that — the authorization and the transition
//! are decided against the exact row the write touches.
//!
//! It sets `status` + `closed_at` only (the `tasks` table has no `updated_at`
//! column; `closed_at` is `Some` IFF target=`closed`, per the schema CHECK the
//! witness already upholds). Before the task-row decision it re-decides, on the
//! SAME transaction connection, per-task PARTICIPATION (the shared
//! `SELECT_AGENT_PARTICIPATES` predicate) and direct-pair READ-ONLY (the
//! connection-bound `ConversationAuthorityReader::{conversation, accepts_posts}`) —
//! participation → read-only → task-owning workspace `write_tasks` → task-row,
//! matching the TS authority while closing the async preflight race. The route
//! door remains an ordering/anti-enumeration hint; it is never a write credential.

use std::{path::Path, sync::Arc, time::Duration};

use silverhand_backend_core::conversation_messaging::messaging::task::{
    AgentEventNames, AgentStatusWriteOutcome, Assignee, Creator, HumanStatusWriteOutcome,
    HumanTaskStatusWrite, TaskEventAssignee, TaskEventFacts, TaskId, TaskMutationWitness,
    TaskStatus, TaskStatusWrite, TaskSystemActor, TaskWriteClock, agent_status_write_decision,
    compose_status_event, human_status_change,
};
use silverhand_backend_core::foundation::identity::{AgentId, ConversationId, UserId};
use silverhand_backend_core::foundation::persistence::PersistenceFailure;
use sqlx::{QueryBuilder, Row, Sqlite};

use super::event_append::append_task_event;
use super::row::{SELECT_TASK_JOINED, agent_event_names, decode_row, decode_sidecars};
use super::write_gate::{AgentTaskCapabilityGate, GateVerdict, agent_task_gate, user_gate};
use super::{SystemTaskWriteClock, TaskStoreError, corrupt, driver};

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// The decision read and the flip share ONE reserved-writer transaction, so a
/// concurrent claim/unclaim surfaces before this write commits rather than as a
/// stale-authorized apply.
const WRITE_BEGIN: &str = "BEGIN IMMEDIATE";

/// The decision-relevant columns, read under the write lock: the owning
/// conversation (for participation + read-only), the status the transition is
/// judged from, both assignee columns (assignee-self / unassigned), the creator
/// pair (creator-self-stale), and the current close instant.
const SELECT_DECISION_ROW: &str = "SELECT conversation_id, status, assignee_agent_id, \
     assignee_user_id, creator_kind, creator_agent_id, creator_user_id, closed_at \
     FROM tasks WHERE id = ?";

/// The flip, by id alone — the enclosing `IMMEDIATE` transaction already read the
/// row under the writer, so the decision and the write see the SAME row; no
/// separate `status=?` predicate is needed and none would add safety the lock
/// does not already give. Sets `status` + `closed_at` (no `updated_at` column).
const APPLY_STATUS: &str = "UPDATE tasks SET status = ?, closed_at = ? WHERE id = ?";

/// The SQLite task status writer. The clock is sampled after reserving the writer
/// and stamps both `closed_at` and the atomic task-event message.
#[derive(Clone)]
pub struct SqliteTaskStatusWrite {
    pool: crate::foundation::database::SqliteDatabase,
    clock: Arc<dyn TaskWriteClock + Send + Sync>,
}

impl SqliteTaskStatusWrite {
    /// Opens an existing product database. A missing file is a failure, not an
    /// empty database: the schema has an owner, and this writer is not it.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, TaskStoreError> {
        let pool = crate::foundation::database::store_pool(path.as_ref(), BUSY_TIMEOUT).await?;
        Ok(Self {
            pool,
            clock: Arc::new(SystemTaskWriteClock),
        })
    }

    /// Construct over an already-open pool (composition / tests sharing one product
    /// database across the task authorities).
    pub fn from_pool(pool: impl Into<crate::foundation::database::SqliteDatabase>) -> Self {
        let pool = pool.into();
        Self {
            pool,
            clock: Arc::new(SystemTaskWriteClock),
        }
    }

    pub fn from_pool_with_clock<K: TaskWriteClock + Send + Sync + 'static>(
        pool: crate::foundation::database::SqliteDatabase,
        clock: K,
    ) -> Self {
        Self {
            pool,
            clock: Arc::new(clock),
        }
    }
}

/// Decode the assignee columns into the domain [`Assignee`]. The schema's XOR
/// CHECK guarantees at most one is set; a row that violated it (both set) is
/// corruption, reported as a safe-500 rather than silently favouring one.
fn decode_assignee(
    agent: Option<String>,
    user: Option<String>,
) -> Result<Assignee, PersistenceFailure> {
    match (agent, user) {
        (Some(_), Some(_)) => Err(corrupt("tasks.assignee")),
        (Some(agent), None) => Ok(Assignee::Agent(
            AgentId::parse(agent).map_err(|_| corrupt("tasks.assignee_agent_id"))?,
        )),
        (None, Some(user)) => Ok(Assignee::User(
            UserId::parse(user).map_err(|_| corrupt("tasks.assignee_user_id"))?,
        )),
        (None, None) => Ok(Assignee::Unassigned),
    }
}

/// Decode the creator pair into the domain [`Creator`], mirroring the schema's
/// `creator_kind` ↔ id CHECK. Any shape the CHECK forbids is corruption.
fn decode_creator(
    kind: String,
    agent: Option<String>,
    user: Option<String>,
) -> Result<Creator, PersistenceFailure> {
    match (kind.as_str(), agent, user) {
        ("agent", Some(agent), None) => Ok(Creator::Agent(
            AgentId::parse(agent).map_err(|_| corrupt("tasks.creator_agent_id"))?,
        )),
        ("user", None, Some(user)) => Ok(Creator::User(
            UserId::parse(user).map_err(|_| corrupt("tasks.creator_user_id"))?,
        )),
        ("system", None, None) => Ok(Creator::System),
        _ => Err(corrupt("tasks.creator_kind")),
    }
}

impl TaskStatusWrite for SqliteTaskStatusWrite {
    async fn apply_agent_status(
        &self,
        task_id: &TaskId,
        actor: &AgentId,
        actor_names: &AgentEventNames,
        target: TaskStatus,
    ) -> Result<AgentStatusWriteOutcome, PersistenceFailure> {
        let mut tx = self.pool.begin_with(WRITE_BEGIN).await.map_err(driver)?;
        let now = self.clock.now();

        let Some(row) = sqlx::query(SELECT_DECISION_ROW)
            .bind(task_id.as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(driver)?
        else {
            tx.rollback().await.map_err(driver)?;
            return Ok(AgentStatusWriteOutcome::NotFound);
        };

        let conversation_id_raw: String = row.try_get("conversation_id").map_err(driver)?;
        let conversation_id = ConversationId::parse(conversation_id_raw)
            .map_err(|_| corrupt("tasks.conversation_id"))?;

        // ORDER (TS `updateStatusAgent` tx): participation → read-only → task-row,
        // via the shared in-tx write gate (same-connection SQL, no external port).
        match agent_task_gate(
            &mut tx,
            &conversation_id,
            actor,
            AgentTaskCapabilityGate::Write,
        )
        .await?
        {
            GateVerdict::AgentNotFound => {
                tx.rollback().await.map_err(driver)?;
                return Ok(AgentStatusWriteOutcome::AgentNotFound);
            }
            GateVerdict::NotFound => {
                tx.rollback().await.map_err(driver)?;
                return Ok(AgentStatusWriteOutcome::NotFound);
            }
            GateVerdict::ReadOnly => {
                tx.rollback().await.map_err(driver)?;
                return Ok(AgentStatusWriteOutcome::ConversationReadonly);
            }
            GateVerdict::Admit => {}
        }

        let status_token: String = row.try_get("status").map_err(driver)?;
        let current_status =
            TaskStatus::parse(&status_token).map_err(|_| corrupt("tasks.status"))?;
        let assignee = decode_assignee(
            row.try_get("assignee_agent_id").map_err(driver)?,
            row.try_get("assignee_user_id").map_err(driver)?,
        )?;
        let creator = decode_creator(
            row.try_get("creator_kind").map_err(driver)?,
            row.try_get("creator_agent_id").map_err(driver)?,
            row.try_get::<Option<String>, _>("creator_user_id")
                .map_err(driver)?,
        )?;
        let current_closed_at: Option<i64> = row.try_get("closed_at").map_err(driver)?;

        // Decide the authorization AND the transition against the fresh row, as one
        // unit, under the lock.
        let change = match agent_status_write_decision(
            actor,
            current_status,
            &assignee,
            &creator,
            current_closed_at,
            target,
            now,
        ) {
            Ok(change) => change,
            Err(refusal) => {
                tx.rollback().await.map_err(driver)?;
                return Ok(AgentStatusWriteOutcome::Refused(refusal));
            }
        };

        sqlx::query(APPLY_STATUS)
            .bind(change.target().as_str())
            .bind(change.closed_at())
            .bind(task_id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(driver)?;

        // Same-transaction `task_status` event over the POST-change snapshot, so the
        // notice commits atomically with the row. The actor pill is the agent's
        // resolved name.
        let actor_pill = TaskSystemActor::Agent {
            id: actor.clone(),
            names: actor_names.clone(),
        };
        let witness = append_status_event(&mut tx, task_id, actor_pill, now).await?;
        tx.commit().await.map_err(driver)?;
        Ok(AgentStatusWriteOutcome::Applied(Box::new(witness)))
    }
}

/// Re-read the POST-change joined snapshot (number/status/title/assignee names) on
/// the writer's own transaction, compose the `task_status` event, append it, and
/// return the mutation witness. Shared by the agent and human status arms — the
/// only difference is the actor pill the caller passes.
async fn append_status_event(
    tx: &mut sqlx::Transaction<'_, Sqlite>,
    task_id: &TaskId,
    actor: TaskSystemActor,
    now: i64,
) -> Result<TaskMutationWitness, PersistenceFailure> {
    let snapshot = {
        let mut builder = QueryBuilder::<Sqlite>::new(SELECT_TASK_JOINED);
        builder.push(" WHERE t.id = ").push_bind(task_id.as_str());
        builder
            .build()
            .fetch_optional(&mut **tx)
            .await
            .map_err(driver)?
            .ok_or_else(|| corrupt("tasks.snapshot_after_status"))?
    };
    let aggregate = decode_row(&snapshot)?;
    let sidecars = decode_sidecars(&snapshot)?;
    let assignee = match aggregate.assignee() {
        Assignee::Unassigned => None,
        Assignee::Agent(agent) => Some(TaskEventAssignee::Agent {
            id: agent.clone(),
            names: agent_event_names(tx, agent)
                .await?
                .unwrap_or_else(|| AgentEventNames::new(agent.as_str(), agent.as_str())),
        }),
        Assignee::User(user) => Some(TaskEventAssignee::User {
            id: user.clone(),
            name: sidecars
                .assignee_user_name
                .clone()
                .unwrap_or_else(|| user.as_str().to_owned()),
        }),
    };
    let facts = TaskEventFacts {
        task_id: task_id.clone(),
        task_number: aggregate.task_number(),
        title: aggregate.title().clone(),
        status: aggregate.status(),
        assignee: assignee.clone(),
        actor,
    };
    let event = compose_status_event(&facts);
    let thread_root = aggregate.thread_root_id().map(|t| t.as_str().to_owned());
    let event_message = append_task_event(
        tx,
        aggregate.conversation_id(),
        thread_root.as_deref(),
        &event,
        now,
    )
    .await?;
    Ok(TaskMutationWitness {
        kind:
            silverhand_backend_core::conversation_messaging::messaging::task::TaskEventKind::Status,
        task_id: task_id.clone(),
        task_number: aggregate.task_number(),
        event_message,
        conversation_id: aggregate.conversation_id().clone(),
        board: aggregate.board().clone(),
        thread_root_id: aggregate.thread_root_id().cloned(),
        title: aggregate.title().clone(),
        status: aggregate.status(),
        assignee,
        closed_at: aggregate.closed_at(),
        source_message_id: aggregate.source_message().cloned(),
    })
}

/// The HUMAN decision-relevant columns: the owning conversation (participation +
/// read-only) and the current status (`already_at` no-op) + close instant. The
/// human override reads NO assignee/creator/transition facts.
const SELECT_HUMAN_DECISION_ROW: &str =
    "SELECT conversation_id, status, closed_at FROM tasks WHERE id = ?";

impl HumanTaskStatusWrite for SqliteTaskStatusWrite {
    async fn apply_human_status(
        &self,
        task_id: &TaskId,
        actor: &UserId,
        target: TaskStatus,
    ) -> Result<HumanStatusWriteOutcome, PersistenceFailure> {
        let mut tx = self.pool.begin_with(WRITE_BEGIN).await.map_err(driver)?;
        let now = self.clock.now();

        let Some(row) = sqlx::query(SELECT_HUMAN_DECISION_ROW)
            .bind(task_id.as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(driver)?
        else {
            tx.rollback().await.map_err(driver)?;
            return Ok(HumanStatusWriteOutcome::NotFound);
        };

        let conversation_id_raw: String = row.try_get("conversation_id").map_err(driver)?;
        let conversation_id = ConversationId::parse(conversation_id_raw)
            .map_err(|_| corrupt("tasks.conversation_id"))?;

        // Participation → read-only for the user actor, via the shared in-tx gate.
        match user_gate(&mut tx, &conversation_id, actor).await? {
            GateVerdict::AgentNotFound => return Err(corrupt("task.user_gate.agent")),
            GateVerdict::NotFound => {
                tx.rollback().await.map_err(driver)?;
                return Ok(HumanStatusWriteOutcome::NotFound);
            }
            GateVerdict::ReadOnly => {
                tx.rollback().await.map_err(driver)?;
                return Ok(HumanStatusWriteOutcome::ConversationReadonly);
            }
            GateVerdict::Admit => {}
        }

        let status_token: String = row.try_get("status").map_err(driver)?;
        let current_status =
            TaskStatus::parse(&status_token).map_err(|_| corrupt("tasks.status"))?;
        let current_closed_at: Option<i64> = row.try_get("closed_at").map_err(driver)?;

        // The human OVERRIDE decision: no transition table, no assignee gate — only
        // the same-status no-op refusal.
        let change = match human_status_change(current_status, target, current_closed_at, now) {
            Ok(change) => change,
            Err(_already) => {
                tx.rollback().await.map_err(driver)?;
                return Ok(HumanStatusWriteOutcome::AlreadyAtStatus);
            }
        };

        sqlx::query(APPLY_STATUS)
            .bind(change.target().as_str())
            .bind(change.closed_at())
            .bind(task_id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(driver)?;

        // A human actor's status event pill is the literal `user` (the TS
        // `taskSystemActorFor`), so no name lookup; the append is otherwise the same.
        let actor_pill = TaskSystemActor::User {
            id: actor.clone(),
            name: "user".to_owned(),
        };
        let witness = append_status_event(&mut tx, task_id, actor_pill, now).await?;
        tx.commit().await.map_err(driver)?;
        Ok(HumanStatusWriteOutcome::Applied(Box::new(witness)))
    }
}
