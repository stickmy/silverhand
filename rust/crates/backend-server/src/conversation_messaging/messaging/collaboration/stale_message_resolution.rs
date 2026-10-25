//! Internal agent acknowledgement that one stale draft became obsolete.
//!
//! This route has no message write, daemon command or domain aftermath. It
//! authenticates the runtime bearer, admits a bounded request against the
//! existing post authority and independent `send_messages` capability, then
//! synchronously emits the one warning record the TypeScript service owns.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Extension, Path, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use garde::Validate;
use serde::Deserialize;
use serde_json::{Value, json};
use silverhand_backend_core::{
    conversation_messaging::messaging::{
        CapabilityDenied, SendCapability,
        collaboration::stale_message_resolution::{
            BoundStaleMessageResolutionAdmission, StaleMessageResolutionAdmissionPort,
            StaleMessageResolutionAdmissionRefusal,
        },
    },
    foundation::cli_invoke_audit::CliStaleResolutionReason,
    foundation::identity::{AgentId, ConversationId},
    host_runtime::runtime_credential::RuntimePrincipal,
};

use crate::foundation::cli_invoke_observer::CliInvokeRequestContext;

use super::json_body::{BodyRead, payload_too_large, read_body};
use crate::foundation::http_error::plain_500 as internal;

pub const STALE_MESSAGE_RESOLUTION_PATH: &str = "/internal/agents/{aid}/stale-message-resolution";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StaleMessageResolutionObservation {
    agent: AgentId,
    conversation: ConversationId,
    stale_event_id: String,
}

impl StaleMessageResolutionObservation {
    fn from_bound(admission: BoundStaleMessageResolutionAdmission, stale_event_id: String) -> Self {
        let (agent, conversation) = admission.into_parts();
        Self {
            agent,
            conversation,
            stale_event_id,
        }
    }

    pub fn agent(&self) -> &AgentId {
        &self.agent
    }

    pub fn source_conversation(&self) -> &ConversationId {
        &self.conversation
    }

    pub fn target_conversation(&self) -> &ConversationId {
        &self.conversation
    }

    pub fn stale_event_id(&self) -> &str {
        &self.stale_event_id
    }

    pub const fn resolution_reason(&self) -> &'static str {
        "obsolete"
    }

    pub const fn outcome(&self) -> &'static str {
        "obsolete"
    }
}

/// Synchronous and infallible by design: production is one `tracing::warn!`.
/// A durable or fallible port here would invent an aftermath the product does
/// not have.
pub trait StaleMessageResolutionObserver {
    fn stale_message_resolved(&self, observation: &StaleMessageResolutionObservation);
}

/// The production observer: the sole real effect is the one warning line whose
/// schema this module owns. The durable resolution record belongs to the
/// TypeScript service, so this stays synchronous, infallible and zero-state — a
/// durable or fallible observer would invent an aftermath the product lacks.
///
/// The TS durable record carries `messageId`/`messageSeq` as `null` for a stale
/// resolution (the draft never became a message). That is not a fact THIS route
/// produces, so the Rust observer projects those nulls as field ABSENCE — the
/// tracing-native shape for "no value" (stable `Option<T>: Value` skips a `None`
/// rather than emitting typed null). This is a named projection difference, not
/// exact-null parity, and it keeps the six real top-level fields queryable.
#[derive(Clone, Copy, Debug, Default)]
pub struct TracingStaleMessageResolutionObserver;

impl StaleMessageResolutionObserver for TracingStaleMessageResolutionObserver {
    fn stale_message_resolved(&self, observation: &StaleMessageResolutionObservation) {
        tracing::warn!(
            agent = observation.agent().as_str(),
            source_conversation = observation.source_conversation().as_str(),
            target_conversation = observation.target_conversation().as_str(),
            stale_event_id = observation.stale_event_id(),
            reason = observation.resolution_reason(),
            outcome = observation.outcome(),
            "stale message resolution recorded"
        );
    }
}

struct Inner<A, C, O> {
    admission: A,
    capability: C,
    observer: O,
}

struct ResolutionState<A, C, O>(Arc<Inner<A, C, O>>);

impl<A, C, O> Clone for ResolutionState<A, C, O> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

pub fn router<A, C, O>(admission: A, capability: C, observer: O) -> Router
where
    A: StaleMessageResolutionAdmissionPort + Send + Sync + 'static,
    C: SendCapability + Send + Sync + 'static,
    O: StaleMessageResolutionObserver + Send + Sync + 'static,
{
    Router::new()
        .route(STALE_MESSAGE_RESOLUTION_PATH, post(resolve::<A, C, O>))
        .with_state(ResolutionState(Arc::new(Inner {
            admission,
            capability,
            observer,
        })))
}

