//! Unmounted HTTP adapters for the public attachment capability.
//!
//! Human authentication and product admission precede multipart extraction.
//! Blob writes precede the writer-reserved SQLite pointer commit; failures before
//! commit attempt orphan cleanup, while maintenance and publication after commit
//! can only be reported and never compensate the committed row.
use crate::foundation::http_error::{coded_error as coded, json_error as error};

mod agent;
mod first_send;

pub use agent::{
    INTERNAL_AGENT_ATTACHMENT_DOWNLOAD_PATH, INTERNAL_AGENT_ATTACHMENT_LOCATE_PATH,
    INTERNAL_AGENT_ATTACHMENT_PATH, INTERNAL_AGENT_ATTACHMENTS_PATH,
    agent_attachment_download_router, agent_attachment_locate_router,
    agent_attachment_update_router, agent_attachment_upload_router,
};
pub use first_send::{FirstSendAttachmentStore, FirstSendUploads, StagedFirstSendFile};
pub(crate) use first_send::{
    FirstSendMultipart, UnplacedFirstSendFile, first_send_attachment_id, is_multipart,
    read_first_send_multipart,
};

#[cfg(test)]
mod tests;

use std::{
    error::Error,
    fmt,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Extension, FromRequest, Multipart, Path, Query, Request, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;
use serde_json::{Value, json};
use silverhand_backend_core::{
    conversation_messaging::messaging::{
        AttachmentId, SendClock, SendEntropy,
        collaboration::attachment::{
            AttachmentChangedFact, AttachmentChangedPublisher, AttachmentContentType,
            AttachmentContentUpdateCommand, AttachmentContentUpdateRefusal,
            AttachmentContentUpdateUnitOfWork, AttachmentCreateCommand, AttachmentCreateRefusal,
            AttachmentCreateUnitOfWork, AttachmentDeleteCommand, AttachmentDeleteRefusal,
            AttachmentDeleteUnitOfWork, AttachmentDownloadReadPort, AttachmentFilename,
            AttachmentListCursor, AttachmentListQuery, AttachmentListReadPort,
            AttachmentPostCommitMaintenance, AttachmentRecord, AttachmentSenderFacet,
            AttachmentSenderFacetReadPort, AttachmentSenderFilter, AttachmentSourceActor,
            AttachmentWriteAdmissionPort, AttachmentWriteAdmissionRefusal,
            BoundAttachmentCreateAdmission, BoundAttachmentListPage, BoundAttachmentSenderFacets,
            ChannelAttachmentReadRefusal, maintain_committed_delete, maintain_committed_update,
        },
        environment::EnvironmentUnavailable,
    },
    foundation::blob_store::{BlobNamespace, BlobReadOutcome, BlobStore},
    foundation::identity::{AgentId, ConversationId, UserId},
    foundation::wire_number::JsSafeInteger,
    foundation::wire_text,
};
use silverhand_backend_filesystem::foundation::blob_store::{
    LocalBlobAliasOutcome, LocalBlobAliasStore,
};

use crate::conversation_messaging::messaging::author_wire::author_dto;

pub const CHANNEL_ATTACHMENTS_PATH: &str = "/api/channels/{id}/attachments";
pub const CHANNEL_ATTACHMENT_SENDERS_PATH: &str = "/api/channels/{id}/attachments/senders";
pub const ATTACHMENT_PATH: &str = "/api/attachments/{id}";
pub const ATTACHMENT_CHANGED_EVENT: &str = "attachment_changed";

const DEFAULT_LIST_LIMIT: u16 = 50;
const MAX_LIST_LIMIT: u16 = 100;
const MAX_FILE_BYTES: usize = 10 * 1024 * 1024;
const MULTIPART_ENVELOPE_BYTES: usize = MAX_FILE_BYTES + 1024 * 1024;
const ATTACHMENT_ID_BYTES: usize = 12;

struct Inner<R, W, B, E, P> {
    reads: R,
    writes: W,
    blobs: B,
    environment: E,
    publisher: P,
}

struct AttachmentState<R, W, B, E, P>(Arc<Inner<R, W, B, E, P>>);

impl<R, W, B, E, P> Clone for AttachmentState<R, W, B, E, P> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

pub fn router<R, W, B, E, P>(reads: R, writes: W, blobs: B, environment: E, publisher: P) -> Router
where
    R: AttachmentListReadPort
        + AttachmentSenderFacetReadPort
        + AttachmentDownloadReadPort
        + Send
        + Sync
        + 'static,
    W: AttachmentWriteAdmissionPort
        + AttachmentCreateUnitOfWork
        + AttachmentContentUpdateUnitOfWork
        + AttachmentDeleteUnitOfWork
        + Send
        + Sync
        + 'static,
    B: BlobStore + LocalBlobAliasStore + Send + Sync + 'static,
    E: SendClock + SendEntropy + Send + Sync + 'static,
    P: AttachmentChangedPublisher + Send + Sync + 'static,
{
    let state = AttachmentState(Arc::new(Inner {
        reads,
        writes,
        blobs,
        environment,
        publisher,
    }));
    Router::new()
        .route(
            CHANNEL_ATTACHMENTS_PATH,
            get(list::<R, W, B, E, P>).post(upload::<R, W, B, E, P>),
        )
        .route(
            CHANNEL_ATTACHMENT_SENDERS_PATH,
            get(sender_facets::<R, W, B, E, P>),
        )
        .route(
            ATTACHMENT_PATH,
            get(download::<R, W, B, E, P>)
                .patch(update::<R, W, B, E, P>)
                .delete(delete::<R, W, B, E, P>),
        )
        .layer(DefaultBodyLimit::max(MULTIPART_ENVELOPE_BYTES))
        .with_state(state)
}

#[derive(Clone, Copy, Debug, Default)]
pub struct HostAttachmentEnvironment;

impl SendEntropy for HostAttachmentEnvironment {
    fn fill(&self, buffer: &mut [u8]) -> Result<(), EnvironmentUnavailable> {
        getrandom::fill(buffer).map_err(EnvironmentUnavailable::from_source)
    }
}

impl SendClock for HostAttachmentEnvironment {
    fn now_unix_ms(&self) -> Result<i64, EnvironmentUnavailable> {
        let duration = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(EnvironmentUnavailable::from_source)?;
        i64::try_from(duration.as_millis()).map_err(EnvironmentUnavailable::from_source)
    }
}

#[derive(Deserialize)]
struct IdPath {
    id: String,
}

#[derive(Default, Deserialize)]
struct ListQueryWire {
    limit: Option<String>,
    cursor: Option<String>,
    sender_kind: Option<String>,
    sender_id: Option<String>,
    since: Option<String>,
    until: Option<String>,
}

async fn list<R, W, B, E, P>(
    State(state): State<AttachmentState<R, W, B, E, P>>,
    principal: Option<Extension<UserId>>,
    Path(path): Path<IdPath>,
    query: Result<Query<ListQueryWire>, axum::extract::rejection::QueryRejection>,
) -> Response
where
    R: AttachmentListReadPort,
{
    let (viewer, conversation) = match channel_coordinates(principal, path.id) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    let query = match query {
        Ok(Query(raw)) => match parse_list_query(raw) {
            Ok(value) => value,
            Err(response) => return *response,
        },
        Err(_) => return invalid_filter("invalid attachment query"),
    };
    match state.0.reads.list(&viewer, &conversation, &query).await {
        Ok(Ok(page)) => match page.bind(&viewer, &conversation, &query) {
            Ok(page) => list_success(&page),
            Err(error) => {
                tracing::error!(
                    error_chain = crate::foundation::safe_log_field::safe_display(
                        &(crate::foundation::error::describe(&error))
                    ),
                    "attachment list identity seal failed"
                );
                internal()
            }
        },
        Ok(Err(refusal)) => channel_refusal(refusal),
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "attachment list failed"
            );
            internal()
        }
    }
}

