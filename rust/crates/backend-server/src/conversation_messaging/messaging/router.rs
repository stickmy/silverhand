//! The product create-message router: the surface the two doors hang on.
//!
//! This is the module's only public face. Mounting the doors here gives the admission
//! and execute stages real callers, which is why the transient `dead_code` allows die
//! with this file rather than before it — a lint quieted by a fake call site would have
//! hidden exactly this gap.
//!
//! This library defines the boundary but does not own its process. The executable app
//! composes the two routers below with different real authentication authorities; a
//! library caller that uses [`send_router`] directly must still supply principal facts.
//!
//! Every outcome has an answer, and each door answers its OWN: a person's committed send
//! becomes 201 and the acknowledgement ([`super::ack`]), a runtime's becomes 200 and the flat
//! coordinate report ([`super::agent_output`]), and a failed one becomes its refusal
//! ([`super::outcome`]).
//!
//! IDENTITY ARRIVES ALREADY AUTHENTICATED. A parent authentication middleware verifies
//! the caller and injects a request-scoped principal fact; a handler here only READS
//! one. That is a different question from this route's own authorization, which is the
//! human door's 404/403 and the agent door's target capability. Conflating them would
//! put a second auth boundary inside the send path.

use axum::{
    Json, Router,
    extract::{Path, Request, State},
    http::StatusCode,
    middleware,
    response::{IntoResponse, Response},
    routing::post,
};
use silverhand_backend_core::{
    conversation_messaging::messaging::{
        AddressRefusal, ChannelAccess, ChannelAccessRefusal, DirectLineAuthority, SendCapability,
        SendMessageOutcome, SendMessageUnitOfWork, TargetAddress,
    },
    foundation::identity::{ConversationId, UserId},
    host_runtime::runtime_credential::RuntimePrincipal,
};
use std::sync::Arc;

use super::{
    ack::send_ack,
    adapters::{AgentRefusal, HumanRefusal, SendPorts, agent_send_observed, human_send},
    admission::PayloadRefusal,
    aftermath::SendAftermath,
    agent_output::agent_send_output,
    envelope::fence_message_send_body,
    outcome::refused_send,
    references::MarkdownReferencePositions,
    send_request::{
        MESSAGE_SEND_MAX_BODY_BYTES, agent_content_too_large, human_content_too_large,
        missing_identifier,
    },
    target_refusal_wire::TargetRefusalWire,
};
use crate::foundation::cli_invoke_observer::CliInvokeRequestContext;
use crate::foundation::http_error::coded_error as gate_code;

/// Everything the doors need, supplied by whoever composes them.
///
/// THE POSITION RULE IS NOT HERE, and that is the point. Where a body may address from is
/// product semantics — an autolink in prose, never quoted syntax and never someone else's
/// quoted words — so a composition that could swap it could decide that a name inside a
/// code span summons its subject, or that no body addresses anyone. The route uses the one
/// canonical rule and offers no way to replace it. The trait seam still exists in the domain,
/// where it lets the derivation be tested against a stand-in; a deployer is not a caller of
/// that seam.
pub struct SendState<C, T, K, D, U, F> {
    pub channel_access: C,
    pub target_address: T,
    pub capability: K,
    pub authority: D,
    pub unit_of_work: U,
    pub aftermath: F,
}

/// The product routes, on the paths the reference uses.
pub fn send_router<C, T, K, D, U, F>(state: SendState<C, T, K, D, U, F>) -> Router
where
    C: ChannelAccess + Send + Sync + 'static,
    T: TargetAddress + Send + Sync + 'static,
    K: SendCapability + Send + Sync + 'static,
    D: DirectLineAuthority + Send + Sync + 'static,
    U: SendMessageUnitOfWork + Send + Sync + 'static,
    F: SendAftermath + Send + Sync + 'static,
{
    let (human, agent) = send_routers(state);
    human.merge(agent)
}

/// The two authenticated product doors, separated so the executable composition
/// can apply the human-session authority only to the public route and the runtime
/// credential authority only to the internal route. Both routers retain one shared
/// application state; this is an authentication boundary, not two product models.
pub fn send_routers<C, T, K, D, U, F>(state: SendState<C, T, K, D, U, F>) -> (Router, Router)
where
    C: ChannelAccess + Send + Sync + 'static,
    T: TargetAddress + Send + Sync + 'static,
    K: SendCapability + Send + Sync + 'static,
    D: DirectLineAuthority + Send + Sync + 'static,
    U: SendMessageUnitOfWork + Send + Sync + 'static,
    F: SendAftermath + Send + Sync + 'static,
{
    let state = Arc::new(state);
    let human = Router::new()
        .route(
            "/api/channels/{id}/messages",
            post(human_route::<C, T, K, D, U, F>),
        )
        .with_state(Arc::clone(&state))
        .layer(middleware::from_fn(fence_message_send_body));
    let agent = Router::new()
        .route(
            "/internal/agents/{aid}/send-message",
            post(agent_route::<C, T, K, D, U, F>),
        )
        .with_state(state)
        .layer(middleware::from_fn(fence_message_send_body));
    (human, agent)
}

