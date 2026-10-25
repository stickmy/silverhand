//! The human (session-authenticated) task-WRITE faces that key off a canonical
//! `:taskId` path segment: `POST /api/tasks/:taskId/{claim,unclaim,status,edit}`.
//!
//! These are the human counterparts of the internal agent writes, but far thinner:
//! a human write has NO by-number arm, NO workspace selector, NO board resolver, and
//! NO capability axis — the acting user is the authenticated session principal and
//! the task is named by its canonical id, so each face is `session → taskId → the
//! core HUMAN write UoW → the typed envelope`. The core UoW owns participation,
//! direct-pair read-only, and the door-specific gate under its own write lock.
//!
//! Identity is the session, injected by a parent middleware as an `Extension<UserId>`
//! exactly as every other public-human route in this crate reads it; a missing
//! principal is a composition `500`. There is no `:aid`-style binding — the session
//! IS the actor, and a human cannot act "as" another user by rewriting the path.
//!
//! HTTP status parity with the TS routes: claim/unclaim/status answer `200` on
//! success and `409` on a typed refusal; edit answers `200`, `404` for
//! `task_not_found`, else `400`. The body is the typed `TaskMutationResult` either
//! way.

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
    AgentExistenceReader, CreatePlacement, HumanClaimOutcome, HumanStatusWriteOutcome,
    HumanTaskStatusWrite, ReadViewer, TaskClaimWrite, TaskContent, TaskConversationBoardRead,
    TaskCreateAdmissionFailure, TaskCreateAdmissionReads, TaskCreateBatch, TaskCreateOutcome,
    TaskCreatePlan, TaskCreateWrite, TaskCreatedWitness, TaskCreator, TaskEditWrite,
    TaskEditWriteOutcome, TaskEditor, TaskId, TaskMutationWitness, TaskParticipationReads,
    TaskStatus, TaskSystemActor, TaskTitle, UnclaimOutcome, admit_task_create,
};
use silverhand_backend_core::foundation::identity::{AgentId, ConversationId, UserId};
use silverhand_backend_core::foundation::persistence::PersistenceFailure;

use crate::conversation_messaging::messaging::task::aftermath::{
    TaskMutationAftermath, run_human_aftermath,
};
use crate::foundation::http_error::{plain_500 as composition_500, plain_500 as safe_500};
use crate::foundation::wire_admission::{ValidatedJson, ValidatedJsonRejection, invalid_input};
use crate::foundation::wire_patch::RequiredPatchField;

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

pub struct HumanStatusState<W, AF> {
    pub write: W,
    pub aftermath: AF,
}

#[derive(Debug, Deserialize, garde::Validate)]
pub(crate) struct HumanStatusBody {
    #[garde(skip)]
    status: String,
}

pub fn human_status_router<W, AF>(state: HumanStatusState<W, AF>) -> Router
where
    W: HumanTaskStatusWrite + Send + Sync + 'static,
    AF: TaskMutationAftermath,
{
    Router::new()
        .route("/api/tasks/{taskId}/status", post(status_route::<W, AF>))
        .with_state(Arc::new(state))
}

async fn status_route<W, AF>(
    State(state): State<Arc<HumanStatusState<W, AF>>>,
    Path(task_id): Path<String>,
    principal: Option<axum::extract::Extension<UserId>>,
    body: Result<ValidatedJson<HumanStatusBody>, ValidatedJsonRejection>,
) -> Response
where
    W: HumanTaskStatusWrite + Send + Sync + 'static,
    AF: TaskMutationAftermath,
{
    let Some(axum::extract::Extension(actor)) = principal else {
        return composition_500();
    };
    let body = match body {
        Ok(ValidatedJson(body)) => body,
        Err(rejection) => return rejection.into_response(),
    };
    let Ok(status) = TaskStatus::parse(&body.status) else {
        return invalid_input();
    };
    // A structurally-empty path id is not a task; the not-found envelope.
    let Ok(task_id) = TaskId::parse(task_id) else {
        return refusal(
            StatusCode::CONFLICT,
            "task_not_found",
            "task not found",
            json!({}),
        );
    };

    match state
        .write
        .apply_human_status(&task_id, &actor, status)
        .await
    {
        Ok(HumanStatusWriteOutcome::Applied(witness)) => {
            run_human_aftermath(&state.aftermath, *witness, &actor).await;
            ok(json!({ "success": true }))
        }
        Ok(HumanStatusWriteOutcome::NotFound) => refusal(
            StatusCode::CONFLICT,
            "task_not_found",
            "task not found",
            json!({}),
        ),
        Ok(HumanStatusWriteOutcome::ConversationReadonly) => refusal(
            StatusCode::CONFLICT,
            "conversation_readonly",
            "the conversation is read-only: a direct-pair endpoint is gone",
            json!({}),
        ),
        Ok(HumanStatusWriteOutcome::AlreadyAtStatus) => refusal(
            StatusCode::CONFLICT,
            "already_at_status",
            "task already has that status",
            json!({}),
        ),
        Err(_) => safe_500(),
    }
}

// ---------------------------------------------------------------------------
// claim (assign a chosen agent) / unclaim (clear)
// ---------------------------------------------------------------------------

pub struct HumanClaimState<W, AF, E> {
    pub write: W,
    pub aftermath: AF,
    /// Narrow existence hint used solely to preserve the TS claim refusal order.
    /// The writer still replays assignee participation under its reserved lock.
    pub assignees: E,
}

#[derive(Debug, Deserialize, garde::Validate)]
pub(crate) struct HumanClaimBody {
    #[garde(skip)]
    agent_id: String,
}

pub fn human_claim_router<W, AF, E>(state: HumanClaimState<W, AF, E>) -> Router
where
    W: TaskClaimWrite + Send + Sync + 'static,
    AF: TaskMutationAftermath,
    E: AgentExistenceReader + Send + Sync + 'static,
{
    Router::new()
        .route("/api/tasks/{taskId}/claim", post(claim_route::<W, AF, E>))
        .route(
            "/api/tasks/{taskId}/unclaim",
            post(unclaim_route::<W, AF, E>),
        )
        .with_state(Arc::new(state))
}

