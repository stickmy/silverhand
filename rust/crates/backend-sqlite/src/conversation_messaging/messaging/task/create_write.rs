//! SQLite adapter for the task `create` write unit-of-work.
//!
//! One indivisible durable fact in ONE `BEGIN IMMEDIATE`, in the order pinned:
//! allocate the next per-board task number → insert the task row → append
//! the `task_create` system message → bind `created_event_message_id` → commit.
//! Any step failing rolls the whole back, so there is never a task without its
//! anchoring message, nor a message without its task.
//!
//! It reuses the crate's existing connection-bound message primitives
//! ([`reserve_next_seq_on`], [`INSERT_MESSAGE`], [`bump_stream_head_on`],
//! [`FROM_KIND_SYSTEM`]) rather than a new generic message-write authority, and
//! serves ONLY task-create: it copies no onboarding semantics and no send
//! admission. The number allocation is `MAX(task_number)+1 WHERE board_key` inside
//! the reserved writer, with the schema's `UNIQUE(board_key, task_number)` as the
//! race backstop; a duplicate `source_message_id` (the permanent 1:1 provenance
//! rule) surfaces as a typed `SourceConflict`.

use std::{collections::HashMap, path::Path, sync::Arc, time::Duration};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use silverhand_backend_core::conversation_messaging::messaging::ThreadRootId as MessagingThreadRootId;
use silverhand_backend_core::conversation_messaging::messaging::task::{
    BoardKey, CreatedEventMessageId, ExistingSourceTask, TaskCommittedEventMessage,
    TaskCreateBatch, TaskCreateOutcome, TaskCreateWrite, TaskCreatedWitness, TaskCreator,
    TaskEventFacts, TaskEventKind, TaskId, TaskMutationWitness, TaskNumber, TaskStatus,
    TaskWriteCapability, TaskWriteClock, compose_create_event,
};
use silverhand_backend_core::foundation::persistence::PersistenceFailure;
use sqlx::Row;

use super::super::{
    FROM_KIND_SYSTEM, INSERT_MESSAGE, MessagingStoreError, ReserveSeqFault, bump_stream_head_on,
    reserve_next_seq_on,
};
use super::write_gate::{GateVerdict, agent_exists, agent_gate, user_gate};
use super::{SystemTaskWriteClock, TaskStoreError, corrupt, driver};

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const WRITE_BEGIN: &str = "BEGIN IMMEDIATE";

/// Allocate the next per-board task number inside the reserved writer. The
/// `UNIQUE(board_key, task_number)` index is the concurrency backstop; the
/// `IMMEDIATE` writer serializes concurrent creates on the same board so each gets
/// a distinct consecutive number.
const ALLOCATE_TASK_NUMBER: &str =
    "SELECT COALESCE(MAX(task_number), 0) + 1 AS next FROM tasks WHERE board_key = ?";

/// Insert the task row. `status` is `in_progress` when self-claimed at create, else
/// `todo`; the `board_key`-derivation trigger enforces the board arc.
const INSERT_TASK: &str = "INSERT INTO tasks \
     (id, workspace_id, board_key, task_number, conversation_id, thread_root_id, \
      title, content, status, assignee_agent_id, assignee_user_id, \
      creator_kind, creator_agent_id, creator_user_id, source_message_id, \
      created_event_message_id, created_at) \
     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, ?, ?, ?, ?, NULL, ?)";

/// Bind the just-appended `task_create` message as the task's created-event anchor.
const BACKFILL_CREATED_EVENT: &str = "UPDATE tasks SET created_event_message_id = ? WHERE id = ?";

/// A random base64url id (12 bytes, mirroring `randomBytes(12).base64url`).
const ID_BYTES: usize = 12;