async fn sender_facets<R, W, B, E, P>(
    State(state): State<AttachmentState<R, W, B, E, P>>,
    principal: Option<Extension<UserId>>,
    Path(path): Path<IdPath>,
) -> Response
where
    R: AttachmentSenderFacetReadPort,
{
    let (viewer, conversation) = match channel_coordinates(principal, path.id) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    match state.0.reads.sender_facets(&viewer, &conversation).await {
        Ok(Ok(facets)) => match facets.bind(&viewer, &conversation) {
            Ok(facets) => sender_facets_success(&facets),
            Err(error) => {
                tracing::error!(
                    error_chain = crate::foundation::safe_log_field::safe_display(
                        &(crate::foundation::error::describe(&error))
                    ),
                    "attachment facets identity seal failed"
                );
                internal()
            }
        },
        Ok(Err(refusal)) => channel_refusal(refusal),
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "attachment facets failed"
            );
            internal()
        }
    }
}

async fn download<R, W, B, E, P>(
    State(state): State<AttachmentState<R, W, B, E, P>>,
    principal: Option<Extension<UserId>>,
    Path(path): Path<IdPath>,
) -> Response
where
    R: AttachmentDownloadReadPort,
    B: BlobStore,
{
    let (viewer, attachment) = match attachment_coordinates(principal, path.id) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    let fact = match state.0.reads.download(&viewer, &attachment).await {
        Ok(Ok(fact)) => fact,
        Ok(Err(_)) => return attachment_not_found(),
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "attachment download authority failed"
            );
            return internal();
        }
    };
    let bound = match fact.bind(&viewer, &attachment) {
        Ok(value) => value,
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "attachment download identity seal failed"
            );
            return internal();
        }
    };
    let record = bound.record();
    let read = match state.0.blobs.read(record.blob()).await {
        Ok(BlobReadOutcome::Found(read)) => read,
        Ok(BlobReadOutcome::Missing) => {
            tracing::error!(
                attachment =
                    crate::foundation::safe_log_field::safe_display(&(attachment.as_str())),
                "committed attachment blob is missing"
            );
            return internal();
        }
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "attachment blob read failed"
            );
            return internal();
        }
    };
    if i64::try_from(read.actual_size()).ok() != Some(record.size().get()) {
        tracing::error!(
            attachment = crate::foundation::safe_log_field::safe_display(&(attachment.as_str())),
            expected = record.size().get(),
            actual = read.actual_size(),
            "attachment blob size differs from committed row"
        );
        return internal();
    }
    download_success(record, read.into_bytes())
}

