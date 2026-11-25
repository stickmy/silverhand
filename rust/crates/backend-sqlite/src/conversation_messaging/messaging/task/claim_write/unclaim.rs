//! Unclaim: agent release (rewinds to `todo`) and human unassign (status kept).

use silverhand_backend_core::conversation_messaging::messaging::task::{
    AgentEventNames, Assignee, AssignmentRefusal, TaskEventKind, TaskId, TaskSystemActor,
    UnclaimOutcome, agent_release_status_change, assignment_open, compose_unclaim_event,
};
use silverhand_backend_core::foundation::identity::{AgentId, UserId};
use silverhand_backend_core::foundation::persistence::PersistenceFailure;
use sqlx::{QueryBuilder, Sqlite};

use super::super::row::{SELECT_TASK_JOINED, agent_event_names, decode_row};
use super::super::write_gate::{AgentTaskCapabilityGate, GateVerdict, agent_task_gate, user_gate};
use super::super::{corrupt, driver};
use super::{
    SqliteTaskClaimWrite, WRITE_BEGIN, append_assignment_event, bind_change, fetch_claim_row,
};

/// Agent release clears assignment and applies the planned rewind; ownership-guarded.
const RELEASE_TO_TODO_AGENT: &str = "UPDATE tasks SET assignee_agent_id = NULL, \
     assignee_user_id = NULL, status = ?, closed_at = ? \
     WHERE id = ? AND assignee_agent_id = ? AND status = ?";
/// Human unassign clears assignment only; workflow status is preserved.
const CLEAR_ASSIGNEE_ANY: &str = "UPDATE tasks SET assignee_agent_id = NULL, \
     assignee_user_id = NULL WHERE id = ? AND status = ?";

pub(super) async fn agent_unclaim(
    store: &SqliteTaskClaimWrite,
    task_id: &TaskId,
    actor: &AgentId,
) -> Result<UnclaimOutcome, PersistenceFailure> {
    let mut tx = store.pool.begin_with(WRITE_BEGIN).await.map_err(driver)?;
    let now = store.clock.now();
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
    let change = match agent_release_status_change(row.status) {
        Ok(change) => change,
        Err(refusal) => {
            tx.rollback().await.map_err(driver)?;
            return Ok(unclaim_refusal(refusal));
        }
    };
    // The prior assignee's display name for the unclaim content (`was @<prev>`),
    // read from the pre-clear join before the assignee columns go NULL.
    let prior_name = prior_assignee_agent_name(&mut tx, task_id).await?;
    bind_change(sqlx::query(RELEASE_TO_TODO_AGENT), change)
        .bind(task_id.as_str())
        .bind(actor.as_str())
        .bind(row.status.as_str())
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

pub(super) async fn human_unclaim(
    store: &SqliteTaskClaimWrite,
    task_id: &TaskId,
    actor: &UserId,
) -> Result<UnclaimOutcome, PersistenceFailure> {
    let mut tx = store.pool.begin_with(WRITE_BEGIN).await.map_err(driver)?;
    let now = store.clock.now();
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
    if let Err(refusal) = assignment_open(row.status) {
        tx.rollback().await.map_err(driver)?;
        return Ok(unclaim_refusal(refusal));
    }
    // The prior assignee name is not interpolated on the human arm (content is
    // `task #N unassigned (by user)`), but the composer takes it uniformly.
    let prior_name = prior_assignee_agent_name(&mut tx, task_id).await?;
    sqlx::query(CLEAR_ASSIGNEE_ANY)
        .bind(task_id.as_str())
        .bind(row.status.as_str())
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

fn unclaim_refusal(refusal: AssignmentRefusal) -> UnclaimOutcome {
    match refusal {
        AssignmentRefusal::TaskDone => UnclaimOutcome::TaskDone,
        AssignmentRefusal::TaskClosed => UnclaimOutcome::TaskClosed,
    }
}
