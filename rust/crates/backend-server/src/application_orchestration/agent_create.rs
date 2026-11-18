//! Authenticated create face for `POST /api/agents`: the public agent-provisioning
//! saga — reserve → materialize on the host → provision the dependent graph — behind
//! a wire-admission gate, wrapped around the core [`create_agent`] unit of work.
//!
//! ## The ladder (parity with `application/create-agent.ts` + `routes/agents.ts`)
//!
//!  1. PRINCIPAL — the human principal is proof the authenticating perimeter ran; its
//!     ABSENCE is a composition fault, not a product request, so it takes a fail-closed
//!     `500` reading nothing. Create has NO visibility/authority ladder: any
//!     authenticated caller may create, and the principal IS the creator. Mounting the
//!     real bearer perimeter is the composition root's job — done by the backend-app
//!     #2234 CP6 mount. Framework body extraction precedes this composition check.
//!  2. WIRE ADMISSION (body) — parse against `CreateAgentInputSchema`: the handle
//!     grammar, the runtime enum, the reasoning-effort token shape AND its
//!     runtime tuning policy, the service-tier runtime allowlist, and the session-scope
//!     enum. Structural failures keep native JSON `400/422/415`; semantic rule
//!     failures return `422 {"error":"invalid input"}` without an issues array.
//!     `host_id: ""` is FALSY in the TS
//!     placement check, so an empty host maps to `None` (default placement), never a
//!     named host.
//!  3. WIRE ADMISSION (Idempotency-Key header, design §14 Q1) — the header MUST be
//!     present and match `/^[A-Za-z0-9_-]{1,64}$/`. Absent/empty → `400`
//!     `idempotency_key_missing`; present-but-malformed → `400`
//!     `idempotency_key_malformed`. Body admission precedes key admission, matching TS.
//!  4. UNIT OF WORK — hand the admitted input, key, and `?retry=true` flag to the core
//!     [`create_agent`], which owns the whole saga: idempotency dispatch, in-core
//!     placement (returning a typed `RuntimePlacementRefusal`, mapped through the ONE
//!     canonical [`placement_refusal`] authority — an improvement over the TS
//!     error-sniffing), the handle gate, the reserve INSERT, the capture-then-dispatch
//!     host materialize (the attachment gate and the command speak ONE truth about which
//!     daemon holds the host), and the flip-active + dependent-graph provisioning
//!     transaction. It returns a sealed [`CreateAgentOutcome`] or a safe-`500`
//!     [`CreateAgentFault`].
//!  5. RESPOND + EFFECTS — map the outcome to its exact HTTP body; on the `Created`
//!     arm (and ONLY it) reproject the committed agent through the canonical
//!     `agent_detail` projector and fire the best-effort post-commit effects in the TS
//!     order. `Echo` reprojects too but fires nothing (the effects ran on the original
//!     creation). Every fault is a detail-free safe `500`.
//!
//! ## Post-commit effects (parity with `buildPostCommitEffects`)
//!
//! Fired ONLY on `Created`, best-effort (a failure is logged, never flips the `201`).
//! The TS order is `[direct_pairs, announce, deliver, start]`; the split here is by
//! blast radius, not by ceremony:
//!
//!  - `direct_pairs_changed(creator, direct_line)` — agent-scoped arm ONLY, and ONLY
//!    when the creator↔agent pair was freshly minted (`pair_created`). A resolved
//!    existing line is not new to the creator, so no edge fires. An in-process
//!    broadcast: awaited inline (it cannot block on remote IO, and its order is
//!    observable).
//!  - `agent_changed(agent)` — the app-scope announce, also an in-process broadcast,
//!    awaited inline.
//!  - `deliver(destination, agent)` — wake the agent on the onboarding conversation.
//!  - `start(destination, agent)` — the runtime auto-start.
//!
//! `deliver` and `start` are remote daemon IO: each is handed to the owned aftermath
//! boundary ([`AftermathDispatch`]) as its OWN independent task, so the `201` is sealed
//! before either touches the host and one leg's panic or fault can never swallow the
//! other. The order matches TS: the agent arm fires the direct-pairs edge first; the
//! conversation arm skips it.
//!
//! ## Named divergences, reachability-priced (flagged for review)
//!
//!  - **`materializeFailed` message is synthesized per-code.** The core carries only
//!    the failure `code`, not the raw driver message TS passes through verbatim, so
//!    this route synthesizes a stable per-code string. No client branches on the
//!    message (the `code` is the actionable signal); the status map is reproduced
//!    exactly.
//!  - **A `MaterializeTransport` fault is a safe `500` (row left pending).** The core
//!    classifies only host REFUSALS into `MaterializeFailed` codes; a transport break
//!    is a fault and the row is left pending for the boot sweep, rather than TS's
//!    transport→`host_attach_pending`/`materialize_timeout` classification + flip.
//!  - **`corruptGraph` is a detail-free bare-text `500`**, not the TS JSON
//!    `{error:message, code:'internal_error'}`. All faults share the lane's bare `500`.
//!  - **`NameTaken` folds the two TS messages into one** fixed `agent name already
//!    exists` (code `agent_name_taken`), since the core carries no message.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Extension, RawQuery, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use garde::Validate;
use serde::{Deserialize, Serialize};
use serde_json::json;
use silverhand_backend_core::{
    application_orchestration::agent_creation::{
        AgentGraphRead, CreateAgentFault, CreateAgentInput, CreateAgentOutcome, CreateAgentPorts,
        CreatedAgent, CreatedArm, CreatedCoordinates, EchoedAgent, MaterializeFailureCode,
        create_agent,
    },
    application_orchestration::agent_materialization::{
        AgentArmProvisioningPort, AgentMaterializationLookup, AgentMaterializationWrite,
        IdempotencyKey,
    },
    conversation_messaging::messaging::{
        MessageId,
        environment::SendEntropy,
        post_commit::{EffectFailure, RuntimeStart},
    },
    foundation::identity::{AgentId, ConversationId, HostId, UserId, WorkspaceId},
    host_runtime::agent_materialize::AgentMaterializePort,
    host_runtime::read::ComputerReadModel,
    identity_access::agent_detail_read::AgentDetailReadModel,
    identity_access::agent_profile_write::AgentChangedSink,
    identity_access::agents::{
        runtime::RuntimeKind,
        runtime_capability::{
            is_reasoning_effort_token, reasoning_effort_valid_for_runtime,
            service_tier_valid_for_runtime,
        },
        session_scope::AgentSessionScope,
    },
    identity_access::user_identity::ParticipantHandle,
};

use crate::conversation_messaging::messaging::collaboration::runtime_control::placement_refusal;
use crate::foundation::clock::ProcessClock;
use crate::foundation::error;
use crate::foundation::wire_admission::{self, ValidatedJson};
use crate::foundation::wire_patch::RequiredPatchField;
use crate::identity_access::agent_wire::{AgentDto, reproject_committed_agent};

/// The product route template. `POST` on the collection; the composition root mounts it
/// alongside the `GET` list on the same path at mount time.
pub const AGENT_CREATE_PATH: &str = "/api/agents";

// ===========================================================================
// The two route-local post-commit effect ports
// ===========================================================================

/// Deliver the onboarding message to the freshly created agent on its destination
/// conversation, mirroring the reference `deliverMessage(conversation, agent, messageId)`:
/// FIRST ensure the durable `conversation_inboxes` row that binds this message to this
/// conversation for this agent (idempotent, sealed by the agent's canonical participation
/// in that conversation), THEN wake the agent so it drains its inbox. The onboarding
/// message id is a load-bearing delivery parameter, not dead payload: the runtime does NOT
/// read a committed inbox on boot (`runtime_start` is pure lifecycle) — delivery custody
/// flows solely through this enqueue-then-wake, so without the durable row the wake finds
/// nothing pending and the agent is never handed its onboarding turn.
///
/// It is a best-effort post-commit effect: the `201` is already sealed, so a failure here
/// (participation not witnessed, transport down) is logged and dropped, never rolled back
/// into the provisioning transaction. No core trait fits the two-step ensure-then-wake, so
/// the create face declares its narrow view; the production adapter composes the single
/// durable inbox-ensure authority with the daemon wake at mount.
pub trait OnboardingDelivery: Clone + Send + Sync + 'static {
    fn deliver(
        &self,
        conversation: &ConversationId,
        agent: &AgentId,
        onboarding_message: &MessageId,
    ) -> impl Future<Output = Result<(), EffectFailure>> + Send;
}