async fn upload<R, W, B, E, P>(
    State(state): State<AttachmentState<R, W, B, E, P>>,
    principal: Option<Extension<UserId>>,
    Path(path): Path<IdPath>,
    request: Request,
) -> Response
where
    W: AttachmentWriteAdmissionPort + AttachmentCreateUnitOfWork,
    B: BlobStore,
    E: SendClock + SendEntropy,
{
    let (viewer, conversation) = match channel_coordinates(principal, path.id) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    let admission = match state.0.writes.admit_create(&viewer, &conversation).await {
        Ok(Ok(value)) => match value.bind(&viewer, &conversation) {
            Ok(value) => value,
            Err(error) => {
                tracing::error!(
                    error_chain = crate::foundation::safe_log_field::safe_display(
                        &(crate::foundation::error::describe(&error))
                    ),
                    "attachment upload admission identity seal failed"
                );
                return internal();
            }
        },
        Ok(Err(AttachmentWriteAdmissionRefusal::NotFound)) => {
            return error(StatusCode::NOT_FOUND, "channel not found");
        }
        Ok(Err(AttachmentWriteAdmissionRefusal::Forbidden)) => {
            return error(StatusCode::FORBIDDEN, "forbidden");
        }
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "attachment upload admission failed"
            );
            return internal();
        }
    };
    let file = match multipart_file(request).await {
        Ok(value) => value,
        Err(response) => return *response,
    };
    let filename = match sanitize_filename(&file.filename)
        .and_then(|raw| AttachmentFilename::parse(raw).ok())
    {
        Some(value) => value,
        None => {
            return coded(
                StatusCode::BAD_REQUEST,
                "attachment filename is invalid",
                "invalid_filename",
            );
        }
    };
    let content_type =
        match detect_content_type(file.content_type.as_deref(), &file.bytes, filename.as_str()) {
            Some(value) => value,
            None => {
                return coded(
                    StatusCode::BAD_REQUEST,
                    "attachment content type is invalid",
                    "invalid_content_type",
                );
            }
        };
    let attachment = match mint_attachment_id(&state.0.environment) {
        Ok(value) => value,
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "attachment id entropy unavailable"
            );
            return internal();
        }
    };
    match persist_upload(
        &state.0.writes,
        &state.0.blobs,
        &state.0.environment,
        &admission,
        &viewer,
        &conversation,
        attachment,
        filename,
        content_type,
        file.bytes,
    )
    .await
    {
        Ok(record) => (StatusCode::CREATED, Json(record_json(&record))).into_response(),
        Err(UploadFailure::NotFound) => error(StatusCode::NOT_FOUND, "channel not found"),
        Err(UploadFailure::Forbidden) => error(StatusCode::FORBIDDEN, "forbidden"),
        Err(UploadFailure::Internal) => internal(),
    }
}

/// Why a validated file was not stored. Our own failures are logged where
/// they happen and carry nothing further.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UploadFailure {
    NotFound,
    Forbidden,
    Internal,
}

impl fmt::Display for UploadFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::NotFound => "attachment conversation not found",
            Self::Forbidden => "viewer cannot upload into the conversation",
            Self::Internal => "attachment upload failed",
        })
    }
}

impl Error for UploadFailure {}

/// Writes one validated file under an admitted conversation: the blob first,
/// then the row, removing the blob when the row is not committed.
#[allow(clippy::too_many_arguments)]
pub(super) async fn persist_upload<W, B, E>(
    writes: &W,
    blobs: &B,
    environment: &E,
    admission: &BoundAttachmentCreateAdmission,
    viewer: &UserId,
    conversation: &ConversationId,
    attachment: AttachmentId,
    filename: AttachmentFilename,
    content_type: AttachmentContentType,
    bytes: Vec<u8>,
) -> Result<AttachmentRecord, UploadFailure>
where
    W: AttachmentCreateUnitOfWork,
    B: BlobStore,
    E: SendClock,
{
    let created_at = match environment.now_unix_ms() {
        Ok(value) => value,
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "attachment clock unavailable"
            );
            return Err(UploadFailure::Internal);
        }
    };
    let namespace = match BlobNamespace::for_semantic_id(
        "attachments",
        admission.authority().conversation().as_str(),
    ) {
        Ok(value) => value,
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "attachment namespace projection failed"
            );
            return Err(UploadFailure::Internal);
        }
    };
    let size = bytes.len() as i64;
    let blob = match blobs.put(&namespace, content_type.as_str(), bytes).await {
        Ok(value) => value,
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "attachment blob write failed"
            );
            return Err(UploadFailure::Internal);
        }
    };
    let command = match AttachmentCreateCommand::new(
        viewer.clone(),
        conversation.clone(),
        attachment,
        filename,
        size,
        content_type,
        blob.clone(),
        created_at,
    ) {
        Ok(value) => value,
        Err(error) => {
            cleanup_uncommitted(blobs, &blob, "invalid upload command").await;
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "attachment upload command projection failed"
            );
            return Err(UploadFailure::Internal);
        }
    };
    match writes.create(command).await {
        Ok(Ok(fact)) => match fact.bind(viewer, conversation) {
            Ok(bound) => Ok(bound.record().clone()),
            Err(error) => {
                tracing::error!(
                    error_chain = crate::foundation::safe_log_field::safe_display(
                        &(crate::foundation::error::describe(&error))
                    ),
                    "attachment create identity seal failed"
                );
                Err(UploadFailure::Internal)
            }
        },
        Ok(Err(refusal)) => {
            cleanup_uncommitted(blobs, &blob, "refused upload").await;
            Err(match refusal {
                AttachmentCreateRefusal::NotFound => UploadFailure::NotFound,
                AttachmentCreateRefusal::Forbidden => UploadFailure::Forbidden,
            })
        }
        Err(error) => {
            cleanup_uncommitted(blobs, &blob, "failed upload persistence").await;
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "attachment upload persistence failed"
            );
            Err(UploadFailure::Internal)
        }
    }
}