async fn claim_route<W, AF, E>(
    State(state): State<Arc<HumanClaimState<W, AF, E>>>,
    Path(task_id): Path<String>,
    principal: Option<axum::extract::Extension<UserId>>,
    body: Result<ValidatedJson<HumanClaimBody>, ValidatedJsonRejection>,
) -> Response
where
    W: TaskClaimWrite + Send + Sync + 'static,
    AF: TaskMutationAftermath,
    E: AgentExistenceReader + Send + Sync + 'static,
{
    let Some(axum::extract::Extension(actor)) = principal else {
        return composition_500();
    };
    let body = match body {
        Ok(ValidatedJson(body)) => body,
        Err(rejection) => return rejection.into_response(),
    };
    // `agent_id` is required and non-empty; an empty/invalid id is a semantic 422.
    let Ok(assignee) = AgentId::parse(body.agent_id) else {
        return invalid_input();
    };
    // Current TS resolves the chosen agent before looking up the task. Preserve
    // that observable precedence without treating this async hint as a write
    // credential: the SQLite UoW rechecks fresh participation after its task,
    // viewer, read-only, and terminal gates.
    match state.assignees.agent_exists(&assignee).await {
        Ok(true) => {}
        Ok(false) => {
            return refusal(
                StatusCode::CONFLICT,
                "assignee_not_conversation_member",
                "agent not found",
                json!({}),
            );
        }
        Err(_) => return safe_500(),
    }
    let Ok(task_id) = TaskId::parse(task_id) else {
        return refusal(
            StatusCode::CONFLICT,
            "task_not_found",
            "task not found",
            json!({}),
        );
    };

    // The assignee's display name is resolved by the write's snapshot join; the
    // door passes the id as the name fallback (a human claim UI supplies the id).
    match state.write.human_claim(&task_id, &actor, &assignee).await {
        Ok(HumanClaimOutcome::Claimed { status: _, witness }) => {
            run_human_aftermath(&state.aftermath, (*witness).clone(), &actor).await;
            // Wire parity: current TS returns {success, task_number, id}.
            let entry = json!({
                "success": true,
                "task_number": witness.task_number.get(),
                "id": witness.task_id.as_str(),
            });
            ok(entry)
        }
        Ok(HumanClaimOutcome::NotFound) => refusal(
            StatusCode::CONFLICT,
            "task_not_found",
            "task not found",
            json!({}),
        ),
        Ok(HumanClaimOutcome::ConversationReadonly) => refusal(
            StatusCode::CONFLICT,
            "conversation_readonly",
            "the conversation is read-only: a direct-pair endpoint is gone",
            json!({}),
        ),
        Ok(HumanClaimOutcome::TaskDone) => {
            refusal(StatusCode::CONFLICT, "task_done", "task is done", json!({}))
        }
        Ok(HumanClaimOutcome::TaskClosed) => refusal(
            StatusCode::CONFLICT,
            "task_closed",
            "task is closed",
            json!({}),
        ),
        Ok(HumanClaimOutcome::AlreadyClaimed) => refusal(
            StatusCode::CONFLICT,
            "already_claimed",
            "task is already assigned",
            json!({}),
        ),
        Ok(HumanClaimOutcome::AssigneeNotConversationMember) => refusal(
            StatusCode::CONFLICT,
            "assignee_not_conversation_member",
            "agent is not a member of this task's conversation",
            json!({}),
        ),
        Err(_) => safe_500(),
    }
}

async fn unclaim_route<W, AF, E>(
    State(state): State<Arc<HumanClaimState<W, AF, E>>>,
    Path(task_id): Path<String>,
    principal: Option<axum::extract::Extension<UserId>>,
) -> Response
where
    W: TaskClaimWrite + Send + Sync + 'static,
    AF: TaskMutationAftermath,
    E: AgentExistenceReader + Send + Sync + 'static,
{
    let Some(axum::extract::Extension(actor)) = principal else {
        return composition_500();
    };
    let Ok(task_id) = TaskId::parse(task_id) else {
        return refusal(
            StatusCode::CONFLICT,
            "task_not_found",
            "task not found",
            json!({}),
        );
    };
    // The human unclaim is ownership-agnostic (any participant may clear); the agent
    // door's NotAssignee arm never arises for the human UoW.
    match state.write.human_unclaim(&task_id, &actor).await {
        Ok(UnclaimOutcome::Unclaimed(witness)) => {
            run_human_aftermath(&state.aftermath, *witness, &actor).await;
            ok(json!({ "success": true }))
        }
        Ok(UnclaimOutcome::AgentNotFound) => safe_500(),
        Ok(UnclaimOutcome::NotFound) => refusal(
            StatusCode::CONFLICT,
            "task_not_found",
            "task not found",
            json!({}),
        ),
        Ok(UnclaimOutcome::ConversationReadonly) => refusal(
            StatusCode::CONFLICT,
            "conversation_readonly",
            "the conversation is read-only: a direct-pair endpoint is gone",
            json!({}),
        ),
        Ok(UnclaimOutcome::TaskDone) => {
            refusal(StatusCode::CONFLICT, "task_done", "task is done", json!({}))
        }
        Ok(UnclaimOutcome::TaskClosed) => refusal(
            StatusCode::CONFLICT,
            "task_closed",
            "task is closed",
            json!({}),
        ),
        // Ownership-agnostic: the human door never returns NotAssignee.
        Ok(UnclaimOutcome::NotAssignee) => safe_500(),
        Err(_) => safe_500(),
    }
}

// ---------------------------------------------------------------------------
// edit
// ---------------------------------------------------------------------------

pub struct HumanEditState<W, AF> {
    pub write: W,
    pub aftermath: AF,
}

#[derive(Debug, Deserialize, garde::Validate)]
pub(crate) struct HumanEditBody {
    #[garde(skip)]
    #[serde(default)]
    title: RequiredPatchField<String>,
    #[garde(skip)]
    #[serde(default)]
    body: RequiredPatchField<String>,
}

pub fn human_edit_router<W, AF>(state: HumanEditState<W, AF>) -> Router
where
    W: TaskEditWrite + Send + Sync + 'static,
    AF: TaskMutationAftermath,
{
    Router::new()
        .route("/api/tasks/{taskId}/edit", post(edit_route::<W, AF>))
        .with_state(Arc::new(state))
}

