//! SQLite adapter for the task `edit` write unit-of-work.
//!
//! Implements [`TaskEditWrite`] as ONE `BEGIN IMMEDIATE` transaction: re-read the
//! task's owning conversation + current title/content UNDER THE LOCK, run the
//! shared write gate (participation → read-only, [`super::write_gate`]), then the
//! core canonical no-op guard ([`plan_edit`]) over the fresh values. On a real
//! change it `COALESCE`-binds `title`/`content` (a `None` field left unchanged) and
//! stamps the `last_editor` triple from the editor; a no-op writes nothing.
//!
//! Edit has NO assignee gate for either actor kind. Agent edits replay both
//! `write_tasks` and `read_tasks` from the task-owning workspace under the same
//! reserved writer; the async route door is only an early refusal/order hint.

use std::{path::Path, sync::Arc, time::Duration};

use silverhand_backend_core::conversation_messaging::messaging::ThreadRootId as MessagingThreadRootId;
use silverhand_backend_core::conversation_messaging::messaging::task::{
    AgentEventNames, Assignee, CreatedEventMessageId, EditPlan, ReadViewer,
    TaskCommittedEventMessage, TaskContent, TaskEditSuccess, TaskEditWitness, TaskEditWrite,
    TaskEditWriteOutcome, TaskEditor, TaskEventAssignee, TaskEventFacts, TaskEventKind, TaskId,
    TaskReadAddress, TaskSystemActor, TaskTitle, TaskWriteClock, VisibleTaskPoint,
    compose_edit_event, plan_edit,
};
use silverhand_backend_core::foundation::identity::ConversationId;
use silverhand_backend_core::foundation::persistence::PersistenceFailure;
use sqlx::{QueryBuilder, Row, Sqlite, SqlitePool};

use super::super::{
    FROM_KIND_SYSTEM, INSERT_MESSAGE, MessagingStoreError, ReserveSeqFault, bump_stream_head_on,
    reserve_next_seq_on,
};
use super::point_detail::fetch_point_on;
use super::row::{SELECT_TASK_JOINED, agent_event_names, decode_row, decode_sidecars};
use super::write_gate::{AgentTaskCapabilityGate, GateVerdict, agent_task_gate, user_gate};
use super::{SystemTaskWriteClock, TaskStoreError, corrupt, driver};

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const WRITE_BEGIN: &str = "BEGIN IMMEDIATE";

/// The edit decision-relevant columns, read under the write lock: the owning
/// conversation (participation + read-only) and the current title/content the
/// no-op guard compares against.
const SELECT_EDIT_ROW: &str = "SELECT conversation_id, title, content FROM tasks WHERE id = ?";

/// A random base64url id (12 bytes, mirroring `randomBytes(12).base64url`).
const ID_BYTES: usize = 12;