/// Publish the creator's direct-pairs invalidation when a fresh creator↔agent pair
/// opened. No core trait exists for this (only the concrete hub method), so the create
/// face declares its narrow view; the production adapter is a named seam at mount.
pub trait DirectPairsChanged: Clone + Send + Sync + 'static {
    fn direct_pairs_changed(
        &self,
        viewer: &UserId,
        conversation: &ConversationId,
    ) -> impl Future<Output = Result<(), EffectFailure>> + Send;
}

/// One owned, best-effort post-commit leg. It awaits its remote effect and logs any
/// failure internally, so it never yields an error that could abort a sibling leg.
pub type AftermathLeg = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// Hand a structurally-independent post-commit leg to the application's owned
/// lifecycle/shutdown boundary. The create response is already sealed before dispatch,
/// so a leg that blocks on remote daemon IO (`deliver`, `start`) never delays the `201`.
/// Each leg is handed off separately and the boundary owns it (tracked, cancelled on
/// shutdown), so one leg's panic or fault is isolated and cannot swallow another. This is
/// NOT an owner-less detached `spawn`: the production adapter is a named seam at mount
/// that plugs into the real task tracker.
pub trait AftermathDispatch: Clone + Send + Sync + 'static {
    fn dispatch(&self, leg: AftermathLeg);
}

// ===========================================================================
// Router + state
// ===========================================================================

#[allow(clippy::type_complexity)]
struct StateInner<C, H, M, R, V, G, E, RD, S, RS, OD, DP, AD, CL> {
    computers: C,
    handles: H,
    materialization: M,
    materialize: R,
    provisioning: V,
    graph: G,
    entropy: E,
    reads: RD,
    sink: S,
    runtime_start: RS,
    onboarding: OD,
    direct_pairs: DP,
    aftermath: AD,
    clock: CL,
}

#[allow(clippy::type_complexity)]
struct CreateState<C, H, M, R, V, G, E, RD, S, RS, OD, DP, AD, CL>(
    Arc<StateInner<C, H, M, R, V, G, E, RD, S, RS, OD, DP, AD, CL>>,
);

impl<C, H, M, R, V, G, E, RD, S, RS, OD, DP, AD, CL> Clone
    for CreateState<C, H, M, R, V, G, E, RD, S, RS, OD, DP, AD, CL>
{
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

/// Compose the create face over its ports: the seven core authorities the
/// [`create_agent`] unit of work drives, the detail read model the post-commit
/// reprojection rides, the four post-commit effect sinks, and the aftermath boundary the
/// two remote legs are handed to. Every port is injected; mounting the real adapters
/// (and, for [`OnboardingDelivery`]/[`DirectPairsChanged`]/[`AftermathDispatch`], the
/// named production seams) is the composition root's job.
#[allow(clippy::too_many_arguments)]
pub fn router<C, H, M, R, V, G, E, RD, S, RS, OD, DP, AD, CL>(
    computers: C,
    handles: H,
    materialization: M,
    materialize: R,
    provisioning: V,
    graph: G,
    entropy: E,
    reads: RD,
    sink: S,
    runtime_start: RS,
    onboarding: OD,
    direct_pairs: DP,
    aftermath: AD,
    clock: CL,
) -> Router
where
    C: ComputerReadModel,
    H: AgentMaterializationLookupHandles,
    M: AgentMaterializationLookup + AgentMaterializationWrite + Send + Sync + 'static,
    R: AgentMaterializePort,
    V: AgentArmProvisioningPort + Send + Sync + 'static,
    G: AgentGraphRead + Send + Sync + 'static,
    E: SendEntropy + Send + Sync + 'static,
    RD: AgentDetailReadModel + Send + Sync + 'static,
    S: AgentChangedSink,
    RS: RuntimeStart + Clone + Send + Sync + 'static,
    OD: OnboardingDelivery,
    DP: DirectPairsChanged,
    AD: AftermathDispatch,
    CL: ProcessClock,
{
    let state = CreateState(Arc::new(StateInner {
        computers,
        handles,
        materialization,
        materialize,
        provisioning,
        graph,
        entropy,
        reads,
        sink,
        runtime_start,
        onboarding,
        direct_pairs,
        aftermath,
        clock,
    }));
    Router::new()
        .route(
            AGENT_CREATE_PATH,
            post(agent_create::<C, H, M, R, V, G, E, RD, S, RS, OD, DP, AD, CL>),
        )
        .with_state(state)
}

/// A blanket-satisfied alias: the handle namespace read already carries the full
/// `Clone`/`Send`/`Sync`/`'static` bundle by its own supertrait bounds, named here only
/// to keep the `router` where-clause readable.
pub trait AgentMaterializationLookupHandles:
    silverhand_backend_core::identity_access::human_auth_flows::HandleNamespaceRead
{
}
impl<T> AgentMaterializationLookupHandles for T where
    T: silverhand_backend_core::identity_access::human_auth_flows::HandleNamespaceRead
{
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
async fn agent_create<C, H, M, R, V, G, E, RD, S, RS, OD, DP, AD, CL>(
    State(state): State<CreateState<C, H, M, R, V, G, E, RD, S, RS, OD, DP, AD, CL>>,
    principal: Option<Extension<UserId>>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
    ValidatedJson(raw): ValidatedJson<RawCreateBody>,
) -> Response
where
    C: ComputerReadModel,
    H: AgentMaterializationLookupHandles,
    M: AgentMaterializationLookup + AgentMaterializationWrite + Send + Sync + 'static,
    R: AgentMaterializePort,
    V: AgentArmProvisioningPort + Send + Sync + 'static,
    G: AgentGraphRead + Send + Sync + 'static,
    E: SendEntropy + Send + Sync + 'static,
    RD: AgentDetailReadModel + Send + Sync + 'static,
    S: AgentChangedSink,
    RS: RuntimeStart + Clone + Send + Sync + 'static,
    OD: OnboardingDelivery,
    DP: DirectPairsChanged,
    AD: AftermathDispatch,
    CL: ProcessClock,
{
    let inner = &state.0;

    // BODY ADMISSION is framework-owned and runs BEFORE this handler (shiki, per the
    // anchor): `ValidatedJson` already decoded the body with native `Json` (a malformed
    // body / wrong-typed or present-`null` field / missing Content-Type keeps its native
    // 400/422/415) and ran the stateless garde rules. So a structural body fault precedes
    // the principal composition check below — the old hand-rolled principal-before-body
    // order no longer holds and is not a parity gate.

    // PRINCIPAL — the auth gate; its absence is a composition fault, not a request.
    let Some(Extension(actor)) = principal else {
        tracing::error!("agent-create route reached without its human principal fact");
        return internal_error();
    };

    // SEMANTIC ADMISSION — the cross-field `TryFrom` settles handle grammar, the runtime
    // enum, reasoning shape+runtime policy, service-tier capability, session-scope enum, and
    // host_id normalization; any fault is the one terminal `422 {"error":"invalid input"}`.
    let input = match CreateAgentInput::try_from(raw) {
        Ok(input) => input,
        Err(()) => return wire_admission::invalid_input(),
    };

    // WIRE ADMISSION (Idempotency-Key header) — after the body, matching TS.
    let key = match parse_idempotency_key(&headers) {
        Ok(key) => key,
        Err(rejection) => return rejection.into_response(),
    };

    let retry = wants_retry(&raw_query);
    let now_ms = inner.clock.now_unix_millis();

    let ports = CreateAgentPorts {
        computers: &inner.computers,
        handles: &inner.handles,
        materialization: &inner.materialization,
        materialize: &inner.materialize,
        provisioning: &inner.provisioning,
        graph: &inner.graph,
        entropy: &inner.entropy,
    };

    let result = create_agent(&ports, &actor, &key, input, retry, now_ms).await;

    finish(
        result,
        &inner.reads,
        &inner.sink,
        &inner.runtime_start,
        &inner.onboarding,
        &inner.direct_pairs,
        &inner.aftermath,
        &actor,
    )
    .await
}

/// Map the unit-of-work result to its HTTP response, reprojecting the committed agent
/// and firing the post-commit effects on the `Created` arm. Isolated from the port
/// wiring so the outcome→HTTP contract and the effect fan-out are tested directly.
#[allow(clippy::too_many_arguments)]
async fn finish<RD, S, RS, OD, DP, AD>(
    result: Result<CreateAgentOutcome, CreateAgentFault>,
    reads: &RD,
    sink: &S,
    runtime_start: &RS,
    onboarding: &OD,
    direct_pairs: &DP,
    aftermath: &AD,
    actor: &UserId,
) -> Response
where
    RD: AgentDetailReadModel,
    S: AgentChangedSink,
    RS: RuntimeStart + Clone + Send + Sync + 'static,
    OD: OnboardingDelivery,
    DP: DirectPairsChanged,
    AD: AftermathDispatch,
{
    let outcome = match result {
        Ok(outcome) => outcome,
        Err(fault) => {
            tracing::error!(
                error_chain =
                    crate::foundation::safe_log_field::safe_display(&(error::describe(&fault))),
                "agent-create unit of work faulted"
            );
            return internal_error();
        }
    };

    match outcome {
        CreateAgentOutcome::Created(created) => {
            let CreatedAgent { agent_id, arm } = created;
            let dto = match reproject_committed_agent(reads, &agent_id).await {
                Ok(dto) => dto,
                Err(fault) => return fault.into_safe_500("agent-create", &agent_id),
            };

            // Post-commit effects, best-effort, in the TS order. The two in-process
            // broadcasts run inline (they cannot block on remote IO, and their order is
            // observable); a failure is logged, never flips the durable `201`.
            if let CreatedArm::Agent {
                direct_line,
                pair_created,
                ..
            } = &arm
                && *pair_created
                && let Err(failure) = direct_pairs.direct_pairs_changed(actor, direct_line).await
            {
                tracing::error!(
                    agent_id = agent_id.as_str(),
                    error_chain = crate::foundation::safe_log_field::safe_display(&(error::describe(&failure))),
                    "agent-create direct-pairs edge failed"
                );
            }
            if let Err(failure) = sink.agent_changed(&agent_id).await {
                tracing::error!(
                    agent_id = agent_id.as_str(),
                    error_chain = crate::foundation::safe_log_field::safe_display(&(error::describe(&failure))),
                    "agent-create agent_changed announce failed"
                );
            }

            // The two remote daemon legs. Each is handed to the owned aftermath boundary
            // as its OWN task, so the `201` below is sealed before either touches the
            // host and a panic/fault in one can never swallow the other. Each leg logs
            // its own failure; the boxed future yields `()`.
            let destination = arm.destination().clone();
            let onboarding_message = arm.onboarding_message_id().clone();
            aftermath.dispatch(Box::pin({
                let onboarding = (*onboarding).clone();
                let destination = destination.clone();
                let onboarding_message = onboarding_message.clone();
                let agent_id = agent_id.clone();
                async move {
                    if let Err(failure) = onboarding
                        .deliver(&destination, &agent_id, &onboarding_message)
                        .await
                    {
                        tracing::error!(
                            agent_id = agent_id.as_str(),
                            error_chain = crate::foundation::safe_log_field::safe_display(&(error::describe(&failure))),
                            "agent-create onboarding delivery failed"
                        );
                    }
                }
            }));
            aftermath.dispatch(Box::pin({
                let runtime_start = (*runtime_start).clone();
                let agent_id = agent_id.clone();
                async move {
                    if let Err(failure) = runtime_start.start(&destination, &agent_id).await {
                        tracing::error!(
                            agent_id = agent_id.as_str(),
                            error_chain = crate::foundation::safe_log_field::safe_display(&(error::describe(&failure))),
                            "agent-create runtime start failed"
                        );
                    }
                }
            }));

            coordinates_response(StatusCode::CREATED, dto, &arm.coordinates())
        }
        CreateAgentOutcome::Echo(echoed) => {
            let EchoedAgent {
                agent_id,
                coordinates,
            } = echoed;
            let dto = match reproject_committed_agent(reads, &agent_id).await {
                Ok(dto) => dto,
                Err(fault) => return fault.into_safe_500("agent-create-echo", &agent_id),
            };
            coordinates_response(StatusCode::OK, dto, &coordinates)
        }
        CreateAgentOutcome::InProgress => (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "an in-flight materialize for this key is still pending",
                "code": "materialization_in_progress",
            })),
        )
            .into_response(),
        CreateAgentOutcome::PreviouslyFailed {
            failed_code,
            failed_at,
        } => (
            StatusCode::CONFLICT,
            Json(json!({
                "error": format!(
                    "previous materialize attempt failed ({failed_code}); resend with ?retry=true to retry this row, or use a fresh Idempotency-Key for a new attempt"
                ),
                "code": "materialization_previously_failed",
                "failed_code": failed_code,
                "failed_at": failed_at,
            })),
        )
            .into_response(),
        CreateAgentOutcome::MetadataMismatch { field } => (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!(
                    "Idempotency-Key reuse: request body differs from the original at field '{}'. Use a fresh Idempotency-Key for a different agent.",
                    field.as_str()
                ),
                "code": "idempotency_metadata_mismatch",
                "mismatched_field": field.as_str(),
            })),
        )
            .into_response(),
        CreateAgentOutcome::RetryTargetNotFound => (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": "no prior agent found for this Idempotency-Key; use a fresh-key POST to create a new agent",
                "code": "retry_target_not_found",
            })),
        )
            .into_response(),
        CreateAgentOutcome::RetryTargetNotFailed => (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "agent row is no longer in failed state; refresh and try again",
                "code": "retry_target_not_failed",
            })),
        )
            .into_response(),
        CreateAgentOutcome::NameTaken => (
            StatusCode::CONFLICT,
            Json(json!({ "error": "agent name already exists", "code": "agent_name_taken" })),
        )
            .into_response(),
        CreateAgentOutcome::PlacementRefused(refusal) => placement_refusal(refusal),
        CreateAgentOutcome::HostAttachPending => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "error": "host runtime is not currently attached",
                "code": "host_attach_pending",
            })),
        )
            .into_response(),
        CreateAgentOutcome::MaterializeFailed { code } => (
            materialize_failure_status(code),
            Json(json!({ "error": materialize_failure_message(code), "code": code.as_str() })),
        )
            .into_response(),
    }
}

