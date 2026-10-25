//! `POST /internal/agents/{aid}/tasks/claim` — the agent self-claim face. A BATCH:
//! exactly one of `task_numbers[]` / `task_ids[]`, each entry claimed for the acting
//! agent, and the results returned in order as `{ "results": [...] }` (always `200`;
//! the CLI reads per-entry success/code from the body).
//!
//! Each entry is authorized at the shared write door and handed to the core
//! [`TaskClaimWrite::agent_claim`] UoW, which owns participation, direct-pair
//! read-only, the terminal-status refusal, and the atomic assign-if-free CAS INSIDE
//! its write lock. The adapter re-implements none of it; it settles WHICH id each
//! entry targets and WHETHER this agent may reach its board.
//!
//! The fixed order per entry is `claimTaskAgent`'s: identity (once, at the door for
//! the whole request) → admission → the by-id arm (agent-slot only; a conversation
//! frame has no by-id surface → the uniform `task_not_found`) or the by-number arm
//! (`resolve_write_board` → `write_tasks` capability → `resolve_id_by_number`) → the
//! claim UoW. A number-batch resolves its shared board ONCE; a board-resolution
//! refusal is reported for every number entry (with its number echoed for batch
//! correlation), exactly as the per-ref TS loop would.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use serde::Deserialize;
use serde_json::{Value, json};
use silverhand_backend_core::conversation_messaging::messaging::task::{
    AgentClaimOutcome, AgentDisplayNameReader, AgentEventNames, AgentExistenceReader,
    TaskByIdCapability, TaskClaimWrite, TaskId, TaskNumber, TaskWriteCapability,
    TaskWriteDoorReads,
};
use silverhand_backend_core::foundation::identity::AgentId;
use silverhand_backend_core::host_runtime::runtime_credential::RuntimePrincipal;

use super::selector::{
    BoardResolveRefusal, ResolvedBoard, WorkspaceSelector, resolve_write_board_with_source_board,
};
use super::update_status::TaskNumberResolver;
use crate::conversation_messaging::messaging::task::aftermath::{
    TaskMutationAftermath, TaskSourcePlacementSnapshot,
};
use crate::foundation::http_error::plain_500 as safe_500;
use crate::foundation::wire_admission::{ValidatedJson, ValidatedJsonRejection, invalid_input};
use crate::foundation::wire_patch::RequiredPatchField;
use crate::host_runtime::runtime_credential::bind_runtime_actor;

/// Everything the claim face composes. The three ports are the same shapes the
/// status face uses (door reads, number resolver, and the claim write UoW).
pub struct TaskClaimState<D, N, W, A, AF> {
    pub door: D,
    pub numbers: N,
    pub write: W,
    /// Resolves the acting agent's display name for the `task_claim` event pill.
    pub names: A,
    /// Mandatory post-commit aftermath, fired once per committed claim.
    pub aftermath: AF,
}

/// The wire body. Optionality + XOR live in [`ValidatedClaim`], not serde.
#[derive(Debug, Deserialize, garde::Validate)]
pub(crate) struct ClaimBody {
    #[garde(skip)]
    #[serde(default)]
    task_numbers: RequiredPatchField<Vec<i64>>,
    #[garde(skip)]
    #[serde(default)]
    task_ids: RequiredPatchField<Vec<String>>,
    #[garde(skip)]
    #[serde(default)]
    workspace_id: RequiredPatchField<String>,
    #[garde(skip)]
    #[serde(default)]
    workspace_name: RequiredPatchField<String>,
}

/// The semantically-validated claim batch — exactly one non-empty family, the
/// selectors mutually exclusive, and a task-id batch barring any selector. Every
/// refine collapses to the verdict-only `422`.
enum ClaimBatch {
    /// A global-identity batch. Each id already determines its board; no selector.
    Ids(Vec<TaskId>),
    /// A per-board number batch, resolved on the shared selector (or source frame).
    Numbers {
        numbers: Vec<i64>,
        selector: WorkspaceSelector,
    },
}

impl ClaimBatch {
    fn try_from_body(body: ClaimBody) -> Result<Self, ()> {
        let task_numbers = body.task_numbers.into_option();
        let task_ids = body.task_ids.into_option();
        let workspace_id =
            crate::foundation::wire_patch::empty_selector_as_none(body.workspace_id.into_option());
        let workspace_name = crate::foundation::wire_patch::empty_selector_as_none(
            body.workspace_name.into_option(),
        );
        match (task_numbers, task_ids) {
            (Some(_), Some(_)) | (None, None) => Err(()),
            (Some(numbers), None) => {
                if numbers.is_empty() {
                    return Err(());
                }
                let numbers = numbers
                    .into_iter()
                    .map(|number| TaskNumber::new(number).map(TaskNumber::get))
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|_| ())?;
                if workspace_id.is_some() && workspace_name.is_some() {
                    return Err(());
                }
                Ok(ClaimBatch::Numbers {
                    numbers,
                    selector: WorkspaceSelector {
                        workspace_id,
                        workspace_name,
                    },
                })
            }
            (None, Some(raw_ids)) => {
                if raw_ids.is_empty() {
                    return Err(());
                }
                if workspace_id.is_some() || workspace_name.is_some() {
                    return Err(());
                }
                let ids = raw_ids
                    .into_iter()
                    .map(TaskId::parse)
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|_| ())?;
                Ok(ClaimBatch::Ids(ids))
            }
        }
    }
}

