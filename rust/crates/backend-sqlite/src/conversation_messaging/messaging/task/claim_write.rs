//! SQLite adapter for the task `claim` / `unclaim` write unit-of-work.
//!
//! Each verb is ONE `BEGIN IMMEDIATE`: re-read the owning conversation (+ the
//! assignment/status the classification needs) UNDER THE LOCK, run the shared
//! participation → read-only gate ([`super::write_gate`]), then the assignment CAS.
//!
//! The commands are deliberately actor-specific so a concurrent write cannot be
//! clobbered and human override semantics cannot leak into agent self-service:
//!  - `assign_self` sets the assignee only WHERE both assignee columns are NULL and
//!    the status is not terminal, and moves `todo → in_progress`; 0 rows affected is
//!    re-classified from the fresh row (already-claimed vs done vs closed);
//!  - human dispatch overwrites either assignee kind on a non-terminal task;
//!  - agent release clears assignment, rewinds status to `todo`, and clears
//!    `closed_at`; human unassign clears assignment only and preserves workflow.

use std::{path::Path, sync::Arc, time::Duration};

use silverhand_backend_core::conversation_messaging::messaging::task::{
    AgentClaimOutcome, AgentEventNames, Assignee, HumanClaimOutcome, TaskClaimWrite,
    TaskEventAssignee, TaskEventFacts, TaskEventKind, TaskEventMessage, TaskId,
    TaskMutationWitness, TaskStatus, TaskSystemActor, TaskWriteClock, UnclaimOutcome,
    compose_claim_event, compose_unclaim_event,
};
use silverhand_backend_core::foundation::identity::{AgentId, ConversationId, UserId};
use silverhand_backend_core::foundation::persistence::PersistenceFailure;
use sqlx::{QueryBuilder, Row, Sqlite, SqlitePool};

use super::event_append::append_task_event;
use super::row::{SELECT_TASK_JOINED, agent_event_names, decode_row, decode_sidecars};
use super::write_gate::{
    AgentTaskCapabilityGate, GateVerdict, agent_gate, agent_task_gate, user_gate,
};
use super::{SystemTaskWriteClock, TaskStoreError, corrupt, driver};

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const WRITE_BEGIN: &str = "BEGIN IMMEDIATE";

/// The conversation + the columns the claim/unclaim classification reads under the
/// lock: assignment (already-taken / ownership) and status (terminal / promotion).
const SELECT_CLAIM_ROW: &str = "SELECT conversation_id, status, assignee_agent_id, \
     assignee_user_id FROM tasks WHERE id = ?";

/// Assign the agent to a FREE, non-terminal task, promoting `todo → in_progress`.
/// Guarded so a race cannot double-assign: 0 rows affected means taken or terminal.
const ASSIGN_TO_AGENT: &str = "UPDATE tasks \
     SET assignee_agent_id = ?, assignee_user_id = NULL, \
     status = CASE WHEN status = 'todo' THEN 'in_progress' ELSE status END \
     WHERE id = ? AND assignee_agent_id IS NULL AND assignee_user_id IS NULL \
     AND status NOT IN ('done', 'closed')";

/// Human dispatch is an override: assign or reassign any non-terminal task.
const DISPATCH_TO_AGENT: &str = "UPDATE tasks \
     SET assignee_agent_id = ?, assignee_user_id = NULL, \
     status = CASE WHEN status = 'todo' THEN 'in_progress' ELSE status END \
     WHERE id = ? AND status NOT IN ('done', 'closed')";

/// Clear the assignee (both columns) on a non-terminal task; `status`/`closed_at`
/// untouched. The agent door adds `assignee_agent_id = ?` (ownership); the human
/// door omits it (ownership-agnostic). Terminal rows are refused BEFORE this runs,
/// so the `NOT IN` guard is defensive.
const RELEASE_TO_TODO_AGENT: &str = "UPDATE tasks SET assignee_agent_id = NULL, \
     assignee_user_id = NULL, status = 'todo', closed_at = NULL \
     WHERE id = ? AND assignee_agent_id = ? \
     AND status NOT IN ('done', 'closed')";