/// The `{ agent, agent_home_workspace_id, dm_channel_id, onboarding_message_id }` body
/// the `Created` (`201`) and `Echo` (`200`) arms share. The three coordinates are all
/// `null` for an agent-scoped agent and all present for a conversation-scoped one.
fn coordinates_response(
    status: StatusCode,
    agent: AgentDto,
    coords: &CreatedCoordinates,
) -> Response {
    (
        status,
        Json(CoordinatesResponse {
            agent,
            agent_home_workspace_id: coords
                .agent_home_workspace_id
                .as_ref()
                .map(WorkspaceId::as_str),
            dm_channel_id: coords.dm_channel_id.as_ref().map(ConversationId::as_str),
            onboarding_message_id: coords.onboarding_message_id.as_ref().map(MessageId::as_str),
        }),
    )
        .into_response()
}

#[derive(Serialize)]
struct CoordinatesResponse<'a> {
    agent: AgentDto,
    agent_home_workspace_id: Option<&'a str>,
    dm_channel_id: Option<&'a str>,
    onboarding_message_id: Option<&'a str>,
}

/// The HTTP status for each host-materialize failure code (parity with the route's
/// `mapAgentCreateFailureToStatus`). The core enum has no `dependent_row_insert_failed`
/// — that path is a [`CreateAgentFault`] safe `500` here, not a materialize code.
fn materialize_failure_status(code: MaterializeFailureCode) -> StatusCode {
    match code {
        MaterializeFailureCode::DaemonInternalError => StatusCode::BAD_GATEWAY,
        MaterializeFailureCode::HostAttachPending => StatusCode::SERVICE_UNAVAILABLE,
        MaterializeFailureCode::MaterializeTimeout => StatusCode::GATEWAY_TIMEOUT,
        MaterializeFailureCode::AgentDirCollision => StatusCode::CONFLICT,
        MaterializeFailureCode::FsQuotaExceeded => StatusCode::INSUFFICIENT_STORAGE,
        MaterializeFailureCode::FsUnavailable => StatusCode::BAD_GATEWAY,
        MaterializeFailureCode::UnsupportedTemplate => StatusCode::BAD_REQUEST,
        MaterializeFailureCode::UnsupportedRuntime => StatusCode::BAD_REQUEST,
        MaterializeFailureCode::DaemonShuttingDown => StatusCode::SERVICE_UNAVAILABLE,
    }
}

