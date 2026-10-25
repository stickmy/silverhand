//! `POST /internal/agents/{aid}/tasks/update-status` — the agent status-change
//! face. A thin adapter over the shared write door: it authorizes at the door and
//! hands the change to the core [`TaskStatusWrite`] unit of work, which re-decides
//! participation, direct-pair read-only, the assignee/creator permission, and the
//! transition table INSIDE its own write lock. The adapter re-implements none of
//! that; it only settles WHICH task-id the write targets and WHETHER this agent may
//! reach that board.
//!
//! The fixed order (parity with `updateStatusAgent`):
//!
//! 1. IDENTITY: [`bind_runtime_actor`](crate::host_runtime::runtime_credential)
//!    binds the acting agent to the path `{aid}` — before body, selector,
//!    capability, or store. A credential for A on B's path is `401`; a missing
//!    credential is a composition `500`. The WHOLE principal is kept so the scope
//!    (agent-slot vs conversation-frame) can drive the by-id gate below.
//! 2. ADMISSION: structural faults keep axum's native rejection; a semantic refusal
//!    (bad XOR, unknown status token) is the minimal `422 {"error":"invalid
//!    input"}`.
//! 3. BY-ID ARM: a by-id write is refused for a conversation-frame principal with the
//!    uniform `task_not_found` (§4.5 — no by-id mutation surface off an agent slot),
//!    then handed straight to the write UoW by its canonical id (the UoW's in-lock
//!    participation + read-only + permission gates are the whole authorization).
//! 4. BY-NUMBER ARM: resolve the board via [`resolve_write_board`] (the
//!    `resolveAgentTaskBoard` parity), gate the `write_tasks` capability on a
//!    workspace board (a direct-pair board has no capability axis), resolve the
//!    per-board number to its canonical id, then hand that id to the write UoW.
//! 5. OUTCOME → the typed `TaskMutationResult` envelope, always `200` so the CLI
//!    reads success/code from the body.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use serde::Deserialize;
use silverhand_backend_core::conversation_messaging::messaging::task::{
    AgentDisplayNameReader, AgentExistenceReader, AgentStatusRefusal, AgentStatusWriteOutcome,
    TaskByIdCapability, TaskId, TaskNumber, TaskStatus, TaskStatusWrite, TaskWriteCapability,
    TaskWriteDoorReads,
};
use silverhand_backend_core::foundation::identity::AgentId;
use silverhand_backend_core::host_runtime::runtime_credential::RuntimePrincipal;

use super::selector::{
    BoardResolveRefusal, ResolvedBoard, WorkspaceSelector, resolve_write_board_with_source_board,
};
use crate::conversation_messaging::messaging::task::aftermath::TaskMutationAftermath;
use crate::foundation::http_error::plain_500 as safe_500;
use crate::foundation::wire_admission::{ValidatedJson, ValidatedJsonRejection, invalid_input};
use crate::foundation::wire_patch::RequiredPatchField;
use crate::host_runtime::runtime_credential::bind_runtime_actor;

/// The board number lookup the by-number arm needs after the door resolves its
/// board: the per-board `(board_key, task_number)` → canonical `TaskId`. Kept as
/// its own narrow port so the adapter depends on the exact read it makes, not the
/// whole SQLite store surface.
pub use silverhand_backend_core::conversation_messaging::messaging::task::TaskNumberResolver;

/// Everything the status face composes, supplied by whoever mounts it. The three
/// are separate ports because they are three genuinely different reads/writes — the
/// door reads settle the board + membership + capability, the number resolver reads
/// one id, the write UoW commits the change — and a composition may back them with
/// different handles. `D` is cloned once per request for the door's `&mut self`
/// ports (pool-backed, cheap), exactly as the list route clones its board authority.
pub struct TaskStatusUpdateState<D, N, W, A, AF> {
    /// Board resolution + membership + capability (the `TaskWriteDoorReads` port).
    pub door: D,
    /// The by-number → canonical-id lookup for the number arm.
    pub numbers: N,
    /// The core status write unit of work.
    pub write: W,
    /// Resolves the acting agent's display name for the `task_status` event pill.
    pub names: A,
    /// The mandatory post-commit aftermath, fired once on a committed change.
    pub aftermath: AF,
}