async fn edit_route<W, AF>(
    State(state): State<Arc<HumanEditState<W, AF>>>,
    Path(task_id): Path<String>,
    principal: Option<axum::extract::Extension<UserId>>,
    body: Result<ValidatedJson<HumanEditBody>, ValidatedJsonRejection>,
) -> Response
where
    W: TaskEditWrite + Send + Sync + 'static,
    AF: TaskMutationAftermath,
{
    let Some(axum::extract::Extension(actor)) = principal else {
        return composition_500();
    };
    let body = match body {
        Ok(ValidatedJson(body)) => body,
        Err(rejection) => return rejection.into_response(),
    };
    // At least one field, each parsed to its value authority (semantic 422).
    let raw_title = body.title.into_option();
    let raw_body = body.body.into_option();
    if raw_title.is_none() && raw_body.is_none() {
        return invalid_input();
    }
    let title = match raw_title {
        Some(raw) => match TaskTitle::parse(&raw) {
            Ok(t) => Some(t),
            Err(_) => return invalid_input(),
        },
        None => None,
    };
    let content = match raw_body {
        Some(raw) => match TaskContent::parse(raw) {
            Ok(c) => Some(c),
            Err(_) => return invalid_input(),
        },
        None => None,
    };
    // Edit answers 404 for a not-found (including a malformed id), else 400.
    let Ok(task_id) = TaskId::parse(task_id) else {
        return refusal(
            StatusCode::NOT_FOUND,
            "task_not_found",
            "task not found",
            json!({}),
        );
    };

    let editor = TaskEditor::Human(actor.clone());
    // A human actor's task-event pill name is the literal `user` (the TS
    // `taskSystemActorFor`), so no name lookup is needed on this face.
    match state
        .write
        .apply_edit(&task_id, &editor, None, title, content)
        .await
    {
        Ok(TaskEditWriteOutcome::Applied(success)) => {
            let Some(witness) = success.witness else {
                return safe_500();
            };
            run_human_aftermath(&state.aftermath, *witness, &actor).await;
            edit_success(success.detail, ReadViewer::User(actor.clone()))
        }
        Ok(TaskEditWriteOutcome::NoOp(success)) => {
            edit_success(success.detail, ReadViewer::User(actor.clone()))
        }
        Ok(TaskEditWriteOutcome::AgentNotFound) => safe_500(),
        Ok(TaskEditWriteOutcome::NotFound) => refusal(
            StatusCode::NOT_FOUND,
            "task_not_found",
            "task not found",
            json!({}),
        ),
        Ok(TaskEditWriteOutcome::ConversationReadonly) => refusal(
            StatusCode::BAD_REQUEST,
            "conversation_readonly",
            "the conversation is read-only: a direct-pair endpoint is gone",
            json!({}),
        ),
        Err(_) => safe_500(),
    }
}

fn edit_success(
    detail: silverhand_backend_core::conversation_messaging::messaging::task::VisibleTaskPoint,
    viewer: ReadViewer,
) -> Response {
    let Some(source) = detail.for_viewer(&viewer) else {
        return safe_500();
    };
    match super::wire::project_detail(source) {
        Ok(task) => ok(json!({ "success": true, "task": task })),
        Err(_) => safe_500(),
    }
}

// ---------------------------------------------------------------------------
// create (POST /api/tasks) — human, single, sourceless, unassigned
// ---------------------------------------------------------------------------

/// Whether the acting user may post to (and thus file a task into) a conversation —
/// the human create's whole authorization (`canPostToConversation`: membership +
/// view), the SAME primitive the agent create route's participation gate uses.
/// `false` = not a member (unknown ∪ non-member collapse to one answer).
pub trait HumanConversationPost: Send + Sync + 'static {
    fn can_post(
        &self,
        user: &UserId,
        conversation: &ConversationId,
    ) -> impl std::future::Future<Output = Result<bool, PersistenceFailure>> + Send;
}

/// The conversation's placement-derived board, for composing the create plan.
pub struct HumanCreateState<P, B, W, AF> {
    /// `canPostToConversation` — the human create's authorization.
    pub post: P,
    /// The conversation's board (workspace/pair), for the plan coordinate.
    pub boards: B,
    /// The core create write unit of work.
    pub write: W,
    /// Mandatory post-commit aftermath, fired once on a committed create.
    pub aftermath: AF,
}

#[derive(Debug, Deserialize, garde::Validate)]
pub(crate) struct HumanCreateBody {
    #[garde(skip)]
    title: String,
    #[garde(skip)]
    #[serde(default)]
    body: RequiredPatchField<String>,
    #[garde(skip)]
    conversation_id: String,
}

pub fn human_create_router<P, B, W, AF>(state: HumanCreateState<P, B, W, AF>) -> Router
where
    P: HumanConversationPost,
    B: TaskConversationBoardRead
        + TaskCreateAdmissionReads
        + TaskParticipationReads
        + Clone
        + Send
        + Sync
        + 'static,
    W: TaskCreateWrite + Send + Sync + 'static,
    AF: TaskMutationAftermath,
{
    Router::new()
        .route("/api/tasks", post(create_route::<P, B, W, AF>))
        .with_state(Arc::new(state))
}

async fn create_route<P, B, W, AF>(
    State(state): State<Arc<HumanCreateState<P, B, W, AF>>>,
    principal: Option<axum::extract::Extension<UserId>>,
    body: Result<ValidatedJson<HumanCreateBody>, ValidatedJsonRejection>,
) -> Response
where
    P: HumanConversationPost,
    B: TaskConversationBoardRead
        + TaskCreateAdmissionReads
        + TaskParticipationReads
        + Clone
        + Send
        + Sync
        + 'static,
    W: TaskCreateWrite + Send + Sync + 'static,
    AF: TaskMutationAftermath,
{
    let Some(axum::extract::Extension(actor)) = principal else {
        return composition_500();
    };
    let body = match body {
        Ok(ValidatedJson(body)) => body,
        Err(rejection) => return rejection.into_response(),
    };
    let Ok(title) = TaskTitle::parse(&body.title) else {
        return invalid_input();
    };
    let content = match body.body.into_option() {
        Some(raw) => match TaskContent::parse(raw) {
            Ok(c) => c,
            Err(_) => return invalid_input(),
        },
        None => TaskContent::empty(),
    };
    let Ok(conversation) = ConversationId::parse(body.conversation_id) else {
        return invalid_input();
    };

    // Fail-closed authz — the same membership/view primitive the agent create uses.
    // A non-member (incl. an unknown conversation) is rejected before the write,
    // with the SAME discriminated {success:false, code} the service failures use.
    match state.post.can_post(&actor, &conversation).await {
        Ok(true) => {}
        Ok(false) => {
            return code_only_refusal(StatusCode::FORBIDDEN, "not_conversation_member");
        }
        Err(_) => return safe_500(),
    }

    // Defense-in-depth behind the public post gate: the same create aggregate
    // authority used by the agent face distinguishes member-visible closed/archived
    // targets and re-proves participation before the write transaction.
    let creator = TaskCreator::Human(actor.clone());
    let admission_reads = state.boards.clone();
    let mut participation = state.boards.clone();
    match admit_task_create(
        &creator,
        &conversation,
        &[],
        &admission_reads,
        &mut participation,
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(failure)) => return human_create_admission_refusal(failure),
        Err(_) => return safe_500(),
    }

    // The plan coordinate from the conversation's placement. A human create is always
    // Filed (no origin frame, no self-claim), creator = the acting user.
    let board = match state.boards.conversation_board(&conversation).await {
        Ok(Some(board)) => board,
        // No board = the conversation vanished between the post-check and here.
        Ok(None) => {
            return code_only_refusal(StatusCode::NOT_FOUND, "conversation_not_found");
        }
        Err(_) => return safe_500(),
    };

    let plan = TaskCreatePlan {
        thread_root_id: None,
        title,
        content,
        claim_by: None,
        source_message_id: None,
        event_actor: TaskSystemActor::User {
            id: actor.clone(),
            name: "user".to_owned(),
        },
        event_assignee: None,
        event_placement: CreatePlacement::Filed,
    };
    let batch = TaskCreateBatch::new(conversation.clone(), board, creator, vec![plan]);
    // A human create is a singleton batch (one task).
    match state.write.create_tasks(&batch).await {
        Ok(TaskCreateOutcome::Created(witnesses)) => {
            let Some(witness) = witnesses.first() else {
                return safe_500();
            };
            run_human_aftermath(&state.aftermath, create_mutation_witness(witness), &actor).await;
            // The human create returns 201 with the identity the txn produced —
            // {id, task_number, title} (the panel opens the modal by id + reads
            // detail separately).
            (
                StatusCode::CREATED,
                Json(json!({
                    "success": true,
                    "task": {
                        "id": witness.task_id().as_str(),
                        "task_number": witness.task_number().get(),
                        "title": witness.title().as_str(),
                    },
                })),
            )
                .into_response()
        }
        Ok(TaskCreateOutcome::AgentNotFound) => safe_500(),
        Ok(TaskCreateOutcome::ConversationReadonly) => refusal(
            StatusCode::CONFLICT,
            "conversation_readonly",
            "the conversation is read-only: a direct-pair endpoint is gone",
            json!({}),
        ),
        // A sourceless human create cannot source-conflict; treat as a safe-500.
        Ok(TaskCreateOutcome::SourceConflict { .. })
        | Ok(TaskCreateOutcome::SourceMessageNotFound)
        | Ok(TaskCreateOutcome::SourceConversationMismatch)
        | Ok(TaskCreateOutcome::SourceNotAccessible)
        | Ok(TaskCreateOutcome::CapabilityDenied(_)) => safe_500(),
        Ok(TaskCreateOutcome::ConversationNotFound) => {
            human_create_admission_refusal(TaskCreateAdmissionFailure::ConversationNotFound)
        }
        Ok(TaskCreateOutcome::ConversationClosed) => {
            human_create_admission_refusal(TaskCreateAdmissionFailure::ConversationClosed)
        }
        Ok(TaskCreateOutcome::ConversationArchived) => {
            human_create_admission_refusal(TaskCreateAdmissionFailure::ConversationArchived)
        }
        Ok(TaskCreateOutcome::NotConversationMember) => {
            human_create_admission_refusal(TaskCreateAdmissionFailure::NotConversationMember)
        }
        Err(_) => safe_500(),
    }
}

