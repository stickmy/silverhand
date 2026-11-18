//! Authenticated write face for `PUT /api/agents/{id}/runtime`: the dedicated
//! runtime-transition command that changes one agent's driver without changing its
//! identity, losing a live session, or racing a delete.
//!
//! Runtime is deliberately NOT part of generic `PATCH`: switching drivers is a
//! lifecycle transition (lock delivery, clear every old-runtime session, install the
//! target runtime + tuning under one guarded commit, then re-drive). This route is the
//! thin authenticated shell around [`switch_agent_runtime`]; the durable orchestration
//! lives in the core saga and its typed ports, all injected here.
//!
//! The ladder mirrors the public identity write face
//! ([`crate::identity_access::agent_patch`]):
//!
//!  1. VISIBILITY ([`can_view_agent`], active-only) — absence, a caller outside the
//!     ACL, and a non-active stage all collapse to ONE `404 {"error":"agent not
//!     found"}`, so the surface is never an existence oracle.
//!  2. AUTHORITY ([`can_edit_agent`]) — only the creator (or system) may change it;
//!     anyone who could merely see it gets `403 {"error":"forbidden"}`.
//!  3. WIRE ADMISSION — `SwitchAgentRuntimeInputSchema`: `.strict()` (serde
//!     `deny_unknown_fields`), all four tuning keys REQUIRED (each nullable via the
//!     frozen [`RequiredNullable`] wire arm, so a missing key is a native decode fault
//!     that cannot silently select a default, while an explicit `null` is the caller's
//!     "target driver's default"), `model` trimmed and non-empty-or-null, plus the two
//!     cross-field tuning-legality refinements against the TARGET runtime. Structural
//!     faults keep their native `Json` rejection (400/422/415); every SEMANTIC fault
//!     collapses to ONE terminal `422 {"error":"invalid input"}` (the Zod `issues` array
//!     is dropped, matching the ratified `PATCH` precedent — no consumer branches on it).
//!
//!     Body extraction is framework-owned and runs BEFORE this handler, so a STRUCTURAL
//!     body fault precedes the visibility/authority gates. Error ORDER is not a parity
//!     gate (shiki, per the anchor + sibling `agent_patch`): the body fault is
//!     agent-independent, so it never distinguishes an existent from an absent agent, and
//!     the anti-enumeration 404 still owns every existence-revealing outcome. The
//!     cross-field `TryFrom` (target-runtime legality) runs after authority, as before.

//!  4. SAGA — hand the validated request to [`switch_agent_runtime`]. Its product
//!     failures render as the TS `{"error":<message>,"code":<code>}` envelope at the
//!     status each arm carries; the availability faults render exactly as the daemon
//!     data-plane faults elsewhere in this crate. An internal invariant break
//!     ([`SwitchAgentRuntimeError::Fault`]) is the lane's fail-closed bare `500`.
//!  5. PROJECT — reproject the committed row through the shared canonical face and
//!     return `{ agent, cleared_sessions, restarted_conversations, restart_failures }`.
//!
//! ## `resume_read_faults` is an operational signal, never a wire field
//!
//! The saga's best-effort post-commit re-drive counts every supplemental resume-plan
//! read it had to degrade past in `resume_read_faults`. The committed switch stands
//! regardless, so this count is NOT part of the response contract (the TS
//! `SwitchAgentRuntimeResponse` has no such field). But a silent `0` would hide an
//! aftermath fault, so the boundary surfaces a non-zero count as a `tracing::warn!`
//! operational signal here — the one place that observes the committed outcome.
//!
//! Like the sibling faces, the human principal is proof the authenticating perimeter
//! ran; its ABSENCE is a composition fault (fail-closed `500`, reads nothing), not a
//! product request. Mounting the real bearer perimeter and the concrete ports is the
//! composition root's job — done by the backend-app #2234 CP4 mount.

