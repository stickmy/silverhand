//! Authenticated destructive face for `DELETE /api/agents/{id}`: the public
//! agent-deletion cascade, behind the same visibility+authority ladder its sibling
//! writes compose, then a materialization-aware two-branch dispatch.
//!
//! ## The ladder (parity with `application/delete-agent.ts`)
//!
//!  1. PRINCIPAL — the human principal is proof the authenticating perimeter ran;
//!     its ABSENCE is a composition fault, not a product request, so it takes a
//!     fail-closed `500` reading nothing. Mounting the real bearer perimeter is the
//!     composition root's job — done by the backend-app #2234 CP3 mount.
//!  2. ID — a malformed id names no agent. Unlike the read/patch faces (which
//!     answer `404 {"error":"agent not found"}`), DELETE is IDEMPOTENT: an unknown
//!     id, an ACL-invisible row, and a row that vanishes mid-flight all collapse to
//!     ONE empty `204`, so the surface is never an existence oracle AND a repeated
//!     delete of an already-gone agent still succeeds.
//!  3. VISIBILITY — [`can_view_agent`], but widened to ALL THREE materialization
//!     stages (active, pending, failed), NOT active-only. Without the opt-in the
//!     default-active filter would `204` every delete of a non-active row, hiding
//!     the management surface that has to reach `pending → 409` and `failed → 204
//!     row-only`. Absence and a non-viewer still collapse to the same idempotent
//!     `204`.
//!  4. AUTHORITY — [`can_edit_agent`]: only the creator (or system) may delete; a
//!     caller who can merely SEE it gets `403 {"error":"forbidden"}`.
//!  5. DISPATCH by [`teardown_branch`]:
//!     - `Refused` (pending_materialization) → `409 materialization_in_progress`: a
//!       reserve→flip is mid-flight; deleting now would race the provisioning tx.
//!     - `RowOnly` (failed) → the SYNCHRONOUS teardown unit of work drops the
//!       agents row (failed rows own no dependents — no stop/purge/sweep), then a
//!       best-effort deletion announce; `204`.
//!     - `Full` (active) → the confirm gate, then the durable begin marker, then
//!       the async resumer drives the fence+purge+row-CAS to a terminal or pending
//!       outcome.
//!
//! ## The active branch: sync begin, async resume
//!
//! The confirm gate (`{"confirm": true}`) is parsed HERE, on the active branch
//! only — so a failed row with a malformed body stays `204`, never `400`, exactly
//! as the TS use case defers the parse into its active branch. Then
//! [`begin_agent_deletion`] persists the fail-forward intent BEFORE any daemon
//! work: this durable marker is the crash-recovery and new-dispatch gate, and
//! remote IO never runs inside its write lock. On the real `null→timestamp`
//! transition (`Started`) the route fires a best-effort per-AGENT availability
//! edge — the agent has just flipped out of `active`, so its viewers recompute —
//! then the resumer's `try_now` establishes the daemon fence, purge, and
//! CAS-deletes the row. A `begin` that finds the row gone/non-active/mid-transition
//! maps to the product refusal without touching the resumer or the availability
//! edge; an `alreadyPending` marker owned by a prior attempt is `409
//! agent_deletion_in_progress` unless `?retry=true` re-drives it (and does NOT
//! re-fire the edge — the flip already happened on the owning attempt).
//!
//! ## One named divergence, reachability-priced (flagged for review)
//!
//!  - **`202` pending message collapses.** The route's `202` body is
//!    `{"error": <message>, "code": "agent_deletion_pending_retry"}` — the fixed
//!    code the TS route overrides the runner's own code to, plus a message. The
//!    async resumer's [`DeletionRunOutcome`] is a payload-less enum (its owning
//!    ticket froze it so), so the daemon-specific pending messages TS passes
//!    through verbatim (workspace-not-provisioned, purge fail-closed, …) collapse
//!    to the closest per-arm string here. No client branches on the `202` message
//!    (the actionable signal is the fixed code), so this is priced acceptable.
//!    A `Faulted` outcome (a frozen non-hot-retriable internal contract fault) is
//!    NOT a `202` retry: it is a detail-free safe `500`.

use std::future::Future;
use std::sync::Arc;

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Extension, Path, RawQuery, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::delete,
};
use serde::Deserialize;
use serde_json::json;
use silverhand_backend_core::{
    application_orchestration::teardown::{
        agent_delete_resume::AgentDeletionResidue,
        eligibility::{AgentTeardownEligibility, TeardownBranch, teardown_branch},
        intent::{
            AgentDeletionAttempt, AgentDeletionLifecyclePort, BeginDeletion, begin_agent_deletion,
        },
        write::{AgentTeardownCommit, AgentTeardownKey, AgentTeardownUnitOfWork},
    },
    foundation::identity::{AgentId, Principal, UserId},
    identity_access::agent_detail_read::{AgentDetailFacts, AgentDetailReadModel},
    identity_access::agent_lifecycle::MaterializationStatus,
    identity_access::capability::{
        agent_authority::{AgentAuthority, can_edit_agent},
        agent_view::{VisibleStages, can_view_agent},
    },
};

use crate::foundation::clock::ProcessClock;
use crate::foundation::error;
use crate::host_runtime::host_agent_background::agent_deletion_resumer::{
    AgentDeletionResumer, DeletionFenceBinder, DeletionRunOutcome,
};
use crate::host_runtime::scope_event_publishers::AgentAvailabilityEdge;

/// The product route template, in axum's `{id}` capture syntax. Shares the path
/// with `PATCH /api/agents/{id}`; the composition root merges the two methods on
/// the same route at mount time.
pub const AGENT_DELETE_PATH: &str = "/api/agents/{id}";

// ===========================================================================
// The narrow async-dispatch port
// ===========================================================================

/// The ONE operation the DELETE face needs from the async deletion resumer: drive
/// a just-begun (or `?retry=true`-re-driven) attempt to whatever terminal/pending
/// outcome the worker reaches THIS call.
///
/// It carries ONLY `try_now`. The route decides the synchronous conflicts
/// (`pending_materialization`, `already-in-progress`) itself from the durable
/// begin marker, so the resumer's `202/204` vocabulary never fuses with the
/// route's pre-dispatch refusals. Implemented for the real
/// [`AgentDeletionResumer`]; deterministic fakes drive it in tests.
pub trait AgentDeletionDispatch: Clone + Send + Sync + 'static {
    fn try_now(
        &self,
        attempt: AgentDeletionAttempt,
    ) -> impl Future<Output = DeletionRunOutcome> + Send;
}

impl<L, B, R> AgentDeletionDispatch for AgentDeletionResumer<L, B, R>
where
    L: AgentDeletionLifecyclePort,
    B: DeletionFenceBinder,
    R: AgentDeletionResidue,
{
    async fn try_now(&self, attempt: AgentDeletionAttempt) -> DeletionRunOutcome {
        AgentDeletionResumer::try_now(self, attempt).await
    }
}

// ===========================================================================
// Router + state
// ===========================================================================

struct StateInner<R, T, L, D, Res, A, C> {
    reads: R,
    teardown: T,
    lifecycle: L,
    dispatch: D,
    residue: Res,
    availability: A,
    clock: C,
}

struct DeleteState<R, T, L, D, Res, A, C>(Arc<StateInner<R, T, L, D, Res, A, C>>);

impl<R, T, L, D, Res, A, C> Clone for DeleteState<R, T, L, D, Res, A, C> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

