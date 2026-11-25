//! SQLite adapter for the task `claim` / `unclaim` write unit-of-work.
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

use std::{path::Path, sync::Arc, time::Duration};

use silverhand_backend_core::conversation_messaging::messaging::PrincipalKind;
use silverhand_backend_core::conversation_messaging::messaging::task::{
    AgentClaimOutcome, AgentEventNames, AssignOutcome, AssignTarget, Assignee, AssigneeNotice,
    AssignmentRefusal, TaskAssigner, TaskClaimWrite, TaskEventAssignee, TaskEventFacts,
    TaskEventKind, TaskEventMessage, TaskId, TaskMutationWitness, TaskNumber, TaskStatus,
    TaskStatusChange, TaskSystemActor, TaskWriteClock, UnclaimOutcome, agent_release_status_change,
    assignment_open, claim_status_change, compose_assign_event, compose_claim_event,
    compose_unclaim_event,
};
use silverhand_backend_core::foundation::identity::{AgentId, ConversationId, UserId, WorkspaceId};
use silverhand_backend_core::foundation::persistence::PersistenceFailure;
use sqlx::{QueryBuilder, Row, Sqlite};

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
const SELECT_CLAIM_ROW: &str = "SELECT conversation_id, task_number, status, assignee_agent_id, \
     assignee_user_id FROM tasks WHERE id = ?";

/// Self-claim a task that is free or already the actor's. Status and `closed_at`
/// come from the planner's [`TaskStatusChange`]; the `status = ?` guard is the
/// status it planned from, so 0 rows affected means taken.
const ASSIGN_TO_AGENT: &str = "UPDATE tasks \
     SET assignee_agent_id = ?1, assignee_user_id = NULL, status = ?2, closed_at = ?3 \
     WHERE id = ?4 AND status = ?5 AND assignee_user_id IS NULL \
     AND (assignee_agent_id IS NULL OR assignee_agent_id = ?1)";

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

/// The human assignee's notice: a `mention_self` row anchored on the event message,
/// so it rides the existing directed-mention badge, Activity and read clearing.
const INSERT_ASSIGNEE_NOTICE: &str = "INSERT INTO notifications (id, kind, user_id, workspace_id, \
     conversation_id, message_id, source_workspace_id, source_conversation_id, source_message_id, \
     payload, created_at) VALUES (?1, 'mention_self', ?2, ?3, ?4, ?5, ?3, ?4, ?5, ?6, ?7)";

/// Agent release clears assignment and applies the planned rewind; ownership-guarded.
const RELEASE_TO_TODO_AGENT: &str = "UPDATE tasks SET assignee_agent_id = NULL, \
     assignee_user_id = NULL, status = ?, closed_at = ? \
     WHERE id = ? AND assignee_agent_id = ? AND status = ?";
/// Human unassign clears assignment only; workflow status is preserved.
const CLEAR_ASSIGNEE_ANY: &str = "UPDATE tasks SET assignee_agent_id = NULL, \
     assignee_user_id = NULL WHERE id = ? AND status = ?";

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
struct ClaimRow {
    conversation_id: ConversationId,
    task_number: TaskNumber,
    status: TaskStatus,
    assignee_agent_id: Option<String>,
    assignee_user_id: Option<String>,
}

impl ClaimRow {
    fn is_taken(&self) -> bool {
        self.assignee_agent_id.is_some() || self.assignee_user_id.is_some()
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

    async fn assign(
        &self,
        task_id: &TaskId,
        actor: &TaskAssigner,
        target: &AssignTarget,
    ) -> Result<AssignOutcome, PersistenceFailure> {
        let mut tx = self.pool.begin_with(WRITE_BEGIN).await.map_err(driver)?;
        let now = self.clock.now();
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
            TaskAssigner::User(user) => match user_gate(&mut tx, &row.conversation_id, user).await?
            {
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
            TaskEventKind::Claim,
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
    let notification_id = super::create_write::new_id()?;
    sqlx::query(INSERT_ASSIGNEE_NOTICE)
        .bind(&notification_id)
        .bind(viewer.as_str())
        .bind(workspace_id.as_ref().map(WorkspaceId::as_str))
        .bind(witness.conversation_id.as_str())
        .bind(witness.event_message.id.as_str())
        .bind(payload)
        .bind(now)
        .execute(&mut **tx)
        .await
        .map_err(driver)?;
    Ok(AssigneeNotice {
        viewer: viewer.clone(),
        notification_id,
        workspace_id,
        conversation_title,
    })
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

/// Bind the planner's `(status, closed_at)` pair together, in that order.
fn bind_change<'q>(
    query: sqlx::query::Query<'q, Sqlite, sqlx::sqlite::SqliteArguments>,
    change: TaskStatusChange,
) -> sqlx::query::Query<'q, Sqlite, sqlx::sqlite::SqliteArguments> {
    query
        .bind(change.target().as_str())
        .bind(change.closed_at())
}

fn unclaim_refusal(refusal: AssignmentRefusal) -> UnclaimOutcome {
    match refusal {
        AssignmentRefusal::TaskDone => UnclaimOutcome::TaskDone,
        AssignmentRefusal::TaskClosed => UnclaimOutcome::TaskClosed,
    }
}
