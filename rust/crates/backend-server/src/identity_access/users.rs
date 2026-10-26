//! The human-directory and avatar HTTP surface — the four routes that are NOT the
//! `me` profile pair (those live in [`crate::identity_access::human_auth`]):
//!
//! * `GET    /api/users`             — the global human directory.
//! * `POST   /api/me/avatar`         — upload/replace the caller's avatar.
//! * `DELETE /api/me/avatar`         — clear the caller's avatar.
//! * `GET    /api/users/{id}/avatar` — the public-by-id avatar byte fetch.
//!
//! All four sit behind the shared [`require_human_session`] perimeter: the two
//! `me` routes read the caller's own [`UserId`] from the extension, and the two
//! reads are "public to any authenticated viewer" (there is no hidden-profile
//! concept in V1). The handlers are thin transports over the application flows in
//! [`silverhand_backend_core::identity_access::avatar`] and the directory read; the domain owns
//! validation order, the committed-fact seal, the changed-only `user_changed`
//! announce, and the fetch's found/not-found/storage-fault trichotomy.
//!
//! Route registration into the served application is a later integration seam;
//! this module never touches the composition root or a route ledger.
//!
//! ## The DTO and avatar-URL rule are REUSED, not forked
//!
//! The directory and the mutating routes project the same [`UserDto`] (and its
//! cache-busted `avatar_url` rule) that `GET /api/me` does; this module imports it
//! from [`crate::identity_access::human_auth`] rather than defining a second identity/DTO/avatar-URL
//! authority.
//!
//! ## `/me` NoSuchUser is a dangling-session safe-500, not the TS 404
//!
//! Under the bearer perimeter the id on `POST`/`DELETE /api/me/avatar` is the
//! caller's OWN id from the session, so a missing row is persistence corruption —
//! the crate-wide safe-500 seam — never the TS `user_not_found` 404 (the same rule
//! `GET`/`PATCH /api/me` follow). The public fetch's id is a URL parameter, so an
//! unknown/malformed id there IS a product `404 avatar not found`.
//!
//! ## Two integration residuals (no production adapter ships here)
//!
//! * The `user_changed` announce sink is the #2220 residual (reused from
//!   [`crate::identity_access::human_auth`]'s wiring); a mutating avatar write owes the same
//!   changed-only app-scope hint a profile edit does.
//! * The avatar blob backend is the #2221 residual; this leaf depends only on the
//!   narrow [`AvatarBlobStore`] capability and ships no production adapter.