async fn update<R, W, B, E, P>(
    State(state): State<AttachmentState<R, W, B, E, P>>,
    principal: Option<Extension<UserId>>,
    Path(path): Path<IdPath>,
    request: Request,
) -> Response
where
    W: AttachmentWriteAdmissionPort + AttachmentContentUpdateUnitOfWork,
    B: BlobStore + LocalBlobAliasStore,
    P: AttachmentChangedPublisher,
{
    let (viewer, attachment) = match attachment_coordinates(principal, path.id) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    let admission = match state.0.writes.admit_mutation(&viewer, &attachment).await {
        Ok(Ok(value)) => match value.bind(&viewer, &attachment) {
            Ok(value) => value,
            Err(error) => {
                tracing::error!(
                    error_chain = crate::foundation::safe_log_field::safe_display(
                        &(crate::foundation::error::describe(&error))
                    ),
                    "attachment update admission identity seal failed"
                );
                return internal();
            }
        },
        Ok(Err(AttachmentWriteAdmissionRefusal::NotFound)) => return attachment_not_found(),
        Ok(Err(AttachmentWriteAdmissionRefusal::Forbidden)) => return update_forbidden(),
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "attachment update admission failed"
            );
            return internal();
        }
    };
    let file = match multipart_file(request).await {
        Ok(value) => value,
        Err(response) => return *response,
    };
    let locked_type = admission.record().content_type().clone();
    if !payload_matches_locked_type(&file.bytes, locked_type.as_str()) {
        return coded(
            StatusCode::BAD_REQUEST,
            "attachment update payload does not match its locked content type",
            "content_type_mismatch",
        );
    }
    let namespace = match BlobNamespace::for_semantic_id(
        "attachments",
        admission.record().channel().as_str(),
    ) {
        Ok(value) => value,
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "attachment update namespace projection failed"
            );
            return internal();
        }
    };
    let size = file.bytes.len() as i64;
    let blob = match state
        .0
        .blobs
        .put(&namespace, locked_type.as_str(), file.bytes)
        .await
    {
        Ok(value) => value,
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "attachment replacement blob write failed"
            );
            return internal();
        }
    };
    let command = match AttachmentContentUpdateCommand::new(
        viewer.clone(),
        attachment.clone(),
        locked_type,
        size,
        blob.clone(),
    ) {
        Ok(value) => value,
        Err(error) => {
            cleanup_uncommitted(&state.0.blobs, &blob, "invalid update command").await;
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "attachment update command projection failed"
            );
            return internal();
        }
    };
    let fact = match state.0.writes.update_content(command).await {
        Ok(Ok(fact)) => fact,
        Ok(Err(refusal)) => {
            cleanup_uncommitted(&state.0.blobs, &blob, "refused update").await;
            return match refusal {
                AttachmentContentUpdateRefusal::NotFound => attachment_not_found(),
                AttachmentContentUpdateRefusal::Forbidden => update_forbidden(),
            };
        }
        Err(error) => {
            cleanup_uncommitted(&state.0.blobs, &blob, "failed update persistence").await;
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "attachment update persistence failed"
            );
            return internal();
        }
    };
    let bound = match fact.bind(&viewer, &attachment) {
        Ok(value) => value,
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "attachment update identity seal failed"
            );
            return internal();
        }
    };
    let outcome = maintain_committed_update(bound, &BlobMaintenance(state.0.blobs.clone())).await;
    if let Err(error) = outcome.maintenance() {
        tracing::warn!(
            error_chain = crate::foundation::safe_log_field::safe_display(
                &(crate::foundation::error::describe(error))
            ),
            "committed attachment update blob maintenance incomplete"
        );
    }
    let committed = outcome.committed();
    let changed = committed.changed();
    if let Err(error) = state.0.publisher.publish(&changed).await {
        tracing::error!(
            error_chain = crate::foundation::safe_log_field::safe_display(
                &(crate::foundation::error::describe(&error))
            ),
            "committed attachment_changed publication failed"
        );
    }
    Json(record_json(committed.after())).into_response()
}

async fn delete<R, W, B, E, P>(
    State(state): State<AttachmentState<R, W, B, E, P>>,
    principal: Option<Extension<UserId>>,
    Path(path): Path<IdPath>,
) -> Response
where
    W: AttachmentDeleteUnitOfWork,
    B: BlobStore + LocalBlobAliasStore,
{
    let (viewer, attachment) = match attachment_coordinates(principal, path.id) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    let fact = match state
        .0
        .writes
        .delete(AttachmentDeleteCommand::new(
            viewer.clone(),
            attachment.clone(),
        ))
        .await
    {
        Ok(Ok(fact)) => fact,
        Ok(Err(AttachmentDeleteRefusal::NotFound)) => return attachment_not_found(),
        Ok(Err(AttachmentDeleteRefusal::Forbidden)) => return delete_forbidden(),
        Ok(Err(AttachmentDeleteRefusal::InUse)) => {
            return error(
                StatusCode::CONFLICT,
                "attachment is referenced by a message and cannot be deleted",
            );
        }
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "attachment delete persistence failed"
            );
            return internal();
        }
    };
    let bound = match fact.bind(&viewer, &attachment) {
        Ok(value) => value,
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "attachment delete identity seal failed"
            );
            return internal();
        }
    };
    let outcome = maintain_committed_delete(bound, &BlobMaintenance(state.0.blobs.clone())).await;
    if let Err(error) = outcome.maintenance() {
        tracing::warn!(
            error_chain = crate::foundation::safe_log_field::safe_display(
                &(crate::foundation::error::describe(error))
            ),
            "committed attachment delete blob maintenance incomplete"
        );
    }
    StatusCode::NO_CONTENT.into_response()
}

struct MultipartFile {
    filename: String,
    content_type: Option<String>,
    bytes: Vec<u8>,
}

async fn multipart_file(request: Request) -> Result<MultipartFile, Box<Response>> {
    Ok(multipart_envelope(request, false).await?.file)
}

struct AgentMultipartUpload {
    target: String,
    file: MultipartFile,
}

