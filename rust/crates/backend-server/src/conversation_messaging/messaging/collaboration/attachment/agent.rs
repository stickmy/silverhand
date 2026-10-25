//! Unmounted HTTP adapters for internal-agent attachment delivery.
//!
//! Runtime identity and the route `aid` are sealed before any body extractor is
//! entered. Upload's destination remains inside the existing multipart wire, so
//! only after the bounded envelope is parsed can target resolution and product
//! authority run. Update is object-addressed and therefore completes its
//! authority read before polling the replacement body.

#[cfg(test)]
mod tests;

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Extension, Path, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, patch, post},
};
use serde::Deserialize;
use serde_json::json;
use silverhand_backend_core::{
    conversation_messaging::messaging::{
        AddressRefusal, AttachmentId, SendClock, SendEntropy, TargetAddress,
        collaboration::attachment::{
            AgentAttachmentContentUpdateCommand, AgentAttachmentContentUpdateRefusal,
            AgentAttachmentContentUpdateUnitOfWork, AgentAttachmentCreateCommand,
            AgentAttachmentCreateRefusal, AgentAttachmentCreateUnitOfWork,
            AgentAttachmentDownloadReadPort, AgentAttachmentLocateReadPort,
            AgentAttachmentReadRefusal, AgentAttachmentRuntime, AgentAttachmentTarget,
            AgentAttachmentTargetRefusal, AgentAttachmentUpdateAdmissionPort,
            AgentAttachmentUpdateAdmissionRefusal, AgentAttachmentUploadAdmissionPort,
            AgentAttachmentUploadRefusal, AttachmentChangedPublisher, AttachmentFilename,
            AttachmentPostCommitMaintenance,
        },
    },
    foundation::blob_store::{BlobAliasId, BlobNamespace, BlobReadOutcome, BlobStore},
    foundation::identity::AgentId,
    host_runtime::runtime_credential::RuntimePrincipal,
};
use silverhand_backend_filesystem::foundation::blob_store::{
    LocalBlobAliasOutcome, LocalBlobAliasStore,
};

use super::{
    BlobMaintenance, MULTIPART_ENVELOPE_BYTES, agent_multipart_upload, attachment_not_found,
    cleanup_uncommitted, coded, detect_content_type, download_success, error, internal,
    mint_attachment_id, multipart_file, payload_matches_locked_type, record_json,
    sanitize_filename,
};
use crate::conversation_messaging::messaging::agent_capability_wire::capability_denied;

pub const INTERNAL_AGENT_ATTACHMENTS_PATH: &str = "/internal/agents/{aid}/attachments";
pub const INTERNAL_AGENT_ATTACHMENT_PATH: &str =
    "/internal/agents/{aid}/attachments/{attachment_id}";
pub const INTERNAL_AGENT_ATTACHMENT_LOCATE_PATH: &str =
    "/internal/agents/{aid}/attachments/{attachment_id}/locate";
pub const INTERNAL_AGENT_ATTACHMENT_DOWNLOAD_PATH: &str =
    "/internal/agents/{aid}/attachments/{attachment_id}/download";

#[derive(Deserialize)]
struct AgentPath {
    aid: String,
}

#[derive(Deserialize)]
struct AgentAttachmentPath {
    aid: String,
    attachment_id: String,
}

struct UploadInner<T, A, B, E> {
    targets: T,
    attachments: A,
    blobs: B,
    environment: E,
}

struct UploadState<T, A, B, E>(Arc<UploadInner<T, A, B, E>>);

impl<T, A, B, E> Clone for UploadState<T, A, B, E> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

pub fn agent_attachment_upload_router<T, A, B, E>(
    targets: T,
    attachments: A,
    blobs: B,
    environment: E,
) -> Router
where
    T: TargetAddress + Send + Sync + 'static,
    A: AgentAttachmentUploadAdmissionPort + AgentAttachmentCreateUnitOfWork + Send + Sync + 'static,
    B: BlobStore + Send + Sync + 'static,
    E: SendClock + SendEntropy + Send + Sync + 'static,
{
    Router::new()
        .route(INTERNAL_AGENT_ATTACHMENTS_PATH, post(upload::<T, A, B, E>))
        .layer(DefaultBodyLimit::max(MULTIPART_ENVELOPE_BYTES))
        .with_state(UploadState(Arc::new(UploadInner {
            targets,
            attachments,
            blobs,
            environment,
        })))
}