/// Compose the delete face over its narrow ports:
///  - `reads` — the gate read model (visibility + authority + branch dispatch, ONE
///    snapshot).
///  - `teardown` — the synchronous row-only teardown unit of work (failed branch).
///  - `lifecycle` — the durable begin/marker authority (active branch gate).
///  - `dispatch` — the async resumer handle ([`AgentDeletionDispatch`]).
///  - `residue` — the deletion-announce effect for the row-only branch (in
///    production the SAME authority the resumer holds; no second announcer).
///  - `availability` — the per-AGENT availability edge fired best-effort when a
///    full delete BEGINS (the agent flips out of `active`; in production the ONE
///    hub fan-out, never a second audience authority).
///  - `clock` — the `deletion_started_at` timestamp source.
pub fn router<R, T, L, D, Res, A, C>(
    reads: R,
    teardown: T,
    lifecycle: L,
    dispatch: D,
    residue: Res,
    availability: A,
    clock: C,
) -> Router
where
    R: AgentDetailReadModel + Send + Sync + 'static,
    T: AgentTeardownUnitOfWork,
    L: AgentDeletionLifecyclePort,
    D: AgentDeletionDispatch,
    Res: AgentDeletionResidue,
    A: AgentAvailabilityEdge,
    C: ProcessClock,
{
    let state = DeleteState(Arc::new(StateInner {
        reads,
        teardown,
        lifecycle,
        dispatch,
        residue,
        availability,
        clock,
    }));
    Router::new()
        .route(
            AGENT_DELETE_PATH,
            delete(agent_delete::<R, T, L, D, Res, A, C>),
        )
        .with_state(state)
}

async fn agent_delete<R, T, L, D, Res, A, C>(
    State(state): State<DeleteState<R, T, L, D, Res, A, C>>,
    principal: Option<Extension<UserId>>,
    Path(agent_id): Path<String>,
    RawQuery(raw_query): RawQuery,
    body: Bytes,
) -> Response
where
    R: AgentDetailReadModel + Send + Sync + 'static,
    T: AgentTeardownUnitOfWork,
    L: AgentDeletionLifecyclePort,
    D: AgentDeletionDispatch,
    Res: AgentDeletionResidue,
    A: AgentAvailabilityEdge,
    C: ProcessClock,
{
    let inner = &state.0;

    let Some(Extension(actor)) = principal else {
        tracing::error!("agent-delete route reached without its human principal fact");
        return internal_error();
    };
    let principal = Principal::human(actor);

    // A malformed id names no agent — the idempotent 204 an absent row gets.
    let Ok(agent) = AgentId::parse(agent_id) else {
        return no_content();
    };

    // ONE snapshot decides visibility, authority, AND the branch dispatch.
    let detail = match inner.reads.read_agent_detail(&agent).await {
        Ok(detail) => detail,
        Err(cause) => {
            tracing::error!(
                error_chain =
                    crate::foundation::safe_log_field::safe_display(&(error::describe(&cause))),
                "agent-delete gate read failed"
            );
            return internal_error();
        }
    };

    // Bind the resolved row to the request: a bundle decoded for another agent is a
    // resolver defect, the lane's safe 500 (never an idempotent 204 that would hide
    // it).
    if let Some(detail) = detail.as_ref()
        && detail.source() != &agent
    {
        tracing::error!(
            requested = agent.as_str(),
            resolved = detail.source().as_str(),
            "agent-delete resolved for the wrong agent"
        );
        return internal_error();
    }

    // VISIBILITY — widened to all three stages so failed/pending rows stay
    // reachable for their dispatch; absence and a non-viewer collapse to the same
    // idempotent 204.
    let decision = can_view_agent(
        &principal,
        &agent,
        detail.as_ref().map(AgentDetailFacts::visibility),
        &VisibleStages::including([
            MaterializationStatus::Active,
            MaterializationStatus::PendingMaterialization,
            MaterializationStatus::Failed,
        ]),
    );
    let Some(detail) = detail.filter(|_| decision.is_allowed()) else {
        return no_content();
    };

    // AUTHORITY — only the creator (or system) may delete.
    let authority = AgentAuthority {
        created_by_user_id: Some(detail.created_by_user_id().clone()),
    };
    if !can_edit_agent(&principal, &agent, Some(&authority)).is_allowed() {
        return forbidden();
    }

    // Materialization-aware dispatch on the ONE snapshot's stage.
    match teardown_branch(detail.visibility().materialization_status) {
        TeardownBranch::Refused => pending_in_flight(),
        TeardownBranch::RowOnly => row_only_delete(inner, &agent).await,
        TeardownBranch::Full => full_delete(inner, &agent, &body, wants_retry(&raw_query)).await,
    }
}

/// The `failed` branch: drop the agents row synchronously (no dependents, so no
/// stop/purge/sweep), then announce the deletion best-effort.
async fn row_only_delete<R, T, L, D, Res, A, C>(
    inner: &StateInner<R, T, L, D, Res, A, C>,
    agent: &AgentId,
) -> Response
where
    T: AgentTeardownUnitOfWork,
    Res: AgentDeletionResidue,
{
    let commit = match inner
        .teardown
        .tear_down_agent(
            AgentTeardownKey {
                agent_id: agent.clone(),
            },
            AgentTeardownEligibility::RowOnly,
        )
        .await
    {
        Ok(commit) => commit,
        Err(failure) => {
            tracing::error!(
                error_chain =
                    crate::foundation::safe_log_field::safe_display(&(error::describe(&failure))),
                "agent-delete row-only teardown failed"
            );
            return internal_error();
        }
    };

    match commit {
        // The row vanished under the lock — idempotent 204.
        AgentTeardownCommit::Gone => no_content(),
        // Its eligibility moved since the pre-lock read (a materialize/placement
        // raced in): nothing was torn down; re-read and retry. Bind the moved facts
        // to the request first — a commit decoded for another agent is a resolver
        // defect, the lane's safe 500, never a product 409.
        AgentTeardownCommit::Moved { facts, .. } => {
            if facts.id != *agent {
                tracing::error!(
                    requested = agent.as_str(),
                    resolved = facts.id.as_str(),
                    "agent-delete row-only move decoded for the wrong agent"
                );
                return internal_error();
            }
            pending_in_flight()
        }
        AgentTeardownCommit::Committed(torn) => {
            // Post-commit fan-out MUST ride the committed seal, never the request:
            // bind the sealed facts' id to the requested agent BEFORE any announce.
            // A seal decoded for another agent is a resolver defect — the lane's
            // safe 500 with ZERO announce, so a liar port can never fan one agent's
            // deletion out onto another agent's carriers.
            let committed = &torn.facts().id;
            if committed != agent {
                tracing::error!(
                    requested = agent.as_str(),
                    resolved = committed.as_str(),
                    "agent-delete row-only committed for the wrong agent"
                );
                return internal_error();
            }
            // The row is gone, so announce the deletion so historical author
            // carriers reconcile to the deleted snapshot — from the COMMITTED id,
            // not the request. A failed announce is logged, never a refusal: the
            // delete is durable.
            if let Err(failure) = inner.residue.agent_deleted(committed).await {
                tracing::error!(
                    agent_id = committed.as_str(),
                    error_chain = crate::foundation::safe_log_field::safe_display(
                        &(error::describe(&failure))
                    ),
                    "agent-delete row-only committed but the deletion announce failed"
                );
            }
            no_content()
        }
    }
}