/// A stable per-code message. DIVERGENCE (flagged): the core carries only the failure
/// `code`, not the raw driver message the TS route echoes; no client branches on the
/// message string, so a synthesized per-code sentence preserves the observable `code`
/// contract without inventing driver detail.
fn materialize_failure_message(code: MaterializeFailureCode) -> &'static str {
    match code {
        MaterializeFailureCode::DaemonInternalError => {
            "the host daemon failed to materialize the agent"
        }
        MaterializeFailureCode::HostAttachPending => "host runtime is not currently attached",
        MaterializeFailureCode::MaterializeTimeout => {
            "the host daemon did not acknowledge the materialize in time"
        }
        MaterializeFailureCode::AgentDirCollision => {
            "the agent directory already exists on the host"
        }
        MaterializeFailureCode::FsQuotaExceeded => {
            "the host has no free space to materialize the agent"
        }
        MaterializeFailureCode::FsUnavailable => "the host filesystem is unavailable",
        MaterializeFailureCode::UnsupportedTemplate => {
            "the host does not support the requested agent template"
        }
        MaterializeFailureCode::UnsupportedRuntime => {
            "the host does not support the requested runtime"
        }
        MaterializeFailureCode::DaemonShuttingDown => "the host daemon is shutting down",
    }
}

// ===========================================================================
// Body admission
// ===========================================================================

/// The raw JSON shape, decoded structurally by serde and validated (statelessly) by
/// garde. `name` and `runtime` are required, non-null keys — an absent or present-`null`
/// key is the native serde data fault (`422`), not a fused `400`. Every other field is
/// Zod `.optional()` but NOT `.nullable()`: a present `null` must be rejected, so they
/// are [`RequiredPatchField<T>`] (Missing/Value; present-`null` is a native decode
/// error) rather than a plain `Option<T>` (which would fold present-`null` into missing
/// and widen the accepted set). Unknown keys are ignored — Zod `.object`'s strip-unknown
/// (no `deny_unknown_fields`).
///
/// Every rule create needs is cross-field or normalization (the handle grammar, the
/// runtime enum, the reasoning-token shape AND its runtime tuning policy, the
/// service-tier runtime allowlist, the session-scope enum, the `host_id` empty→default
/// normalization), so all of it lives in `TryFrom<RawCreateBody>`; there is no stateless
/// single-field garde rule to run, hence every field is `#[garde(skip)]`.
#[derive(Debug, Deserialize, Validate)]
struct RawCreateBody {
    #[garde(skip)]
    name: String,
    #[garde(skip)]
    runtime: String,
    #[serde(default)]
    #[garde(skip)]
    model: RequiredPatchField<String>,
    #[serde(default)]
    #[garde(skip)]
    reasoning_effort: RequiredPatchField<String>,
    #[serde(default)]
    #[garde(skip)]
    service_tier: RequiredPatchField<String>,
    #[serde(default)]
    #[garde(skip)]
    description: RequiredPatchField<String>,
    #[serde(default)]
    #[garde(skip)]
    host_id: RequiredPatchField<String>,
    #[serde(default)]
    #[garde(skip)]
    session_scope: RequiredPatchField<String>,
}

/// Collapse an optional, non-nullable wire field to a domain `Option`: a present value
/// carries through, an absent key is `None`. (A present `null` never reaches here — it
/// is the decode error `RequiredPatchField` enforces.)
fn present<T>(field: RequiredPatchField<T>) -> Option<T> {
    match field {
        RequiredPatchField::Value(value) => Some(value),
        RequiredPatchField::Missing => None,
    }
}

impl TryFrom<RawCreateBody> for CreateAgentInput {
    /// Semantic-admission failure; the caller maps it to the minimal `422`.
    type Error = ();

    /// Admit the decoded body against `CreateAgentInputSchema` + its `superRefine`. Every
    /// fault folds to `Err(())` (the ONE terminal `invalid input`, issues dropped — the
    /// PATCH precedent). All format legality is settled HERE, before the core value is
    /// built (the core trusts it).
    fn try_from(raw: RawCreateBody) -> Result<Self, Self::Error> {
        let model = present(raw.model);
        let reasoning_effort = present(raw.reasoning_effort);
        let service_tier = present(raw.service_tier);
        let description = present(raw.description);
        let host_id = present(raw.host_id);
        let session_scope = present(raw.session_scope);

        // `name` and `runtime` are present (required keys); the handle grammar and the
        // runtime enum are their field-level schemas.
        let name = ParticipantHandle::parse(raw.name.as_str()).map_err(|_| ())?;
        let runtime = RuntimeKind::parse(&raw.runtime).ok_or(())?;

        // `reasoning_effort`: the token SHAPE (ReasoningEffortTokenSchema) then the
        // runtime tuning policy (the superRefine). A present token must clear both.
        if let Some(token) = reasoning_effort.as_deref()
            && !is_reasoning_effort_token(token)
        {
            return Err(());
        }
        if !reasoning_effort_valid_for_runtime(runtime, reasoning_effort.as_deref()) {
            return Err(());
        }

        if !service_tier_valid_for_runtime(runtime, service_tier.as_deref()) {
            return Err(());
        }

        // `session_scope`: the enum, when present. Absent defaults to Conversation in core.
        let session_scope = match session_scope.as_deref() {
            None => None,
            Some(scope) => Some(AgentSessionScope::parse(scope).ok_or(())?),
        };

        // `host_id`: empty/absent → default placement (TS truthy check); a non-empty
        // string always parses (the id newtype rejects only the empty string) and lets
        // core placement decide ownership/attachment.
        let host_id = match host_id.as_deref() {
            Some(host) if !host.is_empty() => Some(HostId::parse(host).map_err(|_| ())?),
            _ => None,
        };

        Ok(CreateAgentInput {
            name,
            runtime,
            model,
            reasoning_effort,
            service_tier,
            description,
            host_id,
            session_scope,
        })
    }
}

/// Parse the `Idempotency-Key` header (design §14 Q1). Absent or empty → the `missing`
/// refusal (TS `!raw` treats an empty string as absent); present but not matching
/// `/^[A-Za-z0-9_-]{1,64}$/` → `malformed`.
/// Why an `Idempotency-Key` header was refused. A cheap descriptor rather than an
/// eagerly-built `Response`, so the `Result` stays small (`result_large_err`) and the
/// helper is unit-testable by code — the `400` body is built at the call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdempotencyKeyRejection {
    /// Header absent, or present but the empty string (TS `!raw`).
    Missing,
    /// Present but not matching `/^[A-Za-z0-9_-]{1,64}$/`.
    Malformed,
}

impl IdempotencyKeyRejection {
    fn into_response(self) -> Response {
        match self {
            Self::Missing => idempotency_error(
                IDEMPOTENCY_KEY_MISSING_CODE,
                IDEMPOTENCY_KEY_MISSING_MESSAGE,
            ),
            Self::Malformed => idempotency_error(
                IDEMPOTENCY_KEY_MALFORMED_CODE,
                IDEMPOTENCY_KEY_MALFORMED_MESSAGE,
            ),
        }
    }
}

fn parse_idempotency_key(headers: &HeaderMap) -> Result<IdempotencyKey, IdempotencyKeyRejection> {
    match headers.get("Idempotency-Key") {
        None => Err(IdempotencyKeyRejection::Missing),
        Some(value) => {
            // A present but non-visible-ASCII header can never match the pattern.
            let raw = value
                .to_str()
                .map_err(|_| IdempotencyKeyRejection::Malformed)?;
            if raw.is_empty() {
                return Err(IdempotencyKeyRejection::Missing);
            }
            IdempotencyKey::parse(raw).map_err(|_| IdempotencyKeyRejection::Malformed)
        }
    }
}

/// `c.req.query('retry') === 'true'`: the single key matched literally, absent → false.
/// Shared shape with the delete face's retry flag.
fn wants_retry(raw_query: &Option<String>) -> bool {
    let Some(query) = raw_query else {
        return false;
    };
    query.split('&').any(|pair| {
        let mut kv = pair.splitn(2, '=');
        kv.next() == Some("retry") && kv.next() == Some("true")
    })
}

// ===========================================================================
// Responses
// ===========================================================================

const INTERNAL_ERROR_BODY: &str = "Internal Server Error";

const IDEMPOTENCY_KEY_MISSING_CODE: &str = "idempotency_key_missing";
const IDEMPOTENCY_KEY_MISSING_MESSAGE: &str =
    "POST /api/agents requires Idempotency-Key header (1..64 chars, [A-Za-z0-9_-])";
const IDEMPOTENCY_KEY_MALFORMED_CODE: &str = "idempotency_key_malformed";
const IDEMPOTENCY_KEY_MALFORMED_MESSAGE: &str =
    "Idempotency-Key must match /^[A-Za-z0-9_-]{1,64}$/ (per design §14 Q1)";