type Shared<C, T, K, D, U, F> = State<Arc<SendState<C, T, K, D, U, F>>>;

async fn human_route<C, T, K, D, U, F>(
    Path(id): Path<String>,
    State(state): Shared<C, T, K, D, U, F>,
    request: Request,
) -> Response
where
    C: ChannelAccess + Send + Sync + 'static,
    T: TargetAddress + Send + Sync + 'static,
    K: SendCapability + Send + Sync + 'static,
    D: DirectLineAuthority + Send + Sync + 'static,
    U: SendMessageUnitOfWork + Send + Sync + 'static,
    F: SendAftermath + Send + Sync + 'static,
{
    // The principal is read, never derived. Absent means the composition is wrong.
    let Some(author) = request.extensions().get::<UserId>().cloned() else {
        return composition_failure("human principal");
    };
    // The channel this send is for is the caller's own coordinate, checked by the gate
    // below rather than trusted here.
    let Ok(channel) = ConversationId::parse(id) else {
        return missing_identifier("channel id required").into_response();
    };
    let body = collected(request).await;

    match human_send(
        &channel,
        &author,
        body,
        &state.channel_access,
        SendPorts {
            positions: &MarkdownReferencePositions,
            authority: &state.authority,
            unit_of_work: &state.unit_of_work,
            aftermath: &state.aftermath,
        },
    )
    .await
    {
        Ok(outcome) => created(&outcome),
        Err(refusal) => human_wire(refusal),
    }
}

async fn agent_route<C, T, K, D, U, F>(
    State(state): Shared<C, T, K, D, U, F>,
    request: Request,
) -> Response
where
    C: ChannelAccess + Send + Sync + 'static,
    T: TargetAddress + Send + Sync + 'static,
    K: SendCapability + Send + Sync + 'static,
    D: DirectLineAuthority + Send + Sync + 'static,
    U: SendMessageUnitOfWork + Send + Sync + 'static,
    F: SendAftermath + Send + Sync + 'static,
{
    let Some(credential) = request.extensions().get::<RuntimePrincipal>().cloned() else {
        return composition_failure("agent credential");
    };
    let audit = request
        .extensions()
        .get::<CliInvokeRequestContext>()
        .cloned();
    let body = collected(request).await;

    match agent_send_observed(
        &credential,
        body,
        &state.target_address,
        &state.capability,
        SendPorts {
            positions: &MarkdownReferencePositions,
            authority: &state.authority,
            unit_of_work: &state.unit_of_work,
            aftermath: &state.aftermath,
        },
        audit.as_ref(),
    )
    .await
    {
        Ok(outcome) => sent(&outcome),
        Err(refusal) => agent_wire(refusal),
    }
}