/// The `active` branch: confirm gate → durable begin marker → async resume.
async fn full_delete<R, T, L, D, Res, A, C>(
    inner: &StateInner<R, T, L, D, Res, A, C>,
    agent: &AgentId,
    body: &Bytes,
    retry: bool,
) -> Response
where
    L: AgentDeletionLifecyclePort,
    D: AgentDeletionDispatch,
    A: AgentAvailabilityEdge,
    C: ProcessClock,
{
    // CONFIRM gate — parsed only on the active branch (a failed row with a
    // malformed body already returned 204), matching the TS parse ordering.
    if !confirmed(body) {
        return confirm_required();
    }

    // Persist the fail-forward intent BEFORE any daemon work: this durable marker
    // is the crash-recovery and new-dispatch gate. Remote IO never runs here.
    let started_at = inner.clock.now_unix_millis();
    let begun = match begin_agent_deletion(&inner.lifecycle, agent, started_at).await {
        Ok(begun) => begun,
        Err(failure) => {
            tracing::error!(
                error_chain =
                    crate::foundation::safe_log_field::safe_display(&(error::describe(&failure))),
                "agent-delete begin failed"
            );
            return internal_error();
        }
    };

    let attempt = match begun {
        // The row vanished before the marker could be written — idempotent 204.
        BeginDeletion::NotFound => return no_content(),
        // A materialize/runtime transition is mid-flight: deleting now would race
        // it. Re-read and retry.
        BeginDeletion::NotActive | BeginDeletion::RuntimeTransitionInProgress => {
            return pending_in_flight();
        }
        // A prior attempt already owns the deletion; without ?retry=true this is a
        // plain "already in progress" conflict. The `deletion_started_at` flip (and
        // its availability edge) already happened on that owning attempt, so a
        // re-drive fires NO fresh edge.
        BeginDeletion::AlreadyPending(attempt) => {
            if !retry {
                return deletion_in_progress();
            }
            attempt
        }
        // The real null→timestamp transition of `deletion_started_at`: the agent
        // has just flipped out of `active`. Fire the best-effort per-AGENT
        // availability edge so its viewers recompute (the availability read already
        // returns Offline the instant the stamp is set; this pushes the live edge).
        // A failed edge is logged, NEVER flips the HTTP outcome — the durable marker
        // is what carries the delete forward.
        BeginDeletion::Started(attempt) => {
            if let Err(failure) = inner.availability.agent_availability_changed(agent).await {
                tracing::error!(
                    agent_id = agent.as_str(),
                    error_chain = crate::foundation::safe_log_field::safe_display(
                        &(error::describe(&failure))
                    ),
                    "agent-delete began but the availability edge failed"
                );
            }
            attempt
        }
    };

    map_run_outcome(inner.dispatch.try_now(attempt).await)
}

/// Map the async resumer's outcome to HTTP. `Completed`/`AlreadyRunning` are the
/// terminal/conflict cases; `Closed`/`Superseded`/`Retained` all take `202` with
/// the fixed retry code and the closest per-arm message (see the module note on the
/// message collapse). The `202` means only that the durable deletion has not yet
/// synced and a LATER request or the recovery sweep can continue it — NOT that this
/// resumer will reschedule itself: per the frozen #2230 contract only `Retained`
/// hangs on the ladder / host-ready wait, `Superseded` dropped its stale token, and
/// `Closed` stopped the worker. `Faulted` is a frozen non-hot-retriable internal
/// contract fault, NOT a client-retry signal → detail-free safe `500`.
fn map_run_outcome(outcome: DeletionRunOutcome) -> Response {
    match outcome {
        DeletionRunOutcome::Completed => no_content(),
        DeletionRunOutcome::AlreadyRunning => deletion_in_progress(),
        DeletionRunOutcome::Closed => pending_retry(WORKER_CLOSED_MESSAGE),
        DeletionRunOutcome::Superseded => pending_retry(ATTEMPT_LOST_MESSAGE),
        DeletionRunOutcome::Retained => pending_retry(RETRY_LATER_MESSAGE),
        DeletionRunOutcome::Faulted => internal_error(),
    }
}

// ===========================================================================
// Body admission — the confirm gate
// ===========================================================================

/// `DeleteAgentInputSchema = z.object({ confirm: z.literal(true) })`, parsed from
/// `c.req.json().catch(() => ({}))`. A malformed body arrives as `{}` and fails the
/// schema, so bad JSON, a missing confirm, and `confirm: false`/non-boolean are ONE
/// refusal. `z.object` strips unknown keys, so extra keys alongside `confirm: true`
/// still pass.
///
/// Typed route-local (no `Value`/`Map` replica), and — per the DELETE ordering contract
/// — decoded ONLY here on the active branch, AFTER the single detail snapshot has
/// selected it. A failed row returns row-only 204 without ever reaching this decode.
#[derive(Debug, Default, Deserialize)]
struct ConfirmBody {
    #[serde(default)]
    confirm: bool,
}

fn confirmed(body: &Bytes) -> bool {
    // `catch(() => ({}))`: ANY decode fault — malformed JSON, a non-object, or a
    // wrong-typed `confirm` — collapses to the default (`confirm: false`), the single
    // refusal. Only an explicit `confirm: true` (extra keys stripped) opens the gate.
    //
    // Decoded through the shared object-only floor (#2246): `z.object` refuses a JSON
    // array, but a bare derived `Deserialize` would bind `[true]` positionally to
    // `confirm`, opening the consent gate off a shape TS rejects. `from_object_slice`
    // makes a top-level array/scalar a decode fault like any other — collapsing to the
    // single `false` refusal — without this leaf re-rolling an object visitor.
    crate::foundation::wire_admission::from_object_slice::<ConfirmBody>(body)
        .map(|parsed| parsed.confirm)
        .unwrap_or_default()
}

/// `c.req.query('retry') === 'true'`: the single key matched literally (Hono
/// decodes, but the compared value `true` needs no decoding), absent → false.
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

const FORBIDDEN: &str = "forbidden";
const INTERNAL_ERROR_BODY: &str = "Internal Server Error";

const PENDING_IN_FLIGHT_MESSAGE: &str = "an in-flight materialize for this agent is still pending; wait for it to complete (or for the §5 boot reconciler to mark it failed) before deleting";
const MATERIALIZATION_IN_PROGRESS_CODE: &str = "materialization_in_progress";

const DELETION_IN_PROGRESS_MESSAGE: &str = "agent deletion is already in progress";
const AGENT_DELETION_IN_PROGRESS_CODE: &str = "agent_deletion_in_progress";

const PENDING_RETRY_CODE: &str = "agent_deletion_pending_retry";
const WORKER_CLOSED_MESSAGE: &str = "agent deletion worker is shutting down";
const ATTEMPT_LOST_MESSAGE: &str = "deletion attempt no longer matches agent placement or marker";
const RETRY_LATER_MESSAGE: &str = "agent deletion could not be completed; retry later";

const CONFIRM_REQUIRED_ERROR: &str = "confirm flag required";
const CONFIRM_REQUIRED_DETAIL: &str = "pass {\"confirm\": true}";

fn no_content() -> Response {
    StatusCode::NO_CONTENT.into_response()
}

fn forbidden() -> Response {
    (StatusCode::FORBIDDEN, Json(json!({ "error": FORBIDDEN }))).into_response()
}

fn pending_in_flight() -> Response {
    (
        StatusCode::CONFLICT,
        Json(
            json!({ "error": PENDING_IN_FLIGHT_MESSAGE, "code": MATERIALIZATION_IN_PROGRESS_CODE }),
        ),
    )
        .into_response()
}

fn deletion_in_progress() -> Response {
    (
        StatusCode::CONFLICT,
        Json(json!({ "error": DELETION_IN_PROGRESS_MESSAGE, "code": AGENT_DELETION_IN_PROGRESS_CODE })),
    )
        .into_response()
}

