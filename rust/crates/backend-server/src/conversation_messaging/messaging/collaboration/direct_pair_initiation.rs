//! Human-session first-send boundary for a direct pair.
//!
//! The route is deliberately ordered as authority -> bounded body admission ->
//! canonical pair allocation/publication -> the existing message-send unit.
//! Every side-effect-free refusal therefore precedes allocation. Once allocation
//! wins, a later send refusal does not compensate the already committed and
//! announced pair; the cold direct-pair list remains the existence authority.
//!
//! A multipart body carries the first message's files with it, because the pair
//! they would be uploaded into does not exist yet. Every file is validated before
//! allocation; after it, the files are stored under the pair through the ordinary
//! upload units and the message references them. A failure there leaves the pair
//! and any stored file in place, and a retry with the same client message id
//! reuses both.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Extension, Path, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use serde_json::json;
use silverhand_backend_core::{
    conversation_messaging::messaging::{
        ClientMessageId, DirectLineAuthority, PrincipalIdentity, SendActor, SendDestination,
        SendMessageError, SendMessageOutcome, SendMessageUnitOfWork,
        attachments::{AttachmentLabel, AttachmentLabelKind},
        collaboration::direct_pair_initiation::{
            DirectPairFirstSendError, DirectPairInitiationPort, DirectPairInitiationRefusal,
            authorize_direct_pair_initiation,
        },
    },
    foundation::identity::{AgentId, UserId},
};

use crate::conversation_messaging::messaging::{
    ack::send_ack,
    admission::{PayloadRefusal, ecmascript_trim, validate_human_payload},
    aftermath::SendAftermath,
    collaboration::attachment::{
        FirstSendMultipart, FirstSendUploads, StagedFirstSendFile, UnplacedFirstSendFile,
        UploadFailure, first_send_attachment_id, is_multipart, read_first_send_multipart,
    },
    envelope::{BodyRead, read_message_send_body},
    execute::{ResolvedSendContext, execute},
    outcome::refused_send,
    references::MarkdownReferencePositions,
    send_request::{envelope_too_large, human_content_too_large},
};
use crate::foundation::http_error::{json_error as error, plain_500 as internal};

pub const HUMAN_DIRECT_PAIR_INITIATION_PATH: &str = "/api/users/{id}/direct-messages";

pub const DIRECT_PAIR_INITIATION_PATH: &str = "/api/agents/{id}/direct-messages";

struct Inner<I, D, U, F, A> {
    initiation: I,
    direct_lines: D,
    unit_of_work: U,
    aftermath: F,
    uploads: A,
}

struct InitiationState<I, D, U, F, A>(Arc<Inner<I, D, U, F, A>>);

impl<I, D, U, F, A> Clone for InitiationState<I, D, U, F, A> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

/// The isolated POST leaf. The executable mount supplies authenticated human
/// identity and the real event/send adapters at the integration seam.
pub fn router<I, D, U, F, A>(
    initiation: I,
    direct_lines: D,
    unit_of_work: U,
    aftermath: F,
    uploads: A,
) -> Router
where
    I: DirectPairInitiationPort + Send + Sync + 'static,
    D: DirectLineAuthority + Send + Sync + 'static,
    U: SendMessageUnitOfWork + Send + Sync + 'static,
    F: SendAftermath + Send + Sync + 'static,
    A: FirstSendUploads,
{
    Router::new()
        .route(DIRECT_PAIR_INITIATION_PATH, post(initiate::<I, D, U, F, A>))
        .route(
            HUMAN_DIRECT_PAIR_INITIATION_PATH,
            post(initiate_human::<I, D, U, F, A>),
        )
        .with_state(InitiationState(Arc::new(Inner {
            initiation,
            direct_lines,
            unit_of_work,
            aftermath,
            uploads,
        })))
}

async fn initiate<I, D, U, F, A>(
    State(state): State<InitiationState<I, D, U, F, A>>,
    principal: Option<Extension<UserId>>,
    Path(raw_target): Path<String>,
    request: Request,
) -> Response
where
    I: DirectPairInitiationPort + Send + Sync,
    D: DirectLineAuthority + Send + Sync,
    U: SendMessageUnitOfWork + Send + Sync,
    F: SendAftermath + Send + Sync,
    A: FirstSendUploads,
{
    let Ok(target) = AgentId::parse(raw_target) else {
        return error(StatusCode::BAD_REQUEST, "agent id required");
    };
    initiate_target(state, principal, PrincipalIdentity::Agent(target), request).await
}

