//! The wire projection of a send that reached the write unit.
//!
//! TWO KINDS OF ANSWER, and they do not share a renderer. A typed refusal is the caller's
//! own answer — their coordinate, their attachment, their staleness — so it is rendered and
//! NOT logged: an operator who sees a line per refused send learns nothing and loses the
//! failures that matter. Our own failures (a violated invariant, an unavailable host, a
//! broken store) are the opposite: logged once with the whole chain, answered with a safe
//! 500 that carries none of it. Funnelling both through one function is what makes the two
//! drift into each other.
//!
//! THE STATUS, THE CODE AND THE SENTENCE LIVE HERE, not on the domain refusal. The core
//! refusals carry a business reason for an operator to read in a source chain; a client's
//! contract is this module's. Both happen to word most of them the same way, and that is
//! deliberate rather than shared: a doc tweak on a domain error must not be able to change
//! what a client parses.
//!
//! WHAT IS NOT HERE: the acknowledgement of a committed send, which needs the shared
//! author projection and the fact's terminal shape. This module answers refusals only.
//!
//! One reference refusal has NO arm here on purpose. `target_not_allowed_for_user` fires
//! when a person's send addresses a foreign stream; the human door cannot express one — it
//! sends to the channel it authorized against — so the case is unrepresentable rather than
//! unhandled.
//!
//! And one domain refusal is deliberately NOT told to the caller. A rejected reply cluster
//! reaches this module only if the resolver that validated it and the store that read it
//! disagreed, so it is ours to fix and not theirs to act on. The reference has no code for
//! it; an invented one would publish a contract no consumer accepts.

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use silverhand_backend_core::conversation_messaging::messaging::{
    ActorRefusal, AttachmentRefusal, ConversationRefusal, SendMessageError, freshness::StaleRefusal,
};

/// What a typed refusal answers with: the reason and the code a client keys on, plus the
/// attachment it was about when there is one.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RefusedSend {
    pub error: &'static str,
    pub code: &'static str,
    /// Present only for an attachment refusal, and only because the reference reports WHICH
    /// attachment: a caller with twenty of them cannot act on "one of these is wrong".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachment_id: Option<String>,
}