use axum::{
    Json, Router,
    body::Body,
    extract::{Extension, FromRequest, Multipart, Path, Request, State, multipart::MultipartError},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Serialize;
use silverhand_backend_core::{
    foundation::identity::UserId,
    identity_access::auth::{AuthenticateHumanSession, SessionClock},
    identity_access::avatar::{
        AVATAR_MAX_BYTES, AvatarBlobStore, AvatarContentType, AvatarFetchOutcome,
        AvatarPointerWriteUnitOfWork, AvatarRejection, AvatarStorageRefRead, AvatarUploadOutcome,
        BlobCleanup, CandidateAvatar, CommittedAvatar, clear_avatar, fetch_avatar, upload_avatar,
    },
    identity_access::user_identity::{UserChangedSink, UserDirectoryRead, UserProfileRead},
};

use crate::foundation::error;
use crate::foundation::multipart_transport::{accumulate_capped, declared_content_type, drain};
use crate::identity_access::auth::{PublicJsonUnauthorized, require_human_session};
use crate::identity_access::avatar_design::{DesignSlot, design_rejected};
use crate::identity_access::human_auth::{UserDto, log_lost_announce};

/// `apiPaths.users`.
pub const USERS_PATH: &str = "/api/users";
/// `apiPaths.meAvatar` — one path, `POST` and `DELETE`.
pub const ME_AVATAR_PATH: &str = "/api/me/avatar";
/// `apiPaths.userAvatar(':id')` in axum `{id}` capture form.
pub const USER_AVATAR_PATH: &str = "/api/users/{id}/avatar";

/// The safe-500 body, a bare `text/plain`, matching the crate's other routes.
const INTERNAL_ERROR_BODY: &str = "Internal Server Error";

// ===========================================================================
// Composition
// ===========================================================================

/// Everything the four routes need, one type per bounded-context capability so a
/// test drives driver-free fakes and the composition root wires the SQLite +
/// blob adapters (several resolve to one store in production). Used directly as
/// the axum state; `Clone` because every port is. The session-read authority for
/// the perimeter is passed separately to [`router`] (it is a middleware layer).
#[derive(Clone)]
pub struct UsersDeps<Dir, Refs, Rd, Wr, Blob, Sink, Clk> {
    /// The global directory read (`GET /api/users`), ordered `created_at ASC`.
    pub directory: Dir,
    /// The avatar serve-ref read (`GET /api/users/{id}/avatar`).
    pub avatar_refs: Refs,
    /// The self-profile existence read the upload pre-check consults.
    pub reads: Rd,
    /// The avatar-pointer write unit of work (`POST`/`DELETE /api/me/avatar`).
    pub avatar_writes: Wr,
    /// The narrow avatar blob capability. Production adapter is the #2221 residual.
    pub blobs: Blob,
    /// The `user_changed` announce sink. Production adapter is the #2220 residual.
    pub user_changed: Sink,
    /// The request-instant clock, reusing the auth context's [`SessionClock`].
    pub clock: Clk,
}

/// Mounts all four routes generic over their ports, wrapped by
/// [`require_human_session`] so the `me` handlers receive the caller's [`UserId`].
#[allow(clippy::type_complexity)]
pub fn router<Dir, Refs, Rd, Wr, Blob, Sink, Clk, A>(
    deps: UsersDeps<Dir, Refs, Rd, Wr, Blob, Sink, Clk>,
    session_authority: A,
) -> Router
where
    Dir: UserDirectoryRead,
    Refs: AvatarStorageRefRead,
    Rd: UserProfileRead,
    Wr: AvatarPointerWriteUnitOfWork,
    Blob: AvatarBlobStore,
    Sink: UserChangedSink,
    Clk: SessionClock,
    A: AuthenticateHumanSession,
{
    let protected = Router::new()
        .route(
            USERS_PATH,
            get(directory::<Dir, Refs, Rd, Wr, Blob, Sink, Clk>),
        )
        .route(
            ME_AVATAR_PATH,
            post(avatar_upload::<Dir, Refs, Rd, Wr, Blob, Sink, Clk>)
                .delete(avatar_clear::<Dir, Refs, Rd, Wr, Blob, Sink, Clk>),
        )
        .route(
            USER_AVATAR_PATH,
            get(avatar_fetch::<Dir, Refs, Rd, Wr, Blob, Sink, Clk>),
        )
        .with_state(deps);
    require_human_session(protected, session_authority, PublicJsonUnauthorized)
}

// ===========================================================================
// GET /api/users — the global human directory.
// ===========================================================================

#[allow(clippy::type_complexity)]
async fn directory<Dir, Refs, Rd, Wr, Blob, Sink, Clk>(
    State(deps): State<UsersDeps<Dir, Refs, Rd, Wr, Blob, Sink, Clk>>,
) -> Response
where
    Dir: UserDirectoryRead,
    Refs: AvatarStorageRefRead,
    Rd: UserProfileRead,
    Wr: AvatarPointerWriteUnitOfWork,
    Blob: AvatarBlobStore,
    Sink: UserChangedSink,
    Clk: SessionClock,
{
    match deps.directory.list().await {
        Ok(users) => {
            let users: Vec<UserDto> = users.iter().map(UserDto::from_record).collect();
            Json(UsersListResponse { users }).into_response()
        }
        Err(cause) => {
            tracing::error!(
                error_chain =
                    crate::foundation::safe_log_field::safe_display(&(error::describe(&cause))),
                "user directory read failed"
            );
            internal_error()
        }
    }
}

// ===========================================================================
// POST /api/me/avatar — upload/replace.
// ===========================================================================

#[allow(clippy::type_complexity)]
async fn avatar_upload<Dir, Refs, Rd, Wr, Blob, Sink, Clk>(
    viewer: Option<Extension<UserId>>,
    State(deps): State<UsersDeps<Dir, Refs, Rd, Wr, Blob, Sink, Clk>>,
    request: Request,
) -> Response
where
    Dir: UserDirectoryRead,
    Refs: AvatarStorageRefRead,
    Rd: UserProfileRead,
    Wr: AvatarPointerWriteUnitOfWork,
    Blob: AvatarBlobStore,
    Sink: UserChangedSink,
    Clk: SessionClock,
{
    let Some(Extension(user_id)) = viewer else {
        tracing::error!("POST /api/me/avatar reached without its human principal fact");
        return internal_error();
    };

    // A non-multipart body (or a malformed multipart stream) is the product's
    // `c.req.formData()` throw → one terminal 400 `multipart/form-data required`.
    let mut multipart = match Multipart::from_request(request, &()).await {
        Ok(multipart) => multipart,
        Err(_) => return multipart_required(),
    };
    let (file, design) = match first_file_field(&mut multipart).await {
        Ok((Some(file), design)) => (file, design),
        // No part named `file`, or the first one is not a file (no filename) —
        // the product's `!(file instanceof File)` 400.
        Ok((None, _)) => return file_field_required(),
        // A stream fault mid-parse folds to the same `formData()` throw → 400.
        Err(_) => return multipart_required(),
    };
    let design = match design.admit() {
        Ok(design) => design,
        Err(rejection) => return design_rejected(rejection),
    };

    let now_ms = deps.clock.now_unix_millis();
    let outcome = upload_avatar(
        &deps.reads,
        &deps.avatar_writes,
        &deps.blobs,
        &deps.user_changed,
        &user_id,
        CandidateAvatar {
            bytes: file.bytes,
            declared_content_type: file.content_type,
            design,
        },
        now_ms,
    )
    .await;
    match outcome {
        Ok(AvatarUploadOutcome::Committed(committed)) => avatar_committed_upload(committed),
        // The caller's own session id resolved to no row: dangling pointer → 500.
        // A new blob may have been written and then orphaned by the vanish; its
        // cleanup fate is logged before the safe-500.
        Ok(AvatarUploadOutcome::NoSuchUser { orphan_cleanup }) => {
            log_blob_cleanup(&orphan_cleanup, "avatar upload orphan");
            dangling_session(&user_id, "avatar upload")
        }
        Ok(AvatarUploadOutcome::Rejected(rejection)) => avatar_rejected(rejection),
        // The blob backend could not store the bytes: log the cause chain, then
        // serve the stable, detail-free `avatar_storage_failed`.
        Ok(AvatarUploadOutcome::StorageFailed(failure)) => {
            tracing::error!(
                error_chain =
                    crate::foundation::safe_log_field::safe_display(&(error::describe(&failure))),
                "avatar blob store put failed"
            );
            avatar_storage_failed()
        }
        Err(cause) => {
            tracing::error!(
                error_chain =
                    crate::foundation::safe_log_field::safe_display(&(error::describe(&cause))),
                "avatar upload failed"
            );
            internal_error()
        }
    }
}

// ===========================================================================
// DELETE /api/me/avatar — clear.
// ===========================================================================

#[allow(clippy::type_complexity)]
async fn avatar_clear<Dir, Refs, Rd, Wr, Blob, Sink, Clk>(
    viewer: Option<Extension<UserId>>,
    State(deps): State<UsersDeps<Dir, Refs, Rd, Wr, Blob, Sink, Clk>>,
) -> Response
where
    Dir: UserDirectoryRead,
    Refs: AvatarStorageRefRead,
    Rd: UserProfileRead,
    Wr: AvatarPointerWriteUnitOfWork,
    Blob: AvatarBlobStore,
    Sink: UserChangedSink,
    Clk: SessionClock,
{
    let Some(Extension(user_id)) = viewer else {
        tracing::error!("DELETE /api/me/avatar reached without its human principal fact");
        return internal_error();
    };

    let now_ms = deps.clock.now_unix_millis();
    match clear_avatar(
        &deps.avatar_writes,
        &deps.blobs,
        &deps.user_changed,
        &user_id,
        now_ms,
    )
    .await
    {
        // The clear route returns the bare profile DTO (no `avatar_url` mirror).
        Ok(AvatarUploadOutcome::Committed(committed)) => {
            let (user, announce, blob_cleanup) = committed.into_parts();
            log_lost_announce(&announce, "avatar clear");
            log_blob_cleanup(&blob_cleanup, "avatar clear");
            Json(UserDto::from_record(&user)).into_response()
        }
        Ok(AvatarUploadOutcome::NoSuchUser { .. }) => dangling_session(&user_id, "avatar clear"),
        // Clear never validates a payload or writes a blob, so these arms are
        // unreachable; they still take the safe-500 rather than inventing a body.
        Ok(AvatarUploadOutcome::Rejected(_)) | Ok(AvatarUploadOutcome::StorageFailed(_)) => {
            tracing::error!("avatar clear produced an upload-only outcome");
            internal_error()
        }
        Err(cause) => {
            tracing::error!(
                error_chain =
                    crate::foundation::safe_log_field::safe_display(&(error::describe(&cause))),
                "avatar clear failed"
            );
            internal_error()
        }
    }
}

// ===========================================================================
// GET /api/users/{id}/avatar — the public-by-id byte fetch.
// ===========================================================================

#[allow(clippy::type_complexity)]
async fn avatar_fetch<Dir, Refs, Rd, Wr, Blob, Sink, Clk>(
    State(deps): State<UsersDeps<Dir, Refs, Rd, Wr, Blob, Sink, Clk>>,
    Path(id): Path<String>,
) -> Response
where
    Dir: UserDirectoryRead,
    Refs: AvatarStorageRefRead,
    Rd: UserProfileRead,
    Wr: AvatarPointerWriteUnitOfWork,
    Blob: AvatarBlobStore,
    Sink: UserChangedSink,
    Clk: SessionClock,
{
    // The id is a URL parameter, not a session fact: an unknown/malformed id is a
    // product `404 avatar not found` (the product resolves it to a null ref),
    // never the dangling-session safe-500 the `me` routes use.
    let Ok(user_id) = UserId::parse(id) else {
        return avatar_not_found();
    };
    match fetch_avatar(&deps.avatar_refs, &deps.blobs, &user_id).await {
        Ok(AvatarFetchOutcome::Found {
            bytes,
            content_type,
            filename,
        }) => serve_avatar(bytes, content_type, &filename),
        Ok(AvatarFetchOutcome::NotFound) => avatar_not_found(),
        // The row points at a blob that could not be served: storage inconsistency
        // → the product's `serve` throws → safe-500. A hard read fault carries its
        // cause chain to the log; a clean "missing bytes" carries none.
        Ok(AvatarFetchOutcome::StorageUnavailable { cause }) => {
            match &cause {
                Some(failure) => tracing::error!(
                    user_id = crate::foundation::safe_log_field::safe_display(&(user_id.as_str())),
                    error_chain = crate::foundation::safe_log_field::safe_display(
                        &(error::describe(failure))
                    ),
                    "avatar blob read failed"
                ),
                None => tracing::error!(
                    user_id = crate::foundation::safe_log_field::safe_display(&(user_id.as_str())),
                    "avatar row points at a missing blob"
                ),
            }
            internal_error()
        }
        Err(cause) => {
            tracing::error!(
                error_chain =
                    crate::foundation::safe_log_field::safe_display(&(error::describe(&cause))),
                "avatar fetch failed"
            );
            internal_error()
        }
    }
}

// ===========================================================================
// Multipart admission
// ===========================================================================

/// A multipart `file` part reduced to what the upload flow needs.
struct UploadedFile {
    bytes: Vec<u8>,
    content_type: String,
}

/// Find the FIRST multipart part named `file`, mirroring `form.get('file')`, and the
/// optional `design` part in any position. The part must be a file (carry a
/// filename); a same-named non-file part yields `None` (→ `file field required`).
/// Every part is read to the end, so a stream/decoding fault anywhere is `Err`
/// (→ the `formData()` throw's 400).
async fn first_file_field(
    multipart: &mut Multipart,
) -> Result<(Option<UploadedFile>, DesignSlot), MultipartError> {
    let mut design = DesignSlot::default();
    let mut claimed: Option<Option<UploadedFile>> = None;
    while let Some(mut field) = multipart.next_field().await? {
        if design.offer(&mut field).await? {
            continue;
        }
        if claimed.is_some() || field.name() != Some("file") {
            drain(&mut field).await?;
            continue;
        }
        if field.file_name().is_none() {
            drain(&mut field).await?;
            claimed = Some(None);
            continue;
        }
        // `file.type || 'application/octet-stream'`; the raw declared type flows to
        // `AvatarContentType::parse` unfolded. Retaining `AVATAR_MAX_BYTES + 1` lets
        // `validate_avatar` tell an at-cap file from the exact `413`.
        let content_type = declared_content_type(&field);
        let bytes = accumulate_capped(&mut field, AVATAR_MAX_BYTES as usize + 1).await?;
        drain(&mut field).await?;
        claimed = Some(Some(UploadedFile {
            bytes,
            content_type,
        }));
    }
    Ok((claimed.flatten(), design))
}

// ===========================================================================
// Wire projection
// ===========================================================================

/// `GET /api/users`'s body: `{ users: UserDto[] }`.
#[derive(Serialize)]
struct UsersListResponse {
    users: Vec<UserDto>,
}

/// The avatar-upload success body: `{ avatar_url, user }` — the top-level
/// `avatar_url` repeats `user.avatar_url` exactly as the product does.
#[derive(Serialize)]
struct AvatarUploadResponse {
    avatar_url: Option<String>,
    user: UserDto,
}

/// The `{ error }` refusal body (the fetch/multipart string refusals).
#[derive(Serialize)]
struct ErrorBody {
    error: &'static str,
}

/// The `{ error, code }` refusal body the avatar validation/storage faults use.
#[derive(Serialize)]
struct ErrorCodeBody {
    error: &'static str,
    code: &'static str,
}

fn avatar_committed_upload(committed: CommittedAvatar) -> Response {
    let (user, announce, blob_cleanup) = committed.into_parts();
    log_lost_announce(&announce, "avatar upload");
    log_blob_cleanup(&blob_cleanup, "avatar upload");
    let dto = UserDto::from_record(&user);
    Json(AvatarUploadResponse {
        avatar_url: dto.avatar_url(),
        user: dto,
    })
    .into_response()
}

/// Log a post-commit blob cleanup that failed — a benign but real storage leak
/// carried out of the domain as a named [`BlobCleanup`] residue. Only the failure
/// arm logs; a clean delete or a nothing-to-clean is silent. Mirrors
/// [`log_lost_announce`]'s carried-failure boundary logging.
fn log_blob_cleanup(cleanup: &BlobCleanup, origin: &str) {
    if let BlobCleanup::Failed(failure) = cleanup {
        tracing::error!(
            error_chain =
                crate::foundation::safe_log_field::safe_display(&(error::describe(failure))),
            origin,
            "avatar blob cleanup failed; a storage blob is orphaned"
        );
    }
}

/// Serve the found bytes with the same header set as the product's blob `serve`:
/// the stored content type (already an allowlist member, so `normalizeContentType`
/// is the identity), an explicit `Content-Length`, an `inline` disposition with
/// both an ASCII and a UTF-8 filename, and `Cache-Control: private, no-store`. Our
/// filename is always `avatar.<subtype>` (pure ASCII), so the two disposition
/// filenames coincide.
fn serve_avatar(bytes: Vec<u8>, content_type: AvatarContentType, filename: &str) -> Response {
    let disposition = format!("inline; filename=\"{filename}\"; filename*=UTF-8''{filename}");
    let content_length = bytes.len();
    (
        [
            (header::CONTENT_TYPE, content_type.as_str().to_owned()),
            (header::CONTENT_LENGTH, content_length.to_string()),
            (header::CONTENT_DISPOSITION, disposition),
            (header::CACHE_CONTROL, "private, no-store".to_owned()),
        ],
        Body::from(bytes),
    )
        .into_response()
}

/// The avatar validation refusals map to the product's fixed statuses:
/// `avatar_missing` → 400, `avatar_too_large` → 413, `avatar_unsupported_type` →
/// 415. The body is `{ error: message, code }` with the domain's stable copy.
fn avatar_rejected(rejection: AvatarRejection) -> Response {
    let status = match rejection {
        AvatarRejection::Empty => StatusCode::BAD_REQUEST,
        AvatarRejection::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
        AvatarRejection::UnsupportedType => StatusCode::UNSUPPORTED_MEDIA_TYPE,
    };
    (
        status,
        Json(ErrorCodeBody {
            error: rejection.message(),
            code: rejection.code(),
        }),
    )
        .into_response()
}

/// The blob backend could not store the bytes — the product's
/// `avatar_storage_failed` (400). The product's message interpolates the adapter
/// error; that suffix is unstable adapter detail no consumer branches on (only the
/// `code` is a contract), so a stable message is served and the detail is logged.
fn avatar_storage_failed() -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(ErrorCodeBody {
            error: "avatar storage failed",
            code: "avatar_storage_failed",
        }),
    )
        .into_response()
}

fn avatar_not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(ErrorBody {
            error: "avatar not found",
        }),
    )
        .into_response()
}

fn multipart_required() -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(ErrorBody {
            error: "multipart/form-data required",
        }),
    )
        .into_response()
}

fn file_field_required() -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(ErrorBody {
            error: "file field required",
        }),
    )
        .into_response()
}

/// A validated session that resolves to no `users` row is a dangling pointer —
/// persistence corruption, the crate's safe-500 seam, never a product 404.
fn dangling_session(user_id: &UserId, origin: &str) -> Response {
    tracing::error!(
        user_id = crate::foundation::safe_log_field::safe_display(&(user_id.as_str())),
        origin,
        "an authenticated session resolves to no user row"
    );
    internal_error()
}

fn internal_error() -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, INTERNAL_ERROR_BODY).into_response()
}
