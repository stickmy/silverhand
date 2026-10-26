//! `POST /api/agents/{id}/schedules/{scheduleId}/cancel` — owner cancel.
//!
//! The shared creator-or-system floor ([`gate_agent_edit`]) runs first; the
//! atomic cancel unit of work then re-binds the schedule's OWN owner and grades
//! `manage_schedules` (a gate pass is not carried as proof of owning THIS
//! schedule). The outcome maps 1:1 to the schedule error wire contract.
//!
//! The success audit line — `agent schedule cancelled by user` with the viewer,
//! agent, and schedule — is emitted EXACTLY ONCE, after the unit of work reports
//! `Cancelled` (i.e. post-commit). Every refusal and every CAS-miss emits ZERO
//! success audit lines: the sink is reached only on the 200 arm.

use axum::{
    Json, Router,
    extract::{Extension, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use serde::Serialize;
use serde_json::json;
use silverhand_backend_core::{
    conversation_messaging::messaging::schedule::{
        ScheduleCancelOutcome, ScheduleCancelUnitOfWork, ScheduleId,
    },
    foundation::identity::UserId,
    identity_access::agent_detail_read::AgentDetailReadModel,
};

use super::{gate_agent_edit, wire::ScheduleDto};
use crate::foundation::http_error::coded_error as coded;

/// The product route template, in axum's `{id}`/`{scheduleId}` capture syntax.
pub const SCHEDULE_CANCEL_PATH: &str = "/api/agents/{id}/schedules/{scheduleId}/cancel";

/// The route name for the shared gate's fail-closed log line.
const ROUTE: &str = "agent-schedule-cancel";

/// The `manage_schedules` capability token, echoed in the capability-denied body.
const MANAGE_SCHEDULES: &str = "manage_schedules";

#[derive(Clone)]
struct CancelState<D, U> {
    detail_reads: D,
    cancels: U,
}

/// The gate read (visibility+authority) and the cancel unit of work are TWO
/// distinct authorities on TWO parameters — never fused.
pub fn router<D, U>(detail_reads: D, cancels: U) -> Router
where
    D: AgentDetailReadModel + Clone + Send + Sync + 'static,
    U: ScheduleCancelUnitOfWork + Clone + Send + Sync + 'static,
{
    Router::new()
        .route(SCHEDULE_CANCEL_PATH, post(cancel_schedule::<D, U>))
        .with_state(CancelState {
            detail_reads,
            cancels,
        })
}

async fn cancel_schedule<D, U>(
    Path((agent_id, schedule_id)): Path<(String, String)>,
    viewer: Option<Extension<UserId>>,
    State(state): State<CancelState<D, U>>,
) -> Response
where
    D: AgentDetailReadModel,
    U: ScheduleCancelUnitOfWork,
{
    let (agent, viewer) = match gate_agent_edit(&state.detail_reads, viewer, agent_id, ROUTE).await
    {
        Ok(bound) => bound,
        Err(refusal) => return refusal.into_response(),
    };

    // A matched route always captures a non-empty scheduleId; an empty one names
    // no schedule — the TS 400 for a missing path segment.
    let Ok(schedule_id) = ScheduleId::parse(schedule_id) else {
        return missing_ids();
    };

    match state
        .cancels
        .cancel_owned_schedule(&schedule_id, &agent)
        .await
    {
        Ok(ScheduleCancelOutcome::Cancelled(view)) => {
            // The success audit line — exactly once, post-commit, on the 200 arm.
            tracing::info!(
                user_id = viewer.as_str(),
                agent_id = agent.as_str(),
                schedule_id = schedule_id.as_str(),
                "agent schedule cancelled by user"
            );
            let dto = ScheduleDto::from(*view);
            (StatusCode::OK, Json(CancelBody { schedule: dto })).into_response()
        }
        Ok(ScheduleCancelOutcome::NotFound) => coded(
            StatusCode::NOT_FOUND,
            format!("schedule {} not found", schedule_id.as_str()),
            "schedule_not_found",
        ),
        Ok(ScheduleCancelOutcome::NotOwner) => coded(
            StatusCode::FORBIDDEN,
            format!(
                "schedule {} is owned by another agent",
                schedule_id.as_str()
            ),
            "not_owner",
        ),
        Ok(ScheduleCancelOutcome::CapabilityDenied { workspace_id }) => capability_denied(format!(
            "agent {} lacks manage_schedules in workspace {}",
            agent.as_str(),
            workspace_id.as_str()
        )),
        Ok(ScheduleCancelOutcome::NotCancellable { current_status }) => coded(
            StatusCode::CONFLICT,
            format!(
                "schedule {} is in terminal state {}",
                schedule_id.as_str(),
                current_status.as_token()
            ),
            "not_cancellable",
        ),
        Err(cause) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&cause))
                ),
                "agent-schedule-cancel unit of work failed"
            );
            super::internal_error()
        }
    }
}

