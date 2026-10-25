//! `POST /internal/agents/{aid}/tasks/create` — the agent task-create face, and the
//! narrow target-resolution port it consumes.
//!
//! Create is the one write face that names its destination by an ADDRESS (not a
//! `:taskId` / by-number board), so it needs address→conversation resolution. That
//! resolution is a sibling lane's authority (#2333's canonical
//! `TargetAddress::resolve`); this leaf does NOT merge or cherry-pick it and does
//! NOT couple to `messaging::pre_send`. It declares its OWN narrow consumer port
//! ([`TaskCreateTargetResolver`]) in TASK's vocabulary — the minimal resolved
//! placement + refusal, explicit WRITE intent, allocation policy for a `dm:` with no
//! pair yet — and the composition later delegates it to the canonical resolver via
//! an adapter. The type dependency points at create's requirement, not the reverse.
//!
//! Fixed order (parity with the TS `taskCreate` route):
//!  1. IDENTITY: [`bind_runtime_actor`] binds the actor to `{aid}` first.
//!  2. ADMISSION: structural → native; semantic (empty batch, bad title/body) → 422.
//!  3. RESOLVE the target address FIRST — WRITE intent, so an unwritable/unreachable
//!     destination refuses here; unknown and unauthorized collapse to one answer.
//!  4. CROSS-CONVERSATION CLAIM: `--claim` filing into a stream other than the
//!     actor's own conversation frame is refused (an agent must not pin work it
//!     cannot execute onto another stream).
//!  5. AUTHORIZE the destination: a workspace board needs the agent to be able to
//!     reach the workspace, then `create_tasks` (plus `write_tasks` for `--claim`);
//!     a direct-pair board has no capability axis (participation is the whole gate).
//!  6. COMPOSE + WRITE each task via the core [`TaskCreateWrite`] UoW (number
//!     allocation + row + `task_create` event + backfill, one tx), collecting the
//!     batch; a committed create fires the shared aftermath once per task.

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
    AgentDisplayNameReader, BoardKey, CreatePlacement, ExistingSourceTask, TaskContent,
    TaskCreateAdmissionFailure, TaskCreateAdmissionReads, TaskCreateBatch, TaskCreateOutcome,
    TaskCreatePlan, TaskCreateWrite, TaskCreatedWitness, TaskCreator, TaskEventAssignee,
    TaskMutationWitness, TaskParticipationReads, TaskSystemActor, TaskTitle, TaskWriteCapability,
    TaskWriteDoorReads, ThreadRootId, admit_task_create,
};
use silverhand_backend_core::foundation::identity::{AgentId, ConversationId, WorkspaceId};
use silverhand_backend_core::host_runtime::runtime_credential::RuntimePrincipal;

use crate::conversation_messaging::messaging::task::aftermath::{
    TaskMutationAftermath, run_agent_aftermath,
};
use crate::foundation::http_error::plain_500 as safe_500;
use crate::foundation::wire_admission::{ValidatedJson, ValidatedJsonRejection, invalid_input};
use crate::foundation::wire_patch::RequiredPatchField;
use crate::host_runtime::runtime_credential::bind_runtime_actor;

// ── The narrow target-resolution port (create's own vocabulary) ──────────────

/// Why a task-create target address did not resolve to a usable destination — the
/// TS `resolveDeliveryTargetForRoute` verdicts for the create door, so delegating to
/// the canonical resolver cannot change the observable answer. Unknown and
/// unauthorized-to-reach COLLAPSE to `Unresolved` (anti-enumeration).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskTargetRefusal {
    /// Not an address (`400 target_malformed`).
    Malformed,
    /// A DM to oneself (`400`).
    SelfPairNotAddressable,
    /// Names no stream the caller can use — unknown ∪ unauthorized (`404`).
    Unresolved,
    /// A reply id that is not a cluster root of the named conversation (`404`).
    ThreadNotInTarget,
    /// Resolution proved participation but the credential's slot may not reach the
    /// stream (`403`) — only about a stream the actor can already enumerate.
    SlotOutOfScope,
}

/// The placement of a resolved create destination — selects the authorization axis
/// (a workspace board gates capabilities; a direct pair is participation-only).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResolvedPlacement {
    /// A workspace channel; `workspace` is the board the capabilities gate on.
    Workspace { workspace: WorkspaceId },
    /// A direct pair — no capability axis.
    DirectPair,
}

/// A resolved create destination: the conversation the task is filed against, its
/// optional thread-cluster root (the `/thread:` suffix), and its placement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedTaskTarget {
    pub conversation: ConversationId,
    pub thread_root: Option<ThreadRootId>,
    pub placement: ResolvedPlacement,
}

/// Whether target resolution may allocate a not-yet-existing direct pair. The
/// task-create reach table permits allocation for both runtime slot kinds; keeping
/// the value explicit prevents composition from guessing or defaulting the policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskCreatePairAllocation {
    Permitted,
}

/// Typed resolver intent. Task create always writes both a task row and its
/// conversation event; callers cannot accidentally ask the canonical resolver for a
/// read-shaped answer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskCreateTargetIntent {
    Write,
}

/// Resolve the wire `target` address for a task create. The caller must explicitly
/// supply the task-create allocation and write intent; an unwritable/unreachable
/// destination is refused here, and a `dm:@handle` with no pair yet is the resolver's
/// business to mint when allocation is permitted. ADDRESS RESOLUTION ONLY: it never
/// judges `create_tasks`/`write_tasks` (those are the door's next step) and never runs
/// send admission.
pub trait TaskCreateTargetResolver {
    type Error: std::error::Error + Send + Sync + 'static;