type Shared<D, N, W, A, AF> = State<Arc<TaskClaimState<D, N, W, A, AF>>>;

pub fn task_claim_router<D, N, W, A, AF>(state: TaskClaimState<D, N, W, A, AF>) -> Router
where
    D: TaskWriteDoorReads + AgentExistenceReader + Clone + Send + Sync + 'static,
    N: TaskNumberResolver + Send + Sync + 'static,
    W: TaskClaimWrite + Send + Sync + 'static,
    A: AgentDisplayNameReader + Send + Sync + 'static,
    AF: TaskMutationAftermath,
{
    Router::new()
        .route(
            "/internal/agents/{aid}/tasks/claim",
            post(claim_route::<D, N, W, A, AF>),
        )
        .with_state(Arc::new(state))
}

async fn claim_route<D, N, W, A, AF>(
    State(state): Shared<D, N, W, A, AF>,
    Path(aid): Path<String>,
    principal: Option<axum::extract::Extension<RuntimePrincipal>>,
    body: Result<ValidatedJson<ClaimBody>, ValidatedJsonRejection>,
) -> Response
where
    D: TaskWriteDoorReads + AgentExistenceReader + Clone + Send + Sync + 'static,
    N: TaskNumberResolver + Send + Sync + 'static,
    W: TaskClaimWrite + Send + Sync + 'static,
    A: AgentDisplayNameReader + Send + Sync + 'static,
    AF: TaskMutationAftermath,
{
    // (1) IDENTITY GATE FIRST — for the whole request, before body/door/store.
    let principal = match bind_runtime_actor(principal.as_ref().map(|ext| &ext.0), &aid) {
        Ok(principal) => principal,
        Err(refusal) => return refusal.into_response(),
    };
    let actor: &AgentId = principal.agent();
    let source = principal.source();

    // (2) ADMISSION.
    let body = match body {
        Ok(ValidatedJson(body)) => body,
        Err(rejection) => return rejection.into_response(),
    };
    let Ok(batch) = ClaimBatch::try_from_body(body) else {
        return invalid_input();
    };

    let mut results: Vec<Value> = Vec::new();
    let mut actor_name: Option<AgentEventNames> = None;
    let mut source_snapshot = None;

    match batch {
        ClaimBatch::Ids(ids) => {
            for id in ids {
                // §4.5: no by-id surface off a conversation frame — the uniform
                // task_not_found, echoing the caller's id for batch correlation.
                if source.is_some() {
                    results.push(fail_id(
                        "task_not_found",
                        "task not found",
                        id.as_str(),
                        json!({}),
                    ));
                    continue;
                }
                match state.door.agent_exists(actor).await {
                    Ok(true) => {}
                    Ok(false) => {
                        results.push(fail_id(
                            "agent_not_found",
                            "agent not found",
                            id.as_str(),
                            json!({}),
                        ));
                        continue;
                    }
                    Err(_) => return safe_500(),
                }
                let mut door = state.door.clone();
                match door
                    .may_mutate_task_by_id(&id, actor, TaskByIdCapability::Write)
                    .await
                {
                    Ok(true) => {}
                    Ok(false) => {
                        results.push(fail_id(
                            "task_not_found",
                            "task not found",
                            id.as_str(),
                            json!({}),
                        ));
                        continue;
                    }
                    Err(_) => return safe_500(),
                }
                if actor_name.is_none() {
                    actor_name = match state.names.agent_display_name(actor).await {
                        Ok(Some(name)) => Some(name),
                        Ok(None) => {
                            results.push(fail_id(
                                "agent_not_found",
                                "agent not found",
                                id.as_str(),
                                json!({}),
                            ));
                            continue;
                        }
                        Err(_) => return safe_500(),
                    };
                }
                match claim_one(&state, &id, actor, None, actor_name.as_ref().unwrap()).await {
                    Ok(entry) => results.push(entry),
                    Err(()) => return safe_500(),
                }
            }
        }
        ClaimBatch::Numbers { numbers, selector } => {
            let mut door = state.door.clone();
            let source_board = if let Some(source) = source {
                match door.conversation_board(source).await {
                    Ok(Some(board)) => {
                        source_snapshot = Some(TaskSourcePlacementSnapshot::new(
                            source.clone(),
                            Some(&board),
                        ));
                        Some(board)
                    }
                    Ok(None) => {
                        let refused = number_refusal_for_all(
                            &numbers,
                            "conversation_not_found",
                            "conversation not found",
                            json!({}),
                        );
                        return (StatusCode::OK, Json(json!({ "results": refused })))
                            .into_response();
                    }
                    Err(_) => return safe_500(),
                }
            } else {
                None
            };
            match door.agent_exists(actor).await {
                Ok(true) => {}
                Ok(false) => {
                    let refused = number_refusal_for_all(
                        &numbers,
                        "agent_not_found",
                        "agent not found",
                        json!({}),
                    );
                    return (StatusCode::OK, Json(json!({ "results": refused }))).into_response();
                }
                Err(_) => return safe_500(),
            }
            // Resolve the shared board ONCE (all numbers count on the same board);
            // a resolution refusal is reported for every number entry.
            let board_key = match resolve_write_board_with_source_board(
                &mut door,
                actor,
                source,
                source_board.as_ref(),
                &selector,
            )
            .await
            {
                Ok(Ok(resolved)) => {
                    match resolved {
                        ResolvedBoard::Workspace {
                            board_key,
                            workspace,
                        } => {
                            // write_tasks on the workspace board (a pair board has no
                            // capability axis).
                            match door
                                .has_capability(TaskWriteCapability::Write, &workspace, actor)
                                .await
                            {
                                Ok(true) => Ok(board_key),
                                Ok(false) => Err(number_refusal_for_all(
                                    &numbers,
                                    "capability_denied",
                                    "agent lacks the write_tasks capability in the target workspace",
                                    json!({ "capability": "write_tasks" }),
                                )),
                                Err(_) => return safe_500(),
                            }
                        }
                        ResolvedBoard::DirectPair { board_key } => Ok(board_key),
                    }
                }
                Ok(Err(refusal)) => Err(number_batch_board_refusal(&numbers, refusal)),
                Err(_) => return safe_500(),
            };
            let board_key = match board_key {
                Ok(key) => key,
                Err(refused_entries) => {
                    return (StatusCode::OK, Json(json!({ "results": refused_entries })))
                        .into_response();
                }
            };
            let actor_name = match state.names.agent_display_name(actor).await {
                Ok(Some(name)) => name,
                Ok(None) => {
                    let refused = number_refusal_for_all(
                        &numbers,
                        "agent_not_found",
                        "agent not found",
                        json!({}),
                    );
                    return (StatusCode::OK, Json(json!({ "results": refused }))).into_response();
                }
                Err(_) => return safe_500(),
            };
            for number in numbers {
                match state.numbers.resolve_id_by_number(&board_key, number).await {
                    Ok(Some(id)) => match state.write.agent_claim(&id, actor, &actor_name).await {
                        Ok(outcome) => {
                            results.push(
                                claim_outcome_number(
                                    outcome,
                                    number,
                                    &state.aftermath,
                                    actor,
                                    source_snapshot.as_ref(),
                                )
                                .await,
                            );
                        }
                        Err(_) => return safe_500(),
                    },
                    Ok(None) => {
                        results.push(fail_number(
                            "task_not_found",
                            "task not found",
                            number,
                            json!({}),
                        ));
                    }
                    Err(_) => return safe_500(),
                }
            }
        }
    }

    (StatusCode::OK, Json(json!({ "results": results }))).into_response()
}

