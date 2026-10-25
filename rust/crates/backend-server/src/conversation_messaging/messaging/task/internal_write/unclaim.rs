//! `POST /internal/agents/{aid}/tasks/unclaim` — the agent unclaim face. A SINGLE
//! ref (exactly one of `task_number` / `task_id`), returning the typed
//! `TaskMutationResult` envelope (always `200`).
//!
//! Assignment-only: the core [`TaskClaimWrite::agent_unclaim`] UoW clears the
//! assignee if the actor IS the assignee and the task is not terminal, all under its
//! own write lock (participation → read-only → terminal → the assignee CAS). The
//! adapter settles WHICH id the ref targets and WHETHER this agent may reach its
//! board — the same door as claim/status — and re-implements no gate.

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
    AgentExistenceReader, TaskByIdCapability, TaskClaimWrite, TaskId, TaskNumber,
    TaskWriteCapability, TaskWriteDoorReads, UnclaimOutcome,
};
use silverhand_backend_core::foundation::identity::AgentId;
use silverhand_backend_core::host_runtime::runtime_credential::RuntimePrincipal;

use super::selector::{
    BoardResolveRefusal, ResolvedBoard, WorkspaceSelector, resolve_write_board_with_source_board,
};
use super::update_status::TaskNumberResolver;
use crate::conversation_messaging::messaging::task::aftermath::TaskMutationAftermath;
use crate::foundation::http_error::plain_500 as safe_500;
use crate::foundation::wire_admission::{ValidatedJson, ValidatedJsonRejection, invalid_input};
use crate::foundation::wire_patch::RequiredPatchField;
use crate::host_runtime::runtime_credential::bind_runtime_actor;

pub struct TaskUnclaimState<D, N, W, AF> {
    pub door: D,
    pub numbers: N,
    pub write: W,
    /// Mandatory post-commit aftermath, fired once on a committed unclaim.
    pub aftermath: AF,
}