use axum::{
    Json, Router,
    extract::{Extension, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::put,
};
use garde::Validate;
use serde::{Deserialize, Serialize};
use silverhand_backend_core::{
    conversation_messaging::messaging::collaboration::RuntimeControlPort,
    foundation::identity::{AgentId, Principal, UserId},
    foundation::name_canonicalization::is_ecmascript_trim_whitespace,
    host_runtime::agent_runtime_switch::{
        RuntimeAvailabilityProbe, RuntimeAvailabilityProbeFault, SwitchAgentRuntimeConfig,
        SwitchAgentRuntimeError, SwitchAgentRuntimeRequest, SwitchAgentSnapshotRead,
        SwitchConversationCoordination, switch_agent_runtime,
    },
    host_runtime::runtime_credential::{AgentSlotPresentationFocusPort, RuntimeCustodyReadPort},
    host_runtime::runtime_transition::{
        RuntimeTransitionInboxWrite, RuntimeTransitionSeatRead, RuntimeTransitionWrite,
    },
    identity_access::agent_detail_read::AgentDetailReadModel,
    identity_access::agent_profile_write::{AgentChangedAnnounce, AgentChangedSink},
    identity_access::agents::{
        runtime::RuntimeKind,
        runtime_capability::{reasoning_effort_valid_for_runtime, runtime_supports_fast_mode},
    },
    identity_access::capability::{
        agent_authority::{AgentAuthority, can_edit_agent},
        agent_view::{VisibleStages, can_view_agent},
    },
};

use crate::foundation::clock::ProcessClock;
use crate::foundation::error;
use crate::foundation::wire_admission::{self, ValidatedJson};
use crate::foundation::wire_patch::RequiredNullable;
use crate::identity_access::agent_wire::{AgentDto, reproject_committed_agent};

/// The product route template, in axum's `{id}` capture syntax.
pub const AGENT_RUNTIME_PATH: &str = "/api/agents/{id}/runtime";

/// The anti-enumeration refusal string, pinned to the TypeScript handler.
const AGENT_NOT_FOUND: &str = "agent not found";
/// The authority-refusal string (`c.json({ error: 'forbidden' }, 403)`).
const FORBIDDEN: &str = "forbidden";
/// The unmapped-error body: an unexpected internal fault falls to the framework default —
/// a bare `text/plain` `500`, never a JSON envelope.
const INTERNAL_ERROR_BODY: &str = "Internal Server Error";

/// `ReasoningEffortTokenSchema` bounds: `.min(1).max(32)` code units.
const REASONING_TOKEN_MIN: usize = 1;
const REASONING_TOKEN_MAX: usize = 32;

#[allow(clippy::type_complexity)]
struct SwitchState<Reads, Snap, Avail, Coord, W, Seats, Inboxes, Cust, Focus, P, Changed, C> {
    reads: Reads,
    snap: Snap,
    availability: Avail,
    coordination: Coord,
    transitions: W,
    seats: Seats,
    inboxes: Inboxes,
    custody: Cust,
    focus: Focus,
    control: P,
    changed: Changed,
    clock: C,
}

// A manual `Clone` (not `derive`) so the bound is on the FIELDS, not on phantom
// type-parameter `Clone` bounds the derive would add — every port is already `Clone` by
// its own supertrait, so this is a field-wise clone with no extra where-clause noise.
impl<Reads, Snap, Avail, Coord, W, Seats, Inboxes, Cust, Focus, P, Changed, C> Clone
    for SwitchState<Reads, Snap, Avail, Coord, W, Seats, Inboxes, Cust, Focus, P, Changed, C>
where
    Reads: Clone,
    Snap: Clone,
    Avail: Clone,
    Coord: Clone,
    W: Clone,
    Seats: Clone,
    Inboxes: Clone,
    Cust: Clone,
    Focus: Clone,
    P: Clone,
    Changed: Clone,
    C: Clone,
{
    fn clone(&self) -> Self {
        Self {
            reads: self.reads.clone(),
            snap: self.snap.clone(),
            availability: self.availability.clone(),
            coordination: self.coordination.clone(),
            transitions: self.transitions.clone(),
            seats: self.seats.clone(),
            inboxes: self.inboxes.clone(),
            custody: self.custody.clone(),
            focus: self.focus.clone(),
            control: self.control.clone(),
            changed: self.changed.clone(),
            clock: self.clock.clone(),
        }
    }
}

/// Build the `PUT /api/agents/{id}/runtime` router over the ladder read model, the ten saga
/// ports (the nine transition ports plus the post-commit `agent_changed` sink), and the
/// clock. Every generic is a narrow typed port; the concrete production backings (and the
/// bearer perimeter) are the composition root's to mount.
#[allow(clippy::too_many_arguments)]
pub fn router<Reads, Snap, Avail, Coord, W, Seats, Inboxes, Cust, Focus, P, Changed, C>(
    reads: Reads,
    snap: Snap,
    availability: Avail,
    coordination: Coord,
    transitions: W,
    seats: Seats,
    inboxes: Inboxes,
    custody: Cust,
    focus: Focus,
    control: P,
    changed: Changed,
    clock: C,
) -> Router
where
    Reads: AgentDetailReadModel + Clone + Send + Sync + 'static,
    Snap: SwitchAgentSnapshotRead,
    Avail: RuntimeAvailabilityProbe,
    Coord: SwitchConversationCoordination,
    W: RuntimeTransitionWrite,
    Seats: RuntimeTransitionSeatRead,
    Inboxes: RuntimeTransitionInboxWrite,
    Cust: RuntimeCustodyReadPort,
    Focus: AgentSlotPresentationFocusPort,
    P: RuntimeControlPort + Clone + Send + Sync + 'static,
    Changed: AgentChangedSink,
    C: ProcessClock,
{
    Router::new()
        .route(
            AGENT_RUNTIME_PATH,
            put(agent_runtime_switch::<
                Reads,
                Snap,
                Avail,
                Coord,
                W,
                Seats,
                Inboxes,
                Cust,
                Focus,
                P,
                Changed,
                C,
            >),
        )
        .with_state(SwitchState {
            reads,
            snap,
            availability,
            coordination,
            transitions,
            seats,
            inboxes,
            custody,
            focus,
            control,
            changed,
            clock,
        })
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
async fn agent_runtime_switch<
    Reads,
    Snap,
    Avail,
    Coord,
    W,
    Seats,
    Inboxes,
    Cust,
    Focus,
    P,
    Changed,
    C,
>(
    Path(agent_id): Path<String>,
    viewer: Option<Extension<UserId>>,
    State(state): State<
        SwitchState<Reads, Snap, Avail, Coord, W, Seats, Inboxes, Cust, Focus, P, Changed, C>,
    >,
    ValidatedJson(raw): ValidatedJson<SwitchBodyRaw>,
) -> Response
where
    Reads: AgentDetailReadModel,
    Snap: SwitchAgentSnapshotRead,
    Avail: RuntimeAvailabilityProbe,
    Coord: SwitchConversationCoordination,
    W: RuntimeTransitionWrite,
    Seats: RuntimeTransitionSeatRead,
    Inboxes: RuntimeTransitionInboxWrite,
    Cust: RuntimeCustodyReadPort,
    Focus: AgentSlotPresentationFocusPort,
    P: RuntimeControlPort + Clone + Send + Sync + 'static,
    Changed: AgentChangedSink,
    C: ProcessClock,
{
    let Some(Extension(viewer)) = viewer else {
        tracing::error!("agent-runtime-switch route reached without its human principal fact");
        return internal_error();
    };
    let actor = viewer.clone();
    let principal = Principal::human(viewer);

    // A malformed id names no agent — the same 404 an absent row gets.
    let Ok(agent) = AgentId::parse(agent_id) else {
        return not_found();
    };

    // ONE snapshot decides visibility AND supplies the creator the authority is judged
    // against, read BEFORE either gate. This is the ACL/detail read; the saga runs its own
    // source-bound row read through the injected snapshot port.
    let detail = match state.reads.read_agent_detail(&agent).await {
        Ok(detail) => detail,
        Err(cause) => {
            tracing::error!(
                error_chain =
                    crate::foundation::safe_log_field::safe_display(&(error::describe(&cause))),
                "agent-runtime-switch pre-write read failed"
            );
            return internal_error();
        }
    };

    // Bind the resolved row to the request before either gate: a bundle decoded for
    // another agent is a resolver defect, the lane's safe 500.
    if let Some(detail) = detail.as_ref()
        && detail.source() != &agent
    {
        tracing::error!(
            requested = agent.as_str(),
            resolved = detail.source().as_str(),
            "agent-runtime-switch resolved for the wrong agent"
        );
        return internal_error();
    }

    // VISIBILITY (active-only): absence, non-membership, and a non-active stage all
    // collapse to one anti-enumeration 404.
    let decision = can_view_agent(
        &principal,
        &agent,
        detail
            .as_ref()
            .map(silverhand_backend_core::identity_access::agent_detail_read::AgentDetailFacts::visibility),
        &VisibleStages::active_only(),
    );
    let Some(detail) = detail.filter(|_| decision.is_allowed()) else {
        return not_found();
    };

    // AUTHORITY: only the creator (or system) may change it; a viewer who is not the
    // creator gets 403.
    let authority = AgentAuthority {
        created_by_user_id: Some(detail.created_by_user_id().clone()),
    };
    if !can_edit_agent(&principal, &agent, Some(&authority)).is_allowed() {
        return forbidden();
    }

    // WIRE ADMISSION (semantic): the body was already decoded + structurally validated by
    // the framework-owned `ValidatedJson` extractor (strict unknown keys, required keys,
    // present-null vs value, types → native 400/422/415). The cross-field `TryFrom` settles
    // the runtime enum, `model` trim/non-empty normalization, the reasoning-token shape, and
    // the two target-runtime tuning-legality refinements; any fault is the one terminal 422.
    let config = match SwitchAgentRuntimeConfig::try_from(raw) {
        Ok(config) => config,
        Err(()) => return wire_admission::invalid_input(),
    };

    // SAGA. The core orchestrates the durable transition against its typed ports; its
    // product failures render as the typed `{error,code}` envelope, its availability faults
    // as the shared daemon-fault envelope, and an internal invariant break as the bare 500.
    let request = SwitchAgentRuntimeRequest {
        agent,
        actor,
        config,
    };
    let now_ms = state.clock.now_unix_millis();
    let announced = match switch_agent_runtime(
        &state.snap,
        &state.availability,
        &state.coordination,
        &state.transitions,
        &state.seats,
        &state.inboxes,
        &state.custody,
        &state.focus,
        &state.control,
        &state.changed,
        &request,
        now_ms,
    )
    .await
    {
        Ok(announced) => announced,
        Err(err) => return switch_error_response(err, &request.agent, &request.config),
    };

    // The switch is durable regardless of the announce; a failed best-effort `agent_changed` hint
    // is LOGGED, never a refusal — the same fate PATCH records. Consuming `into_parts` here is the
    // only way to read the committed tallies, so the announce residue can never be silently
    // dropped.
    let (committed, announce) = announced.into_parts();
    if let AgentChangedAnnounce::Failed(failure) = announce {
        tracing::error!(
            agent_id = committed.committed.agent_id().as_str(),
            error_chain =
                crate::foundation::safe_log_field::safe_display(&(error::describe(&failure))),
            "agent-runtime-switch committed but the agent_changed announce failed"
        );
    }

    // OPERATIONAL SIGNAL (not a wire field): a non-zero degraded-read count means the
    // best-effort re-drive could not enumerate every conversation that might need
    // restarting. The committed switch stands; surface it so the aftermath fault is never a
    // silent zero.
    if committed.resume_read_faults > 0 {
        tracing::warn!(
            agent_id = committed.committed.agent_id().as_str(),
            resume_read_faults = committed.resume_read_faults,
            restarted_conversations = committed.restarted_conversations,
            restart_failures = committed.restart_failures,
            "runtime switch committed but supplemental resume-plan reads faulted; \
             known-live/reclaimed conversations were dispatched, but resume enumeration \
             was incomplete"
        );
    }

    // PROJECT: re-read the committed row through the shared canonical face so the wire
    // (avatar_url cache-buster included) reflects the just-stamped `updated_at`. Every
    // post-commit fault is the safe 500 the face logs at the boundary.
    let dto = match reproject_committed_agent(&state.reads, committed.committed.agent_id()).await {
        Ok(dto) => dto,
        Err(fault) => {
            return fault.into_safe_500("agent-runtime-switch", committed.committed.agent_id());
        }
    };

    Json(SwitchResponse {
        agent: dto,
        cleared_sessions: committed.cleared_sessions,
        restarted_conversations: committed.restarted_conversations,
        restart_failures: committed.restart_failures,
    })
    .into_response()
}

// ===========================================================================
// Wire admission — `SwitchAgentRuntimeInputSchema`
// ===========================================================================

/// The raw JSON shape of `SwitchAgentRuntimeInputSchema.strict()`. `deny_unknown_fields`
/// is the `.strict()` policy; every field is a REQUIRED key (no `#[serde(default)]`), so a
/// missing key is the native serde data fault, never a silent default. `runtime` is a
/// required non-null enum (decoded as a string, mapped by the closed-set `TryFrom`); the
/// three tuning keys are required-AND-nullable, the frozen [`RequiredNullable`] arm —
/// decoded via [`RequiredNullable::deserialize_present`] on a field that is deliberately
/// NOT `#[serde(default)]` so the derive owns the "missing key" verdict.
///
/// Every remaining rule is normalization or cross-field (the runtime enum, `model`
/// trim/non-empty, the reasoning-token shape, and the two target-runtime tuning-legality
/// refinements), so all of it lives in `TryFrom`; there is no stateless single-field garde
/// rule, hence every field is `#[garde(skip)]`.
#[derive(Debug, Deserialize, Validate)]
#[serde(deny_unknown_fields)]
struct SwitchBodyRaw {
    #[garde(skip)]
    runtime: String,
    #[serde(deserialize_with = "RequiredNullable::deserialize_present")]
    #[garde(skip)]
    model: RequiredNullable<String>,
    #[serde(deserialize_with = "RequiredNullable::deserialize_present")]
    #[garde(skip)]
    reasoning_effort: RequiredNullable<String>,
    #[serde(deserialize_with = "RequiredNullable::deserialize_present")]
    #[garde(skip)]
    fast_mode: RequiredNullable<bool>,
}

impl TryFrom<SwitchBodyRaw> for SwitchAgentRuntimeConfig {
    /// Semantic-admission failure; the caller maps it to the minimal `422`.
    type Error = ();

    /// Admit the decoded body against `SwitchAgentRuntimeInputSchema`'s value rules + its
    /// `superRefine`. Every fault folds to `Err(())` (the ONE terminal `invalid input`,
    /// issues dropped — the PATCH precedent).
    fn try_from(raw: SwitchBodyRaw) -> Result<Self, Self::Error> {
        // `runtime: RuntimeKindSchema` — required; must be a member of the closed set.
        let runtime = RuntimeKind::parse(&raw.runtime).ok_or(())?;

        // `model: z.string().trim().min(1).nullable()` — `null` → driver default; a string
        // is trimmed by the shared ECMAScript authority and must be non-empty after trim.
        let model = match raw.model {
            RequiredNullable::Null => None,
            RequiredNullable::Value(raw_model) => {
                let trimmed = raw_model.trim_matches(is_ecmascript_trim_whitespace);
                if trimmed.is_empty() {
                    return Err(());
                }
                Some(trimmed.to_owned())
            }
        };

        // `reasoning_effort: ReasoningEffortTokenSchema.nullable()` — `null` → default;
        // a string must clear the token SHAPE (per-runtime legality is the refine below).
        let reasoning_effort = match raw.reasoning_effort {
            RequiredNullable::Null => None,
            RequiredNullable::Value(token) if is_reasoning_token(&token) => Some(token),
            RequiredNullable::Value(_) => return Err(()),
        };

        // `fast_mode: FastModeSchema.nullable()` — `null` → default; a boolean carries.
        let fast_mode = match raw.fast_mode {
            RequiredNullable::Null => None,
            RequiredNullable::Value(value) => Some(value),
        };

        // `.superRefine`: reasoning-effort must be valid for the TARGET runtime + model, and
        // fast-mode may be set only on a runtime that supports it (an explicit null — now
        // `None` — is always allowed, matching the schema's `!== null` guard).
        if !reasoning_effort_valid_for_runtime(
            runtime,
            reasoning_effort.as_deref(),
            model.as_deref(),
        ) {
            return Err(());
        }
        if !runtime_supports_fast_mode(runtime) && fast_mode.is_some() {
            return Err(());
        }

        Ok(SwitchAgentRuntimeConfig {
            runtime,
            model,
            reasoning_effort,
            fast_mode,
        })
    }
}

/// `ReasoningEffortTokenSchema`: `.min(1).max(32).regex(/^[a-z][a-z0-9_-]*$/)`. The regex
/// admits only ASCII `[a-z0-9_-]`, so a code-unit count equals the char count; the first
/// char must be `[a-z]`, the rest `[a-z0-9_-]`.
fn is_reasoning_token(value: &str) -> bool {
    let length = value.chars().count();
    if !(REASONING_TOKEN_MIN..=REASONING_TOKEN_MAX).contains(&length) {
        return false;
    }
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

// ===========================================================================
// Responses
// ===========================================================================

/// Success body: the full DTO plus the effect tallies, mirroring
/// `SwitchAgentRuntimeResponse`. `resume_read_faults` is deliberately absent — it is an
/// operational signal, not a client field (see the module note).
#[derive(Serialize)]
struct SwitchResponse {
    agent: AgentDto,
    cleared_sessions: usize,
    restarted_conversations: usize,
    restart_failures: usize,
}

/// The flat `{ "error": ... }` refusal body for the static refusals.
#[derive(Serialize)]
struct ErrorBody {
    error: &'static str,
}

/// The typed product refusal body, `{ "error": <message>, "code": <code> }`, mirroring the
/// TS `c.json({ error: err.message, code: err.code }, err.status)`.
#[derive(Serialize)]
struct CodedErrorBody {
    error: String,
    code: &'static str,
}

/// Map a saga failure to its HTTP response. The product arms render the typed
/// `{error,code}` envelope at the status the TS `SwitchAgentRuntimeError` carries; the
/// availability faults render the shared daemon-fault envelope; an internal invariant
/// break is the lane's fail-closed bare `500`.
fn switch_error_response(
    err: SwitchAgentRuntimeError,
    agent: &AgentId,
    config: &SwitchAgentRuntimeConfig,
) -> Response {
    let runtime = config.runtime.as_str();
    let id = agent.as_str();
    match err {
        SwitchAgentRuntimeError::AgentNotFound => coded(
            StatusCode::NOT_FOUND,
            format!("agent {id} does not exist"),
            "agent_not_found",
        ),
        SwitchAgentRuntimeError::RuntimeUnchanged => coded(
            StatusCode::CONFLICT,
            format!("agent already uses runtime '{runtime}'"),
            "runtime_unchanged",
        ),
        SwitchAgentRuntimeError::RuntimeNotSelectable => coded(
            StatusCode::BAD_REQUEST,
            format!("runtime '{runtime}' is not selectable"),
            "runtime_not_selectable",
        ),
        SwitchAgentRuntimeError::RuntimeNotInstalled => coded(
            StatusCode::CONFLICT,
            format!("runtime '{runtime}' is not available on this computer"),
            "runtime_not_installed",
        ),
        SwitchAgentRuntimeError::TransitionInProgress => coded(
            StatusCode::CONFLICT,
            format!("agent {id} is already switching runtime"),
            "runtime_transition_in_progress",
        ),
        SwitchAgentRuntimeError::DeletionInProgress => coded(
            StatusCode::CONFLICT,
            format!("agent {id} is being deleted"),
            "agent_deletion_in_progress",
        ),
        SwitchAgentRuntimeError::TransitionLost => coded(
            StatusCode::CONFLICT,
            "runtime transition ownership was lost before commit".to_owned(),
            "runtime_transition_lost",
        ),
        SwitchAgentRuntimeError::SessionClearFailed => coded(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to clear every old-runtime session; runtime was not changed".to_owned(),
            "runtime_session_clear_failed",
        ),
        SwitchAgentRuntimeError::Availability(fault) => availability_response(fault),
        SwitchAgentRuntimeError::Fault(fault) => {
            tracing::error!(
                agent_id = id,
                fault = ?fault,
                "agent runtime switch hit an internal invariant fault"
            );
            internal_error()
        }
    }
}

/// The daemon data-plane fault envelope, byte-identical to the availability faults the
/// create face and the standalone catalog dispatch report, so one HTTP status renders per
/// cause. A transport-contract violation is NOT a daemon-attributed fault — it is OUR
/// contract with the transport breaking — so it is the lane's fail-closed bare `500`, the
/// same disposition [`SwitchAgentRuntimeError::Fault`] gets, never a `502` blaming the daemon.
fn availability_response(fault: RuntimeAvailabilityProbeFault) -> Response {
    match fault {
        RuntimeAvailabilityProbeFault::AttachPending => coded(
            StatusCode::SERVICE_UNAVAILABLE,
            "host runtime is not currently attached".to_owned(),
            "host_attach_pending",
        ),
        RuntimeAvailabilityProbeFault::Timeout => coded(
            StatusCode::GATEWAY_TIMEOUT,
            "daemon request timed out".to_owned(),
            "daemon_timeout",
        ),
        RuntimeAvailabilityProbeFault::DaemonInternalError => coded(
            StatusCode::BAD_GATEWAY,
            "daemon request failed".to_owned(),
            "daemon_internal_error",
        ),
        RuntimeAvailabilityProbeFault::TransportContractViolation => internal_error(),
    }
}

fn coded(status: StatusCode, error: String, code: &'static str) -> Response {
    (status, Json(CodedErrorBody { error, code })).into_response()
}

fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(ErrorBody {
            error: AGENT_NOT_FOUND,
        }),
    )
        .into_response()
}

fn forbidden() -> Response {
    (StatusCode::FORBIDDEN, Json(ErrorBody { error: FORBIDDEN })).into_response()
}

fn internal_error() -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, INTERNAL_ERROR_BODY).into_response()
}