fn idempotency_error(code: &'static str, message: &'static str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": message, "code": code })),
    )
        .into_response()
}

fn internal_error() -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, INTERNAL_ERROR_BODY).into_response()
}

#[cfg(test)]
mod tests {
    use std::{
        convert::Infallible,
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
        application_orchestration::agent_creation::MetadataField,
        application_orchestration::agent_materialization::{
            IdempotentReservationRow, MaterializationFlipRow, RawProvisionOutcome,
            ReserveAgentInput, ReserveRecord,
        },
        conversation_messaging::messaging::{
            MessageId,
            collaboration::RuntimePlacementRefusal,
            environment::{EnvironmentUnavailable, SendEntropy},
            post_commit::{EffectFailure, RuntimeStart},
        },
        foundation::identity::{AgentId, ConversationId, HostId, UserId, WorkspaceId},
        foundation::persistence::PersistenceFailure,
        host_runtime::agent_materialize::{
            AgentMaterializeReceipt, AgentMaterializeRefusal, AgentMaterializeTarget,
        },
        host_runtime::{computer::ComputerRecord, read::ComputerReadModel},
        identity_access::agent_detail_read::{
            AgentDetailFacts, AgentDetailProjection, AgentDetailReadModel,
        },
        identity_access::agent_lifecycle::MaterializationStatus,
        identity_access::agent_profile_write::{AgentChangedAnnounceFailure, AgentChangedSink},
        identity_access::agents::runtime::RuntimeKind,
        identity_access::human_auth_flows::HandleNamespaceRead,
        identity_access::user_identity::ParticipantHandle,
    };
    use tower::ServiceExt;

    use super::*;

    // ---- helpers ----

    fn user(id: &str) -> UserId {
        UserId::parse(id).expect("user id")
    }
    fn agent(id: &str) -> AgentId {
        AgentId::parse(id).expect("agent id")
    }
    fn conversation(id: &str) -> ConversationId {
        ConversationId::parse(id).expect("conversation id")
    }
    fn message(id: &str) -> MessageId {
        MessageId::parse(id).expect("message id")
    }
    fn workspace(id: &str) -> WorkspaceId {
        WorkspaceId::parse(id).expect("workspace id")
    }

    // ---- admission unit tests (SEMANTIC value plane) ----
    //
    // These exercise serde-decode + `TryFrom` together, the semantic-admission verdict.
    // The wire STATUS split (native structural 400/422/415 vs the semantic 422 envelope)
    // is the extractor's contract, covered by the router-level tests further down.

    fn admit(json: Value) -> Option<CreateAgentInput> {
        let raw: RawCreateBody = serde_json::from_value(json).ok()?;
        CreateAgentInput::try_from(raw).ok()
    }

    #[test]
    fn a_minimal_defaulted_body_admits() {
        let input = admit(json!({ "name": "alice", "runtime": "codex" })).expect("admits");
        assert_eq!(input.name, ParticipantHandle::parse("alice").unwrap());
        assert_eq!(input.runtime, RuntimeKind::Codex);
        assert!(input.host_id.is_none());
        assert!(input.session_scope.is_none());
    }

    #[test]
    fn unknown_keys_are_stripped_not_rejected() {
        assert!(admit(json!({ "name": "alice", "runtime": "codex", "surprise": 1 })).is_some());
    }

    #[test]
    fn an_empty_host_id_defaults_placement_a_non_empty_one_is_carried() {
        let empty = admit(json!({ "name": "a", "runtime": "codex", "host_id": "" })).unwrap();
        assert!(empty.host_id.is_none());
        let named = admit(json!({ "name": "a", "runtime": "codex", "host_id": "h1" })).unwrap();
        assert_eq!(named.host_id, Some(HostId::parse("h1").unwrap()));
    }

    #[test]
    fn idempotency_header_missing_empty_malformed_and_valid() {
        let absent = HeaderMap::new();
        assert_eq!(
            parse_idempotency_key(&absent),
            Err(IdempotencyKeyRejection::Missing)
        );

        let mut empty = HeaderMap::new();
        empty.insert("Idempotency-Key", "".parse().unwrap());
        assert_eq!(
            parse_idempotency_key(&empty),
            Err(IdempotencyKeyRejection::Missing),
            "a present-but-empty header is `missing`, matching TS `!raw`"
        );

        let mut malformed = HeaderMap::new();
        malformed.insert("Idempotency-Key", "bad key!".parse().unwrap());
        assert_eq!(
            parse_idempotency_key(&malformed),
            Err(IdempotencyKeyRejection::Malformed)
        );

        let mut good = HeaderMap::new();
        good.insert("Idempotency-Key", "abc-123_DEF".parse().unwrap());
        assert!(parse_idempotency_key(&good).is_ok());
    }

    // ---- finish() fakes ----

    fn projection() -> AgentDetailProjection {
        AgentDetailProjection {
            name: "alice".to_owned(),
            display_name: "Alice".to_owned(),
            runtime: RuntimeKind::Codex,
            model: Some("gpt-5.6-sol".to_owned()),
            reasoning_effort: Some("high".to_owned()),
            service_tier: Some("default".into()),
            host_id: "h1".to_owned(),
            host_display_label: Some("Host One".to_owned()),
            host_name: Some("host-one".to_owned()),
            description: Some("desc".to_owned()),
            session_scope: "conversation".to_owned(),
            avatar_storage_key: None,
            materialization_failed_code: None,
            materialization_failed_at: None,
            deletion_started_at: None,
            created_at: 1000,
            updated_at: 2000,
        }
    }

    #[derive(Clone)]
    struct FakeReads {
        present: bool,
    }
    impl AgentDetailReadModel for FakeReads {
        async fn read_agent_detail(
            &self,
            agent_id: &AgentId,
        ) -> Result<Option<AgentDetailFacts>, PersistenceFailure> {
            if self.present {
                Ok(Some(AgentDetailFacts::from_source(
                    agent_id.clone(),
                    user("creator-1"),
                    [],
                    [],
                    MaterializationStatus::Active,
                    projection(),
                )))
            } else {
                Ok(None)
            }
        }
    }

    #[derive(Clone)]
    struct FakeSink {
        calls: Arc<AtomicUsize>,
    }
    impl AgentChangedSink for FakeSink {
        async fn agent_changed(&self, _agent: &AgentId) -> Result<(), AgentChangedAnnounceFailure> {
            self.calls.fetch_add(1, SeqCst);
            Ok(())
        }
    }

    #[derive(Clone)]
    struct FakeStart {
        calls: Arc<AtomicUsize>,
        last: Arc<Mutex<Option<(ConversationId, AgentId)>>>,
    }
    impl RuntimeStart for FakeStart {
        async fn start(
            &self,
            conversation: &ConversationId,
            agent: &AgentId,
        ) -> Result<(), EffectFailure> {
            self.calls.fetch_add(1, SeqCst);
            *self.last.lock().unwrap() = Some((conversation.clone(), agent.clone()));
            Ok(())
        }
    }

    #[derive(Clone)]
    struct FakeDeliver {
        calls: Arc<AtomicUsize>,
        last: Arc<Mutex<Option<(ConversationId, AgentId, MessageId)>>>,
        panic: bool,
    }
    impl OnboardingDelivery for FakeDeliver {
        async fn deliver(
            &self,
            conversation: &ConversationId,
            agent: &AgentId,
            onboarding_message: &MessageId,
        ) -> Result<(), EffectFailure> {
            if self.panic {
                panic!("deliver leg blew up");
            }
            self.calls.fetch_add(1, SeqCst);
            *self.last.lock().unwrap() = Some((
                conversation.clone(),
                agent.clone(),
                onboarding_message.clone(),
            ));
            Ok(())
        }
    }

    #[derive(Clone)]
    struct FakeDirectPairs {
        calls: Arc<AtomicUsize>,
        last: Arc<Mutex<Option<(UserId, ConversationId)>>>,
    }
    impl DirectPairsChanged for FakeDirectPairs {
        async fn direct_pairs_changed(
            &self,
            viewer: &UserId,
            conversation: &ConversationId,
        ) -> Result<(), EffectFailure> {
            self.calls.fetch_add(1, SeqCst);
            *self.last.lock().unwrap() = Some((viewer.clone(), conversation.clone()));
            Ok(())
        }
    }