async fn agent_multipart_upload(request: Request) -> Result<AgentMultipartUpload, Box<Response>> {
    let envelope = multipart_envelope(request, true).await?;
    Ok(AgentMultipartUpload {
        target: envelope.target.unwrap_or_default(),
        file: envelope.file,
    })
}

struct MultipartEnvelope {
    file: MultipartFile,
    target: Option<String>,
}

async fn multipart_envelope(
    request: Request,
    capture_target: bool,
) -> Result<MultipartEnvelope, Box<Response>> {
    if let Some(declared) = request.headers().get(header::CONTENT_LENGTH) {
        let declared = declared
            .to_str()
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or_else(|| Box::new(error(StatusCode::BAD_REQUEST, "invalid content-length")))?;
        if declared > MULTIPART_ENVELOPE_BYTES as u64 {
            return Err(Box::new(coded(
                StatusCode::PAYLOAD_TOO_LARGE,
                "attachment exceeds 10 MiB",
                "too_large",
            )));
        }
    }
    let mut multipart = Multipart::from_request(request, &())
        .await
        .map_err(|rejection| {
            Box::new(if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
                coded(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "attachment exceeds 10 MiB",
                    "too_large",
                )
            } else {
                error(StatusCode::BAD_REQUEST, "multipart/form-data required")
            })
        })?;
    let mut found = None;
    let mut file_seen = false;
    let mut target = None;
    let mut target_seen = false;
    loop {
        let field = multipart.next_field().await.map_err(|failure| {
            Box::new(if failure.status() == StatusCode::PAYLOAD_TOO_LARGE {
                coded(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "attachment exceeds 10 MiB",
                    "too_large",
                )
            } else {
                error(StatusCode::BAD_REQUEST, "multipart/form-data required")
            })
        })?;
        let Some(field) = field else { break };
        if capture_target && field.name() == Some("target") && !target_seen {
            target_seen = true;
            if field.file_name().is_none() {
                let bytes = field.bytes().await.map_err(|failure| {
                    Box::new(if failure.status() == StatusCode::PAYLOAD_TOO_LARGE {
                        coded(
                            StatusCode::PAYLOAD_TOO_LARGE,
                            "attachment exceeds 10 MiB",
                            "too_large",
                        )
                    } else {
                        error(StatusCode::BAD_REQUEST, "multipart/form-data required")
                    })
                })?;
                target = String::from_utf8(bytes.to_vec()).ok();
            }
            continue;
        }
        if field.name() != Some("file") || file_seen {
            continue;
        }
        file_seen = true;
        let Some(filename) = field.file_name().map(str::to_owned) else {
            continue;
        };
        let content_type = field.content_type().map(str::to_owned);
        let bytes = field.bytes().await.map_err(|failure| {
            Box::new(if failure.status() == StatusCode::PAYLOAD_TOO_LARGE {
                coded(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "attachment exceeds 10 MiB",
                    "too_large",
                )
            } else {
                error(StatusCode::BAD_REQUEST, "multipart/form-data required")
            })
        })?;
        found = Some(MultipartFile {
            filename,
            content_type,
            bytes: bytes.to_vec(),
        });
    }
    let Some(file) = found else {
        return Err(Box::new(error(
            StatusCode::BAD_REQUEST,
            "file field required",
        )));
    };
    if file.bytes.is_empty() {
        return Err(Box::new(coded(
            StatusCode::BAD_REQUEST,
            "attachment buffer is empty",
            "invalid_buffer",
        )));
    }
    if file.bytes.len() > MAX_FILE_BYTES {
        return Err(Box::new(coded(
            StatusCode::PAYLOAD_TOO_LARGE,
            "attachment exceeds 10 MiB",
            "too_large",
        )));
    }
    Ok(MultipartEnvelope { file, target })
}

#[derive(Clone)]
struct BlobMaintenance<B>(B);

impl<B> AttachmentPostCommitMaintenance for BlobMaintenance<B>
where
    B: BlobStore + LocalBlobAliasStore,
{
    type Error = BlobMaintenanceError;

    async fn updated(
        &self,
        before: &silverhand_backend_core::foundation::blob_store::StoredBlob,
        after: &silverhand_backend_core::foundation::blob_store::StoredBlob,
        alias: &silverhand_backend_core::foundation::blob_store::BlobAliasId,
    ) -> Result<(), Self::Error> {
        match self
            .0
            .materialize_alias(after, alias)
            .await
            .map_err(|error| BlobMaintenanceError::from_source("relink alias", error))?
        {
            LocalBlobAliasOutcome::Located(_) => {}
            LocalBlobAliasOutcome::MissingPhysical => {
                return Err(BlobMaintenanceError::message(
                    "relink alias",
                    "committed replacement blob is missing",
                ));
            }
        }
        self.0
            .delete(before)
            .await
            .map_err(|error| BlobMaintenanceError::from_source("delete displaced blob", error))?;
        Ok(())
    }

    async fn deleted(
        &self,
        deleted: &silverhand_backend_core::foundation::blob_store::StoredBlob,
        alias: &silverhand_backend_core::foundation::blob_store::BlobAliasId,
    ) -> Result<(), Self::Error> {
        let physical =
            self.0.delete(deleted).await.map_err(|error| {
                BlobMaintenanceError::from_source("delete attachment blob", error)
            });
        let alias =
            self.0.delete_alias(deleted, alias).await.map_err(|error| {
                BlobMaintenanceError::from_source("delete attachment alias", error)
            });

        // Both committed cleanup legs are best-effort and independent. A physical
        // deletion failure must not strand the stable alias merely because it happened
        // first; report both failures when both authorities reject cleanup.
        match (physical, alias) {
            (Ok(_), Ok(_)) => Ok(()),
            (Err(error), Ok(_)) | (Ok(_), Err(error)) => Err(error),
            (Err(primary), Err(secondary)) => {
                Err(BlobMaintenanceError::combined(primary, secondary))
            }
        }
    }
}