/// The 400 for a missing path segment: `{ "error": "agent id and schedule id
/// required" }`.
fn missing_ids() -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": "agent id and schedule id required" })),
    )
        .into_response()
}

/// The capability-denied 403, which additionally echoes the capability and a
/// `details.capability`, matching `scheduleServiceErrorResponse`'s
/// `capability_denied` arm.
fn capability_denied(message: String) -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({
            "error": message,
            "code": "capability_denied",
            "capability": MANAGE_SCHEDULES,
            "message": message,
            "details": { "capability": MANAGE_SCHEDULES },
        })),
    )
        .into_response()
}

/// Success body: `{ "schedule": { ... } }`.
#[derive(Serialize)]
struct CancelBody {
    schedule: ScheduleDto,
}

#[cfg(test)]
mod tests {
    use std::{
        io,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering::SeqCst},
        },
    };

    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode},
    };
    use serde_json::{Value, json};
    use silverhand_backend_core::{
        conversation_messaging::messaging::schedule::{ScheduleSpec, ScheduleStatus, ScheduleView},
        foundation::identity::{AgentId, WorkspaceId},
        foundation::persistence::PersistenceFailure,
        identity_access::agent_detail_read::{AgentDetailFacts, AgentDetailProjection},
        identity_access::agent_lifecycle::MaterializationStatus,
        identity_access::agents::runtime::RuntimeKind,
    };
    use tower::ServiceExt;

    use super::*;

    fn user(id: &str) -> UserId {
        UserId::parse(id).expect("user id")
    }

    fn agent(id: &str) -> AgentId {
        AgentId::parse(id).expect("agent id")
    }

    fn projection() -> AgentDetailProjection {
        AgentDetailProjection {
            name: "agent-one".to_owned(),
            display_name: "Base Name".to_owned(),
            runtime: RuntimeKind::Codex,
            model: None,
            reasoning_effort: None,
            fast_mode: None,
            host_id: "h1".to_owned(),
            host_display_label: None,
            host_name: None,
            description: None,
            session_scope: "agent".to_owned(),
            avatar_storage_key: None,
            materialization_failed_code: None,
            materialization_failed_at: None,
            deletion_started_at: None,
            created_at: 1000,
            updated_at: 2000,
        }
    }

    fn facts(creator: &str, viewers: &[&str]) -> AgentDetailFacts {
        AgentDetailFacts::from_source(
            agent("a1"),
            user(creator),
            [],
            viewers.iter().map(|v| user(v)).collect::<Vec<_>>(),
            MaterializationStatus::Active,
            projection(),
        )
    }

    fn cancelled_view() -> ScheduleView {
        ScheduleView {
            id: ScheduleId::parse("s1").expect("schedule id"),
            owner_agent_id: agent("a1"),
            target_conversation_id: "c1".to_owned(),
            target_label: "#ops".to_owned(),
            title: "digest".to_owned(),
            message: "hi".to_owned(),
            spec: ScheduleSpec::Once { fire_at: 100 },
            next_fire_at: 100,
            status: ScheduleStatus::Cancelled,
            fired_count: 0,
            last_fired_at: None,
            max_occurrences: None,
            until_at: None,
            failure_reason: None,
            cancelled_at: Some(9000),
            last_failure_reason: None,
            last_failure_at: None,
            consecutive_failure_count: 0,
            created_at: 1,
            updated_at: 9000,
        }
    }

    #[derive(Clone)]
    enum DetailOutcome {
        Present(Box<AgentDetailFacts>),
        Absent,
        Fails,
    }

    #[derive(Clone)]
    enum CancelScript {
        Outcome(ScheduleCancelOutcome),
        Fails,
    }

    #[derive(Clone)]
    struct FakeReads {
        detail: DetailOutcome,
        cancel: CancelScript,
        cancel_calls: Arc<AtomicUsize>,
        last_owner: Arc<Mutex<Option<AgentId>>>,
    }

    impl FakeReads {
        fn new(detail: DetailOutcome, cancel: CancelScript) -> Self {
            Self {
                detail,
                cancel,
                cancel_calls: Arc::new(AtomicUsize::new(0)),
                last_owner: Arc::new(Mutex::new(None)),
            }
        }
    }

    impl AgentDetailReadModel for FakeReads {
        async fn read_agent_detail(
            &self,
            _agent: &AgentId,
        ) -> Result<Option<AgentDetailFacts>, PersistenceFailure> {
            match &self.detail {
                DetailOutcome::Present(facts) => Ok(Some(facts.as_ref().clone())),
                DetailOutcome::Absent => Ok(None),
                DetailOutcome::Fails => {
                    Err(PersistenceFailure::from_source(io::Error::other("boom")))
                }
            }
        }
    }

    impl ScheduleCancelUnitOfWork for FakeReads {
        async fn cancel_owned_schedule(
            &self,
            _schedule_id: &ScheduleId,
            owner: &AgentId,
        ) -> Result<ScheduleCancelOutcome, PersistenceFailure> {
            self.cancel_calls.fetch_add(1, SeqCst);
            *self.last_owner.lock().unwrap() = Some(owner.clone());
            match &self.cancel {
                CancelScript::Outcome(outcome) => Ok(outcome.clone()),
                CancelScript::Fails => Err(PersistenceFailure::from_source(io::Error::other(
                    "uow boom",
                ))),
            }
        }
    }

    struct Reply {
        status: StatusCode,
        body: String,
        cancel_calls: usize,
    }

    async fn cancel_with(reads: FakeReads, with_principal: bool) -> Reply {
        let cancel_calls = reads.cancel_calls.clone();
        let mut request = Request::post("/api/agents/a1/schedules/s1/cancel");
        if with_principal {
            request = request.extension(user("viewer-1"));
        }
        let response = router(reads.clone(), reads)
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        Reply {
            status,
            body: String::from_utf8(bytes.to_vec()).unwrap(),
            cancel_calls: cancel_calls.load(SeqCst),
        }
    }

    fn parsed(body: &str) -> Value {
        serde_json::from_str::<Value>(body).unwrap()
    }

    fn creator_reads(cancel: CancelScript) -> FakeReads {
        FakeReads::new(
            DetailOutcome::Present(Box::new(facts("viewer-1", &[]))),
            cancel,
        )
    }

    // ---- the ladder runs first; refusals never reach the unit of work ----

    #[tokio::test]
    async fn a_missing_principal_is_a_safe_500_that_never_cancels() {
        let reply = cancel_with(
            creator_reads(CancelScript::Outcome(ScheduleCancelOutcome::Cancelled(
                Box::new(cancelled_view()),
            ))),
            false,
        )
        .await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply.body, "Internal Server Error");
        assert_eq!(reply.cancel_calls, 0);
    }

    #[tokio::test]
    async fn an_absent_agent_is_the_anti_enumeration_404_and_never_cancels() {
        let reply = cancel_with(
            FakeReads::new(
                DetailOutcome::Absent,
                CancelScript::Outcome(ScheduleCancelOutcome::NotFound),
            ),
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        assert_eq!(parsed(&reply.body), json!({ "error": "agent not found" }));
        assert_eq!(reply.cancel_calls, 0);
    }

    #[tokio::test]
    async fn a_non_creator_is_forbidden_and_never_cancels() {
        let reply = cancel_with(
            FakeReads::new(
                DetailOutcome::Present(Box::new(facts("creator-user", &["viewer-1"]))),
                CancelScript::Outcome(ScheduleCancelOutcome::NotFound),
            ),
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN);
        assert_eq!(parsed(&reply.body), json!({ "error": "forbidden" }));
        assert_eq!(reply.cancel_calls, 0);
    }

    #[tokio::test]
    async fn a_gate_read_failure_is_a_safe_500_and_never_cancels() {
        let reply = cancel_with(
            FakeReads::new(
                DetailOutcome::Fails,
                CancelScript::Outcome(ScheduleCancelOutcome::NotFound),
            ),
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply.cancel_calls, 0);
    }

    // ---- outcome → wire mapping (past the gate, owner re-bound) ----

    #[tokio::test]
    async fn a_cancelled_schedule_is_a_200_with_the_post_image_and_re_binds_the_route_agent() {
        let reads = creator_reads(CancelScript::Outcome(ScheduleCancelOutcome::Cancelled(
            Box::new(cancelled_view()),
        )));
        let last_owner = reads.last_owner.clone();
        let reply = cancel_with(reads, true).await;
        assert_eq!(reply.status, StatusCode::OK);
        // The unit of work is handed the ROUTE agent as the expected owner.
        assert_eq!(last_owner.lock().unwrap().clone().unwrap(), agent("a1"));
        let body = parsed(&reply.body);
        assert_eq!(body["schedule"]["id"], "s1");
        assert_eq!(body["schedule"]["status"], "cancelled");
        assert_eq!(body["schedule"]["cancelled_at"], 9000);
        assert_eq!(body["schedule"]["target_label"], "#ops");
    }

    #[tokio::test]
    async fn a_not_found_schedule_is_a_404_with_the_schedule_code() {
        let reply = cancel_with(
            creator_reads(CancelScript::Outcome(ScheduleCancelOutcome::NotFound)),
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        assert_eq!(
            parsed(&reply.body),
            json!({ "error": "schedule s1 not found", "code": "schedule_not_found" })
        );
    }

    #[tokio::test]
    async fn a_schedule_owned_by_another_agent_is_a_403_not_owner() {
        let reply = cancel_with(
            creator_reads(CancelScript::Outcome(ScheduleCancelOutcome::NotOwner)),
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN);
        assert_eq!(
            parsed(&reply.body),
            json!({
                "error": "schedule s1 is owned by another agent",
                "code": "not_owner"
            })
        );
    }

    #[tokio::test]
    async fn a_capability_denial_is_a_403_that_echoes_manage_schedules() {
        let reply = cancel_with(
            creator_reads(CancelScript::Outcome(
                ScheduleCancelOutcome::CapabilityDenied {
                    workspace_id: WorkspaceId::parse("ws-9").expect("workspace id"),
                },
            )),
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN);
        assert_eq!(
            parsed(&reply.body),
            json!({
                "error": "agent a1 lacks manage_schedules in workspace ws-9",
                "code": "capability_denied",
                "capability": "manage_schedules",
                "message": "agent a1 lacks manage_schedules in workspace ws-9",
                "details": { "capability": "manage_schedules" }
            })
        );
    }

    #[tokio::test]
    async fn a_terminal_schedule_is_a_409_naming_its_state() {
        let reply = cancel_with(
            creator_reads(CancelScript::Outcome(
                ScheduleCancelOutcome::NotCancellable {
                    current_status: ScheduleStatus::Done,
                },
            )),
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::CONFLICT);
        assert_eq!(
            parsed(&reply.body),
            json!({
                "error": "schedule s1 is in terminal state done",
                "code": "not_cancellable"
            })
        );
    }

    #[tokio::test]
    async fn a_unit_of_work_failure_is_a_safe_500() {
        let reply = cancel_with(creator_reads(CancelScript::Fails), true).await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply.body, "Internal Server Error");
        assert_eq!(reply.cancel_calls, 1);
    }
}
