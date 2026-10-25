//! Internal agent schedule CANCEL — `POST /internal/agents/{aid}/schedules/cancel`.
//!
//! Cancel is addressed by the schedule's OWN id (`ScheduleCancelInputSchema`
//! carries only `schedule_id`); ownership is re-bound inside the atomic unit of
//! work. Authentication is the internal runtime credential; the acting owner is
//! bound to the path `{aid}` by the shared
//! shared runtime-actor binding gate BEFORE the write —
//! the SAME identity gate the create and list faces use. The cancel itself reuses
//! the existing [`ScheduleCancelUnitOfWork`] authority (the mounted public cancel's
//! port); this face only differs in its door (POST body + runtime credential vs the
//! public path + human viewer).

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use garde::Validate;
use serde::{Deserialize, Serialize};
use serde_json::json;
use silverhand_backend_core::conversation_messaging::messaging::schedule::{
    ScheduleCancelOutcome, ScheduleCancelUnitOfWork, ScheduleId,
};
use silverhand_backend_core::host_runtime::runtime_credential::RuntimePrincipal;

use crate::foundation::http_error::coded_error as schedule_error;
use crate::foundation::wire_admission::{ValidatedJson, ValidatedJsonRejection};
use crate::scheduling::wire::ScheduleDto;

/// The cancel face's mounted path.
pub const SCHEDULE_CANCEL_PATH: &str = "/internal/agents/{aid}/schedules/cancel";

/// The cancel request body (`ScheduleCancelInputSchema`): the target schedule id.
/// Serde owns the structural "is a string" rule; a non-empty check is unnecessary
/// because [`ScheduleId::parse`] refuses an empty id and any unknown id collapses to
/// the same `schedule_not_found` the cancel authority returns.
#[derive(Debug, Deserialize, Validate)]
pub(crate) struct ScheduleCancelBody {
    #[garde(skip)]
    schedule_id: String,
}

/// Everything the cancel door needs.
pub struct ScheduleCancelState<U> {
    pub cancel: U,
}

type Shared<U> = State<Arc<ScheduleCancelState<U>>>;

/// The internal cancel router. Backend-app applies the runtime credential authority
/// and mounts it; this returns it unmounted.
pub fn schedule_cancel_router<U>(state: ScheduleCancelState<U>) -> Router
where
    U: ScheduleCancelUnitOfWork + Send + Sync + 'static,
{
    Router::new()
        .route(SCHEDULE_CANCEL_PATH, post(cancel_route::<U>))
        .with_state(Arc::new(state))
}

async fn cancel_route<U>(
    State(state): Shared<U>,
    Path(aid): Path<String>,
    principal: Option<axum::extract::Extension<RuntimePrincipal>>,
    body: Result<ValidatedJson<ScheduleCancelBody>, ValidatedJsonRejection>,
) -> Response
where
    U: ScheduleCancelUnitOfWork + Send + Sync + 'static,
{
    // IDENTITY GATE FIRST — the shared owner binding; a credential for another
    // agent is refused before the cancel is ever attempted.
    let owner = match crate::host_runtime::runtime_credential::bind_runtime_actor(
        principal.as_ref().map(|extension| &extension.0),
        &aid,
    ) {
        Ok(principal) => principal.agent().clone(),
        Err(refusal) => return refusal.into_response(),
    };

    let body = match body {
        Ok(ValidatedJson(body)) => body,
        Err(rejection) => return rejection.into_response(),
    };

    // An empty id is refused as `schedule_not_found` (parse fails), collapsing with
    // an unknown id: naming a non-existent schedule reveals nothing.
    let raw_schedule_id = body.schedule_id;
    let Ok(schedule_id) = ScheduleId::parse(&raw_schedule_id) else {
        return schedule_error(
            StatusCode::NOT_FOUND,
            format!("schedule {raw_schedule_id} not found"),
            "schedule_not_found",
        );
    };

    match state
        .cancel
        .cancel_owned_schedule(&schedule_id, &owner)
        .await
    {
        Ok(ScheduleCancelOutcome::Cancelled(view)) => (
            StatusCode::OK,
            Json(CancelledBody {
                schedule: ScheduleDto::from(*view),
            }),
        )
            .into_response(),
        Ok(ScheduleCancelOutcome::NotFound) => schedule_error(
            StatusCode::NOT_FOUND,
            format!("schedule {} not found", schedule_id.as_str()),
            "schedule_not_found",
        ),
        Ok(ScheduleCancelOutcome::NotOwner) => schedule_error(
            StatusCode::FORBIDDEN,
            format!(
                "schedule {} is owned by another agent",
                schedule_id.as_str()
            ),
            "not_owner",
        ),
        Ok(ScheduleCancelOutcome::CapabilityDenied { workspace_id }) => capability_denied(format!(
            "agent {} lacks manage_schedules in workspace {}",
            owner.as_str(),
            workspace_id.as_str()
        )),
        Ok(ScheduleCancelOutcome::NotCancellable { current_status }) => schedule_error(
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
                "internal agent-schedules-cancel failed"
            );
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
        }
    }
}

/// `{ schedule: ScheduleDto }` — `ScheduleCancelOutputSchema`.
#[derive(Serialize)]
struct CancelledBody {
    schedule: ScheduleDto,
}