struct UpdateInner<A, B, P> {
    attachments: A,
    blobs: B,
    publisher: P,
}

struct UpdateState<A, B, P>(Arc<UpdateInner<A, B, P>>);

impl<A, B, P> Clone for UpdateState<A, B, P> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

pub fn agent_attachment_update_router<A, B, P>(attachments: A, blobs: B, publisher: P) -> Router
where
    A: AgentAttachmentUpdateAdmissionPort
        + AgentAttachmentContentUpdateUnitOfWork
        + Send
        + Sync
        + 'static,
    B: BlobStore + LocalBlobAliasStore + Send + Sync + 'static,
    P: AttachmentChangedPublisher + Send + Sync + 'static,
{
    Router::new()
        .route(INTERNAL_AGENT_ATTACHMENT_PATH, patch(update::<A, B, P>))
        .layer(DefaultBodyLimit::max(MULTIPART_ENVELOPE_BYTES))
        .with_state(UpdateState(Arc::new(UpdateInner {
            attachments,
            blobs,
            publisher,
        })))
}

struct ReadInner<A, B> {
    attachments: A,
    blobs: B,
}

struct ReadState<A, B>(Arc<ReadInner<A, B>>);

impl<A, B> Clone for ReadState<A, B> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

pub fn agent_attachment_locate_router<A, B>(attachments: A, blobs: B) -> Router
where
    A: AgentAttachmentLocateReadPort + Send + Sync + 'static,
    B: LocalBlobAliasStore + Send + Sync + 'static,
{
    Router::new()
        .route(INTERNAL_AGENT_ATTACHMENT_LOCATE_PATH, get(locate::<A, B>))
        .with_state(ReadState(Arc::new(ReadInner { attachments, blobs })))
}

pub fn agent_attachment_download_router<A, B>(attachments: A, blobs: B) -> Router
where
    A: AgentAttachmentDownloadReadPort + Send + Sync + 'static,
    B: BlobStore + Send + Sync + 'static,
{
    Router::new()
        .route(
            INTERNAL_AGENT_ATTACHMENT_DOWNLOAD_PATH,
            get(download::<A, B>),
        )
        .with_state(ReadState(Arc::new(ReadInner { attachments, blobs })))
}

async fn upload<T, A, B, E>(
    State(state): State<UploadState<T, A, B, E>>,
    principal: Option<Extension<RuntimePrincipal>>,
    Path(path): Path<AgentPath>,
    request: Request,
) -> Response
where
    T: TargetAddress,
    A: AgentAttachmentUploadAdmissionPort + AgentAttachmentCreateUnitOfWork,
    B: BlobStore,
    E: SendClock + SendEntropy,
{
    let runtime = match runtime_coordinates(principal, path.aid) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    let upload = match agent_multipart_upload(request).await {
        Ok(value) => value,
        Err(response) => return *response,
    };
    let resolved = match state
        .0
        .targets
        .resolve(
            runtime.agent(),
            &upload.target,
            runtime.target_allocation(),
            silverhand_backend_core::conversation_messaging::messaging::DeliveryIntent::Write,
        )
        .await
    {
        Ok(Ok(value)) => value,
        Ok(Err(refusal)) => return address_refusal(refusal),
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "agent attachment target resolution failed"
            );
            return internal();
        }
    };
    let target = match AgentAttachmentTarget::resolved(&runtime, resolved) {
        Ok(value) => value,
        Err(refusal) => return target_refusal(refusal),
    };
    let admission = match state
        .0
        .attachments
        .admit_upload(runtime.agent(), target.conversation())
        .await
    {
        Ok(Ok(value)) => match value.bind(&target) {
            Ok(value) => value,
            Err(error) => {
                tracing::error!(
                    error_chain = crate::foundation::safe_log_field::safe_display(
                        &(crate::foundation::error::describe(&error))
                    ),
                    "agent attachment upload admission identity seal failed"
                );
                return internal();
            }
        },
        Ok(Err(refusal)) => return upload_refusal(refusal),
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "agent attachment upload admission failed"
            );
            return internal();
        }
    };

    let file = upload.file;
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
                "agent attachment id entropy unavailable"
            );
            return internal();
        }
    };
    let created_at = match state.0.environment.now_unix_ms() {
        Ok(value) => value,
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "agent attachment clock unavailable"
            );
            return internal();
        }
    };
    let namespace =
        match BlobNamespace::for_semantic_id("attachments", admission.conversation().as_str()) {
            Ok(value) => value,
            Err(error) => {
                tracing::error!(
                    error_chain = crate::foundation::safe_log_field::safe_display(
                        &(crate::foundation::error::describe(&error))
                    ),
                    "agent attachment namespace projection failed"
                );
                return internal();
            }
        };
    let size = file.bytes.len() as i64;
    let blob = match state
        .0
        .blobs
        .put(&namespace, content_type.as_str(), file.bytes)
        .await
    {
        Ok(value) => value,
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "agent attachment blob write failed"
            );
            return internal();
        }
    };
    let command = match AgentAttachmentCreateCommand::new(
        &admission,
        attachment,
        filename,
        size,
        content_type,
        blob.clone(),
        created_at,
    ) {
        Ok(value) => value,
        Err(error) => {
            cleanup_uncommitted(&state.0.blobs, &blob, "invalid agent upload command").await;
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "agent attachment upload command projection failed"
            );
            return internal();
        }
    };
    match state.0.attachments.create_for_agent(command).await {
        Ok(Ok(fact)) => match fact.bind(&runtime, target.conversation()) {
            Ok(bound) => (StatusCode::CREATED, Json(record_json(bound.record()))).into_response(),
            Err(error) => {
                tracing::error!(
                    error_chain = crate::foundation::safe_log_field::safe_display(
                        &(crate::foundation::error::describe(&error))
                    ),
                    "agent attachment create identity seal failed"
                );
                internal()
            }
        },
        Ok(Err(refusal)) => {
            cleanup_uncommitted(&state.0.blobs, &blob, "refused agent upload").await;
            create_refusal(refusal)
        }
        Err(error) => {
            cleanup_uncommitted(&state.0.blobs, &blob, "failed agent upload persistence").await;
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "agent attachment upload persistence failed"
            );
            internal()
        }
    }
}

