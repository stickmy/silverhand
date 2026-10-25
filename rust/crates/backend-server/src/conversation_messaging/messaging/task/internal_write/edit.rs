//! `POST /internal/agents/{aid}/tasks/edit` — the agent edit face. A SINGLE ref
//! (exactly one of `task_number` / `task_id`) with at least one of `title` / `body`,
//! returning the typed edit envelope (always `200`).
//!
//! The core [`TaskEditWrite::apply_edit`] UoW owns participation, direct-pair
//! read-only, and the canonical no-op guard under its own write lock; on a real
//! change it `COALESCE`-binds the changed fields and stamps `last_editor`. The
//! adapter settles WHICH id the ref targets and WHETHER this agent may reach its
//! board — the same door as status/claim — and gates the edit's DUAL capability.
//!
//! Edit is BOTH a mutation and a read (its success returns the full definition), so
//! the workspace arm requires BOTH `write_tasks` AND `read_tasks`, in that order,
//! each folding into the same `capability_denied` (naming whichever is missing) —
//! parity with `editTaskAgent`. A direct-pair board has no capability axis.
//!
//! Field semantics: an ABSENT field means "leave unchanged" (`None`); a PRESENT
//! field (including an empty `body`) sets that value. At least one must be present
//! (the `422` refine). The core UoW's `NoOp` — both absent-or-equal — is a success
//! (the current detail is still the answer).

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
    AgentDisplayNameReader, AgentExistenceReader, ReadViewer, TaskByIdCapability, TaskContent,
    TaskEditWrite, TaskEditWriteOutcome, TaskEditor, TaskId, TaskNumber, TaskTitle,
    TaskWriteCapability, TaskWriteDoorReads,
};
use silverhand_backend_core::foundation::identity::AgentId;
use silverhand_backend_core::host_runtime::runtime_credential::RuntimePrincipal;

use crate::conversation_messaging::messaging::task::aftermath::TaskMutationAftermath;

use super::selector::{
    BoardResolveRefusal, ResolvedBoard, WorkspaceSelector, resolve_write_board_with_source_board,
};
use super::update_status::TaskNumberResolver;
use crate::foundation::http_error::plain_500 as safe_500;
use crate::foundation::wire_admission::{ValidatedJson, ValidatedJsonRejection, invalid_input};
use crate::foundation::wire_patch::RequiredPatchField;
use crate::host_runtime::runtime_credential::bind_runtime_actor;

pub struct TaskEditState<D, N, W, A, AF> {
    pub door: D,
    pub numbers: N,
    pub write: W,
    /// Resolves the acting agent's display name for the `task_edit` event's actor
    /// pill (the DB-backed name the appended system message records).
    pub names: A,
    /// Mandatory post-commit aftermath, fired once on a committed edit.
    pub aftermath: AF,
}

#[derive(Debug, Deserialize, garde::Validate)]
pub(crate) struct EditBody {
    #[garde(skip)]
    #[serde(default)]
    task_number: RequiredPatchField<i64>,
    #[garde(skip)]
    #[serde(default)]
    task_id: RequiredPatchField<String>,
    #[garde(skip)]
    #[serde(default)]
    title: RequiredPatchField<String>,
    #[garde(skip)]
    #[serde(default)]
    body: RequiredPatchField<String>,
    #[garde(skip)]
    #[serde(default)]
    workspace_id: RequiredPatchField<String>,
    #[garde(skip)]
    #[serde(default)]
    workspace_name: RequiredPatchField<String>,
}

enum EditTarget {
    ById(TaskId),
    ByNumber {
        task_number: i64,
        selector: WorkspaceSelector,
    },
}

struct ValidatedEdit {
    target: EditTarget,
    /// `Some` when the field was present and its value parsed; `None` = unchanged.
    title: Option<TaskTitle>,
    content: Option<TaskContent>,
}

