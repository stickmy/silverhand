//! Assignment by an agent or a human: override the assignee, keep the status, and
//! notify a human assignee.

use silverhand_backend_core::conversation_messaging::messaging::PrincipalKind;
use silverhand_backend_core::conversation_messaging::messaging::task::{
    AssignOutcome, AssignTarget, Assignee, AssigneeNotice, AssignmentRefusal, TaskAssigner,
    TaskEventKind, TaskId, TaskMutationWitness, TaskSystemActor, assignment_open,
    compose_assign_event,
};
use silverhand_backend_core::foundation::identity::{AgentId, ConversationId, UserId, WorkspaceId};
use silverhand_backend_core::foundation::persistence::PersistenceFailure;
use sqlx::{Row, Sqlite};

use super::super::super::mention_self::{MentionSelfRow, insert_mention_self};
use super::super::write_gate::{
    AgentTaskCapabilityGate, GateVerdict, agent_gate, agent_task_gate, user_gate,
};
use super::super::{corrupt, driver};
use super::{SqliteTaskClaimWrite, WRITE_BEGIN, append_assignment_event, fetch_claim_row};

/// Assignment overrides any current assignee and keeps the status.
const ASSIGN_OVERRIDE: &str = "UPDATE tasks SET assignee_agent_id = ?, assignee_user_id = ? \
     WHERE id = ? AND status = ?";

/// The participant of the conversation holding `handle` (at most one: agent and user
/// names are unique across both tables).
const SELECT_PARTICIPANT_BY_HANDLE: &str = "SELECT 'agent' AS kind, a.id AS id FROM agents a \
     JOIN conversations c ON c.id = ?1 WHERE a.name = ?2 AND \
     ((c.placement_kind = 'workspace' AND EXISTS (SELECT 1 FROM conversation_agents ca WHERE \
     ca.conversation_id = c.id AND ca.agent_id = a.id)) OR (c.placement_kind = 'direct_pair' AND \
     ((c.endpoint_a_kind = 'agent' AND c.endpoint_a_id = a.id) OR (c.endpoint_b_kind = 'agent' \
     AND c.endpoint_b_id = a.id)))) \
     UNION ALL SELECT 'user' AS kind, u.id AS id FROM users u JOIN conversations c ON c.id = ?1 \
     WHERE u.name = ?2 AND ((c.placement_kind = 'workspace' AND EXISTS (SELECT 1 FROM \
     conversation_users cu WHERE cu.conversation_id = c.id AND cu.user_id = u.id) AND EXISTS \
     (SELECT 1 FROM workspace_users wu WHERE wu.workspace_id = c.workspace_id AND \
     wu.user_id = u.id)) OR (c.placement_kind = 'direct_pair' AND ((c.endpoint_a_kind = 'human' \
     AND c.endpoint_a_id = u.id) OR (c.endpoint_b_kind = 'human' AND c.endpoint_b_id = u.id))))";

const SELECT_NOTICE_PLACE: &str =
    "SELECT workspace_id, roster_policy, title FROM conversations WHERE id = ?";

pub(super) async fn assign(
    store: &SqliteTaskClaimWrite,
    task_id: &TaskId,
    actor: &TaskAssigner,
    target: &AssignTarget,
) -> Result<AssignOutcome, PersistenceFailure> {
    let mut tx = store.pool.begin_with(WRITE_BEGIN).await.map_err(driver)?;
    let now = store.clock.now();
    let Some(row) = fetch_claim_row(&mut tx, task_id).await? else {
        tx.rollback().await.map_err(driver)?;
        return Ok(AssignOutcome::NotFound);
    };
    let verdict = match actor {
        TaskAssigner::Agent { id, .. } => {
            agent_task_gate(
                &mut tx,
                &row.conversation_id,
                id,
                AgentTaskCapabilityGate::Write,
            )
            .await?
        }
        TaskAssigner::User(user) => match user_gate(&mut tx, &row.conversation_id, user).await? {
            GateVerdict::AgentNotFound => return Err(corrupt("task.user_gate.agent")),
            verdict => verdict,
        },
    };
    let refused = match verdict {
        GateVerdict::Admit => None,
        GateVerdict::AgentNotFound => Some(AssignOutcome::AgentNotFound),
        GateVerdict::NotFound => Some(AssignOutcome::NotFound),
        GateVerdict::ReadOnly => Some(AssignOutcome::ConversationReadonly),
    };
    if let Some(outcome) = refused.or_else(|| match assignment_open(row.status) {
        Ok(()) => None,
        Err(AssignmentRefusal::TaskDone) => Some(AssignOutcome::TaskDone),
        Err(AssignmentRefusal::TaskClosed) => Some(AssignOutcome::TaskClosed),
    }) {
        tx.rollback().await.map_err(driver)?;
        return Ok(outcome);
    }
    let assignee = match resolve_assignee(&mut tx, &row.conversation_id, target).await? {
        Ok(assignee) => assignee,
        Err(outcome) => {
            tx.rollback().await.map_err(driver)?;
            return Ok(outcome);
        }
    };
    let unchanged = match &assignee {
        Assignee::Agent(id) => row.assignee_agent_id.as_deref() == Some(id.as_str()),
        Assignee::User(id) => row.assignee_user_id.as_deref() == Some(id.as_str()),
        Assignee::Unassigned => false,
    };
    if unchanged {
        tx.rollback().await.map_err(driver)?;
        return Ok(AssignOutcome::Unchanged {
            task_id: task_id.clone(),
            task_number: row.task_number,
        });
    }
    let (agent, user) = match &assignee {
        Assignee::Agent(id) => (Some(id.as_str()), None),
        Assignee::User(id) => (None, Some(id.as_str())),
        Assignee::Unassigned => return Err(corrupt("task.assign.unassigned")),
    };
    sqlx::query(ASSIGN_OVERRIDE)
        .bind(agent)
        .bind(user)
        .bind(task_id.as_str())
        .bind(row.status.as_str())
        .execute(&mut *tx)
        .await
        .map_err(driver)?;
    let actor_pill = match actor {
        TaskAssigner::Agent { id, names } => TaskSystemActor::Agent {
            id: id.clone(),
            names: names.clone(),
        },
        TaskAssigner::User(id) => TaskSystemActor::User {
            id: id.clone(),
            name: "user".to_owned(),
        },
    };
    let witness = append_assignment_event(
        &mut tx,
        task_id,
        actor_pill,
        TaskEventKind::Assign,
        now,
        compose_assign_event,
    )
    .await?;
    let notice = match &assignee {
        Assignee::User(viewer) if !matches!(actor, TaskAssigner::User(a) if a == viewer) => {
            Some(write_assignee_notice(&mut tx, viewer, &witness, actor, now).await?)
        }
        _ => None,
    };
    tx.commit().await.map_err(driver)?;
    Ok(AssignOutcome::Assigned {
        witness: Box::new(witness),
        notice,
    })
}