async fn initiate_human<I, D, U, F, A>(
    State(state): State<InitiationState<I, D, U, F, A>>,
    principal: Option<Extension<UserId>>,
    Path(raw_target): Path<String>,
    request: Request,
) -> Response
where
    I: DirectPairInitiationPort + Send + Sync,
    D: DirectLineAuthority + Send + Sync,
    U: SendMessageUnitOfWork + Send + Sync,
    F: SendAftermath + Send + Sync,
    A: FirstSendUploads,
{
    let Ok(target) = UserId::parse(raw_target) else {
        return error(StatusCode::BAD_REQUEST, "user id required");
    };
    initiate_target(state, principal, PrincipalIdentity::Human(target), request).await
}

async fn initiate_target<I, D, U, F, A>(
    state: InitiationState<I, D, U, F, A>,
    principal: Option<Extension<UserId>>,
    target: PrincipalIdentity,
    request: Request,
) -> Response
where
    I: DirectPairInitiationPort + Send + Sync,
    D: DirectLineAuthority + Send + Sync,
    U: SendMessageUnitOfWork + Send + Sync,
    F: SendAftermath + Send + Sync,
    A: FirstSendUploads,
{
    let Some(Extension(viewer)) = principal else {
        tracing::error!("direct-pair initiation route reached without human principal");
        return internal();
    };
    let authorized =
        match authorize_direct_pair_initiation(&state.0.initiation, viewer.clone(), target.clone())
            .await
        {
            Ok(Ok(authorized)) => authorized,
            Ok(Err(DirectPairInitiationRefusal::NotFound)) => {
                return error(StatusCode::NOT_FOUND, "not found");
            }
            Ok(Err(DirectPairInitiationRefusal::Forbidden)) => {
                return error(StatusCode::FORBIDDEN, "forbidden");
            }
            Err(failure) => {
                tracing::error!(
                    error_chain = crate::foundation::safe_log_field::safe_display(
                        &(crate::foundation::error::describe(&failure))
                    ),
                    "direct-pair initiation authority failed"
                );
                return internal();
            }
        };

    // Unlike the ordinary send doors, this route's authority is intentionally
    // ahead of body polling. The helpers still apply their body bounds.
    let (raw_body, files) = if is_multipart(&request) {
        let FirstSendMultipart { payload, files } = match read_first_send_multipart(request).await {
            Ok(value) => value,
            Err(response) => return *response,
        };
        match first_send_body(payload, files, &viewer, &target) {
            Ok(value) => value,
            Err(response) => return *response,
        }
    } else {
        let raw_body = match read_message_send_body(request).await {
            BodyRead::Within(bytes) => serde_json::from_slice(&bytes).ok(),
            BodyRead::OverCap => return envelope_too_large().into_response(),
            // Production TS catches a non-limit JSON/body failure as `{}` before
            // schema admission. `None` is that same empty-document input here.
            BodyRead::Unreadable(_) => None,
        };
        (raw_body, Vec::new())
    };
    let admitted = match validate_human_payload(raw_body) {
        Ok(admitted) => admitted,
        Err(refusal) => return refuse_payload(refusal),
    };
    // TS refuses a reply target (400 `reply_to_unsupported`) BEFORE it meets the content
    // cap (413). Admission has only refined so far; the cap is stage two, sealed below,
    // so this door checks the reply target in between and restores the reference order.
    // A direct pair has no cluster to reply into, so any reply target is a client error.
    if admitted.reply_to().is_some() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "cannot open a direct-message line into a reply cluster",
                "code": "reply_to_unsupported",
            })),
        )
            .into_response();
    }
    // STAGE TWO — the content cap, run only now that the reply target has been refused.
    let payload = match admitted.seal() {
        Ok(payload) => payload,
        Err(refusal) => return refuse_payload(refusal),
    };

    let inner = Arc::clone(&state.0);
    let send_inner = Arc::clone(&inner);
    let result = inner
        .initiation
        .open_and_send(authorized, |opened| async move {
            let conversation = opened.conversation().clone();
            for file in files {
                send_inner
                    .uploads
                    .stage(&viewer, &conversation, file)
                    .await
                    .map_err(FirstSendFailure::Upload)?;
            }
            execute(
                ResolvedSendContext::resolved(
                    SendActor::User(viewer),
                    conversation.clone(),
                    conversation,
                    SendDestination::MainLine,
                    false,
                ),
                payload,
                &MarkdownReferencePositions,
                &send_inner.direct_lines,
                &send_inner.unit_of_work,
                &send_inner.aftermath,
            )
            .await
            .map_err(FirstSendFailure::Send)
        })
        .await;

    match result {
        Ok(outcome) => created(&outcome),
        Err(DirectPairFirstSendError::Send(FirstSendFailure::Send(failure))) => {
            refused_send(&failure)
        }
        Err(DirectPairFirstSendError::Send(FirstSendFailure::Upload(failure))) => match failure {
            UploadFailure::NotFound => error(StatusCode::NOT_FOUND, "not found"),
            UploadFailure::Forbidden => error(StatusCode::FORBIDDEN, "forbidden"),
            UploadFailure::Internal => internal(),
        },
        Err(failure) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&failure))
                ),
                "direct-pair initiation failed before first-message send completed"
            );
            internal()
        }
    }
}