const CLEAR_ASSIGNEE_ANY: &str = "UPDATE tasks SET assignee_agent_id = NULL, \
     assignee_user_id = NULL WHERE id = ? AND status NOT IN ('done', 'closed')";

/// The SQLite task claim/unclaim writer. Holds only a pool.
#[derive(Clone)]
pub struct SqliteTaskClaimWrite {
    pool: SqlitePool,
    clock: Arc<dyn TaskWriteClock + Send + Sync>,
}

impl SqliteTaskClaimWrite {
    /// Opens an existing product database. A missing file is a failure.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, TaskStoreError> {
        let pool = crate::foundation::database::store_pool(path.as_ref(), BUSY_TIMEOUT).await?;
        Ok(Self {
            pool,
            clock: Arc::new(SystemTaskWriteClock),
        })
    }

    /// Construct over an already-open pool.
    pub fn from_pool(pool: SqlitePool) -> Self {
        Self {
            pool,
            clock: Arc::new(SystemTaskWriteClock),
        }
    }

    pub fn from_pool_with_clock<K: TaskWriteClock + Send + Sync + 'static>(
        pool: SqlitePool,
        clock: K,
    ) -> Self {
        Self {
            pool,
            clock: Arc::new(clock),
        }
    }
}

/// The decision-relevant assignment/status facts read under the lock.
struct ClaimRow {
    conversation_id: ConversationId,
    status: TaskStatus,
    assignee_agent_id: Option<String>,
    assignee_user_id: Option<String>,
}

impl ClaimRow {
    fn is_taken(&self) -> bool {
        self.assignee_agent_id.is_some() || self.assignee_user_id.is_some()
    }

    fn is_terminal(&self) -> bool {
        matches!(self.status, TaskStatus::Done | TaskStatus::Closed)
    }

    /// The status a claim promotes to: `todo → in_progress`, else unchanged.
    fn claimed_status(&self) -> TaskStatus {
        match self.status {
            TaskStatus::Todo => TaskStatus::InProgress,
            other => other,
        }
    }
}

/// Fetch + decode the decision-relevant claim/unclaim row.
async fn fetch_claim_row(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    task_id: &TaskId,
) -> Result<Option<ClaimRow>, PersistenceFailure> {
    let Some(row) = sqlx::query(SELECT_CLAIM_ROW)
        .bind(task_id.as_str())
        .fetch_optional(&mut **tx)
        .await
        .map_err(driver)?
    else {
        return Ok(None);
    };
    let conversation_id_raw: String = row.try_get("conversation_id").map_err(driver)?;
    let status_token: String = row.try_get("status").map_err(driver)?;
    Ok(Some(ClaimRow {
        conversation_id: ConversationId::parse(conversation_id_raw)
            .map_err(|_| corrupt("tasks.conversation_id"))?,
        status: TaskStatus::parse(&status_token).map_err(|_| corrupt("tasks.status"))?,
        assignee_agent_id: row.try_get("assignee_agent_id").map_err(driver)?,
        assignee_user_id: row.try_get("assignee_user_id").map_err(driver)?,
    }))
}