/// The wire body. Optionality + XOR are re-decided in [`ValidatedUpdateStatus`], not
/// leaned on serde: an absent field and a malformed combination are both the minimal
/// `422`, never a native "missing field" leak. No field carries a garde rule — every
/// semantic predicate (XOR, id/selector exclusivity, the status token) lives in the
/// `ValidatedUpdateStatus` refine, so admission is purely structural here.
#[derive(Debug, Deserialize, garde::Validate)]
pub(crate) struct UpdateStatusBody {
    #[garde(skip)]
    #[serde(default)]
    task_number: RequiredPatchField<i64>,
    #[garde(skip)]
    #[serde(default)]
    task_id: RequiredPatchField<String>,
    #[garde(skip)]
    status: String,
    #[garde(skip)]
    #[serde(default)]
    workspace_id: RequiredPatchField<String>,
    #[garde(skip)]
    #[serde(default)]
    workspace_name: RequiredPatchField<String>,
}

/// How the status change names its target — exactly one arm, decided by the XOR the
/// DTO enforces.
enum StatusTarget {
    /// A global-identity write; the id already determines the board, so no selector
    /// may accompany it.
    ById(TaskId),
    /// A per-board human number; the selector (or the source frame) resolves which
    /// board it counts on.
    ByNumber {
        task_number: i64,
        selector: WorkspaceSelector,
    },
}

/// The semantically-validated status change. Construction enforces every refine the
/// TS `UpdateTaskStatusInputSchema` does — exactly one of `task_number`/`task_id`,
/// `workspace_id` XOR `workspace_name`, a task id barring any workspace selector, and
/// a status token that is one of the five — collapsing all of them to the same
/// verdict-only `422` (no issue list).
struct ValidatedUpdateStatus {
    target: StatusTarget,
    status: TaskStatus,
}

impl ValidatedUpdateStatus {
    fn try_from_body(body: UpdateStatusBody) -> Result<Self, ()> {
        // Status must be one of the five canonical tokens (no aliases / casing).
        let status = TaskStatus::parse(&body.status).map_err(|_| ())?;

        // Exactly one of task_number / task_id.
        let task_number = body.task_number.into_option();
        let task_id = body.task_id.into_option();
        let workspace_id =
            crate::foundation::wire_patch::empty_selector_as_none(body.workspace_id.into_option());
        let workspace_name = crate::foundation::wire_patch::empty_selector_as_none(
            body.workspace_name.into_option(),
        );
        let target = match (task_number, task_id) {
            (Some(_), Some(_)) | (None, None) => return Err(()),
            (Some(task_number), None) => {
                // A number resolves on a board; a workspace selector may narrow it,
                // but the two selectors are mutually exclusive.
                if workspace_id.is_some() && workspace_name.is_some() {
                    return Err(());
                }
                StatusTarget::ByNumber {
                    task_number: TaskNumber::new(task_number).map_err(|_| ())?.get(),
                    selector: WorkspaceSelector {
                        workspace_id,
                        workspace_name,
                    },
                }
            }
            (None, Some(raw_id)) => {
                // A task id already determines its board: no workspace selector may
                // accompany it.
                if workspace_id.is_some() || workspace_name.is_some() {
                    return Err(());
                }
                // A structurally-empty id is a semantic refusal here, matching the
                // schema's `.min(1)` — not a `not_found` lookup.
                let id = TaskId::parse(raw_id).map_err(|_| ())?;
                StatusTarget::ById(id)
            }
        };

        Ok(Self { target, status })
    }
}

type Shared<D, N, W, A, AF> = State<Arc<TaskStatusUpdateState<D, N, W, A, AF>>>;

/// The status-change router on the internal path, unmounted. Backend-app applies
/// the runtime-credential authority and mounts it.
pub fn task_update_status_router<D, N, W, A, AF>(
    state: TaskStatusUpdateState<D, N, W, A, AF>,
) -> Router
where
    D: TaskWriteDoorReads + AgentExistenceReader + Clone + Send + Sync + 'static,
    N: TaskNumberResolver + Send + Sync + 'static,
    W: TaskStatusWrite + Send + Sync + 'static,
    A: AgentDisplayNameReader + Send + Sync + 'static,
    AF: TaskMutationAftermath,
{
    Router::new()
        .route(
            "/internal/agents/{aid}/tasks/update-status",
            post(update_status_route::<D, N, W, A, AF>),
        )
        .with_state(Arc::new(state))
}