    fn resolve(
        &self,
        agent: &AgentId,
        address: &str,
        allocation: TaskCreatePairAllocation,
        intent: TaskCreateTargetIntent,
    ) -> impl std::future::Future<
        Output = Result<Result<ResolvedTaskTarget, TaskTargetRefusal>, Self::Error>,
    > + Send;
}

// ── State + wire body ────────────────────────────────────────────────────────

pub struct TaskCreateState<R, D, W, A, AF> {
    /// Address → destination resolution (the narrow sibling-lane consumer port).
    pub resolver: R,
    /// `create_tasks` / `write_tasks` capability on a workspace board.
    pub door: D,
    /// The core create write unit of work.
    pub write: W,
    /// Resolves the actor/assignee display name for the `task_create` event pill.
    pub names: A,
    /// The mandatory post-commit aftermath, fired once per committed task.
    pub aftermath: AF,
}

#[derive(Debug, Deserialize, garde::Validate)]
pub(crate) struct CreateBody {
    #[garde(skip)]
    tasks: Vec<CreateTaskItem>,
    #[garde(skip)]
    #[serde(default)]
    claim: RequiredPatchField<bool>,
    #[garde(skip)]
    target: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CreateTaskItem {
    title: String,
    #[serde(default)]
    body: RequiredPatchField<String>,
    #[serde(default)]
    source_message_id: RequiredPatchField<String>,
}

/// A validated create item: title + optional body parsed to their value
/// authorities, and the optional source-message provenance id parsed at admission.
struct ValidatedItem {
    title: TaskTitle,
    content: TaskContent,
    source_message_id:
        Option<silverhand_backend_core::conversation_messaging::messaging::task::SourceMessageId>,
}

struct ValidatedCreate {
    items: Vec<ValidatedItem>,
    claim: bool,
    target: String,
}

impl ValidatedCreate {
    fn try_from_body(body: CreateBody) -> Result<Self, ()> {
        if body.tasks.is_empty() || body.target.is_empty() {
            return Err(());
        }
        let mut items = Vec::with_capacity(body.tasks.len());
        for item in body.tasks {
            let title = TaskTitle::parse(&item.title).map_err(|_| ())?;
            let content = match item.body.into_option() {
                Some(raw) => TaskContent::parse(raw).map_err(|_| ())?,
                None => TaskContent::empty(),
            };
            // The provenance id (if present) must be a valid non-empty id — a
            // semantic refusal here, not a `not_found` at the writer.
            let source_message_id = match item.source_message_id.into_option() {
                Some(raw) => Some(
                    silverhand_backend_core::conversation_messaging::messaging::task::SourceMessageId::parse(raw)
                        .map_err(|_| ())?,
                ),
                None => None,
            };
            items.push(ValidatedItem {
                title,
                content,
                source_message_id,
            });
        }
        Ok(Self {
            items,
            claim: body.claim.into_option().unwrap_or(false),
            target: body.target,
        })
    }
}

type Shared<R, D, W, A, AF> = State<Arc<TaskCreateState<R, D, W, A, AF>>>;

pub fn task_create_router<R, D, W, A, AF>(state: TaskCreateState<R, D, W, A, AF>) -> Router
where
    R: TaskCreateTargetResolver + Send + Sync + 'static,
    D: TaskWriteDoorReads
        + TaskCreateAdmissionReads
        + TaskParticipationReads
        + Clone
        + Send
        + Sync
        + 'static,
    W: TaskCreateWrite + Send + Sync + 'static,
    A: AgentDisplayNameReader + Send + Sync + 'static,
    AF: TaskMutationAftermath,
{
    Router::new()
        .route(
            "/internal/agents/{aid}/tasks/create",
            post(create_route::<R, D, W, A, AF>),
        )
        .with_state(Arc::new(state))
}

async fn create_route<R, D, W, A, AF>(
    State(state): Shared<R, D, W, A, AF>,
    Path(aid): Path<String>,
    principal: Option<axum::extract::Extension<RuntimePrincipal>>,
    body: Result<ValidatedJson<CreateBody>, ValidatedJsonRejection>,
) -> Response
where
    R: TaskCreateTargetResolver + Send + Sync + 'static,
    D: TaskWriteDoorReads
        + TaskCreateAdmissionReads
        + TaskParticipationReads
        + Clone
        + Send
        + Sync
        + 'static,
    W: TaskCreateWrite + Send + Sync + 'static,
    A: AgentDisplayNameReader + Send + Sync + 'static,
    AF: TaskMutationAftermath,
{
    // (1) IDENTITY.
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
    let Ok(validated) = ValidatedCreate::try_from_body(body) else {
        return invalid_input();
    };

    // (3) RESOLVE the target FIRST (write intent).
    let resolved = match state
        .resolver
        .resolve(
            actor,
            &validated.target,
            TaskCreatePairAllocation::Permitted,
            TaskCreateTargetIntent::Write,
        )
        .await
    {
        Ok(Ok(target)) => target,
        Ok(Err(refusal)) => return target_refusal(refusal),
        Err(_) => return safe_500(),
    };

    // (4) CROSS-CONVERSATION CLAIM: `--claim` into a stream other than the actor's
    // own conversation frame is refused (an agent must not pin work onto another
    // stream). "Cross" is a fact about the conversation-frame actor; an agent-slot
    // (no origin anchor) is never "cross" and falls through.
    if validated.claim
        && let Some(origin) = source
        && *origin != resolved.conversation
    {
        return cross_conversation_claim();
    }

    // (5) AUTHORIZE the destination.
    let mut door = state.door.clone();
    match &resolved.placement {
        ResolvedPlacement::Workspace { workspace } => {
            // create_tasks first; write_tasks additionally for --claim. A missing
            // grant is the same forbidden either way.
            match door
                .has_capability(TaskWriteCapability::Create, workspace, actor)
                .await
            {
                Ok(true) => {}
                Ok(false) => return capability_denied("create_tasks"),
                Err(_) => return safe_500(),
            }
            if validated.claim {
                match door
                    .has_capability(TaskWriteCapability::Write, workspace, actor)
                    .await
                {
                    Ok(true) => {}
                    Ok(false) => return capability_denied("write_tasks"),
                    Err(_) => return safe_500(),
                }
            }
        }
        ResolvedPlacement::DirectPair => {}
    }

    // (6) CREATE AGGREGATE ADMISSION. The resolver/capability door has already
    // answered address reachability; the shared create policy now re-reads the
    // destination lifecycle + participation and validates every source before the
    // batch transaction. Source uniqueness is replayed by the writer's UNIQUE key.
    let creator = TaskCreator::Agent(actor.clone());
    let sources: Vec<_> = validated
        .items
        .iter()
        .filter_map(|item| item.source_message_id.clone())
        .collect();
    let admission_reads = state.door.clone();
    match admit_task_create(
        &creator,
        &resolved.conversation,
        &sources,
        &admission_reads,
        &mut door,
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(failure)) => return create_admission_refusal(failure),
        Err(_) => return safe_500(),
    }