    /// Captures the remote legs the create face hands off WITHOUT polling them, so a test
    /// can prove the `201` was sealed before any leg ran, then drive each leg as its own
    /// isolated task (mirroring the owned task-set the production adapter plugs into) so a
    /// panic in one leg cannot abort another.
    #[derive(Clone)]
    struct FakeAftermath {
        legs: Arc<Mutex<Vec<AftermathLeg>>>,
    }
    impl AftermathDispatch for FakeAftermath {
        fn dispatch(&self, leg: AftermathLeg) {
            self.legs.lock().unwrap().push(leg);
        }
    }
    impl FakeAftermath {
        fn new() -> Self {
            Self {
                legs: Arc::new(Mutex::new(Vec::new())),
            }
        }
        fn scheduled(&self) -> usize {
            self.legs.lock().unwrap().len()
        }
        /// Run every captured leg as its own spawned task and join them; a leg panic is
        /// isolated to its own `JoinHandle` and never aborts the others.
        async fn run_isolated(&self) {
            let legs: Vec<AftermathLeg> = std::mem::take(&mut *self.legs.lock().unwrap());
            let handles: Vec<_> = legs.into_iter().map(tokio::spawn).collect();
            for handle in handles {
                let _ = handle.await;
            }
        }
    }

    struct Effects {
        sink: FakeSink,
        start: FakeStart,
        deliver: FakeDeliver,
        direct_pairs: FakeDirectPairs,
        aftermath: FakeAftermath,
    }
    impl Effects {
        fn new() -> Self {
            Self::with_panicking_deliver(false)
        }
        fn with_panicking_deliver(panic: bool) -> Self {
            Self {
                sink: FakeSink {
                    calls: Arc::new(AtomicUsize::new(0)),
                },
                start: FakeStart {
                    calls: Arc::new(AtomicUsize::new(0)),
                    last: Arc::new(Mutex::new(None)),
                },
                deliver: FakeDeliver {
                    calls: Arc::new(AtomicUsize::new(0)),
                    last: Arc::new(Mutex::new(None)),
                    panic,
                },
                direct_pairs: FakeDirectPairs {
                    calls: Arc::new(AtomicUsize::new(0)),
                    last: Arc::new(Mutex::new(None)),
                },
                aftermath: FakeAftermath::new(),
            }
        }
        /// `(direct_pairs, announce, deliver, start)`. `deliver`/`start` only advance once
        /// their legs are actually run — see [`FakeAftermath::run_isolated`].
        fn counts(&self) -> (usize, usize, usize, usize) {
            (
                self.direct_pairs.calls.load(SeqCst),
                self.sink.calls.load(SeqCst),
                self.deliver.calls.load(SeqCst),
                self.start.calls.load(SeqCst),
            )
        }
    }

    struct Parsed {
        status: StatusCode,
        json: Value,
        raw: String,
    }

    async fn drive(
        result: Result<CreateAgentOutcome, CreateAgentFault>,
        present: bool,
        fx: &Effects,
    ) -> Parsed {
        let reads = FakeReads { present };
        let response = finish(
            result,
            &reads,
            &fx.sink,
            &fx.start,
            &fx.deliver,
            &fx.direct_pairs,
            &fx.aftermath,
            &user("creator-1"),
        )
        .await;
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let raw = String::from_utf8(bytes.to_vec()).unwrap();
        let json = serde_json::from_str(&raw).unwrap_or(Value::Null);
        Parsed { status, json, raw }
    }

    fn created_agent_arm(pair_created: bool) -> CreateAgentOutcome {
        CreateAgentOutcome::Created(CreatedAgent {
            agent_id: agent("a1"),
            arm: CreatedArm::Agent {
                direct_line: conversation("dl-1"),
                onboarding_message_id: message("m-1"),
                pair_created,
            },
        })
    }

    fn created_conversation_arm() -> CreateAgentOutcome {
        CreateAgentOutcome::Created(CreatedAgent {
            agent_id: agent("a1"),
            arm: CreatedArm::Conversation {
                home_workspace: workspace("wsah_a1"),
                dm_channel: conversation("onb-1"),
                onboarding_message_id: message("m-1"),
            },
        })
    }

    #[tokio::test]
    async fn created_agent_arm_is_201_with_null_coordinates_and_all_four_effects() {
        let fx = Effects::new();
        let reply = drive(Ok(created_agent_arm(true)), true, &fx).await;
        assert_eq!(reply.status, StatusCode::CREATED);
        assert!(reply.json["agent"].is_object());
        assert_eq!(
            reply.json.get("agent_home_workspace_id"),
            Some(&Value::Null)
        );
        assert_eq!(reply.json.get("dm_channel_id"), Some(&Value::Null));
        assert_eq!(reply.json.get("onboarding_message_id"), Some(&Value::Null));
        // The two in-process broadcasts already ran inline; the two remote legs are
        // scheduled but not yet run (the `201` did not wait on them).
        assert_eq!(fx.counts(), (1, 1, 0, 0));
        assert_eq!(fx.aftermath.scheduled(), 2);
        assert_eq!(
            *fx.direct_pairs.last.lock().unwrap(),
            Some((user("creator-1"), conversation("dl-1")))
        );
        // Drive the remote legs: now deliver + start each ran exactly once, at the arm's
        // destination.
        fx.aftermath.run_isolated().await;
        assert_eq!(fx.counts(), (1, 1, 1, 1));
        assert_eq!(
            *fx.deliver.last.lock().unwrap(),
            Some((conversation("dl-1"), agent("a1"), message("m-1")))
        );
        assert_eq!(
            *fx.start.last.lock().unwrap(),
            Some((conversation("dl-1"), agent("a1")))
        );
    }

    #[tokio::test]
    async fn created_agent_arm_without_a_fresh_pair_fires_no_direct_pairs_edge() {
        let fx = Effects::new();
        let reply = drive(Ok(created_agent_arm(false)), true, &fx).await;
        assert_eq!(reply.status, StatusCode::CREATED);
        // No direct-pairs edge; announce ran inline, deliver/start scheduled then run.
        assert_eq!(fx.counts(), (0, 1, 0, 0));
        fx.aftermath.run_isolated().await;
        assert_eq!(fx.counts(), (0, 1, 1, 1));
    }

    #[tokio::test]
    async fn created_conversation_arm_is_201_with_present_coordinates_and_no_direct_pairs() {
        let fx = Effects::new();
        let reply = drive(Ok(created_conversation_arm()), true, &fx).await;
        assert_eq!(reply.status, StatusCode::CREATED);
        assert_eq!(reply.json["agent_home_workspace_id"], "wsah_a1");
        assert_eq!(reply.json["dm_channel_id"], "onb-1");
        assert_eq!(reply.json["onboarding_message_id"], "m-1");
        // Conversation arm: no direct-pairs edge; the DM is the destination.
        assert_eq!(fx.counts(), (0, 1, 0, 0));
        fx.aftermath.run_isolated().await;
        assert_eq!(fx.counts(), (0, 1, 1, 1));
        assert_eq!(
            *fx.deliver.last.lock().unwrap(),
            Some((conversation("onb-1"), agent("a1"), message("m-1")))
        );
    }

    #[tokio::test]
    async fn echo_is_200_and_fires_no_effects() {
        let fx = Effects::new();
        let reply = drive(
            Ok(CreateAgentOutcome::Echo(EchoedAgent {
                agent_id: agent("a1"),
                coordinates: CreatedCoordinates {
                    agent_home_workspace_id: Some(workspace("wsah_a1")),
                    dm_channel_id: Some(conversation("onb-1")),
                    onboarding_message_id: Some(message("m-1")),
                },
            })),
            true,
            &fx,
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.json["dm_channel_id"], "onb-1");
        assert_eq!(fx.counts(), (0, 0, 0, 0));
        assert_eq!(fx.aftermath.scheduled(), 0);
    }

    #[tokio::test]
    async fn separately_scheduled_legs_can_run_after_a_delivery_panic() {
        // The face schedules separate futures; this fake aftermath runs them in
        // isolated tasks. The application's real task tracker is not exercised here.
        let fx = Effects::with_panicking_deliver(true);
        let reply = drive(Ok(created_agent_arm(true)), true, &fx).await;
        assert_eq!(reply.status, StatusCode::CREATED);
        assert_eq!(fx.aftermath.scheduled(), 2);
        fx.aftermath.run_isolated().await;
        // deliver blew up (its counter never advanced) yet start still ran.
        assert_eq!(fx.deliver.calls.load(SeqCst), 0);
        assert_eq!(fx.start.calls.load(SeqCst), 1);
    }

