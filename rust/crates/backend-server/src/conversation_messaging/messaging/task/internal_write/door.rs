//! The agent task-write DOOR shared by every by-ref face: which task a ref names
//! and whether the acting agent may reach it, plus the typed refusal envelope.
//!
//! Order (parity with the TS faces): by-id → conversation frame refused as
//! `task_not_found` (§4.5), actor exists, by-id capability; by-number → source
//! frame board, actor exists, [`resolve_write_board_with_source_board`], workspace
//! capabilities in order, number → id. The write UoW re-proves participation under
//! its own lock; the door only settles the target.

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use silverhand_backend_core::conversation_messaging::messaging::task::{
    AgentExistenceReader, TaskByIdCapability, TaskId, TaskNumber, TaskNumberResolver,
    TaskWriteCapability, TaskWriteDoorReads,
};
use silverhand_backend_core::foundation::identity::{AgentId, ConversationId};

use super::selector::{
    BoardResolveRefusal, ResolvedBoard, WorkspaceSelector, resolve_write_board_with_source_board,
};
use crate::conversation_messaging::messaging::task::TaskSourcePlacementSnapshot;
use crate::foundation::http_error::plain_500 as safe_500;
use crate::foundation::wire_patch::empty_selector_as_none;

/// A single task ref: exactly one of a global id or a per-board number.
pub(crate) enum TaskRef {
    ById(TaskId),
    ByNumber {
        task_number: i64,
        selector: WorkspaceSelector,
    },
}

impl TaskRef {
    /// Exactly one of `task_number` / `task_id`; the selectors are mutually
    /// exclusive and barred on a by-id ref. Every refusal is the minimal `422`.
    pub(crate) fn from_wire(
        task_number: Option<i64>,
        task_id: Option<String>,
        workspace_id: Option<String>,
        workspace_name: Option<String>,
    ) -> Result<Self, ()> {
        let workspace_id = empty_selector_as_none(workspace_id);
        let workspace_name = empty_selector_as_none(workspace_name);
        match (task_number, task_id) {
            (Some(_), Some(_)) | (None, None) => Err(()),
            (Some(task_number), None) => {
                if workspace_id.is_some() && workspace_name.is_some() {
                    return Err(());
                }
                Ok(Self::ByNumber {
                    task_number: TaskNumber::new(task_number).map_err(|_| ())?.get(),
                    selector: WorkspaceSelector {
                        workspace_id,
                        workspace_name,
                    },
                })
            }
            (None, Some(raw_id)) => {
                if workspace_id.is_some() || workspace_name.is_some() {
                    return Err(());
                }
                Ok(Self::ById(TaskId::parse(raw_id).map_err(|_| ())?))
            }
        }
    }
}

/// The capabilities a face requires: on a workspace board, each token in order
/// (the first missing one is reported); on the by-id arm, the combined check.
#[derive(Clone, Copy)]
pub(crate) struct RequiredCapability {
    workspace: &'static [TaskWriteCapability],
    by_id: TaskByIdCapability,
}

impl RequiredCapability {
    pub(crate) const WRITE: Self = Self {
        workspace: &[TaskWriteCapability::Write],
        by_id: TaskByIdCapability::Write,
    };
    /// Edit returns the full definition, so it needs `read_tasks` too.
    pub(crate) const WRITE_AND_READ: Self = Self {
        workspace: &[TaskWriteCapability::Write, TaskWriteCapability::Read],
        by_id: TaskByIdCapability::WriteAndRead,
    };
}

