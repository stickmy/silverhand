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
    TaskClaimWrite, TaskId, TaskNumber, TaskWriteDoorReads,
};
use silverhand_backend_core::conversation_messaging::messaging::{
    ParticipantHandle, PrincipalKind,
};
use silverhand_backend_core::foundation::identity::AgentId;
use silverhand_backend_core::host_runtime::runtime_credential::RuntimePrincipal;

use super::door::{
    Located, RequiredCapability, TaskRef, agent_not_found, conversation_readonly, locate,
    mutation_failure, task_closed, task_done, task_not_found,
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
    let Ok(target) = TaskRef::from_wire(
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

    let Located {
        task_id,
        source_snapshot,
    } = match locate(
        &state.door,
        &state.numbers,
        actor,
        source,
        target,
        RequiredCapability::WRITE,
    )
    .await
    {
        Ok(located) => located,
        Err(refusal) => return refusal.into_response(),
    };

    let names = match state.names.agent_display_name(actor).await {
        Ok(Some(names)) => names,
        Ok(None) => return agent_not_found(),
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
        Ok(AssignOutcome::AgentNotFound) => agent_not_found(),
        Ok(AssignOutcome::NotFound) => task_not_found(),
        Ok(AssignOutcome::ConversationReadonly) => conversation_readonly(),
        Ok(AssignOutcome::TaskDone) => task_done(),
        Ok(AssignOutcome::TaskClosed) => task_closed(),
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