#[derive(Debug, Deserialize, garde::Validate)]
pub(crate) struct UnclaimBody {
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

enum UnclaimTarget {
    ById(TaskId),
    ByNumber {
        task_number: i64,
        selector: WorkspaceSelector,
    },
}

impl UnclaimTarget {
    fn try_from_body(body: UnclaimBody) -> Result<Self, ()> {
        let task_number = body.task_number.into_option();
        let task_id = body.task_id.into_option();
        let workspace_id =
            crate::foundation::wire_patch::empty_selector_as_none(body.workspace_id.into_option());
        let workspace_name = crate::foundation::wire_patch::empty_selector_as_none(
            body.workspace_name.into_option(),
        );
        match (task_number, task_id) {
            (Some(_), Some(_)) | (None, None) => Err(()),
            (Some(task_number), None) => {
                if workspace_id.is_some() && workspace_name.is_some() {
                    return Err(());
                }
                Ok(UnclaimTarget::ByNumber {
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
                Ok(UnclaimTarget::ById(TaskId::parse(raw_id).map_err(|_| ())?))
            }
        }
    }
}

type Shared<D, N, W, AF> = State<Arc<TaskUnclaimState<D, N, W, AF>>>;

pub fn task_unclaim_router<D, N, W, AF>(state: TaskUnclaimState<D, N, W, AF>) -> Router
where
    D: TaskWriteDoorReads + AgentExistenceReader + Clone + Send + Sync + 'static,
    N: TaskNumberResolver + Send + Sync + 'static,
    W: TaskClaimWrite + Send + Sync + 'static,
    AF: TaskMutationAftermath,
{
    Router::new()
        .route(
            "/internal/agents/{aid}/tasks/unclaim",
            post(unclaim_route::<D, N, W, AF>),
        )
        .with_state(Arc::new(state))
}

async fn unclaim_route<D, N, W, AF>(
    State(state): Shared<D, N, W, AF>,
    Path(aid): Path<String>,
    principal: Option<axum::extract::Extension<RuntimePrincipal>>,
    body: Result<ValidatedJson<UnclaimBody>, ValidatedJsonRejection>,
) -> Response
where
    D: TaskWriteDoorReads + AgentExistenceReader + Clone + Send + Sync + 'static,
    N: TaskNumberResolver + Send + Sync + 'static,
    W: TaskClaimWrite + Send + Sync + 'static,
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
    let Ok(target) = UnclaimTarget::try_from_body(body) else {
        return invalid_input();
    };

    let mut source_snapshot = None;
    let task_id = match target {
        UnclaimTarget::ById(id) => {
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
        UnclaimTarget::ByNumber {
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

    match state.write.agent_unclaim(&task_id, actor).await {
        Ok(UnclaimOutcome::Unclaimed(witness)) => {
            crate::conversation_messaging::messaging::task::aftermath::run_agent_aftermath(
                &state.aftermath,
                *witness,
                actor,
                source_snapshot,
            )
            .await;
            success()
        }
        Ok(UnclaimOutcome::AgentNotFound) => {
            mutation_failure("agent_not_found", "agent not found", json!({}))
        }
        Ok(UnclaimOutcome::NotFound) => task_not_found(),
        Ok(UnclaimOutcome::ConversationReadonly) => conversation_readonly(),
        Ok(UnclaimOutcome::TaskDone) => {
            mutation_failure("task_done", "task is already done", json!({}))
        }
        Ok(UnclaimOutcome::TaskClosed) => {
            mutation_failure("task_closed", "task is closed", json!({}))
        }
        Ok(UnclaimOutcome::NotAssignee) => {
            mutation_failure("not_assignee", "only the assignee can unclaim", json!({}))
        }
        Err(_) => safe_500(),
    }
}

fn success() -> Response {
    (StatusCode::OK, Json(json!({ "success": true }))).into_response()
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
    use axum::body::{Body, to_bytes};
    use axum::extract::Extension;
    use axum::http::Request;
    use serde_json::Value;
    use silverhand_backend_core::conversation_messaging::messaging::task::{
        AgentClaimOutcome, BoardKey, HumanClaimOutcome, PairKey, WorkspaceMembership,
    };
    use silverhand_backend_core::foundation::identity::{
        ConversationId, DaemonInstanceId, UserId, WorkspaceId,
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
        capability: bool,
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
        outcome: UnclaimOutcome,
        calls: Arc<AtomicUsize>,
    }
    impl TaskClaimWrite for RecordingWrite {
        async fn agent_claim(
            &self,
            _id: &TaskId,
            _actor: &AgentId,
            _actor_name: &silverhand_backend_core::conversation_messaging::messaging::task::AgentEventNames,
        ) -> Result<AgentClaimOutcome, PersistenceFailure> {
            unreachable!("unclaim face never claims")
        }
        async fn human_claim(
            &self,
            _id: &TaskId,
            _actor: &UserId,
            _assignee: &AgentId,
        ) -> Result<HumanClaimOutcome, PersistenceFailure> {
            unreachable!("agent face never calls human_claim")
        }
        async fn agent_unclaim(
            &self,
            _id: &TaskId,
            _actor: &AgentId,
        ) -> Result<UnclaimOutcome, PersistenceFailure> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.outcome.clone())
        }
        async fn human_unclaim(
            &self,
            _id: &TaskId,
            _actor: &UserId,
        ) -> Result<UnclaimOutcome, PersistenceFailure> {
            unreachable!("agent face never calls human_unclaim")
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
    /// An `Unclaimed` outcome with a stub witness.
    fn unclaimed() -> UnclaimOutcome {
        use silverhand_backend_core::conversation_messaging::messaging::task::{
            CreatedEventMessageId, TaskMutationWitness, TaskNumber,
        };
        use silverhand_backend_core::foundation::identity::ConversationId;
        UnclaimOutcome::Unclaimed(Box::new(TaskMutationWitness {
            kind: silverhand_backend_core::conversation_messaging::messaging::task::TaskEventKind::Unclaim,
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
            status: silverhand_backend_core::conversation_messaging::messaging::task::TaskStatus::InProgress,
            assignee: None,
            closed_at: None,
            source_message_id: None,
        }))
    }

    struct Counts {
        board: Arc<AtomicUsize>,
        membership: Arc<AtomicUsize>,
        capability: Arc<AtomicUsize>,
        numbers: Arc<AtomicUsize>,
        write: Arc<AtomicUsize>,
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
        outcome: UnclaimOutcome,
    ) -> (Router, Counts) {
        let counts = Counts {
            board: Arc::new(AtomicUsize::new(0)),
            membership: Arc::new(AtomicUsize::new(0)),
            capability: Arc::new(AtomicUsize::new(0)),
            numbers: Arc::new(AtomicUsize::new(0)),
            write: Arc::new(AtomicUsize::new(0)),
        };
        let state = TaskUnclaimState {
            door: RecordingDoor {
                agent_exists: door.agent_exists,
                named: door.named,
                board: door.board,
                membership: door.membership,
                capability: door.capability,
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
            aftermath: RecordingAftermath {
                fired: Arc::new(AtomicUsize::new(0)),
            },
        };
        let router = task_unclaim_router(state).layer(Extension(principal));
        (router, counts)
    }

    async fn call_at(app: Router, path_aid: &str, body: String) -> (StatusCode, Value) {
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/internal/agents/{path_aid}/tasks/unclaim"))
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
        let (app, counts) = app(agent_slot(), DoorSpec::default(), true, unclaimed());
        let (status, _b) = call_at(app, "agent-2", r#"{"task_number":7}"#.to_owned()).await;
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
            unclaimed(),
        );
        let (status, body) = call(router, r#"{"task_id":"task-1"}"#.to_owned()).await;
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
            unclaimed(),
        );
        let (status, body) = call(app, r#"{"task_id":"task-1"}"#.to_owned()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["code"], "task_not_found");
        assert_eq!(counts.write.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn both_selectors_is_a_semantic_422() {
        let (app, _c) = app(agent_slot(), DoorSpec::default(), true, unclaimed());
        let (status, body) = call(app, r#"{"task_number":7,"task_id":"task-1"}"#.to_owned()).await;
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
                unclaimed(),
            );
            let (status, body) = call(
                app,
                json!({"task_number":case["value"].clone()}).to_string(),
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
            }
        }
    }

    #[tokio::test]
    async fn optional_nonnullable_fields_reject_null_and_empty_selectors_are_absent() {
        for body in [
            r#"{"task_number":null,"task_id":"task-1"}"#,
            r#"{"task_number":7,"task_id":null}"#,
            r#"{"task_id":"task-1","workspace_id":null}"#,
            r#"{"task_id":"task-1","workspace_name":null}"#,
        ] {
            let (app, counts) = app(agent_slot(), DoorSpec::default(), true, unclaimed());
            let (status, _value) = call(app, body.to_owned()).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
            assert_eq!(counts.board.load(Ordering::SeqCst), 0, "{body}");
            assert_eq!(counts.numbers.load(Ordering::SeqCst), 0, "{body}");
            assert_eq!(counts.write.load(Ordering::SeqCst), 0, "{body}");
        }

        for body in [
            r#"{"task_id":"task-1","workspace_id":""}"#,
            r#"{"task_id":"task-1","workspace_name":""}"#,
            r#"{"task_id":"task-1","workspace_id":"","workspace_name":""}"#,
        ] {
            let (app, counts) = app(agent_slot(), DoorSpec::default(), true, unclaimed());
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
            unclaimed(),
        );
        let (status, value) = call(
            app,
            r#"{"task_number":7,"workspace_id":"","workspace_name":""}"#.to_owned(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["success"], true);
        assert_eq!(counts.board.load(Ordering::SeqCst), 1);
        assert_eq!(counts.write.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn by_id_off_a_conversation_frame_is_task_not_found_zero_reach() {
        let (app, counts) = app(conversation_slot(), DoorSpec::default(), true, unclaimed());
        let (status, body) = call(app, r#"{"task_id":"task-1"}"#.to_owned()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["code"], "task_not_found");
        assert_eq!(counts.write.load(Ordering::SeqCst), 0);
        assert_eq!(counts.board.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn by_id_agent_slot_unclaims() {
        let (app, counts) = app(agent_slot(), DoorSpec::default(), true, unclaimed());
        let (status, body) = call(app, r#"{"task_id":"task-1"}"#.to_owned()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["success"], true);
        assert_eq!(counts.write.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn not_assignee_maps_to_not_assignee() {
        let (app, _c) = app(
            agent_slot(),
            DoorSpec::default(),
            true,
            UnclaimOutcome::NotAssignee,
        );
        let (status, body) = call(app, r#"{"task_id":"task-1"}"#.to_owned()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["code"], "not_assignee");
    }

    #[tokio::test]
    async fn capability_denied_zero_reach() {
        let (app, counts) = app(
            agent_slot(),
            DoorSpec {
                named: 1,
                capability: false,
                ..DoorSpec::default()
            },
            true,
            unclaimed(),
        );
        let (status, body) = call(
            app,
            r#"{"task_number":7,"workspace_name":"only"}"#.to_owned(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["code"], "capability_denied");
        assert_eq!(counts.numbers.load(Ordering::SeqCst), 0);
        assert_eq!(counts.write.load(Ordering::SeqCst), 0);
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
            unclaimed(),
        );
        let (status, body) = call(
            app,
            r#"{"task_number":7,"workspace_name":"dup"}"#.to_owned(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["code"], "ambiguous_workspace");
        assert_eq!(counts.membership.load(Ordering::SeqCst), 0);
        assert_eq!(counts.write.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn direct_pair_unclaim_reaches_no_membership_or_capability() {
        let (app, counts) = app(
            conversation_slot(),
            DoorSpec {
                board: Some(BoardKey::Pair(PairKey::parse("a__b").unwrap())),
                ..DoorSpec::default()
            },
            true,
            unclaimed(),
        );
        let (status, body) = call(app, r#"{"task_number":7}"#.to_owned()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["success"], true);
        assert_eq!(counts.membership.load(Ordering::SeqCst), 0);
        assert_eq!(counts.capability.load(Ordering::SeqCst), 0);
        assert_eq!(counts.write.load(Ordering::SeqCst), 1);
    }
}