async fn resolve<A, C, O>(
    State(state): State<ResolutionState<A, C, O>>,
    principal: Option<Extension<RuntimePrincipal>>,
    Path(raw_agent): Path<String>,
    request: Request,
) -> Response
where
    A: StaleMessageResolutionAdmissionPort + Send + Sync,
    C: SendCapability + Send + Sync,
    O: StaleMessageResolutionObserver + Send + Sync,
{
    let Some(Extension(principal)) = principal else {
        tracing::error!("stale-message resolution route reached without runtime principal");
        return internal();
    };
    let Ok(path_agent) = AgentId::parse(raw_agent) else {
        return not_found();
    };
    if principal.agent() != &path_agent {
        return not_found();
    }

    let audit = request
        .extensions()
        .get::<CliInvokeRequestContext>()
        .cloned();

    let request = match request_json(request).await {
        Ok(value) => value,
        Err(response) => return *response,
    };
    let admitted = match admit_request(request) {
        Ok(admitted) => admitted,
        Err(()) => return crate::foundation::wire_admission::invalid_input(),
    };

    let source = match state
        .0
        .admission
        .admit(principal.agent(), &admitted.conversation)
        .await
    {
        Ok(Ok(source)) => source,
        Ok(Err(StaleMessageResolutionAdmissionRefusal::Forbidden)) => return forbidden(),
        Err(failure) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&failure))
                ),
                "stale-message resolution post authority failed"
            );
            return internal();
        }
    };
    let bound = match source.bind(principal.agent(), &admitted.conversation) {
        Ok(bound) => bound,
        Err(failure) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&failure))
                ),
                "stale-message resolution admission identity seal failed"
            );
            return internal();
        }
    };

    match state
        .0
        .capability
        .may_send_messages(principal.agent(), &admitted.conversation)
        .await
    {
        Ok(Ok(())) => {}
        Ok(Err(CapabilityDenied)) => return capability_denied(),
        Err(failure) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&failure))
                ),
                "stale-message resolution capability authority failed"
            );
            return internal();
        }
    }

    if let Some(audit) = audit {
        audit.set_stale_reason(CliStaleResolutionReason::Obsolete);
    }

    let observation = StaleMessageResolutionObservation::from_bound(bound, admitted.stale_event_id);
    state.0.observer.stale_message_resolved(&observation);
    (StatusCode::OK, Json(json!({"ok":true}))).into_response()
}

struct AdmittedRequest {
    conversation: ConversationId,
    stale_event_id: String,
}

async fn request_json(request: Request) -> Result<Value, Box<Response>> {
    match read_body(request).await {
        BodyRead::Within(body) => Ok(serde_json::from_slice(&body).unwrap_or_else(|_| json!({}))),
        BodyRead::OverLimit => Err(Box::new(payload_too_large())),
        BodyRead::Unreadable => Ok(json!({})),
    }
}

/// The strict typed envelope this route accepts. All three members are REQUIRED — none
/// is `.optional()` in the reference — so a missing or wrong-typed one is a native decode
/// fault, and the two identifiers carry the `.min(1)` half of their `z.string().min(1)` as
/// a stateless garde rule. It has no `deny_unknown_fields`: the reference schema is not
/// `.strict()`, so an unknown member is stripped rather than refused.
#[derive(Debug, Deserialize, Validate)]
struct StaleResolutionBody {
    #[garde(length(min = 1))]
    conversation_id: String,
    #[garde(length(min = 1))]
    stale_event_id: String,
    #[garde(skip)]
    resolution_reason: ObsoleteReason,
}

/// The one resolution a stale draft may report here: `obsolete`, the reference's
/// `z.literal('obsolete')`. A single-variant enum makes every other string a native
/// decode fault rather than a hand-rolled literal check.
#[derive(Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum ObsoleteReason {
    Obsolete,
}

impl TryFrom<StaleResolutionBody> for AdmittedRequest {
    type Error = ();

    fn try_from(raw: StaleResolutionBody) -> Result<Self, Self::Error> {
        // Shape-checked and dropped: decoding `resolution_reason` as the single-variant
        // `ObsoleteReason` IS the `z.literal('obsolete')` rule, and the route reports the
        // outcome from its own const, so the field carries no further.
        let ObsoleteReason::Obsolete = raw.resolution_reason;
        // The `.min(1)` already ran, so this parse cannot fail on emptiness; it is the
        // domain id's own authority all the same, and its fallible arm collapses to the
        // one semantic refusal rather than an `expect`.
        let conversation = ConversationId::parse(raw.conversation_id).map_err(|_| ())?;
        Ok(AdmittedRequest {
            conversation,
            stale_event_id: raw.stale_event_id,
        })
    }
}

/// Decode the body into its typed envelope, enforce the stateless rules, then normalize.
/// Any fault — structural, semantic, or the id parse — is the one semantic refusal; an
/// unreadable body arrived as an empty object, so its required fields are simply missing.
///
/// The top-level object floor (rejecting a value-shaped array serde would bind to the
/// struct positionally) is the shared [`ObjectOnly<T>`](crate::foundation::wire_admission::ObjectOnly)
/// map-only wrapper from the `ValidatedJson` anchor (#2246), consumed here rather than
/// restated as a route-local `Value` check. This route decodes Value-first to keep the
/// bounded-body/identity ordering, so duplicate keys were already collapsed last-wins at the
/// `serde_json::from_slice::<Value>` parse — a route-order exception, not native duplicate
/// rejection.
fn admit_request(value: Value) -> Result<AdmittedRequest, ()> {
    let crate::foundation::wire_admission::ObjectOnly(raw): crate::foundation::wire_admission::ObjectOnly<
        StaleResolutionBody,
    > = serde_json::from_value(value).map_err(|_| ())?;
    raw.validate().map_err(|_| ())?;
    AdmittedRequest::try_from(raw)
}

fn capability_denied() -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({
            "code":"capability_denied",
            "capability":"send_messages",
            "message":"missing 'send_messages' capability in this workspace",
            "details":{"capability":"send_messages"}
        })),
    )
        .into_response()
}

fn forbidden() -> Response {
    (StatusCode::FORBIDDEN, Json(json!({"error":"forbidden"}))).into_response()
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, Json(json!({"error":"not found"}))).into_response()
}

#[cfg(test)]
mod tests;