fn human_create_admission_refusal(failure: TaskCreateAdmissionFailure) -> Response {
    match failure {
        TaskCreateAdmissionFailure::ConversationNotFound => {
            code_only_refusal(StatusCode::NOT_FOUND, "conversation_not_found")
        }
        TaskCreateAdmissionFailure::ConversationClosed => {
            code_only_refusal(StatusCode::CONFLICT, "conversation_closed")
        }
        TaskCreateAdmissionFailure::ConversationArchived => {
            code_only_refusal(StatusCode::CONFLICT, "conversation_archived")
        }
        TaskCreateAdmissionFailure::AgentNotFound => safe_500(),
        TaskCreateAdmissionFailure::NotConversationMember => {
            code_only_refusal(StatusCode::FORBIDDEN, "not_conversation_member")
        }
        // Human create is sourceless. Reaching a source-only refusal means the
        // component wiring violated its own plan contract.
        TaskCreateAdmissionFailure::SourceMessageNotFound
        | TaskCreateAdmissionFailure::SourceConversationMismatch
        | TaskCreateAdmissionFailure::SourceNotAccessible
        | TaskCreateAdmissionFailure::SourceConflict { .. } => safe_500(),
    }
}

/// Project the create witness onto the shared mutation witness the aftermath takes.
fn create_mutation_witness(witness: &TaskCreatedWitness) -> TaskMutationWitness {
    witness.mutation().clone()
}

// ---------------------------------------------------------------------------
// shared response helpers
// ---------------------------------------------------------------------------

fn ok(body: Value) -> Response {
    (StatusCode::OK, Json(body)).into_response()
}

fn code_only_refusal(status: StatusCode, code: &'static str) -> Response {
    (status, Json(json!({ "success": false, "code": code }))).into_response()
}