async fn update<A, B, P>(
    State(state): State<UpdateState<A, B, P>>,
    principal: Option<Extension<RuntimePrincipal>>,
    Path(path): Path<AgentAttachmentPath>,
    request: Request,
) -> Response
where
    A: AgentAttachmentUpdateAdmissionPort + AgentAttachmentContentUpdateUnitOfWork,
    B: BlobStore + LocalBlobAliasStore,
    P: AttachmentChangedPublisher,
{
    let (runtime, attachment) = match attachment_coordinates(principal, path) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    let admission = match state
        .0
        .attachments
        .admit_update(&runtime, &attachment)
        .await
    {
        Ok(Ok(value)) => match value.bind(&runtime, &attachment) {
            Ok(value) => value,
            Err(error) => {
                tracing::error!(
                    error_chain = crate::foundation::safe_log_field::safe_display(
                        &(crate::foundation::error::describe(&error))
                    ),
                    "agent attachment update admission identity seal failed"
                );
                return internal();
            }
        },
        Ok(Err(refusal)) => return update_admission_refusal(refusal),
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "agent attachment update admission failed"
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
                "agent attachment update namespace projection failed"
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
                "agent attachment replacement blob write failed"
            );
            return internal();
        }
    };
    let command = match AgentAttachmentContentUpdateCommand::new(&admission, size, blob.clone()) {
        Ok(value) => value,
        Err(error) => {
            cleanup_uncommitted(&state.0.blobs, &blob, "invalid agent update command").await;
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "agent attachment update command projection failed"
            );
            return internal();
        }
    };
    let fact = match state.0.attachments.update_content_for_agent(command).await {
        Ok(Ok(fact)) => fact,
        Ok(Err(refusal)) => {
            cleanup_uncommitted(&state.0.blobs, &blob, "refused agent update").await;
            return update_refusal(refusal);
        }
        Err(error) => {
            cleanup_uncommitted(&state.0.blobs, &blob, "failed agent update persistence").await;
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "agent attachment update persistence failed"
            );
            return internal();
        }
    };
    let bound = match fact.bind(&runtime, &attachment) {
        Ok(value) => value,
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "agent attachment update identity seal failed"
            );
            return internal();
        }
    };
    if let Err(error) = BlobMaintenance(state.0.blobs.clone())
        .updated(bound.before().blob(), bound.after().blob(), bound.alias())
        .await
    {
        tracing::warn!(
            error_chain = crate::foundation::safe_log_field::safe_display(
                &(crate::foundation::error::describe(&error))
            ),
            "committed agent attachment update blob maintenance incomplete"
        );
    }
    let changed = bound.changed();
    if let Err(error) = state.0.publisher.publish(&changed).await {
        tracing::error!(
            error_chain = crate::foundation::safe_log_field::safe_display(
                &(crate::foundation::error::describe(&error))
            ),
            "committed agent attachment_changed publication failed"
        );
    }
    Json(record_json(bound.after())).into_response()
}