impl ValidatedEdit {
    fn try_from_body(body: EditBody) -> Result<Self, ()> {
        let task_number = body.task_number.into_option();
        let task_id = body.task_id.into_option();
        let raw_title = body.title.into_option();
        let raw_body = body.body.into_option();
        let workspace_id =
            crate::foundation::wire_patch::empty_selector_as_none(body.workspace_id.into_option());
        let workspace_name = crate::foundation::wire_patch::empty_selector_as_none(
            body.workspace_name.into_option(),
        );
        // At least one of title / body must be present.
        if raw_title.is_none() && raw_body.is_none() {
            return Err(());
        }
        // A present field is parsed to its value authority (semantic 422 on
        // failure); an absent field stays None ("leave unchanged").
        let title = match raw_title {
            Some(raw) => Some(TaskTitle::parse(&raw).map_err(|_| ())?),
            None => None,
        };
        let content = match raw_body {
            Some(raw) => Some(TaskContent::parse(raw).map_err(|_| ())?),
            None => None,
        };

        let target = match (task_number, task_id) {
            (Some(_), Some(_)) | (None, None) => return Err(()),
            (Some(task_number), None) => {
                if workspace_id.is_some() && workspace_name.is_some() {
                    return Err(());
                }
                EditTarget::ByNumber {
                    task_number: TaskNumber::new(task_number).map_err(|_| ())?.get(),
                    selector: WorkspaceSelector {
                        workspace_id,
                        workspace_name,
                    },
                }
            }
            (None, Some(raw_id)) => {
                if workspace_id.is_some() || workspace_name.is_some() {
                    return Err(());
                }
                EditTarget::ById(TaskId::parse(raw_id).map_err(|_| ())?)
            }
        };

        Ok(Self {
            target,
            title,
            content,
        })
    }
}

type Shared<D, N, W, A, AF> = State<Arc<TaskEditState<D, N, W, A, AF>>>;

pub fn task_edit_router<D, N, W, A, AF>(state: TaskEditState<D, N, W, A, AF>) -> Router
where
    D: TaskWriteDoorReads + AgentExistenceReader + Clone + Send + Sync + 'static,
    N: TaskNumberResolver + Send + Sync + 'static,
    W: TaskEditWrite + Send + Sync + 'static,
    A: AgentDisplayNameReader + Send + Sync + 'static,
    AF: TaskMutationAftermath,
{
    Router::new()
        .route(
            "/internal/agents/{aid}/tasks/edit",
            post(edit_route::<D, N, W, A, AF>),
        )
        .with_state(Arc::new(state))
}