    #[tokio::test]
    async fn a_reproject_that_vanished_is_a_safe_500_with_no_effects() {
        let fx = Effects::new();
        // `present: false` → the committed agent cannot be reprojected → safe 500.
        let reply = drive(Ok(created_agent_arm(true)), false, &fx).await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(fx.counts(), (0, 0, 0, 0));
        assert_eq!(fx.aftermath.scheduled(), 0);
    }

    #[tokio::test]
    async fn terminal_arms_map_status_codes_and_detail_fields() {
        let fx = Effects::new();

        let in_progress = drive(Ok(CreateAgentOutcome::InProgress), true, &fx).await;
        assert_eq!(in_progress.status, StatusCode::CONFLICT);
        assert_eq!(in_progress.json["code"], "materialization_in_progress");

        let prev = drive(
            Ok(CreateAgentOutcome::PreviouslyFailed {
                failed_code: "fs_unavailable".to_owned(),
                failed_at: 4242,
            }),
            true,
            &fx,
        )
        .await;
        assert_eq!(prev.status, StatusCode::CONFLICT);
        assert_eq!(prev.json["code"], "materialization_previously_failed");
        assert_eq!(prev.json["failed_code"], "fs_unavailable");
        assert_eq!(prev.json["failed_at"], 4242);
        assert!(
            prev.json["error"]
                .as_str()
                .unwrap()
                .contains("previous materialize attempt failed (fs_unavailable)")
        );

        let mismatch = drive(
            Ok(CreateAgentOutcome::MetadataMismatch {
                field: MetadataField::Runtime,
            }),
            true,
            &fx,
        )
        .await;
        assert_eq!(mismatch.status, StatusCode::BAD_REQUEST);
        assert_eq!(mismatch.json["code"], "idempotency_metadata_mismatch");
        assert_eq!(mismatch.json["mismatched_field"], "runtime");

        let not_found = drive(Ok(CreateAgentOutcome::RetryTargetNotFound), true, &fx).await;
        assert_eq!(not_found.status, StatusCode::NOT_FOUND);
        assert_eq!(not_found.json["code"], "retry_target_not_found");

        let not_failed = drive(Ok(CreateAgentOutcome::RetryTargetNotFailed), true, &fx).await;
        assert_eq!(not_failed.status, StatusCode::CONFLICT);
        assert_eq!(not_failed.json["code"], "retry_target_not_failed");

        let name_taken = drive(Ok(CreateAgentOutcome::NameTaken), true, &fx).await;
        assert_eq!(name_taken.status, StatusCode::CONFLICT);
        assert_eq!(name_taken.json["code"], "agent_name_taken");
        assert_eq!(name_taken.json["error"], "agent name already exists");

        let host_pending = drive(Ok(CreateAgentOutcome::HostAttachPending), true, &fx).await;
        assert_eq!(host_pending.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(host_pending.json["code"], "host_attach_pending");

        // No terminal arm fires an effect.
        assert_eq!(fx.counts(), (0, 0, 0, 0));
    }

    #[tokio::test]
    async fn a_placement_refusal_maps_through_the_canonical_mapper() {
        let fx = Effects::new();
        let reply = drive(
            Ok(CreateAgentOutcome::PlacementRefused(
                RuntimePlacementRefusal::HostMissing,
            )),
            true,
            &fx,
        )
        .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        assert_eq!(
            reply.json,
            json!({"error": "daemon host was not found", "code": "host_missing"})
        );
        assert_eq!(fx.counts(), (0, 0, 0, 0));
    }

    #[tokio::test]
    async fn each_materialize_failure_code_maps_to_its_status() {
        let fx = Effects::new();
        let cases = [
            (
                MaterializeFailureCode::DaemonInternalError,
                502,
                "daemon_internal_error",
            ),
            (
                MaterializeFailureCode::HostAttachPending,
                503,
                "host_attach_pending",
            ),
            (
                MaterializeFailureCode::MaterializeTimeout,
                504,
                "materialize_timeout",
            ),
            (
                MaterializeFailureCode::AgentDirCollision,
                409,
                "agent_dir_collision",
            ),
            (
                MaterializeFailureCode::FsQuotaExceeded,
                507,
                "fs_quota_exceeded",
            ),
            (MaterializeFailureCode::FsUnavailable, 502, "fs_unavailable"),
            (
                MaterializeFailureCode::UnsupportedTemplate,
                400,
                "unsupported_template",
            ),
            (
                MaterializeFailureCode::UnsupportedRuntime,
                400,
                "unsupported_runtime",
            ),
            (
                MaterializeFailureCode::DaemonShuttingDown,
                503,
                "daemon_shutting_down",
            ),
        ];
        for (code, status, wire_code) in cases {
            let reply = drive(
                Ok(CreateAgentOutcome::MaterializeFailed { code }),
                true,
                &fx,
            )
            .await;
            assert_eq!(reply.status.as_u16(), status, "code {}", code.as_str());
            assert_eq!(reply.json["code"], wire_code);
            assert!(reply.json["error"].is_string());
        }
    }

    #[tokio::test]
    async fn a_fault_is_a_detail_free_bare_text_500() {
        let fx = Effects::new();
        let reply = drive(
            Err(CreateAgentFault::CorruptGraph {
                reason: "active conversation-scoped row with no home DM",
            }),
            true,
            &fx,
        )
        .await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply.raw, "Internal Server Error");
        assert_eq!(fx.counts(), (0, 0, 0, 0));
    }

    // ---- end-to-end router wiring ----
    //
    // One oneshot proves the 14-generic router composes and the handler glue
    // (principal → body → key → create_agent → finish) wires end-to-end. It drives the
    // retry-with-no-reservation path, which reaches `RetryTargetNotFound` after touching
    // ONLY the materialization lookup — every other core port is inert.

    #[derive(Clone)]
    struct InertCore;

    impl ComputerReadModel for InertCore {
        async fn list_for_owner(
            &self,
            _owner: UserId,
        ) -> Result<Vec<ComputerRecord>, PersistenceFailure> {
            unreachable!("placement is never reached on the retry-not-found path")
        }
        async fn read_computer(
            &self,
            _computer: HostId,
        ) -> Result<Option<ComputerRecord>, PersistenceFailure> {
            unreachable!("placement is never reached on the retry-not-found path")
        }
    }
    impl HandleNamespaceRead for InertCore {
        async fn is_handle_taken(
            &self,
            _handle: &ParticipantHandle,
        ) -> Result<bool, PersistenceFailure> {
            unreachable!("handle read is never reached on the retry-not-found path")
        }
    }
    impl AgentMaterializationLookup for InertCore {
        async fn find_by_idempotency(
            &self,
            _creator: &UserId,
            _key: &IdempotencyKey,
        ) -> Result<Option<IdempotentReservationRow>, PersistenceFailure> {
            Ok(None)
        }
    }
    impl AgentMaterializationWrite for InertCore {
        async fn reserve(
            &self,
            _input: &ReserveAgentInput,
            _now_ms: i64,
        ) -> Result<ReserveRecord, PersistenceFailure> {
            unreachable!("reserve is never reached on the retry-not-found path")
        }
        async fn fail(
            &self,
            _agent: &AgentId,
            _failed_code: &str,
            _failed_at: i64,
            _now_ms: i64,
        ) -> Result<Option<MaterializationFlipRow>, PersistenceFailure> {
            unreachable!("fail is never reached on the retry-not-found path")
        }
        async fn reopen_failed(
            &self,
            _agent: &AgentId,
            _now_ms: i64,
        ) -> Result<Option<MaterializationFlipRow>, PersistenceFailure> {
            unreachable!("reopen is never reached on the retry-not-found path")
        }
        async fn mark_stale_failed(
            &self,
            _cutoff_created_at: i64,
            _failed_at: i64,
            _now_ms: i64,
        ) -> Result<Vec<MaterializationFlipRow>, PersistenceFailure> {
            unreachable!("stale sweep is never reached on the retry-not-found path")
        }
    }
    impl AgentMaterializePort for InertCore {
        type Error = Infallible;
        type Captured = ();
        async fn capture(&self, _host: &HostId) -> Result<Option<Self::Captured>, Self::Error> {
            unreachable!("capture is never reached on the retry-not-found path")
        }
        async fn materialize(
            &self,
            _captured: Self::Captured,
            _target: &AgentMaterializeTarget,
        ) -> Result<Result<AgentMaterializeReceipt, AgentMaterializeRefusal>, Self::Error> {
            unreachable!("materialize is never reached on the retry-not-found path")
        }
    }
    impl AgentArmProvisioningPort for InertCore {
        async fn provision_and_activate(
            &self,
            _agent: &AgentId,
            _candidate_direct_line: ConversationId,
            _candidate_dm_channel: ConversationId,
            _onboarding_message_id: MessageId,
            _now_ms: i64,
        ) -> Result<RawProvisionOutcome, PersistenceFailure> {
            unreachable!("provision is never reached on the retry-not-found path")
        }
        async fn fail_after_rolled_back_provision(
            &self,
            _agent: &AgentId,
            _failed_at: i64,
            _now_ms: i64,
        ) -> Result<Option<MaterializationFlipRow>, PersistenceFailure> {
            unreachable!("compensation is never reached on the retry-not-found path")
        }
    }
    impl SendEntropy for InertCore {
        fn fill(&self, _buffer: &mut [u8]) -> Result<(), EnvironmentUnavailable> {
            unreachable!("entropy is never reached on the retry-not-found path")
        }
    }
    impl AgentGraphRead for InertCore {
        async fn find_home_dm_channel(
            &self,
            _agent: &AgentId,
        ) -> Result<Option<ConversationId>, PersistenceFailure> {
            unreachable!("graph read is never reached on the retry-not-found path")
        }
        async fn find_first_onboarding_message(
            &self,
            _dm: &ConversationId,
        ) -> Result<Option<MessageId>, PersistenceFailure> {
            unreachable!("graph read is never reached on the retry-not-found path")
        }
    }

