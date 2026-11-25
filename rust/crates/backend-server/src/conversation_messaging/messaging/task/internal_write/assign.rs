//! `POST /internal/agents/{aid}/tasks/assign` — the agent assign face. A SINGLE
//! ref (exactly one of `task_number` / `task_id`) plus `assignee`, returning the
//! typed `TaskMutationResult` envelope (always `200`).
//!
//! The door is unclaim's (board resolution + `write_tasks`); the core
//! [`TaskClaimWrite::assign`] UoW re-proves the actor, resolves the assignee among
//! the task conversation's participants and overrides the assignee under its lock.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use serde::Deserialize;
use serde_json::json;
use silverhand_backend_core::conversation_messaging::messaging::task::{
    AgentDisplayNameReader, AgentExistenceReader, AssignOutcome, AssignTarget, TaskAssigner,
    TaskByIdCapability, TaskClaimWrite, TaskId, TaskNumber, TaskWriteCapability,
    TaskWriteDoorReads,
};
use silverhand_backend_core::conversation_messaging::messaging::{
    ParticipantHandle, PrincipalKind,
};
use silverhand_backend_core::foundation::identity::AgentId;
use silverhand_backend_core::host_runtime::runtime_credential::RuntimePrincipal;

use super::selector::{
    BoardResolveRefusal, ResolvedBoard, WorkspaceSelector, resolve_write_board_with_source_board,
};
use super::update_status::TaskNumberResolver;
use crate::conversation_messaging::messaging::task::aftermath::{
    TaskAftermathTrigger, TaskMutationAftermath, run_aftermath,
};
use crate::foundation::http_error::plain_500 as safe_500;
use crate::foundation::wire_admission::{ValidatedJson, ValidatedJsonRejection, invalid_input};
use crate::foundation::wire_patch::RequiredPatchField;
use crate::host_runtime::runtime_credential::bind_runtime_actor;

pub struct TaskAssignState<D, N, W, A, AF> {
    pub door: D,
    pub numbers: N,
    pub write: W,
    /// Resolves the acting agent's names for the `task_claim` event pill.
    pub names: A,
    /// Mandatory post-commit aftermath, fired once on a committed assignment.
    pub aftermath: AF,
}

#[derive(Debug, Deserialize, garde::Validate)]
pub(crate) struct AssignBody {
    #[garde(skip)]
    #[serde(default)]
    assignee: RequiredPatchField<String>,
    #[garde(skip)]
    #[serde(default)]
    task_number: RequiredPatchField<i64>,
    #[garde(skip)]
    #[serde(default)]
    task_id: RequiredPatchField<String>,
    #[garde(skip)]
    #[serde(default)]
    workspace_id: RequiredPatchField<String>,
    #[garde(skip)]
    #[serde(default)]
    workspace_name: RequiredPatchField<String>,
}

enum AssignTaskRef {
    ById(TaskId),
    ByNumber {
        task_number: i64,
        selector: WorkspaceSelector,
    },
}