fn refusal(status: StatusCode, code: &str, reason: &str, extra: Value) -> Response {
    let mut body = json!({ "success": false, "code": code, "reason": reason });
    if let (Some(map), Value::Object(extra)) = (body.as_object_mut(), extra) {
        map.extend(extra);
    }
    (status, Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation_messaging::messaging::task::aftermath::{
        AftermathFailure, AftermathOutcome,
    };
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use silverhand_backend_core::conversation_messaging::messaging::task::AgentClaimOutcome;
    use silverhand_backend_core::conversation_messaging::messaging::task::{
        BoardKey, CreatedEventMessageId, TaskNumber,
    };
    use silverhand_backend_core::foundation::identity::ConversationId;

    use silverhand_backend_core::conversation_messaging::messaging::task::TaskMutationWitness;

    fn witness() -> TaskMutationWitness {
        TaskMutationWitness {
            kind: silverhand_backend_core::conversation_messaging::messaging::task::TaskEventKind::Edit,
            task_id: TaskId::parse("task-1").unwrap(),
            task_number: TaskNumber::new(7).unwrap(),
            event_message: silverhand_backend_core::conversation_messaging::messaging::task::TaskCommittedEventMessage::new(
                CreatedEventMessageId::parse("msg-1").unwrap(),
                ConversationId::parse("conv-1").unwrap(), None,
                silverhand_backend_core::conversation_messaging::messaging::task::TaskEventMessage { content: "event".into(), metadata: "{}".into() }, 1, 1,
            ),
            conversation_id: ConversationId::parse("conv-1").unwrap(),
            board: BoardKey::Workspace(silverhand_backend_core::foundation::identity::WorkspaceId::parse("ws-1").unwrap()),
            thread_root_id: None,
            title: TaskTitle::parse("title").unwrap(),
            status: TaskStatus::Todo,
            assignee: None,
            closed_at: None,
            source_message_id: None,
        }
    }
    fn committed_detail()
    -> silverhand_backend_core::conversation_messaging::messaging::task::VisibleTaskPoint {
        use silverhand_backend_core::conversation_messaging::messaging::task::{
            TaskAggregate, TaskDisplaySidecars, TaskPointCandidate, TaskRow, VisibleTaskPoint,
        };
        let task = TaskAggregate::decode(TaskRow {
            id: "task-1".into(),
            task_number: 7,
            conversation_id: "conv-1".into(),
            thread_root_id: None,
            board_key: "ws:ws-1".into(),
            workspace_id: Some("ws-1".into()),
            title: "title".into(),
            content: "body".into(),
            status: "todo".into(),
            assignee_agent_id: None,
            assignee_user_id: None,
            creator_kind: "system".into(),
            creator_agent_id: None,
            creator_user_id: None,
            source_message_id: None,
            created_event_message_id: None,
            last_editor_kind: None,
            last_editor_id: None,
            last_edited_at: None,
            created_at: 1,
            closed_at: None,
        })
        .unwrap();
        let candidate = TaskPointCandidate::new(
            task,
            TaskDisplaySidecars {
                channel_name: Some("General".into()),
                assignee_agent_name: None,
                assignee_user_name: None,
                creator_agent_name: None,
                creator_user_name: None,
            },
            None,
            None,
            None,
        );
        VisibleTaskPoint::from_committed_edit(ReadViewer::User(user()), candidate).unwrap()
    }
    fn applied_witness() -> TaskEditWriteOutcome {
        TaskEditWriteOutcome::Applied(
            silverhand_backend_core::conversation_messaging::messaging::task::TaskEditSuccess {
                detail: committed_detail(),
                witness: Some(Box::new(witness())),
            },
        )
    }

    /// A recording aftermath counting committed witnesses.
    struct RecordingAftermath {
        fired: Arc<AtomicUsize>,
    }
    impl TaskMutationAftermath for RecordingAftermath {
        async fn fire(
            &self,
            _trigger: &crate::conversation_messaging::messaging::task::aftermath::TaskAftermathTrigger,
        ) -> AftermathOutcome {
            self.fired.fetch_add(1, Ordering::SeqCst);
            AftermathOutcome::Delivered
        }
    }
    fn aftermath() -> RecordingAftermath {
        RecordingAftermath {
            fired: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// A FAILING aftermath — the positive control that a real sink failure does NOT
    /// rewrite the committed success. It records the fire (so the test sees it ran)
    /// and returns `Failed`; the face must still answer committed success and the
    /// `run_aftermath` helper records the failure (a `tracing::error!`) rather than
    /// dropping it silently.
    struct FailingAftermath {
        fired: Arc<AtomicUsize>,
    }
    impl TaskMutationAftermath for FailingAftermath {
        async fn fire(
            &self,
            _trigger: &crate::conversation_messaging::messaging::task::aftermath::TaskAftermathTrigger,
        ) -> AftermathOutcome {
            self.fired.fetch_add(1, Ordering::SeqCst);
            AftermathOutcome::Failed(AftermathFailure {
                reason: "scope-event sink unavailable".to_owned(),
            })
        }
    }
    use silverhand_backend_core::foundation::persistence::PersistenceFailure;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;

    // A `MakeWriter` log capture (the repo's standard shape) so the failure-audit test
    // asserts the `tracing::error!` in `run_aftermath` was actually emitted — not just
    // that the fake fired.
    #[derive(Clone, Default)]
    struct LogBuffer(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    struct LogWriter(LogBuffer);
    impl std::io::Write for LogWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuffer {
        type Writer = LogWriter;
        fn make_writer(&'a self) -> Self::Writer {
            LogWriter(self.clone())
        }
    }
    impl LogBuffer {
        fn events(&self) -> Vec<serde_json::Value> {
            String::from_utf8(self.0.lock().unwrap().clone())
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        }
    }

    fn user() -> UserId {
        UserId::parse("user-1").unwrap()
    }

    #[derive(Clone, Copy)]
    struct ExistingAssignee;
    impl AgentExistenceReader for ExistingAssignee {
        async fn agent_exists(&self, _agent: &AgentId) -> Result<bool, PersistenceFailure> {
            Ok(true)
        }
    }

    #[derive(Clone, Copy)]
    struct MissingAssignee;
    impl AgentExistenceReader for MissingAssignee {
        async fn agent_exists(&self, _agent: &AgentId) -> Result<bool, PersistenceFailure> {
            Ok(false)
        }
    }

    // A write fake that answers a fixed outcome per verb and counts the reach.
    #[derive(Clone)]
    struct FakeWrite {
        status: HumanStatusWriteOutcome,
        claim: HumanClaimOutcome,
        unclaim: UnclaimOutcome,
        edit: TaskEditWriteOutcome,
        calls: Arc<AtomicUsize>,
    }
    impl HumanTaskStatusWrite for FakeWrite {
        async fn apply_human_status(
            &self,
            _id: &TaskId,
            _actor: &UserId,
            _target: TaskStatus,
        ) -> Result<HumanStatusWriteOutcome, PersistenceFailure> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.status.clone())
        }
    }
    impl TaskClaimWrite for FakeWrite {
        async fn agent_claim(
            &self,
            _id: &TaskId,
            _actor: &AgentId,
            _actor_name: &silverhand_backend_core::conversation_messaging::messaging::task::AgentEventNames,
        ) -> Result<AgentClaimOutcome, PersistenceFailure> {
            unreachable!("human face never agent-claims")
        }
        async fn human_claim(
            &self,
            _id: &TaskId,
            _actor: &UserId,
            _assignee: &AgentId,
        ) -> Result<HumanClaimOutcome, PersistenceFailure> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.claim.clone())
        }
        async fn agent_unclaim(
            &self,
            _id: &TaskId,
            _actor: &AgentId,
        ) -> Result<UnclaimOutcome, PersistenceFailure> {
            unreachable!("human face never agent-unclaims")
        }
        async fn human_unclaim(
            &self,
            _id: &TaskId,
            _actor: &UserId,
        ) -> Result<UnclaimOutcome, PersistenceFailure> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.unclaim.clone())
        }
    }
    impl TaskEditWrite for FakeWrite {
        async fn apply_edit(
            &self,
            _id: &TaskId,
            _editor: &TaskEditor,
            _editor_name: Option<
                &silverhand_backend_core::conversation_messaging::messaging::task::AgentEventNames,
            >,
            _title: Option<TaskTitle>,
            _content: Option<TaskContent>,
        ) -> Result<TaskEditWriteOutcome, PersistenceFailure> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.edit.clone())
        }
    }

    fn fake(calls: &Arc<AtomicUsize>) -> FakeWrite {
        FakeWrite {
            status: HumanStatusWriteOutcome::Applied(Box::new(witness())),
            claim: HumanClaimOutcome::Claimed {
                status: TaskStatus::InProgress,
                witness: Box::new(witness()),
            },
            unclaim: UnclaimOutcome::Unclaimed(Box::new(witness())),
            edit: applied_witness(),
            calls: Arc::clone(calls),
        }
    }

    async fn send(
        router: Router,
        uri: &str,
        body: Option<&str>,
        with_user: bool,
    ) -> (StatusCode, Value) {
        let mut builder = Request::builder().method("POST").uri(uri);
        if let Some(_b) = body {
            builder = builder.header("content-type", "application/json");
        }
        let mut request = builder
            .body(Body::from(body.unwrap_or("").to_owned()))
            .unwrap();
        if with_user {
            request.extensions_mut().insert(user());
        }
        let response = router.oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: Value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, value)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_failing_aftermath_is_audited_and_does_not_rewrite_the_committed_success() {
        // POSITIVE CONTROL for the "aftermath failure never fakes a rollback + is
        // recorded, not swallowed" contract, driven by a REAL failing aftermath:
        //  * the route's write fake returns Applied(witness) — i.e. THIS ROUTE was
        //    handed a committed witness (real durable row+message persistence is the
        //    SQLite integration gate's job, not this fake's call count);
        //  * the aftermath then returns `Failed`, and the face must STILL answer the
        //    committed HTTP 200 {success:true};
        //  * `run_aftermath`'s `tracing::error!` MUST fire with the witness' task_id +
        //    the failure reason. We capture the log via a MakeWriter and assert the
        //    exact event — so deleting that `tracing::error!` turns this test RED.
        let logs = LogBuffer::default();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .without_time()
            .with_target(false)
            .with_writer(logs.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let write_calls = Arc::new(AtomicUsize::new(0));
        let fired = Arc::new(AtomicUsize::new(0));
        let router = human_status_router(HumanStatusState {
            write: fake(&write_calls),
            aftermath: FailingAftermath {
                fired: Arc::clone(&fired),
            },
        });
        let (status, body) = send(
            router,
            "/api/tasks/task-1/status",
            Some(r#"{"status":"done"}"#),
            true,
        )
        .await;
        // Committed success is preserved despite the aftermath failure.
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["success"], true);
        // This ROUTE was handed a committed witness (the write fake returned Applied);
        // real row+message durability is proven by the SQLite integration gates, not
        // by this fake's call count.
        assert_eq!(
            write_calls.load(Ordering::SeqCst),
            1,
            "the route received a committed witness from its write UoW"
        );
        // The aftermath fired exactly once and returned Failed (a real failure).
        assert_eq!(fired.load(Ordering::SeqCst), 1, "aftermath fired once");
        // AND the failure was AUDITED: `run_aftermath` emitted the structured error
        // carrying the witness' task_id + reason. This is the observable audit
        // boundary — deleting the `tracing::error!` fails this assertion.
        let audited = logs.events();
        assert!(
            audited.iter().any(|event| {
                event["fields"]["task_id"] == "task-1"
                    && event["fields"]["reason"] == "scope-event sink unavailable"
                    && event["level"] == "ERROR"
            }),
            "the Failed aftermath must be recorded with task_id + reason, got: {audited:?}"
        );
    }

    #[tokio::test]
    async fn status_already_at_is_409() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut f = fake(&calls);
        f.status = HumanStatusWriteOutcome::AlreadyAtStatus;
        let router = human_status_router(HumanStatusState {
            write: f,
            aftermath: aftermath(),
        });
        let (status, body) = send(
            router,
            "/api/tasks/task-1/status",
            Some(r#"{"status":"done"}"#),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["code"], "already_at_status");
    }

    #[tokio::test]
    async fn status_bad_token_is_422() {
        let calls = Arc::new(AtomicUsize::new(0));
        let router = human_status_router(HumanStatusState {
            write: fake(&calls),
            aftermath: aftermath(),
        });
        let (status, _b) = send(
            router,
            "/api/tasks/task-1/status",
            Some(r#"{"status":"nope"}"#),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn status_without_session_is_500_zero_reach() {
        let calls = Arc::new(AtomicUsize::new(0));
        let router = human_status_router(HumanStatusState {
            write: fake(&calls),
            aftermath: aftermath(),
        });
        let (status, _b) = send(
            router,
            "/api/tasks/task-1/status",
            Some(r#"{"status":"done"}"#),
            false,
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "write zero-reach without a session"
        );
    }

    #[tokio::test]
    async fn claim_applied_is_200_without_status() {
        let calls = Arc::new(AtomicUsize::new(0));
        let router = human_claim_router(HumanClaimState {
            write: fake(&calls),
            aftermath: aftermath(),
            assignees: ExistingAssignee,
        });
        let (status, body) = send(
            router,
            "/api/tasks/task-1/claim",
            Some(r#"{"agent_id":"agent-9"}"#),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["success"], true);
        assert_eq!(body["id"], "task-1");
        assert_eq!(body["task_number"], 7);
        assert!(body.get("status").is_none());
    }

    #[tokio::test]
    async fn claim_empty_agent_id_is_422() {
        let calls = Arc::new(AtomicUsize::new(0));
        let router = human_claim_router(HumanClaimState {
            write: fake(&calls),
            aftermath: aftermath(),
            assignees: ExistingAssignee,
        });
        let (status, _b) = send(
            router,
            "/api/tasks/task-1/claim",
            Some(r#"{"agent_id":""}"#),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn missing_assignee_precedes_unknown_or_terminal_task_and_never_reaches_writer() {
        for (task_id, outcome) in [
            ("unknown-task", HumanClaimOutcome::NotFound),
            ("task-1", HumanClaimOutcome::TaskDone),
        ] {
            let calls = Arc::new(AtomicUsize::new(0));
            let mut write = fake(&calls);
            write.claim = outcome;
            let router = human_claim_router(HumanClaimState {
                write,
                aftermath: aftermath(),
                assignees: MissingAssignee,
            });
            let (status, body) = send(
                router,
                &format!("/api/tasks/{task_id}/claim"),
                Some(r#"{"agent_id":"ghost-agent"}"#),
                true,
            )
            .await;
            assert_eq!(status, StatusCode::CONFLICT);
            assert_eq!(
                body,
                json!({
                    "success": false,
                    "code": "assignee_not_conversation_member",
                    "reason": "agent not found",
                })
            );
            assert_eq!(calls.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    async fn claim_already_claimed_is_409() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut f = fake(&calls);
        f.claim = HumanClaimOutcome::AlreadyClaimed;
        let router = human_claim_router(HumanClaimState {
            write: f,
            aftermath: aftermath(),
            assignees: ExistingAssignee,
        });
        let (status, body) = send(
            router,
            "/api/tasks/task-1/claim",
            Some(r#"{"agent_id":"agent-9"}"#),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["code"], "already_claimed");
    }

    #[tokio::test]
    async fn claim_assignee_not_member_is_409() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut f = fake(&calls);
        f.claim = HumanClaimOutcome::AssigneeNotConversationMember;
        let router = human_claim_router(HumanClaimState {
            write: f,
            aftermath: aftermath(),
            assignees: ExistingAssignee,
        });
        let (status, body) = send(
            router,
            "/api/tasks/task-1/claim",
            Some(r#"{"agent_id":"agent-9"}"#),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(
            body,
            json!({
                "success": false,
                "code": "assignee_not_conversation_member",
                "reason": "agent is not a member of this task's conversation",
            })
        );
    }

    #[tokio::test]
    async fn claim_task_done_has_the_exact_ts_reason() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut f = fake(&calls);
        f.claim = HumanClaimOutcome::TaskDone;
        let router = human_claim_router(HumanClaimState {
            write: f,
            aftermath: aftermath(),
            assignees: ExistingAssignee,
        });
        let (status, body) = send(
            router,
            "/api/tasks/task-1/claim",
            Some(r#"{"agent_id":"agent-9"}"#),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(
            body,
            json!({
                "success": false,
                "code": "task_done",
                "reason": "task is done",
            })
        );
    }

    #[tokio::test]
    async fn unclaim_is_200() {
        let calls = Arc::new(AtomicUsize::new(0));
        let router = human_claim_router(HumanClaimState {
            write: fake(&calls),
            aftermath: aftermath(),
            assignees: ExistingAssignee,
        });
        let (status, body) = send(router, "/api/tasks/task-1/unclaim", None, true).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["success"], true);
    }

    #[tokio::test]
    async fn unclaim_task_done_is_409() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut f = fake(&calls);
        f.unclaim = UnclaimOutcome::TaskDone;
        let router = human_claim_router(HumanClaimState {
            write: f,
            aftermath: aftermath(),
            assignees: ExistingAssignee,
        });
        let (status, body) = send(router, "/api/tasks/task-1/unclaim", None, true).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(
            body,
            json!({
                "success": false,
                "code": "task_done",
                "reason": "task is done",
            })
        );
    }

    #[tokio::test]
    async fn edit_applied_is_200() {
        let calls = Arc::new(AtomicUsize::new(0));
        let router = human_edit_router(HumanEditState {
            write: fake(&calls),
            aftermath: aftermath(),
        });
        let (status, body) = send(
            router,
            "/api/tasks/task-1/edit",
            Some(r#"{"title":"x"}"#),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["success"], true);
        assert_eq!(body["task"]["content"], "body");
    }

    #[tokio::test]
    async fn edit_not_found_is_404() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut f = fake(&calls);
        f.edit = TaskEditWriteOutcome::NotFound;
        let router = human_edit_router(HumanEditState {
            write: f,
            aftermath: aftermath(),
        });
        let (status, body) = send(
            router,
            "/api/tasks/task-1/edit",
            Some(r#"{"title":"x"}"#),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["code"], "task_not_found");
    }

    #[tokio::test]
    async fn edit_neither_field_is_422() {
        let calls = Arc::new(AtomicUsize::new(0));
        let router = human_edit_router(HumanEditState {
            write: fake(&calls),
            aftermath: aftermath(),
        });
        let (status, _b) = send(router, "/api/tasks/task-1/edit", Some(r#"{}"#), true).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn edit_optional_nonnullable_fields_reject_null_before_the_writer() {
        for body in [
            r#"{"title":null,"body":"x"}"#,
            r#"{"title":"x","body":null}"#,
        ] {
            let calls = Arc::new(AtomicUsize::new(0));
            let router = human_edit_router(HumanEditState {
                write: fake(&calls),
                aftermath: aftermath(),
            });
            let (status, _value) = send(router, "/api/tasks/task-1/edit", Some(body), true).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
            assert_eq!(calls.load(Ordering::SeqCst), 0, "writer zero-reach");
        }
    }

    #[tokio::test]
    async fn edit_readonly_is_400() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut f = fake(&calls);
        f.edit = TaskEditWriteOutcome::ConversationReadonly;
        let router = human_edit_router(HumanEditState {
            write: f,
            aftermath: aftermath(),
        });
        let (status, body) = send(
            router,
            "/api/tasks/task-1/edit",
            Some(r#"{"title":"x"}"#),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "conversation_readonly");
    }

    // ── human create (POST /api/tasks) ─────────────────────────────────────
    use silverhand_backend_core::conversation_messaging::messaging::task::{
        BoardKey as CBoardKey, PairKey, TaskCreateBatch, TaskCreateOutcome, TaskCreateWrite,
        TaskCreatedWitness, TaskNumber as CTaskNumber, TaskStatus as CTaskStatus,
    };
    use silverhand_backend_core::foundation::identity::WorkspaceId;
    use silverhand_backend_core::foundation::persistence::PersistenceFailure as CPersistenceFailure;

    struct Post {
        allow: bool,
        calls: Arc<AtomicUsize>,
    }
    impl HumanConversationPost for Post {
        async fn can_post(
            &self,
            _u: &UserId,
            _c: &ConversationId,
        ) -> Result<bool, CPersistenceFailure> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.allow)
        }
    }
    #[derive(Clone)]
    struct Boards {
        board: Option<CBoardKey>,
        lifecycle: silverhand_backend_core::conversation_messaging::messaging::task::TaskCreateConversationState,
    }
    impl TaskConversationBoardRead for Boards {
        async fn conversation_board(
            &self,
            _c: &ConversationId,
        ) -> Result<Option<CBoardKey>, CPersistenceFailure> {
            Ok(self.board.clone())
        }
    }
    impl TaskCreateAdmissionReads for Boards {
        async fn agent_exists(&self, _agent: &AgentId) -> Result<bool, CPersistenceFailure> {
            Ok(true)
        }

        async fn conversation_state(
            &self,
            _c: &ConversationId,
        ) -> Result<Option<silverhand_backend_core::conversation_messaging::messaging::task::TaskCreateConversationState>, CPersistenceFailure>{
            Ok(Some(self.lifecycle))
        }

        async fn source_message(
            &self,
            _source: &silverhand_backend_core::conversation_messaging::messaging::task::SourceMessageId,
        ) -> Result<Option<silverhand_backend_core::conversation_messaging::messaging::task::TaskCreateSourceRef>, CPersistenceFailure>{
            Ok(None)
        }

        async fn task_for_source(
            &self,
            _source: &silverhand_backend_core::conversation_messaging::messaging::task::SourceMessageId,
        ) -> Result<Option<silverhand_backend_core::conversation_messaging::messaging::task::ExistingSourceTask>, CPersistenceFailure>{
            Ok(None)
        }
    }
    impl TaskParticipationReads for Boards {
        async fn is_agent_participant(
            &mut self,
            _c: &ConversationId,
            _a: &AgentId,
        ) -> Result<bool, CPersistenceFailure> {
            Ok(true)
        }

        async fn is_user_participant(
            &mut self,
            _c: &ConversationId,
            _u: &UserId,
        ) -> Result<bool, CPersistenceFailure> {
            Ok(true)
        }

        async fn agent_participating_conversations(
            &mut self,
            _a: &AgentId,
            conversations: &[ConversationId],
        ) -> Result<std::collections::HashSet<ConversationId>, CPersistenceFailure> {
            Ok(conversations.iter().cloned().collect())
        }
    }
    struct CreateWrite {
        created: bool,
        calls: Arc<AtomicUsize>,
    }
    impl TaskCreateWrite for CreateWrite {
        async fn create_tasks(
            &self,
            batch: &TaskCreateBatch,
        ) -> Result<TaskCreateOutcome, CPersistenceFailure> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(if self.created {
                TaskCreateOutcome::Created(
                    batch.items()
                        .iter()
                        .map(|plan| {
                            TaskCreatedWitness::new(TaskMutationWitness {
                                kind: silverhand_backend_core::conversation_messaging::messaging::task::TaskEventKind::Create,
                                task_id: TaskId::parse("task-1").unwrap(),
                                task_number: CTaskNumber::new(1).unwrap(),
                                event_message: silverhand_backend_core::conversation_messaging::messaging::task::TaskCommittedEventMessage::new(
                                    CreatedEventMessageId::parse("msg-1").unwrap(),
                                    batch.conversation_id().clone(), plan.thread_root_id.clone(),
                                    silverhand_backend_core::conversation_messaging::messaging::task::TaskEventMessage { content: "event".into(), metadata: "{}".into() }, 1, 1,
                                ),
                                conversation_id: batch.conversation_id().clone(),
                                board: batch.board().clone(),
                                thread_root_id: plan.thread_root_id.clone(),
                                title: plan.title.clone(),
                                status: CTaskStatus::Todo,
                                assignee: plan.event_assignee.clone(),
                                closed_at: None,
                                source_message_id: plan.source_message_id.clone(),
                            }).unwrap()
                        })
                        .collect(),
                )
            } else {
                TaskCreateOutcome::ConversationReadonly
            })
        }
    }

    fn ws_board() -> CBoardKey {
        CBoardKey::Workspace(WorkspaceId::parse("ws-1").unwrap())
    }

    fn create_app(
        allow: bool,
        board: Option<CBoardKey>,
        created: bool,
    ) -> (Router, Arc<AtomicUsize>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        create_app_with_lifecycle(
            allow,
            board,
            created,
            silverhand_backend_core::conversation_messaging::messaging::task::TaskCreateConversationState::Open,
        )
    }

    fn create_app_with_lifecycle(
        allow: bool,
        board: Option<CBoardKey>,
        created: bool,
        lifecycle: silverhand_backend_core::conversation_messaging::messaging::task::TaskCreateConversationState,
    ) -> (Router, Arc<AtomicUsize>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let post_calls = Arc::new(AtomicUsize::new(0));
        let write_calls = Arc::new(AtomicUsize::new(0));
        let fired = Arc::new(AtomicUsize::new(0));
        let state = HumanCreateState {
            post: Post {
                allow,
                calls: Arc::clone(&post_calls),
            },
            boards: Boards { board, lifecycle },
            write: CreateWrite {
                created,
                calls: Arc::clone(&write_calls),
            },
            aftermath: RecordingAftermath {
                fired: Arc::clone(&fired),
            },
        };
        (human_create_router(state), post_calls, write_calls, fired)
    }

    #[tokio::test]
    async fn human_create_success_is_201_and_fires_aftermath() {
        let (router, post_calls, write_calls, fired) = create_app(true, Some(ws_board()), true);
        let (status, body) = send(
            router,
            "/api/tasks",
            Some(r#"{"title":"x","conversation_id":"c-1"}"#),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(body["success"], true);
        assert_eq!(body["task"]["id"], "task-1");
        assert_eq!(body["task"]["task_number"], 1);
        assert_eq!(post_calls.load(Ordering::SeqCst), 1);
        assert_eq!(write_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fired.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn human_create_non_member_is_403_zero_write() {
        let (router, post_calls, write_calls, fired) = create_app(false, Some(ws_board()), true);
        let (status, body) = send(
            router,
            "/api/tasks",
            Some(r#"{"title":"x","conversation_id":"c-1"}"#),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            body,
            json!({"success": false, "code": "not_conversation_member"})
        );
        assert_eq!(post_calls.load(Ordering::SeqCst), 1);
        assert_eq!(write_calls.load(Ordering::SeqCst), 0);
        assert_eq!(fired.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn human_create_without_session_is_500_zero_reach() {
        let (router, post_calls, _w, _f) = create_app(true, Some(ws_board()), true);
        let (status, _b) = send(
            router,
            "/api/tasks",
            Some(r#"{"title":"x","conversation_id":"c-1"}"#),
            false,
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            post_calls.load(Ordering::SeqCst),
            0,
            "post-check zero-reach without a session"
        );
    }

    #[tokio::test]
    async fn human_create_blank_title_is_422() {
        let (router, post_calls, _w, _f) = create_app(true, Some(ws_board()), true);
        let (status, _b) = send(
            router,
            "/api/tasks",
            Some(r#"{"title":"   ","conversation_id":"c-1"}"#),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            post_calls.load(Ordering::SeqCst),
            0,
            "admission before authz"
        );
    }

    #[tokio::test]
    async fn human_create_optional_body_rejects_null_before_post_authority() {
        let (router, post_calls, write_calls, fired) = create_app(true, Some(ws_board()), true);
        let (status, _value) = send(
            router,
            "/api/tasks",
            Some(r#"{"title":"x","conversation_id":"c-1","body":null}"#),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(post_calls.load(Ordering::SeqCst), 0);
        assert_eq!(write_calls.load(Ordering::SeqCst), 0);
        assert_eq!(fired.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn human_create_pair_board_is_ok() {
        let board = CBoardKey::Pair(PairKey::parse("a__b").unwrap());
        let (router, _p, write_calls, _f) = create_app(true, Some(board), true);
        let (status, body) = send(
            router,
            "/api/tasks",
            Some(r#"{"title":"x","conversation_id":"c-1"}"#),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(body["success"], true);
        assert_eq!(write_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn human_create_service_refusals_are_code_only_and_zero_effect() {
        use silverhand_backend_core::conversation_messaging::messaging::task::TaskCreateConversationState;
        for (board, lifecycle, expected_status, expected_code) in [
            (
                None,
                TaskCreateConversationState::Open,
                StatusCode::NOT_FOUND,
                "conversation_not_found",
            ),
            (
                Some(ws_board()),
                TaskCreateConversationState::Closed,
                StatusCode::CONFLICT,
                "conversation_closed",
            ),
            (
                Some(ws_board()),
                TaskCreateConversationState::Archived,
                StatusCode::CONFLICT,
                "conversation_archived",
            ),
        ] {
            let (router, _post, write, fired) =
                create_app_with_lifecycle(true, board, true, lifecycle);
            let (status, body) = send(
                router,
                "/api/tasks",
                Some(r#"{"title":"x","conversation_id":"c-1"}"#),
                true,
            )
            .await;
            assert_eq!(status, expected_status);
            assert_eq!(body, json!({"success": false, "code": expected_code}));
            assert_eq!(write.load(Ordering::SeqCst), 0);
            assert_eq!(fired.load(Ordering::SeqCst), 0);
        }
    }
}