async fn edit_route<D, N, W, A, AF>(
    State(state): Shared<D, N, W, A, AF>,
    Path(aid): Path<String>,
    principal: Option<axum::extract::Extension<RuntimePrincipal>>,
    body: Result<ValidatedJson<EditBody>, ValidatedJsonRejection>,
) -> Response
where
    D: TaskWriteDoorReads + AgentExistenceReader + Clone + Send + Sync + 'static,
    N: TaskNumberResolver + Send + Sync + 'static,
    W: TaskEditWrite + Send + Sync + 'static,
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
    let Ok(validated) = ValidatedEdit::try_from_body(body) else {
        return invalid_input();
    };

    let mut source_snapshot = None;
    let task_id = match validated.target {
        EditTarget::ById(id) => {
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
                .may_mutate_task_by_id(&id, actor, TaskByIdCapability::WriteAndRead)
                .await
            {
                Ok(true) => {}
                Ok(false) => return task_not_found(),
                Err(_) => return safe_500(),
            }
            id
        }
        EditTarget::ByNumber {
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
                    // DUAL capability, write_tasks THEN read_tasks — edit returns the
                    // full definition, so an agent with write but not read could
                    // otherwise exfiltrate content. Report whichever is missing.
                    match door
                        .has_capability(TaskWriteCapability::Write, &workspace, actor)
                        .await
                    {
                        Ok(true) => {}
                        Ok(false) => return capability_denied("write_tasks"),
                        Err(_) => return safe_500(),
                    }
                    match door
                        .has_capability(TaskWriteCapability::Read, &workspace, actor)
                        .await
                    {
                        Ok(true) => {}
                        Ok(false) => return capability_denied("read_tasks"),
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

    let editor = TaskEditor::Agent(actor.clone());
    // The actor pill name for the appended task_edit event: the DB-resolved display
    // name, falling back to the id when the agent row is gone (the TS `?? id`).
    let editor_name = match state.names.agent_display_name(actor).await {
        Ok(Some(name)) => name,
        Ok(None) => return mutation_failure("agent_not_found", "agent not found", json!({})),
        Err(_) => return safe_500(),
    };
    match state
        .write
        .apply_edit(
            &task_id,
            &editor,
            Some(&editor_name),
            validated.title,
            validated.content,
        )
        .await
    {
        // A committed change fires the aftermath once on its witness; a no-op has no
        // witness and no aftermath. Both answers use the authorized detail post-image
        // carried out of the same writer transaction, never a mutable live re-read.
        Ok(TaskEditWriteOutcome::Applied(success)) => {
            let Some(witness) = success.witness else {
                return safe_500();
            };
            crate::conversation_messaging::messaging::task::aftermath::run_agent_aftermath(
                &state.aftermath,
                *witness,
                actor,
                source_snapshot,
            )
            .await;
            edit_success(success.detail, actor)
        }
        Ok(TaskEditWriteOutcome::NoOp(success)) => edit_success(success.detail, actor),
        Ok(TaskEditWriteOutcome::AgentNotFound) => {
            mutation_failure("agent_not_found", "agent not found", json!({}))
        }
        Ok(TaskEditWriteOutcome::NotFound) => task_not_found(),
        Ok(TaskEditWriteOutcome::ConversationReadonly) => conversation_readonly(),
        Err(_) => safe_500(),
    }
}

fn edit_success(
    detail: silverhand_backend_core::conversation_messaging::messaging::task::VisibleTaskPoint,
    actor: &AgentId,
) -> Response {
    let Some(source) = detail.for_viewer(&ReadViewer::Agent(actor.clone())) else {
        return safe_500();
    };
    match super::super::wire::project_detail(source) {
        Ok(task) => (
            StatusCode::OK,
            Json(json!({ "success": true, "task": task })),
        )
            .into_response(),
        Err(_) => safe_500(),
    }
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
fn capability_denied(capability: &str) -> Response {
    mutation_failure(
        "capability_denied",
        &format!("agent lacks the {capability} capability in the target workspace"),
        json!({ "capability": capability }),
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
    use axum::body::{Body, to_bytes};
    use axum::extract::Extension;
    use axum::http::Request;
    use serde_json::Value;
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;

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
        write_cap: bool,
        read_cap: bool,
        board_calls: Arc<AtomicUsize>,
        membership_calls: Arc<AtomicUsize>,
        write_cap_calls: Arc<AtomicUsize>,
        read_cap_calls: Arc<AtomicUsize>,
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
            cap: TaskWriteCapability,
            _w: &WorkspaceId,
            _a: &AgentId,
        ) -> Result<bool, PersistenceFailure> {
            match cap {
                TaskWriteCapability::Write => {
                    self.write_cap_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(self.write_cap)
                }
                TaskWriteCapability::Read => {
                    self.read_cap_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(self.read_cap)
                }
                TaskWriteCapability::Create => Ok(false),
            }
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
        outcome: TaskEditWriteOutcome,
        calls: Arc<AtomicUsize>,
    }
    impl TaskEditWrite for RecordingWrite {
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
            Ok(self.outcome.clone())
        }
    }

    /// A recording aftermath counting committed witnesses.
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

    /// A name reader that always resolves a fixed display name, counting the read.
    struct RecordingNames {
        calls: Arc<AtomicUsize>,
    }
    impl AgentDisplayNameReader for RecordingNames {
        async fn agent_display_name(
            &self,
            _agent: &AgentId,
        ) -> Result<
            Option<
                silverhand_backend_core::conversation_messaging::messaging::task::AgentEventNames,
            >,
            PersistenceFailure,
        > {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Some(silverhand_backend_core::conversation_messaging::messaging::task::AgentEventNames::new("alice", "Alice")))
        }
    }

    fn committed_detail()
    -> silverhand_backend_core::conversation_messaging::messaging::task::VisibleTaskPoint {
        use silverhand_backend_core::conversation_messaging::messaging::task::{
            ReadViewer, TaskAggregate, TaskDisplaySidecars, TaskPointCandidate, TaskRow,
            VisibleTaskPoint,
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
        VisibleTaskPoint::from_committed_edit(
            ReadViewer::Agent(AgentId::parse(AGENT).unwrap()),
            candidate,
        )
        .unwrap()
    }

    /// The `Applied` result the tests use.
    fn applied() -> TaskEditWriteOutcome {
        let witness = silverhand_backend_core::conversation_messaging::messaging::task::TaskEditWitness {
            kind: silverhand_backend_core::conversation_messaging::messaging::task::TaskEventKind::Edit,
            task_id: TaskId::parse("task-1").unwrap(),
            task_number: silverhand_backend_core::conversation_messaging::messaging::task::TaskNumber::new(7).unwrap(),
            event_message: silverhand_backend_core::conversation_messaging::messaging::task::TaskCommittedEventMessage::new(
                silverhand_backend_core::conversation_messaging::messaging::task::CreatedEventMessageId::parse("msg-1").unwrap(),
                ConversationId::parse("conv-1").unwrap(), None,
                silverhand_backend_core::conversation_messaging::messaging::task::TaskEventMessage { content: "event".into(), metadata: "{}".into() }, 1, 1,
            ),
            conversation_id: silverhand_backend_core::foundation::identity::ConversationId::parse("conv-1").unwrap(),
            board: silverhand_backend_core::conversation_messaging::messaging::task::BoardKey::Workspace(silverhand_backend_core::foundation::identity::WorkspaceId::parse("ws-1").unwrap()),
            thread_root_id: None,
            title: silverhand_backend_core::conversation_messaging::messaging::task::TaskTitle::parse("title").unwrap(),
            status: silverhand_backend_core::conversation_messaging::messaging::task::TaskStatus::Todo,
            assignee: None,
            closed_at: None,
            source_message_id: None,
        };
        TaskEditWriteOutcome::Applied(
            silverhand_backend_core::conversation_messaging::messaging::task::TaskEditSuccess {
                detail: committed_detail(),
                witness: Some(Box::new(witness)),
            },
        )
    }

    struct Counts {
        board: Arc<AtomicUsize>,
        membership: Arc<AtomicUsize>,
        write_cap: Arc<AtomicUsize>,
        read_cap: Arc<AtomicUsize>,
        numbers: Arc<AtomicUsize>,
        write: Arc<AtomicUsize>,
    }
    struct DoorSpec {
        agent_exists: bool,
        named: usize,
        board: Option<BoardKey>,
        membership: WorkspaceMembership,
        write_cap: bool,
        read_cap: bool,
    }
    impl Default for DoorSpec {
        fn default() -> Self {
            Self {
                agent_exists: true,
                named: 1,
                board: None,
                membership: WorkspaceMembership::Member,
                write_cap: true,
                read_cap: true,
            }
        }
    }

    fn app(
        principal: RuntimePrincipal,
        door: DoorSpec,
        number_found: bool,
        outcome: TaskEditWriteOutcome,
    ) -> (Router, Counts) {
        let counts = Counts {
            board: Arc::new(AtomicUsize::new(0)),
            membership: Arc::new(AtomicUsize::new(0)),
            write_cap: Arc::new(AtomicUsize::new(0)),
            read_cap: Arc::new(AtomicUsize::new(0)),
            numbers: Arc::new(AtomicUsize::new(0)),
            write: Arc::new(AtomicUsize::new(0)),
        };
        let state = TaskEditState {
            door: RecordingDoor {
                agent_exists: door.agent_exists,
                named: door.named,
                board: door.board,
                membership: door.membership,
                write_cap: door.write_cap,
                read_cap: door.read_cap,
                board_calls: Arc::clone(&counts.board),
                membership_calls: Arc::clone(&counts.membership),
                write_cap_calls: Arc::clone(&counts.write_cap),
                read_cap_calls: Arc::clone(&counts.read_cap),
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
                calls: Arc::new(AtomicUsize::new(0)),
            },
            aftermath: RecordingAftermath {
                fired: Arc::new(AtomicUsize::new(0)),
            },
        };
        let router = task_edit_router(state).layer(Extension(principal));
        (router, counts)
    }

    async fn call_at(app: Router, path_aid: &str, body: String) -> (StatusCode, Value) {
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/internal/agents/{path_aid}/tasks/edit"))
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

    #[tokio::test]
    async fn a_credential_for_another_agent_is_401_zero_reach() {
        let (app, counts) = app(agent_slot(), DoorSpec::default(), true, applied());
        let (status, _b) = call_at(
            app,
            "agent-2",
            r#"{"task_number":7,"title":"x"}"#.to_owned(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(counts.write.load(Ordering::SeqCst), 0);
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
        let (status, body) = call(router, r#"{"task_id":"task-1","title":"x"}"#.to_owned()).await;
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
        let (status, body) = call(app, r#"{"task_id":"task-1","title":"x"}"#.to_owned()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["code"], "task_not_found");
        assert_eq!(counts.write.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn neither_title_nor_body_is_a_semantic_422() {
        let (app, _c) = app(agent_slot(), DoorSpec::default(), true, applied());
        let (status, body) = call(app, r#"{"task_id":"task-1"}"#.to_owned()).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["error"], "invalid input");
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
                json!({"task_number":case["value"].clone(),"title":"edited"}).to_string(),
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
                assert_eq!(counts.write_cap.load(Ordering::SeqCst), 0, "{case}");
                assert_eq!(counts.read_cap.load(Ordering::SeqCst), 0, "{case}");
                assert_eq!(counts.numbers.load(Ordering::SeqCst), 0, "{case}");
                assert_eq!(counts.write.load(Ordering::SeqCst), 0, "{case}");
            }
        }
    }

    #[tokio::test]
    async fn all_six_optional_nonnullable_fields_reject_null_and_empty_selectors_are_absent() {
        for body in [
            r#"{"task_number":null,"task_id":"task-1","title":"x"}"#,
            r#"{"task_number":7,"task_id":null,"title":"x"}"#,
            r#"{"task_id":"task-1","title":null,"body":"x"}"#,
            r#"{"task_id":"task-1","title":"x","body":null}"#,
            r#"{"task_id":"task-1","title":"x","workspace_id":null}"#,
            r#"{"task_id":"task-1","title":"x","workspace_name":null}"#,
        ] {
            let (app, counts) = app(agent_slot(), DoorSpec::default(), true, applied());
            let (status, _value) = call(app, body.to_owned()).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
            assert_eq!(counts.board.load(Ordering::SeqCst), 0, "{body}");
            assert_eq!(counts.numbers.load(Ordering::SeqCst), 0, "{body}");
            assert_eq!(counts.write.load(Ordering::SeqCst), 0, "{body}");
        }

        for body in [
            r#"{"task_id":"task-1","title":"x","workspace_id":""}"#,
            r#"{"task_id":"task-1","title":"x","workspace_name":""}"#,
            r#"{"task_id":"task-1","title":"x","workspace_id":"","workspace_name":""}"#,
        ] {
            let (app, counts) = app(agent_slot(), DoorSpec::default(), true, applied());
            let (status, value) = call(app, body.to_owned()).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(value["success"], true);
            assert_eq!(value["task"]["id"], "task-1");
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
            r#"{"task_number":7,"title":"x","workspace_id":"","workspace_name":""}"#.to_owned(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["success"], true);
        assert_eq!(value["task"]["id"], "task-1");
        assert_eq!(counts.board.load(Ordering::SeqCst), 1);
        assert_eq!(counts.write.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_blank_title_is_a_semantic_422() {
        // Title trims to empty → the title value authority rejects it.
        let (app, _c) = app(agent_slot(), DoorSpec::default(), true, applied());
        let (status, body) = call(app, r#"{"task_id":"task-1","title":"   "}"#.to_owned()).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["error"], "invalid input");
    }

    #[tokio::test]
    async fn an_empty_body_is_a_legal_clear() {
        // An empty body is a legal value (clears the content), not a 422.
        let (app, counts) = app(agent_slot(), DoorSpec::default(), true, applied());
        let (status, body) = call(app, r#"{"task_id":"task-1","body":""}"#.to_owned()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["success"], true);
        assert_eq!(counts.write.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn by_id_off_a_conversation_frame_is_task_not_found_zero_reach() {
        let (app, counts) = app(conversation_slot(), DoorSpec::default(), true, applied());
        let (status, body) = call(app, r#"{"task_id":"task-1","title":"x"}"#.to_owned()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["code"], "task_not_found");
        assert_eq!(counts.write.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_no_op_is_success() {
        let (app, _c) = app(
            agent_slot(),
            DoorSpec::default(),
            true,
            TaskEditWriteOutcome::NoOp(
                silverhand_backend_core::conversation_messaging::messaging::task::TaskEditSuccess {
                    detail: committed_detail(),
                    witness: None,
                },
            ),
        );
        let (status, body) = call(app, r#"{"task_id":"task-1","title":"x"}"#.to_owned()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["success"], true);
    }

    #[tokio::test]
    async fn missing_write_tasks_is_capability_denied_before_read_tasks() {
        let (app, counts) = app(
            agent_slot(),
            DoorSpec {
                write_cap: false,
                ..DoorSpec::default()
            },
            true,
            applied(),
        );
        let (status, body) = call(
            app,
            r#"{"task_number":7,"title":"x","workspace_name":"only"}"#.to_owned(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["code"], "capability_denied");
        assert_eq!(body["capability"], "write_tasks");
        assert_eq!(counts.write_cap.load(Ordering::SeqCst), 1);
        assert_eq!(
            counts.read_cap.load(Ordering::SeqCst),
            0,
            "read_tasks not consulted once write_tasks fails"
        );
        assert_eq!(counts.write.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn write_but_not_read_tasks_is_capability_denied_read() {
        // An agent with write but not read is refused — edit returns the definition,
        // which read_tasks gates — naming read_tasks.
        let (app, counts) = app(
            agent_slot(),
            DoorSpec {
                write_cap: true,
                read_cap: false,
                ..DoorSpec::default()
            },
            true,
            applied(),
        );
        let (status, body) = call(
            app,
            r#"{"task_number":7,"title":"x","workspace_name":"only"}"#.to_owned(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["code"], "capability_denied");
        assert_eq!(body["capability"], "read_tasks");
        assert_eq!(counts.write_cap.load(Ordering::SeqCst), 1);
        assert_eq!(counts.read_cap.load(Ordering::SeqCst), 1);
        assert_eq!(
            counts.write.load(Ordering::SeqCst),
            0,
            "write zero-reach when read_tasks missing"
        );
    }

    #[tokio::test]
    async fn a_direct_pair_edit_reaches_neither_capability() {
        let (app, counts) = app(
            conversation_slot(),
            DoorSpec {
                board: Some(BoardKey::Pair(PairKey::parse("a__b").unwrap())),
                ..DoorSpec::default()
            },
            true,
            applied(),
        );
        let (status, body) = call(app, r#"{"task_number":7,"title":"x"}"#.to_owned()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["success"], true);
        assert_eq!(counts.write_cap.load(Ordering::SeqCst), 0);
        assert_eq!(counts.read_cap.load(Ordering::SeqCst), 0);
        assert_eq!(counts.write.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn ambiguous_before_membership() {
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
            r#"{"task_number":7,"title":"x","workspace_name":"dup"}"#.to_owned(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["code"], "ambiguous_workspace");
        assert_eq!(counts.membership.load(Ordering::SeqCst), 0);
    }
}
