pub(super) use crate::foundation::http_error::plain_500 as internal_error_response;
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use garde::Validate;
use serde::{Deserialize, Serialize};
use silverhand_backend_core::conversation_messaging::messaging::collaboration::reaction::{
    InvalidReactionEmoji, ReactionChange, ReactionChangedFact, ReactionDetail,
    ReactionMutationOutcome, ReactionMutationRefusal,
};

pub const MESSAGE_REACTION_CHANGED_EVENT: &str = "message_reaction_changed";

/// Human mutation body (`POST`/`DELETE /api/messages/{id}/reactions`). The message id
/// is a path coordinate, so the body carries only the emoji. A closed object: an unknown
/// key is a native decode fault, an array-shaped body is rejected by the shared
/// [`ObjectOnly`](crate::foundation::wire_admission::ObjectOnly) floor inside [`ValidatedJson`], and a
/// non-string `emoji` keeps axum's native typed rejection. There is NO garde rule — emoji
/// grammar is a domain verdict resolved by [`ReactionEmoji`](silverhand_backend_core::conversation_messaging::messaging::collaboration::reaction::ReactionEmoji)
/// under the write lock, never an admission-stage refusal.
///
/// [`ValidatedJson`]: crate::foundation::wire_admission::ValidatedJson
#[derive(Debug, Deserialize, Validate)]
#[serde(deny_unknown_fields)]
pub(super) struct HumanReactRaw {
    #[garde(skip)]
    pub(super) emoji: String,
}

/// Agent mutation body (`POST`/`DELETE /internal/agents/{aid}/react-message`). The agent
/// route is conversation-scoped, so the target message id rides the body. Both fields are
/// plain strings at admission: `message_id`'s domain shape is decided by
/// [`MessageId::parse`](silverhand_backend_core::conversation_messaging::messaging::MessageId::parse) and the emoji
/// grammar by the domain path under the write lock — deferring both is what lets an invalid
/// emoji paired with a hidden/unauthorized target refuse as 404/403, not 422 (proved by the
/// `agent_invalid_emoji_*` cases). Closed object, no garde rule, same structural guarantees
/// as [`HumanReactRaw`].
#[derive(Debug, Deserialize, Validate)]
#[serde(deny_unknown_fields)]
pub(super) struct AgentReactRaw {
    #[garde(skip)]
    pub(super) message_id: String,
    #[garde(skip)]
    pub(super) emoji: String,
}

#[derive(Serialize)]
struct ErrorBody {
    error: &'static str,
    code: &'static str,
}

#[derive(Serialize)]
struct MutationBody<'a> {
    changed: bool,
    emoji: &'a str,
    count: i64,
}

#[derive(Serialize)]
struct DetailEnvelope<'a> {
    reactions: Vec<DetailBody<'a>>,
}

#[derive(Serialize)]
struct DetailBody<'a> {
    emoji: &'a str,
    reactors: Vec<ReactorBody<'a>>,
}

#[derive(Serialize)]
struct ReactorBody<'a> {
    kind: &'static str,
    id: &'a str,
    display_name: &'a str,
}

#[derive(Serialize)]
pub struct ReactionChangedBody<'a> {
    message_id: &'a str,
    conversation_id: &'a str,
    thread_root_id: Option<&'a str>,
    emoji: &'a str,
    op: &'static str,
    reactor_kind: &'static str,
    reactor_id: &'a str,
    count: i64,
}

pub fn changed_event(fact: &ReactionChangedFact) -> ReactionChangedBody<'_> {
    ReactionChangedBody {
        message_id: fact.message().as_str(),
        conversation_id: fact.conversation().as_str(),
        thread_root_id: fact.thread_root().map(|id| id.as_str()),
        emoji: fact.emoji().as_str(),
        op: match fact.change() {
            ReactionChange::Add => "add",
            ReactionChange::Remove => "remove",
        },
        reactor_kind: fact.reactor().kind(),
        reactor_id: fact.reactor().id(),
        count: fact.count().value(),
    }
}

pub(super) fn mutation_success(outcome: &ReactionMutationOutcome) -> Response {
    Json(MutationBody {
        changed: outcome.changed(),
        emoji: outcome.emoji().as_str(),
        count: outcome.count().value(),
    })
    .into_response()
}

pub(super) fn detail_success(detail: &[ReactionDetail]) -> Response {
    Json(DetailEnvelope {
        reactions: detail
            .iter()
            .map(|detail| DetailBody {
                emoji: detail.emoji().as_str(),
                reactors: detail
                    .reactors()
                    .iter()
                    .map(|reactor| ReactorBody {
                        kind: reactor.principal().kind(),
                        id: reactor.principal().id(),
                        display_name: reactor.display_name(),
                    })
                    .collect(),
            })
            .collect(),
    })
    .into_response()
}

pub(super) fn mutation_refusal(refusal: ReactionMutationRefusal) -> Response {
    match refusal {
        ReactionMutationRefusal::InvalidReaction(error) => invalid_reaction(error),
        ReactionMutationRefusal::TargetHidden => not_found(),
        ReactionMutationRefusal::MessageNotFound => coded(
            StatusCode::NOT_FOUND,
            "message not found",
            "message_not_found",
        ),
        ReactionMutationRefusal::SystemMessageNotReactable => coded(
            StatusCode::UNPROCESSABLE_ENTITY,
            "cannot react to a system message",
            "system_message_not_reactable",
        ),
        ReactionMutationRefusal::ConversationNotFound => coded(
            StatusCode::NOT_FOUND,
            "conversation not found",
            "conversation_not_found",
        ),
        ReactionMutationRefusal::Forbidden => coded(
            StatusCode::FORBIDDEN,
            "cannot react in this conversation",
            "forbidden",
        ),
        ReactionMutationRefusal::CapabilityDenied => (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "code": "capability_denied",
                "capability": "send_messages",
                "message": "missing 'send_messages' capability in this workspace",
                "details": { "capability": "send_messages" }
            })),
        )
            .into_response(),
        ReactionMutationRefusal::ConversationClosed => coded(
            StatusCode::CONFLICT,
            "conversation is closed",
            "conversation_closed",
        ),
        ReactionMutationRefusal::ConversationArchived => coded(
            StatusCode::CONFLICT,
            "conversation is archived",
            "conversation_archived",
        ),
    }
}

pub(super) fn invalid_reaction(error: InvalidReactionEmoji) -> Response {
    let message = match error {
        InvalidReactionEmoji::Empty => "emoji is empty",
        InvalidReactionEmoji::Whitespace => "emoji must not contain whitespace",
        InvalidReactionEmoji::TooLong => "emoji exceeds 16 chars",
        InvalidReactionEmoji::NotUnicodeEmoji => "emoji must be a Unicode emoji",
    };
    coded(
        StatusCode::UNPROCESSABLE_ENTITY,
        message,
        "invalid_reaction",
    )
}

pub(super) fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({"error":"message not found"})),
    )
        .into_response()
}

pub(super) fn internal_error(message: &'static str) -> Response {
    tracing::error!("{message}");
    internal_error_response()
}

fn coded(status: StatusCode, error: &'static str, code: &'static str) -> Response {
    (status, Json(ErrorBody { error, code })).into_response()
}