/// Why the door refused, before any write.
pub(crate) enum DoorRefusal {
    TaskNotFound,
    AgentNotFound,
    ConversationNotFound,
    Board(BoardResolveRefusal),
    CapabilityDenied(&'static str),
    /// A store read failed — the caller answers the safe `500`.
    Internal,
}

impl DoorRefusal {
    /// The failure fields for the `{success:false, code, reason, ...}` envelope;
    /// `None` for [`DoorRefusal::Internal`].
    pub(crate) fn failure(&self) -> Option<(&str, String, Value)> {
        Some(match self {
            Self::TaskNotFound => ("task_not_found", "task not found".to_owned(), json!({})),
            Self::AgentNotFound => ("agent_not_found", "agent not found".to_owned(), json!({})),
            Self::ConversationNotFound => (
                "conversation_not_found",
                "conversation not found".to_owned(),
                json!({}),
            ),
            Self::Board(refusal) => (
                refusal.code(),
                refusal.reason().to_owned(),
                refusal.candidates().map_or_else(
                    || json!({}),
                    |candidates| {
                        json!({
                            "workspace_candidates":
                                candidates.iter().map(|w| w.as_str()).collect::<Vec<_>>(),
                        })
                    },
                ),
            ),
            Self::CapabilityDenied(capability) => (
                "capability_denied",
                format!("agent lacks the {capability} capability in the target workspace"),
                json!({ "capability": capability }),
            ),
            Self::Internal => return None,
        })
    }
}

impl IntoResponse for DoorRefusal {
    fn into_response(self) -> Response {
        match self.failure() {
            Some((code, reason, extra)) => mutation_failure(code, &reason, extra),
            None => safe_500(),
        }
    }
}

/// The task a single-ref face targets, with the source frame snapshot the
/// aftermath needs (set only on the by-number arm off a conversation frame).
pub(crate) struct Located {
    pub(crate) task_id: TaskId,
    pub(crate) source_snapshot: Option<TaskSourcePlacementSnapshot>,
}

pub(crate) async fn locate<D, N>(
    door: &D,
    numbers: &N,
    actor: &AgentId,
    source: Option<&ConversationId>,
    task: TaskRef,
    required: RequiredCapability,
) -> Result<Located, DoorRefusal>
where
    D: TaskWriteDoorReads + AgentExistenceReader + Clone,
    N: TaskNumberResolver,
{
    match task {
        TaskRef::ById(id) => {
            admit_by_id(door, actor, source, &id, required).await?;
            Ok(Located {
                task_id: id,
                source_snapshot: None,
            })
        }
        TaskRef::ByNumber {
            task_number,
            selector,
        } => {
            let board = number_board(door, actor, source, &selector, required).await?;
            match numbers
                .resolve_id_by_number(&board.board_key, task_number)
                .await
            {
                Ok(Some(task_id)) => Ok(Located {
                    task_id,
                    source_snapshot: board.source_snapshot,
                }),
                Ok(None) => Err(DoorRefusal::TaskNotFound),
                Err(_) => Err(DoorRefusal::Internal),
            }
        }
    }
}

/// The by-id gate: no by-id surface off a conversation frame (§4.5), then actor
/// existence, then the combined by-id capability.
pub(crate) async fn admit_by_id<D>(
    door: &D,
    actor: &AgentId,
    source: Option<&ConversationId>,
    id: &TaskId,
    required: RequiredCapability,
) -> Result<(), DoorRefusal>
where
    D: TaskWriteDoorReads + AgentExistenceReader + Clone,
{
    if source.is_some() {
        return Err(DoorRefusal::TaskNotFound);
    }
    ensure_agent(door, actor).await?;
    let mut door = door.clone();
    match door.may_mutate_task_by_id(id, actor, required.by_id).await {
        Ok(true) => Ok(()),
        Ok(false) => Err(DoorRefusal::TaskNotFound),
        Err(_) => Err(DoorRefusal::Internal),
    }
}

/// The board a by-number ref counts on, gated for the acting agent.
pub(crate) struct NumberBoard {
    pub(crate) board_key: String,
    pub(crate) source_snapshot: Option<TaskSourcePlacementSnapshot>,
}

pub(crate) async fn number_board<D>(
    door: &D,
    actor: &AgentId,
    source: Option<&ConversationId>,
    selector: &WorkspaceSelector,
    required: RequiredCapability,
) -> Result<NumberBoard, DoorRefusal>
where
    D: TaskWriteDoorReads + AgentExistenceReader + Clone,
{
    let mut door = door.clone();
    let (source_board, source_snapshot) = match source {
        Some(source) => match door.conversation_board(source).await {
            Ok(Some(board)) => {
                let snapshot = TaskSourcePlacementSnapshot::new(source.clone(), Some(&board));
                (Some(board), Some(snapshot))
            }
            Ok(None) => return Err(DoorRefusal::ConversationNotFound),
            Err(_) => return Err(DoorRefusal::Internal),
        },
        None => (None, None),
    };
    ensure_agent(&door, actor).await?;
    let resolved = match resolve_write_board_with_source_board(
        &mut door,
        actor,
        source,
        source_board.as_ref(),
        selector,
    )
    .await
    {
        Ok(Ok(resolved)) => resolved,
        Ok(Err(refusal)) => return Err(DoorRefusal::Board(refusal)),
        Err(_) => return Err(DoorRefusal::Internal),
    };
    let board_key = match resolved {
        ResolvedBoard::Workspace {
            board_key,
            workspace,
        } => {
            for capability in required.workspace {
                match door.has_capability(*capability, &workspace, actor).await {
                    Ok(true) => {}
                    Ok(false) => {
                        return Err(DoorRefusal::CapabilityDenied(capability_name(*capability)));
                    }
                    Err(_) => return Err(DoorRefusal::Internal),
                }
            }
            board_key
        }
        // A direct-pair board has no capability axis; participation is its gate.
        ResolvedBoard::DirectPair { board_key } => board_key,
    };
    Ok(NumberBoard {
        board_key,
        source_snapshot,
    })
}

async fn ensure_agent<D: AgentExistenceReader>(
    door: &D,
    actor: &AgentId,
) -> Result<(), DoorRefusal> {
    match door.agent_exists(actor).await {
        Ok(true) => Ok(()),
        Ok(false) => Err(DoorRefusal::AgentNotFound),
        Err(_) => Err(DoorRefusal::Internal),
    }
}

fn capability_name(capability: TaskWriteCapability) -> &'static str {
    match capability {
        TaskWriteCapability::Create => "create_tasks",
        TaskWriteCapability::Write => "write_tasks",
        TaskWriteCapability::Read => "read_tasks",
    }
}

/// `{ success: true }`.
pub(crate) fn success() -> Response {
    (StatusCode::OK, Json(json!({ "success": true }))).into_response()
}

/// The `{ success: false, code, reason, ...extra }` envelope, always `200` so the
/// CLI reads the outcome from the body.
pub(crate) fn mutation_failure(code: &str, reason: &str, extra: Value) -> Response {
    let mut body = json!({ "success": false, "code": code, "reason": reason });
    if let (Some(map), Value::Object(extra)) = (body.as_object_mut(), extra) {
        map.extend(extra);
    }
    (StatusCode::OK, Json(body)).into_response()
}

pub(crate) fn agent_not_found() -> Response {
    DoorRefusal::AgentNotFound.into_response()
}

pub(crate) fn task_not_found() -> Response {
    DoorRefusal::TaskNotFound.into_response()
}

pub(crate) fn conversation_readonly() -> Response {
    mutation_failure(
        "conversation_readonly",
        "the conversation is read-only: a direct-pair endpoint is gone",
        json!({}),
    )
}

pub(crate) fn task_done() -> Response {
    mutation_failure("task_done", "task is already done", json!({}))
}

pub(crate) fn task_closed() -> Response {
    mutation_failure("task_closed", "task is closed", json!({}))
}