#[derive(Debug)]
struct BlobMaintenanceError {
    operation: &'static str,
    source: Box<dyn Error + Send + Sync + 'static>,
}

impl BlobMaintenanceError {
    fn from_source(operation: &'static str, source: impl Error + Send + Sync + 'static) -> Self {
        Self {
            operation,
            source: Box::new(source),
        }
    }

    fn message(operation: &'static str, message: &'static str) -> Self {
        Self::from_source(operation, std::io::Error::other(message))
    }

    fn combined(primary: Self, secondary: Self) -> Self {
        Self::from_source(
            "delete attachment blob and alias",
            CombinedBlobMaintenanceFailures { primary, secondary },
        )
    }
}

#[derive(Debug)]
struct CombinedBlobMaintenanceFailures {
    primary: BlobMaintenanceError,
    secondary: BlobMaintenanceError,
}

impl fmt::Display for CombinedBlobMaintenanceFailures {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "primary [{}]; secondary [{}]",
            crate::foundation::error::describe(&self.primary),
            crate::foundation::error::describe(&self.secondary)
        )
    }
}

impl Error for CombinedBlobMaintenanceFailures {}

impl fmt::Display for BlobMaintenanceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "attachment blob maintenance failed: {}",
            self.operation
        )
    }
}

impl Error for BlobMaintenanceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source.as_ref())
    }
}

async fn cleanup_uncommitted<B: BlobStore>(
    blobs: &B,
    blob: &silverhand_backend_core::foundation::blob_store::StoredBlob,
    cause: &'static str,
) {
    if let Err(error) = blobs.delete(blob).await {
        tracing::warn!(
            error_chain = crate::foundation::safe_log_field::safe_display(
                &(crate::foundation::error::describe(&error))
            ),
            cause,
            "uncommitted attachment blob cleanup failed"
        );
    }
}

fn mint_attachment_id(
    environment: &impl SendEntropy,
) -> Result<AttachmentId, EnvironmentUnavailable> {
    let mut bytes = [0_u8; ATTACHMENT_ID_BYTES];
    environment.fill(&mut bytes)?;
    AttachmentId::parse(URL_SAFE_NO_PAD.encode(bytes)).map_err(EnvironmentUnavailable::from_source)
}

fn channel_coordinates(
    principal: Option<Extension<UserId>>,
    raw: String,
) -> Result<(UserId, ConversationId), Box<Response>> {
    let Some(Extension(viewer)) = principal else {
        tracing::error!("attachment route reached without human principal");
        return Err(Box::new(internal()));
    };
    let conversation = ConversationId::parse(raw)
        .map_err(|_| Box::new(error(StatusCode::BAD_REQUEST, "channel id required")))?;
    Ok((viewer, conversation))
}

fn attachment_coordinates(
    principal: Option<Extension<UserId>>,
    raw: String,
) -> Result<(UserId, AttachmentId), Box<Response>> {
    let Some(Extension(viewer)) = principal else {
        tracing::error!("attachment route reached without human principal");
        return Err(Box::new(internal()));
    };
    let attachment = AttachmentId::parse(raw)
        .map_err(|_| Box::new(error(StatusCode::BAD_REQUEST, "attachment id required")))?;
    Ok((viewer, attachment))
}

fn parse_list_query(raw: ListQueryWire) -> Result<AttachmentListQuery, Box<Response>> {
    let limit = parse_limit(raw.limit.as_deref());
    let cursor = raw
        .cursor
        .filter(|value| !value.is_empty())
        .map(|value| decode_cursor(&value))
        .transpose()
        .map_err(|_| {
            Box::new(coded(
                StatusCode::BAD_REQUEST,
                "invalid cursor",
                "invalid_cursor",
            ))
        })?;
    let sender = match (raw.sender_kind.as_deref(), raw.sender_id) {
        (None, None) => None,
        (None, Some(_)) => {
            return Err(Box::new(invalid_filter("sender_id requires sender_kind")));
        }
        (Some("system"), None) => Some(AttachmentSenderFilter::System),
        (Some("system"), Some(_)) => {
            return Err(Box::new(invalid_filter(
                "sender_id must be omitted when sender_kind=system",
            )));
        }
        (Some("user"), Some(id))
            if !crate::conversation_messaging::messaging::admission::ecmascript_trim(&id)
                .is_empty() =>
        {
            Some(AttachmentSenderFilter::Human(UserId::parse(id).map_err(
                |_| {
                    Box::new(invalid_filter(
                        "sender_id is required when sender_kind=user",
                    ))
                },
            )?))
        }
        (Some("agent"), Some(id))
            if !crate::conversation_messaging::messaging::admission::ecmascript_trim(&id)
                .is_empty() =>
        {
            Some(AttachmentSenderFilter::Agent(AgentId::parse(id).map_err(
                |_| {
                    Box::new(invalid_filter(
                        "sender_id is required when sender_kind=agent",
                    ))
                },
            )?))
        }
        (Some("user"), _) => {
            return Err(Box::new(invalid_filter(
                "sender_id is required when sender_kind=user",
            )));
        }
        (Some("agent"), _) => {
            return Err(Box::new(invalid_filter(
                "sender_id is required when sender_kind=agent",
            )));
        }
        (Some(_), _) => {
            return Err(Box::new(invalid_filter(
                "sender_kind must be user, agent, or system",
            )));
        }
    };
    let since = parse_epoch(raw.since.as_deref(), "since")?;
    let until = parse_epoch(raw.until.as_deref(), "until")?;
    AttachmentListQuery::new_safe(limit, cursor, sender, since, until)
        .map_err(|_| Box::new(invalid_filter("since must be strictly less than until")))
}