impl AssignTaskRef {
    fn try_from_parts(
        task_number: Option<i64>,
        task_id: Option<String>,
        workspace_id: Option<String>,
        workspace_name: Option<String>,
    ) -> Result<Self, ()> {
        let workspace_id = crate::foundation::wire_patch::empty_selector_as_none(workspace_id);
        let workspace_name = crate::foundation::wire_patch::empty_selector_as_none(workspace_name);
        match (task_number, task_id) {
            (Some(_), Some(_)) | (None, None) => Err(()),
            (Some(task_number), None) => {
                if workspace_id.is_some() && workspace_name.is_some() {
                    return Err(());
                }
                Ok(AssignTaskRef::ByNumber {
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
                Ok(AssignTaskRef::ById(TaskId::parse(raw_id).map_err(|_| ())?))
            }
        }
    }
}

type Shared<D, N, W, A, AF> = State<Arc<TaskAssignState<D, N, W, A, AF>>>;

pub fn task_assign_router<D, N, W, A, AF>(state: TaskAssignState<D, N, W, A, AF>) -> Router
where
    D: TaskWriteDoorReads + AgentExistenceReader + Clone + Send + Sync + 'static,
    N: TaskNumberResolver + Send + Sync + 'static,
    W: TaskClaimWrite + Send + Sync + 'static,
    A: AgentDisplayNameReader + Send + Sync + 'static,
    AF: TaskMutationAftermath,
{
    Router::new()
        .route(
            "/internal/agents/{aid}/tasks/assign",
            post(assign_route::<D, N, W, A, AF>),
        )
        .with_state(Arc::new(state))
}

/// Parse `assignee`: a reference (`<silverhand://agent/x>`, `silverhand://human/x`) or a
/// bare handle with or without `@`.
fn parse_assignee(raw: &str) -> Option<AssignTarget> {
    let raw = raw.trim();
    let raw = raw
        .strip_prefix('<')
        .and_then(|rest| rest.strip_suffix('>'))
        .unwrap_or(raw);
    if let Some(rest) = raw.strip_prefix("silverhand://") {
        let (kind, handle) = rest.split_once('/')?;
        let kind = match kind {
            "agent" => PrincipalKind::Agent,
            "human" => PrincipalKind::Human,
            _ => return None,
        };
        return Some(AssignTarget::Handle {
            handle: ParticipantHandle::parse(handle).ok()?,
            kind: Some(kind),
        });
    }
    let handle = raw.strip_prefix('@').unwrap_or(raw);
    Some(AssignTarget::Handle {
        handle: ParticipantHandle::parse(handle).ok()?,
        kind: None,
    })
}

async fn assign_route<D, N, W, A, AF>(
    State(state): Shared<D, N, W, A, AF>,
    Path(aid): Path<String>,
    principal: Option<axum::extract::Extension<RuntimePrincipal>>,
    body: Result<ValidatedJson<AssignBody>, ValidatedJsonRejection>,
) -> Response
where
    D: TaskWriteDoorReads + AgentExistenceReader + Clone + Send + Sync + 'static,
    N: TaskNumberResolver + Send + Sync + 'static,
    W: TaskClaimWrite + Send + Sync + 'static,
    A: AgentDisplayNameReader + Send + Sync + 'static,
    AF: TaskMutationAftermath,
{
    let principal = match bind_runtime_actor(principal.as_ref().map(|ext| &ext.0), &aid) {
        Ok(principal) => principal,
        Err(refusal) => return refusal.into_response(),
    };
    let actor: &AgentId = principal.agent();
    let source = principal.source();

    let body = match body {
        Ok(ValidatedJson(body)) => body,
        Err(rejection) => return rejection.into_response(),
    };
    let Some(assignee) = body.assignee.into_option() else {
        return invalid_input();
    };
    let Ok(target) = AssignTaskRef::try_from_parts(
        body.task_number.into_option(),
        body.task_id.into_option(),
        body.workspace_id.into_option(),
        body.workspace_name.into_option(),
    ) else {
        return invalid_input();
    };
    let Some(assignee) = parse_assignee(&assignee) else {
        return mutation_failure(
            "assignee_not_conversation_member",
            "assignee is not a member of this task's conversation",
            json!({}),
        );
    };

    let mut source_snapshot = None;
    let task_id = match target {
        AssignTaskRef::ById(id) => {
            if source.is_some() {
                return task_not_found();
            }
            match state.door.agent_exists(actor).await {
                Ok(true) => {}
                Ok(false) => {
                    return mutation_failure("agent_not_found", "agent not found", json!({}));
                }
                Err(_) => return safe_500(),
            }
            let mut door = state.door.clone();
            match door
                .may_mutate_task_by_id(&id, actor, TaskByIdCapability::Write)
                .await
            {
                Ok(true) => {}
                Ok(false) => return task_not_found(),
                Err(_) => return safe_500(),
            }
            id
        }
        AssignTaskRef::ByNumber {
            task_number,
            selector,
        } => {
            let mut door = state.door.clone();
            let source_board = if let Some(source) = source {
                match door.conversation_board(source).await {
                    Ok(Some(board)) => {
                        source_snapshot = Some(
                            crate::conversation_messaging::messaging::task::TaskSourcePlacementSnapshot::new(
                                source.clone(), Some(&board),
                            ),
                        );
                        Some(board)
                    }
                    Ok(None) => {
                        return mutation_failure(
                            "conversation_not_found",
                            "conversation not found",
                            json!({}),
                        );
                    }
                    Err(_) => return safe_500(),
                }
            } else {
                None
            };
            match door.agent_exists(actor).await {
                Ok(true) => {}
                Ok(false) => {
                    return mutation_failure("agent_not_found", "agent not found", json!({}));
                }
                Err(_) => return safe_500(),
            }
            let resolved = match resolve_write_board_with_source_board(
                &mut door,
                actor,
                source,
                source_board.as_ref(),
                &selector,
            )
            .await
            {
                Ok(Ok(board)) => board,
                Ok(Err(refusal)) => return board_refusal(refusal),
                Err(_) => return safe_500(),
            };
            let board_key = match resolved {
                ResolvedBoard::Workspace {
                    board_key,
                    workspace,
                } => {
                    match door
                        .has_capability(TaskWriteCapability::Write, &workspace, actor)
                        .await
                    {
                        Ok(true) => {}
                        Ok(false) => return capability_denied(),
                        Err(_) => return safe_500(),
                    }
                    board_key
                }
                ResolvedBoard::DirectPair { board_key } => board_key,
            };
            match state
                .numbers
                .resolve_id_by_number(&board_key, task_number)
                .await
            {
                Ok(Some(id)) => id,
                Ok(None) => return task_not_found(),
                Err(_) => return safe_500(),
            }
        }
    };

    let names = match state.names.agent_display_name(actor).await {
        Ok(Some(names)) => names,
        Ok(None) => return mutation_failure("agent_not_found", "agent not found", json!({})),
        Err(_) => return safe_500(),
    };
    let assigner = TaskAssigner::Agent {
        id: actor.clone(),
        names,
    };
    match state.write.assign(&task_id, &assigner, &assignee).await {
        Ok(AssignOutcome::Assigned { witness, notice }) => {
            let entry = assigned(&witness.task_id, witness.task_number);
            run_aftermath(
                &state.aftermath,
                &TaskAftermathTrigger::for_agent(*witness, actor.clone(), source_snapshot)
                    .with_assignee_notice(notice),
            )
            .await;
            entry
        }
        Ok(AssignOutcome::Unchanged {
            task_id,
            task_number,
        }) => assigned(&task_id, task_number),
        Ok(AssignOutcome::AgentNotFound) => {
            mutation_failure("agent_not_found", "agent not found", json!({}))
        }
        Ok(AssignOutcome::NotFound) => task_not_found(),
        Ok(AssignOutcome::ConversationReadonly) => conversation_readonly(),
        Ok(AssignOutcome::TaskDone) => {
            mutation_failure("task_done", "task is already done", json!({}))
        }
        Ok(AssignOutcome::TaskClosed) => {
            mutation_failure("task_closed", "task is closed", json!({}))
        }
        Ok(AssignOutcome::AssigneeNotConversationMember) => mutation_failure(
            "assignee_not_conversation_member",
            "assignee is not a member of this task's conversation",
            json!({}),
        ),
        Err(_) => safe_500(),
    }
}

fn assigned(id: &TaskId, number: TaskNumber) -> Response {
    (
        StatusCode::OK,
        Json(json!({ "success": true, "task_number": number.get(), "id": id.as_str() })),
    )
        .into_response()
}

fn task_not_found() -> Response {
    mutation_failure("task_not_found", "task not found", json!({}))
}
fn conversation_readonly() -> Response {
    mutation_failure(
        "conversation_readonly",
        "the conversation is read-only: a direct-pair endpoint is gone",
        json!({}),
    )
}
fn capability_denied() -> Response {
    mutation_failure(
        "capability_denied",
        "agent lacks the write_tasks capability in the target workspace",
        json!({ "capability": "write_tasks" }),
    )
}
fn board_refusal(refusal: BoardResolveRefusal) -> Response {
    let extra = refusal.candidates().map_or_else(
        || json!({}),
        |candidates| json!({ "workspace_candidates": candidates.iter().map(|w| w.as_str()).collect::<Vec<_>>() }),
    );
    mutation_failure(refusal.code(), refusal.reason(), extra)
}
fn mutation_failure(code: &str, reason: &str, extra: serde_json::Value) -> Response {
    let mut body = json!({ "success": false, "code": code, "reason": reason });
    if let (Some(map), serde_json::Value::Object(extra)) = (body.as_object_mut(), extra) {
        map.extend(extra);
    }
    (StatusCode::OK, Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assignee_accepts_references_and_bare_handles() {
        let handle = |raw: &str, kind| AssignTarget::Handle {
            handle: ParticipantHandle::parse(raw).unwrap(),
            kind,
        };
        assert_eq!(
            parse_assignee("<silverhand://human/shiki>"),
            Some(handle("shiki", Some(PrincipalKind::Human)))
        );
        assert_eq!(
            parse_assignee("silverhand://agent/Alice"),
            Some(handle("Alice", Some(PrincipalKind::Agent)))
        );
        assert_eq!(parse_assignee("@Alice"), Some(handle("Alice", None)));
        assert_eq!(parse_assignee("Alice"), Some(handle("Alice", None)));
        assert_eq!(parse_assignee("silverhand://workspace/x"), None);
        assert_eq!(parse_assignee(""), None);
    }
}