async fn locate<A, B>(
    State(state): State<ReadState<A, B>>,
    principal: Option<Extension<RuntimePrincipal>>,
    Path(path): Path<AgentAttachmentPath>,
) -> Response
where
    A: AgentAttachmentLocateReadPort,
    B: LocalBlobAliasStore,
{
    let (runtime, attachment) = match attachment_coordinates(principal, path) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    let fact = match state
        .0
        .attachments
        .locate_for_agent(runtime.agent(), &attachment)
        .await
    {
        Ok(Ok(value)) => value,
        Ok(Err(refusal)) => return read_refusal(refusal),
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "agent attachment locate authority failed"
            );
            return internal();
        }
    };
    let bound = match fact.bind(&runtime, &attachment) {
        Ok(value) => value,
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "agent attachment locate identity seal failed"
            );
            return internal();
        }
    };
    let alias = match BlobAliasId::from_semantic_id(attachment.as_str()) {
        Ok(value) => value,
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "agent attachment alias projection failed"
            );
            return internal();
        }
    };
    let record = bound.record();
    match state.0.blobs.materialize_alias(record.blob(), &alias).await {
        Ok(LocalBlobAliasOutcome::Located(path)) => {
            let Some(local_path) = path.as_path().to_str() else {
                tracing::error!(
                    attachment =
                        crate::foundation::safe_log_field::safe_display(&(attachment.as_str())),
                    "agent attachment alias path is not UTF-8"
                );
                return internal();
            };
            Json(json!({
                "storage": "local",
                "local_path": local_path,
                "filename": record.filename().as_str(),
                "content_type": record.content_type().as_str(),
            }))
            .into_response()
        }
        Ok(LocalBlobAliasOutcome::MissingPhysical) => {
            tracing::error!(
                attachment =
                    crate::foundation::safe_log_field::safe_display(&(attachment.as_str())),
                "committed agent attachment blob is missing during locate"
            );
            internal()
        }
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "agent attachment alias materialization failed"
            );
            internal()
        }
    }
}

async fn download<A, B>(
    State(state): State<ReadState<A, B>>,
    principal: Option<Extension<RuntimePrincipal>>,
    Path(path): Path<AgentAttachmentPath>,
) -> Response
where
    A: AgentAttachmentDownloadReadPort,
    B: BlobStore,
{
    let (runtime, attachment) = match attachment_coordinates(principal, path) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    let fact = match state
        .0
        .attachments
        .download_for_agent(runtime.agent(), &attachment)
        .await
    {
        Ok(Ok(value)) => value,
        Ok(Err(refusal)) => return read_refusal(refusal),
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "agent attachment download authority failed"
            );
            return internal();
        }
    };
    let bound = match fact.bind(&runtime, &attachment) {
        Ok(value) => value,
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "agent attachment download identity seal failed"
            );
            return internal();
        }
    };
    let record = bound.record();
    let read = match state.0.blobs.read(record.blob()).await {
        Ok(BlobReadOutcome::Found(value)) => value,
        Ok(BlobReadOutcome::Missing) => {
            tracing::error!(
                attachment =
                    crate::foundation::safe_log_field::safe_display(&(attachment.as_str())),
                "committed agent attachment blob is missing during download"
            );
            return internal();
        }
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "agent attachment blob read failed"
            );
            return internal();
        }
    };
    if i64::try_from(read.actual_size()).ok() != Some(record.size().get()) {
        tracing::error!(
            attachment = crate::foundation::safe_log_field::safe_display(&(attachment.as_str())),
            expected = record.size().get(),
            actual = read.actual_size(),
            "agent attachment blob size differs from committed row"
        );
        return internal();
    }
    download_success(record, read.into_bytes())
}

fn runtime_coordinates(
    principal: Option<Extension<RuntimePrincipal>>,
    raw_agent: String,
) -> Result<AgentAttachmentRuntime, Box<Response>> {
    let Some(Extension(principal)) = principal else {
        tracing::error!("agent attachment route reached without runtime principal");
        return Err(Box::new(internal()));
    };
    let path_agent = AgentId::parse(raw_agent)
        .map_err(|_| Box::new(error(StatusCode::BAD_REQUEST, "agent id required")))?;
    AgentAttachmentRuntime::bind(&principal, &path_agent).map_err(|error| {
        tracing::error!(
            error_chain = crate::foundation::safe_log_field::safe_display(
                &(crate::foundation::error::describe(&error))
            ),
            "agent attachment runtime/path identity seal failed"
        );
        Box::new(internal())
    })
}