fn parse_limit(raw: Option<&str>) -> u16 {
    let Some(raw) = raw else {
        return DEFAULT_LIST_LIMIT;
    };
    let raw = crate::conversation_messaging::messaging::admission::ecmascript_trim(raw);
    if raw.starts_with('-') {
        return DEFAULT_LIST_LIMIT;
    }
    let digits: String = raw
        .strip_prefix('+')
        .unwrap_or(raw)
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    if digits.is_empty() {
        return DEFAULT_LIST_LIMIT;
    }
    match digits.parse::<u128>() {
        Ok(0) => DEFAULT_LIST_LIMIT,
        Ok(value) => value.min(u128::from(MAX_LIST_LIMIT)) as u16,
        Err(_) => MAX_LIST_LIMIT,
    }
}

fn parse_epoch(
    raw: Option<&str>,
    name: &'static str,
) -> Result<Option<JsSafeInteger>, Box<Response>> {
    let Some(raw) = raw else { return Ok(None) };
    let message = match name {
        "since" => "since must be a non-negative integer epoch-ms",
        _ => "until must be a non-negative integer epoch-ms",
    };
    if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(Box::new(invalid_filter(message)));
    }
    let value = raw
        .parse::<i64>()
        .map_err(|_| Box::new(invalid_filter(message)))?;
    let value = JsSafeInteger::new(value).map_err(|_| Box::new(invalid_filter(message)))?;
    Ok(Some(value))
}

fn decode_cursor(raw: &str) -> Result<AttachmentListCursor, ()> {
    let bytes = URL_SAFE_NO_PAD.decode(raw).map_err(|_| ())?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| ())?;
    let values = value
        .as_array()
        .filter(|values| values.len() == 2)
        .ok_or(())?;
    let appeared_at = values[0].as_i64().ok_or(())?;
    let attachment = values[1].as_str().ok_or(())?;
    AttachmentListCursor::new_safe(
        JsSafeInteger::new(appeared_at).map_err(|_| ())?,
        AttachmentId::parse(attachment).map_err(|_| ())?,
    )
    .map_err(|_| ())
}

fn encode_cursor(cursor: &AttachmentListCursor) -> String {
    URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&(cursor.appeared_at().get(), cursor.attachment().as_str()))
            .expect("typed attachment cursor always serializes"),
    )
}

fn list_success(page: &BoundAttachmentListPage) -> Response {
    Json(json!({
        "items": page.items().iter().map(list_item_json).collect::<Vec<_>>(),
        "next_cursor": page.next_cursor().map(encode_cursor),
    }))
    .into_response()
}

fn list_item_json(
    item: &silverhand_backend_core::conversation_messaging::messaging::collaboration::attachment::AttachmentListItem,
) -> Value {
    let source = item.source();
    let (from_kind, from_user_id, from_agent_id) = actor_wire(source.actor());
    json!({
        "attachment": record_json(item.attachment()),
        "source": {
            "message_id": source.message().as_str(),
            "conversation_id": source.conversation().as_str(),
            "thread_root_id": source.thread_root().map(|value| value.as_str()),
            "message_seq": source.message_seq().get(),
            "author": source.author().map(author_dto),
            "from_kind": from_kind,
            "from_user_id": from_user_id,
            "from_agent_id": from_agent_id,
            "appeared_at": source.appeared_at().get(),
        }
    })
}

fn actor_wire(actor: &AttachmentSourceActor) -> (&'static str, Option<&str>, Option<&str>) {
    match actor {
        AttachmentSourceActor::Human(id) => ("user", id.as_ref().map(UserId::as_str), None),
        AttachmentSourceActor::Agent(id) => ("agent", None, id.as_ref().map(AgentId::as_str)),
        AttachmentSourceActor::System => ("system", None, None),
    }
}

fn sender_facets_success(facets: &BoundAttachmentSenderFacets) -> Response {
    let senders: Vec<Value> = facets
        .senders()
        .iter()
        .map(|sender| match sender {
            AttachmentSenderFacet::Human(id) => {
                json!({"from_kind":"user","from_user_id":id.as_str(),"from_agent_id":Value::Null})
            }
            AttachmentSenderFacet::Agent(id) => {
                json!({"from_kind":"agent","from_user_id":Value::Null,"from_agent_id":id.as_str()})
            }
            AttachmentSenderFacet::System => {
                json!({"from_kind":"system","from_user_id":Value::Null,"from_agent_id":Value::Null})
            }
        })
        .collect();
    Json(json!({"senders":senders})).into_response()
}

fn record_json(record: &AttachmentRecord) -> Value {
    json!({
        "id": record.id().as_str(),
        "channel_id": record.channel().as_str(),
        "source_conversation_id": record.source_conversation().map(ConversationId::as_str),
        "filename": record.filename().as_str(),
        "size": record.size().get(),
        "content_type": record.content_type().as_str(),
        "storage": record.blob().storage().as_str(),
        "content_revision": record.content_revision().get(),
        "created_by_kind": record.creator().kind(),
        "created_by_id": record.creator().id(),
        "created_at": record.created_at().get(),
    })
}

pub fn changed_event(fact: &AttachmentChangedFact) -> Value {
    json!({
        "attachment_id": fact.attachment().as_str(),
        "content_revision": fact.content_revision().get(),
    })
}

