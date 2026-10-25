//! The `POST /internal/agents/{aid}/schedules` route adapter: the thin HTTP door
//! that sequences the create admission ladder and renders the outcome. It owns NO
//! domain decision — every verdict comes from a port or the core use-case:
//!
//! 1. the internal `RuntimePrincipal` (set by the credential middleware) names the
//!    owner; its absence is a composition fault (`500`), never a silent reinterpret.
//! 2. inbound wire admission ([`super::admission`]) settles the body into a
//!    [`ValidatedScheduleDefinition`]; a structural fault keeps axum's native
//!    `4xx`, a semantic one is the minimal `422 {"error":"invalid input"}`.
//! 3. TARGET RESOLUTION FIRST ([`ScheduleTargetResolver`]): the raw address becomes
//!    a conversation id, or its typed refusal renders at the refusal's own status.
//! 4. CAPABILITY SECOND ([`ScheduleManageCapability`]): `manage_schedules` at the
//!    resolved destination; denial keeps the typed `capability_denied` envelope.
//!    This order is observable — a nonexistent target is answered by step 3 before
//!    capability is ever consulted.
//! 5. the atomic [`ScheduleCreateStore::create`] UoW replays owner/target/
//!    participation/capability/cap under one write lock, then samples its clock,
//!    applies [`ScheduleCreateRequest::plan`], and inserts. Its typed outcome maps
//!    without introducing a second stateful or time authority.
//! 6. only a committed create triggers the near-timer rearm effect; its failure is
//!    logged but cannot rewrite durable success.
//!
//! The route registers nothing here — backend-app composes and mounts it, injecting
//! the production resolver/capability/store. This leaf stays unmounted.

use std::sync::Arc;

use super::admission::{ScheduleCreateBody, ValidatedScheduleDefinition};
use super::ports::{
    ResolvedScheduleTarget, ScheduleManageCapability, ScheduleTargetAllocation,
    ScheduleTargetIntent, ScheduleTargetRefusal, ScheduleTargetResolver,
};
use crate::foundation::http_error::plain_500 as internal_error;
use crate::foundation::wire_admission::ValidatedJson;
use crate::scheduling::wire::ScheduleDto;
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use serde::Serialize;
use silverhand_backend_core::conversation_messaging::messaging::schedule::{
    CronNextOccurrence, ScheduleCreateCommittedEffect, ScheduleCreateGuardError,
    ScheduleCreateOutcome, ScheduleCreateRequest, ScheduleCreateStore,
};
use silverhand_backend_core::host_runtime::runtime_credential::RuntimePrincipal;

/// The create face's mounted path.
pub const SCHEDULES_PATH: &str = "/internal/agents/{aid}/schedules";

/// Everything the create door needs, supplied by whoever composes it.
pub struct ScheduleCreateState<R, K, S, C, E> {
    pub resolver: R,
    pub capability: K,
    pub store: S,
    /// The cron evaluator the core canonicalization uses for a `cron` spec.
    pub cron: C,
    pub committed_effect: E,
}

type Shared<R, K, S, C, E> = State<Arc<ScheduleCreateState<R, K, S, C, E>>>;

/// The create router on the internal path. Backend-app applies the runtime
/// credential authority to it and mounts it; this returns it unmounted.
pub fn schedule_create_router<R, K, S, C, E>(state: ScheduleCreateState<R, K, S, C, E>) -> Router
where
    R: ScheduleTargetResolver + Send + Sync + 'static,
    K: ScheduleManageCapability + Send + Sync + 'static,
    S: ScheduleCreateStore + Send + Sync + 'static,
    C: CronNextOccurrence + Send + Sync + 'static,
    E: ScheduleCreateCommittedEffect + Send + Sync + 'static,
{
    Router::new()
        .route(SCHEDULES_PATH, post(create_route::<R, K, S, C, E>))
        .with_state(Arc::new(state))
}