    #[derive(Clone)]
    struct FixedClock;
    impl ProcessClock for FixedClock {
        fn now_unix_millis(&self) -> i64 {
            7000
        }
    }

    fn wired_router() -> Router {
        router(
            InertCore,
            InertCore,
            InertCore,
            InertCore,
            InertCore,
            InertCore,
            InertCore,
            FakeReads { present: true },
            FakeSink {
                calls: Arc::new(AtomicUsize::new(0)),
            },
            FakeStart {
                calls: Arc::new(AtomicUsize::new(0)),
                last: Arc::new(Mutex::new(None)),
            },
            FakeDeliver {
                calls: Arc::new(AtomicUsize::new(0)),
                last: Arc::new(Mutex::new(None)),
                panic: false,
            },
            FakeDirectPairs {
                calls: Arc::new(AtomicUsize::new(0)),
                last: Arc::new(Mutex::new(None)),
            },
            FakeAftermath::new(),
            FixedClock,
        )
    }

    async fn oneshot(request: Request<Body>) -> (StatusCode, String) {
        let response = wired_router().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn a_retry_with_no_reservation_wires_through_to_404() {
        let request = Request::post("/api/agents?retry=true")
            .header("content-type", "application/json")
            .header("Idempotency-Key", "fresh-key-1")
            .extension(user("creator-1"))
            .body(Body::from(
                serde_json::to_vec(&json!({ "name": "alice", "runtime": "codex" })).unwrap(),
            ))
            .unwrap();
        let (status, body) = oneshot(request).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let json: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["code"], "retry_target_not_found");
    }

    #[tokio::test]
    async fn a_missing_principal_is_a_safe_500() {
        let request = Request::post("/api/agents")
            .header("content-type", "application/json")
            .header("Idempotency-Key", "fresh-key-1")
            .body(Body::from(
                serde_json::to_vec(&json!({ "name": "alice", "runtime": "codex" })).unwrap(),
            ))
            .unwrap();
        let (status, body) = oneshot(request).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body, "Internal Server Error");
    }

    // A request builder carrying a valid principal + Idempotency-Key, so only the BODY
    // (and its Content-Type) varies. `wired_router`'s core ports all `unreachable!()` off
    // the retry-not-found path, so a plain (non-error) response to any of these bodies is
    // itself the saga-zero-reach proof: an admission fault returns before `create_agent`.
    fn create_request(body: &str, content_type: bool) -> Request<Body> {
        let mut builder = Request::post("/api/agents")
            .header("Idempotency-Key", "fresh-key-1")
            .extension(user("creator-1"));
        if content_type {
            builder = builder.header("content-type", "application/json");
        }
        builder.body(Body::from(body.to_owned())).unwrap()
    }

    fn is_semantic_envelope(body: &str) -> bool {
        serde_json::from_str::<Value>(body).ok() == Some(json!({ "error": "invalid input" }))
    }

    #[tokio::test]
    async fn a_malformed_body_keeps_the_native_400_before_the_key_check() {
        // No Idempotency-Key header at all, but the framework-owned body extraction runs
        // first: a malformed body is axum's NATIVE 400 (not the fused semantic envelope,
        // not the idempotency error), and the handler — hence the key check — is skipped.
        let request = Request::post("/api/agents")
            .header("content-type", "application/json")
            .extension(user("creator-1"))
            .body(Body::from("not json"))
            .unwrap();
        let (status, body) = oneshot(request).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            !is_semantic_envelope(&body),
            "native rejection, not semantic"
        );
    }

    #[tokio::test]
    async fn a_structurally_wrong_body_keeps_the_native_422() {
        // Non-object shape, missing required key, wrong-typed key, and a present-null on a
        // `.optional()`-not-`.nullable()` field — all NATIVE serde data faults (422 today),
        // never the semantic envelope, never the saga.
        for body in [
            "5",
            r#"{"runtime":"codex"}"#,
            r#"{"name":"a"}"#,
            r#"{"name":123,"runtime":"codex"}"#,
            r#"{"name":"a","runtime":"codex","service_tier":true}"#,
            r#"{"name":"a","runtime":"codex","model":null}"#,
            r#"{"name":"a","runtime":"codex","reasoning_effort":null}"#,
            r#"{"name":"a","runtime":"codex","service_tier":null}"#,
            r#"{"name":"a","runtime":"codex","description":null}"#,
            r#"{"name":"a","runtime":"codex","host_id":null}"#,
            r#"{"name":"a","runtime":"codex","session_scope":null}"#,
            // The accepted-set falsifier: field-aligned and well-typed, this array would
            // positionally bind name="alice"/runtime="codex" — a body that WOULD create —
            // under a bare derive. The shared object-only floor (#2246) makes it a native
            // 422 before the handler; the saga's placement/handle reads (`unreachable!`
            // in `InertCore`) are never touched, so authority reach is zero.
            r#"["alice","codex"]"#,
        ] {
            let (status, resp) = oneshot(create_request(body, true)).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "body {body:?}");
            assert!(
                !is_semantic_envelope(&resp),
                "native 422, not semantic: {body:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_semantic_rule_fault_is_the_minimal_422_invalid_input() {
        // Structurally valid bodies that fail the cross-field `TryFrom`: bad handle
        // grammar, bad runtime enum, bad reasoning shape, service-tier on a non-capable
        // runtime, bad session-scope enum. Each is the ONE terminal semantic 422.
        for body in [
            r#"{"name":"has space","runtime":"codex"}"#,
            r#"{"name":"a","runtime":"nope"}"#,
            r#"{"name":"a","runtime":"codex","reasoning_effort":"HIGH"}"#,
            r#"{"name":"a","runtime":"opencode","reasoning_effort":"high"}"#,
            r#"{"name":"a","runtime":"claude","service_tier":"default"}"#,
            r#"{"name":"a","runtime":"codex","service_tier":"Bad Tier"}"#,
            r#"{"name":"a","runtime":"codex","session_scope":"team"}"#,
        ] {
            let (status, resp) = oneshot(create_request(body, true)).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "body {body:?}");
            assert!(
                is_semantic_envelope(&resp),
                "semantic envelope expected: {body:?}"
            );
        }
    }

    #[tokio::test]
    async fn missing_content_type_keeps_the_native_415() {
        // Create is a JSON write; a body with no Content-Type is the native `Json` 415.
        let (status, body) = oneshot(create_request(
            r#"{"name":"alice","runtime":"codex"}"#,
            false,
        ))
        .await;
        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert!(!is_semantic_envelope(&body), "native 415, not semantic");
    }

    #[tokio::test]
    async fn a_missing_idempotency_key_is_400_after_a_valid_body() {
        let request = Request::post("/api/agents")
            .header("content-type", "application/json")
            .extension(user("creator-1"))
            .body(Body::from(
                serde_json::to_vec(&json!({ "name": "alice", "runtime": "codex" })).unwrap(),
            ))
            .unwrap();
        let (status, body) = oneshot(request).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let json: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["code"], "idempotency_key_missing");
    }
}
