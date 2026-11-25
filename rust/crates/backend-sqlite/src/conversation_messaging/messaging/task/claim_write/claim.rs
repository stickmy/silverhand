//! Agent self-claim: assign-if-free (or already the actor's) and start the task.

use silverhand_backend_core::conversation_messaging::messaging::task::{
    AgentClaimOutcome, AgentEventNames, AssignmentRefusal, TaskEventKind, TaskId, TaskStatus,
    TaskSystemActor, assignment_open, claim_status_change, compose_claim_event,
};
use silverhand_backend_core::foundation::identity::AgentId;
use silverhand_backend_core::foundation::persistence::PersistenceFailure;

use super::super::driver;
use super::super::write_gate::{AgentTaskCapabilityGate, GateVerdict, agent_task_gate};
use super::{
    SqliteTaskClaimWrite, WRITE_BEGIN, append_assignment_event, bind_change, fetch_claim_row,
};

/// Self-claim a task that is free or already the actor's. Status and `closed_at`
/// come from the planner's `TaskStatusChange`; the `status = ?` guard is the
/// status it planned from, so 0 rows affected means taken.
const ASSIGN_TO_AGENT: &str = "UPDATE tasks \
     SET assignee_agent_id = ?1, assignee_user_id = NULL, status = ?2, closed_at = ?3 \
     WHERE id = ?4 AND status = ?5 AND assignee_user_id IS NULL \
     AND (assignee_agent_id IS NULL OR assignee_agent_id = ?1)";

pub(super) async fn agent_claim(
    store: &SqliteTaskClaimWrite,
    task_id: &TaskId,
    actor: &AgentId,
    actor_names: &AgentEventNames,
) -> Result<AgentClaimOutcome, PersistenceFailure> {
    let mut tx = store.pool.begin_with(WRITE_BEGIN).await.map_err(driver)?;
    let now = store.clock.now();
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
    let own = row.assignee_agent_id.as_deref() == Some(actor.as_str());
    if own && row.status != TaskStatus::Todo && assignment_open(row.status).is_ok() {
        tx.rollback().await.map_err(driver)?;
        return Ok(AgentClaimOutcome::AlreadyYours {
            task_id: task_id.clone(),
            task_number: row.task_number,
        });
    }
    let change = match claim_status_change(row.status) {
        Ok(change) => change,
        Err(refusal) => {
            tx.rollback().await.map_err(driver)?;
            return Ok(match refusal {
                AssignmentRefusal::TaskDone => AgentClaimOutcome::TaskDone,
                AssignmentRefusal::TaskClosed => AgentClaimOutcome::TaskClosed,
            });
        }
    };
    let affected = bind_change(sqlx::query(ASSIGN_TO_AGENT).bind(actor.as_str()), change)
        .bind(task_id.as_str())
        .bind(row.status.as_str())
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
        status: change.target(),
        witness: Box::new(witness),
    })
}
