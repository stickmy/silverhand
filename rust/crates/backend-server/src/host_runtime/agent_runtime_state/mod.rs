//! Authenticated cold face for `GET /api/agents/{id}/runtime-state`: where one
//! agent participates and which durable runtimes it holds, redacted to the caller.
//!
//! The route is AUTHENTICATED and answers about a SPECIFIC agent, so it composes
//! its authorities in a fixed order, exactly like the sibling [`super::availability`]:
//!
//!  1. VISIBILITY ([`can_view_agent`], active-only) — an absent row, a caller
//!     outside the ACL, and a row whose stage this view hides all collapse to ONE
//!     byte-identical `404 {"error":"agent not found"}`, so the surface can never
//!     become an existence oracle.
//!  2. PAYLOAD — projected only once the caller is admitted, from the SAME snapshot
//!     the ACL was judged against.
//!
//! ## One snapshot closes the authority↔payload TOCTOU
//!
//! [`AgentRuntimeStateReadModel::read_agent_runtime_state`] resolves the agent's
//! visibility arms + stage, its COMPLETE participation set (each row carrying its
//! archived flag and this viewer's readability), and its durable seats with per-scope
//! undelivered depth on ONE `BEGIN DEFERRED` snapshot. Reading the authority and the
//! payload together is what stops an agent reparented — or a conversation archived —
//! between two reads from admitting a caller one read would hide. The bundle is sealed
//! to the ONE decoded `agents.id` the adapter read back; the handler binds that to the
//! id it was asked for BEFORE the ACL decision, so a resolver that answered for another
//! row is a composition defect ([`internal_error`]), never a product `404`.
//!
//! ## Presentation focus is ephemeral, generation-guarded, applied last
//!
//! A live agent's presentation focus is an in-memory fact of a running process, so it
//! is deliberately ABSENT from the snapshot bundle and read AFTER it, through the
//! runtime custody/focus ports. The guard is exact-generation: the live focus is
//! projected onto the agent-scope seat ONLY when the live principal's generation
//! matches the generation of the agent-scope seat IN THIS SNAPSHOT
//! (`AgentRuntimeStateFacts::agent_scope_generation`). A re-claim that raced the read
//! advances the generation, so its focus is ignored rather than spliced onto the stale
//! seat. The focus conversation's readability is resolved through the SAME strict
//! human-participation authority that redacts the participation set
//! ([`AgentRuntimeStateReadModel::read_conversation_visibility`]), so an unreadable
//! focus collapses to `other` — the agent is focused somewhere the caller may not name.
//!
//! Every post-snapshot port fault — the custody read, the focus read, or the focus
//! conversation's readability read — is the lane's fail-closed bare `500`, never a
//! fabricated or silently-dropped focus.
//!
//! Like the sibling faces, the human principal is proof the authenticating perimeter
//! ran; its ABSENCE is a composition fault (fail-closed `500`, reads nothing), not a
//! product request. Mounting the real bearer perimeter and the concrete ports is the
//! composition root's job, not this factory's.