async fn create_route<R, K, S, C, E>(
    State(state): Shared<R, K, S, C, E>,
    Path(aid): Path<String>,
    principal: Option<axum::extract::Extension<RuntimePrincipal>>,
    body: Result<
        ValidatedJson<ScheduleCreateBody>,
        crate::foundation::wire_admission::ValidatedJsonRejection,
    >,
) -> Response
where
    R: ScheduleTargetResolver + Send + Sync + 'static,
    K: ScheduleManageCapability + Send + Sync + 'static,
    S: ScheduleCreateStore + Send + Sync + 'static,
    C: CronNextOccurrence + Send + Sync + 'static,
    E: ScheduleCreateCommittedEffect + Send + Sync + 'static,
{
    // (1) IDENTITY GATE FIRST: bind the acting owner to the path `{aid}`. A
    // missing credential is a composition fault (500); a credential whose agent is
    // not exactly `{aid}` is `401 unauthorized` — refused before any resolver,
    // capability, or store touch, so A can never act for B. Shared by every
    // schedule face.
    let owner = match crate::host_runtime::runtime_credential::bind_runtime_actor(
        principal.as_ref().map(|extension| &extension.0),
        &aid,
    ) {
        Ok(principal) => principal.agent().clone(),
        Err(refusal) => return refusal.into_response(),
    };

    // (2) Inbound admission. A structural rejection renders axum's native status;
    // a semantic refusal is the minimal 422. Then the TryFrom refine.
    let body = match body {
        Ok(ValidatedJson(body)) => body,
        Err(rejection) => return rejection.into_response(),
    };
    let Ok(definition) = ValidatedScheduleDefinition::try_from(body) else {
        return invalid_input();
    };

    // (3) Resolve the target address FIRST.
    let resolved = match state
        .resolver
        .resolve(
            &owner,
            &definition.target_conversation,
            ScheduleTargetAllocation::Permitted,
            ScheduleTargetIntent::Write,
        )
        .await
    {
        Ok(Ok(target)) => target,
        Ok(Err(refusal)) => return target_refusal(refusal),
        Err(_) => return internal_error(),
    };
    let ResolvedScheduleTarget { conversation } = resolved;

    // (4) Capability SECOND, at the resolved destination.
    match state
        .capability
        .may_manage_schedules(&owner, &conversation)
        .await
    {
        Ok(Ok(())) => {}
        Ok(Err(_denied)) => {
            return crate::conversation_messaging::messaging::agent_capability_wire::capability_denied(
                "manage_schedules",
            );
        }
        Err(_) => return internal_error(),
    }

    // (5) The create UoW owns the single linearization boundary. It replays all
    // stateful gates in product order, then canonicalizes the time policy and
    // inserts before releasing the reserved writer.
    let request = ScheduleCreateRequest {
        owner_agent_id: owner,
        target_conversation_id: conversation,
        title: definition.title,
        message: definition.message,
        spec: definition.spec,
        max_occurrences: definition.max_occurrences,
        until_at: definition.until_at,
        allow_unbounded: definition.allow_unbounded,
    };
    let owner_id = request.owner_agent_id.as_str().to_owned();
    let target_id = request.target_conversation_id.as_str().to_owned();
    match state.store.create(request, &state.cron).await {
        Ok(ScheduleCreateOutcome::Created(view)) => {
            if let Err(error) = state.committed_effect.rearm_near_timer().await {
                tracing::error!(
                    error = crate::foundation::safe_log_field::safe_display(&(error)),
                    "schedule create committed but near-timer rearm failed"
                );
            }
            (
                StatusCode::OK,
                Json(CreatedBody {
                    schedule: ScheduleDto::from(*view),
                }),
            )
                .into_response()
        }
        Ok(ScheduleCreateOutcome::CapExceeded { active }) => schedule_error(
            StatusCode::TOO_MANY_REQUESTS,
            "too_many_active_schedules",
            format!("agent has {active} active schedules (cap 50)"),
        ),
        Ok(ScheduleCreateOutcome::TargetNotFound) => schedule_error(
            StatusCode::NOT_FOUND,
            "target_conversation_not_found",
            format!("target conversation {target_id} not found"),
        ),
        Ok(ScheduleCreateOutcome::TargetClosed) => schedule_error(
            StatusCode::CONFLICT,
            "target_conversation_closed",
            format!("target conversation {target_id} is closed"),
        ),
        Ok(ScheduleCreateOutcome::ActorNotParticipant) => schedule_error(
            StatusCode::FORBIDDEN,
            "agent_not_in_target_conversation",
            format!("agent {owner_id} is not a participant of target conversation {target_id}"),
        ),
        Ok(ScheduleCreateOutcome::CapabilityDenied) => {
            crate::conversation_messaging::messaging::agent_capability_wire::capability_denied(
                "manage_schedules",
            )
        }
        Ok(ScheduleCreateOutcome::AgentNotFound) => schedule_error(
            StatusCode::NOT_FOUND,
            "agent_not_found",
            format!("agent {owner_id} not found"),
        ),
        Ok(ScheduleCreateOutcome::Invalid(guard)) => guard_refusal(guard),
        Err(_) => internal_error(),
    }
}