/// The stale refusal's envelope, which is NESTED where every other refusal here is flat.
///
/// Not a shape to tidy up. The daemon reads `details.subkind` and hard-compares it to
/// `message_stale` before it will arm a stale event, and it replays `stale_event_id` into
/// `stale-receive`. Flattening this body, or renaming a field into the flat form the
/// siblings use, removes the stale capability as surely as deleting the guard would.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct StaleConflict {
    pub error: StaleConflictReason,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct StaleConflictReason {
    pub code: &'static str,
    pub message: &'static str,
    pub details: StaleConflictDetails,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct StaleConflictDetails {
    /// THE WIRE DISCRIMINANT. The daemon compares it literally.
    pub subkind: &'static str,
    pub stale_event_id: String,
    /// The conversation the rejected send TARGETED, surfaced for the same reason a
    /// successful send reports where it landed: a later resolve of a name-shaped draft has
    /// to be able to name the target it aimed at.
    pub target_conversation_id: String,
    /// The one-shot discovery path for `message stale-receive`, consumed verbatim.
    pub hint: String,
}

const CONFLICT: &str = "conflict";
const MESSAGE_STALE: &str = "message_stale";
const STALE_MESSAGE: &str = "message is stale: this runtime has undelivered input it must \
                             consume before sending (drain it, or resend with --anyway)";

/// Projects a failed send onto the wire.
///
/// The dispatch is the whole of the "two kinds" rule: every arm above the last three names
/// a caller's own answer, and the last three are ours. Adding a variant to
/// [`SendMessageError`] therefore forces a decision about which side it falls on, because
/// there is no default arm to absorb it.
pub(crate) fn refused_send(cause: &SendMessageError) -> Response {
    match cause {
        SendMessageError::ClientMessageConflict => told(
            StatusCode::CONFLICT,
            "client_message_id was already used for a different request",
            "client_message_conflict",
        ),
        SendMessageError::Conversation(refusal) => conversation(refusal),
        SendMessageError::Actor(refusal) => actor(refusal),
        SendMessageError::ReplyTarget(_) => told(
            StatusCode::NOT_FOUND,
            "target message not found in target conversation",
            "reply_target_not_found",
        ),
        SendMessageError::Attachment(refusal) => attachment(refusal),
        SendMessageError::Stale(refusal) => stale(refusal),
        // NOT a caller refusal, on either door. The agent door's resolver validates the
        // cluster before the send and the human door names a MESSAGE to reply to, so this
        // arm firing means the resolver's answer and the store disagreed — our contract
        // broke, not the caller's request. The reference has no code for it, and neither
        // inventing one nor borrowing `reply_target_not_found` (which would tell a client
        // they replied to a missing message) is a thing to publish.
        SendMessageError::ThreadRoot(_)
        | SendMessageError::Invariant(_)
        | SendMessageError::Environment(_)
        | SendMessageError::Persistence(_) => ours(cause),
    }
}

fn conversation(refusal: &ConversationRefusal) -> Response {
    match refusal {
        ConversationRefusal::NotFound => told(
            StatusCode::NOT_FOUND,
            "conversation not found",
            "conversation_not_found",
        ),
        // A lifecycle state, so a conflict: the caller may retry when it changes.
        ConversationRefusal::Archived => told(
            StatusCode::CONFLICT,
            "conversation is archived",
            "conversation_archived",
        ),
        // NOT a lifecycle state and never postable again, so 403 rather than 409 — the same
        // answer the request-layer gate gives for the identical fact. A 409 would invite a
        // client to retry a state that cannot change.
        ConversationRefusal::DirectMessageScope => told(
            StatusCode::FORBIDDEN,
            "Conversation-scoped agents do not support DMs. Use a shared channel.",
            "direct_message_scope_forbidden",
        ),
        ConversationRefusal::EndpointGone => told(
            StatusCode::FORBIDDEN,
            "the other endpoint of this direct conversation no longer exists",
            "conversation_endpoint_gone",
        ),
    }
}

fn actor(refusal: &ActorRefusal) -> Response {
    match refusal {
        ActorRefusal::AgentNotFound => {
            told(StatusCode::NOT_FOUND, "agent not found", "agent_not_found")
        }
        // Two codes for one rule, because the two actor kinds reach the wire through
        // different clients: an operator reading `agent_…` for a person's refused send would
        // look for the wrong subject.
        ActorRefusal::AgentNotParticipant => told(
            StatusCode::FORBIDDEN,
            "agent is not a participant of this conversation",
            "agent_not_participant_of_conversation",
        ),
        ActorRefusal::UserNotParticipant => told(
            StatusCode::FORBIDDEN,
            "user is not a participant of this conversation",
            "user_not_participant_of_conversation",
        ),
    }
}

fn attachment(refusal: &AttachmentRefusal) -> Response {
    let (error, code) = match refusal {
        AttachmentRefusal::OutsideChannel { .. } => (
            "attachment does not belong to this channel",
            "attachment_outside_channel",
        ),
        AttachmentRefusal::LabelKindMismatch { .. } => (
            "attachment label kind does not match its content type",
            "attachment_label_kind_mismatch",
        ),
    };
    (
        StatusCode::BAD_REQUEST,
        Json(RefusedSend {
            error,
            code,
            attachment_id: Some(refusal.attachment_id().as_str().to_owned()),
        }),
    )
        .into_response()
}

fn stale(refusal: &StaleRefusal) -> Response {
    let stale_event_id = refusal.stale_event_id.as_str().to_owned();
    (
        StatusCode::CONFLICT,
        Json(StaleConflict {
            error: StaleConflictReason {
                code: CONFLICT,
                message: STALE_MESSAGE,
                details: StaleConflictDetails {
                    subkind: MESSAGE_STALE,
                    hint: format!(
                        "send rejected; run `silverhand message stale-receive \
                         --stale-event-id {stale_event_id}` to pull this draft's pending \
                         messages now, or stop this send attempt until they arrive through \
                         normal delivery"
                    ),
                    stale_event_id,
                    target_conversation_id: refusal.target_conversation_id.as_str().to_owned(),
                },
            },
        }),
    )
        .into_response()
}

/// A refusal the CALLER can act on. Rendered, and deliberately not logged.
fn told(status: StatusCode, error: &'static str, code: &'static str) -> Response {
    (
        status,
        Json(RefusedSend {
            error,
            code,
            attachment_id: None,
        }),
    )
        .into_response()
}

/// OUR failure. Reported once with the whole chain, and answered with nothing of it.
///
/// The caller gets no code to key on, because there is no action for them to take and
/// nothing about our internals is theirs to read.
fn ours(cause: &SendMessageError) -> Response {
    tracing::error!(
        error_chain = crate::foundation::safe_log_field::safe_display(
            &(crate::foundation::error::describe(cause))
        ),
        "message-send failed"
    );
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        "Internal Server Error",
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use std::{
        io::{self, Write},
        sync::{Arc, Mutex},
    };

    use silverhand_backend_core::foundation::persistence::PersistenceFailure;
    use silverhand_backend_core::{
        conversation_messaging::messaging::{
            AttachmentId, EnvironmentUnavailable, ReplyTargetRefusal, SendEntropy,
            ThreadRootRefusal,
            freshness::{FreshnessGuardInvariant, StaleEventId},
        },
        foundation::identity::{AgentId, ConversationId},
    };
    use tracing_subscriber::fmt::MakeWriter;

    use super::*;

    /// Entropy with no randomness in it, so the minted arming id is exact and the hint can
    /// be asserted character for character.
    struct FixedEntropy;

    impl SendEntropy for FixedEntropy {
        fn fill(&self, buffer: &mut [u8]) -> Result<(), EnvironmentUnavailable> {
            buffer.fill(0xAB);
            Ok(())
        }
    }

    #[derive(Debug, thiserror::Error)]
    #[error("the disk went away")]
    struct Underlying;

    fn attachment_id() -> AttachmentId {
        AttachmentId::parse("att-7").unwrap()
    }

    fn stale_refusal() -> StaleRefusal {
        StaleRefusal {
            stale_event_id: StaleEventId::mint(&FixedEntropy).unwrap(),
            target_conversation_id: ConversationId::parse("conv-2").unwrap(),
        }
    }

    /// Renders one refusal and reads back the status, the body, and how many times an
    /// operator was told about it.
    async fn answered(cause: &SendMessageError) -> (StatusCode, serde_json::Value, usize) {
        let logs = LogBuffer::default();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .without_time()
            .with_target(false)
            .with_writer(logs.clone())
            .finish();

        let response = tracing::subscriber::with_default(subscriber, || refused_send(cause));
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        if status == StatusCode::INTERNAL_SERVER_ERROR {
            assert_eq!(body.as_ref(), b"Internal Server Error");
        }
        let reported = logs.errors();
        let parsed = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
        (status, parsed, reported)
    }

    /// EVERY CALLER REFUSAL, exactly as a client receives it.
    ///
    /// Written as one table rather than a test per family: the statuses are the interesting
    /// part and they disagree across families for reasons that are only visible side by side
    /// — a lifecycle state is a conflict the caller may retry, a dead DM endpoint never
    /// becomes postable and so is a 403, and "names nothing" is a 404 whichever coordinate
    /// named it.
    #[tokio::test(flavor = "current_thread")]
    async fn each_family_answers_with_its_own_status_and_code() {
        let cases: Vec<(SendMessageError, StatusCode, serde_json::Value)> = vec![
            (
                ConversationRefusal::NotFound.into(),
                StatusCode::NOT_FOUND,
                serde_json::json!({
                    "error": "conversation not found",
                    "code": "conversation_not_found"
                }),
            ),
            (
                ConversationRefusal::Archived.into(),
                StatusCode::CONFLICT,
                serde_json::json!({
                    "error": "conversation is archived",
                    "code": "conversation_archived"
                }),
            ),
            (
                ConversationRefusal::DirectMessageScope.into(),
                StatusCode::FORBIDDEN,
                serde_json::json!({
                    "error": "Conversation-scoped agents do not support DMs. Use a shared channel.",
                    "code": "direct_message_scope_forbidden"
                }),
            ),
            (
                ConversationRefusal::EndpointGone.into(),
                StatusCode::FORBIDDEN,
                serde_json::json!({
                    "error": "the other endpoint of this direct conversation no longer exists",
                    "code": "conversation_endpoint_gone"
                }),
            ),
            (
                ActorRefusal::AgentNotFound.into(),
                StatusCode::NOT_FOUND,
                serde_json::json!({"error": "agent not found", "code": "agent_not_found"}),
            ),
            (
                ActorRefusal::AgentNotParticipant.into(),
                StatusCode::FORBIDDEN,
                serde_json::json!({
                    "error": "agent is not a participant of this conversation",
                    "code": "agent_not_participant_of_conversation"
                }),
            ),
            (
                ActorRefusal::UserNotParticipant.into(),
                StatusCode::FORBIDDEN,
                serde_json::json!({
                    "error": "user is not a participant of this conversation",
                    "code": "user_not_participant_of_conversation"
                }),
            ),
            (
                ReplyTargetRefusal.into(),
                StatusCode::NOT_FOUND,
                serde_json::json!({
                    "error": "target message not found in target conversation",
                    "code": "reply_target_not_found"
                }),
            ),
            (
                AttachmentRefusal::OutsideChannel {
                    attachment_id: attachment_id(),
                }
                .into(),
                StatusCode::BAD_REQUEST,
                serde_json::json!({
                    "error": "attachment does not belong to this channel",
                    "code": "attachment_outside_channel",
                    "attachment_id": "att-7"
                }),
            ),
            (
                AttachmentRefusal::LabelKindMismatch {
                    attachment_id: attachment_id(),
                }
                .into(),
                StatusCode::BAD_REQUEST,
                serde_json::json!({
                    "error": "attachment label kind does not match its content type",
                    "code": "attachment_label_kind_mismatch",
                    "attachment_id": "att-7"
                }),
            ),
        ];

        for (cause, status, body) in cases {
            let (answered_status, answered_body, reported) = answered(&cause).await;
            assert_eq!(answered_status, status, "{cause:?}");
            assert_eq!(answered_body, body, "{cause:?}");
            // A TYPED REFUSAL IS NOT LOGGED. One line per refused send tells an operator
            // nothing and buries the failures that are ours.
            assert_eq!(reported, 0, "{cause:?} was reported to the operator");
        }
    }

    /// THE STALE ENVELOPE IS NESTED, and every field in it is consumed fail-closed.
    ///
    /// `details.subkind` is hard-compared to `message_stale` before a stale event is armed,
    /// and `stale_event_id` is replayed into `stale-receive` — so this asserts the whole
    /// document rather than a field at a time. Flattening it into the shape its siblings use
    /// would remove the stale capability while every other test here stayed green.
    #[tokio::test(flavor = "current_thread")]
    async fn the_stale_refusal_answers_the_nested_conflict_the_daemon_arms_on() {
        let refusal = stale_refusal();
        let arming_id = refusal.stale_event_id.as_str().to_owned();
        let (status, body, reported) = answered(&refusal.into()).await;

        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(
            body,
            serde_json::json!({
                "error": {
                    "code": "conflict",
                    "message": "message is stale: this runtime has undelivered input it must \
                                consume before sending (drain it, or resend with --anyway)",
                    "details": {
                        "subkind": "message_stale",
                        "stale_event_id": arming_id,
                        "target_conversation_id": "conv-2",
                        "hint": format!(
                            "send rejected; run `silverhand message stale-receive \
                             --stale-event-id {arming_id}` to pull this draft's pending \
                             messages now, or stop this send attempt until they arrive \
                             through normal delivery"
                        )
                    }
                }
            })
        );
        // The runtime's own answer, so it is not an operator's problem either.
        assert_eq!(reported, 0);

        // FOUR DETAIL FIELDS, and the set is closed. The guard also knows which scope
        // rejected the send, how many rows are undelivered, and which stream was aimed at;
        // all three are log-only, because the agent surface is signal-only and a count here
        // would answer "how far behind am I". Asserted as the KEY SET rather than by
        // scanning the rendered document — "undelivered" appears in the reference's own
        // message text, so a substring search reads as a leak when nothing leaked.
        let mut detail_keys = body["error"]["details"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        detail_keys.sort();
        assert_eq!(
            detail_keys,
            [
                "hint",
                "stale_event_id",
                "subkind",
                "target_conversation_id"
            ]
        );
    }

    /// OUR FAILURES are the mirror image: reported once with the whole chain, and answered
    /// with none of it and no code to key on.
    #[tokio::test(flavor = "current_thread")]
    async fn our_own_failures_are_reported_once_and_never_described_to_the_caller() {
        let ours: Vec<SendMessageError> = vec![
            // A cluster the resolver approved and the store then refused: a broken contract
            // of ours, so the caller gets no code to key on and an operator gets the line.
            ThreadRootRefusal.into(),
            FreshnessGuardInvariant::AgentAbsent {
                agent: AgentId::parse("agent-1").unwrap(),
            }
            .into(),
            EnvironmentUnavailable::from_source(Underlying).into(),
            PersistenceFailure::from_source(Underlying).into(),
        ];

        for cause in ours {
            let (status, body, reported) = answered(&cause).await;
            assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{cause:?}");
            // Not JSON at all, so there is no code or reason for a client to branch on.
            assert_eq!(body, serde_json::Value::Null, "{cause:?}");
            assert_eq!(reported, 1, "{cause:?}");
        }
    }

    /// The operator's line carries the cause CHAIN, and the caller's response carries none
    /// of it. Two audiences, one failure, and the split is the whole point.
    #[tokio::test(flavor = "current_thread")]
    async fn the_chain_reaches_the_operator_and_nothing_reaches_the_caller() {
        let logs = LogBuffer::default();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .without_time()
            .with_target(false)
            .with_writer(logs.clone())
            .finish();

        let response = tracing::subscriber::with_default(subscriber, || {
            refused_send(&PersistenceFailure::from_source(Underlying).into())
        });
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();

        let events = logs.events();
        assert_eq!(events.len(), 1, "{events:?}");
        let chain = events[0]["fields"]["error_chain"].as_str().unwrap();
        assert!(chain.contains("persistence operation failed"), "{chain}");
        assert!(chain.contains("the disk went away"), "{chain}");

        assert!(!body.contains("disk"), "{body}");
        assert!(!body.contains("persistence"), "{body}");
    }

    #[derive(Clone, Default)]
    struct LogBuffer(Arc<Mutex<Vec<u8>>>);

    impl LogBuffer {
        fn events(&self) -> Vec<serde_json::Value> {
            let bytes = self.0.lock().unwrap().clone();
            String::from_utf8(bytes)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        }

        fn errors(&self) -> usize {
            self.events()
                .iter()
                .filter(|event| event["level"] == "ERROR")
                .count()
        }
    }

    struct LogWriter(LogBuffer);

    impl Write for LogWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for LogBuffer {
        type Writer = LogWriter;

        fn make_writer(&'a self) -> Self::Writer {
            LogWriter(self.clone())
        }
    }
}