fn attachment_coordinates(
    principal: Option<Extension<RuntimePrincipal>>,
    path: AgentAttachmentPath,
) -> Result<(AgentAttachmentRuntime, AttachmentId), Box<Response>> {
    let runtime = runtime_coordinates(principal, path.aid)?;
    let attachment = AttachmentId::parse(path.attachment_id)
        .map_err(|_| Box::new(error(StatusCode::BAD_REQUEST, "attachment id required")))?;
    Ok((runtime, attachment))
}

fn address_refusal(refusal: AddressRefusal) -> Response {
    let (status, message, code) = match refusal {
        AddressRefusal::Malformed => (
            StatusCode::BAD_REQUEST,
            "target is not an address",
            "target_malformed",
        ),
        AddressRefusal::SelfPair => (
            StatusCode::BAD_REQUEST,
            "a direct message needs another participant",
            "self_pair_not_addressable",
        ),
        AddressRefusal::DirectMessageScope => (
            StatusCode::FORBIDDEN,
            "Conversation-scoped agents do not support DMs. Use a shared channel.",
            "direct_message_scope_forbidden",
        ),
        AddressRefusal::Unresolved => (
            StatusCode::NOT_FOUND,
            "target names no stream you can use",
            "target_unresolved",
        ),
        AddressRefusal::ThreadNotInTarget => (
            StatusCode::NOT_FOUND,
            "target names no reply cluster of that conversation",
            "thread_not_in_target",
        ),
    };
    coded(status, message, code)
}

fn target_refusal(refusal: AgentAttachmentTargetRefusal) -> Response {
    match refusal {
        AgentAttachmentTargetRefusal::ThreadSuffixNotSupported => coded(
            StatusCode::BAD_REQUEST,
            "this operation addresses a conversation, not a reply cluster",
            "thread_suffix_not_supported",
        ),
        AgentAttachmentTargetRefusal::SlotOutOfScope => coded(
            StatusCode::FORBIDDEN,
            "this operation acts only in the conversation your session is anchored to",
            "slot_out_of_scope",
        ),
    }
}

fn upload_refusal(refusal: AgentAttachmentUploadRefusal) -> Response {
    match refusal {
        AgentAttachmentUploadRefusal::NotFound => error(StatusCode::NOT_FOUND, "channel not found"),
        AgentAttachmentUploadRefusal::CapabilityDenied => capability_denied("upload_attachments"),
    }
}

fn create_refusal(refusal: AgentAttachmentCreateRefusal) -> Response {
    match refusal {
        AgentAttachmentCreateRefusal::NotFound => error(StatusCode::NOT_FOUND, "channel not found"),
        AgentAttachmentCreateRefusal::CapabilityDenied => capability_denied("upload_attachments"),
    }
}

fn update_admission_refusal(refusal: AgentAttachmentUpdateAdmissionRefusal) -> Response {
    match refusal {
        AgentAttachmentUpdateAdmissionRefusal::NotFound => attachment_not_found(),
        AgentAttachmentUpdateAdmissionRefusal::CapabilityDenied => {
            capability_denied("upload_attachments")
        }
        AgentAttachmentUpdateAdmissionRefusal::NotCreator => error(
            StatusCode::FORBIDDEN,
            "cannot update attachment owned by another uploader",
        ),
    }
}

fn update_refusal(refusal: AgentAttachmentContentUpdateRefusal) -> Response {
    match refusal {
        AgentAttachmentContentUpdateRefusal::NotFound => attachment_not_found(),
        AgentAttachmentContentUpdateRefusal::CapabilityDenied => {
            capability_denied("upload_attachments")
        }
        AgentAttachmentContentUpdateRefusal::NotCreator => error(
            StatusCode::FORBIDDEN,
            "cannot update attachment owned by another uploader",
        ),
    }
}

fn read_refusal(refusal: AgentAttachmentReadRefusal) -> Response {
    match refusal {
        AgentAttachmentReadRefusal::NotFound => attachment_not_found(),
        AgentAttachmentReadRefusal::CapabilityDenied => capability_denied("read_attachments"),
    }
}