fn new_id() -> Result<String, PersistenceFailure> {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    let mut bytes = [0_u8; ID_BYTES];
    getrandom::fill(&mut bytes)
        .map_err(|error| PersistenceFailure::from_source(TaskStoreError::Entropy(error)))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// The edit UPDATE: `COALESCE(?, column)` leaves a `None`-bound field unchanged; a
/// change witness carries the requested values verbatim. The `last_editor` triple
/// is stamped together (the schema CHECK requires all-null or a complete triple).
/// The `tasks` table has no `updated_at`.
const APPLY_EDIT: &str = "UPDATE tasks SET \
     title = COALESCE(?, title), content = COALESCE(?, content), \
     last_editor_kind = ?, last_editor_id = ?, last_edited_at = ? \
     WHERE id = ?";

/// The SQLite task edit writer. Holds only a pool.
#[derive(Clone)]
pub struct SqliteTaskEditWrite {
    pool: SqlitePool,
    clock: Arc<dyn TaskWriteClock + Send + Sync>,
}

impl SqliteTaskEditWrite {
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

/// The `(kind, id)` the `last_editor` triple stamps for an editor.
fn editor_columns(editor: &TaskEditor) -> (&'static str, &str) {
    match editor {
        TaskEditor::Agent(agent) => ("agent", agent.as_str()),
        TaskEditor::Human(user) => ("user", user.as_str()),
    }
}

impl TaskEditWrite for SqliteTaskEditWrite {
    async fn apply_edit(
        &self,
        task_id: &TaskId,
        editor: &TaskEditor,
        editor_names: Option<&AgentEventNames>,
        requested_title: Option<TaskTitle>,
        requested_content: Option<TaskContent>,
    ) -> Result<TaskEditWriteOutcome, PersistenceFailure> {
        let mut tx = self.pool.begin_with(WRITE_BEGIN).await.map_err(driver)?;
        let now = self.clock.now();

        let Some(row) = sqlx::query(SELECT_EDIT_ROW)
            .bind(task_id.as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(driver)?
        else {
            tx.rollback().await.map_err(driver)?;
            return Ok(TaskEditWriteOutcome::NotFound);
        };

        let conversation_id_raw: String = row.try_get("conversation_id").map_err(driver)?;
        let conversation_id = ConversationId::parse(conversation_id_raw)
            .map_err(|_| corrupt("tasks.conversation_id"))?;

        // Participation → read-only, on this transaction's connection, in the TS
        // order — before the task-row (no-op) decision.
        let verdict = match editor {
            TaskEditor::Agent(agent) => {
                agent_task_gate(
                    &mut tx,
                    &conversation_id,
                    agent,
                    AgentTaskCapabilityGate::WriteAndRead,
                )
                .await?
            }
            TaskEditor::Human(user) => user_gate(&mut tx, &conversation_id, user).await?,
        };
        match verdict {
            GateVerdict::AgentNotFound => {
                tx.rollback().await.map_err(driver)?;
                return Ok(TaskEditWriteOutcome::AgentNotFound);
            }
            GateVerdict::NotFound => {
                tx.rollback().await.map_err(driver)?;
                return Ok(TaskEditWriteOutcome::NotFound);
            }
            GateVerdict::ReadOnly => {
                tx.rollback().await.map_err(driver)?;
                return Ok(TaskEditWriteOutcome::ConversationReadonly);
            }
            GateVerdict::Admit => {}
        }

        // The canonical no-op guard over the FRESH title/content.
        let current_title_raw: String = row.try_get("title").map_err(driver)?;
        let current_content_raw: String = row.try_get("content").map_err(driver)?;
        let current_title =
            TaskTitle::parse(&current_title_raw).map_err(|_| corrupt("tasks.title"))?;
        let current_content = TaskContent::from_persisted(current_content_raw);

        let change = match plan_edit(
            &current_title,
            &current_content,
            requested_title,
            requested_content,
        ) {
            EditPlan::NoOp => {
                let candidate = fetch_point_on(&mut tx, &TaskReadAddress::ById(task_id.clone()))
                    .await?
                    .ok_or_else(|| corrupt("tasks.snapshot_after_noop"))?;
                let viewer = match editor {
                    TaskEditor::Agent(agent) => ReadViewer::Agent(agent.clone()),
                    TaskEditor::Human(user) => ReadViewer::User(user.clone()),
                };
                let detail = VisibleTaskPoint::from_committed_edit(viewer, candidate)
                    .ok_or_else(|| corrupt("tasks.edit_detail_navigation"))?;
                tx.rollback().await.map_err(driver)?;
                return Ok(TaskEditWriteOutcome::NoOp(TaskEditSuccess {
                    detail,
                    witness: None,
                }));
            }
            EditPlan::Change(change) => change,
        };

        let (new_title, new_content) = change.into_parts();
        let (editor_kind, editor_id) = editor_columns(editor);
        sqlx::query(APPLY_EDIT)
            .bind(new_title.as_ref().map(TaskTitle::as_str))
            .bind(new_content.as_ref().map(TaskContent::as_str))
            .bind(editor_kind)
            .bind(editor_id)
            .bind(now)
            .bind(task_id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(driver)?;

        // POST-UPDATE snapshot in the SAME transaction: the joined read gives the
        // post-change title/status + the assignee's display name, so the appended
        // `task_edit` event carries the fresh title, never the stale pre-edit one.
        // A missing row here (the row we just updated) is a hard invariant break,
        // not a degrade — a safe-500, never a fall-back to the pre-edit title.
        let snapshot = {
            let mut builder = QueryBuilder::<Sqlite>::new(SELECT_TASK_JOINED);
            builder.push(" WHERE t.id = ").push_bind(task_id.as_str());
            builder
                .build()
                .fetch_optional(&mut *tx)
                .await
                .map_err(driver)?
                .ok_or_else(|| corrupt("tasks.snapshot_after_update"))?
        };
        let aggregate = decode_row(&snapshot)?;
        let sidecars = decode_sidecars(&snapshot)?;

        // The assignee DTO: the joined display name, falling back to the id when the
        // actor join is absent (the projector's `?? id` parity). Edit never changes
        // the assignee, so this is the current owner.
        let assignee = match aggregate.assignee() {
            Assignee::Unassigned => None,
            Assignee::Agent(agent) => Some(TaskEventAssignee::Agent {
                id: agent.clone(),
                names: agent_event_names(&mut tx, agent)
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
        let actor = match editor {
            TaskEditor::Agent(agent) => TaskSystemActor::Agent {
                id: agent.clone(),
                names: editor_names
                    .cloned()
                    .ok_or_else(|| corrupt("task_edit.agent_event_names"))?,
            },
            TaskEditor::Human(user) => TaskSystemActor::User {
                id: user.clone(),
                name: "user".to_owned(),
            },
        };
        let facts = TaskEventFacts {
            task_id: task_id.clone(),
            task_number: aggregate.task_number(),
            title: aggregate.title().clone(),
            status: aggregate.status(),
            assignee: assignee.clone(),
            actor,
        };
        let event = compose_edit_event(&facts);

        // Append the `task_edit` system message on the same connection, reusing the
        // shared seq reservation + insert + stream-head advance — the same
        // primitives the create writer uses. UPDATE + this append commit atomically.
        let message_id = new_id()?;
        let thread_root = aggregate.thread_root_id().map(|t| t.as_str().to_owned());
        let seq = reserve_next_seq_on(&mut tx, &conversation_id)
            .await
            .map_err(|fault| match fault {
                ReserveSeqFault::Driver(error) => driver(error),
                ReserveSeqFault::Ordinal(error) => {
                    PersistenceFailure::from_source(MessagingStoreError::Ordinal(error))
                }
            })?;
        sqlx::query(INSERT_MESSAGE)
            .bind(&message_id)
            .bind(conversation_id.as_str())
            .bind(FROM_KIND_SYSTEM)
            .bind(None::<&str>)
            .bind(None::<&str>)
            .bind(&event.content)
            .bind(&event.metadata)
            .bind(seq.get())
            .bind(now)
            .bind(thread_root.as_deref())
            .bind(None::<&str>)
            .bind(None::<&str>)
            .bind(None::<&str>)
            .bind(None::<&str>)
            .execute(&mut *tx)
            .await
            .map_err(driver)?;
        let message_thread_root = thread_root
            .as_deref()
            .map(MessagingThreadRootId::parse)
            .transpose()
            .map_err(|_| corrupt("tasks.thread_root_id"))?;
        bump_stream_head_on(&mut tx, &conversation_id, message_thread_root.as_ref(), seq)
            .await
            .map_err(driver)?;

        let witness = TaskEditWitness {
            kind: TaskEventKind::Edit,
            task_id: task_id.clone(),
            task_number: aggregate.task_number(),
            event_message: TaskCommittedEventMessage {
                id: CreatedEventMessageId::parse(message_id)
                    .map_err(|_| corrupt("task_edit.event_message_id"))?,
                conversation_id: conversation_id.clone(),
                thread_root_id: aggregate.thread_root_id().cloned(),
                message: event,
                seq: seq.get(),
                created_at: now,
            },
            conversation_id,
            board: aggregate.board().clone(),
            thread_root_id: aggregate.thread_root_id().cloned(),
            title: aggregate.title().clone(),
            status: aggregate.status(),
            assignee,
            closed_at: aggregate.closed_at(),
            source_message_id: aggregate.source_message().cloned(),
        };
        let candidate = fetch_point_on(&mut tx, &TaskReadAddress::ById(task_id.clone()))
            .await?
            .ok_or_else(|| corrupt("tasks.snapshot_after_edit"))?;
        let viewer = match editor {
            TaskEditor::Agent(agent) => ReadViewer::Agent(agent.clone()),
            TaskEditor::Human(user) => ReadViewer::User(user.clone()),
        };
        let detail = VisibleTaskPoint::from_committed_edit(viewer, candidate)
            .ok_or_else(|| corrupt("tasks.edit_detail_navigation"))?;
        tx.commit().await.map_err(driver)?;
        Ok(TaskEditWriteOutcome::Applied(TaskEditSuccess {
            detail,
            witness: Some(Box::new(witness)),
        }))
    }
}