#[cfg(test)]
mod tests {
    use std::{
        io,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering::SeqCst},
        },
    };

    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode},
    };
    use serde_json::{Value, json};
    use silverhand_backend_core::{
        conversation_messaging::messaging::collaboration::{
            ClearSessionOutcome, ClearSessionProviderRefusal, HardStopRuntimeOutcome,
            HardStopRuntimeProviderRefusal, RuntimeSeatTarget, RuntimeStartTarget,
            StartRuntimeOutcome, StartRuntimeProviderRefusal,
        },
        foundation::identity::{ConversationId, HostId},
        host_runtime::agent_runtime_switch::{
            RuntimeAvailabilityProbeFault, RuntimeAvailabilityVerdict, SwitchAgentRuntimeError,
            SwitchAgentRuntimeFault, SwitchAgentSnapshot,
        },
        host_runtime::runtime_credential::{RuntimePrincipal, RuntimeScope},
        host_runtime::runtime_transition::{
            AbortTransition, BeginTransitionRecord, CompleteTransitionRecord,
            RuntimeTransitionCommit, RuntimeTransitionInboxReclaimRecord, RuntimeTransitionLock,
            RuntimeTransitionSeatSnapshotRecord,
        },
        identity_access::agent_detail_read::{AgentDetailFacts, AgentDetailProjection},
        identity_access::agent_lifecycle::MaterializationStatus,
        identity_access::agents::session_scope::AgentSessionScope,
    };
    use tower::ServiceExt;

    use super::*;

    fn user(id: &str) -> UserId {
        UserId::parse(id).expect("user id")
    }

    fn agent(id: &str) -> AgentId {
        AgentId::parse(id).expect("agent id")
    }

    // ---- clock ----

    #[derive(Clone)]
    struct FixedClock(i64);
    impl ProcessClock for FixedClock {
        fn now_unix_millis(&self) -> i64 {
            self.0
        }
    }
    const NOW: i64 = 5000;

    // ---- ladder read model (single outcome per run) ----

    #[derive(Clone)]
    enum ReadOutcome {
        Present(Box<AgentDetailFacts>),
        Absent,
        Fails,
    }

    #[derive(Clone)]
    struct FakeReads {
        outcome: ReadOutcome,
        calls: Arc<AtomicUsize>,
    }

    impl FakeReads {
        fn new(outcome: ReadOutcome) -> Self {
            Self {
                outcome,
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl AgentDetailReadModel for FakeReads {
        async fn read_agent_detail(
            &self,
            _agent: &AgentId,
        ) -> Result<
            Option<AgentDetailFacts>,
            silverhand_backend_core::foundation::persistence::PersistenceFailure,
        > {
            self.calls.fetch_add(1, SeqCst);
            match &self.outcome {
                ReadOutcome::Present(facts) => Ok(Some(facts.as_ref().clone())),
                ReadOutcome::Absent => Ok(None),
                ReadOutcome::Fails => Err(
                    silverhand_backend_core::foundation::persistence::PersistenceFailure::from_source(
                        io::Error::other("boom"),
                    ),
                ),
            }
        }
    }

    fn present(facts: AgentDetailFacts) -> ReadOutcome {
        ReadOutcome::Present(Box::new(facts))
    }

    // ---- the snapshot the saga reads (configurable to short-circuit the preflight) ----

    #[derive(Clone)]
    struct FakeSnap {
        snapshot: Option<SwitchAgentSnapshot>,
    }

    impl SwitchAgentSnapshotRead for FakeSnap {
        async fn read_switch_snapshot(
            &self,
            _agent: &AgentId,
        ) -> Result<
            Option<SwitchAgentSnapshot>,
            silverhand_backend_core::foundation::persistence::PersistenceFailure,
        > {
            Ok(self.snapshot.clone())
        }
    }

    fn snapshot(runtime: RuntimeKind) -> SwitchAgentSnapshot {
        SwitchAgentSnapshot {
            source: agent("a1"),
            runtime,
            session_scope: AgentSessionScope::Agent,
            host: HostId::parse("h1").expect("host id"),
        }
    }

    // ---- ports the ladder / preflight tests never reach ----
    //
    // Every test here stops at the ladder, at admission, or in the saga PREFLIGHT (a
    // snapshot the injected `FakeSnap` supplies), so none of these ports is ever driven.
    // A single unit struct stands in for all nine and panics if any method runs — a driven
    // method would be a routing regression, caught loudly.

    #[derive(Clone)]
    struct Unreached;

    impl RuntimeAvailabilityProbe for Unreached {
        async fn probe_runtime_available(
            &self,
            _host: &HostId,
            _runtime: RuntimeKind,
        ) -> Result<RuntimeAvailabilityVerdict, RuntimeAvailabilityProbeFault> {
            unreachable!("availability probe must not run in these tests")
        }
    }

    impl SwitchConversationCoordination for Unreached {
        async fn list_pending_for_agent(
            &self,
            _agent: &AgentId,
        ) -> Result<
            Vec<ConversationId>,
            silverhand_backend_core::foundation::persistence::PersistenceFailure,
        > {
            unreachable!("coordination must not run in these tests")
        }
        async fn list_pending_for_agent_since(
            &self,
            _agent: &AgentId,
            _since_ms: i64,
        ) -> Result<
            Vec<ConversationId>,
            silverhand_backend_core::foundation::persistence::PersistenceFailure,
        > {
            unreachable!("coordination must not run in these tests")
        }
        async fn is_agent_participant(
            &self,
            _conversation: &ConversationId,
            _agent: &AgentId,
        ) -> Result<bool, silverhand_backend_core::foundation::persistence::PersistenceFailure>
        {
            unreachable!("coordination must not run in these tests")
        }
    }

    impl RuntimeTransitionWrite for Unreached {
        async fn begin(
            &self,
            _agent: &AgentId,
            _target: RuntimeKind,
            _now_ms: i64,
        ) -> Result<
            BeginTransitionRecord,
            silverhand_backend_core::foundation::persistence::PersistenceFailure,
        > {
            unreachable!("transition write must not run in these tests")
        }
        async fn abort(
            &self,
            _lock: &RuntimeTransitionLock,
        ) -> Result<
            AbortTransition,
            silverhand_backend_core::foundation::persistence::PersistenceFailure,
        > {
            unreachable!("transition write must not run in these tests")
        }
        async fn complete(
            &self,
            _lock: &RuntimeTransitionLock,
            _commit: &RuntimeTransitionCommit,
            _now_ms: i64,
        ) -> Result<
            CompleteTransitionRecord,
            silverhand_backend_core::foundation::persistence::PersistenceFailure,
        > {
            unreachable!("transition write must not run in these tests")
        }
    }

    impl RuntimeTransitionSeatRead for Unreached {
        async fn read_transition_seats(
            &self,
            _lock: &RuntimeTransitionLock,
        ) -> Result<
            RuntimeTransitionSeatSnapshotRecord,
            silverhand_backend_core::foundation::persistence::PersistenceFailure,
        > {
            unreachable!("seat read must not run in these tests")
        }
    }

    impl RuntimeTransitionInboxWrite for Unreached {
        async fn reclaim_transition_inboxes(
            &self,
            _lock: &RuntimeTransitionLock,
        ) -> Result<
            RuntimeTransitionInboxReclaimRecord,
            silverhand_backend_core::foundation::persistence::PersistenceFailure,
        > {
            unreachable!("inbox reclaim must not run in these tests")
        }
    }

    impl RuntimeCustodyReadPort for Unreached {
        async fn current(
            &self,
            _scope: RuntimeScope,
        ) -> Result<
            Option<RuntimePrincipal>,
            silverhand_backend_core::foundation::persistence::PersistenceFailure,
        > {
            unreachable!("custody must not run in these tests")
        }
    }

    impl AgentSlotPresentationFocusPort for Unreached {
        async fn agent_slot_presentation_focus(
            &self,
            _principal: RuntimePrincipal,
        ) -> Result<
            Option<ConversationId>,
            silverhand_backend_core::foundation::persistence::PersistenceFailure,
        > {
            unreachable!("focus must not run in these tests")
        }
    }

    impl RuntimeControlPort for Unreached {
        type Error = io::Error;
        async fn start(
            &self,
            _target: &RuntimeStartTarget,
        ) -> Result<Result<StartRuntimeOutcome, StartRuntimeProviderRefusal>, Self::Error> {
            unreachable!("runtime control must not run in these tests")
        }
        async fn hard_stop(
            &self,
            _target: &RuntimeSeatTarget,
        ) -> Result<Result<HardStopRuntimeOutcome, HardStopRuntimeProviderRefusal>, Self::Error>
        {
            unreachable!("runtime control must not run in these tests")
        }
        async fn clear_session(
            &self,
            _target: &RuntimeSeatTarget,
        ) -> Result<Result<ClearSessionOutcome, ClearSessionProviderRefusal>, Self::Error> {
            unreachable!("runtime control must not run in these tests")
        }
    }

    impl AgentChangedSink for Unreached {
        async fn agent_changed(
            &self,
            _agent: &AgentId,
        ) -> Result<(), silverhand_backend_core::identity_access::agent_profile_write::AgentChangedAnnounceFailure>
{
            // Every test here stops at the ladder, admission, or the saga PREFLIGHT — none reaches
            // a commit, and the `agent_changed` hint fires ONLY after a commit. So a consulted sink
            // would be a pre-commit-announce regression, caught loudly here rather than silently
            // recorded. The announce semantics themselves are covered by the core saga tests.
            unreachable!("agent_changed must not run before a commit in these tests")
        }
    }

    // ---- fixtures ----

    fn codex_projection() -> AgentDetailProjection {
        AgentDetailProjection {
            name: "agent-one".to_owned(),
            display_name: "Base Name".to_owned(),
            runtime: RuntimeKind::Codex,
            model: Some("gpt-5.6-sol".to_owned()),
            reasoning_effort: Some("ultra".to_owned()),
            fast_mode: Some(true),
            host_id: "h1".to_owned(),
            host_display_label: Some("Host One".to_owned()),
            host_name: Some("host-one".to_owned()),
            description: Some("base desc".to_owned()),
            session_scope: "agent".to_owned(),
            avatar_storage_key: Some("avatars/a1.png".to_owned()),
            materialization_failed_code: None,
            materialization_failed_at: None,
            deletion_started_at: None,
            created_at: 1000,
            updated_at: 2000,
        }
    }

    /// The creator arm admits `viewer-1`; active stage; codex projection.
    fn creator_facts() -> AgentDetailFacts {
        AgentDetailFacts::from_source(
            agent("a1"),
            user("viewer-1"),
            [],
            [],
            MaterializationStatus::Active,
            codex_projection(),
        )
    }

    // ---- response rendering ----

    async fn render(response: Response) -> (StatusCode, String) {
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    fn parsed(body: &str) -> Value {
        serde_json::from_str::<Value>(body).unwrap()
    }

    #[allow(clippy::too_many_arguments)]
    async fn run(
        reads: FakeReads,
        snap: FakeSnap,
        id: &str,
        body: &str,
        with_principal: bool,
    ) -> (StatusCode, String, usize) {
        let uri = format!("/api/agents/{id}/runtime");
        let mut request = Request::put(&uri).header("content-type", "application/json");
        if with_principal {
            request = request.extension(user("viewer-1"));
        }
        run_raw(
            reads,
            snap,
            request.body(Body::from(body.to_owned())).unwrap(),
        )
        .await
    }

    /// Drive the router with a fully built request (so a test can omit the `Content-Type`
    /// or otherwise shape the raw request). Returns the status, the body, and how many times
    /// the ACL read ran — the read count is the witness that a STRUCTURAL body fault is
    /// refused by the framework-owned extractor BEFORE the ladder (zero reads), while a
    /// SEMANTIC fault is refused inside the handler AFTER visibility+authority (one read).
    async fn run_raw(
        reads: FakeReads,
        snap: FakeSnap,
        request: Request<Body>,
    ) -> (StatusCode, String, usize) {
        let read_calls = reads.calls.clone();
        let response = router(
            reads,
            snap,
            Unreached,
            Unreached,
            Unreached,
            Unreached,
            Unreached,
            Unreached,
            Unreached,
            Unreached,
            Unreached,
            FixedClock(NOW),
        )
        .oneshot(request)
        .await
        .unwrap();
        let (status, text) = render(response).await;
        (status, text, read_calls.load(SeqCst))
    }

    fn is_semantic_envelope(body: &str) -> bool {
        serde_json::from_str::<Value>(body).ok() == Some(json!({ "error": "invalid input" }))
    }

    fn valid_body() -> &'static str {
        r#"{"runtime":"claude","model":null,"reasoning_effort":null,"fast_mode":null}"#
    }

    fn absent_snap() -> FakeSnap {
        FakeSnap { snapshot: None }
    }

    // ===================================================================
    // The ladder
    // ===================================================================

    #[tokio::test]
    async fn a_missing_principal_is_a_safe_500_that_never_reads() {
        let (status, body, reads) = run(
            FakeReads::new(present(creator_facts())),
            absent_snap(),
            "a1",
            valid_body(),
            false,
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body, "Internal Server Error");
        assert_eq!(reads, 0);
    }

    #[tokio::test]
    async fn absent_invisible_and_non_active_rows_share_the_anti_enumeration_404() {
        for outcome in [
            ReadOutcome::Absent,
            present(AgentDetailFacts::from_source(
                agent("a1"),
                user("someone-else"),
                [],
                [],
                MaterializationStatus::Active,
                codex_projection(),
            )),
            present(AgentDetailFacts::from_source(
                agent("a1"),
                user("viewer-1"),
                [],
                [],
                MaterializationStatus::Failed,
                codex_projection(),
            )),
        ] {
            let (status, body, reads) = run(
                FakeReads::new(outcome),
                absent_snap(),
                "a1",
                valid_body(),
                true,
            )
            .await;
            assert_eq!(status, StatusCode::NOT_FOUND);
            assert_eq!(parsed(&body), json!({ "error": "agent not found" }));
            assert_eq!(reads, 1);
        }
    }

    #[tokio::test]
    async fn a_viewer_who_is_not_the_creator_is_forbidden() {
        // `viewer-1` may SEE it (a workspace member) but did not create it.
        let facts = AgentDetailFacts::from_source(
            agent("a1"),
            user("creator-user"),
            [],
            [user("viewer-1")],
            MaterializationStatus::Active,
            codex_projection(),
        );
        let (status, body, _) = run(
            FakeReads::new(present(facts)),
            absent_snap(),
            "a1",
            valid_body(),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(parsed(&body), json!({ "error": "forbidden" }));
    }

    #[tokio::test]
    async fn a_pre_read_failure_is_a_safe_500() {
        let (status, body, _) = run(
            FakeReads::new(ReadOutcome::Fails),
            absent_snap(),
            "a1",
            valid_body(),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body, "Internal Server Error");
    }

    #[tokio::test]
    async fn a_bundle_resolved_for_another_agent_is_a_safe_500() {
        let spliced = AgentDetailFacts::from_source(
            agent("a2"),
            user("viewer-1"),
            [],
            [],
            MaterializationStatus::Active,
            codex_projection(),
        );
        let (status, body, _) = run(
            FakeReads::new(present(spliced)),
            absent_snap(),
            "a1",
            valid_body(),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body, "Internal Server Error");
    }

    // ===================================================================
    // Wire admission — through the ladder
    //
    // Structural faults keep the native `Json` rejection (400/422/415) and are refused by
    // the framework-owned extractor BEFORE the ladder, so nothing is read; SEMANTIC faults
    // collapse to ONE terminal `422 {"error":"invalid input"}` inside the handler, AFTER
    // visibility+authority, so the ACL read has run exactly once.
    // ===================================================================

    #[tokio::test]
    async fn a_malformed_body_keeps_the_native_400_before_the_ladder() {
        let (status, text, reads) = run(
            FakeReads::new(present(creator_facts())),
            absent_snap(),
            "a1",
            "not json",
            true,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            !is_semantic_envelope(&text),
            "native rejection, not semantic"
        );
        assert_eq!(reads, 0, "the body extractor precedes the ACL read");
    }

    #[tokio::test]
    async fn every_structural_body_fault_keeps_the_native_422_before_the_ladder() {
        for body in [
            "[]", // top-level array — wrong shape
            "5",  // scalar — wrong shape
            // Field-aligned and well-typed, this array would positionally bind
            // runtime="claude"/model=Null/reasoning_effort=Null/fast_mode=Null — a switch
            // that WOULD reach the snapshot/transition — under a bare derive. The shared
            // object-only floor (#2246) makes it a native 422 before the ladder, so the
            // ACL read (reads==0 below) and every transition port are never touched.
            r#"["claude",null,null,null]"#,
            "{}",                     // no keys — every required key missing
            r#"{"runtime":"codex"}"#, // model/reasoning_effort/fast_mode keys missing
            r#"{"model":null,"reasoning_effort":null,"fast_mode":null}"#,
            r#"{"runtime":"codex","reasoning_effort":null,"fast_mode":null}"#,
            r#"{"runtime":"codex","model":null,"fast_mode":null}"#,
            r#"{"runtime":"codex","model":null,"reasoning_effort":null}"#,
            r#"{"runtime":"codex","model":null,"reasoning_effort":null,"fast_mode":null,"x":1}"#, // deny_unknown_fields
            r#"{"runtime":"codex","model":5,"reasoning_effort":null,"fast_mode":null}"#, // model wrong type
            r#"{"runtime":"codex","model":null,"reasoning_effort":5,"fast_mode":null}"#, // token wrong type
            r#"{"runtime":"codex","model":null,"reasoning_effort":null,"fast_mode":"yes"}"#, // fast_mode wrong type
        ] {
            let (status, text, reads) = run(
                FakeReads::new(present(creator_facts())),
                absent_snap(),
                "a1",
                body,
                true,
            )
            .await;
            assert_eq!(
                status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "body {body:?} is the native structural 422"
            );
            assert!(
                !is_semantic_envelope(&text),
                "body {body:?} keeps the native rejection, not the semantic envelope"
            );
            assert_eq!(reads, 0, "body {body:?} is refused before the ACL read");
        }
    }

    #[tokio::test]
    async fn every_semantic_body_fault_is_one_terminal_invalid_input_after_the_ladder() {
        for body in [
            r#"{"runtime":"bogus","model":null,"reasoning_effort":null,"fast_mode":null}"#, // bad enum
            r#"{"runtime":"codex","model":"   ","reasoning_effort":null,"fast_mode":null}"#, // trims empty
            r#"{"runtime":"codex","model":null,"reasoning_effort":"BAD","fast_mode":null}"#, // token regex
            r#"{"runtime":"opencode","model":null,"reasoning_effort":"high","fast_mode":null}"#, // valid shape, wrong runtime allowlist
            r#"{"runtime":"claude","model":null,"reasoning_effort":null,"fast_mode":true}"#, // fast-mode refine
        ] {
            let (status, text, reads) = run(
                FakeReads::new(present(creator_facts())),
                absent_snap(),
                "a1",
                body,
                true,
            )
            .await;
            assert_eq!(
                status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "body {body:?} is the semantic 422"
            );
            assert!(
                is_semantic_envelope(&text),
                "body {body:?} is the terminal invalid input"
            );
            assert_eq!(reads, 1, "body {body:?} is judged after the ladder read");
        }
    }

    #[tokio::test]
    async fn a_missing_content_type_keeps_the_native_415_before_the_ladder() {
        let request = Request::put("/api/agents/a1/runtime")
            .extension(user("viewer-1"))
            .body(Body::from(valid_body().to_owned()))
            .unwrap();
        let (status, text, reads) = run_raw(
            FakeReads::new(present(creator_facts())),
            absent_snap(),
            request,
        )
        .await;
        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert!(
            !is_semantic_envelope(&text),
            "native rejection, not semantic"
        );
        assert_eq!(reads, 0, "the body extractor precedes the ACL read");
    }

    // ===================================================================
    // Saga PREFLIGHT reached — proves admission passes and error mapping wires
    // ===================================================================

    #[tokio::test]
    async fn an_agent_the_saga_cannot_read_is_the_typed_agent_not_found_404_shape() {
        // Ladder passes on the ACL read; the saga's own snapshot read finds nothing → the
        // typed `{error,code}` envelope, distinct from the ladder's bare `agent not found`.
        let (status, body, _) = run(
            FakeReads::new(present(creator_facts())),
            absent_snap(),
            "a1",
            valid_body(),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(
            parsed(&body),
            json!({ "error": "agent a1 does not exist", "code": "agent_not_found" })
        );
    }

    #[tokio::test]
    async fn a_target_equal_to_the_current_runtime_is_runtime_unchanged_409() {
        // The snapshot reports the same runtime the body requests → the preflight rejects it.
        let (status, body, _) = run(
            FakeReads::new(present(creator_facts())),
            FakeSnap {
                snapshot: Some(snapshot(RuntimeKind::Claude)),
            },
            "a1",
            valid_body(),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(
            parsed(&body),
            json!({ "error": "agent already uses runtime 'claude'", "code": "runtime_unchanged" })
        );
    }

    // ===================================================================
    // Pure admission — `SwitchBodyRaw` decode + `TryFrom`
    // ===================================================================

    fn admit(body: &str) -> Option<SwitchAgentRuntimeConfig> {
        serde_json::from_str::<SwitchBodyRaw>(body)
            .ok()
            .and_then(|raw| SwitchAgentRuntimeConfig::try_from(raw).ok())
    }

    #[test]
    fn a_valid_body_trims_the_model_and_carries_the_tuning() {
        let config = admit(
            r#"{"runtime":"codex","model":"  gpt-5  ","reasoning_effort":null,"fast_mode":true}"#,
        )
        .expect("valid");
        assert_eq!(
            config,
            SwitchAgentRuntimeConfig {
                runtime: RuntimeKind::Codex,
                model: Some("gpt-5".to_owned()),
                reasoning_effort: None,
                fast_mode: Some(true),
            }
        );
    }

    #[test]
    fn a_null_model_and_null_fast_mode_are_the_driver_default() {
        let config = admit(valid_body()).expect("valid");
        assert_eq!(config.runtime, RuntimeKind::Claude);
        assert_eq!(config.model, None);
        assert_eq!(config.fast_mode, None);
        assert_eq!(config.reasoning_effort, None);
    }

    // ===================================================================
    // Pure error mapping — `switch_error_response`
    // ===================================================================

    fn config(runtime: RuntimeKind) -> SwitchAgentRuntimeConfig {
        SwitchAgentRuntimeConfig {
            runtime,
            model: None,
            reasoning_effort: None,
            fast_mode: None,
        }
    }

    async fn mapped(err: SwitchAgentRuntimeError) -> (StatusCode, String) {
        render(switch_error_response(
            err,
            &agent("a1"),
            &config(RuntimeKind::Claude),
        ))
        .await
    }

    #[tokio::test]
    async fn each_product_arm_maps_to_its_status_and_coded_body() {
        let cases = [
            (
                SwitchAgentRuntimeError::RuntimeNotSelectable,
                StatusCode::BAD_REQUEST,
                json!({ "error": "runtime 'claude' is not selectable", "code": "runtime_not_selectable" }),
            ),
            (
                SwitchAgentRuntimeError::RuntimeNotInstalled,
                StatusCode::CONFLICT,
                json!({ "error": "runtime 'claude' is not available on this computer", "code": "runtime_not_installed" }),
            ),
            (
                SwitchAgentRuntimeError::TransitionInProgress,
                StatusCode::CONFLICT,
                json!({ "error": "agent a1 is already switching runtime", "code": "runtime_transition_in_progress" }),
            ),
            (
                SwitchAgentRuntimeError::DeletionInProgress,
                StatusCode::CONFLICT,
                json!({ "error": "agent a1 is being deleted", "code": "agent_deletion_in_progress" }),
            ),
            (
                SwitchAgentRuntimeError::TransitionLost,
                StatusCode::CONFLICT,
                json!({ "error": "runtime transition ownership was lost before commit", "code": "runtime_transition_lost" }),
            ),
            (
                SwitchAgentRuntimeError::SessionClearFailed,
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "error": "failed to clear every old-runtime session; runtime was not changed", "code": "runtime_session_clear_failed" }),
            ),
        ];
        for (err, status, body) in cases {
            let (got_status, got_body) = mapped(err).await;
            assert_eq!(got_status, status, "arm {err:?}");
            assert_eq!(parsed(&got_body), body, "arm {err:?}");
        }
    }

    #[tokio::test]
    async fn availability_faults_map_to_the_shared_daemon_envelope() {
        let cases = [
            (
                RuntimeAvailabilityProbeFault::AttachPending,
                StatusCode::SERVICE_UNAVAILABLE,
                json!({ "error": "host runtime is not currently attached", "code": "host_attach_pending" }),
            ),
            (
                RuntimeAvailabilityProbeFault::Timeout,
                StatusCode::GATEWAY_TIMEOUT,
                json!({ "error": "daemon request timed out", "code": "daemon_timeout" }),
            ),
            (
                RuntimeAvailabilityProbeFault::DaemonInternalError,
                StatusCode::BAD_GATEWAY,
                json!({ "error": "daemon request failed", "code": "daemon_internal_error" }),
            ),
        ];
        for (fault, status, body) in cases {
            let (got_status, got_body) = mapped(SwitchAgentRuntimeError::Availability(fault)).await;
            assert_eq!(got_status, status, "fault {fault:?}");
            assert_eq!(parsed(&got_body), body, "fault {fault:?}");
        }
    }

    #[tokio::test]
    async fn a_transport_contract_violation_is_the_fail_closed_bare_500() {
        // NOT a daemon-attributed 502: a broken transport contract is our own invariant
        // failing, so it renders the lane's bare 500, the same as `Fault`.
        let (status, body) = mapped(SwitchAgentRuntimeError::Availability(
            RuntimeAvailabilityProbeFault::TransportContractViolation,
        ))
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body, "Internal Server Error");
    }

    #[tokio::test]
    async fn an_internal_fault_is_the_fail_closed_bare_500() {
        let (status, body) = mapped(SwitchAgentRuntimeError::Fault(
            SwitchAgentRuntimeFault::Persistence,
        ))
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body, "Internal Server Error");
    }

    // ===================================================================
    // Success body shape — reproject + `SwitchResponse`
    // ===================================================================

    #[tokio::test]
    async fn serialized_response_carries_identity_runtime_and_effect_tallies() {
        // The Ok arm shapes `{ agent, cleared_sessions, restarted_conversations,
        // restart_failures }`; `resume_read_faults` is deliberately NOT a wire field.
        let Ok(dto) =
            reproject_committed_agent(&FakeReads::new(present(creator_facts())), &agent("a1"))
                .await
        else {
            panic!("reproject must succeed on a present, source-bound row");
        };
        let (status, body) = render(
            Json(SwitchResponse {
                agent: dto,
                cleared_sessions: 3,
                restarted_conversations: 2,
                restart_failures: 1,
            })
            .into_response(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let value = parsed(&body);
        assert_eq!(value["agent"]["id"], "a1");
        assert_eq!(value["agent"]["runtime"], "codex");
        assert_eq!(value["cleared_sessions"], 3);
        assert_eq!(value["restarted_conversations"], 2);
        assert_eq!(value["restart_failures"], 1);
        assert!(
            value.get("resume_read_faults").is_none(),
            "resume_read_faults must not be a wire field"
        );
    }
}