async fn update_status_route<D, N, W, A, AF>(
    State(state): Shared<D, N, W, A, AF>,
    Path(aid): Path<String>,
    principal: Option<axum::extract::Extension<RuntimePrincipal>>,
    body: Result<ValidatedJson<UpdateStatusBody>, ValidatedJsonRejection>,
) -> Response
where
    D: TaskWriteDoorReads + AgentExistenceReader + Clone + Send + Sync + 'static,
    N: TaskNumberResolver + Send + Sync + 'static,
    W: TaskStatusWrite + Send + Sync + 'static,
    A: AgentDisplayNameReader + Send + Sync + 'static,
    AF: TaskMutationAftermath,
{
    // (1) IDENTITY GATE FIRST — before body, selector, capability, or store.
    let principal = match bind_runtime_actor(principal.as_ref().map(|ext| &ext.0), &aid) {
        Ok(principal) => principal,
        Err(refusal) => return refusal.into_response(),
    };
    let actor: &AgentId = principal.agent();
    let source = principal.source();

    // (2) ADMISSION: structural → native rejection; semantic → minimal 422.
    let body = match body {
        Ok(ValidatedJson(body)) => body,
        Err(rejection) => return rejection.into_response(),
    };
    let Ok(validated) = ValidatedUpdateStatus::try_from_body(body) else {
        return invalid_input();
    };

    // (3)/(4) Resolve the canonical target id, gating the board this agent may reach.
    let mut source_snapshot = None;
    let task_id = match validated.target {
        StatusTarget::ById(id) => {
            // §4.5: no by-id mutation surface off a conversation frame — the uniform
            // indistinguishable `task_not_found`, decided before any store touch.
            if source.is_some() {
                return task_not_found();
            }
            match state.door.agent_exists(actor).await {
                Ok(true) => {}
                Ok(false) => {
                    return mutation_failure(
                        "agent_not_found",
                        "agent not found",
                        serde_json::json!({}),
                    );
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
        StatusTarget::ByNumber {
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
                            serde_json::json!({}),
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
                    return mutation_failure(
                        "agent_not_found",
                        "agent not found",
                        serde_json::json!({}),
                    );
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
                    // `write_tasks` on the resolved workspace board (a direct-pair
                    // board has no capability axis — participation is its whole gate).
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
            // Resolve the per-board number to a canonical id. A number that names no
            // row on that board is the uniform `task_not_found` — the write UoW would
            // reach the same verdict, but there is no id to hand it.
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

    // (5) The core write UoW owns participation, read-only, permission, and the
    // transition table under its own lock; the adapter maps the outcome.
    // The actor pill name for the appended task_status event (fallback to id).
    let actor_name = match state.names.agent_display_name(actor).await {
        Ok(Some(name)) => name,
        Ok(None) => {
            return mutation_failure("agent_not_found", "agent not found", serde_json::json!({}));
        }
        Err(_) => return safe_500(),
    };
    match state
        .write
        .apply_agent_status(&task_id, actor, &actor_name, validated.status)
        .await
    {
        Ok(AgentStatusWriteOutcome::Applied(witness)) => {
            // Post-commit aftermath, fired once on the committed witness. Its failure
            // never rewrites the committed change as a failure.
            crate::conversation_messaging::messaging::task::aftermath::run_agent_aftermath(
                &state.aftermath,
                *witness,
                actor,
                source_snapshot,
            )
            .await;
            success()
        }
        Ok(AgentStatusWriteOutcome::AgentNotFound) => {
            mutation_failure("agent_not_found", "agent not found", serde_json::json!({}))
        }
        Ok(AgentStatusWriteOutcome::NotFound) => task_not_found(),
        Ok(AgentStatusWriteOutcome::ConversationReadonly) => conversation_readonly(),
        Ok(AgentStatusWriteOutcome::Refused(refusal)) => status_refused(refusal),
        Err(_) => safe_500(),
    }
}

/// `{ success: true }` — the CLI reads success from the body, so the HTTP status is
/// always `200`.
fn success() -> Response {
    (StatusCode::OK, Json(serde_json::json!({ "success": true }))).into_response()
}

/// The uniform anti-enumeration miss: no such task, OR the agent is not a
/// participant, OR a by-id write off a conversation frame. One envelope, `200`.
fn task_not_found() -> Response {
    mutation_failure("task_not_found", "task not found", serde_json::json!({}))
}

/// The task's direct-pair conversation is read-only (a peer endpoint is gone),
/// judged after participation so only a participant learns the peer left.
fn conversation_readonly() -> Response {
    mutation_failure(
        "conversation_readonly",
        "the conversation is read-only: a direct-pair endpoint is gone",
        serde_json::json!({}),
    )
}

/// The agent may reach the board but lacks `write_tasks` on it — the typed denial
/// naming the capability bucket, parity with `updateStatusAgent`'s `capability_denied`.
fn capability_denied() -> Response {
    mutation_failure(
        "capability_denied",
        "agent lacks the write_tasks capability in the target workspace",
        serde_json::json!({ "capability": "write_tasks" }),
    )
}

/// The core status refusal — either the permission gate (`not_assignee`) or the
/// transition table (`invalid_transition`, into which a same-status request folds,
/// matching the agent door's absence of a distinct `already_at_status`).
fn status_refused(refusal: AgentStatusRefusal) -> Response {
    match refusal {
        AgentStatusRefusal::NotAssignee => mutation_failure(
            "not_assignee",
            "only the assignee can update status (creator may update own unassigned task)",
            serde_json::json!({}),
        ),
        AgentStatusRefusal::Transition(_) => mutation_failure(
            "invalid_transition",
            "invalid status transition",
            serde_json::json!({}),
        ),
    }
}

/// A board-resolution refusal → the mutation envelope, parity with the codes
/// `resolveAgentTaskBoard` surfaces through `mutationFailure`.
fn board_refusal(refusal: BoardResolveRefusal) -> Response {
    let extra = refusal.candidates().map_or_else(
        || serde_json::json!({}),
        |candidates| {
            serde_json::json!({
                "workspace_candidates": candidates.iter().map(|w| w.as_str()).collect::<Vec<_>>(),
            })
        },
    );
    mutation_failure(refusal.code(), refusal.reason(), extra)
}

/// The shared `{ success: false, code, reason, ...extra }` envelope, always `200`.
fn mutation_failure(code: &str, reason: &str, extra: serde_json::Value) -> Response {
    let mut body = serde_json::json!({
        "success": false,
        "code": code,
        "reason": reason,
    });
    if let (Some(map), serde_json::Value::Object(extra)) = (body.as_object_mut(), extra) {
        map.extend(extra);
    }
    (StatusCode::OK, Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::extract::Extension;
    use axum::http::Request;
    use serde_json::{Value, json};
    use silverhand_backend_core::conversation_messaging::messaging::task::{
        BoardKey, PairKey, WorkspaceMembership,
    };
    use silverhand_backend_core::foundation::identity::{
        ConversationId, DaemonInstanceId, WorkspaceId,
    };
    use silverhand_backend_core::foundation::persistence::PersistenceFailure;
    use silverhand_backend_core::host_runtime::runtime_credential::{
        RuntimeCustody, RuntimeGeneration, RuntimeScope,
    };
    use silverhand_backend_core::identity_access::workspace_name::CanonicalWorkspaceName;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;

    const AGENT: &str = "agent-1";
    const CONVERSATION: &str = "conv-1";

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
                source: ConversationId::parse(CONVERSATION).unwrap(),
            },
            custody(),
        )
    }

    /// The recording door. Every read bumps its own counter, so a test asserts not
    /// just the verdict but WHICH reads the branch reached — the proof the branch
    /// itself consults the authority, not a standalone unit.
    #[derive(Clone)]
    struct RecordingDoor {
        agent_exists: bool,
        /// How many workspaces a name resolves to (drives the ambiguity branch).
        named: usize,
        /// The board a no-selector source conversation resolves to.
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
            _canonical: &CanonicalWorkspaceName,
        ) -> Result<Vec<WorkspaceId>, PersistenceFailure> {
            self.named_calls.fetch_add(1, Ordering::SeqCst);
            Ok((0..self.named)
                .map(|i| WorkspaceId::parse(format!("ws-{i}")).unwrap())
                .collect())
        }
        async fn conversation_board(
            &mut self,
            _conversation: &ConversationId,
        ) -> Result<Option<BoardKey>, PersistenceFailure> {
            self.board_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.board.clone())
        }
        async fn workspace_membership(
            &mut self,
            _workspace: &WorkspaceId,
            _agent: &AgentId,
        ) -> Result<WorkspaceMembership, PersistenceFailure> {
            self.membership_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.membership)
        }
        async fn has_capability(
            &mut self,
            _capability: TaskWriteCapability,
            _workspace: &WorkspaceId,
            _agent: &AgentId,
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
            _board_key: &str,
            _task_number: i64,
        ) -> Result<Option<TaskId>, PersistenceFailure> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.found.then(|| TaskId::parse("task-1").unwrap()))
        }
    }

    struct RecordingWrite {
        outcome: AgentStatusWriteOutcome,
        calls: Arc<AtomicUsize>,
    }
    impl TaskStatusWrite for RecordingWrite {
        async fn apply_agent_status(
            &self,
            _task_id: &TaskId,
            _actor: &AgentId,
            _actor_name: &silverhand_backend_core::conversation_messaging::messaging::task::AgentEventNames,
            _target: TaskStatus,
        ) -> Result<AgentStatusWriteOutcome, PersistenceFailure> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.outcome.clone())
        }
    }

    /// A name reader that resolves a fixed name, counting reads.
    struct RecordingNames;
    impl silverhand_backend_core::conversation_messaging::messaging::task::AgentDisplayNameReader
        for RecordingNames
    {
        async fn agent_display_name(
            &self,
            _agent: &AgentId,
        ) -> Result<
            Option<
                silverhand_backend_core::conversation_messaging::messaging::task::AgentEventNames,
            >,
            PersistenceFailure,
        > {
            Ok(Some(silverhand_backend_core::conversation_messaging::messaging::task::AgentEventNames::new("alice", "Alice")))
        }
    }

    /// A recording aftermath: counts how many committed witnesses fired it.
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

    /// The `Applied` witness the tests use.
    fn applied() -> AgentStatusWriteOutcome {
        AgentStatusWriteOutcome::Applied(Box::new(mutation_witness()))
    }
    fn mutation_witness()
    -> silverhand_backend_core::conversation_messaging::messaging::task::TaskMutationWitness {
        use silverhand_backend_core::conversation_messaging::messaging::task::{
            CreatedEventMessageId, TaskMutationWitness, TaskNumber,
        };
        use silverhand_backend_core::foundation::identity::ConversationId;
        TaskMutationWitness {
            kind: silverhand_backend_core::conversation_messaging::messaging::task::TaskEventKind::Status,
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
            status: TaskStatus::InProgress,
            assignee: None,
            closed_at: None,
            source_message_id: None,
        }
    }

    struct Counts {
        named: Arc<AtomicUsize>,
        board: Arc<AtomicUsize>,
        membership: Arc<AtomicUsize>,
        capability: Arc<AtomicUsize>,
        numbers: Arc<AtomicUsize>,
        write: Arc<AtomicUsize>,
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
        outcome: AgentStatusWriteOutcome,
    ) -> (Router, Counts) {
        let counts = Counts {
            named: Arc::new(AtomicUsize::new(0)),
            board: Arc::new(AtomicUsize::new(0)),
            membership: Arc::new(AtomicUsize::new(0)),
            capability: Arc::new(AtomicUsize::new(0)),
            numbers: Arc::new(AtomicUsize::new(0)),
            write: Arc::new(AtomicUsize::new(0)),
            aftermath: Arc::new(AtomicUsize::new(0)),
        };
        let state = TaskStatusUpdateState {
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
            names: RecordingNames,
            aftermath: RecordingAftermath {
                fired: Arc::clone(&counts.aftermath),
            },
        };
        let router = task_update_status_router(state).layer(Extension(principal));
        (router, counts)
    }

    async fn call_at(app: Router, path_aid: &str, body: String) -> (StatusCode, Value) {
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/internal/agents/{path_aid}/tasks/update-status"))
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

    // ------------------------------------------------------------------ identity

    #[tokio::test]
    async fn a_credential_for_another_agent_is_401_with_full_zero_reach() {
        // The credential is agent-1; the path names agent-2. The exact-match binding
        // refuses BEFORE admission, the door, the number resolver, and the write —
        // A can never act for B — with 401, the TS agent_mismatch replay.
        let (app, counts) = app(agent_slot(), DoorSpec::default(), true, applied());
        let (status, body) = call_at(
            app,
            "agent-2",
            r#"{"task_number":7,"status":"done"}"#.to_owned(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"], "unauthorized");
        assert_eq!(counts.named.load(Ordering::SeqCst), 0);
        assert_eq!(counts.board.load(Ordering::SeqCst), 0);
        assert_eq!(counts.membership.load(Ordering::SeqCst), 0);
        assert_eq!(counts.write.load(Ordering::SeqCst), 0, "write zero-reach");
    }

    #[tokio::test]
    async fn ghost_actor_is_typed_after_the_conversation_frame_by_id_collapse() {
        let (router, counts) = app(
            agent_slot(),
            DoorSpec {
                agent_exists: false,
                ..DoorSpec::default()
            },
            true,
            applied(),
        );
        let (status, body) =
            call(router, r#"{"task_id":"task-1","status":"done"}"#.to_owned()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["code"], "agent_not_found");
        assert_eq!(counts.write.load(Ordering::SeqCst), 0);

        let (app, counts) = app(
            conversation_slot(),
            DoorSpec {
                agent_exists: false,
                ..DoorSpec::default()
            },
            true,
            applied(),
        );
        let (status, body) = call(app, r#"{"task_id":"task-1","status":"done"}"#.to_owned()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["code"], "task_not_found");
        assert_eq!(counts.write.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_missing_credential_is_a_composition_500_zero_reach() {
        // No RuntimePrincipal extension at all — the credential middleware did not
        // run. That is a composition fault (500), not a caller 4xx, and nothing
        // downstream is touched.
        let state = TaskStatusUpdateState {
            door: RecordingDoor {
                agent_exists: true,
                named: 1,
                board: None,
                membership: WorkspaceMembership::Member,
                capability: true,
                named_calls: Arc::new(AtomicUsize::new(0)),
                board_calls: Arc::new(AtomicUsize::new(0)),
                membership_calls: Arc::new(AtomicUsize::new(0)),
                capability_calls: Arc::new(AtomicUsize::new(0)),
            },
            numbers: RecordingNumbers {
                found: true,
                calls: Arc::new(AtomicUsize::new(0)),
            },
            write: RecordingWrite {
                outcome: applied(),
                calls: Arc::new(AtomicUsize::new(0)),
            },
            names: RecordingNames,
            aftermath: RecordingAftermath {
                fired: Arc::new(AtomicUsize::new(0)),
            },
        };
        // No `.layer(Extension(..))`: the principal is absent.
        let router = task_update_status_router(state);
        let (status, _body) = call(router, r#"{"task_number":7,"status":"done"}"#.to_owned()).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    }

    // ---------------------------------------------------------------- admission

    #[tokio::test]
    async fn both_task_selectors_is_a_semantic_422_before_the_door() {
        let (app, counts) = app(agent_slot(), DoorSpec::default(), true, applied());
        let (status, body) = call(
            app,
            r#"{"task_number":7,"task_id":"task-1","status":"done"}"#.to_owned(),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["error"], "invalid input");
        assert_eq!(counts.board.load(Ordering::SeqCst), 0);
        assert_eq!(counts.write.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn task_number_consumes_the_shared_positive_js_safe_corpus_before_downstream_reads() {
        let fixture: Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../packages/shared/tests/fixtures/task-schedule-wire-canonicalization.json"
        )))
        .unwrap();
        for case in fixture["positive_js_safe"].as_array().unwrap() {
            let accepted = case["accepted"].as_bool().unwrap();
            let (app, counts) = app(
                conversation_slot(),
                DoorSpec {
                    board: Some(BoardKey::Pair(PairKey::parse("a__b").unwrap())),
                    ..DoorSpec::default()
                },
                true,
                applied(),
            );
            let (status, body) = call(
                app,
                json!({"task_number":case["value"].clone(),"status":"done"}).to_string(),
            )
            .await;
            if accepted {
                assert_eq!(status, StatusCode::OK, "{case}");
                assert_eq!(body["success"], true, "{case}");
                assert_eq!(counts.numbers.load(Ordering::SeqCst), 1, "{case}");
                assert_eq!(counts.write.load(Ordering::SeqCst), 1, "{case}");
            } else {
                assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{case}");
                assert_eq!(counts.board.load(Ordering::SeqCst), 0, "{case}");
                assert_eq!(counts.membership.load(Ordering::SeqCst), 0, "{case}");
                assert_eq!(counts.capability.load(Ordering::SeqCst), 0, "{case}");
                assert_eq!(counts.numbers.load(Ordering::SeqCst), 0, "{case}");
                assert_eq!(counts.write.load(Ordering::SeqCst), 0, "{case}");
                assert_eq!(counts.aftermath.load(Ordering::SeqCst), 0, "{case}");
            }
        }
    }

    #[tokio::test]
    async fn optional_nonnullable_fields_reject_null_and_empty_selectors_are_absent() {
        for body in [
            r#"{"task_number":null,"task_id":"task-1","status":"done"}"#,
            r#"{"task_number":7,"task_id":null,"status":"done"}"#,
            r#"{"task_id":"task-1","status":"done","workspace_id":null}"#,
            r#"{"task_id":"task-1","status":"done","workspace_name":null}"#,
        ] {
            let (app, counts) = app(agent_slot(), DoorSpec::default(), true, applied());
            let (status, _value) = call(app, body.to_owned()).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
            assert_eq!(counts.board.load(Ordering::SeqCst), 0, "{body}");
            assert_eq!(counts.numbers.load(Ordering::SeqCst), 0, "{body}");
            assert_eq!(counts.write.load(Ordering::SeqCst), 0, "{body}");
            assert_eq!(counts.aftermath.load(Ordering::SeqCst), 0, "{body}");
        }

        for body in [
            r#"{"task_id":"task-1","status":"done","workspace_id":""}"#,
            r#"{"task_id":"task-1","status":"done","workspace_name":""}"#,
            r#"{"task_id":"task-1","status":"done","workspace_id":"","workspace_name":""}"#,
        ] {
            let (app, counts) = app(agent_slot(), DoorSpec::default(), true, applied());
            let (status, value) = call(app, body.to_owned()).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(value["success"], true);
            assert_eq!(counts.write.load(Ordering::SeqCst), 1, "{body}");
        }

        let (app, counts) = app(
            conversation_slot(),
            DoorSpec {
                board: Some(BoardKey::Pair(PairKey::parse("a__b").unwrap())),
                ..DoorSpec::default()
            },
            true,
            applied(),
        );
        let (status, value) = call(
            app,
            r#"{"task_number":7,"status":"done","workspace_id":"","workspace_name":""}"#.to_owned(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["success"], true);
        assert_eq!(counts.board.load(Ordering::SeqCst), 1);
        assert_eq!(counts.write.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn an_unknown_status_token_is_a_semantic_422() {
        let (app, _counts) = app(agent_slot(), DoorSpec::default(), true, applied());
        let (status, body) = call(
            app,
            r#"{"task_id":"task-1","status":"archived"}"#.to_owned(),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["error"], "invalid input");
    }

    #[tokio::test]
    async fn a_task_id_with_a_workspace_selector_is_a_semantic_422() {
        // The id already determines its board; a selector alongside it is refused.
        let (app, _counts) = app(agent_slot(), DoorSpec::default(), true, applied());
        let (status, body) = call(
            app,
            r#"{"task_id":"task-1","status":"done","workspace_id":"ws-0"}"#.to_owned(),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["error"], "invalid input");
    }

    // ------------------------------------------------------------------- by-id

    #[tokio::test]
    async fn a_by_id_write_off_a_conversation_frame_is_task_not_found_zero_reach() {
        // §4.5: no by-id mutation surface for a conversation-frame principal — the
        // uniform indistinguishable task_not_found, decided before the door or write.
        let (app, counts) = app(conversation_slot(), DoorSpec::default(), true, applied());
        let (status, body) = call(app, r#"{"task_id":"task-1","status":"done"}"#.to_owned()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["success"], false);
        assert_eq!(body["code"], "task_not_found");
        assert_eq!(counts.write.load(Ordering::SeqCst), 0, "write zero-reach");
        assert_eq!(counts.board.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_by_id_write_on_an_agent_slot_hands_straight_to_the_write_uow() {
        // Agent-slot by-id: no board resolution, no capability — the id goes straight
        // to the write UoW, whose in-lock gates are the whole authorization.
        let (app, counts) = app(agent_slot(), DoorSpec::default(), true, applied());
        let (status, body) = call(app, r#"{"task_id":"task-1","status":"done"}"#.to_owned()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["success"], true);
        assert_eq!(counts.named.load(Ordering::SeqCst), 0);
        assert_eq!(counts.board.load(Ordering::SeqCst), 0);
        assert_eq!(counts.membership.load(Ordering::SeqCst), 0);
        assert_eq!(counts.write.load(Ordering::SeqCst), 1, "write reached");
        assert_eq!(
            counts.aftermath.load(Ordering::SeqCst),
            1,
            "aftermath fires exactly once on a committed change"
        );
    }

    // ---------------------------------------------------------------- by-number

    #[tokio::test]
    async fn ambiguous_name_is_answered_before_any_membership_read() {
        // A name matching >1 workspace is ambiguous_workspace, decided from the name
        // count alone: membership, capability, the number resolver, and the write are
        // ALL zero-reach, so the count cannot be rewritten by a later authorization.
        let (app, counts) = app(
            agent_slot(),
            DoorSpec {
                named: 2,
                ..DoorSpec::default()
            },
            true,
            applied(),
        );
        let (status, body) = call(
            app,
            r#"{"task_number":7,"status":"done","workspace_name":"dup"}"#.to_owned(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["code"], "ambiguous_workspace");
        assert_eq!(
            body["workspace_candidates"],
            serde_json::json!(["ws-0", "ws-1"])
        );
        assert_eq!(counts.named.load(Ordering::SeqCst), 1);
        assert_eq!(
            counts.membership.load(Ordering::SeqCst),
            0,
            "membership zero-reach: ambiguity decided before it"
        );
        assert_eq!(counts.capability.load(Ordering::SeqCst), 0);
        assert_eq!(counts.numbers.load(Ordering::SeqCst), 0);
        assert_eq!(counts.write.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_non_member_of_a_named_workspace_is_membership_required_zero_reach() {
        let (app, counts) = app(
            agent_slot(),
            DoorSpec {
                named: 1,
                membership: WorkspaceMembership::NotMember,
                ..DoorSpec::default()
            },
            true,
            applied(),
        );
        let (status, body) = call(
            app,
            r#"{"task_number":7,"status":"done","workspace_name":"only"}"#.to_owned(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["code"], "workspace_membership_required");
        assert_eq!(counts.membership.load(Ordering::SeqCst), 1);
        assert_eq!(
            counts.capability.load(Ordering::SeqCst),
            0,
            "capability zero-reach when not a member"
        );
        assert_eq!(counts.numbers.load(Ordering::SeqCst), 0);
        assert_eq!(counts.write.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_member_without_write_tasks_is_capability_denied_zero_reach() {
        // Member of the workspace, but lacking write_tasks: the typed capability
        // denial, and the number resolver + write are never reached.
        let (app, counts) = app(
            agent_slot(),
            DoorSpec {
                named: 1,
                membership: WorkspaceMembership::Member,
                capability: false,
                ..DoorSpec::default()
            },
            true,
            applied(),
        );
        let (status, body) = call(
            app,
            r#"{"task_number":7,"status":"done","workspace_name":"only"}"#.to_owned(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["code"], "capability_denied");
        assert_eq!(body["capability"], "write_tasks");
        assert_eq!(counts.membership.load(Ordering::SeqCst), 1);
        assert_eq!(counts.capability.load(Ordering::SeqCst), 1);
        assert_eq!(
            counts.numbers.load(Ordering::SeqCst),
            0,
            "number resolver zero-reach when capability denied"
        );
        assert_eq!(counts.write.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_number_naming_no_row_on_its_board_is_task_not_found() {
        let (app, counts) = app(
            agent_slot(),
            DoorSpec {
                named: 1,
                membership: WorkspaceMembership::Member,
                capability: true,
                ..DoorSpec::default()
            },
            false,
            applied(),
        );
        let (status, body) = call(
            app,
            r#"{"task_number":7,"status":"done","workspace_name":"only"}"#.to_owned(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["code"], "task_not_found");
        assert_eq!(counts.numbers.load(Ordering::SeqCst), 1);
        assert_eq!(
            counts.write.load(Ordering::SeqCst),
            0,
            "write zero-reach when the number resolves to no id"
        );
    }

    #[tokio::test]
    async fn a_direct_pair_frame_write_reaches_no_membership_or_capability() {
        // A no-selector conversation frame whose board is a direct pair: participation
        // is the whole authority (the write UoW's gate), so the door consults its own
        // board once and NEITHER membership NOR capability.
        let (app, counts) = app(
            conversation_slot(),
            DoorSpec {
                board: Some(BoardKey::Pair(
                    silverhand_backend_core::conversation_messaging::messaging::task::PairKey::parse(
                        "a__b",
                    )
                    .unwrap(),
                )),
                ..DoorSpec::default()
            },
            true,
            applied(),
        );
        let (status, body) = call(app, r#"{"task_number":7,"status":"done"}"#.to_owned()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["success"], true);
        assert_eq!(
            counts.board.load(Ordering::SeqCst),
            1,
            "own board consulted"
        );
        assert_eq!(
            counts.membership.load(Ordering::SeqCst),
            0,
            "a pair board has no membership axis"
        );
        assert_eq!(
            counts.capability.load(Ordering::SeqCst),
            0,
            "a pair board has no capability axis"
        );
        assert_eq!(counts.numbers.load(Ordering::SeqCst), 1);
        assert_eq!(counts.write.load(Ordering::SeqCst), 1);
    }

    // ------------------------------------------------------------ outcome map

    #[tokio::test]
    async fn write_not_found_maps_to_task_not_found() {
        let (app, _counts) = app(
            agent_slot(),
            DoorSpec::default(),
            true,
            AgentStatusWriteOutcome::NotFound,
        );
        let (status, body) = call(app, r#"{"task_id":"task-1","status":"done"}"#.to_owned()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["code"], "task_not_found");
    }

    #[tokio::test]
    async fn write_conversation_readonly_maps_to_conversation_readonly() {
        let (app, _counts) = app(
            agent_slot(),
            DoorSpec::default(),
            true,
            AgentStatusWriteOutcome::ConversationReadonly,
        );
        let (status, body) = call(app, r#"{"task_id":"task-1","status":"done"}"#.to_owned()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["code"], "conversation_readonly");
    }

    #[tokio::test]
    async fn write_not_assignee_maps_to_not_assignee() {
        let (app, counts) = app(
            agent_slot(),
            DoorSpec::default(),
            true,
            AgentStatusWriteOutcome::Refused(AgentStatusRefusal::NotAssignee),
        );
        let (status, body) = call(app, r#"{"task_id":"task-1","status":"done"}"#.to_owned()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["code"], "not_assignee");
        assert_eq!(
            counts.aftermath.load(Ordering::SeqCst),
            0,
            "a refusal fires no aftermath"
        );
    }
}