/// Resolve the request's target to a participating assignee, judged on the write
/// transaction so a removal racing the request cannot be assigned.
async fn resolve_assignee(
    tx: &mut sqlx::Transaction<'_, Sqlite>,
    conversation: &ConversationId,
    target: &AssignTarget,
) -> Result<Result<Assignee, AssignOutcome>, PersistenceFailure> {
    match target {
        AssignTarget::Agent(id) => Ok(match agent_gate(tx, conversation, id).await? {
            GateVerdict::NotFound => Err(AssignOutcome::AssigneeNotConversationMember),
            _ => Ok(Assignee::Agent(id.clone())),
        }),
        AssignTarget::User(id) => Ok(match user_gate(tx, conversation, id).await? {
            GateVerdict::NotFound => Err(AssignOutcome::AssigneeNotConversationMember),
            _ => Ok(Assignee::User(id.clone())),
        }),
        AssignTarget::Handle { handle, kind } => {
            let rows = sqlx::query(SELECT_PARTICIPANT_BY_HANDLE)
                .bind(conversation.as_str())
                .bind(handle.as_str())
                .fetch_all(&mut **tx)
                .await
                .map_err(driver)?;
            let mut found = None;
            for row in rows {
                let row_kind: String = row.try_get("kind").map_err(driver)?;
                let id: String = row.try_get("id").map_err(driver)?;
                let assignee = match row_kind.as_str() {
                    "agent" => {
                        Assignee::Agent(AgentId::parse(id).map_err(|_| corrupt("agents.id"))?)
                    }
                    _ => Assignee::User(UserId::parse(id).map_err(|_| corrupt("users.id"))?),
                };
                let wanted = matches!(
                    (kind, &assignee),
                    (None, _)
                        | (Some(PrincipalKind::Agent), Assignee::Agent(_))
                        | (Some(PrincipalKind::Human), Assignee::User(_))
                );
                if wanted {
                    found = Some(assignee);
                }
            }
            Ok(found.ok_or(AssignOutcome::AssigneeNotConversationMember))
        }
    }
}

async fn write_assignee_notice(
    tx: &mut sqlx::Transaction<'_, Sqlite>,
    viewer: &UserId,
    witness: &TaskMutationWitness,
    actor: &TaskAssigner,
    now: i64,
) -> Result<AssigneeNotice, PersistenceFailure> {
    let place = sqlx::query(SELECT_NOTICE_PLACE)
        .bind(witness.conversation_id.as_str())
        .fetch_one(&mut **tx)
        .await
        .map_err(driver)?;
    let workspace: Option<String> = place.try_get("workspace_id").map_err(driver)?;
    let workspace_id = workspace
        .map(WorkspaceId::parse)
        .transpose()
        .map_err(|_| corrupt("conversations.workspace_id"))?;
    let fixed = place
        .try_get::<String, _>("roster_policy")
        .map_err(driver)?
        == "fixed";
    let conversation_title = if fixed {
        None
    } else {
        place.try_get("title").map_err(driver)?
    };
    let trigger_name = match actor {
        TaskAssigner::Agent { names, .. } => names.display_name().to_owned(),
        TaskAssigner::User(_) => "user".to_owned(),
    };
    let payload = serde_json::json!({
        "conversation_title": conversation_title,
        "trigger_name": trigger_name,
        "mention_excerpt": witness.event_message.message.content,
    })
    .to_string();
    let notification_id = super::super::create_write::new_id()?;
    insert_mention_self(
        tx,
        MentionSelfRow {
            id: &notification_id,
            viewer: viewer.as_str(),
            workspace_id: workspace_id.as_ref().map(WorkspaceId::as_str),
            conversation_id: witness.conversation_id.as_str(),
            message_id: witness.event_message.id.as_str(),
            payload,
            created_at: now,
        },
    )
    .await
    .map_err(driver)?;
    Ok(AssigneeNotice {
        viewer: viewer.clone(),
        notification_id,
        workspace_id,
        conversation_title,
    })
}
