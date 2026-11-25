//! Human-session and runtime-credential doors for message answer options.

use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    Json, Router,
    extract::{Extension, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use garde::Validate;
use serde::Deserialize;
use serde_json::{Value, json};
use silverhand_backend_core::{
    conversation_messaging::messaging::collaboration::message_options::{
        AnswerCommand, AnswerEventMessage, AnswerRefusal, InvalidOptions, MessageAnswer,
        MessageOptions, MessageOptionsAftermath, MessageOptionsPort, MessageOptionsState,
        OptionsActor, QuestionCoordinates, UpdateOptionsCommand, WithdrawAnswerCommand,
    },
    conversation_messaging::messaging::{MessageId, PrincipalIdentity},
    foundation::identity::{AgentId, UserId},
    host_runtime::runtime_credential::RuntimePrincipal,
};

use crate::conversation_messaging::messaging::admission::MessageOptionsInput;
use crate::foundation::http_error::plain_500;
use crate::foundation::wire_admission::{ValidatedJson, ValidatedJsonRejection, invalid_input};

pub const HUMAN_ANSWER_PATH: &str = "/api/messages/{id}/answer";
pub const AGENT_ANSWER_PATH: &str = "/internal/agents/{aid}/answer-message";
pub const AGENT_OPTIONS_PATH: &str = "/internal/agents/{aid}/message-options";
pub const MESSAGE_OPTIONS_CHANGED_EVENT: &str = "message_options_changed";

struct Inner<P, A> {
    port: P,
    aftermath: A,
}

struct OptionsState<P, A>(Arc<Inner<P, A>>);

impl<P, A> Clone for OptionsState<P, A> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

pub fn human_router<P, A>(port: P, aftermath: A) -> Router
where
    P: MessageOptionsPort + Send + Sync + 'static,
    A: MessageOptionsAftermath + Send + Sync + 'static,
{
    Router::new()
        .route(
            HUMAN_ANSWER_PATH,
            post(human_answer::<P, A>).delete(human_withdraw::<P, A>),
        )
        .with_state(OptionsState(Arc::new(Inner { port, aftermath })))
}

pub fn agent_router<P, A>(port: P, aftermath: A) -> Router
where
    P: MessageOptionsPort + Send + Sync + 'static,
    A: MessageOptionsAftermath + Send + Sync + 'static,
{
    Router::new()
        .route(
            AGENT_ANSWER_PATH,
            post(agent_answer::<P, A>).delete(agent_withdraw::<P, A>),
        )
        .route(AGENT_OPTIONS_PATH, post(agent_update::<P, A>))
        .with_state(OptionsState(Arc::new(Inner { port, aftermath })))
}

#[derive(Deserialize)]
struct MessagePath {
    id: String,
}

#[derive(Deserialize)]
struct AgentPath {
    aid: String,
}

#[derive(Debug, Deserialize, Validate)]
struct HumanAnswerBody {
    #[serde(default)]
    #[garde(skip)]
    option_ids: Vec<String>,
    #[serde(default)]
    #[garde(skip)]
    other: Option<String>,
}

#[derive(Debug, Deserialize, Validate)]
struct AgentAnswerBody {
    #[garde(skip)]
    message_id: String,
    #[serde(default)]
    #[garde(skip)]
    option_ids: Vec<String>,
    #[serde(default)]
    #[garde(skip)]
    other: Option<String>,
}

#[derive(Debug, Deserialize, Validate)]
struct AgentWithdrawBody {
    #[garde(skip)]
    message_id: String,
}

#[derive(Debug, Deserialize, Validate)]
struct AgentOptionsBody {
    #[garde(skip)]
    message_id: String,
    #[garde(skip)]
    options: MessageOptionsInput,
}

async fn human_answer<P, A>(
    State(state): State<OptionsState<P, A>>,
    principal: Option<Extension<UserId>>,
    Path(path): Path<MessagePath>,
    body: Result<ValidatedJson<HumanAnswerBody>, ValidatedJsonRejection>,
) -> Response
where
    P: MessageOptionsPort,
    A: MessageOptionsAftermath,
{
    let Some(Extension(viewer)) = principal else {
        return internal_error("message answer route reached without human principal");
    };
    let Ok(message) = MessageId::parse(path.id) else {
        return not_found();
    };
    let body = match body {
        Ok(ValidatedJson(body)) => body,
        Err(rejection) => return rejection.into_response(),
    };
    answer(
        &state.0,
        OptionsActor::Human(viewer),
        message,
        body.option_ids,
        body.other,
    )
    .await
}

async fn human_withdraw<P, A>(
    State(state): State<OptionsState<P, A>>,
    principal: Option<Extension<UserId>>,
    Path(path): Path<MessagePath>,
) -> Response
where
    P: MessageOptionsPort,
    A: MessageOptionsAftermath,
{
    let Some(Extension(viewer)) = principal else {
        return internal_error("message answer route reached without human principal");
    };
    let Ok(message) = MessageId::parse(path.id) else {
        return not_found();
    };
    withdraw(&state.0, OptionsActor::Human(viewer), message).await
}

async fn agent_withdraw<P, A>(
    State(state): State<OptionsState<P, A>>,
    principal: Option<Extension<RuntimePrincipal>>,
    Path(path): Path<AgentPath>,
    body: Result<ValidatedJson<AgentWithdrawBody>, ValidatedJsonRejection>,
) -> Response
where
    P: MessageOptionsPort,
    A: MessageOptionsAftermath,
{
    let Some(Extension(principal)) = principal else {
        return internal_error("message options route reached without runtime principal");
    };
    let Some(actor) = agent_actor(principal, path.aid) else {
        return not_found();
    };
    let body = match body {
        Ok(ValidatedJson(body)) => body,
        Err(rejection) => return rejection.into_response(),
    };
    let Ok(message) = MessageId::parse(body.message_id) else {
        return invalid_input();
    };
    withdraw(&state.0, actor, message).await
}

async fn withdraw<P, A>(state: &Inner<P, A>, actor: OptionsActor, message: MessageId) -> Response
where
    P: MessageOptionsPort,
    A: MessageOptionsAftermath,
{
    let Some(occurred_at_ms) = now_ms() else {
        return internal_error("system time is outside message answer range");
    };
    let command = WithdrawAnswerCommand {
        actor,
        message,
        occurred_at_ms,
    };
    match state.port.withdraw_answer(command).await {
        Ok(Ok(fact)) => {
            log_failures(state.aftermath.answer_withdrawn(&fact).await);
            Json(json!({
                "message_id": fact.question.message.as_str(),
                "event_message_id": fact.event.id.as_str(),
            }))
            .into_response()
        }
        Ok(Err(refusal)) => refusal_response(refusal),
        Err(error) => store_failure(&error),
    }
}

/// `None` is a path agent other than the credential: an indistinguishable 404.
fn agent_actor(principal: RuntimePrincipal, aid: String) -> Option<OptionsActor> {
    match AgentId::parse(aid) {
        Ok(agent) if &agent == principal.agent() => Some(OptionsActor::Agent {
            id: agent,
            source: principal.source().cloned(),
        }),
        _ => None,
    }
}

async fn agent_answer<P, A>(
    State(state): State<OptionsState<P, A>>,
    principal: Option<Extension<RuntimePrincipal>>,
    Path(path): Path<AgentPath>,
    body: Result<ValidatedJson<AgentAnswerBody>, ValidatedJsonRejection>,
) -> Response
where
    P: MessageOptionsPort,
    A: MessageOptionsAftermath,
{
    let Some(Extension(principal)) = principal else {
        return internal_error("message options route reached without runtime principal");
    };
    let Some(actor) = agent_actor(principal, path.aid) else {
        return not_found();
    };
    let body = match body {
        Ok(ValidatedJson(body)) => body,
        Err(rejection) => return rejection.into_response(),
    };
    let Ok(message) = MessageId::parse(body.message_id) else {
        return invalid_input();
    };
    answer(&state.0, actor, message, body.option_ids, body.other).await
}

async fn agent_update<P, A>(
    State(state): State<OptionsState<P, A>>,
    principal: Option<Extension<RuntimePrincipal>>,
    Path(path): Path<AgentPath>,
    body: Result<ValidatedJson<AgentOptionsBody>, ValidatedJsonRejection>,
) -> Response
where
    P: MessageOptionsPort,
    A: MessageOptionsAftermath,
{
    let Some(Extension(principal)) = principal else {
        return internal_error("message options route reached without runtime principal");
    };
    let Some(actor) = agent_actor(principal, path.aid) else {
        return not_found();
    };
    let body = match body {
        Ok(ValidatedJson(body)) => body,
        Err(rejection) => return rejection.into_response(),
    };
    let Ok(message) = MessageId::parse(body.message_id) else {
        return invalid_input();
    };
    let options = body.options.parse().ok_or(InvalidOptions);
    let command = UpdateOptionsCommand {
        actor,
        message,
        options,
    };
    match state.0.port.update_options(command).await {
        Ok(Ok(fact)) => {
            log_failures(state.0.aftermath.options_updated(&fact).await);
            Json(json!({
                "message_id": fact.question.message.as_str(),
                "options": options_json(fact.state.options()),
            }))
            .into_response()
        }
        Ok(Err(refusal)) => refusal_response(refusal),
        Err(error) => store_failure(&error),
    }
}

async fn answer<P, A>(
    state: &Inner<P, A>,
    actor: OptionsActor,
    message: MessageId,
    option_ids: Vec<String>,
    other: Option<String>,
) -> Response
where
    P: MessageOptionsPort,
    A: MessageOptionsAftermath,
{
    let Some(occurred_at_ms) = now_ms() else {
        return internal_error("system time is outside message answer range");
    };
    let command = AnswerCommand {
        actor,
        message,
        option_ids,
        other,
        occurred_at_ms,
    };
    match state.port.answer(command).await {
        Ok(Ok(fact)) => {
            log_failures(state.aftermath.answered(&fact).await);
            let answer = fact.state.answer().map(answer_json).unwrap_or(Value::Null);
            Json(json!({
                "message_id": fact.question.message.as_str(),
                "answer": answer,
                "event_message_id": fact.event.id.as_str(),
            }))
            .into_response()
        }
        Ok(Err(refusal)) => refusal_response(refusal),
        Err(error) => store_failure(&error),
    }
}

pub fn options_json(options: &MessageOptions) -> Value {
    json!({
        "mode": options.mode().as_str(),
        "items": options
            .items()
            .iter()
            .map(|item| json!({ "id": item.id(), "label": item.label() }))
            .collect::<Vec<_>>(),
    })
}

pub fn answer_json(answer: &MessageAnswer) -> Value {
    let kind = match answer.by().principal() {
        PrincipalIdentity::Human(_) => "user",
        PrincipalIdentity::Agent(_) => "agent",
    };
    json!({
        "by": {
            "kind": kind,
            "id": answer.by().principal().as_str(),
            "name": answer.by().name(),
            "display_name": answer.by().display_name(),
        },
        "option_ids": answer.option_ids(),
        "other": answer.other(),
        "answered_at": answer.answered_at_ms(),
    })
}

/// Add `options` / `answer` to a message DTO when the message carries options.
pub fn attach_to_message(dto: &mut Value, state: Option<&MessageOptionsState>) {
    if let (Some(state), Some(object)) = (state, dto.as_object_mut()) {
        object.insert("options".into(), options_json(state.options()));
        object.insert(
            "answer".into(),
            state.answer().map(answer_json).unwrap_or(Value::Null),
        );
    }
}

/// The `message_options_changed` payload: absolute post-change state.
pub fn changed_payload(question: &QuestionCoordinates, state: &MessageOptionsState) -> Value {
    json!({
        "message_id": question.message.as_str(),
        "conversation_id": question.conversation.as_str(),
        "thread_root_id": question.thread_root.as_ref().map(|id| id.as_str()),
        "options": options_json(state.options()),
        "answer": state.answer().map(answer_json).unwrap_or(Value::Null),
    })
}

/// The answer event as a wire `MessageDto`.
pub fn answer_event_message(
    question: &QuestionCoordinates,
    event: &AnswerEventMessage,
) -> Result<Value, serde_json::Error> {
    let metadata: Value = serde_json::from_str(&event.metadata)?;
    Ok(json!({
        "id": event.id.as_str(),
        "conversation_id": question.conversation.as_str(),
        "from_kind": "system",
        "from_agent_id": Value::Null,
        "from_user_id": Value::Null,
        "author": Value::Null,
        "content": event.content,
        "metadata": metadata,
        "attachments": Value::Null,
        "reactions": Value::Null,
        "seq": event.seq,
        "created_at": event.created_at_ms,
        "thread_root_id": question.thread_root.as_ref().map(|id| id.as_str()),
        "linked_task": Value::Null,
    }))
}

fn refusal_response(refusal: AnswerRefusal) -> Response {
    let (status, error, code) = match refusal {
        AnswerRefusal::TargetHidden => return not_found(),
        AnswerRefusal::MessageNotFound => (
            StatusCode::NOT_FOUND,
            "message not found",
            "message_not_found",
        ),
        AnswerRefusal::Forbidden => (
            StatusCode::FORBIDDEN,
            "cannot answer in this conversation",
            "forbidden",
        ),
        AnswerRefusal::CapabilityDenied => {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({
                    "code": "capability_denied",
                    "capability": "send_messages",
                    "message": "missing 'send_messages' capability in this workspace",
                    "details": { "capability": "send_messages" }
                })),
            )
                .into_response();
        }
        AnswerRefusal::ConversationArchived => (
            StatusCode::CONFLICT,
            "conversation is archived",
            "conversation_archived",
        ),
        AnswerRefusal::NoOptions => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "message has no options",
            "no_options",
        ),
        AnswerRefusal::UnknownOption => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "option id is not in the message's options",
            "unknown_option",
        ),
        AnswerRefusal::InvalidAnswer => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "answer does not fit the option mode",
            "invalid_answer",
        ),
        AnswerRefusal::OwnQuestion => (
            StatusCode::FORBIDDEN,
            "cannot answer your own question",
            "own_question",
        ),
        AnswerRefusal::OptionsAnswered => (
            StatusCode::CONFLICT,
            "options cannot change once answered",
            "options_answered",
        ),
        AnswerRefusal::NotAuthor => (
            StatusCode::FORBIDDEN,
            "only the author may update options",
            "not_author",
        ),
        AnswerRefusal::InvalidOptions => return invalid_input(),
        AnswerRefusal::NoAnswer => (
            StatusCode::CONFLICT,
            "message has no answer to withdraw",
            "no_answer",
        ),
        AnswerRefusal::NotAnswerer => (
            StatusCode::FORBIDDEN,
            "only the current answerer may withdraw the answer",
            "not_answerer",
        ),
    };
    (status, Json(json!({ "error": error, "code": code }))).into_response()
}

fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "message not found" })),
    )
        .into_response()
}

fn internal_error(message: &'static str) -> Response {
    tracing::error!("{message}");
    plain_500()
}

fn store_failure(error: &(dyn std::error::Error + 'static)) -> Response {
    tracing::error!(
        error_chain = crate::foundation::safe_log_field::safe_display(
            &(crate::foundation::error::describe(error))
        ),
        "message options store failed"
    );
    plain_500()
}

fn log_failures(failures: Vec<String>) {
    for failure in failures {
        tracing::error!(
            error_chain = crate::foundation::safe_log_field::safe_display(&failure),
            "message options publication failed"
        );
    }
}

fn now_ms() -> Option<i64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
}