/// Read the POST-change joined snapshot and build the event facts with the given
/// actor pill, then append `event(&facts)` and return the witness. Shared by the
/// claim/unclaim Applied paths — the caller supplies the actor pill and the composer
/// closure (claim vs unclaim, whose content differs).
async fn append_assignment_event<F>(
    tx: &mut sqlx::Transaction<'_, Sqlite>,
    task_id: &TaskId,
    actor: TaskSystemActor,
    kind: TaskEventKind,
    now: i64,
    compose: F,
) -> Result<TaskMutationWitness, PersistenceFailure>
where
    F: FnOnce(&TaskEventFacts) -> TaskEventMessage,
{
    let snapshot = {
        let mut builder = QueryBuilder::<Sqlite>::new(SELECT_TASK_JOINED);
        builder.push(" WHERE t.id = ").push_bind(task_id.as_str());
        builder
            .build()
            .fetch_optional(&mut **tx)
            .await
            .map_err(driver)?
            .ok_or_else(|| corrupt("tasks.snapshot_after_assignment"))?
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
    let event = compose(&facts);
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
        kind,
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

impl TaskClaimWrite for SqliteTaskClaimWrite {
    async fn agent_claim(
        &self,
        task_id: &TaskId,
        actor: &AgentId,
        actor_names: &AgentEventNames,
    ) -> Result<AgentClaimOutcome, PersistenceFailure> {
        let mut tx = self.pool.begin_with(WRITE_BEGIN).await.map_err(driver)?;
        let now = self.clock.now();
        let Some(row) = fetch_claim_row(&mut tx, task_id).await? else {
            tx.rollback().await.map_err(driver)?;
            return Ok(AgentClaimOutcome::NotFound);
        };
        match agent_task_gate(
            &mut tx,
            &row.conversation_id,
            actor,
            AgentTaskCapabilityGate::Write,
        )
        .await?
        {
            GateVerdict::AgentNotFound => {
                tx.rollback().await.map_err(driver)?;
                return Ok(AgentClaimOutcome::AgentNotFound);
            }
            GateVerdict::NotFound => {
                tx.rollback().await.map_err(driver)?;
                return Ok(AgentClaimOutcome::NotFound);
            }
            GateVerdict::ReadOnly => {
                tx.rollback().await.map_err(driver)?;
                return Ok(AgentClaimOutcome::ConversationReadonly);
            }
            GateVerdict::Admit => {}
        }
        // Terminal check first (TS order), then the guarded assign-if-free CAS.
        if row.is_terminal() {
            let outcome = terminal_claim_outcome(row.status);
            tx.rollback().await.map_err(driver)?;
            return Ok(outcome);
        }
        let claimed_status = row.claimed_status();
        let affected = sqlx::query(ASSIGN_TO_AGENT)
            .bind(actor.as_str())
            .bind(task_id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(driver)?
            .rows_affected();
        if affected == 0 {
            // The CAS matched nothing: re-read to classify (taken vs since-terminal).
            let cur = fetch_claim_row(&mut tx, task_id).await?;
            let outcome = match cur {
                Some(cur) if cur.is_taken() => AgentClaimOutcome::AlreadyClaimed,
                Some(cur) if cur.status == TaskStatus::Closed => AgentClaimOutcome::TaskClosed,
                Some(_) => AgentClaimOutcome::TaskDone,
                None => AgentClaimOutcome::NotFound,
            };
            tx.rollback().await.map_err(driver)?;
            return Ok(outcome);
        }
        // Same-tx `task_claim` event: `@<actor> claimed task #N`.
        let actor_pill = TaskSystemActor::Agent {
            id: actor.clone(),
            names: actor_names.clone(),
        };
        let witness = append_assignment_event(
            &mut tx,
            task_id,
            actor_pill,
            TaskEventKind::Claim,
            now,
            compose_claim_event,
        )
        .await?;
        tx.commit().await.map_err(driver)?;
        Ok(AgentClaimOutcome::Claimed {
            status: claimed_status,
            witness: Box::new(witness),
        })
    }

    async fn human_claim(
        &self,
        task_id: &TaskId,
        actor: &UserId,
        assignee: &AgentId,
    ) -> Result<HumanClaimOutcome, PersistenceFailure> {
        let mut tx = self.pool.begin_with(WRITE_BEGIN).await.map_err(driver)?;
        let now = self.clock.now();
        let Some(row) = fetch_claim_row(&mut tx, task_id).await? else {
            tx.rollback().await.map_err(driver)?;
            return Ok(HumanClaimOutcome::NotFound);
        };
        match user_gate(&mut tx, &row.conversation_id, actor).await? {
            GateVerdict::AgentNotFound => return Err(corrupt("task.user_gate.agent")),
            GateVerdict::NotFound => {
                tx.rollback().await.map_err(driver)?;
                return Ok(HumanClaimOutcome::NotFound);
            }
            GateVerdict::ReadOnly => {
                tx.rollback().await.map_err(driver)?;
                return Ok(HumanClaimOutcome::ConversationReadonly);
            }
            GateVerdict::Admit => {}
        }
        if row.is_terminal() {
            let outcome = match row.status {
                TaskStatus::Closed => HumanClaimOutcome::TaskClosed,
                _ => HumanClaimOutcome::TaskDone,
            };
            tx.rollback().await.map_err(driver)?;
            return Ok(outcome);
        }
        // The chosen assignee agent must itself participate (a fire dispatches to
        // it). Judged on the same connection via the shared agent participation.
        if agent_gate(&mut tx, &row.conversation_id, assignee).await? == GateVerdict::NotFound {
            tx.rollback().await.map_err(driver)?;
            return Ok(HumanClaimOutcome::AssigneeNotConversationMember);
        }
        let claimed_status = row.claimed_status();
        let affected = sqlx::query(DISPATCH_TO_AGENT)
            .bind(assignee.as_str())
            .bind(task_id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(driver)?
            .rows_affected();
        if affected == 0 {
            let cur = fetch_claim_row(&mut tx, task_id).await?;
            let outcome = match cur {
                Some(cur) if cur.status == TaskStatus::Closed => HumanClaimOutcome::TaskClosed,
                Some(_) => HumanClaimOutcome::TaskDone,
                None => HumanClaimOutcome::NotFound,
            };
            tx.rollback().await.map_err(driver)?;
            return Ok(outcome);
        }
        // Same-tx `task_claim` event: `task #N assigned to @<assignee> (by user)`.
        let actor_pill = TaskSystemActor::User {
            id: actor.clone(),
            name: "user".to_owned(),
        };
        let witness = append_assignment_event(
            &mut tx,
            task_id,
            actor_pill,
            TaskEventKind::Claim,
            now,
            compose_claim_event,
        )
        .await?;
        tx.commit().await.map_err(driver)?;
        Ok(HumanClaimOutcome::Claimed {
            status: claimed_status,
            witness: Box::new(witness),
        })
    }

    async fn agent_unclaim(
        &self,
        task_id: &TaskId,
        actor: &AgentId,
    ) -> Result<UnclaimOutcome, PersistenceFailure> {
        let mut tx = self.pool.begin_with(WRITE_BEGIN).await.map_err(driver)?;
        let now = self.clock.now();
        let Some(row) = fetch_claim_row(&mut tx, task_id).await? else {
            tx.rollback().await.map_err(driver)?;
            return Ok(UnclaimOutcome::NotFound);
        };
        match agent_task_gate(
            &mut tx,
            &row.conversation_id,
            actor,
            AgentTaskCapabilityGate::Write,
        )
        .await?
        {
            GateVerdict::AgentNotFound => {
                tx.rollback().await.map_err(driver)?;
                return Ok(UnclaimOutcome::AgentNotFound);
            }
            GateVerdict::NotFound => {
                tx.rollback().await.map_err(driver)?;
                return Ok(UnclaimOutcome::NotFound);
            }
            GateVerdict::ReadOnly => {
                tx.rollback().await.map_err(driver)?;
                return Ok(UnclaimOutcome::ConversationReadonly);
            }
            GateVerdict::Admit => {}
        }
        // Ownership: only the assignee agent may unclaim (checked before terminal,
        // matching `unclaimTaskAgent`).
        if row.assignee_agent_id.as_deref() != Some(actor.as_str()) {
            tx.rollback().await.map_err(driver)?;
            return Ok(UnclaimOutcome::NotAssignee);
        }
        if row.is_terminal() {
            let outcome = match row.status {
                TaskStatus::Closed => UnclaimOutcome::TaskClosed,
                _ => UnclaimOutcome::TaskDone,
            };
            tx.rollback().await.map_err(driver)?;
            return Ok(outcome);
        }
        // The prior assignee's display name for the unclaim content (`was @<prev>`),
        // read from the pre-clear join before the assignee columns go NULL.
        let prior_name = prior_assignee_agent_name(&mut tx, task_id).await?;
        sqlx::query(RELEASE_TO_TODO_AGENT)
            .bind(task_id.as_str())
            .bind(actor.as_str())
            .execute(&mut *tx)
            .await
            .map_err(driver)?;
        let actor_pill = TaskSystemActor::Agent {
            id: actor.clone(),
            names: AgentEventNames::new(prior_name.clone(), prior_name.clone()),
        };
        let witness = append_assignment_event(
            &mut tx,
            task_id,
            actor_pill,
            TaskEventKind::Unclaim,
            now,
            |facts| compose_unclaim_event(facts, &prior_name),
        )
        .await?;
        tx.commit().await.map_err(driver)?;
        Ok(UnclaimOutcome::Unclaimed(Box::new(witness)))
    }

    async fn human_unclaim(
        &self,
        task_id: &TaskId,
        actor: &UserId,
    ) -> Result<UnclaimOutcome, PersistenceFailure> {
        let mut tx = self.pool.begin_with(WRITE_BEGIN).await.map_err(driver)?;
        let now = self.clock.now();
        let Some(row) = fetch_claim_row(&mut tx, task_id).await? else {
            tx.rollback().await.map_err(driver)?;
            return Ok(UnclaimOutcome::NotFound);
        };
        match user_gate(&mut tx, &row.conversation_id, actor).await? {
            GateVerdict::AgentNotFound => return Err(corrupt("task.user_gate.agent")),
            GateVerdict::NotFound => {
                tx.rollback().await.map_err(driver)?;
                return Ok(UnclaimOutcome::NotFound);
            }
            GateVerdict::ReadOnly => {
                tx.rollback().await.map_err(driver)?;
                return Ok(UnclaimOutcome::ConversationReadonly);
            }
            GateVerdict::Admit => {}
        }
        // Human unclaim is ownership-agnostic; only terminal is refused.
        if row.is_terminal() {
            let outcome = match row.status {
                TaskStatus::Closed => UnclaimOutcome::TaskClosed,
                _ => UnclaimOutcome::TaskDone,
            };
            tx.rollback().await.map_err(driver)?;
            return Ok(outcome);
        }
        // The prior assignee name is not interpolated on the human arm (content is
        // `task #N unassigned (by user)`), but the composer takes it uniformly.
        let prior_name = prior_assignee_agent_name(&mut tx, task_id).await?;
        sqlx::query(CLEAR_ASSIGNEE_ANY)
            .bind(task_id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(driver)?;
        let actor_pill = TaskSystemActor::User {
            id: actor.clone(),
            name: "user".to_owned(),
        };
        let witness = append_assignment_event(
            &mut tx,
            task_id,
            actor_pill,
            TaskEventKind::Unclaim,
            now,
            |facts| compose_unclaim_event(facts, &prior_name),
        )
        .await?;
        tx.commit().await.map_err(driver)?;
        Ok(UnclaimOutcome::Unclaimed(Box::new(witness)))
    }
}

/// The pre-clear assignee agent's display name for the unclaim event's `was @<prev>`
/// line — read from the join BEFORE the assignee columns are cleared. Falls back to
/// the id (the projector's `?? id`), or `agent` when the row is somehow unassigned.
async fn prior_assignee_agent_name(
    tx: &mut sqlx::Transaction<'_, Sqlite>,
    task_id: &TaskId,
) -> Result<String, PersistenceFailure> {
    let snapshot = {
        let mut builder = QueryBuilder::<Sqlite>::new(SELECT_TASK_JOINED);
        builder.push(" WHERE t.id = ").push_bind(task_id.as_str());
        builder
            .build()
            .fetch_optional(&mut **tx)
            .await
            .map_err(driver)?
            .ok_or_else(|| corrupt("tasks.snapshot_before_unclaim"))?
    };
    let aggregate = decode_row(&snapshot)?;
    Ok(match aggregate.assignee() {
        Assignee::Agent(agent) => agent_event_names(tx, agent)
            .await?
            .map(|names| names.content_name().to_owned())
            .unwrap_or_else(|| agent.as_str().to_owned()),
        _ => "agent".to_owned(),
    })
}

/// The claim outcome for a task already terminal at the participation/read-only
/// gate (before the CAS).
fn terminal_claim_outcome(status: TaskStatus) -> AgentClaimOutcome {
    match status {
        TaskStatus::Closed => AgentClaimOutcome::TaskClosed,
        _ => AgentClaimOutcome::TaskDone,
    }
}