use axum::{
    Json, Router,
    extract::{Extension, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Serialize;
use silverhand_backend_core::{
    foundation::identity::{AgentId, Principal, UserId},
    host_runtime::agent_runtime_state_read::{
        AgentRuntimeStateReadModel, AgentRuntimeStateView, PresentationFocus, RuntimeFocusView,
        RuntimeParticipationView, RuntimeSeatScopeView, RuntimeSeatView,
    },
    host_runtime::runtime_credential::{
        AgentSlotPresentationFocusPort, RuntimeCustodyReadPort, RuntimeGeneration, RuntimeScope,
    },
    identity_access::capability::agent_view::{VisibleStages, can_view_agent},
};

use crate::foundation::error;

/// The product route template, in axum's `{id}` capture syntax.
pub const RUNTIME_STATE_PATH: &str = "/api/agents/{id}/runtime-state";

/// The anti-enumeration refusal string, pinned to the TypeScript handler
/// (`c.json({ error: 'agent not found' }, 404)`) so a fixture drift shows here.
const AGENT_NOT_FOUND: &str = "agent not found";

/// The unmapped-error body: the product route registers no `onError`, so an
/// unexpected failure falls to the framework default — a `500` with a bare
/// `text/plain` `Internal Server Error`, never a JSON envelope.
const INTERNAL_ERROR_BODY: &str = "Internal Server Error";

/// The three authorities bundled into one cloneable router state: the source-bound
/// snapshot read model, the live runtime-custody read port, and the live
/// agent-slot presentation-focus port. The latter two are the SAME reuse-only ports
/// the runtime-switch saga consults — this face never mints write authority.
#[derive(Clone)]
struct RuntimeStateState<R, C, F> {
    cold_read: R,
    custody: C,
    focus: F,
}

pub fn router<R, C, F>(cold_read: R, custody: C, focus: F) -> Router
where
    R: AgentRuntimeStateReadModel + Clone + Send + Sync + 'static,
    C: RuntimeCustodyReadPort,
    F: AgentSlotPresentationFocusPort,
{
    Router::new()
        .route(RUNTIME_STATE_PATH, get(runtime_state::<R, C, F>))
        .with_state(RuntimeStateState {
            cold_read,
            custody,
            focus,
        })
}

async fn runtime_state<R, C, F>(
    Path(agent_id): Path<String>,
    viewer: Option<Extension<UserId>>,
    State(state): State<RuntimeStateState<R, C, F>>,
) -> Response
where
    R: AgentRuntimeStateReadModel,
    C: RuntimeCustodyReadPort,
    F: AgentSlotPresentationFocusPort,
{
    let Some(Extension(viewer)) = viewer else {
        tracing::error!("runtime-state route reached without its human principal fact");
        return internal_error();
    };
    let principal = Principal::human(viewer.clone());

    // A malformed id names no agent — the same 404 an absent row gets, so the
    // surface never distinguishes "unparseable" from "not there".
    let Ok(agent) = AgentId::parse(agent_id) else {
        return not_found();
    };

    // The whole source-bound bundle on ONE snapshot, read BEFORE the gate: the ACL
    // arms + stage, the complete redacted participation set, and the durable seats.
    let facts = match state
        .cold_read
        .read_agent_runtime_state(&agent, &viewer)
        .await
    {
        Ok(Some(facts)) => facts,
        // The ACL's absence arm — never a failed read — is the anti-enumeration 404.
        Ok(None) => return not_found(),
        Err(cause) => {
            tracing::error!(
                error_chain =
                    crate::foundation::safe_log_field::safe_display(&(error::describe(&cause))),
                "runtime-state snapshot read failed"
            );
            return internal_error();
        }
    };

    // Bind the resolved row to the request BEFORE the gate: a bundle whose decoded id
    // is not the one asked for would judge the ACL against one agent and project a
    // payload from another — a resolver defect that takes the safe 500, not a 404.
    if facts.source() != &agent {
        tracing::error!(
            requested = agent.as_str(),
            resolved = facts.source().as_str(),
            "runtime-state snapshot resolved for the wrong agent"
        );
        return internal_error();
    }

    // Access precedes product: absence, non-membership, and a hidden stage alike all
    // collapse to one anti-enumeration 404.
    let decision = can_view_agent(
        &principal,
        &agent,
        Some(facts.visibility()),
        &VisibleStages::active_only(),
    );
    if !decision.is_allowed() {
        return not_found();
    }

    // The live presentation focus is read AFTER the snapshot and guarded by the
    // snapshot's agent-scope generation, so a re-claim that raced the read can never
    // splice a newer generation's focus onto this snapshot's seat.
    let focus = match resolve_focus(&state, &agent, &viewer, facts.agent_scope_generation()).await {
        Ok(focus) => focus,
        Err(cause) => {
            tracing::error!(
                error_chain =
                    crate::foundation::safe_log_field::safe_display(&(error::describe(&cause))),
                "runtime-state live focus resolution failed"
            );
            return internal_error();
        }
    };

    Json(RuntimeStateResponse::from(facts.project(focus))).into_response()
}

/// Resolve the generation-guarded live presentation focus for the agent-scope seat.
///
/// The guard is exact-generation: the live focus is admitted ONLY when the live
/// principal's generation matches `snapshot_generation` (the agent-scope seat's
/// generation in this snapshot). Every other outcome — no agent-scope seat, no live
/// principal, a mismatched (raced) generation, or a process reporting no focus —
/// resolves to [`PresentationFocus::None`], which the pure projection renders as a
/// null focus. Any port fault propagates as the caller's safe 500.
async fn resolve_focus<R, C, F>(
    state: &RuntimeStateState<R, C, F>,
    agent: &AgentId,
    viewer: &UserId,
    snapshot_generation: Option<RuntimeGeneration>,
) -> Result<PresentationFocus, silverhand_backend_core::foundation::persistence::PersistenceFailure>
where
    R: AgentRuntimeStateReadModel,
    C: RuntimeCustodyReadPort,
    F: AgentSlotPresentationFocusPort,
{
    // No agent-scope seat in the snapshot ⇒ nothing for a live focus to attach to.
    let Some(snapshot_generation) = snapshot_generation else {
        return Ok(PresentationFocus::None);
    };

    let scope = RuntimeScope::AgentSlot {
        agent: agent.clone(),
    };
    let Some(live_principal) = state.custody.current(scope).await? else {
        // No live process owns the agent slot ⇒ no focus.
        return Ok(PresentationFocus::None);
    };

    // The exact-generation guard: a live focus is admitted ONLY on a generation match.
    // A re-claim that raced the snapshot advanced the generation, so its focus is
    // ignored rather than spliced onto the stale seat.
    if live_principal.custody().generation() != snapshot_generation {
        return Ok(PresentationFocus::None);
    }

    let Some(conversation) = state
        .focus
        .agent_slot_presentation_focus(live_principal)
        .await?
    else {
        // The live process reports no focused conversation.
        return Ok(PresentationFocus::None);
    };

    // Readability rides the SAME strict human-participation authority that redacts the
    // participation set — asked once here for the conversation whose identity is only
    // known after the snapshot. An unreadable focus collapses to `other` downstream.
    let viewer_can_read = state
        .cold_read
        .read_conversation_visibility(viewer, &conversation)
        .await?;
    Ok(PresentationFocus::Live {
        conversation,
        viewer_can_read,
    })
}

/// The success body: the two orthogonal read models the wire returns, mirroring the
/// TypeScript `{ participations, visible_participations, seats }`.
#[derive(Serialize)]
struct RuntimeStateResponse {
    participations: Vec<ParticipationDto>,
    visible_participations: Vec<ParticipationDto>,
    seats: Vec<SeatDto>,
}

impl From<AgentRuntimeStateView> for RuntimeStateResponse {
    fn from(view: AgentRuntimeStateView) -> Self {
        Self {
            participations: view
                .participations
                .iter()
                .map(ParticipationDto::from_view)
                .collect(),
            visible_participations: view
                .visible_participations
                .iter()
                .map(ParticipationDto::from_view)
                .collect(),
            seats: view.seats.iter().map(SeatDto::from_view).collect(),
        }
    }
}

/// One participation row, keyed exactly as the TS wire (`conversation_id`,
/// `channel_id` — both the conversation id — plus the `conversation_`-prefixed
/// placement/roster tokens the TS `SELECT ... AS` aliases produce).
#[derive(Serialize)]
struct ParticipationDto {
    conversation_id: String,
    workspace_id: Option<String>,
    workspace_name: Option<String>,
    conversation_placement_kind: String,
    conversation_roster_policy: &'static str,
    channel_id: String,
    channel_title: String,
    /// Closing is retired; deployed clients still read the field.
    closed_at: Option<i64>,
}

impl ParticipationDto {
    fn from_view(view: &RuntimeParticipationView) -> Self {
        Self {
            conversation_id: view.conversation.as_str().to_owned(),
            workspace_id: view.workspace_id.clone(),
            workspace_name: view.workspace_name.clone(),
            conversation_placement_kind: view.placement_kind.clone(),
            conversation_roster_policy: view.roster_policy.as_str(),
            channel_id: view.conversation.as_str().to_owned(),
            channel_title: view.channel_title.clone(),
            closed_at: None,
        }
    }
}

/// One durable seat, mirroring the TS wire seat object.
#[derive(Serialize)]
struct SeatDto {
    scope: ScopeDto,
    generation: i64,
    status: &'static str,
    pid: Option<i64>,
    cli_session_id: Option<String>,
    focus: Option<FocusDto>,
    queue_depth: i64,
}

impl SeatDto {
    fn from_view(view: &RuntimeSeatView) -> Self {
        Self {
            scope: ScopeDto::from_view(&view.scope),
            generation: view.generation.as_i64(),
            status: view.status.as_str(),
            pid: view.pid,
            cli_session_id: view.cli_session_id.clone(),
            focus: view.focus.as_ref().map(FocusDto::from_view),
            queue_depth: view.queue_depth,
        }
    }
}

/// The seat's scope: `{ "kind": "agent" }` or
/// `{ "kind": "conversation", "conversation_id": ... }`.
#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ScopeDto {
    Agent,
    Conversation { conversation_id: String },
}

impl ScopeDto {
    fn from_view(scope: &RuntimeSeatScopeView) -> Self {
        match scope {
            RuntimeSeatScopeView::Agent => Self::Agent,
            RuntimeSeatScopeView::Conversation(conversation) => Self::Conversation {
                conversation_id: conversation.as_str().to_owned(),
            },
        }
    }
}

/// The seat's presentation focus: `{ "kind": "conversation", "conversation_id": ... }`
/// for a readable focus, `{ "kind": "other" }` when the focus conversation is redacted.
/// A seat with no focus renders the whole field as `null` (the `Option` wrapper).
#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum FocusDto {
    Conversation { conversation_id: String },
    Other,
}

impl FocusDto {
    fn from_view(focus: &RuntimeFocusView) -> Self {
        match focus {
            RuntimeFocusView::Conversation(conversation) => Self::Conversation {
                conversation_id: conversation.as_str().to_owned(),
            },
            RuntimeFocusView::Other => Self::Other,
        }
    }
}

/// The flat refusal body, mirroring the product `{ "error": ... }`.
#[derive(Serialize)]
struct ErrorBody {
    error: &'static str,
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

fn internal_error() -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, INTERNAL_ERROR_BODY).into_response()
}

#[cfg(test)]
mod tests;