/// `{ schedule: ScheduleDto }` — the create output shape (`ScheduleCreateOutputSchema`).
#[derive(Serialize)]
struct CreatedBody {
    schedule: ScheduleDto,
}

/// A typed `{ error, code }` schedule refusal, mirroring `scheduleServiceErrorResponse`.
#[derive(Serialize)]
struct ScheduleErrorBody {
    error: String,
    code: &'static str,
}

/// The stateless-guard `ScheduleError` code + status, mirroring `scheduleErrorStatusMap`.
fn guard_refusal(guard: ScheduleCreateGuardError) -> Response {
    let code = match guard {
        ScheduleCreateGuardError::FireAtTooSoon => "fire_at_too_soon",
        ScheduleCreateGuardError::FireAtTooFar => "fire_at_too_far",
        ScheduleCreateGuardError::StartAtTooFarPast => "fire_at_too_soon",
        ScheduleCreateGuardError::UnboundedRepeatRequiresExplicitLimit => {
            "unbounded_repeat_requires_explicit_limit"
        }
        ScheduleCreateGuardError::UntilAtNotFuture
        | ScheduleCreateGuardError::UntilAtBeforeFirstFire => "invalid_input",
        ScheduleCreateGuardError::InvalidTimezone(_) => "invalid_timezone",
        ScheduleCreateGuardError::InvalidCronExpression(_) => "invalid_cron_expression",
        ScheduleCreateGuardError::IntervalBelowMinimum => "invalid_input",
    };
    // Every stateless guard maps to 400 in `scheduleErrorStatusMap`.
    schedule_error(StatusCode::BAD_REQUEST, code, guard.to_string())
}

/// Render a target-resolution refusal at the SAME status the TS delivery-target
/// route uses for the schedule-create door.
fn target_refusal(refusal: ScheduleTargetRefusal) -> Response {
    use crate::conversation_messaging::messaging::target_refusal_wire::TargetRefusalWire;
    match refusal {
        ScheduleTargetRefusal::Malformed => TargetRefusalWire::Malformed,
        ScheduleTargetRefusal::SelfPairNotAddressable => TargetRefusalWire::SelfPairNotAddressable,
        ScheduleTargetRefusal::Unresolved => TargetRefusalWire::Unresolved,
        ScheduleTargetRefusal::ThreadNotInTarget => TargetRefusalWire::ThreadNotInTarget,
        ScheduleTargetRefusal::ThreadSuffixUnsupported => {
            TargetRefusalWire::ThreadSuffixUnsupported
        }
        ScheduleTargetRefusal::SlotOutOfScope => TargetRefusalWire::SlotOutOfScope,
    }
    .into_response()
}

fn schedule_error(status: StatusCode, code: &'static str, error: String) -> Response {
    (status, Json(ScheduleErrorBody { error, code })).into_response()
}

/// The stateless-semantic admission refusal: minimal `422 {"error":"invalid input"}`.
fn invalid_input() -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(InvalidInputBody {
            error: "invalid input",
        }),
    )
        .into_response()
}

#[derive(Serialize)]
struct InvalidInputBody {
    error: &'static str,
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::extract::Extension;
    use axum::http::Request;
    use serde_json::{Value, json};
    use silverhand_backend_core::conversation_messaging::messaging::schedule::{
        CronOutcome, CronQuery, ScheduleView,
    };
    use silverhand_backend_core::foundation::identity::{
        AgentId, ConversationId, DaemonInstanceId,
    };
    use silverhand_backend_core::foundation::persistence::PersistenceFailure;
    use silverhand_backend_core::host_runtime::runtime_credential::{
        RuntimeCustody, RuntimeGeneration, RuntimePrincipal, RuntimeScope,
    };
    use std::convert::Infallible;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;

    const AGENT: &str = "agent-1";
    const CONVERSATION: &str = "conv-1";

    fn agent_slot() -> RuntimePrincipal {
        RuntimePrincipal::for_scope(
            RuntimeScope::AgentSlot {
                agent: AgentId::parse(AGENT).unwrap(),
            },
            RuntimeCustody::new(
                RuntimeGeneration::new(1).unwrap(),
                DaemonInstanceId::parse("daemon-1").unwrap(),
            ),
        )
    }