/// Serialize a served blob to the download wire: raw bytes with the pinned
/// Content-Type / Content-Length / Content-Disposition / Cache-Control headers.
/// `pub(crate)`: the agent-audit attachment face serves bytes byte-identically to
/// this membership-gated download, so the two cannot drift on the wire.
pub(crate) fn download_success(record: &AttachmentRecord, bytes: Vec<u8>) -> Response {
    let mut response = Body::from(bytes).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(record.content_type().as_str())
            .expect("typed attachment content type is a valid header"),
    );
    response.headers_mut().insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&record.size().get().to_string())
            .expect("nonnegative size is a header"),
    );
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&content_disposition(record.filename().as_str()))
            .expect("sanitized filename produces an ASCII disposition"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response
}

fn content_disposition(filename: &str) -> String {
    let name = crate::conversation_messaging::messaging::admission::ecmascript_trim(filename);
    let name = if name.is_empty() {
        "download.bin"
    } else {
        name
    };
    let ascii: String = name
        .chars()
        .map(|character| {
            if character.is_ascii() && (' '..='~').contains(&character) && character != '"' {
                character
            } else {
                '_'
            }
        })
        .collect();
    let mut encoded = String::new();
    for byte in name.as_bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(byte) {
            encoded.push(char::from(*byte));
        } else {
            use fmt::Write as _;
            write!(&mut encoded, "%{byte:02X}").expect("writing to String cannot fail");
        }
    }
    format!("inline; filename=\"{ascii}\"; filename*=UTF-8''{encoded}")
}

fn sanitize_filename(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .chars()
        .map(|character| {
            if matches!(character, '/' | '\\') {
                '_'
            } else {
                character
            }
        })
        .filter(|character| !matches!(character, '\0'..='\u{1f}'))
        .collect();
    let cleaned = crate::conversation_messaging::messaging::admission::ecmascript_trim(&cleaned);
    if cleaned.is_empty() {
        return None;
    }
    if wire_text::within_utf16_max(cleaned, 255) {
        return Some(cleaned.to_owned());
    }
    let mut units = 0;
    let mut end = 0;
    for (offset, character) in cleaned.char_indices() {
        let next = units + character.len_utf16();
        if next > 255 {
            break;
        }
        units = next;
        end = offset + character.len_utf8();
    }
    Some(cleaned[..end].to_owned())
}

fn detect_content_type(
    explicit: Option<&str>,
    bytes: &[u8],
    filename: &str,
) -> Option<AttachmentContentType> {
    let normalized = explicit
        .and_then(|value| value.split(';').next())
        .map(crate::conversation_messaging::messaging::admission::ecmascript_trim)
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    let generic = matches!(
        normalized.as_str(),
        "" | "application/octet-stream" | "binary/octet-stream"
    ) || AttachmentContentType::parse(normalized.clone()).is_err();
    let selected = if !generic {
        normalized
    } else if let Some(sniffed) = sniff_magic(bytes) {
        sniffed.to_owned()
    } else {
        mime_from_filename(filename)
            .unwrap_or("application/octet-stream")
            .to_owned()
    };
    AttachmentContentType::parse(selected).ok()
}

fn sniff_magic(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, 0x50, 0x4e, 0x47]) {
        return Some("image/png");
    }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF8") {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    if bytes.starts_with(b"%PDF") {
        return Some("application/pdf");
    }
    if bytes.starts_with(&[0x50, 0x4b, 0x03, 0x04]) {
        return Some("application/zip");
    }
    None
}

fn mime_from_filename(filename: &str) -> Option<&'static str> {
    let (_, extension) = filename.rsplit_once('.')?;
    if filename.starts_with('.') || extension.is_empty() {
        return None;
    }
    match extension.to_ascii_lowercase().as_str() {
        "html" | "htm" => Some("text/html"),
        "svg" => Some("image/svg+xml"),
        "json" => Some("application/json"),
        "md" | "markdown" => Some("text/markdown"),
        "txt" | "log" => Some("text/plain"),
        "csv" => Some("text/csv"),
        "pdf" => Some("application/pdf"),
        "zip" => Some("application/zip"),
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

fn payload_matches_locked_type(bytes: &[u8], locked: &str) -> bool {
    let detected = sniff_magic(bytes).map(|value| {
        if value == "application/zip" {
            "zip"
        } else {
            value
        }
    });
    let expected = match locked {
        "image/png" | "image/jpeg" | "image/gif" | "image/webp" | "application/pdf" => Some(locked),
        value
            if value == "application/zip"
                || value.ends_with("+zip")
                || value.starts_with("application/vnd.openxmlformats-officedocument.")
                || value.starts_with("application/vnd.oasis.opendocument.") =>
        {
            Some("zip")
        }
        _ => None,
    };
    match expected {
        Some(expected) => detected == Some(expected),
        None => detected.is_none(),
    }
}

fn channel_refusal(refusal: ChannelAttachmentReadRefusal) -> Response {
    match refusal {
        ChannelAttachmentReadRefusal::NotFound => error(StatusCode::NOT_FOUND, "channel not found"),
        ChannelAttachmentReadRefusal::Forbidden => error(StatusCode::FORBIDDEN, "forbidden"),
    }
}

fn invalid_filter(message: &'static str) -> Response {
    coded(StatusCode::BAD_REQUEST, message, "invalid_filter")
}

fn attachment_not_found() -> Response {
    error(StatusCode::NOT_FOUND, "attachment not found")
}

fn update_forbidden() -> Response {
    error(
        StatusCode::FORBIDDEN,
        "cannot update attachment owned by another uploader",
    )
}

fn delete_forbidden() -> Response {
    error(
        StatusCode::FORBIDDEN,
        "cannot delete attachment owned by another uploader",
    )
}

fn internal() -> Response {
    error(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
}