fn pending_retry(message: &'static str) -> Response {
    (
        StatusCode::ACCEPTED,
        Json(json!({ "error": message, "code": PENDING_RETRY_CODE })),
    )
        .into_response()
}

fn confirm_required() -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": CONFIRM_REQUIRED_ERROR, "detail": CONFIRM_REQUIRED_DETAIL })),
    )
        .into_response()
}

fn internal_error() -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, INTERNAL_ERROR_BODY).into_response()
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
        application_orchestration::teardown::{
            agent_delete_resume::AgentDeletionResidue,
            cascade::CascadeConsent,
            contract::TeardownFailure,
            eligibility::{
                AgentTeardownEligibility, EligibilityMoved, EligibilityVerdict, hold_eligibility,
            },
            facts::{AgentTeardownCapture, AgentTeardownFacts, StoredBlobRef},
            intent::{
                AgentDeletionAttempt, AgentDeletionLifecyclePort, BeginDeletionRecord,
                CompleteDeletion, PersistedDeletionIntent,
            },
            write::{
                AgentTeardownCommit, AgentTeardownKey, AgentTeardownUnitOfWork, AgentTornDown,
                ComputerCascadeCommit, ComputerTeardownKey,
            },
        },
        foundation::identity::{AgentId, HostId, UserId},
        foundation::persistence::PersistenceFailure,
        host_runtime::aftermath::CustodyEffectFailure,
        identity_access::agent_detail_read::{
            AgentDetailFacts, AgentDetailProjection, AgentDetailReadModel,
        },
        identity_access::agent_lifecycle::MaterializationStatus,
        identity_access::agents::runtime::RuntimeKind,
    };
    use tower::ServiceExt;

    use super::*;

    const NOW: i64 = 7000;

    fn user(id: &str) -> UserId {
        UserId::parse(id).expect("user id")
    }

    fn agent(id: &str) -> AgentId {
        AgentId::parse(id).expect("agent id")
    }

    // ---- fixtures ----

    fn projection() -> AgentDetailProjection {
        AgentDetailProjection {
            name: "agent-one".to_owned(),
            display_name: "Agent One".to_owned(),
            runtime: RuntimeKind::Codex,
            model: Some("gpt-5.6-sol".to_owned()),
            reasoning_effort: Some("high".to_owned()),
            service_tier: Some("default".into()),
            host_id: "h1".to_owned(),
            host_display_label: Some("Host One".to_owned()),
            host_name: Some("host-one".to_owned()),
            description: Some("desc".to_owned()),
            session_scope: "agent".to_owned(),
            avatar_storage_key: None,
            materialization_failed_code: None,
            materialization_failed_at: None,
            deletion_started_at: None,
            created_at: 1000,
            updated_at: 2000,
        }
    }

    /// `viewer-1` is the creator (can see AND edit).
    fn creator_facts(status: MaterializationStatus) -> AgentDetailFacts {
        AgentDetailFacts::from_source(agent("a1"), user("viewer-1"), [], [], status, projection())
    }

    /// `viewer-1` shares a workspace (can see) but did NOT create it (cannot edit).
    fn workspace_only_facts(status: MaterializationStatus) -> AgentDetailFacts {
        AgentDetailFacts::from_source(
            agent("a1"),
            user("creator-user"),
            [],
            [user("viewer-1")],
            status,
            projection(),
        )
    }

    fn teardown_facts(agent: &AgentId, status: MaterializationStatus) -> AgentTeardownFacts {
        AgentTeardownFacts {
            id: agent.clone(),
            name: "agent-one".to_owned(),
            display_name: "Agent One".to_owned(),
            runtime: "codex".to_owned(),
            host_id: HostId::parse("h1").expect("host id"),
            materialization_status: status,
            failed_code: Some("boot_failed".to_owned()),
            deletion_started_at: None,
        }
    }

    fn committed_row_only(agent: &AgentId) -> AgentTeardownCommit {
        let facts = teardown_facts(agent, MaterializationStatus::Failed);
        let EligibilityVerdict::Held(held) =
            hold_eligibility(&AgentTeardownEligibility::RowOnly, facts)
        else {
            panic!("a failed row must hold against RowOnly");
        };
        let capture = AgentTeardownCapture {
            agent_id: agent.clone(),
            runtime_sessions: Vec::new(),
            dm_blob_refs: Vec::new(),
            dm_channel_id: None,
            regular_workspace_count: 0,
        };
        let torn = AgentTornDown::seal(agent, held, capture).expect("seal");
        AgentTeardownCommit::Committed(Box::new(torn))
    }

    // ---- fakes ----

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
        fn present(facts: AgentDetailFacts) -> Self {
            Self {
                outcome: ReadOutcome::Present(Box::new(facts)),
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }
        fn absent() -> Self {
            Self {
                outcome: ReadOutcome::Absent,
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }
        fn fails() -> Self {
            Self {
                outcome: ReadOutcome::Fails,
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl AgentDetailReadModel for FakeReads {
        async fn read_agent_detail(
            &self,
            _agent: &AgentId,
        ) -> Result<Option<AgentDetailFacts>, PersistenceFailure> {
            self.calls.fetch_add(1, SeqCst);
            match &self.outcome {
                ReadOutcome::Present(facts) => Ok(Some(facts.as_ref().clone())),
                ReadOutcome::Absent => Ok(None),
                ReadOutcome::Fails => Err(PersistenceFailure::from_source(io::Error::other(
                    "read boom",
                ))),
            }
        }
    }

    #[derive(Clone, Copy)]
    enum TeardownOutcome {
        Commit,
        Gone,
        Moved,
        Fails,
        /// Row-only teardown is never invoked on this run (active/refused branch).
        Unused,
    }

    #[derive(Clone)]
    struct FakeTeardown {
        outcome: TeardownOutcome,
        calls: Arc<AtomicUsize>,
    }

    impl FakeTeardown {
        fn new(outcome: TeardownOutcome) -> Self {
            Self {
                outcome,
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl AgentTeardownUnitOfWork for FakeTeardown {
        async fn tear_down_agent(
            &self,
            key: AgentTeardownKey,
            expected: AgentTeardownEligibility,
        ) -> Result<AgentTeardownCommit, TeardownFailure> {
            self.calls.fetch_add(1, SeqCst);
            // The delete face only ever tears a row down on the FAILED branch.
            assert_eq!(expected, AgentTeardownEligibility::RowOnly);
            match self.outcome {
                TeardownOutcome::Commit => Ok(committed_row_only(&key.agent_id)),
                TeardownOutcome::Gone => Ok(AgentTeardownCommit::Gone),
                TeardownOutcome::Moved => Ok(AgentTeardownCommit::Moved {
                    moved: EligibilityMoved::Materialization,
                    facts: Box::new(teardown_facts(&key.agent_id, MaterializationStatus::Active)),
                }),
                TeardownOutcome::Fails => Err(TeardownFailure::Persistence(
                    PersistenceFailure::from_source(io::Error::other("teardown boom")),
                )),
                TeardownOutcome::Unused => unreachable!("row-only teardown was not expected"),
            }
        }

        async fn cascade_computer(
            &self,
            _key: ComputerTeardownKey,
            _consent: CascadeConsent,
        ) -> Result<ComputerCascadeCommit, TeardownFailure> {
            unreachable!("the single-agent delete face never cascades a computer")
        }
    }

    #[derive(Clone, Copy)]
    enum BeginOutcome {
        Started,
        AlreadyPending,
        NotFound,
        NotActive,
        RuntimeTransitionInProgress,
        Fails,
        /// Begin is never invoked on this run (refused/row-only branch, or a pre-begin refusal).
        Unused,
    }

    #[derive(Clone)]
    struct FakeLifecycle {
        outcome: BeginOutcome,
        calls: Arc<AtomicUsize>,
    }

    impl FakeLifecycle {
        fn new(outcome: BeginOutcome) -> Self {
            Self {
                outcome,
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl AgentDeletionLifecyclePort for FakeLifecycle {
        async fn begin(
            &self,
            agent_id: &AgentId,
            started_at: i64,
        ) -> Result<BeginDeletionRecord, PersistenceFailure> {
            self.calls.fetch_add(1, SeqCst);
            let intent = PersistedDeletionIntent {
                agent_id: agent_id.clone(),
                host_id: HostId::parse("h1").expect("host id"),
                deletion_started_at: started_at,
            };
            Ok(match self.outcome {
                BeginOutcome::Started => BeginDeletionRecord::Started(intent),
                BeginOutcome::AlreadyPending => BeginDeletionRecord::AlreadyPending(intent),
                BeginOutcome::NotFound => BeginDeletionRecord::NotFound,
                BeginOutcome::NotActive => BeginDeletionRecord::NotActive,
                BeginOutcome::RuntimeTransitionInProgress => {
                    BeginDeletionRecord::RuntimeTransitionInProgress
                }
                BeginOutcome::Fails => {
                    return Err(PersistenceFailure::from_source(io::Error::other(
                        "begin boom",
                    )));
                }
                BeginOutcome::Unused => unreachable!("begin was not expected"),
            })
        }

        async fn list_pending(&self) -> Result<Vec<PersistedDeletionIntent>, PersistenceFailure> {
            Ok(Vec::new())
        }

        async fn complete(
            &self,
            _attempt: &AgentDeletionAttempt,
        ) -> Result<CompleteDeletion, TeardownFailure> {
            unreachable!("the delete face drives completion through the resumer, never complete()")
        }
    }

    #[derive(Clone)]
    struct FakeDispatch {
        outcome: DeletionRunOutcome,
        calls: Arc<AtomicUsize>,
    }

    impl FakeDispatch {
        fn new(outcome: DeletionRunOutcome) -> Self {
            Self {
                outcome,
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl AgentDeletionDispatch for FakeDispatch {
        async fn try_now(&self, _attempt: AgentDeletionAttempt) -> DeletionRunOutcome {
            self.calls.fetch_add(1, SeqCst);
            self.outcome
        }
    }

    #[derive(Clone)]
    struct FakeResidue {
        fail: bool,
        calls: Arc<AtomicUsize>,
        last_agent: Arc<Mutex<Option<AgentId>>>,
    }

    impl FakeResidue {
        fn new(fail: bool) -> Self {
            Self {
                fail,
                calls: Arc::new(AtomicUsize::new(0)),
                last_agent: Arc::new(Mutex::new(None)),
            }
        }
        /// The id the announce was fired for — proof the announce rides the
        /// committed seal, not the request.
        fn last_agent(&self) -> Arc<Mutex<Option<AgentId>>> {
            self.last_agent.clone()
        }
    }

    impl AgentDeletionResidue for FakeResidue {
        async fn agent_deleted(&self, agent: &AgentId) -> Result<(), CustodyEffectFailure> {
            self.calls.fetch_add(1, SeqCst);
            *self.last_agent.lock().expect("lock") = Some(agent.clone());
            if self.fail {
                Err(CustodyEffectFailure::from_source(io::Error::other(
                    "announce boom",
                )))
            } else {
                Ok(())
            }
        }

        async fn forget_stored_blob(
            &self,
            _blob: &StoredBlobRef,
        ) -> Result<(), CustodyEffectFailure> {
            unreachable!("the row-only delete branch sweeps no blobs")
        }
    }

    #[derive(Clone)]
    struct FixedClock(i64);
    impl ProcessClock for FixedClock {
        fn now_unix_millis(&self) -> i64 {
            self.0
        }
    }

    /// The per-AGENT availability edge. Records the agent it was fired for and
    /// whether it should fail (a failed edge must never flip the HTTP outcome).
    #[derive(Clone)]
    struct FakeAvailability {
        fail: bool,
        calls: Arc<AtomicUsize>,
        last_agent: Arc<Mutex<Option<AgentId>>>,
    }

    impl FakeAvailability {
        fn new(fail: bool) -> Self {
            Self {
                fail,
                calls: Arc::new(AtomicUsize::new(0)),
                last_agent: Arc::new(Mutex::new(None)),
            }
        }
    }

    impl AgentAvailabilityEdge for FakeAvailability {
        async fn agent_availability_changed(
            &self,
            agent: &AgentId,
        ) -> Result<(), CustodyEffectFailure> {
            self.calls.fetch_add(1, SeqCst);
            *self.last_agent.lock().expect("lock") = Some(agent.clone());
            if self.fail {
                Err(CustodyEffectFailure::from_source(io::Error::other(
                    "availability boom",
                )))
            } else {
                Ok(())
            }
        }
    }

    // ---- driver ----

    struct Reply {
        status: StatusCode,
        body: String,
        reads: usize,
        teardowns: usize,
        begins: usize,
        dispatches: usize,
        announces: usize,
        availabilities: usize,
        availability_agent: Option<AgentId>,
    }

    impl Reply {
        fn json(&self) -> Value {
            serde_json::from_str(&self.body).expect("json body")
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn run(
        reads: FakeReads,
        teardown: FakeTeardown,
        lifecycle: FakeLifecycle,
        dispatch: FakeDispatch,
        residue: FakeResidue,
        availability: FakeAvailability,
        path_id: &str,
        query: &str,
        body: &str,
        with_principal: bool,
    ) -> Reply {
        let read_calls = reads.calls.clone();
        let teardown_calls = teardown.calls.clone();
        let begin_calls = lifecycle.calls.clone();
        let dispatch_calls = dispatch.calls.clone();
        let announce_calls = residue.calls.clone();
        let availability_calls = availability.calls.clone();
        let availability_last = availability.last_agent.clone();

        let uri = if query.is_empty() {
            format!("/api/agents/{path_id}")
        } else {
            format!("/api/agents/{path_id}?{query}")
        };
        let mut request = Request::delete(&uri).header("content-type", "application/json");
        if with_principal {
            request = request.extension(user("viewer-1"));
        }
        let response = router(
            reads,
            teardown,
            lifecycle,
            dispatch,
            residue,
            availability,
            FixedClock(NOW),
        )
        .oneshot(request.body(Body::from(body.to_owned())).unwrap())
        .await
        .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        Reply {
            status,
            body: String::from_utf8(bytes.to_vec()).unwrap(),
            reads: read_calls.load(SeqCst),
            teardowns: teardown_calls.load(SeqCst),
            begins: begin_calls.load(SeqCst),
            dispatches: dispatch_calls.load(SeqCst),
            announces: announce_calls.load(SeqCst),
            availabilities: availability_calls.load(SeqCst),
            availability_agent: availability_last.lock().expect("lock").clone(),
        }
    }

    /// A run whose active-branch ports are inert (never reached).
    fn inert_teardown() -> FakeTeardown {
        FakeTeardown::new(TeardownOutcome::Unused)
    }
    fn inert_lifecycle() -> FakeLifecycle {
        FakeLifecycle::new(BeginOutcome::Unused)
    }
    fn inert_dispatch() -> FakeDispatch {
        FakeDispatch::new(DeletionRunOutcome::Completed)
    }
    fn inert_residue() -> FakeResidue {
        FakeResidue::new(false)
    }
    fn inert_availability() -> FakeAvailability {
        FakeAvailability::new(false)
    }

    // ---- the ladder ----

    #[tokio::test]
    async fn a_missing_principal_is_a_safe_500_that_never_reads() {
        let reply = run(
            FakeReads::present(creator_facts(MaterializationStatus::Failed)),
            inert_teardown(),
            inert_lifecycle(),
            inert_dispatch(),
            inert_residue(),
            inert_availability(),
            "a1",
            "",
            "",
            false,
        )
        .await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply.body, "Internal Server Error");
        assert_eq!(reply.reads, 0);
    }

    #[tokio::test]
    async fn an_absent_row_is_the_idempotent_204_not_a_404_body() {
        let reply = run(
            FakeReads::absent(),
            inert_teardown(),
            inert_lifecycle(),
            inert_dispatch(),
            inert_residue(),
            inert_availability(),
            "a1",
            "",
            "",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::NO_CONTENT);
        assert!(reply.body.is_empty());
        assert_eq!(reply.reads, 1);
    }

    #[tokio::test]
    async fn a_read_failure_is_a_safe_500() {
        let reply = run(
            FakeReads::fails(),
            inert_teardown(),
            inert_lifecycle(),
            inert_dispatch(),
            inert_residue(),
            inert_availability(),
            "a1",
            "",
            "",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(!reply.body.contains("read boom"));
    }

    #[tokio::test]
    async fn a_bundle_resolved_for_another_agent_is_a_safe_500_never_a_204() {
        let spliced = AgentDetailFacts::from_source(
            agent("a2"),
            user("viewer-1"),
            [],
            [],
            MaterializationStatus::Failed,
            projection(),
        );
        let reply = run(
            FakeReads::present(spliced),
            inert_teardown(),
            inert_lifecycle(),
            inert_dispatch(),
            inert_residue(),
            inert_availability(),
            "a1",
            "",
            "",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn a_viewer_who_cannot_see_it_is_the_same_idempotent_204() {
        let facts = AgentDetailFacts::from_source(
            agent("a1"),
            user("someone-else"),
            [],
            [],
            MaterializationStatus::Active,
            projection(),
        );
        let reply = run(
            FakeReads::present(facts),
            inert_teardown(),
            inert_lifecycle(),
            inert_dispatch(),
            inert_residue(),
            inert_availability(),
            "a1",
            "",
            r#"{"confirm":true}"#,
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::NO_CONTENT);
        assert_eq!(reply.begins, 0);
    }

    #[tokio::test]
    async fn a_viewer_who_is_not_the_creator_is_forbidden() {
        let reply = run(
            FakeReads::present(workspace_only_facts(MaterializationStatus::Failed)),
            inert_teardown(),
            inert_lifecycle(),
            inert_dispatch(),
            inert_residue(),
            inert_availability(),
            "a1",
            "",
            "",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN);
        assert_eq!(reply.json(), json!({ "error": "forbidden" }));
        assert_eq!(reply.teardowns, 0);
    }

    // ---- the pending_materialization (refused) branch ----

    #[tokio::test]
    async fn a_pending_materialization_row_is_visible_and_409_materialization_in_progress() {
        // The three-stage widener is what makes a pending row reachable at all: an
        // active-only filter would 204 here.
        let reply = run(
            FakeReads::present(creator_facts(MaterializationStatus::PendingMaterialization)),
            inert_teardown(),
            inert_lifecycle(),
            inert_dispatch(),
            inert_residue(),
            inert_availability(),
            "a1",
            "",
            "",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::CONFLICT);
        assert_eq!(reply.json()["code"], "materialization_in_progress");
        assert_eq!(reply.teardowns, 0);
        assert_eq!(reply.begins, 0);
    }

    // ---- the failed (row-only) branch ----

    #[tokio::test]
    async fn a_failed_row_commits_row_only_announces_and_returns_204() {
        let residue = FakeResidue::new(false);
        let reply = run(
            FakeReads::present(creator_facts(MaterializationStatus::Failed)),
            FakeTeardown::new(TeardownOutcome::Commit),
            inert_lifecycle(),
            inert_dispatch(),
            residue,
            inert_availability(),
            "a1",
            "",
            "",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::NO_CONTENT);
        assert_eq!(reply.teardowns, 1);
        assert_eq!(reply.announces, 1);
        // No confirm gate, no begin marker on the row-only branch.
        assert_eq!(reply.begins, 0);
    }

    #[tokio::test]
    async fn a_failed_row_with_a_malformed_body_still_deletes_204_no_confirm_gate() {
        // Parity anchor: the confirm gate is active-branch only, so a failed row with
        // a garbage body is 204, not 400.
        let reply = run(
            FakeReads::present(creator_facts(MaterializationStatus::Failed)),
            FakeTeardown::new(TeardownOutcome::Commit),
            inert_lifecycle(),
            inert_dispatch(),
            FakeResidue::new(false),
            inert_availability(),
            "a1",
            "",
            "not json at all",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::NO_CONTENT);
        assert_eq!(reply.teardowns, 1);
    }

    #[tokio::test]
    async fn a_failed_row_ignores_a_confirming_body_and_stays_row_only_204() {
        // Branch-order gate: the confirm decode is reachable ONLY on the active branch.
        // A failed row dispatches row-only on the snapshot's status alone — even a body
        // that WOULD satisfy the gate never diverts it into the active begin path.
        // (`inert_lifecycle` also `unreachable!()`s on begin, so begins==0 is doubly
        // guarded.)
        let reply = run(
            FakeReads::present(creator_facts(MaterializationStatus::Failed)),
            FakeTeardown::new(TeardownOutcome::Commit),
            inert_lifecycle(),
            inert_dispatch(),
            FakeResidue::new(false),
            inert_availability(),
            "a1",
            "",
            r#"{"confirm":true}"#,
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::NO_CONTENT);
        assert_eq!(reply.teardowns, 1);
        assert_eq!(reply.begins, 0);
    }

    #[tokio::test]
    async fn a_failed_row_with_a_positional_array_body_stays_row_only_204() {
        // The failed branch never decodes the body at all, so the `[true]` array that the
        // active branch now refuses (object-only floor, #2246) is equally inert here: the
        // row-only dispatch rides the snapshot status, not the body. begins==0 is doubly
        // guarded by `inert_lifecycle`'s `unreachable!()` on begin.
        let reply = run(
            FakeReads::present(creator_facts(MaterializationStatus::Failed)),
            FakeTeardown::new(TeardownOutcome::Commit),
            inert_lifecycle(),
            inert_dispatch(),
            FakeResidue::new(false),
            inert_availability(),
            "a1",
            "",
            "[true]",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::NO_CONTENT);
        assert_eq!(reply.teardowns, 1);
        assert_eq!(reply.begins, 0);
    }

    #[tokio::test]
    async fn a_failed_row_announce_failure_does_not_flip_the_204() {
        let reply = run(
            FakeReads::present(creator_facts(MaterializationStatus::Failed)),
            FakeTeardown::new(TeardownOutcome::Commit),
            inert_lifecycle(),
            inert_dispatch(),
            FakeResidue::new(true),
            inert_availability(),
            "a1",
            "",
            "",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::NO_CONTENT);
        assert_eq!(reply.announces, 1);
    }

    #[tokio::test]
    async fn a_row_only_teardown_that_finds_it_gone_is_the_idempotent_204() {
        let reply = run(
            FakeReads::present(creator_facts(MaterializationStatus::Failed)),
            FakeTeardown::new(TeardownOutcome::Gone),
            inert_lifecycle(),
            inert_dispatch(),
            inert_residue(),
            inert_availability(),
            "a1",
            "",
            "",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::NO_CONTENT);
        // Gone means no commit, so no announce.
        assert_eq!(reply.announces, 0);
    }

    #[tokio::test]
    async fn a_row_only_eligibility_move_is_409_materialization_in_progress() {
        let reply = run(
            FakeReads::present(creator_facts(MaterializationStatus::Failed)),
            FakeTeardown::new(TeardownOutcome::Moved),
            inert_lifecycle(),
            inert_dispatch(),
            inert_residue(),
            inert_availability(),
            "a1",
            "",
            "",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::CONFLICT);
        assert_eq!(reply.json()["code"], "materialization_in_progress");
        assert_eq!(reply.announces, 0);
    }

    #[tokio::test]
    async fn a_row_only_teardown_failure_is_a_safe_500() {
        let reply = run(
            FakeReads::present(creator_facts(MaterializationStatus::Failed)),
            FakeTeardown::new(TeardownOutcome::Fails),
            inert_lifecycle(),
            inert_dispatch(),
            inert_residue(),
            inert_availability(),
            "a1",
            "",
            "",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(!reply.body.contains("teardown boom"));
    }

    // ---- the active (full) branch: confirm gate ----

    async fn active_run(
        query: &str,
        body: &str,
        begin: BeginOutcome,
        dispatch: DeletionRunOutcome,
    ) -> Reply {
        active_run_with(query, body, begin, dispatch, inert_availability()).await
    }

    /// `active_run`, but with an explicit availability edge so the active-branch
    /// tests can assert the delete-began fire (and that a failing edge never flips
    /// the HTTP outcome).
    async fn active_run_with(
        query: &str,
        body: &str,
        begin: BeginOutcome,
        dispatch: DeletionRunOutcome,
        availability: FakeAvailability,
    ) -> Reply {
        run(
            FakeReads::present(creator_facts(MaterializationStatus::Active)),
            inert_teardown(),
            FakeLifecycle::new(begin),
            FakeDispatch::new(dispatch),
            inert_residue(),
            availability,
            "a1",
            query,
            body,
            true,
        )
        .await
    }

    #[tokio::test]
    async fn an_active_delete_without_confirm_is_400_and_never_begins() {
        for body in [
            "",
            "not json",
            "{}",
            r#"{"confirm":false}"#,
            r#"{"confirm":"true"}"#,
            "5",
            // The accepted-set falsifier: a bare derived `ConfirmBody` would bind the
            // JSON array `[true]` positionally to `confirm` and open the gate off a shape
            // TS's `z.object` rejects. The shared object-only floor (#2246) makes it a
            // decode fault like any other — one refusal, zero begins.
            "[true]",
        ] {
            let reply = active_run(
                "",
                body,
                BeginOutcome::Unused,
                DeletionRunOutcome::Completed,
            )
            .await;
            assert_eq!(
                reply.status,
                StatusCode::BAD_REQUEST,
                "body {body:?} must be 400"
            );
            assert_eq!(
                reply.json(),
                json!({ "error": "confirm flag required", "detail": "pass {\"confirm\": true}" }),
                "body {body:?}"
            );
            assert_eq!(reply.begins, 0, "body {body:?} must not begin");
        }
    }

    #[tokio::test]
    async fn confirm_true_with_extra_keys_still_passes_the_gate() {
        let reply = active_run(
            "",
            r#"{"confirm":true,"unknown":1}"#,
            BeginOutcome::Started,
            DeletionRunOutcome::Completed,
        )
        .await;
        assert_eq!(reply.status, StatusCode::NO_CONTENT);
        assert_eq!(reply.begins, 1);
        assert_eq!(reply.dispatches, 1);
    }

    // ---- the active branch: begin outcomes ----

    #[tokio::test]
    async fn a_begin_not_found_is_the_idempotent_204() {
        let reply = active_run(
            "",
            r#"{"confirm":true}"#,
            BeginOutcome::NotFound,
            DeletionRunOutcome::Completed,
        )
        .await;
        assert_eq!(reply.status, StatusCode::NO_CONTENT);
        assert_eq!(reply.begins, 1);
        assert_eq!(reply.dispatches, 0);
    }

    #[tokio::test]
    async fn a_begin_not_active_or_mid_transition_is_409_materialization_in_progress() {
        for begin in [
            BeginOutcome::NotActive,
            BeginOutcome::RuntimeTransitionInProgress,
        ] {
            let reply = active_run(
                "",
                r#"{"confirm":true}"#,
                begin,
                DeletionRunOutcome::Completed,
            )
            .await;
            assert_eq!(reply.status, StatusCode::CONFLICT);
            assert_eq!(reply.json()["code"], "materialization_in_progress");
            assert_eq!(reply.dispatches, 0);
        }
    }

    #[tokio::test]
    async fn an_already_pending_marker_without_retry_is_409_agent_deletion_in_progress() {
        let reply = active_run(
            "",
            r#"{"confirm":true}"#,
            BeginOutcome::AlreadyPending,
            DeletionRunOutcome::Completed,
        )
        .await;
        assert_eq!(reply.status, StatusCode::CONFLICT);
        assert_eq!(
            reply.json(),
            json!({ "error": "agent deletion is already in progress", "code": "agent_deletion_in_progress" })
        );
        // Without ?retry=true the resumer is never driven.
        assert_eq!(reply.dispatches, 0);
    }

    #[tokio::test]
    async fn an_already_pending_marker_with_retry_true_re_drives_the_resumer() {
        let reply = active_run(
            "retry=true",
            r#"{"confirm":true}"#,
            BeginOutcome::AlreadyPending,
            DeletionRunOutcome::Completed,
        )
        .await;
        assert_eq!(reply.status, StatusCode::NO_CONTENT);
        assert_eq!(reply.dispatches, 1);
    }

    #[tokio::test]
    async fn retry_only_matches_the_literal_true() {
        // `retry=1`/`retry=false`/absent are all "not a retry".
        for query in ["", "retry=false", "retry=1", "other=true"] {
            let reply = active_run(
                query,
                r#"{"confirm":true}"#,
                BeginOutcome::AlreadyPending,
                DeletionRunOutcome::Completed,
            )
            .await;
            assert_eq!(reply.status, StatusCode::CONFLICT, "query {query:?}");
            assert_eq!(reply.dispatches, 0, "query {query:?} must not re-drive");
        }
    }

    #[tokio::test]
    async fn a_begin_persistence_failure_is_a_safe_500() {
        let reply = active_run(
            "",
            r#"{"confirm":true}"#,
            BeginOutcome::Fails,
            DeletionRunOutcome::Completed,
        )
        .await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(!reply.body.contains("begin boom"));
        assert_eq!(reply.dispatches, 0);
    }

    // ---- the active branch: resumer outcomes ----

    #[tokio::test]
    async fn a_started_delete_that_completes_is_204() {
        let reply = active_run(
            "",
            r#"{"confirm":true}"#,
            BeginOutcome::Started,
            DeletionRunOutcome::Completed,
        )
        .await;
        assert_eq!(reply.status, StatusCode::NO_CONTENT);
        assert_eq!(reply.dispatches, 1);
    }

    #[tokio::test]
    async fn a_resumer_already_running_is_409_agent_deletion_in_progress() {
        let reply = active_run(
            "",
            r#"{"confirm":true}"#,
            BeginOutcome::Started,
            DeletionRunOutcome::AlreadyRunning,
        )
        .await;
        assert_eq!(reply.status, StatusCode::CONFLICT);
        assert_eq!(reply.json()["code"], "agent_deletion_in_progress");
    }

    #[tokio::test]
    async fn every_non_terminal_resumer_outcome_is_202_with_the_fixed_code_and_its_message() {
        let cases = [
            (
                DeletionRunOutcome::Closed,
                "agent deletion worker is shutting down",
            ),
            (
                DeletionRunOutcome::Superseded,
                "deletion attempt no longer matches agent placement or marker",
            ),
            (
                DeletionRunOutcome::Retained,
                "agent deletion could not be completed; retry later",
            ),
        ];
        for (outcome, message) in cases {
            let reply = active_run("", r#"{"confirm":true}"#, BeginOutcome::Started, outcome).await;
            assert_eq!(reply.status, StatusCode::ACCEPTED);
            assert_eq!(
                reply.json(),
                json!({ "error": message, "code": "agent_deletion_pending_retry" })
            );
        }
    }

    #[tokio::test]
    async fn a_faulted_resumer_outcome_is_a_detail_free_safe_500_not_a_202_retry() {
        // `Faulted` is a frozen non-hot-retriable internal contract fault, not a
        // client-retry signal: it must be a detail-free 500, never the 202 the other
        // non-terminal arms take.
        let reply = active_run(
            "",
            r#"{"confirm":true}"#,
            BeginOutcome::Started,
            DeletionRunOutcome::Faulted,
        )
        .await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply.body, "Internal Server Error");
        assert_eq!(reply.dispatches, 1);
    }

    // ---- P0-2: the row-only branch source-binds the committed/moved facts ----

    #[tokio::test]
    async fn a_committed_seal_for_another_agent_is_a_safe_500_with_zero_announce() {
        // A liar teardown port that seals a DIFFERENT agent than the request must
        // fail closed BEFORE any announce, so it can never fan one agent's deletion
        // out onto another agent's carriers.
        #[derive(Clone)]
        struct LiarTeardown {
            sealed: AgentId,
            calls: Arc<AtomicUsize>,
        }
        impl AgentTeardownUnitOfWork for LiarTeardown {
            async fn tear_down_agent(
                &self,
                _key: AgentTeardownKey,
                expected: AgentTeardownEligibility,
            ) -> Result<AgentTeardownCommit, TeardownFailure> {
                self.calls.fetch_add(1, SeqCst);
                assert_eq!(expected, AgentTeardownEligibility::RowOnly);
                // Seal a commit whose facts name `self.sealed`, NOT the request.
                Ok(committed_row_only(&self.sealed))
            }
            async fn cascade_computer(
                &self,
                _key: ComputerTeardownKey,
                _consent: CascadeConsent,
            ) -> Result<ComputerCascadeCommit, TeardownFailure> {
                unreachable!("no cascade")
            }
        }

        let residue = FakeResidue::new(false);
        let announce_calls = residue.calls.clone();
        let uri = "/api/agents/a1";
        let request = Request::delete(uri)
            .header("content-type", "application/json")
            .extension(user("viewer-1"))
            .body(Body::from(String::new()))
            .unwrap();
        let response = router(
            FakeReads::present(creator_facts(MaterializationStatus::Failed)),
            LiarTeardown {
                sealed: agent("a2"),
                calls: Arc::new(AtomicUsize::new(0)),
            },
            inert_lifecycle(),
            inert_dispatch(),
            residue,
            inert_availability(),
            FixedClock(NOW),
        )
        .oneshot(request)
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        // Zero announce: the source-bind failed before any external effect.
        assert_eq!(announce_calls.load(SeqCst), 0);
    }

    #[tokio::test]
    async fn a_matching_committed_seal_announces_the_deleted_agent() {
        // The announce must ride the committed seal's id. The fake reads back the
        // agent it was asked to announce; a matching seal announces exactly once for
        // the requested (== committed) agent.
        let residue = FakeResidue::new(false);
        let announce_agent = residue.last_agent();
        let reply = run(
            FakeReads::present(creator_facts(MaterializationStatus::Failed)),
            FakeTeardown::new(TeardownOutcome::Commit),
            inert_lifecycle(),
            inert_dispatch(),
            residue,
            inert_availability(),
            "a1",
            "",
            "",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::NO_CONTENT);
        assert_eq!(reply.announces, 1);
        assert_eq!(
            announce_agent.lock().expect("lock").as_ref(),
            Some(&agent("a1"))
        );
    }

    #[tokio::test]
    async fn a_moved_commit_for_another_agent_is_a_safe_500_not_a_409() {
        // A move decoded for a DIFFERENT agent is a resolver defect: safe 500, never
        // the product 409 a genuine same-agent move takes.
        #[derive(Clone)]
        struct MovedLiar;
        impl AgentTeardownUnitOfWork for MovedLiar {
            async fn tear_down_agent(
                &self,
                _key: AgentTeardownKey,
                _expected: AgentTeardownEligibility,
            ) -> Result<AgentTeardownCommit, TeardownFailure> {
                Ok(AgentTeardownCommit::Moved {
                    moved: EligibilityMoved::Materialization,
                    facts: Box::new(teardown_facts(&agent("a2"), MaterializationStatus::Active)),
                })
            }
            async fn cascade_computer(
                &self,
                _key: ComputerTeardownKey,
                _consent: CascadeConsent,
            ) -> Result<ComputerCascadeCommit, TeardownFailure> {
                unreachable!("no cascade")
            }
        }

        let request = Request::delete("/api/agents/a1")
            .header("content-type", "application/json")
            .extension(user("viewer-1"))
            .body(Body::from(String::new()))
            .unwrap();
        let response = router(
            FakeReads::present(creator_facts(MaterializationStatus::Failed)),
            MovedLiar,
            inert_lifecycle(),
            inert_dispatch(),
            inert_residue(),
            inert_availability(),
            FixedClock(NOW),
        )
        .oneshot(request)
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    // ---- P0-3: the delete-began availability edge ----

    #[tokio::test]
    async fn a_started_delete_fires_the_availability_edge_for_the_agent() {
        let availability = FakeAvailability::new(false);
        let reply = active_run_with(
            "",
            r#"{"confirm":true}"#,
            BeginOutcome::Started,
            DeletionRunOutcome::Completed,
            availability,
        )
        .await;
        assert_eq!(reply.status, StatusCode::NO_CONTENT);
        assert_eq!(reply.availabilities, 1);
        assert_eq!(reply.availability_agent.as_ref(), Some(&agent("a1")));
    }

    #[tokio::test]
    async fn a_failing_availability_edge_does_not_flip_the_started_outcome() {
        // The edge is best-effort: a failure is logged, never flips the HTTP result.
        let availability = FakeAvailability::new(true);
        let reply = active_run_with(
            "",
            r#"{"confirm":true}"#,
            BeginOutcome::Started,
            DeletionRunOutcome::Completed,
            availability,
        )
        .await;
        assert_eq!(reply.status, StatusCode::NO_CONTENT);
        assert_eq!(reply.availabilities, 1);
        assert_eq!(reply.dispatches, 1);
    }

    #[tokio::test]
    async fn an_already_pending_retry_re_drives_the_resumer_but_fires_no_fresh_edge() {
        // The `deletion_started_at` flip (and its edge) already happened on the
        // owning attempt; a ?retry=true re-drive must NOT re-fire the availability
        // edge.
        let availability = FakeAvailability::new(false);
        let reply = active_run_with(
            "retry=true",
            r#"{"confirm":true}"#,
            BeginOutcome::AlreadyPending,
            DeletionRunOutcome::Completed,
            availability,
        )
        .await;
        assert_eq!(reply.status, StatusCode::NO_CONTENT);
        assert_eq!(reply.dispatches, 1);
        assert_eq!(reply.availabilities, 0);
    }
}