    // (6) COMPOSE + WRITE each task. The actor pill name resolves once (fallback id);
    // a self-claim assigns the actor, so the assignee DTO is the same actor.
    let actor_name = match state.names.agent_display_name(actor).await {
        Ok(Some(name)) => name,
        Ok(None) => {
            return (StatusCode::OK, Json(json!({ "error": "agent_not_found" }))).into_response();
        }
        Err(_) => return safe_500(),
    };
    let placement_kind = if source.is_some_and(|origin| *origin != resolved.conversation) {
        CreatePlacement::CrossConversation
    } else if validated.claim {
        CreatePlacement::Claimed
    } else {
        CreatePlacement::Filed
    };
    let board = match &resolved.placement {
        ResolvedPlacement::Workspace { workspace } => BoardKey::Workspace(workspace.clone()),
        ResolvedPlacement::DirectPair => {
            match board_key_for(&mut state.door.clone(), &resolved.conversation).await {
                Ok(Some(BoardKey::Pair(pair))) => BoardKey::Pair(pair),
                Ok(_) => return safe_500(),
                Err(_) => return safe_500(),
            }
        }
    };

    let event_assignee = if validated.claim {
        Some(TaskEventAssignee::Agent {
            id: actor.clone(),
            names: actor_name.clone(),
        })
    } else {
        None
    };

    // Build the whole batch of plans, then commit them as ONE transaction. A refusal
    // (source-conflict/read-only) is the WHOLE create's — nothing written, no
    // aftermath — matching the TS batch `uow.atomically`.
    let plans: Vec<TaskCreatePlan> = validated
        .items
        .into_iter()
        .map(|item| TaskCreatePlan {
            thread_root_id: resolved.thread_root.clone(),
            title: item.title,
            content: item.content,
            claim_by: if validated.claim {
                Some(actor.clone())
            } else {
                None
            },
            source_message_id: item.source_message_id,
            event_actor: TaskSystemActor::Agent {
                id: actor.clone(),
                names: actor_name.clone(),
            },
            event_assignee: event_assignee.clone(),
            event_placement: placement_kind,
        })
        .collect();
    let batch = TaskCreateBatch::new(resolved.conversation.clone(), board, creator, plans);
    let source_snapshot = {
        let mut door = state.door.clone();
        match crate::conversation_messaging::messaging::task::aftermath::capture_task_source(
            &mut door, source,
        )
        .await
        {
            Ok(source) => source,
            Err(_) => return safe_500(),
        }
    };

    match state.write.create_tasks(&batch).await {
        Ok(TaskCreateOutcome::Created(witnesses)) => {
            // Post-commit: fire the aftermath once per committed task, then ACK the
            // whole batch as {tasks:[{id,task_number,title}]}.
            let mut tasks = Vec::with_capacity(witnesses.len());
            for witness in &witnesses {
                run_agent_aftermath(
                    &state.aftermath,
                    mutation_witness(witness),
                    actor,
                    source_snapshot.clone(),
                )
                .await;
                tasks.push(created_entry(witness));
            }
            (StatusCode::OK, Json(json!({ "tasks": tasks }))).into_response()
        }
        Ok(TaskCreateOutcome::AgentNotFound) => {
            (StatusCode::OK, Json(json!({ "error": "agent_not_found" }))).into_response()
        }
        // A whole-create refusal — NOT a per-item array (TS parity): the failure
        // envelope is the entire create's.
        Ok(TaskCreateOutcome::SourceConflict { existing_task }) => source_conflict(existing_task),
        Ok(TaskCreateOutcome::SourceMessageNotFound) => {
            create_admission_refusal(TaskCreateAdmissionFailure::SourceMessageNotFound)
        }
        Ok(TaskCreateOutcome::SourceConversationMismatch) => {
            create_admission_refusal(TaskCreateAdmissionFailure::SourceConversationMismatch)
        }
        Ok(TaskCreateOutcome::SourceNotAccessible) => {
            create_admission_refusal(TaskCreateAdmissionFailure::SourceNotAccessible)
        }
        Ok(TaskCreateOutcome::CapabilityDenied(TaskWriteCapability::Create)) => {
            capability_denied("create_tasks")
        }
        Ok(TaskCreateOutcome::CapabilityDenied(TaskWriteCapability::Write)) => {
            capability_denied("write_tasks")
        }
        Ok(TaskCreateOutcome::CapabilityDenied(TaskWriteCapability::Read)) => safe_500(),
        Ok(TaskCreateOutcome::ConversationNotFound) => {
            create_admission_refusal(TaskCreateAdmissionFailure::ConversationNotFound)
        }
        Ok(TaskCreateOutcome::ConversationClosed) => {
            create_admission_refusal(TaskCreateAdmissionFailure::ConversationClosed)
        }
        Ok(TaskCreateOutcome::ConversationArchived) => {
            create_admission_refusal(TaskCreateAdmissionFailure::ConversationArchived)
        }
        Ok(TaskCreateOutcome::NotConversationMember) => {
            create_admission_refusal(TaskCreateAdmissionFailure::NotConversationMember)
        }
        Ok(TaskCreateOutcome::ConversationReadonly) => conversation_readonly(),
        Err(_) => safe_500(),
    }
}