    /// A resolver that always resolves to `CONVERSATION`, or always refuses.
    struct Resolver {
        refuse: Option<ScheduleTargetRefusal>,
        calls: Arc<AtomicUsize>,
    }
    impl ScheduleTargetResolver for Resolver {
        type Error = Infallible;
        async fn resolve(
            &self,
            _agent: &AgentId,
            _address: &str,
            _allocation: ScheduleTargetAllocation,
            _intent: ScheduleTargetIntent,
        ) -> Result<Result<ResolvedScheduleTarget, ScheduleTargetRefusal>, Self::Error> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(match self.refuse {
                Some(refusal) => Err(refusal),
                None => Ok(ResolvedScheduleTarget {
                    conversation: ConversationId::parse(CONVERSATION).unwrap(),
                }),
            })
        }
    }

    /// A capability port that grants or denies, counting consultations.
    struct Capability {
        grant: bool,
        calls: Arc<AtomicUsize>,
    }
    impl ScheduleManageCapability for Capability {
        type Error = Infallible;
        async fn may_manage_schedules(
            &self,
            _agent: &AgentId,
            _destination: &ConversationId,
        ) -> Result<Result<(), super::super::ports::ScheduleCapabilityDenied>, Self::Error>
        {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(if self.grant {
                Ok(())
            } else {
                Err(super::super::ports::ScheduleCapabilityDenied)
            })
        }
    }

    /// A store that records whether it was reached, always creating a stub row.
    struct Store {
        calls: Arc<AtomicUsize>,
    }
    impl ScheduleCreateStore for Store {
        async fn create<C>(
            &self,
            request: ScheduleCreateRequest,
            cron: &C,
        ) -> Result<ScheduleCreateOutcome, PersistenceFailure>
        where
            C: CronNextOccurrence + Sync,
        {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match request.plan(1_000, cron) {
                Ok(plan) => Ok(ScheduleCreateOutcome::Created(Box::new(stub_view(plan)))),
                Err(error) => Ok(ScheduleCreateOutcome::Invalid(error)),
            }
        }
    }

    struct Effect {
        calls: Arc<AtomicUsize>,
        fail: bool,
    }
    impl ScheduleCreateCommittedEffect for Effect {
        async fn rearm_near_timer(&self) -> Result<(), PersistenceFailure> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                Err(PersistenceFailure::from_source(std::io::Error::other(
                    "timer unavailable",
                )))
            } else {
                Ok(())
            }
        }
    }

    fn stub_view(
        plan: silverhand_backend_core::conversation_messaging::messaging::schedule::ScheduleCreatePlan,
    ) -> ScheduleView {
        ScheduleView {
            id: silverhand_backend_core::conversation_messaging::messaging::schedule::ScheduleId::parse("sched-1").unwrap(),
            owner_agent_id: plan.owner_agent_id.clone(),
            target_conversation_id: plan.target_conversation_id.as_str().to_owned(),
            target_label: "#general".to_owned(),
            title: plan.title.as_str().to_owned(),
            message: plan.message.as_str().to_owned(),
            spec: plan.spec.clone(),
            next_fire_at: plan.next_fire_at,
            status: silverhand_backend_core::conversation_messaging::messaging::schedule::ScheduleStatus::Pending,
            fired_count: 0,
            last_fired_at: None,
            max_occurrences: plan.caps.max_occurrences(),
            until_at: plan.caps.until_at(),
            failure_reason: None,
            cancelled_at: None,
            last_failure_reason: None,
            last_failure_at: None,
            consecutive_failure_count: 0,
            created_at: 1,
            updated_at: 1,
        }
    }

    struct NoCron;
    impl CronNextOccurrence for NoCron {
        fn next_after(&self, _query: CronQuery<'_>) -> CronOutcome {
            CronOutcome::NoFutureOccurrence
        }
    }

    struct Counts {
        resolver: Arc<AtomicUsize>,
        capability: Arc<AtomicUsize>,
        store: Arc<AtomicUsize>,
        effect: Arc<AtomicUsize>,
    }

    fn app(refuse: Option<ScheduleTargetRefusal>, grant: bool) -> (Router, Counts) {
        app_with_effect(refuse, grant, false)
    }

    fn app_with_effect(
        refuse: Option<ScheduleTargetRefusal>,
        grant: bool,
        fail_effect: bool,
    ) -> (Router, Counts) {
        let counts = Counts {
            resolver: Arc::new(AtomicUsize::new(0)),
            capability: Arc::new(AtomicUsize::new(0)),
            store: Arc::new(AtomicUsize::new(0)),
            effect: Arc::new(AtomicUsize::new(0)),
        };
        let state = ScheduleCreateState {
            resolver: Resolver {
                refuse,
                calls: Arc::clone(&counts.resolver),
            },
            capability: Capability {
                grant,
                calls: Arc::clone(&counts.capability),
            },
            store: Store {
                calls: Arc::clone(&counts.store),
            },
            cron: NoCron,
            committed_effect: Effect {
                calls: Arc::clone(&counts.effect),
                fail: fail_effect,
            },
        };
        let router = schedule_create_router(state).layer(Extension(agent_slot()));
        (router, counts)
    }

    fn once_body() -> String {
        r#"{"title":"digest","message":"hi","spec":{"kind":"once","fire_at":10000000},"target_conversation":"dm:@bob"}"#.to_owned()
    }

    async fn call(app: Router, body: String) -> (StatusCode, Value) {
        call_at(app, "agent-1", body).await
    }

    async fn call_at(app: Router, path_aid: &str, body: String) -> (StatusCode, Value) {
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/internal/agents/{path_aid}/schedules"))
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

    #[tokio::test]
    async fn manage_schedules_grant_creates_and_rearms_timer() {
        let (app, counts) = app(None, true);
        let (status, body) = call(app, once_body()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["schedule"]["status"], "pending");
        assert_eq!(counts.resolver.load(Ordering::SeqCst), 1);
        assert_eq!(counts.capability.load(Ordering::SeqCst), 1);
        assert_eq!(counts.store.load(Ordering::SeqCst), 1);
        assert_eq!(counts.effect.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_failed_timer_rearm_never_rewrites_the_committed_schedule_success() {
        let (app, counts) = app_with_effect(None, true, true);
        let (status, body) = call(app, once_body()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["schedule"]["id"], "sched-1");
        assert_eq!(counts.store.load(Ordering::SeqCst), 1);
        assert_eq!(counts.effect.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn manage_schedules_denial_refuses_without_store_or_effect() {
        // Resolution succeeds; the manage_schedules capability is denied. The store
        // is NEVER reached — zero-reach — and the answer is 403 forbidden.
        let (app, counts) = app(None, false);
        let (status, body) = call(app, once_body()).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            body,
            json!({
                "code": "capability_denied",
                "capability": "manage_schedules",
                "message": "missing 'manage_schedules' capability in this workspace",
                "details": { "capability": "manage_schedules" }
            })
        );
        assert_eq!(counts.resolver.load(Ordering::SeqCst), 1);
        assert_eq!(counts.capability.load(Ordering::SeqCst), 1);
        assert_eq!(counts.store.load(Ordering::SeqCst), 0, "store zero-reach");
        assert_eq!(counts.effect.load(Ordering::SeqCst), 0, "effect zero-reach");
    }

    #[tokio::test]
    async fn all_target_refusals_preserve_the_canonical_wire_and_zero_reach() {
        // Resolve-before-capability ordering + anti-enumeration: every resolver
        // refusal is projected byte-for-byte from the shared delivery-target
        // descriptor. No later authority, persistence, or committed effect runs.
        for (refusal, expected_status, expected_body) in [
            (
                ScheduleTargetRefusal::Malformed,
                StatusCode::BAD_REQUEST,
                json!({"error":"target is not an address","code":"target_malformed"}),
            ),
            (
                ScheduleTargetRefusal::SelfPairNotAddressable,
                StatusCode::BAD_REQUEST,
                json!({"error":"a direct message needs another participant","code":"self_pair_not_addressable"}),
            ),
            (
                ScheduleTargetRefusal::Unresolved,
                StatusCode::NOT_FOUND,
                json!({"error":"target names no stream you can use","code":"target_unresolved"}),
            ),
            (
                ScheduleTargetRefusal::ThreadNotInTarget,
                StatusCode::NOT_FOUND,
                json!({"error":"target names no reply cluster of that conversation","code":"thread_not_in_target"}),
            ),
            (
                ScheduleTargetRefusal::ThreadSuffixUnsupported,
                StatusCode::BAD_REQUEST,
                json!({"error":"this operation addresses a conversation, not a reply cluster","code":"conversation_target_suffix"}),
            ),
            (
                ScheduleTargetRefusal::SlotOutOfScope,
                StatusCode::FORBIDDEN,
                json!({"error":"this operation acts only in the conversation your session is anchored to","code":"slot_out_of_scope"}),
            ),
        ] {
            let (app, counts) = app(Some(refusal), true);
            let (status, body) = call(app, once_body()).await;
            assert_eq!(status, expected_status);
            assert_eq!(body, expected_body);
            assert_eq!(counts.resolver.load(Ordering::SeqCst), 1);
            assert_eq!(counts.capability.load(Ordering::SeqCst), 0);
            assert_eq!(counts.store.load(Ordering::SeqCst), 0);
            assert_eq!(counts.effect.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    async fn a_credential_for_another_agent_is_unauthorized_with_full_zero_reach() {
        // Identity cross-wire gate: the credential is agent-1 (see `agent_slot`),
        // but the path names agent-2. The exact-match binding refuses BEFORE the
        // admission, resolver, capability, and store are touched — A can never act
        // for B — with 401 unauthorized, matching the TS verifier's agent_mismatch.
        let (app, counts) = app(None, true);
        let (status, body) = call_at(app, "agent-2", once_body()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"], "unauthorized");
        assert_eq!(
            counts.resolver.load(Ordering::SeqCst),
            0,
            "resolver zero-reach on identity mismatch"
        );
        assert_eq!(
            counts.capability.load(Ordering::SeqCst),
            0,
            "capability zero-reach on identity mismatch"
        );
        assert_eq!(
            counts.store.load(Ordering::SeqCst),
            0,
            "store zero-reach on identity mismatch"
        );
    }

    #[tokio::test]
    async fn a_stateless_guard_failure_is_the_existing_400_schedule_error_taxonomy_not_a_422() {
        // POSITIVE CONTROL for guard -> 400: `fire_at` equal to `now` is inside the
        // 30s minimum delay, so the core canonicalization refuses with the EXISTING
        // public `ScheduleError` code `fire_at_too_soon`, which `scheduleErrorStatusMap`
        // maps to 400 — NOT the Mandatory-standard default 422. The create UoW is
        // reached because stateful gates precede the in-lock policy clock/plan.
        let (app, counts) = app(None, true);
        let body = r#"{"title":"digest","message":"hi","spec":{"kind":"once","fire_at":1000},"target_conversation":"dm:@bob"}"#.to_owned();
        let (status, value) = call(app, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(value["code"], "fire_at_too_soon");
        assert_eq!(counts.store.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn all_six_wire_numeric_bounds_refuse_before_resolver_capability_store_and_effect() {
        let invalid = [
            r#"{"title":"d","message":"m","spec":{"kind":"once","fire_at":0},"target_conversation":"dm:@bob"}"#,
            r#"{"title":"d","message":"m","spec":{"kind":"once","delay_seconds":0},"target_conversation":"dm:@bob"}"#,
            r#"{"title":"d","message":"m","spec":{"kind":"interval","every_seconds":59},"allow_unbounded":true,"target_conversation":"dm:@bob"}"#,
            r#"{"title":"d","message":"m","spec":{"kind":"interval","every_seconds":60,"start_at":0},"allow_unbounded":true,"target_conversation":"dm:@bob"}"#,
            r#"{"title":"d","message":"m","spec":{"kind":"interval","every_seconds":60},"allow_unbounded":true,"until_at":0,"target_conversation":"dm:@bob"}"#,
            r#"{"title":"d","message":"m","spec":{"kind":"interval","every_seconds":60},"max_occurrences":0,"target_conversation":"dm:@bob"}"#,
            r#"{"title":"d","message":"m","spec":{"kind":"once","fire_at":9007199254740992},"target_conversation":"dm:@bob"}"#,
            r#"{"title":"d","message":"m","spec":{"kind":"once","delay_seconds":9007199254740992},"target_conversation":"dm:@bob"}"#,
            r#"{"title":"d","message":"m","spec":{"kind":"interval","every_seconds":9007199254740992},"allow_unbounded":true,"target_conversation":"dm:@bob"}"#,
            r#"{"title":"d","message":"m","spec":{"kind":"interval","every_seconds":60,"start_at":9007199254740992},"allow_unbounded":true,"target_conversation":"dm:@bob"}"#,
            r#"{"title":"d","message":"m","spec":{"kind":"interval","every_seconds":60},"allow_unbounded":true,"until_at":9007199254740992,"target_conversation":"dm:@bob"}"#,
            r#"{"title":"d","message":"m","spec":{"kind":"interval","every_seconds":60},"max_occurrences":9007199254740992,"target_conversation":"dm:@bob"}"#,
        ];
        for body in invalid {
            let (app, counts) = app(None, true);
            let (status, value) = call(app, body.to_owned()).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
            assert_eq!(value, serde_json::json!({"error":"invalid input"}));
            assert_eq!(counts.resolver.load(Ordering::SeqCst), 0, "{body}");
            assert_eq!(counts.capability.load(Ordering::SeqCst), 0, "{body}");
            assert_eq!(counts.store.load(Ordering::SeqCst), 0, "{body}");
            assert_eq!(counts.effect.load(Ordering::SeqCst), 0, "{body}");
        }
    }

    #[tokio::test]
    async fn all_six_optional_nonnullable_fields_accept_absence_but_reject_null_zero_reach() {
        let absent_positive_controls = [
            once_body(),
            r#"{"title":"d","message":"m","spec":{"kind":"once","delay_seconds":60},"target_conversation":"dm:@bob"}"#.to_owned(),
            r#"{"title":"d","message":"m","spec":{"kind":"interval","every_seconds":60},"allow_unbounded":true,"target_conversation":"dm:@bob"}"#.to_owned(),
        ];
        for body in absent_positive_controls {
            let (app, counts) = app(None, true);
            let (status, _) = call(app, body).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(counts.store.load(Ordering::SeqCst), 1);
        }

        let present_null = [
            r#"{"title":"d","message":"m","spec":{"kind":"once","fire_at":null,"delay_seconds":60},"target_conversation":"dm:@bob"}"#,
            r#"{"title":"d","message":"m","spec":{"kind":"once","fire_at":10000000,"delay_seconds":null},"target_conversation":"dm:@bob"}"#,
            r#"{"title":"d","message":"m","spec":{"kind":"interval","every_seconds":60,"start_at":null},"allow_unbounded":true,"target_conversation":"dm:@bob"}"#,
            r#"{"title":"d","message":"m","spec":{"kind":"once","fire_at":10000000},"until_at":null,"target_conversation":"dm:@bob"}"#,
            r#"{"title":"d","message":"m","spec":{"kind":"once","fire_at":10000000},"max_occurrences":null,"target_conversation":"dm:@bob"}"#,
            r#"{"title":"d","message":"m","spec":{"kind":"once","fire_at":10000000},"allow_unbounded":null,"target_conversation":"dm:@bob"}"#,
        ];
        for body in present_null {
            let (app, counts) = app(None, true);
            let (status, _value) = call(app, body.to_owned()).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
            assert_eq!(counts.resolver.load(Ordering::SeqCst), 0, "{body}");
            assert_eq!(counts.capability.load(Ordering::SeqCst), 0, "{body}");
            assert_eq!(counts.store.load(Ordering::SeqCst), 0, "{body}");
            assert_eq!(counts.effect.load(Ordering::SeqCst), 0, "{body}");
        }
    }

    #[tokio::test]
    async fn cron_wire_bounds_count_utf16_before_any_downstream_authority() {
        for (field, value) in [
            ("expression", "😀".repeat(101)),
            ("timezone", "😀".repeat(33)),
        ] {
            let (app, counts) = app(None, true);
            let mut cron = serde_json::json!({
                "kind": "cron",
                "expression": "0 9 * * *",
                "timezone": "UTC"
            });
            cron[field] = Value::String(value);
            let body = serde_json::json!({
                "title": "d",
                "message": "m",
                "spec": cron,
                "max_occurrences": 1,
                "target_conversation": "dm:@bob"
            })
            .to_string();
            let (status, value) = call(app, body).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{field}");
            assert_eq!(value, serde_json::json!({"error":"invalid input"}));
            assert_eq!(counts.resolver.load(Ordering::SeqCst), 0, "{field}");
            assert_eq!(counts.capability.load(Ordering::SeqCst), 0, "{field}");
            assert_eq!(counts.store.load(Ordering::SeqCst), 0, "{field}");
            assert_eq!(counts.effect.load(Ordering::SeqCst), 0, "{field}");
        }
    }
}