fn capability_denied(message: String) -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({
            "error": message,
            "code": "capability_denied",
            "capability": "manage_schedules",
            "message": message,
            "details": { "capability": "manage_schedules" },
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::extract::Extension;
    use axum::http::Request;
    use serde_json::Value;
    use silverhand_backend_core::conversation_messaging::messaging::schedule::{
        ScheduleStatus, ScheduleView,
    };
    use silverhand_backend_core::foundation::identity::{AgentId, DaemonInstanceId, WorkspaceId};
    use silverhand_backend_core::foundation::persistence::PersistenceFailure;
    use silverhand_backend_core::host_runtime::runtime_credential::{
        RuntimeCustody, RuntimeGeneration, RuntimePrincipal, RuntimeScope,
    };
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;

    fn agent_slot(agent: &str) -> RuntimePrincipal {
        RuntimePrincipal::for_scope(
            RuntimeScope::AgentSlot {
                agent: AgentId::parse(agent).unwrap(),
            },
            RuntimeCustody::new(
                RuntimeGeneration::new(1).unwrap(),
                DaemonInstanceId::parse("daemon-1").unwrap(),
            ),
        )
    }

    fn stub_view() -> ScheduleView {
        ScheduleView {
            id: ScheduleId::parse("sched-1").unwrap(),
            owner_agent_id: AgentId::parse("agent-1").unwrap(),
            target_conversation_id: "conv-1".to_owned(),
            target_label: "#general".to_owned(),
            title: "digest".to_owned(),
            message: "hi".to_owned(),
            spec: silverhand_backend_core::conversation_messaging::messaging::schedule::ScheduleSpec::Once { fire_at: 10_000_000 },
            next_fire_at: 10_000_000,
            status: ScheduleStatus::Cancelled,
            fired_count: 0,
            last_fired_at: None,
            max_occurrences: None,
            until_at: None,
            failure_reason: None,
            cancelled_at: Some(2),
            last_failure_reason: None,
            last_failure_at: None,
            consecutive_failure_count: 0,
            created_at: 1,
            updated_at: 2,
        }
    }

    /// A cancel authority that returns a fixed outcome, counting reaches.
    struct Cancel {
        outcome: fn() -> ScheduleCancelOutcome,
        calls: Arc<AtomicUsize>,
    }
    impl ScheduleCancelUnitOfWork for Cancel {
        async fn cancel_owned_schedule(
            &self,
            _schedule_id: &ScheduleId,
            _owner: &AgentId,
        ) -> Result<ScheduleCancelOutcome, PersistenceFailure> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok((self.outcome)())
        }
    }

    fn app(outcome: fn() -> ScheduleCancelOutcome) -> (Router, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let router = schedule_cancel_router(ScheduleCancelState {
            cancel: Cancel {
                outcome,
                calls: Arc::clone(&calls),
            },
        })
        .layer(Extension(agent_slot("agent-1")));
        (router, calls)
    }

    async fn call_at(app: Router, path_aid: &str, body: &str) -> (StatusCode, Value) {
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/internal/agents/{path_aid}/schedules/cancel"))
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_owned()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    #[tokio::test]
    async fn a_cancelled_schedule_is_200_with_the_post_image() {
        let (app, calls) = app(|| ScheduleCancelOutcome::Cancelled(Box::new(stub_view())));
        let (status, body) = call_at(app, "agent-1", r#"{"schedule_id":"sched-1"}"#).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["schedule"]["status"], "cancelled");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn the_cancel_outcomes_map_to_their_statuses() {
        for (outcome, status, code) in [
            (
                (|| ScheduleCancelOutcome::NotFound) as fn() -> ScheduleCancelOutcome,
                StatusCode::NOT_FOUND,
                "schedule_not_found",
            ),
            (
                || ScheduleCancelOutcome::NotOwner,
                StatusCode::FORBIDDEN,
                "not_owner",
            ),
            (
                || ScheduleCancelOutcome::CapabilityDenied {
                    workspace_id: WorkspaceId::parse("ws-1").unwrap(),
                },
                StatusCode::FORBIDDEN,
                "capability_denied",
            ),
            (
                || ScheduleCancelOutcome::NotCancellable {
                    current_status: ScheduleStatus::Done,
                },
                StatusCode::CONFLICT,
                "not_cancellable",
            ),
        ] {
            let (app, _calls) = app(outcome);
            let (got, body) = call_at(app, "agent-1", r#"{"schedule_id":"sched-1"}"#).await;
            assert_eq!(got, status, "status for {code}");
            assert_eq!(body["code"], code);
        }
    }

    #[tokio::test]
    async fn capability_denial_preserves_the_typed_schedule_error_envelope() {
        let (app, calls) = app(|| ScheduleCancelOutcome::CapabilityDenied {
            workspace_id: WorkspaceId::parse("ws-1").unwrap(),
        });
        let (status, body) = call_at(app, "agent-1", r#"{"schedule_id":"sched-1"}"#).await;
        let message = "agent agent-1 lacks manage_schedules in workspace ws-1";
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            body,
            json!({
                "error": message,
                "code": "capability_denied",
                "capability": "manage_schedules",
                "message": message,
                "details": { "capability": "manage_schedules" }
            })
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn an_empty_schedule_id_collapses_to_not_found_without_reaching_the_authority() {
        let (app, calls) = app(|| ScheduleCancelOutcome::Cancelled(Box::new(stub_view())));
        let (status, body) = call_at(app, "agent-1", r#"{"schedule_id":""}"#).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["code"], "schedule_not_found");
        assert_eq!(calls.load(Ordering::SeqCst), 0, "authority zero-reach");
    }

    #[tokio::test]
    async fn a_credential_for_another_agent_is_unauthorized_with_authority_zero_reach() {
        // The SHARED identity gate: credential agent-1 on path agent-2 -> 401 before
        // the cancel authority is ever reached.
        let (app, calls) = app(|| ScheduleCancelOutcome::Cancelled(Box::new(stub_view())));
        let (status, body) = call_at(app, "agent-2", r#"{"schedule_id":"sched-1"}"#).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"], "unauthorized");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "cancel authority zero-reach"
        );
    }
}