/// The `board_key` a conversation's placement derives — used for the direct-pair arm
/// where the resolved placement carries no workspace.
async fn board_key_for<D: TaskWriteDoorReads>(
    door: &mut D,
    conversation: &ConversationId,
) -> Result<Option<BoardKey>, silverhand_backend_core::foundation::persistence::PersistenceFailure>
{
    door.conversation_board(conversation).await
}

/// Project the create witness onto the shared [`TaskMutationWitness`] the aftermath
/// consumes — the same durable-fact coordinates under the create field name.
fn mutation_witness(witness: &TaskCreatedWitness) -> TaskMutationWitness {
    witness.mutation().clone()
}

fn created_entry(witness: &TaskCreatedWitness) -> Value {
    json!({
        "id": witness.task_id().as_str(),
        "task_number": witness.task_number().get(),
        "title": witness.title().as_str(),
    })
}

/// The whole-create source-conflict envelope, `{error:'source_conflict',
/// existing_task?}` (the TS shape — not a per-item array entry). Always `200` (the
/// typed op-error the CLI classifies by `error`).
fn source_conflict(existing: Option<ExistingSourceTask>) -> Response {
    let mut body = json!({ "error": "source_conflict" });
    if let (Some(map), Some(existing)) = (body.as_object_mut(), existing) {
        map.insert(
            "existing_task".to_owned(),
            json!({
                "id": existing.task_id.as_str(),
                "task_number": existing.task_number.get(),
                "status": existing.status.as_str(),
            }),
        );
    }
    (StatusCode::OK, Json(body)).into_response()
}
fn conversation_readonly() -> Response {
    (
        StatusCode::OK,
        Json(json!({ "error": "conversation_readonly" })),
    )
        .into_response()
}

fn create_admission_refusal(failure: TaskCreateAdmissionFailure) -> Response {
    let (code, existing) = match failure {
        TaskCreateAdmissionFailure::ConversationNotFound => ("conversation_not_found", None),
        TaskCreateAdmissionFailure::ConversationClosed => ("conversation_closed", None),
        TaskCreateAdmissionFailure::ConversationArchived => ("conversation_archived", None),
        TaskCreateAdmissionFailure::AgentNotFound => ("agent_not_found", None),
        TaskCreateAdmissionFailure::NotConversationMember => ("not_conversation_member", None),
        TaskCreateAdmissionFailure::SourceMessageNotFound => ("source_message_not_found", None),
        TaskCreateAdmissionFailure::SourceConversationMismatch => {
            ("source_conversation_mismatch", None)
        }
        TaskCreateAdmissionFailure::SourceNotAccessible => ("source_not_accessible", None),
        TaskCreateAdmissionFailure::SourceConflict { existing_task } => {
            ("source_conflict", Some(existing_task))
        }
    };
    if code == "source_conflict" {
        return source_conflict(existing);
    }
    (StatusCode::OK, Json(json!({ "error": code }))).into_response()
}

fn target_refusal(refusal: TaskTargetRefusal) -> Response {
    use crate::conversation_messaging::messaging::target_refusal_wire::TargetRefusalWire;
    match refusal {
        TaskTargetRefusal::Malformed => TargetRefusalWire::Malformed,
        TaskTargetRefusal::SelfPairNotAddressable => TargetRefusalWire::SelfPairNotAddressable,
        TaskTargetRefusal::Unresolved => TargetRefusalWire::Unresolved,
        TaskTargetRefusal::ThreadNotInTarget => TargetRefusalWire::ThreadNotInTarget,
        TaskTargetRefusal::SlotOutOfScope => TargetRefusalWire::SlotOutOfScope,
    }
    .into_response()
}

fn cross_conversation_claim() -> Response {
    // The typed 200 op-error envelope (like source_conflict) so the CLI classifies
    // by `error`; same-stream create+claim is untouched.
    (
        StatusCode::OK,
        Json(json!({ "error": "cross_conversation_claim" })),
    )
        .into_response()
}