fn new_id() -> Result<String, PersistenceFailure> {
    let mut bytes = [0_u8; ID_BYTES];
    getrandom::fill(&mut bytes)
        .map_err(|error| PersistenceFailure::from_source(TaskStoreError::Entropy(error)))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// The SQLite task create writer.
#[derive(Clone)]
pub struct SqliteTaskCreateWrite {
    pool: crate::foundation::database::SqliteDatabase,
    clock: Arc<dyn TaskWriteClock + Send + Sync>,
}

impl SqliteTaskCreateWrite {
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

/// `true` if a driver error is the `source_message_id` uniqueness violation — the
/// permanent 1:1 provenance rule the create must report as `SourceConflict` rather
/// than a raw driver failure.
fn is_source_conflict(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .is_some_and(|db| db.is_unique_violation() && db.message().contains("source_message_id"))
}

/// `(creator_kind, creator_agent_id, creator_user_id)` for the row's creator triple.
fn creator_columns(creator: &TaskCreator) -> (&'static str, Option<&str>, Option<&str>) {
    match creator {
        TaskCreator::Agent(agent) => ("agent", Some(agent.as_str()), None),
        TaskCreator::Human(user) => ("user", None, Some(user.as_str())),
    }
}

fn storage_board_key(board: &BoardKey) -> String {
    match board {
        BoardKey::Workspace(workspace) => format!("ws:{}", workspace.as_str()),
        BoardKey::Pair(pair) => format!("pair:{}", pair.as_str()),
    }
}

/// Read the task an existing `source_message_id` already backs, for the
/// `SourceConflict` winner (best-effort — a `None` still refuses the batch).
const SELECT_TASK_BY_SOURCE: &str =
    "SELECT id, task_number, status FROM tasks WHERE source_message_id = ?";
const SELECT_DESTINATION_LIFECYCLE: &str =
    "SELECT closed_at, archived_at FROM conversations WHERE id = ?";
const SELECT_SOURCE_CONVERSATION: &str = "SELECT conversation_id FROM messages WHERE id = ?";
const RESOLVE_CAPABILITY: &str = "SELECT CASE WHEN w.owner_agent_id = ? OR EXISTS ( \
     SELECT 1 FROM workspace_agent_capabilities c WHERE c.workspace_id = w.id \
     AND c.agent_id = ? AND c.capability = ?) THEN 1 ELSE 0 END \
     FROM workspaces w WHERE w.id = ?";

async fn has_capability(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    workspace: &silverhand_backend_core::foundation::identity::WorkspaceId,
    agent: &silverhand_backend_core::foundation::identity::AgentId,
    capability: &'static str,
) -> Result<bool, PersistenceFailure> {
    let granted: Option<i64> = sqlx::query_scalar(RESOLVE_CAPABILITY)
        .bind(agent.as_str())
        .bind(agent.as_str())
        .bind(capability)
        .bind(workspace.as_str())
        .fetch_optional(&mut **tx)
        .await
        .map_err(driver)?;
    Ok(granted.unwrap_or(0) != 0)
}

impl TaskCreateWrite for SqliteTaskCreateWrite {
    async fn create_tasks(
        &self,
        batch: &TaskCreateBatch,
    ) -> Result<TaskCreateOutcome, PersistenceFailure> {
        // An empty batch is not this authority's to reject (admission caught it); a
        // caller that reaches here with one has nothing to write.
        if batch.items().is_empty() {
            return Ok(TaskCreateOutcome::Created(Vec::new()));
        }
        let conversation = batch.conversation_id().clone();

        let mut tx = self.pool.begin_with(WRITE_BEGIN).await.map_err(driver)?;

        // Re-prove lifecycle, participation, and direct-pair writability under the
        // SAME reserved writer as the batch. Pre-admission fixes refusal order; this
        // closes lifecycle/roster races before any task or event is inserted.
        let Some(lifecycle) = sqlx::query(SELECT_DESTINATION_LIFECYCLE)
            .bind(conversation.as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(driver)?
        else {
            tx.rollback().await.map_err(driver)?;
            return Ok(TaskCreateOutcome::ConversationNotFound);
        };
        if lifecycle
            .try_get::<Option<i64>, _>("closed_at")
            .map_err(driver)?
            .is_some()
        {
            tx.rollback().await.map_err(driver)?;
            return Ok(TaskCreateOutcome::ConversationClosed);
        }
        if lifecycle
            .try_get::<Option<i64>, _>("archived_at")
            .map_err(driver)?
            .is_some()
        {
            tx.rollback().await.map_err(driver)?;
            return Ok(TaskCreateOutcome::ConversationArchived);
        }
        let creator_gate = match batch.creator() {
            TaskCreator::Agent(agent) => {
                if !agent_exists(&mut tx, agent).await? {
                    tx.rollback().await.map_err(driver)?;
                    return Ok(TaskCreateOutcome::AgentNotFound);
                }
                agent_gate(&mut tx, &conversation, agent).await?
            }
            TaskCreator::Human(user) => user_gate(&mut tx, &conversation, user).await?,
        };
        match creator_gate {
            GateVerdict::AgentNotFound => return Err(corrupt("task.creator_gate.agent")),
            GateVerdict::NotFound => {
                tx.rollback().await.map_err(driver)?;
                return Ok(TaskCreateOutcome::NotConversationMember);
            }
            GateVerdict::ReadOnly => {
                tx.rollback().await.map_err(driver)?;
                return Ok(TaskCreateOutcome::ConversationReadonly);
            }
            GateVerdict::Admit => {}
        }

        if let (TaskCreator::Agent(agent), BoardKey::Workspace(workspace)) =
            (batch.creator(), batch.board())
        {
            if !has_capability(&mut tx, workspace, agent, "create_tasks").await? {
                tx.rollback().await.map_err(driver)?;
                return Ok(TaskCreateOutcome::CapabilityDenied(
                    TaskWriteCapability::Create,
                ));
            }
            if batch.items().iter().any(|item| item.claim_by.is_some())
                && !has_capability(&mut tx, workspace, agent, "write_tasks").await?
            {
                tx.rollback().await.map_err(driver)?;
                return Ok(TaskCreateOutcome::CapabilityDenied(
                    TaskWriteCapability::Write,
                ));
            }
        }

        let mut distinct_sources = std::collections::HashSet::new();
        for source in batch
            .items()
            .iter()
            .filter_map(|item| item.source_message_id.as_ref())
        {
            if !distinct_sources.insert(source.as_str()) {
                continue;
            }
            let source_conversation: Option<String> =
                sqlx::query_scalar(SELECT_SOURCE_CONVERSATION)
                    .bind(source.as_str())
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(driver)?;
            let Some(source_conversation) = source_conversation else {
                tx.rollback().await.map_err(driver)?;
                return Ok(TaskCreateOutcome::SourceMessageNotFound);
            };
            if source_conversation != batch.conversation_id().as_str() {
                tx.rollback().await.map_err(driver)?;
                return Ok(TaskCreateOutcome::SourceConversationMismatch);
            }
            // Same destination + the fresh creator participation gate above is the
            // transaction-bound readability witness; no second async authority is used.
        }

        // Snapshot bindings that PRE-DATE this batch. A duplicate within the same
        // batch must not report the first (soon-to-be-rolled-back) item as an
        // `existing_task`; only a committed winner may be surfaced.
        let mut preexisting_sources = HashMap::new();
        for plan in batch.items() {
            if let Some(source) = &plan.source_message_id
                && let Some(existing) = lookup_existing_source(&mut tx, source.as_str()).await?
            {
                preexisting_sources.insert(source.as_str().to_owned(), existing);
            }
        }
        if let Some(existing_task) = batch.items().iter().find_map(|plan| {
            plan.source_message_id
                .as_ref()
                .and_then(|source| preexisting_sources.get(source.as_str()))
                .cloned()
        }) {
            tx.rollback().await.map_err(driver)?;
            return Ok(TaskCreateOutcome::SourceConflict {
                existing_task: Some(existing_task),
            });
        }

        let now = self.clock.now();

        let mut witnesses = Vec::with_capacity(batch.items().len());
        for plan in batch.items() {
            let board_key = storage_board_key(batch.board());
            // 1. Allocate the next per-board number under the reserved writer. Within
            //    the same tx, a prior item's insert is visible, so consecutive numbers
            //    are handed out across the batch.
            let next_number: i64 = sqlx::query(ALLOCATE_TASK_NUMBER)
                .bind(&board_key)
                .fetch_one(&mut *tx)
                .await
                .map_err(driver)?
                .try_get("next")
                .map_err(driver)?;

            let task_id = new_id()?;
            let status = if plan.claim_by.is_some() {
                TaskStatus::InProgress
            } else {
                TaskStatus::Todo
            };
            let (creator_kind, creator_agent, creator_user) = creator_columns(batch.creator());

            // 2. Insert the task row. A duplicate source_message_id — from a prior
            //    task OR an earlier item in THIS batch naming the same source — is the
            //    typed SourceConflict; it rolls the WHOLE batch back (nothing written).
            let insert = sqlx::query(INSERT_TASK)
                .bind(&task_id)
                .bind(batch.board().workspace_id().map(|w| w.as_str()))
                .bind(&board_key)
                .bind(next_number)
                .bind(batch.conversation_id().as_str())
                .bind(plan.thread_root_id.as_ref().map(|t| t.as_str()))
                .bind(plan.title.as_str())
                .bind(plan.content.as_str())
                .bind(status.as_str())
                .bind(plan.claim_by.as_ref().map(|a| a.as_str()))
                .bind(creator_kind)
                .bind(creator_agent)
                .bind(creator_user)
                .bind(plan.source_message_id.as_ref().map(|s| s.as_str()))
                .bind(now)
                .execute(&mut *tx)
                .await;
            if let Err(error) = &insert
                && is_source_conflict(error)
            {
                tx.rollback().await.map_err(driver)?;
                return Ok(TaskCreateOutcome::SourceConflict {
                    existing_task: None,
                });
            }
            insert.map_err(driver)?;

            // 3. Compose the `task_create` event over the REAL allocated number, then
            //    append it on the same connection (content/metadata carry the true #N).
            let facts = TaskEventFacts {
                task_id: TaskId::parse(task_id.clone()).map_err(|_| corrupt("tasks.id"))?,
                task_number: TaskNumber::new(next_number)
                    .map_err(|_| corrupt("tasks.task_number"))?,
                title: plan.title.clone(),
                status,
                assignee: plan.event_assignee.clone(),
                actor: plan.event_actor.clone(),
            };
            let event = compose_create_event(&facts, plan.event_placement);
            let message_id = new_id()?;
            let seq = reserve_next_seq_on(&mut tx, batch.conversation_id())
                .await
                .map_err(|fault| match fault {
                    ReserveSeqFault::Driver(error) => driver(error),
                    ReserveSeqFault::Ordinal(error) => {
                        PersistenceFailure::from_source(MessagingStoreError::Ordinal(error))
                    }
                })?;
            sqlx::query(INSERT_MESSAGE)
                .bind(&message_id)
                .bind(batch.conversation_id().as_str())
                .bind(FROM_KIND_SYSTEM)
                .bind(None::<&str>)
                .bind(None::<&str>)
                .bind(&event.content)
                .bind(&event.metadata)
                .bind(seq.get())
                .bind(now)
                .bind(plan.thread_root_id.as_ref().map(|t| t.as_str()))
                .bind(None::<&str>)
                .bind(None::<&str>)
                .bind(None::<&str>)
                .bind(None::<&str>)
                .execute(&mut *tx)
                .await
                .map_err(driver)?;
            let message_thread_root = plan
                .thread_root_id
                .as_ref()
                .map(|t| MessagingThreadRootId::parse(t.as_str()))
                .transpose()
                .map_err(|_| corrupt("tasks.thread_root_id"))?;
            bump_stream_head_on(
                &mut tx,
                batch.conversation_id(),
                message_thread_root.as_ref(),
                seq,
            )
            .await
            .map_err(driver)?;

            // 4. Bind the message as the created-event anchor.
            sqlx::query(BACKFILL_CREATED_EVENT)
                .bind(&message_id)
                .bind(&task_id)
                .execute(&mut *tx)
                .await
                .map_err(driver)?;

            let mutation = TaskMutationWitness {
                kind: TaskEventKind::Create,
                task_id: TaskId::parse(task_id).map_err(|_| corrupt("tasks.id"))?,
                task_number: TaskNumber::new(next_number)
                    .map_err(|_| corrupt("tasks.task_number"))?,
                event_message: TaskCommittedEventMessage {
                    id: CreatedEventMessageId::parse(message_id)
                        .map_err(|_| corrupt("tasks.created_event_message_id"))?,
                    conversation_id: batch.conversation_id().clone(),
                    thread_root_id: plan.thread_root_id.clone(),
                    message: event,
                    seq: seq.get(),
                    created_at: now,
                },
                conversation_id: batch.conversation_id().clone(),
                board: batch.board().clone(),
                thread_root_id: plan.thread_root_id.clone(),
                title: plan.title.clone(),
                status,
                assignee: plan.event_assignee.clone(),
                closed_at: None,
                source_message_id: plan.source_message_id.clone(),
            };
            witnesses.push(
                TaskCreatedWitness::new(mutation)
                    .ok_or_else(|| corrupt("task_create.witness_kind"))?,
            );
        }

        tx.commit().await.map_err(driver)?;
        Ok(TaskCreateOutcome::Created(witnesses))
    }
}

/// The existing task an already-bound `source_message_id` backs, for the
/// `SourceConflict` winner. Read on the batch's own connection before rollback.
async fn lookup_existing_source(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    source: &str,
) -> Result<Option<ExistingSourceTask>, PersistenceFailure> {
    let Some(row) = sqlx::query(SELECT_TASK_BY_SOURCE)
        .bind(source)
        .fetch_optional(&mut **tx)
        .await
        .map_err(driver)?
    else {
        return Ok(None);
    };
    let id: String = row.try_get("id").map_err(driver)?;
    let number: i64 = row.try_get("task_number").map_err(driver)?;
    let status: String = row.try_get("status").map_err(driver)?;
    Ok(Some(ExistingSourceTask {
        task_id: TaskId::parse(id).map_err(|_| corrupt("tasks.id"))?,
        task_number: TaskNumber::new(number).map_err(|_| corrupt("tasks.task_number"))?,
        status: TaskStatus::parse(&status).map_err(|_| corrupt("tasks.status"))?,
    }))
}