/// The fence already bounded this body, so reading it here cannot be unbounded. An
/// unparseable body becomes `None`, which admission answers for.
async fn collected(request: Request) -> Option<serde_json::Value> {
    let bytes = axum::body::to_bytes(request.into_body(), MESSAGE_SEND_MAX_BODY_BYTES)
        .await
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// A principal the composition was supposed to supply is missing.
///
/// Reported ONCE and never to the caller, and deliberately NOT degraded into an
/// anonymous or default identity: attributing a send to the wrong principal is worse
/// than refusing it.
fn composition_failure(missing: &str) -> Response {
    tracing::error!(
        missing,
        "message-send route reached without its principal fact"
    );
    internal("Internal Server Error")
}

/// The HUMAN door's committed answer: 201 and the acknowledgement its sender reads.
///
/// A strict superset of the frame the conversation receives, so the renderer applies both
/// through one seam.
fn created(outcome: &SendMessageOutcome) -> Response {
    (StatusCode::CREATED, Json(send_ack(outcome))).into_response()
}

/// The AGENT door's committed answer: 200 and the flat coordinate report its own schema
/// publishes.
///
/// TWO CONTRACTS, ONE OUTCOME. Both doors project from the same `SendMessageOutcome` and
/// share the one sender-only projector, and there the sharing stops: a runtime reads
/// `message_id`/`message_seq`/`conversation_id` to advance its own cursor and has no schema
/// for a message DTO or cluster projections, so answering it with the person's ACK is a
/// client break rather than a simplification.
/// The statuses differ for the same reason the bodies do — each is what its own contract published.
fn sent(outcome: &SendMessageOutcome) -> Response {
    (StatusCode::OK, Json(agent_send_output(outcome))).into_response()
}

/// The HUMAN door's gate refusals carry an `error` sentence and nothing else, because the
/// reference's own bodies here have none either — that door's two answers are about a channel
/// the caller named, and a code would be a distinction it does not draw.
fn gate_refusal(status: StatusCode, error: &'static str) -> Response {
    (status, Json(serde_json::json!({ "error": error }))).into_response()
}

/// The AGENT door's ADDRESS refusals, which DO carry a code: an agent acts on which refusal it
/// got, and the CLI turns each into a different instruction — rewrite the address, or stop and
/// report.
pub(crate) fn address_refusal(refusal: AddressRefusal) -> Response {
    let refusal = match refusal {
        AddressRefusal::Malformed => TargetRefusalWire::Malformed,
        AddressRefusal::SelfPair => TargetRefusalWire::SelfPairNotAddressable,
        AddressRefusal::Unresolved => TargetRefusalWire::Unresolved,
        AddressRefusal::DirectMessageScope => TargetRefusalWire::DirectMessageScope,
        AddressRefusal::ThreadNotInTarget => TargetRefusalWire::ThreadNotInTarget,
    };
    refusal.into_response()
}

/// The capability this door asks for. Named here rather than carried through the port: which
/// grant a send needs is this door's own fact, and a port that reported it could report a
/// different one.
const SEND_CAPABILITY: &str = "send_messages";

fn capability_denied() -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(serde_json::json!({
            "code": "capability_denied",
            "capability": SEND_CAPABILITY,
            "message": format!("missing '{SEND_CAPABILITY}' capability in this workspace"),
            "details": { "capability": SEND_CAPABILITY },
        })),
    )
        .into_response()
}

fn internal(body: &'static str) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        body,
    )
        .into_response()
}

fn authority_failure(cause: &crate::foundation::error::BoundaryFailure) -> Response {
    tracing::error!(
        error_chain = crate::foundation::safe_log_field::safe_display(
            &(crate::foundation::error::describe(cause))
        ),
        "message-send pre-send authority failed"
    );
    internal("Internal Server Error")
}

/// Each door maps ITS OWN reasons to its own words, and that mapping is the only thing
/// allowed to differ — which is why it lives at the route, not in the door. Neither
/// function has an arm for a reason its door cannot produce, because neither refusal
/// type can carry one.
fn human_wire(refusal: HumanRefusal) -> Response {
    match refusal {
        HumanRefusal::Access(ChannelAccessRefusal::NotFound) => {
            gate_refusal(StatusCode::NOT_FOUND, "channel not found")
        }
        HumanRefusal::Access(ChannelAccessRefusal::Forbidden) => {
            gate_refusal(StatusCode::FORBIDDEN, "forbidden")
        }
        HumanRefusal::Authority(cause) => authority_failure(&cause),
        HumanRefusal::Payload(PayloadRefusal::Invalid) => {
            crate::foundation::wire_admission::invalid_input()
        }
        HumanRefusal::Payload(PayloadRefusal::ContentTooLarge { actual_bytes }) => {
            human_content_too_large(actual_bytes).into_response()
        }
        HumanRefusal::Send(cause) => refused_send(&cause),
    }
}

fn agent_wire(refusal: AgentRefusal) -> Response {
    match refusal {
        AgentRefusal::Payload(PayloadRefusal::Invalid) => {
            crate::foundation::wire_admission::invalid_input()
        }
        AgentRefusal::Payload(PayloadRefusal::ContentTooLarge { actual_bytes }) => {
            agent_content_too_large(actual_bytes).into_response()
        }
        AgentRefusal::Address(refusal) => address_refusal(refusal),
        AgentRefusal::OutOfSessionScope => gate_code(
            StatusCode::FORBIDDEN,
            "this operation acts only in the conversation your session is anchored to",
            "slot_out_of_scope",
        ),
        // ITS OWN BODY SHAPE, and not by accident: no `error` member, and the missing bucket
        // named twice. The CLI preserves the code verbatim only when it is present — a codeless
        // 403 degrades to `forbidden` — and it reads the bucket to tell the agent what a human
        // has to grant.
        AgentRefusal::CapabilityDenied => capability_denied(),
        AgentRefusal::Authority(cause) => authority_failure(&cause),
        AgentRefusal::Send(cause) => refused_send(&cause),
    }
}