fn capability_denied(capability: &'static str) -> Response {
    crate::conversation_messaging::messaging::agent_capability_wire::capability_denied(capability)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::extract::Extension;
    use axum::http::Request;
    use silverhand_backend_core::conversation_messaging::messaging::task::{
        CreatedEventMessageId, SourceMessageId, TaskCreateConversationState, TaskCreateSourceRef,
        TaskId, TaskNumber, TaskStatus, WorkspaceMembership,
    };
    use silverhand_backend_core::foundation::identity::DaemonInstanceId;
    use silverhand_backend_core::foundation::persistence::PersistenceFailure;
    use silverhand_backend_core::host_runtime::runtime_credential::{
        RuntimeCustody, RuntimeGeneration, RuntimeScope,
    };
    use silverhand_backend_core::identity_access::workspace_name::CanonicalWorkspaceName;
    use std::convert::Infallible;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;

    const AGENT: &str = "agent-1";
    const CONV: &str = "conv-1";

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
    fn conversation_slot(source: &str) -> RuntimePrincipal {
        RuntimePrincipal::for_scope(
            RuntimeScope::ConversationSlot {
                agent: AgentId::parse(AGENT).unwrap(),
                source: ConversationId::parse(source).unwrap(),
            },
            custody(),
        )
    }

    /// A resolver that resolves to a fixed workspace destination, or refuses.
    struct Resolver {
        refuse: Option<TaskTargetRefusal>,
        placement: ResolvedPlacement,
        calls: Arc<AtomicUsize>,
        allocation: Arc<Mutex<Option<TaskCreatePairAllocation>>>,
        intent: Arc<Mutex<Option<TaskCreateTargetIntent>>>,
    }
    impl TaskCreateTargetResolver for Resolver {
        type Error = Infallible;
        async fn resolve(
            &self,
            _agent: &AgentId,
            _address: &str,
            allocation: TaskCreatePairAllocation,
            intent: TaskCreateTargetIntent,
        ) -> Result<Result<ResolvedTaskTarget, TaskTargetRefusal>, Infallible> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.allocation.lock().unwrap() = Some(allocation);
            *self.intent.lock().unwrap() = Some(intent);
            Ok(match self.refuse {
                Some(refusal) => Err(refusal),
                None => Ok(ResolvedTaskTarget {
                    conversation: ConversationId::parse(CONV).unwrap(),
                    thread_root: None,
                    placement: self.placement.clone(),
                }),
            })
        }
    }

    #[derive(Clone)]
    struct Door {
        agent_exists: bool,
        create_cap: bool,
        write_cap: bool,
        create_calls: Arc<AtomicUsize>,
        write_calls: Arc<AtomicUsize>,
        lifecycle: Option<TaskCreateConversationState>,
        participant: bool,
        source_mode: SourceMode,
    }

    #[derive(Clone, Copy)]
    enum SourceMode {
        Usable,
        Missing,
        WrongConversation,
        Existing,
    }
    impl TaskWriteDoorReads for Door {
        async fn may_mutate_task_by_id(
            &mut self,
            _task: &TaskId,
            _agent: &AgentId,
            _required: silverhand_backend_core::conversation_messaging::messaging::task::TaskByIdCapability,
        ) -> Result<bool, PersistenceFailure> {
            Ok(true)
        }

        async fn workspaces_named(
            &mut self,
            _c: &CanonicalWorkspaceName,
        ) -> Result<Vec<WorkspaceId>, PersistenceFailure> {
            Ok(vec![])
        }
        async fn conversation_board(
            &mut self,
            _c: &ConversationId,
        ) -> Result<Option<BoardKey>, PersistenceFailure> {
            Ok(Some(BoardKey::Pair(
                silverhand_backend_core::conversation_messaging::messaging::task::PairKey::parse(
                    "a__b",
                )
                .unwrap(),
            )))
        }
        async fn workspace_membership(
            &mut self,
            _w: &WorkspaceId,
            _a: &AgentId,
        ) -> Result<WorkspaceMembership, PersistenceFailure> {
            Ok(WorkspaceMembership::Member)
        }
        async fn has_capability(
            &mut self,
            cap: TaskWriteCapability,
            _w: &WorkspaceId,
            _a: &AgentId,
        ) -> Result<bool, PersistenceFailure> {
            match cap {
                TaskWriteCapability::Create => {
                    self.create_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(self.create_cap)
                }
                TaskWriteCapability::Write => {
                    self.write_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(self.write_cap)
                }
                TaskWriteCapability::Read => Ok(false),
            }
        }
    }

    impl TaskCreateAdmissionReads for Door {
        async fn agent_exists(&self, _agent: &AgentId) -> Result<bool, PersistenceFailure> {
            Ok(self.agent_exists)
        }

        async fn conversation_state(
            &self,
            _conversation: &ConversationId,
        ) -> Result<Option<TaskCreateConversationState>, PersistenceFailure> {
            Ok(self.lifecycle)
        }

        async fn source_message(
            &self,
            source: &SourceMessageId,
        ) -> Result<Option<TaskCreateSourceRef>, PersistenceFailure> {
            Ok(match self.source_mode {
                SourceMode::Missing => None,
                SourceMode::WrongConversation => Some(TaskCreateSourceRef {
                    id: source.clone(),
                    conversation_id: ConversationId::parse("conv-other").unwrap(),
                }),
                SourceMode::Usable | SourceMode::Existing => Some(TaskCreateSourceRef {
                    id: source.clone(),
                    conversation_id: ConversationId::parse(CONV).unwrap(),
                }),
            })
        }

        async fn task_for_source(
            &self,
            _source: &SourceMessageId,
        ) -> Result<Option<ExistingSourceTask>, PersistenceFailure> {
            Ok(
                matches!(self.source_mode, SourceMode::Existing).then(|| ExistingSourceTask {
                    task_id: TaskId::parse("existing-task").unwrap(),
                    task_number: TaskNumber::new(7).unwrap(),
                    status: TaskStatus::InProgress,
                }),
            )
        }
    }

    impl TaskParticipationReads for Door {
        async fn is_agent_participant(
            &mut self,
            _conversation: &ConversationId,
            _agent: &AgentId,
        ) -> Result<bool, PersistenceFailure> {
            Ok(self.participant)
        }

        async fn is_user_participant(
            &mut self,
            _conversation: &ConversationId,
            _user: &silverhand_backend_core::foundation::identity::UserId,
        ) -> Result<bool, PersistenceFailure> {
            Ok(true)
        }

        async fn agent_participating_conversations(
            &mut self,
            _agent: &AgentId,
            conversations: &[ConversationId],
        ) -> Result<std::collections::HashSet<ConversationId>, PersistenceFailure> {
            Ok(conversations.iter().cloned().collect())
        }
    }

    struct Names;
    impl AgentDisplayNameReader for Names {
        async fn agent_display_name(
            &self,
            _a: &AgentId,
        ) -> Result<
            Option<
                silverhand_backend_core::conversation_messaging::messaging::task::AgentEventNames,
            >,
            PersistenceFailure,
        > {
            Ok(Some(silverhand_backend_core::conversation_messaging::messaging::task::AgentEventNames::new("alice", "Alice")))
        }
    }

    struct Write {
        outcome_created: bool,
        calls: Arc<AtomicUsize>,
    }
    impl TaskCreateWrite for Write {
        async fn create_tasks(
            &self,
            batch: &TaskCreateBatch,
        ) -> Result<TaskCreateOutcome, PersistenceFailure> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(if self.outcome_created {
                TaskCreateOutcome::Created(
                    batch.items()
                        .iter()
                        .enumerate()
                        .map(|(i, plan)| {
                            TaskCreatedWitness::new(TaskMutationWitness {
                                kind: silverhand_backend_core::conversation_messaging::messaging::task::TaskEventKind::Create,
                                task_id: TaskId::parse(format!("task-{}", i + 1)).unwrap(),
                                task_number: TaskNumber::new((i + 1) as i64).unwrap(),
                                event_message: silverhand_backend_core::conversation_messaging::messaging::task::TaskCommittedEventMessage::new(
                                    CreatedEventMessageId::parse(format!("msg-{}", i + 1)).unwrap(),
                                    batch.conversation_id().clone(), plan.thread_root_id.clone(),
                                    silverhand_backend_core::conversation_messaging::messaging::task::TaskEventMessage { content: "event".into(), metadata: "{}".into() }, i as i64 + 1, 1,
                                ),
                                conversation_id: batch.conversation_id().clone(),
                                board: batch.board().clone(),
                                thread_root_id: plan.thread_root_id.clone(),
                                title: plan.title.clone(),
                                status: TaskStatus::Todo,
                                assignee: plan.event_assignee.clone(),
                                closed_at: None,
                                source_message_id: plan.source_message_id.clone(),
                            }).unwrap()
                        })
                        .collect(),
                )
            } else {
                TaskCreateOutcome::SourceConflict {
                    existing_task: None,
                }
            })
        }
    }

    struct Aftermath {
        fired: Arc<AtomicUsize>,
    }
    impl TaskMutationAftermath for Aftermath {
        async fn fire(
            &self,
            _trigger: &crate::conversation_messaging::messaging::task::aftermath::TaskAftermathTrigger,
        ) -> crate::conversation_messaging::messaging::task::aftermath::AftermathOutcome {
            self.fired.fetch_add(1, Ordering::SeqCst);
            crate::conversation_messaging::messaging::task::aftermath::AftermathOutcome::Delivered
        }
    }

    struct Counts {
        resolver: Arc<AtomicUsize>,
        create_cap: Arc<AtomicUsize>,
        write_cap: Arc<AtomicUsize>,
        write: Arc<AtomicUsize>,
        aftermath: Arc<AtomicUsize>,
        allocation: Arc<Mutex<Option<TaskCreatePairAllocation>>>,
        intent: Arc<Mutex<Option<TaskCreateTargetIntent>>>,
    }

    struct Spec {
        agent_exists: bool,
        principal: RuntimePrincipal,
        refuse: Option<TaskTargetRefusal>,
        placement: ResolvedPlacement,
        create_cap: bool,
        write_cap: bool,
        created: bool,
        lifecycle: Option<TaskCreateConversationState>,
        participant: bool,
        source_mode: SourceMode,
    }
    impl Default for Spec {
        fn default() -> Self {
            Self {
                agent_exists: true,
                principal: agent_slot(),
                refuse: None,
                placement: ResolvedPlacement::Workspace {
                    workspace: WorkspaceId::parse("ws-1").unwrap(),
                },
                create_cap: true,
                write_cap: true,
                created: true,
                lifecycle: Some(TaskCreateConversationState::Open),
                participant: true,
                source_mode: SourceMode::Usable,
            }
        }
    }

    fn app(spec: Spec) -> (Router, Counts) {
        let counts = Counts {
            resolver: Arc::new(AtomicUsize::new(0)),
            create_cap: Arc::new(AtomicUsize::new(0)),
            write_cap: Arc::new(AtomicUsize::new(0)),
            write: Arc::new(AtomicUsize::new(0)),
            aftermath: Arc::new(AtomicUsize::new(0)),
            allocation: Arc::new(Mutex::new(None)),
            intent: Arc::new(Mutex::new(None)),
        };
        let state = TaskCreateState {
            resolver: Resolver {
                refuse: spec.refuse,
                placement: spec.placement,
                calls: Arc::clone(&counts.resolver),
                allocation: Arc::clone(&counts.allocation),
                intent: Arc::clone(&counts.intent),
            },
            door: Door {
                agent_exists: spec.agent_exists,
                create_cap: spec.create_cap,
                write_cap: spec.write_cap,
                create_calls: Arc::clone(&counts.create_cap),
                write_calls: Arc::clone(&counts.write_cap),
                lifecycle: spec.lifecycle,
                participant: spec.participant,
                source_mode: spec.source_mode,
            },
            write: Write {
                outcome_created: spec.created,
                calls: Arc::clone(&counts.write),
            },
            names: Names,
            aftermath: Aftermath {
                fired: Arc::clone(&counts.aftermath),
            },
        };
        let router = task_create_router(state).layer(Extension(spec.principal));
        (router, counts)
    }

    async fn call(app: Router, path_aid: &str, body: &str) -> (StatusCode, Value) {
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/internal/agents/{path_aid}/tasks/create"))
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_owned()))
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

    #[tokio::test]
    async fn identity_mismatch_is_401_zero_reach() {
        let (app, counts) = app(Spec::default());
        let (status, _b) = call(
            app,
            "agent-2",
            r##"{"tasks":[{"title":"x"}],"target":"#general"}"##,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(counts.resolver.load(Ordering::SeqCst), 0);
        assert_eq!(counts.write.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn ghost_actor_is_the_create_specific_200_error_after_target_resolution() {
        let (router, counts) = app(Spec {
            agent_exists: false,
            ..Spec::default()
        });
        let (status, body) = call(
            router,
            AGENT,
            r##"{"tasks":[{"title":"x"}],"target":"#general"}"##,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({"error":"agent_not_found"}));
        assert_eq!(counts.resolver.load(Ordering::SeqCst), 1);
        assert_eq!(counts.create_cap.load(Ordering::SeqCst), 1);
        assert_eq!(counts.write.load(Ordering::SeqCst), 0);
        assert_eq!(counts.aftermath.load(Ordering::SeqCst), 0);

        let (app, counts) = app(Spec {
            agent_exists: false,
            refuse: Some(TaskTargetRefusal::Unresolved),
            ..Spec::default()
        });
        let (status, body) = call(
            app,
            AGENT,
            r##"{"tasks":[{"title":"x"}],"target":"#nope"}"##,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["code"], "target_unresolved");
        assert_eq!(counts.create_cap.load(Ordering::SeqCst), 0);
        assert_eq!(counts.write.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn resolver_receives_typed_write_intent_and_scope_bound_allocation() {
        for (principal, allocation) in [
            (agent_slot(), TaskCreatePairAllocation::Permitted),
            (conversation_slot(CONV), TaskCreatePairAllocation::Permitted),
        ] {
            let (app, counts) = app(Spec {
                principal,
                ..Spec::default()
            });
            let (status, _) = call(
                app,
                AGENT,
                r##"{"tasks":[{"title":"a"}],"target":"#general"}"##,
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(*counts.allocation.lock().unwrap(), Some(allocation));
            assert_eq!(
                *counts.intent.lock().unwrap(),
                Some(TaskCreateTargetIntent::Write)
            );
        }
    }

    #[tokio::test]
    async fn empty_batch_is_422_before_resolve() {
        let (app, counts) = app(Spec::default());
        let (status, body) = call(app, AGENT, r##"{"tasks":[],"target":"#general"}"##).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["error"], "invalid input");
        assert_eq!(counts.resolver.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn all_three_optional_nonnullable_fields_accept_absence_but_reject_null_zero_reach() {
        let (router, counts) = app(Spec::default());
        let (status, _) = call(
            router,
            AGENT,
            r##"{"tasks":[{"title":"x"}],"target":"#general"}"##,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "claim/body/source all absent");
        assert_eq!(counts.write.load(Ordering::SeqCst), 1);

        for body in [
            r##"{"tasks":[{"title":"x"}],"target":"#general","claim":null}"##,
            r##"{"tasks":[{"title":"x","body":null}],"target":"#general"}"##,
            r##"{"tasks":[{"title":"x","source_message_id":null}],"target":"#general"}"##,
        ] {
            let (app, counts) = app(Spec::default());
            let (status, _value) = call(app, AGENT, body).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
            assert_eq!(counts.resolver.load(Ordering::SeqCst), 0, "{body}");
            assert_eq!(counts.create_cap.load(Ordering::SeqCst), 0, "{body}");
            assert_eq!(counts.write_cap.load(Ordering::SeqCst), 0, "{body}");
            assert_eq!(counts.write.load(Ordering::SeqCst), 0, "{body}");
            assert_eq!(counts.aftermath.load(Ordering::SeqCst), 0, "{body}");
        }
    }

    #[tokio::test]
    async fn all_target_refusals_preserve_the_canonical_wire_and_zero_reach() {
        for (refusal, expected_status, expected_body) in [
            (
                TaskTargetRefusal::Malformed,
                StatusCode::BAD_REQUEST,
                json!({"error":"target is not an address","code":"target_malformed"}),
            ),
            (
                TaskTargetRefusal::SelfPairNotAddressable,
                StatusCode::BAD_REQUEST,
                json!({"error":"a direct message needs another participant","code":"self_pair_not_addressable"}),
            ),
            (
                TaskTargetRefusal::Unresolved,
                StatusCode::NOT_FOUND,
                json!({"error":"target names no stream you can use","code":"target_unresolved"}),
            ),
            (
                TaskTargetRefusal::ThreadNotInTarget,
                StatusCode::NOT_FOUND,
                json!({"error":"target names no reply cluster of that conversation","code":"thread_not_in_target"}),
            ),
            (
                TaskTargetRefusal::SlotOutOfScope,
                StatusCode::FORBIDDEN,
                json!({"error":"this operation acts only in the conversation your session is anchored to","code":"slot_out_of_scope"}),
            ),
        ] {
            let (app, counts) = app(Spec {
                refuse: Some(refusal),
                ..Spec::default()
            });
            let (status, body) = call(
                app,
                AGENT,
                r##"{"tasks":[{"title":"x"}],"target":"#nope"}"##,
            )
            .await;
            assert_eq!(status, expected_status);
            assert_eq!(body, expected_body);
            assert_eq!(counts.resolver.load(Ordering::SeqCst), 1);
            assert_eq!(counts.create_cap.load(Ordering::SeqCst), 0);
            assert_eq!(counts.write_cap.load(Ordering::SeqCst), 0);
            assert_eq!(counts.write.load(Ordering::SeqCst), 0);
            assert_eq!(counts.aftermath.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    async fn missing_create_tasks_is_forbidden_zero_write() {
        let (app, counts) = app(Spec {
            create_cap: false,
            ..Spec::default()
        });
        let (status, body) = call(
            app,
            AGENT,
            r##"{"tasks":[{"title":"x"}],"target":"#general"}"##,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            body,
            json!({
                "code": "capability_denied",
                "capability": "create_tasks",
                "message": "missing 'create_tasks' capability in this workspace",
                "details": { "capability": "create_tasks" }
            })
        );
        assert_eq!(counts.create_cap.load(Ordering::SeqCst), 1);
        assert_eq!(counts.write.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn claim_needs_write_tasks_too() {
        let (app, counts) = app(Spec {
            write_cap: false,
            ..Spec::default()
        });
        let (status, body) = call(
            app,
            AGENT,
            r##"{"tasks":[{"title":"x"}],"target":"#general","claim":true}"##,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            body,
            json!({
                "code": "capability_denied",
                "capability": "write_tasks",
                "message": "missing 'write_tasks' capability in this workspace",
                "details": { "capability": "write_tasks" }
            })
        );
        assert_eq!(counts.create_cap.load(Ordering::SeqCst), 1);
        assert_eq!(counts.write_cap.load(Ordering::SeqCst), 1);
        assert_eq!(counts.write.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn cross_conversation_claim_is_refused_before_write() {
        // Actor runs in `origin`, files+claims into CONV — cross-conversation.
        let (app, counts) = app(Spec {
            principal: conversation_slot("origin-conv"),
            ..Spec::default()
        });
        let (status, body) = call(
            app,
            AGENT,
            r##"{"tasks":[{"title":"x"}],"target":"#general","claim":true}"##,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["error"], "cross_conversation_claim");
        assert_eq!(counts.write.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_workspace_create_writes_and_fires_aftermath() {
        let (app, counts) = app(Spec::default());
        let (status, body) = call(
            app,
            AGENT,
            r##"{"tasks":[{"title":"x"}],"target":"#general"}"##,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["tasks"][0]["id"], "task-1");
        assert_eq!(body["tasks"][0]["task_number"], 1);
        assert_eq!(counts.write.load(Ordering::SeqCst), 1);
        assert_eq!(
            counts.aftermath.load(Ordering::SeqCst),
            1,
            "aftermath fires once per committed create"
        );
    }

    #[tokio::test]
    async fn a_direct_pair_create_reaches_no_capability() {
        let (app, counts) = app(Spec {
            placement: ResolvedPlacement::DirectPair,
            ..Spec::default()
        });
        let (status, body) = call(
            app,
            AGENT,
            r#"{"tasks":[{"title":"x"}],"target":"dm:@bob"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["tasks"][0]["id"], "task-1");
        assert_eq!(
            counts.create_cap.load(Ordering::SeqCst),
            0,
            "a pair board has no capability axis"
        );
        assert_eq!(counts.write.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_two_item_batch_conflict_is_one_refusal_without_aftermath() {
        // The route hands off one batch and emits no aftermath on refusal.
        // Actual row/event rollback is owned by the SQLite create integration tests.
        let (app, counts) = app(Spec {
            created: false,
            ..Spec::default()
        });
        let (status, body) = call(
            app,
            AGENT,
            r##"{"tasks":[{"title":"a"},{"title":"b"}],"target":"#general"}"##,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["error"], "source_conflict");
        assert!(body.get("tasks").is_none());
        assert_eq!(
            counts.write.load(Ordering::SeqCst),
            1,
            "one batch write call"
        );
        assert_eq!(
            counts.aftermath.load(Ordering::SeqCst),
            0,
            "no aftermath on a refused batch"
        );
    }

    #[tokio::test]
    async fn closed_and_archived_destinations_are_typed_zero_write_refusals() {
        for (lifecycle, expected) in [
            (TaskCreateConversationState::Closed, "conversation_closed"),
            (
                TaskCreateConversationState::Archived,
                "conversation_archived",
            ),
        ] {
            let (app, counts) = app(Spec {
                lifecycle: Some(lifecycle),
                ..Spec::default()
            });
            let (status, body) = call(
                app,
                AGENT,
                r##"{"tasks":[{"title":"a"}],"target":"#general"}"##,
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body["error"], expected);
            assert_eq!(counts.write.load(Ordering::SeqCst), 0);
            assert_eq!(counts.aftermath.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    async fn destination_membership_is_reproved_before_the_batch_write() {
        let (app, counts) = app(Spec {
            participant: false,
            ..Spec::default()
        });
        let (status, body) = call(
            app,
            AGENT,
            r##"{"tasks":[{"title":"a"}],"target":"#general"}"##,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["error"], "not_conversation_member");
        assert_eq!(counts.write.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn source_existence_and_conversation_are_checked_before_write() {
        for (source_mode, expected) in [
            (SourceMode::Missing, "source_message_not_found"),
            (
                SourceMode::WrongConversation,
                "source_conversation_mismatch",
            ),
        ] {
            let (app, counts) = app(Spec {
                source_mode,
                ..Spec::default()
            });
            let (status, body) = call(
                app,
                AGENT,
                r##"{"tasks":[{"title":"a","source_message_id":"msg-1"}],"target":"#general"}"##,
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body["error"], expected);
            assert_eq!(counts.write.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    async fn preexisting_source_conflict_returns_the_winner_and_zero_write() {
        let (app, counts) = app(Spec {
            source_mode: SourceMode::Existing,
            ..Spec::default()
        });
        let (status, body) = call(
            app,
            AGENT,
            r##"{"tasks":[{"title":"a","source_message_id":"msg-1"}],"target":"#general"}"##,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["error"], "source_conflict");
        assert_eq!(body["existing_task"]["id"], "existing-task");
        assert_eq!(body["existing_task"]["task_number"], 7);
        assert_eq!(counts.write.load(Ordering::SeqCst), 0);
        assert_eq!(counts.aftermath.load(Ordering::SeqCst), 0);
    }
}