/// Claim one by-id task on an agent slot: straight to the write UoW (its in-lock
/// gates are the authorization), then map the outcome, echoing the id.
async fn claim_one<D, N, W, A, AF>(
    state: &Arc<TaskClaimState<D, N, W, A, AF>>,
    id: &TaskId,
    actor: &AgentId,
    source: Option<&TaskSourcePlacementSnapshot>,
    actor_name: &AgentEventNames,
) -> Result<Value, ()>
where
    W: TaskClaimWrite + Send + Sync + 'static,
    AF: TaskMutationAftermath,
{
    match state.write.agent_claim(id, actor, actor_name).await {
        Ok(outcome) => Ok(claim_outcome_id(outcome, id, &state.aftermath, actor, source).await),
        Err(_) => Err(()),
    }
}

/// A claim outcome for a BY-ID entry → the result entry, echoing `id`. Fires the
/// aftermath once on a committed claim.
async fn claim_outcome_id<AF: TaskMutationAftermath>(
    outcome: AgentClaimOutcome,
    id: &TaskId,
    aftermath: &AF,
    actor: &AgentId,
    source: Option<&TaskSourcePlacementSnapshot>,
) -> Value {
    match outcome {
        AgentClaimOutcome::Claimed { status: _, witness } => {
            let entry = claimed(&witness);
            crate::conversation_messaging::messaging::task::aftermath::run_agent_aftermath(
                aftermath,
                *witness,
                actor,
                source.cloned(),
            )
            .await;
            entry
        }
        AgentClaimOutcome::AgentNotFound => {
            fail_id("agent_not_found", "agent not found", id.as_str(), json!({}))
        }
        AgentClaimOutcome::NotFound => {
            fail_id("task_not_found", "task not found", id.as_str(), json!({}))
        }
        AgentClaimOutcome::ConversationReadonly => fail_id(
            "conversation_readonly",
            "the conversation is read-only: a direct-pair endpoint is gone",
            id.as_str(),
            json!({}),
        ),
        AgentClaimOutcome::TaskDone => {
            fail_id("task_done", "task is already done", id.as_str(), json!({}))
        }
        AgentClaimOutcome::TaskClosed => {
            fail_id("task_closed", "task is closed", id.as_str(), json!({}))
        }
        AgentClaimOutcome::AlreadyClaimed => fail_id(
            "already_claimed",
            "task is already assigned",
            id.as_str(),
            json!({}),
        ),
    }
}

