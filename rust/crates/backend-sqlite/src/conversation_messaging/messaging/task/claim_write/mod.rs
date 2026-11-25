//! SQLite adapter for the task `claim` / `assign` / `unclaim` write units-of-work.
//!
//! Each verb is ONE `BEGIN IMMEDIATE`: re-read the owning conversation (+ the
//! assignment/status the classification needs) UNDER THE LOCK, run the shared
//! participation → read-only gate ([`super::write_gate`]), then the assignment CAS.
//!
//! The commands are deliberately actor-specific so a concurrent write cannot be
//! clobbered and human override semantics cannot leak into agent self-service:
//!  - agent self-claim sets the assignee only WHERE both assignee columns are NULL
//!    (or already the actor) and the status is not terminal, and moves
//!    `todo → in_progress`; 0 rows affected is re-classified from the fresh row;
//!  - `assign` overwrites either assignee kind on a non-terminal task and leaves
//!    status alone;
//!  - agent release clears assignment, rewinds status to `todo`, and clears
//!    `closed_at`; human unassign clears assignment only and preserves workflow.

mod assign;
mod claim;
mod unclaim;

use std::{path::Path, sync::Arc, time::Duration};

use silverhand_backend_core::conversation_messaging::messaging::task::{
    AgentClaimOutcome, AgentEventNames, AssignOutcome, AssignTarget, Assignee, TaskAssigner,
    TaskClaimWrite, TaskEventAssignee, TaskEventFacts, TaskEventKind, TaskEventMessage, TaskId,
    TaskMutationWitness, TaskNumber, TaskStatus, TaskStatusChange, TaskSystemActor, TaskWriteClock,
    UnclaimOutcome,
};
use silverhand_backend_core::foundation::identity::{AgentId, ConversationId, UserId};
use silverhand_backend_core::foundation::persistence::PersistenceFailure;
use sqlx::{QueryBuilder, Row, Sqlite};

use super::event_append::append_task_event;
use super::row::{SELECT_TASK_JOINED, agent_event_names, decode_row, decode_sidecars};
use super::{SystemTaskWriteClock, TaskStoreError, corrupt, driver};

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const WRITE_BEGIN: &str = "BEGIN IMMEDIATE";

/// The conversation + the columns the claim/unclaim classification reads under the
/// lock: assignment (already-taken / ownership) and status (terminal / promotion).
const SELECT_CLAIM_ROW: &str = "SELECT conversation_id, task_number, status, assignee_agent_id, \
     assignee_user_id FROM tasks WHERE id = ?";

/// The SQLite task claim/unclaim writer. Holds only a pool.
#[derive(Clone)]
pub struct SqliteTaskClaimWrite {
    pool: crate::foundation::database::SqliteDatabase,
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

/// The decision-relevant assignment/status facts read under the lock.
pub(super) struct ClaimRow {
    pub(super) conversation_id: ConversationId,
    pub(super) task_number: TaskNumber,
    pub(super) status: TaskStatus,
    pub(super) assignee_agent_id: Option<String>,
    pub(super) assignee_user_id: Option<String>,
}

impl ClaimRow {
    pub(super) fn is_taken(&self) -> bool {
        self.assignee_agent_id.is_some() || self.assignee_user_id.is_some()
    }
}

/// Fetch + decode the decision-relevant claim/unclaim row.
pub(super) async fn fetch_claim_row(
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
        task_number: TaskNumber::new(row.try_get("task_number").map_err(driver)?)
            .map_err(|_| corrupt("tasks.task_number"))?,
        status: TaskStatus::parse(&status_token).map_err(|_| corrupt("tasks.status"))?,
        assignee_agent_id: row.try_get("assignee_agent_id").map_err(driver)?,
        assignee_user_id: row.try_get("assignee_user_id").map_err(driver)?,
    }))
}

/// Read the POST-change joined snapshot and build the event facts with the given
/// actor pill, then append `event(&facts)` and return the witness. Shared by the
/// claim/unclaim Applied paths — the caller supplies the actor pill and the composer
/// closure (claim vs unclaim, whose content differs).
pub(super) async fn append_assignment_event<F>(
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

/// Bind the planner's `(status, closed_at)` pair together, in that order.
pub(super) fn bind_change<'q>(
    query: sqlx::query::Query<'q, Sqlite, sqlx::sqlite::SqliteArguments>,
    change: TaskStatusChange,
) -> sqlx::query::Query<'q, Sqlite, sqlx::sqlite::SqliteArguments> {
    query
        .bind(change.target().as_str())
        .bind(change.closed_at())
}

impl TaskClaimWrite for SqliteTaskClaimWrite {
    async fn agent_claim(
        &self,
        task_id: &TaskId,
        actor: &AgentId,
        actor_names: &AgentEventNames,
    ) -> Result<AgentClaimOutcome, PersistenceFailure> {
        claim::agent_claim(self, task_id, actor, actor_names).await
    }

    async fn assign(
        &self,
        task_id: &TaskId,
        actor: &TaskAssigner,
        target: &AssignTarget,
    ) -> Result<AssignOutcome, PersistenceFailure> {
        assign::assign(self, task_id, actor, target).await
    }

    async fn agent_unclaim(
        &self,
        task_id: &TaskId,
        actor: &AgentId,
    ) -> Result<UnclaimOutcome, PersistenceFailure> {
        unclaim::agent_unclaim(self, task_id, actor).await
    }

    async fn human_unclaim(
        &self,
        task_id: &TaskId,
        actor: &UserId,
    ) -> Result<UnclaimOutcome, PersistenceFailure> {
        unclaim::human_unclaim(self, task_id, actor).await
    }
}