#[derive(Debug)]
enum FirstSendFailure {
    Upload(UploadFailure),
    Send(SendMessageError),
}

impl std::fmt::Display for FirstSendFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Upload(_) => formatter.write_str("first-send attachment was not stored"),
            Self::Send(failure) => failure.fmt(formatter),
        }
    }
}

impl std::error::Error for FirstSendFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Upload(failure) => Some(failure),
            Self::Send(failure) => failure.source(),
        }
    }
}

/// The JSON body the send admission reads, built from the multipart `payload`
/// part and the validated files: each file becomes one attachment reference,
/// in order, carrying the label at its position.
///
/// A label must agree with its file's type here, before the pair is opened; the
/// send unit would otherwise refuse it only after.
fn first_send_body(
    payload: Option<serde_json::Value>,
    files: Vec<UnplacedFirstSendFile>,
    viewer: &UserId,
    target: &PrincipalIdentity,
) -> Result<(Option<serde_json::Value>, Vec<StagedFirstSendFile>), Box<Response>> {
    let Some(serde_json::Value::Object(mut body)) = payload else {
        return Err(Box::new(crate::foundation::wire_admission::invalid_input()));
    };
    if body.contains_key("attachment_ids") {
        return Err(Box::new(crate::foundation::wire_admission::invalid_input()));
    }
    let labels = match body.remove("attachment_labels") {
        None => Vec::new(),
        Some(serde_json::Value::Array(labels)) => labels,
        Some(_) => return Err(Box::new(crate::foundation::wire_admission::invalid_input())),
    };
    if labels.len() > files.len() {
        return Err(Box::new(crate::foundation::wire_admission::invalid_input()));
    }
    // A retry must name the same files, so a first send with files has to say
    // which send it is.
    let client_message_id = match body.get("client_message_id") {
        Some(serde_json::Value::String(raw)) => ClientMessageId::parse(raw.clone())
            .map_err(|_| Box::new(crate::foundation::wire_admission::invalid_input()))?,
        _ if files.is_empty() => {
            return Ok((Some(serde_json::Value::Object(body)), Vec::new()));
        }
        _ => return Err(Box::new(crate::foundation::wire_admission::invalid_input())),
    };
    let mut references = Vec::with_capacity(files.len());
    let mut staged = Vec::with_capacity(files.len());
    for (ordinal, file) in files.into_iter().enumerate() {
        let id = first_send_attachment_id(viewer, target, &client_message_id, ordinal);
        let reference = match labels.get(ordinal) {
            None | Some(serde_json::Value::Null) => json!({ "id": id.as_str() }),
            Some(serde_json::Value::String(label)) => {
                let kind =
                    AttachmentLabel::parse(ecmascript_trim(label)).map(AttachmentLabel::kind);
                if kind.is_some_and(|kind| {
                    kind != AttachmentLabelKind::of_content_type(file.content_type().as_str())
                }) {
                    return Err(Box::new(
                        (
                            StatusCode::BAD_REQUEST,
                            Json(json!({
                                "error": "attachment label kind does not match its content type",
                                "code": "attachment_label_kind_mismatch",
                                "attachment_id": id.as_str(),
                            })),
                        )
                            .into_response(),
                    ));
                }
                json!({ "id": id.as_str(), "label": label })
            }
            Some(_) => return Err(Box::new(crate::foundation::wire_admission::invalid_input())),
        };
        references.push(reference);
        staged.push(file.place(id));
    }
    body.insert(
        "attachment_ids".into(),
        serde_json::Value::Array(references),
    );
    Ok((Some(serde_json::Value::Object(body)), staged))
}

fn created(outcome: &SendMessageOutcome) -> Response {
    (StatusCode::CREATED, Json(send_ack(outcome))).into_response()
}

/// The two payload stages — refine (shape/says-nothing) and seal (content cap) — refuse
/// with the same [`PayloadRefusal`], and this door renders each to the identical wire
/// wherever it was raised. Splitting the stages moved WHEN the cap runs, never WHAT it
/// answers.
fn refuse_payload(refusal: PayloadRefusal) -> Response {
    match refusal {
        PayloadRefusal::Invalid => crate::foundation::wire_admission::invalid_input(),
        PayloadRefusal::ContentTooLarge { actual_bytes } => {
            human_content_too_large(actual_bytes).into_response()
        }
    }
}

#[cfg(test)]
mod tests;