/// A claim outcome for a BY-NUMBER entry → the result entry, echoing `task_number`.
/// Fires the aftermath once on a committed claim.
async fn claim_outcome_number<AF: TaskMutationAftermath>(
    outcome: AgentClaimOutcome,
    number: i64,
    aftermath: &AF,
    actor: &AgentId,
    source: Option<&TaskSourcePlacementSnapshot>,
) -> Value {
    match outcome {
        AgentClaimOutcome::Claimed { status: _, witness } => {
            let entry = claimed(&witness);
            crate::conversation_messaging::messaging::task::aftermath::run_agent_aftermath(
                aftermath,
                *witness,
                actor,
                source.cloned(),
            )
            .await;
            entry
        }
        AgentClaimOutcome::AgentNotFound => {
            fail_number("agent_not_found", "agent not found", number, json!({}))
        }
        AgentClaimOutcome::NotFound => {
            fail_number("task_not_found", "task not found", number, json!({}))
        }
        AgentClaimOutcome::ConversationReadonly => fail_number(
            "conversation_readonly",
            "the conversation is read-only: a direct-pair endpoint is gone",
            number,
            json!({}),
        ),
        AgentClaimOutcome::TaskDone => {
            fail_number("task_done", "task is already done", number, json!({}))
        }
        AgentClaimOutcome::TaskClosed => {
            fail_number("task_closed", "task is closed", number, json!({}))
        }
        AgentClaimOutcome::AlreadyClaimed => fail_number(
            "already_claimed",
            "task is already assigned",
            number,
            json!({}),
        ),
    }
}

/// A board-resolution refusal → one refusal entry per number in the batch, mirroring
/// the per-ref TS loop where each `claimTaskAgent` re-resolves and fails identically.
fn number_batch_board_refusal(numbers: &[i64], refusal: BoardResolveRefusal) -> Vec<Value> {
    let extra = refusal.candidates().map_or_else(
        || json!({}),
        |candidates| json!({ "workspace_candidates": candidates.iter().map(|w| w.as_str()).collect::<Vec<_>>() }),
    );
    number_refusal_for_all(numbers, refusal.code(), refusal.reason(), extra)
}

fn number_refusal_for_all(numbers: &[i64], code: &str, reason: &str, extra: Value) -> Vec<Value> {
    numbers
        .iter()
        .map(|number| fail_number(code, reason, *number, extra.clone()))
        .collect()
}

fn claimed(
    witness: &silverhand_backend_core::conversation_messaging::messaging::task::TaskMutationWitness,
) -> Value {
    json!({
        "success": true,
        "task_number": witness.task_number.get(),
        "id": witness.task_id.as_str(),
    })
}

fn fail_id(code: &str, reason: &str, id: &str, extra: Value) -> Value {
    fail_entry(code, reason, json!({ "id": id }), extra)
}

fn fail_number(code: &str, reason: &str, number: i64, extra: Value) -> Value {
    fail_entry(code, reason, json!({ "task_number": number }), extra)
}

/// The `{ success: false, code, reason, <correlation>, ...extra }` entry.
fn fail_entry(code: &str, reason: &str, correlation: Value, extra: Value) -> Value {
    let mut entry = json!({ "success": false, "code": code, "reason": reason });
    if let Some(map) = entry.as_object_mut() {
        if let Value::Object(correlation) = correlation {
            map.extend(correlation);
        }
        if let Value::Object(extra) = extra {
            map.extend(extra);
        }
    }
    entry
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::extract::Extension;
    use axum::http::Request;
    use silverhand_backend_core::conversation_messaging::messaging::task::{
        BoardKey, PairKey, TaskStatus, WorkspaceMembership,
    };
    use silverhand_backend_core::foundation::identity::{
        ConversationId, DaemonInstanceId, WorkspaceId,
    };
    use silverhand_backend_core::foundation::persistence::PersistenceFailure;
    use silverhand_backend_core::host_runtime::runtime_credential::{
        RuntimeCustody, RuntimeGeneration, RuntimeScope,
    };
    use silverhand_backend_core::identity_access::workspace_name::CanonicalWorkspaceName;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const AGENT: &str = "agent-1";

    fn custody() -> RuntimeCustody {
        RuntimeCustody::new(
            RuntimeGeneration::new(1).unwrap(),
            DaemonInstanceId::parse("daemon-1").unwrap(),
        )
    }
    fn agent_slot() -> RuntimePrincipal {
        RuntimePrincipal::for_scope(
            RuntimeScope::AgentSlot {
                agent: AgentId::parse(AGENT).unwrap(),
            },
            custody(),
        )
    }
    fn conversation_slot() -> RuntimePrincipal {
        RuntimePrincipal::for_scope(
            RuntimeScope::ConversationSlot {
                agent: AgentId::parse(AGENT).unwrap(),
                source: ConversationId::parse("conv-1").unwrap(),
            },
            custody(),
        )
    }

    #[derive(Clone)]
    struct RecordingDoor {
        agent_exists: bool,
        named: usize,
        board: Option<BoardKey>,
        membership: WorkspaceMembership,
        capability: bool,
        named_calls: Arc<AtomicUsize>,
        board_calls: Arc<AtomicUsize>,
        membership_calls: Arc<AtomicUsize>,
        capability_calls: Arc<AtomicUsize>,
    }
    impl AgentExistenceReader for RecordingDoor {
        async fn agent_exists(&self, _agent: &AgentId) -> Result<bool, PersistenceFailure> {
            Ok(self.agent_exists)
        }
    }

    impl TaskWriteDoorReads for RecordingDoor {
        async fn may_mutate_task_by_id(
            &mut self,
            _task: &TaskId,
            _agent: &AgentId,
            _required: TaskByIdCapability,
        ) -> Result<bool, PersistenceFailure> {
            Ok(true)
        }

        async fn workspaces_named(
            &mut self,
            _c: &CanonicalWorkspaceName,
        ) -> Result<Vec<WorkspaceId>, PersistenceFailure> {
            self.named_calls.fetch_add(1, Ordering::SeqCst);
            Ok((0..self.named)
                .map(|i| WorkspaceId::parse(format!("ws-{i}")).unwrap())
                .collect())
        }
        async fn conversation_board(
            &mut self,
            _c: &ConversationId,
        ) -> Result<Option<BoardKey>, PersistenceFailure> {
            self.board_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.board.clone())
        }
        async fn workspace_membership(
            &mut self,
            _w: &WorkspaceId,
            _a: &AgentId,
        ) -> Result<WorkspaceMembership, PersistenceFailure> {
            self.membership_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.membership)
        }
        async fn has_capability(
            &mut self,
            _cap: TaskWriteCapability,
            _w: &WorkspaceId,
            _a: &AgentId,
        ) -> Result<bool, PersistenceFailure> {
            self.capability_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.capability)
        }
    }

    struct RecordingNumbers {
        found: bool,
        calls: Arc<AtomicUsize>,
    }
    impl TaskNumberResolver for RecordingNumbers {
        async fn resolve_id_by_number(
            &self,
            _b: &str,
            _n: i64,
        ) -> Result<Option<TaskId>, PersistenceFailure> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.found.then(|| TaskId::parse("task-1").unwrap()))
        }
    }

    struct RecordingWrite {
        outcome: AgentClaimOutcome,
        calls: Arc<AtomicUsize>,
    }
    impl TaskClaimWrite for RecordingWrite {
        async fn agent_claim(
            &self,
            _id: &TaskId,
            _actor: &AgentId,
            _actor_name: &AgentEventNames,
        ) -> Result<AgentClaimOutcome, PersistenceFailure> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.outcome.clone())
        }
        async fn human_claim(
            &self,
            _id: &TaskId,
            _actor: &silverhand_backend_core::foundation::identity::UserId,
            _assignee: &AgentId,
        ) -> Result<
            silverhand_backend_core::conversation_messaging::messaging::task::HumanClaimOutcome,
            PersistenceFailure,
        > {
            unreachable!("agent face never calls human_claim")
        }
        async fn agent_unclaim(
            &self,
            _id: &TaskId,
            _actor: &AgentId,
        ) -> Result<
            silverhand_backend_core::conversation_messaging::messaging::task::UnclaimOutcome,
            PersistenceFailure,
        > {
            unreachable!("claim face never unclaims")
        }
        async fn human_unclaim(
            &self,
            _id: &TaskId,
            _actor: &silverhand_backend_core::foundation::identity::UserId,
        ) -> Result<
            silverhand_backend_core::conversation_messaging::messaging::task::UnclaimOutcome,
            PersistenceFailure,
        > {
            unreachable!("agent face never calls human_unclaim")
        }
    }

    struct RecordingNames {
        calls: Arc<AtomicUsize>,
    }
    impl silverhand_backend_core::conversation_messaging::messaging::task::AgentDisplayNameReader
        for RecordingNames
    {
        async fn agent_display_name(
            &self,
            _agent: &AgentId,
        ) -> Result<Option<AgentEventNames>, PersistenceFailure> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Some(AgentEventNames::new("alice", "Alice")))
        }
    }
    struct RecordingAftermath {
        fired: Arc<AtomicUsize>,
    }
    impl crate::conversation_messaging::messaging::task::aftermath::TaskMutationAftermath
        for RecordingAftermath
    {
        async fn fire(
            &self,
            _trigger: &crate::conversation_messaging::messaging::task::aftermath::TaskAftermathTrigger,
        ) -> crate::conversation_messaging::messaging::task::aftermath::AftermathOutcome {
            self.fired.fetch_add(1, Ordering::SeqCst);
            crate::conversation_messaging::messaging::task::aftermath::AftermathOutcome::Delivered
        }
    }
    /// A `Claimed` outcome with a stub witness for the tests.
    fn claimed(status: TaskStatus) -> AgentClaimOutcome {
        use silverhand_backend_core::conversation_messaging::messaging::task::{
            CreatedEventMessageId, TaskMutationWitness, TaskNumber,
        };
        use silverhand_backend_core::foundation::identity::ConversationId;
        AgentClaimOutcome::Claimed {
            status,
            witness: Box::new(TaskMutationWitness {
                kind: silverhand_backend_core::conversation_messaging::messaging::task::TaskEventKind::Claim,
                task_id: TaskId::parse("task-1").unwrap(),
                task_number: TaskNumber::new(7).unwrap(),
                event_message: silverhand_backend_core::conversation_messaging::messaging::task::TaskCommittedEventMessage::new(
                    CreatedEventMessageId::parse("msg-1").unwrap(),
                    ConversationId::parse("conv-1").unwrap(), None,
                    silverhand_backend_core::conversation_messaging::messaging::task::TaskEventMessage { content: "event".into(), metadata: "{}".into() }, 1, 1,
                ),
                conversation_id: ConversationId::parse("conv-1").unwrap(),
                board: silverhand_backend_core::conversation_messaging::messaging::task::BoardKey::Workspace(silverhand_backend_core::foundation::identity::WorkspaceId::parse("ws-1").unwrap()),
                thread_root_id: None,
                title: silverhand_backend_core::conversation_messaging::messaging::task::TaskTitle::parse("title").unwrap(),
                status,
                assignee: None,
                closed_at: None,
                source_message_id: None,
            }),
        }
    }

    struct Counts {
        named: Arc<AtomicUsize>,
        board: Arc<AtomicUsize>,
        membership: Arc<AtomicUsize>,
        capability: Arc<AtomicUsize>,
        numbers: Arc<AtomicUsize>,
        write: Arc<AtomicUsize>,
        names: Arc<AtomicUsize>,
        aftermath: Arc<AtomicUsize>,
    }

    struct DoorSpec {
        agent_exists: bool,
        named: usize,
        board: Option<BoardKey>,
        membership: WorkspaceMembership,
        capability: bool,
    }
    impl Default for DoorSpec {
        fn default() -> Self {
            Self {
                agent_exists: true,
                named: 1,
                board: None,
                membership: WorkspaceMembership::Member,
                capability: true,
            }
        }
    }

    fn app(
        principal: RuntimePrincipal,
        door: DoorSpec,
        number_found: bool,
        outcome: AgentClaimOutcome,
    ) -> (Router, Counts) {
        let counts = Counts {
            named: Arc::new(AtomicUsize::new(0)),
            board: Arc::new(AtomicUsize::new(0)),
            membership: Arc::new(AtomicUsize::new(0)),
            capability: Arc::new(AtomicUsize::new(0)),
            numbers: Arc::new(AtomicUsize::new(0)),
            write: Arc::new(AtomicUsize::new(0)),
            names: Arc::new(AtomicUsize::new(0)),
            aftermath: Arc::new(AtomicUsize::new(0)),
        };
        let state = TaskClaimState {
            door: RecordingDoor {
                agent_exists: door.agent_exists,
                named: door.named,
                board: door.board,
                membership: door.membership,
                capability: door.capability,
                named_calls: Arc::clone(&counts.named),
                board_calls: Arc::clone(&counts.board),
                membership_calls: Arc::clone(&counts.membership),
                capability_calls: Arc::clone(&counts.capability),
            },
            numbers: RecordingNumbers {
                found: number_found,
                calls: Arc::clone(&counts.numbers),
            },
            write: RecordingWrite {
                outcome,
                calls: Arc::clone(&counts.write),
            },
            names: RecordingNames {
                calls: Arc::clone(&counts.names),
            },
            aftermath: RecordingAftermath {
                fired: Arc::clone(&counts.aftermath),
            },
        };
        let router = task_claim_router(state).layer(Extension(principal));
        (router, counts)
    }

    async fn call_at(app: Router, path_aid: &str, body: String) -> (StatusCode, Value) {
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/internal/agents/{path_aid}/tasks/claim"))
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: Value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, value)
    }
    async fn call(app: Router, body: String) -> (StatusCode, Value) {
        call_at(app, AGENT, body).await
    }

    use tower::ServiceExt;

    #[tokio::test]
    async fn a_credential_for_another_agent_is_401_full_zero_reach() {
        let (app, counts) = app(
            agent_slot(),
            DoorSpec::default(),
            true,
            claimed(TaskStatus::InProgress),
        );
        let (status, body) = call_at(app, "agent-2", r#"{"task_numbers":[7]}"#.to_owned()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"], "unauthorized");
        assert_eq!(counts.board.load(Ordering::SeqCst), 0);
        assert_eq!(counts.write.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn ghost_actor_is_typed_after_the_conversation_frame_collapse_and_before_target_reads() {
        let ghost = DoorSpec {
            agent_exists: false,
            ..DoorSpec::default()
        };
        let (router, counts) = app(agent_slot(), ghost, true, claimed(TaskStatus::InProgress));
        let (status, body) = call(router, r#"{"task_ids":["task-1"]}"#.to_owned()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["results"][0]["code"], "agent_not_found");
        assert_eq!(counts.write.load(Ordering::SeqCst), 0);
        assert_eq!(counts.names.load(Ordering::SeqCst), 0);

        let (app, counts) = app(
            conversation_slot(),
            DoorSpec {
                agent_exists: false,
                ..DoorSpec::default()
            },
            true,
            claimed(TaskStatus::InProgress),
        );
        let (status, body) = call(app, r#"{"task_ids":["task-1"]}"#.to_owned()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["results"][0]["code"], "task_not_found");
        assert_eq!(counts.write.load(Ordering::SeqCst), 0);
        assert_eq!(counts.names.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn mixed_families_is_a_semantic_422() {
        let (app, _c) = app(
            agent_slot(),
            DoorSpec::default(),
            true,
            claimed(TaskStatus::InProgress),
        );
        let (status, body) = call(
            app,
            r#"{"task_numbers":[7],"task_ids":["task-1"]}"#.to_owned(),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["error"], "invalid input");
    }

    #[tokio::test]
    async fn shared_numeric_corpus_is_admitted_before_downstream_ports() {
        let fixture: Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../packages/shared/tests/fixtures/task-schedule-wire-canonicalization.json"
        )))
        .unwrap();
        for case in fixture["positive_js_safe"].as_array().unwrap() {
            let value = case["value"].clone();
            let accepted = case["accepted"].as_bool().unwrap();
            let (app, counts) = app(
                conversation_slot(),
                DoorSpec {
                    board: Some(BoardKey::Pair(PairKey::parse("a__b").unwrap())),
                    ..DoorSpec::default()
                },
                true,
                claimed(TaskStatus::InProgress),
            );
            let (status, body) = call(app, json!({"task_numbers":[value]}).to_string()).await;
            if accepted {
                assert_eq!(status, StatusCode::OK, "{case}");
                assert_eq!(body["results"][0]["success"], true, "{case}");
                assert_eq!(counts.numbers.load(Ordering::SeqCst), 1, "{case}");
                assert_eq!(counts.write.load(Ordering::SeqCst), 1, "{case}");
                assert_eq!(counts.aftermath.load(Ordering::SeqCst), 1, "{case}");
            } else {
                assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{case}");
                assert_eq!(counts.board.load(Ordering::SeqCst), 0, "{case}");
                assert_eq!(counts.membership.load(Ordering::SeqCst), 0, "{case}");
                assert_eq!(counts.capability.load(Ordering::SeqCst), 0, "{case}");
                assert_eq!(counts.names.load(Ordering::SeqCst), 0, "{case}");
                assert_eq!(counts.numbers.load(Ordering::SeqCst), 0, "{case}");
                assert_eq!(counts.write.load(Ordering::SeqCst), 0, "{case}");
                assert_eq!(counts.aftermath.load(Ordering::SeqCst), 0, "{case}");
            }
        }
    }

    #[tokio::test]
    async fn optional_nonnullable_fields_reject_null_and_empty_selectors_are_absent() {
        for body in [
            r#"{"task_numbers":null,"task_ids":["task-1"]}"#,
            r#"{"task_numbers":[7],"task_ids":null}"#,
            r#"{"task_ids":["task-1"],"workspace_id":null}"#,
            r#"{"task_ids":["task-1"],"workspace_name":null}"#,
        ] {
            let (app, counts) = app(
                agent_slot(),
                DoorSpec::default(),
                true,
                claimed(TaskStatus::InProgress),
            );
            let (status, _value) = call(app, body.to_owned()).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
            assert_eq!(counts.board.load(Ordering::SeqCst), 0, "{body}");
            assert_eq!(counts.numbers.load(Ordering::SeqCst), 0, "{body}");
            assert_eq!(counts.write.load(Ordering::SeqCst), 0, "{body}");
        }

        for body in [
            r#"{"task_ids":["task-1"],"workspace_id":""}"#,
            r#"{"task_ids":["task-1"],"workspace_name":""}"#,
            r#"{"task_ids":["task-1"],"workspace_id":"","workspace_name":""}"#,
        ] {
            let (app, counts) = app(
                agent_slot(),
                DoorSpec::default(),
                true,
                claimed(TaskStatus::InProgress),
            );
            let (status, value) = call(app, body.to_owned()).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(value["results"][0]["success"], true);
            assert_eq!(counts.write.load(Ordering::SeqCst), 1, "{body}");
        }

        let (app, counts) = app(
            conversation_slot(),
            DoorSpec {
                board: Some(BoardKey::Pair(PairKey::parse("a__b").unwrap())),
                ..DoorSpec::default()
            },
            true,
            claimed(TaskStatus::InProgress),
        );
        let (status, value) = call(
            app,
            r#"{"task_numbers":[7],"workspace_id":"","workspace_name":""}"#.to_owned(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["results"][0]["success"], true);
        assert_eq!(counts.board.load(Ordering::SeqCst), 1);
        assert_eq!(counts.write.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn an_empty_batch_is_a_semantic_422() {
        let (app, _c) = app(
            agent_slot(),
            DoorSpec::default(),
            true,
            claimed(TaskStatus::InProgress),
        );
        let (status, _b) = call(app, r#"{"task_numbers":[]}"#.to_owned()).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn by_id_off_a_conversation_frame_is_task_not_found_per_entry_zero_reach() {
        let (app, counts) = app(
            conversation_slot(),
            DoorSpec::default(),
            true,
            claimed(TaskStatus::InProgress),
        );
        let (status, body) = call(app, r#"{"task_ids":["task-1","task-2"]}"#.to_owned()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["results"][0]["code"], "task_not_found");
        assert_eq!(body["results"][0]["id"], "task-1");
        assert_eq!(body["results"][1]["id"], "task-2");
        assert_eq!(counts.write.load(Ordering::SeqCst), 0, "write zero-reach");
        assert_eq!(counts.names.load(Ordering::SeqCst), 0, "name zero-reach");
    }

    #[tokio::test]
    async fn a_by_id_agent_slot_claims_one_id() {
        let (app, counts) = app(
            agent_slot(),
            DoorSpec::default(),
            true,
            claimed(TaskStatus::InProgress),
        );
        let (status, body) = call(app, r#"{"task_ids":["task-1"]}"#.to_owned()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["results"][0]["success"], true);
        assert_eq!(body["results"][0]["id"], "task-1");
        assert_eq!(body["results"][0]["task_number"], 7);
        assert!(body["results"][0].get("status").is_none());
        assert_eq!(counts.write.load(Ordering::SeqCst), 1);
        assert_eq!(counts.board.load(Ordering::SeqCst), 0);
        assert_eq!(counts.names.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn ambiguous_name_refuses_every_number_entry_before_membership() {
        let (app, counts) = app(
            agent_slot(),
            DoorSpec {
                named: 2,
                ..DoorSpec::default()
            },
            true,
            claimed(TaskStatus::InProgress),
        );
        let (status, body) = call(
            app,
            r#"{"task_numbers":[7,8],"workspace_name":"dup"}"#.to_owned(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["results"][0]["code"], "ambiguous_workspace");
        assert_eq!(body["results"][0]["task_number"], 7);
        assert_eq!(body["results"][1]["task_number"], 8);
        assert_eq!(
            body["results"][0]["workspace_candidates"],
            json!(["ws-0", "ws-1"])
        );
        assert_eq!(counts.membership.load(Ordering::SeqCst), 0);
        assert_eq!(counts.write.load(Ordering::SeqCst), 0);
        assert_eq!(counts.names.load(Ordering::SeqCst), 0, "name zero-reach");
    }

    #[tokio::test]
    async fn capability_denied_refuses_every_number_entry_zero_reach() {
        let (app, counts) = app(
            agent_slot(),
            DoorSpec {
                named: 1,
                capability: false,
                ..DoorSpec::default()
            },
            true,
            claimed(TaskStatus::InProgress),
        );
        let (status, body) = call(
            app,
            r#"{"task_numbers":[7,8],"workspace_name":"only"}"#.to_owned(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["results"][0]["code"], "capability_denied");
        assert_eq!(body["results"][0]["capability"], "write_tasks");
        assert_eq!(
            counts.capability.load(Ordering::SeqCst),
            1,
            "capability judged once for the batch"
        );
        assert_eq!(counts.numbers.load(Ordering::SeqCst), 0);
        assert_eq!(counts.write.load(Ordering::SeqCst), 0);
        assert_eq!(counts.names.load(Ordering::SeqCst), 0, "name zero-reach");
    }

    #[tokio::test]
    async fn a_number_batch_on_a_direct_pair_claims_each_without_membership_or_capability() {
        let (app, counts) = app(
            conversation_slot(),
            DoorSpec {
                board: Some(BoardKey::Pair(PairKey::parse("a__b").unwrap())),
                ..DoorSpec::default()
            },
            true,
            claimed(TaskStatus::InProgress),
        );
        let (status, body) = call(app, r#"{"task_numbers":[7,8]}"#.to_owned()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["results"][0]["success"], true);
        assert_eq!(body["results"][1]["success"], true);
        assert_eq!(
            counts.board.load(Ordering::SeqCst),
            1,
            "board resolved once for the batch"
        );
        assert_eq!(counts.membership.load(Ordering::SeqCst), 0);
        assert_eq!(counts.capability.load(Ordering::SeqCst), 0);
        assert_eq!(counts.numbers.load(Ordering::SeqCst), 2);
        assert_eq!(counts.write.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn already_claimed_maps_per_entry() {
        let (app, _c) = app(
            agent_slot(),
            DoorSpec::default(),
            true,
            AgentClaimOutcome::AlreadyClaimed,
        );
        let (status, body) = call(app, r#"{"task_ids":["task-1"]}"#.to_owned()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["results"][0]["code"], "already_claimed");
        assert_eq!(body["results"][0]["id"], "task-1");
    }

    #[tokio::test]
    async fn task_closed_maps_per_entry() {
        let (app, _c) = app(
            agent_slot(),
            DoorSpec::default(),
            true,
            AgentClaimOutcome::TaskClosed,
        );
        let (status, body) = call(app, r#"{"task_ids":["task-1"]}"#.to_owned()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["results"][0]["code"], "task_closed");
    }
}
