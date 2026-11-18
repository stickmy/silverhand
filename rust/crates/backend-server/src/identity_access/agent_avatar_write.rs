//! Authenticated write face for `POST` / `DELETE /api/agents/{id}/avatar`: upload
//! (or replace) and clear one agent's avatar, behind the same visibility + authority
//! ladder the sibling [`super::agent_patch`] tuning edit uses.
//!
//! Both verbs are public MANAGEMENT-plane surfaces (creator-or-system on the human
//! side; agent-self is reachable only via the internal route), so they compose the
//! same ladder the tuning edit does:
//!
//!  1. VISIBILITY ([`can_view_agent`], active-only) — an absent row, a caller outside
//!     the ACL, and a non-active stage all collapse to ONE
//!     `404 {"error":"agent not found"}`, so neither surface is an existence oracle.
//!  2. AUTHORITY ([`can_edit_agent`]) — only the creator (or system) may MUTATE it; a
//!     caller who may merely see it gets `403 {"error":"forbidden"}`. Access precedes
//!     product: a refused caller never reaches the payload or the write.
//!  3. (POST only) WIRE ADMISSION — a non-multipart body (or a mid-stream fault) is the
//!     product's `c.req.formData()` throw → `400 {"error":"multipart/form-data
//!     required"}`; a missing/`non-File` `file` part is `400 {"error":"file field
//!     required"}`. The declared part MIME (`file.type || 'application/octet-stream'`)
//!     rides through to the domain validator, which owns the empty→size→type refusal.
//!  4. WRITE + CLEANUP + ANNOUNCE — hand the subject (already past the gates) to the
//!     avatar use case ([`upload_agent_avatar`] / [`clear_agent_avatar`]), which drives
//!     the blob store and the canonical `agents`-row writer, cleans up orphaned/replaced
//!     blobs, and fires the best-effort `agent_changed` hint. The domain's typed outcome
//!     is mapped to HTTP here; the carried announce and blob-cleanup fates are logged,
//!     never allowed to flip a durable commit.
//!  5. PROJECT — re-read the committed row through the canonical `agent_detail`
//!     projector and return the FULL DTO, so `avatar_url`'s cache-buster reflects the
//!     just-stamped `updated_at`. POST returns `{ avatar_url, agent }` (the top-level
//!     `avatar_url` mirrors `agent.avatar_url`); DELETE returns `{ agent }`. A concurrent
//!     delete that races the projection is honest post-commit residue: the write is
//!     durable but no trustworthy DTO exists, so it takes the lane's safe `500`.
//!
//! As in the sibling routes the human principal is proof the authenticating perimeter
//! ran; its ABSENCE is a composition fault (a fail-closed `500` that reads and writes
//! nothing), and mounting the real bearer perimeter is the composition root's job —
//! done by the backend-app #2234 CP2 mount.
//!
//! ## Multipart admission composes shared transport primitives, keeps its own policy
//!
//! The bounded read, the field drain, and the "declared type or octet-stream default"
//! read are the route-neutral [`crate::foundation::multipart_transport`] primitives, shared with the
//! sibling avatar/profile faces. What stays HERE is this route's POLICY: the whole
//! envelope is decoded (every part drained to end, including the selected file's bytes
//! past `cap + 1`) so a malformed/truncated part ANYWHERE — even after a valid `file` —
//! is the transport `400`, mirroring `await c.req.formData()` parsing the entire body
//! before `form.get('file')`. The byte cap is single-sourced from
//! [`silverhand_backend_core::identity_access::avatar::AVATAR_MAX_BYTES`], and the MIME allowlist is owned
//! solely by [`silverhand_backend_core::identity_access::avatar::AvatarContentType::parse`] (which
//! lowercases and exact-matches), so this route adds no second normalizer.

use axum::{
    Json, Router,
    extract::{Extension, FromRequest, Multipart, Path, Request, State, multipart::MultipartError},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use serde::Serialize;
use silverhand_backend_core::{
    foundation::identity::{AgentId, Principal, UserId},
    identity_access::agent_avatar::AgentAvatarBlobStore,
    identity_access::agent_avatar_write::{
        AgentAvatarClearOutcome, AgentAvatarUploadOutcome, clear_agent_avatar, upload_agent_avatar,
    },
    identity_access::agent_detail_read::{AgentDetailFacts, AgentDetailReadModel},
    identity_access::agent_profile_write::{
        AgentChangedAnnounce, AgentChangedSink, AgentProfileWriteUnitOfWork,
    },
    identity_access::avatar::{AVATAR_MAX_BYTES, AvatarRejection, BlobCleanup, CandidateAvatar},
    identity_access::capability::{
        agent_authority::{AgentAuthority, can_edit_agent},
        agent_view::{VisibleStages, can_view_agent},
    },
};

use crate::foundation::clock::ProcessClock;
use crate::foundation::error;

use super::agent_avatar::AGENT_AVATAR_PATH;
use crate::foundation::multipart_transport::{accumulate_capped, declared_content_type, drain};
use crate::identity_access::agent_wire::{AgentDto, reproject_committed_agent};
use crate::identity_access::avatar_design::{DesignSlot, design_rejected};

/// The anti-enumeration refusal string, pinned to the TypeScript handler.
const AGENT_NOT_FOUND: &str = "agent not found";
/// The authority-refusal string (`c.json({ error: 'forbidden' }, 403)`).
const FORBIDDEN: &str = "forbidden";
/// The unmapped-error body: a bare `text/plain` `500`, never a JSON envelope.
const INTERNAL_ERROR_BODY: &str = "Internal Server Error";

#[derive(Clone)]
struct AvatarWriteState<R, W, B, S, C> {
    reads: R,
    writes: W,
    blobs: B,
    sink: S,
    clock: C,
}

pub fn router<R, W, B, S, C>(reads: R, writes: W, blobs: B, sink: S, clock: C) -> Router
where
    R: AgentDetailReadModel + Clone + Send + Sync + 'static,
    W: AgentProfileWriteUnitOfWork,
    B: AgentAvatarBlobStore,
    S: AgentChangedSink,
    C: ProcessClock,
{
    Router::new()
        .route(
            AGENT_AVATAR_PATH,
            post(avatar_upload::<R, W, B, S, C>).delete(avatar_clear::<R, W, B, S, C>),
        )
        .with_state(AvatarWriteState {
            reads,
            writes,
            blobs,
            sink,
            clock,
        })
}

// ===========================================================================
// POST — upload / replace.
// ===========================================================================

async fn avatar_upload<R, W, B, S, C>(
    Path(agent_id): Path<String>,
    viewer: Option<Extension<UserId>>,
    State(state): State<AvatarWriteState<R, W, B, S, C>>,
    request: Request,
) -> Response
where
    R: AgentDetailReadModel,
    W: AgentProfileWriteUnitOfWork,
    B: AgentAvatarBlobStore,
    S: AgentChangedSink,
    C: ProcessClock,
{
    let Some(principal) = principal(viewer, "agent-avatar-upload") else {
        return internal_error();
    };
    let Ok(agent) = AgentId::parse(agent_id) else {
        return agent_not_found();
    };
    if let Some(refusal) = gate_avatar_edit(&state.reads, &principal, &agent).await {
        return refusal;
    }

    // WIRE ADMISSION: only past the gates do we touch the body.
    let mut multipart = match Multipart::from_request(request, &()).await {
        Ok(multipart) => multipart,
        Err(_) => return multipart_required(),
    };
    let (file, design) = match first_file_field(&mut multipart).await {
        Ok((Some(file), design)) => (file, design),
        Ok((None, _)) => return file_field_required(),
        Err(_) => return multipart_required(),
    };
    let design = match design.admit() {
        Ok(design) => design,
        Err(rejection) => return design_rejected(rejection),
    };

    let now_ms = state.clock.now_unix_millis();
    let outcome = upload_agent_avatar(
        &state.writes,
        &state.blobs,
        &state.sink,
        &agent,
        CandidateAvatar {
            bytes: file.bytes,
            declared_content_type: file.content_type,
            design,
        },
        now_ms,
    )
    .await;

    match outcome {
        AgentAvatarUploadOutcome::Committed(effect) => {
            let (id, announce, blob_cleanup) = effect.into_parts();
            log_lost_announce(&announce, "agent-avatar upload");
            log_blob_cleanup(&blob_cleanup, "agent-avatar upload replaced blob");
            match reproject_committed_agent(&state.reads, &id).await {
                Ok(dto) => Json(AvatarUploadResponse {
                    avatar_url: dto.avatar_url(),
                    agent: dto,
                })
                .into_response(),
                Err(fault) => fault.into_safe_500("agent-avatar upload", &id),
            }
        }
        // The row vanished between the gate and the repoint; the fresh blob was
        // orphaned and cleaned best-effort. Log the fate, then the anti-enumeration 404.
        AgentAvatarUploadOutcome::NoSuchAgent { orphan_cleanup } => {
            log_blob_cleanup(&orphan_cleanup, "agent-avatar upload orphan (row vanished)");
            agent_not_found()
        }
        AgentAvatarUploadOutcome::Rejected(rejection) => avatar_rejected(rejection),
        AgentAvatarUploadOutcome::StorageFailed(failure) => {
            tracing::error!(
                error_chain =
                    crate::foundation::safe_log_field::safe_display(&(error::describe(&failure))),
                "agent-avatar blob store put failed"
            );
            avatar_storage_failed()
        }
        AgentAvatarUploadOutcome::WriteFaulted {
            cause,
            orphan_cleanup,
        } => {
            tracing::error!(
                error_chain =
                    crate::foundation::safe_log_field::safe_display(&(error::describe(&cause))),
                "agent-avatar upload write faulted before a durable commit"
            );
            log_blob_cleanup(
                &orphan_cleanup,
                "agent-avatar upload orphan (write faulted)",
            );
            internal_error()
        }
        // A durable commit whose witness could not be trusted: the fresh blob is
        // deliberately RETAINED (deleting it would strand the committed row on a dangling
        // pointer), no announce fired. Record the retained pointer for reconciliation.
        AgentAvatarUploadOutcome::CommitUntrustworthy { retained_fresh } => {
            tracing::error!(
                retained_blob = retained_fresh.as_str(),
                "agent-avatar upload committed durably but the write fact was untrustworthy; \
                 the fresh blob is retained pending reconciliation"
            );
            internal_error()
        }
    }
}

// ===========================================================================
// DELETE — clear.
// ===========================================================================

async fn avatar_clear<R, W, B, S, C>(
    Path(agent_id): Path<String>,
    viewer: Option<Extension<UserId>>,
    State(state): State<AvatarWriteState<R, W, B, S, C>>,
) -> Response
where
    R: AgentDetailReadModel,
    W: AgentProfileWriteUnitOfWork,
    B: AgentAvatarBlobStore,
    S: AgentChangedSink,
    C: ProcessClock,
{
    let Some(principal) = principal(viewer, "agent-avatar-clear") else {
        return internal_error();
    };
    let Ok(agent) = AgentId::parse(agent_id) else {
        return agent_not_found();
    };
    if let Some(refusal) = gate_avatar_edit(&state.reads, &principal, &agent).await {
        return refusal;
    }

    let now_ms = state.clock.now_unix_millis();
    let outcome =
        match clear_agent_avatar(&state.writes, &state.blobs, &state.sink, &agent, now_ms).await {
            Ok(outcome) => outcome,
            Err(cause) => {
                tracing::error!(
                    error_chain =
                        crate::foundation::safe_log_field::safe_display(&(error::describe(&cause))),
                    "agent-avatar clear write failed"
                );
                return internal_error();
            }
        };

    // A genuine clear and an already-absent no-op both return the current DTO; they
    // differ only in whether a blob was removed / an announce owed. The id to re-read is
    // the same source-bound subject either way.
    let id = match outcome {
        AgentAvatarClearOutcome::Cleared(effect) => {
            let (id, announce, blob_cleanup) = effect.into_parts();
            log_lost_announce(&announce, "agent-avatar clear");
            log_blob_cleanup(&blob_cleanup, "agent-avatar clear removed blob");
            id
        }
        AgentAvatarClearOutcome::NoOp { agent_id } => agent_id,
        AgentAvatarClearOutcome::NoSuchAgent => return agent_not_found(),
    };

    match reproject_committed_agent(&state.reads, &id).await {
        Ok(dto) => Json(AgentEnvelope { agent: dto }).into_response(),
        Err(fault) => fault.into_safe_500("agent-avatar clear", &id),
    }
}

// ===========================================================================
// Shared gate + projection.
// ===========================================================================

/// Recover the human principal, or log the composition fault and signal a fail-closed
/// `500` (the caller returns [`internal_error`]).
fn principal(viewer: Option<Extension<UserId>>, route: &str) -> Option<Principal> {
    match viewer {
        Some(Extension(viewer)) => Some(Principal::human(viewer)),
        None => {
            tracing::error!(
                route,
                "agent-avatar write route reached without its human principal"
            );
            None
        }
    }
}

/// The shared visibility + authority ladder both verbs run before touching the payload
/// or the write. Returns `Some(refusal)` — the early `Response` — on any failure; `None`
/// means the caller may mutate this agent's avatar.
pub(crate) async fn gate_avatar_edit<R>(
    reads: &R,
    principal: &Principal,
    agent: &AgentId,
) -> Option<Response>
where
    R: AgentDetailReadModel,
{
    let detail = match reads.read_agent_detail(agent).await {
        Ok(detail) => detail,
        Err(cause) => {
            tracing::error!(
                error_chain =
                    crate::foundation::safe_log_field::safe_display(&(error::describe(&cause))),
                "agent-avatar write pre-gate read failed"
            );
            return Some(internal_error());
        }
    };

    // Bind the resolved row to the request before either gate: a bundle decoded for
    // another agent would gate one row and mutate another — a resolver defect, safe 500.
    if let Some(detail) = detail.as_ref()
        && detail.source() != agent
    {
        tracing::error!(
            requested = agent.as_str(),
            resolved = detail.source().as_str(),
            "agent-avatar write resolved for the wrong agent"
        );
        return Some(internal_error());
    }

    // VISIBILITY (active-only): absence, non-membership, and a non-active stage all
    // collapse to one anti-enumeration 404.
    let decision = can_view_agent(
        principal,
        agent,
        detail.as_ref().map(AgentDetailFacts::visibility),
        &VisibleStages::active_only(),
    );
    let Some(detail) = detail.filter(|_| decision.is_allowed()) else {
        return Some(agent_not_found());
    };

    // AUTHORITY: only the creator (or system) may mutate it.
    let authority = AgentAuthority {
        created_by_user_id: Some(detail.created_by_user_id().clone()),
    };
    if !can_edit_agent(principal, agent, Some(&authority)).is_allowed() {
        return Some(forbidden());
    }
    None
}

/// Log a lost best-effort `agent_changed` hint; a delivered hint is silent.
fn log_lost_announce(announce: &AgentChangedAnnounce, origin: &str) {
    if let AgentChangedAnnounce::Failed(failure) = announce {
        tracing::error!(
            error_chain =
                crate::foundation::safe_log_field::safe_display(&(error::describe(failure))),
            origin,
            "agent-avatar write committed but the agent_changed announce failed"
        );
    }
}

/// Log a failed post-commit blob cleanup — a benign but real storage leak carried out of
/// the domain as a named [`BlobCleanup`] residue. Only the failure arm logs.
fn log_blob_cleanup(cleanup: &BlobCleanup, origin: &str) {
    if let BlobCleanup::Failed(failure) = cleanup {
        tracing::error!(
            error_chain =
                crate::foundation::safe_log_field::safe_display(&(error::describe(failure))),
            origin,
            "agent-avatar blob cleanup failed; a storage blob is orphaned"
        );
    }
}

// ===========================================================================
// Multipart admission (shared transport primitives; whole-envelope drain policy).
// ===========================================================================

/// A multipart `file` part reduced to what the upload flow needs.
struct UploadedFile {
    bytes: Vec<u8>,
    content_type: String,
}

/// Decode the WHOLE multipart envelope, mirroring `await c.req.formData()` followed by
/// `form.get('file')`. `formData()` parses EVERY part before any field is inspected, so a
/// malformed/truncated part ANYWHERE is the transport `400` — even after a valid `file`
/// part, and even past the domain size cap. We therefore keep the first `file`-named slot's
/// decision (first-wins, like `form.get`) but still drain every remaining part (and the file
/// part's own bytes past `cap + 1`) to completion, so a trailing fault refuses at transport
/// before the domain validator ever runs. `Ok(Some)` is that first file; `Ok(None)` is
/// `file field required` (no `file` part, or a same-named non-file part); `Err` is the
/// `formData()` throw's `400`.
async fn first_file_field(
    multipart: &mut Multipart,
) -> Result<(Option<UploadedFile>, DesignSlot), MultipartError> {
    let mut design = DesignSlot::default();
    // Outer `Some` = the first `file`-named slot has been claimed; inner `Option` = it was a
    // file (`Some`) or a same-named non-file (`None`). The claim is made on the first match
    // but the loop runs to the end regardless, so `?` still surfaces a later part's fault.
    let mut claimed: Option<Option<UploadedFile>> = None;
    while let Some(mut field) = multipart.next_field().await? {
        if design.offer(&mut field).await? {
            continue;
        }
        if claimed.is_none() && field.name() == Some("file") {
            if field.file_name().is_none() {
                // A non-file `file` slot is `file field required`, but only AFTER the whole
                // envelope decodes cleanly — so drain it and keep parsing the rest.
                drain(&mut field).await?;
                claimed = Some(None);
            } else {
                // Raw declared type (or the octet-stream default); the domain's
                // `AvatarContentType::parse` lowercases and exact-matches, so no transport
                // normalizer is applied here.
                let content_type = declared_content_type(&field);
                // Retain at most `cap + 1` — enough for the domain validator to tell a
                // `cap` file (accept) from `cap + 1` (the exact `413`) — then drain the rest
                // of this part so a truncated tail still surfaces as the transport `400`.
                let bytes = accumulate_capped(&mut field, AVATAR_MAX_BYTES as usize + 1).await?;
                drain(&mut field).await?;
                claimed = Some(Some(UploadedFile {
                    bytes,
                    content_type,
                }));
            }
        } else {
            // Not the claimed slot: drain fully with zero accumulation so its bytes still pass
            // through the parser and any fault becomes the transport `400`.
            drain(&mut field).await?;
        }
    }
    Ok((claimed.flatten(), design))
}

// ===========================================================================
// Responses.
// ===========================================================================

/// POST success body: `{ avatar_url, agent }` — the top-level `avatar_url` repeats
/// `agent.avatar_url` exactly as the product does.
#[derive(Serialize)]
struct AvatarUploadResponse {
    avatar_url: Option<String>,
    agent: AgentDto,
}

/// DELETE success body: `{ agent }`, the full DTO the canonical projector builds.
#[derive(Serialize)]
struct AgentEnvelope {
    agent: AgentDto,
}

/// The flat `{ "error": ... }` refusal body for the static refusals.
#[derive(Serialize)]
struct ErrorBody {
    error: &'static str,
}

/// The `{ "error": <message>, "code": <code> }` refusal body the avatar validation /
/// storage faults use.
#[derive(Serialize)]
struct ErrorCodeBody {
    error: &'static str,
    code: &'static str,
}

pub(crate) fn agent_not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(ErrorBody {
            error: AGENT_NOT_FOUND,
        }),
    )
        .into_response()
}

fn forbidden() -> Response {
    (StatusCode::FORBIDDEN, Json(ErrorBody { error: FORBIDDEN })).into_response()
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

/// The avatar validation refusals map to the product's fixed statuses:
/// `avatar_missing` → 400, `avatar_too_large` → 413, `avatar_unsupported_type` → 415.
/// The body is `{ error: message, code }` with the domain's stable copy.
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

/// The blob backend could not store the bytes — the product's `avatar_storage_failed`
/// (400). The product interpolates the adapter error into the message; that suffix is
/// unstable adapter detail no consumer branches on (only the `code` is a contract), so a
/// stable message is served and the detail is logged.
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

fn internal_error() -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, INTERNAL_ERROR_BODY).into_response()
}

#[cfg(test)]
mod tests {
    use std::{
        io,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering::SeqCst},
        },
    };

    use axum::{
        body::{Body, to_bytes},
        extract::DefaultBodyLimit,
        http::{Request, StatusCode},
    };
    use serde_json::{Value, json};
    use silverhand_backend_core::{
        foundation::persistence::PersistenceFailure,
        identity_access::agent_avatar::{
            AgentAvatarBlobPointer, AgentAvatarBlobRead, AgentAvatarRow, OwnedAgentAvatarBlob,
        },
        identity_access::agent_detail_read::{AgentDetailFacts, AgentDetailProjection},
        identity_access::agent_lifecycle::MaterializationStatus,
        identity_access::agent_profile_write::{
            AdmittedAgentProfileEdit, AgentChangedAnnounceFailure, AgentProfileRowWrite,
            AgentRowBefore, AgentTuningContext, CommittedAgentRow,
        },
        identity_access::agents::runtime::RuntimeKind,
        identity_access::avatar::{AvatarBlobReadOutcome, AvatarContentType, AvatarStorageFailure},
    };
    use tower::ServiceExt;

    use super::*;

    const NOW: i64 = 5_000;
    /// The pointer the fake blob store mints on its FIRST `put` — deterministic so the
    /// committed row can name it.
    const FRESH: &str = "agents/mint/blob-0";

    fn user(id: &str) -> UserId {
        UserId::parse(id).expect("user id")
    }

    fn agent(id: &str) -> AgentId {
        AgentId::parse(id).expect("agent id")
    }

    fn ptr(raw: &str) -> AgentAvatarBlobPointer {
        AgentAvatarBlobPointer::parse(raw).unwrap()
    }

    // ---- fakes ----

    #[derive(Clone)]
    struct FixedClock(i64);
    impl ProcessClock for FixedClock {
        fn now_unix_millis(&self) -> i64 {
            self.0
        }
    }

    /// A read model answering the pre-gate read with `first` and every later read (the
    /// post-commit projection) with `rest`.
    #[derive(Clone)]
    struct FakeReads {
        first: ReadOutcome,
        rest: ReadOutcome,
        calls: Arc<AtomicUsize>,
    }

    #[derive(Clone)]
    enum ReadOutcome {
        Present(Box<AgentDetailFacts>),
        Absent,
        Fails,
    }

    impl FakeReads {
        fn new(first: ReadOutcome, rest: ReadOutcome) -> Self {
            Self {
                first,
                rest,
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl AgentDetailReadModel for FakeReads {
        async fn read_agent_detail(
            &self,
            _agent: &AgentId,
        ) -> Result<Option<AgentDetailFacts>, PersistenceFailure> {
            let nth = self.calls.fetch_add(1, SeqCst);
            let outcome = if nth == 0 { &self.first } else { &self.rest };
            match outcome {
                ReadOutcome::Present(facts) => Ok(Some(facts.as_ref().clone())),
                ReadOutcome::Absent => Ok(None),
                ReadOutcome::Fails => {
                    Err(PersistenceFailure::from_source(io::Error::other("boom")))
                }
            }
        }
    }

    fn present(facts: AgentDetailFacts) -> ReadOutcome {
        ReadOutcome::Present(Box::new(facts))
    }

    /// A writer returning a fixed fact (or a fault), ignoring the decision: the DOMAIN
    /// re-derives the verdict from the witnessed before-image, which is what the outcome
    /// mapping here consumes. Counts calls so a gate refusal can prove `writes == 0`.
    #[derive(Clone)]
    struct FakeWrites {
        outcome: WriteFixture,
        calls: Arc<AtomicUsize>,
    }

    #[derive(Clone)]
    // A test fixture; the size asymmetry between a marker fault and a full committed row
    // is intended and never allocated in a hot path.
    #[allow(clippy::large_enum_variant)]
    enum WriteFixture {
        Fault,
        Row(AgentProfileRowWrite),
    }

    impl FakeWrites {
        fn new(outcome: WriteFixture) -> Self {
            Self {
                outcome,
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl AgentProfileWriteUnitOfWork for FakeWrites {
        async fn write_agent_row<D>(
            &self,
            _agent: &AgentId,
            _decide: D,
            _now_ms: i64,
        ) -> Result<AgentProfileRowWrite, PersistenceFailure>
        where
            D: FnOnce(&AgentRowBefore) -> Option<AdmittedAgentProfileEdit> + Send,
        {
            self.calls.fetch_add(1, SeqCst);
            match &self.outcome {
                WriteFixture::Fault => Err(PersistenceFailure::from_source(io::Error::other(
                    "write boom",
                ))),
                WriteFixture::Row(row) => Ok(row.clone()),
            }
        }
    }

    /// A blob store: `put` mints a deterministic sequential pointer and counts puts;
    /// `delete` records the deleted pointer and can be made to fail.
    #[derive(Clone)]
    struct FakeBlobs {
        puts: Arc<AtomicUsize>,
        next: Arc<AtomicUsize>,
        deletes: Arc<Mutex<Vec<String>>>,
        fail_put: bool,
        fail_delete: bool,
    }

    impl FakeBlobs {
        fn new() -> Self {
            Self {
                puts: Arc::new(AtomicUsize::new(0)),
                next: Arc::new(AtomicUsize::new(0)),
                deletes: Arc::new(Mutex::new(Vec::new())),
                fail_put: false,
                fail_delete: false,
            }
        }
        fn failing_put(mut self) -> Self {
            self.fail_put = true;
            self
        }
        fn deleted(&self) -> Vec<String> {
            self.deletes.lock().unwrap().clone()
        }
        fn puts(&self) -> usize {
            self.puts.load(SeqCst)
        }
    }

    impl AgentAvatarBlobRead for FakeBlobs {
        async fn read(
            &self,
            _blob: &OwnedAgentAvatarBlob,
        ) -> Result<AvatarBlobReadOutcome, AvatarStorageFailure> {
            Ok(AvatarBlobReadOutcome::Missing)
        }
    }

    impl AgentAvatarBlobStore for FakeBlobs {
        async fn put(
            &self,
            _agent_id: &AgentId,
            _content_type: AvatarContentType,
            _bytes: Vec<u8>,
        ) -> Result<AgentAvatarBlobPointer, AvatarStorageFailure> {
            if self.fail_put {
                #[derive(Debug, thiserror::Error)]
                #[error("disk full")]
                struct DiskFull;
                return Err(AvatarStorageFailure::from_source(DiskFull));
            }
            let n = self.next.fetch_add(1, SeqCst);
            self.puts.fetch_add(1, SeqCst);
            Ok(ptr(&format!("agents/mint/blob-{n}")))
        }

        async fn delete(&self, blob: &OwnedAgentAvatarBlob) -> Result<(), AvatarStorageFailure> {
            self.deletes
                .lock()
                .unwrap()
                .push(blob.pointer().as_str().to_owned());
            if self.fail_delete {
                #[derive(Debug, thiserror::Error)]
                #[error("unlink failed")]
                struct UnlinkFailed;
                Err(AvatarStorageFailure::from_source(UnlinkFailed))
            } else {
                Ok(())
            }
        }
    }

    #[derive(Clone)]
    struct FakeSink {
        calls: Arc<AtomicUsize>,
        fail: bool,
    }
    impl FakeSink {
        fn new(fail: bool) -> Self {
            Self {
                calls: Arc::new(AtomicUsize::new(0)),
                fail,
            }
        }
    }
    impl AgentChangedSink for FakeSink {
        async fn agent_changed(&self, _agent: &AgentId) -> Result<(), AgentChangedAnnounceFailure> {
            self.calls.fetch_add(1, SeqCst);
            if self.fail {
                Err(AgentChangedAnnounceFailure::from_source(io::Error::other(
                    "bus down",
                )))
            } else {
                Ok(())
            }
        }
    }

    // ---- before-image / committed row builders (avatar-only seal) ----

    fn ctx() -> AgentTuningContext {
        AgentTuningContext {
            runtime: RuntimeKind::Claude,
            model: None,
            reasoning_effort: None,
        }
    }

    fn before(avatar: AgentAvatarRow) -> AgentRowBefore {
        AgentRowBefore {
            agent_id: agent("a1"),
            display_name: "Agent One".to_owned(),
            description: None,
            service_tier: None,
            avatar,
            context: ctx(),
        }
    }

    fn committed(avatar: AgentAvatarRow) -> CommittedAgentRow {
        CommittedAgentRow {
            id: agent("a1"),
            display_name: "Agent One".to_owned(),
            description: None,
            model: None,
            reasoning_effort: None,
            service_tier: None,
            avatar,
            updated_at: NOW,
        }
    }

    fn present_avatar(raw: &str) -> AgentAvatarRow {
        AgentAvatarRow::Present {
            pointer: ptr(raw),
            content_type: AvatarContentType::Png,
        }
    }

    // ---- projection / facts fixtures ----

    fn projection(avatar_key: Option<&str>) -> AgentDetailProjection {
        AgentDetailProjection {
            name: "agent-one".to_owned(),
            display_name: "Agent One".to_owned(),
            runtime: RuntimeKind::Claude,
            model: None,
            reasoning_effort: None,
            service_tier: None,
            host_id: "h1".to_owned(),
            host_display_label: None,
            host_name: Some("host-one".to_owned()),
            description: None,
            session_scope: "agent".to_owned(),
            avatar_storage_key: avatar_key.map(str::to_owned),
            materialization_failed_code: None,
            materialization_failed_at: None,
            deletion_started_at: None,
            created_at: 1000,
            updated_at: NOW,
        }
    }

    /// The creator arm admits `viewer-1`; active stage; avatar present in the DTO.
    fn creator_facts() -> AgentDetailFacts {
        AgentDetailFacts::from_source(
            agent("a1"),
            user("viewer-1"),
            [],
            [],
            MaterializationStatus::Active,
            projection(Some("agents/a1/avatars/blob-0.png")),
        )
    }

    /// `viewer-1` may SEE it (workspace member) but did NOT create it, so authority denies.
    fn workspace_only_facts() -> AgentDetailFacts {
        AgentDetailFacts::from_source(
            agent("a1"),
            user("creator-user"),
            [],
            [user("viewer-1")],
            MaterializationStatus::Active,
            projection(Some("agents/a1/avatars/blob-0.png")),
        )
    }

    /// The post-commit projection: avatar present (upload) or absent (clear).
    fn projected_facts(avatar_key: Option<&str>) -> AgentDetailFacts {
        AgentDetailFacts::from_source(
            agent("a1"),
            user("viewer-1"),
            [],
            [],
            MaterializationStatus::Active,
            projection(avatar_key),
        )
    }

    // ---- request drivers ----

    struct Reply {
        status: StatusCode,
        body: Vec<u8>,
        reads: usize,
        writes: usize,
        announces: usize,
    }

    fn parsed(reply: &Reply) -> Value {
        serde_json::from_slice(&reply.body).unwrap()
    }

    const BOUNDARY: &str = "XBOUNDARYX";

    /// Build a `multipart/form-data` body with one `file` part.
    fn multipart_file(filename: Option<&str>, content_type: Option<&str>, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        let disposition = match filename {
            Some(name) => {
                format!("Content-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\n")
            }
            None => "Content-Disposition: form-data; name=\"file\"\r\n".to_owned(),
        };
        out.extend_from_slice(disposition.as_bytes());
        if let Some(ct) = content_type {
            out.extend_from_slice(format!("Content-Type: {ct}\r\n").as_bytes());
        }
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(bytes);
        out.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
        out
    }

    /// A body whose FIRST `file` part is complete and valid, followed by a second part that
    /// is opened but truncated — its data never terminates with a boundary, so the multipart
    /// parser hits end-of-stream mid-part and faults. Drives the "drain the whole envelope"
    /// contract: the valid first file must not shortcut past the malformed tail.
    fn multipart_valid_file_then_truncated_part() -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        out.extend_from_slice(
            b"Content-Disposition: form-data; name=\"file\"; filename=\"a.png\"\r\n",
        );
        out.extend_from_slice(b"Content-Type: image/png\r\n\r\n");
        out.extend_from_slice(&[1, 2, 3]);
        // Open a second part, then cut the stream off inside its data with no closing
        // boundary — a premature end the parser cannot complete.
        out.extend_from_slice(format!("\r\n--{BOUNDARY}\r\n").as_bytes());
        out.extend_from_slice(
            b"Content-Disposition: form-data; name=\"junk\"; filename=\"b.bin\"\r\n",
        );
        out.extend_from_slice(b"Content-Type: application/octet-stream\r\n\r\n");
        out.extend_from_slice(b"DANGLING-NO-CLOSING-BOUNDARY");
        out
    }

    #[allow(clippy::too_many_arguments)]
    async fn post_with(
        reads: FakeReads,
        writes: FakeWrites,
        blobs: FakeBlobs,
        sink: FakeSink,
        path_id: &str,
        with_principal: bool,
        content_type: &str,
        body: Vec<u8>,
    ) -> Reply {
        let read_calls = reads.calls.clone();
        let write_calls = writes.calls.clone();
        let announce_calls = sink.calls.clone();
        let uri = format!("/api/agents/{path_id}/avatar");
        let mut request = Request::post(&uri).header("content-type", content_type);
        if with_principal {
            request = request.extension(user("viewer-1"));
        }
        // The served app's request perimeter disables axum's default 2 MiB body limit
        // (`request_perimeter.rs` → `DefaultBodyLimit::disable()`), so the handler's own
        // bounded read (`accumulate_capped`) + the domain validator produce the precise
        // `413`. Reproduce that seam here; without it axum would pre-empt an over-cap body
        // with a generic rejection before the leaf ever sees it.
        let response = router(reads, writes, blobs, sink, FixedClock(NOW))
            .layer(DefaultBodyLimit::disable())
            .oneshot(request.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        Reply {
            status,
            body: bytes.to_vec(),
            reads: read_calls.load(SeqCst),
            writes: write_calls.load(SeqCst),
            announces: announce_calls.load(SeqCst),
        }
    }

    /// The ordinary POST driver: a valid `image/png` `file` part.
    async fn post_upload(
        reads: FakeReads,
        writes: FakeWrites,
        blobs: FakeBlobs,
        sink: FakeSink,
    ) -> Reply {
        post_with(
            reads,
            writes,
            blobs,
            sink,
            "a1",
            true,
            &format!("multipart/form-data; boundary={BOUNDARY}"),
            multipart_file(Some("a.png"), Some("image/png"), &[1, 2, 3]),
        )
        .await
    }

    async fn delete_with(
        reads: FakeReads,
        writes: FakeWrites,
        blobs: FakeBlobs,
        sink: FakeSink,
        path_id: &str,
        with_principal: bool,
    ) -> Reply {
        let read_calls = reads.calls.clone();
        let write_calls = writes.calls.clone();
        let announce_calls = sink.calls.clone();
        let uri = format!("/api/agents/{path_id}/avatar");
        let mut request = Request::delete(&uri);
        if with_principal {
            request = request.extension(user("viewer-1"));
        }
        let response = router(reads, writes, blobs, sink, FixedClock(NOW))
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        Reply {
            status,
            body: bytes.to_vec(),
            reads: read_calls.load(SeqCst),
            writes: write_calls.load(SeqCst),
            announces: announce_calls.load(SeqCst),
        }
    }

    // ================= POST happy path =================

    #[tokio::test]
    async fn a_first_upload_commits_and_returns_avatar_url_plus_the_reprojected_dto() {
        let reply = post_upload(
            FakeReads::new(
                present(creator_facts()),
                present(projected_facts(Some("agents/a1/avatars/blob-0.png"))),
            ),
            FakeWrites::new(WriteFixture::Row(AgentProfileRowWrite::Committed {
                before: before(AgentAvatarRow::Absent),
                row: committed(present_avatar(FRESH)),
            })),
            FakeBlobs::new(),
            FakeSink::new(false),
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        let body = parsed(&reply);
        // The top-level avatar_url mirrors agent.avatar_url, cache-busted on updated_at.
        let expected_url = format!("/api/agents/a1/avatar?v={NOW}");
        assert_eq!(body["avatar_url"], expected_url);
        assert_eq!(body["agent"]["avatar_url"], expected_url);
        assert_eq!(body["agent"]["id"], "a1");
        // One write, one announce, two reads (gate + post-commit projection).
        assert_eq!(reply.writes, 1);
        assert_eq!(reply.announces, 1);
        assert_eq!(reply.reads, 2);
    }

    #[tokio::test]
    async fn a_failed_announce_does_not_flip_the_committed_upload() {
        let reply = post_upload(
            FakeReads::new(
                present(creator_facts()),
                present(projected_facts(Some("agents/a1/avatars/blob-0.png"))),
            ),
            FakeWrites::new(WriteFixture::Row(AgentProfileRowWrite::Committed {
                before: before(AgentAvatarRow::Absent),
                row: committed(present_avatar(FRESH)),
            })),
            FakeBlobs::new(),
            FakeSink::new(true),
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.announces, 1);
    }

    // ================= POST ladder =================

    #[tokio::test]
    async fn a_missing_principal_is_a_safe_500_that_never_reads_or_writes() {
        let reply = post_with(
            FakeReads::new(present(creator_facts()), present(creator_facts())),
            FakeWrites::new(WriteFixture::Row(AgentProfileRowWrite::NoSuchAgent)),
            FakeBlobs::new(),
            FakeSink::new(false),
            "a1",
            false,
            &format!("multipart/form-data; boundary={BOUNDARY}"),
            multipart_file(Some("a.png"), Some("image/png"), &[1, 2, 3]),
        )
        .await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply.body, b"Internal Server Error");
        assert_eq!(reply.reads, 0);
        assert_eq!(reply.writes, 0);
    }

    #[tokio::test]
    async fn an_absent_row_is_the_anti_enumeration_404_and_never_writes() {
        let reply = post_upload(
            FakeReads::new(ReadOutcome::Absent, ReadOutcome::Absent),
            FakeWrites::new(WriteFixture::Row(AgentProfileRowWrite::NoSuchAgent)),
            FakeBlobs::new(),
            FakeSink::new(false),
        )
        .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        assert_eq!(parsed(&reply), json!({ "error": "agent not found" }));
        assert_eq!(reply.writes, 0);
    }

    #[tokio::test]
    async fn a_pre_gate_read_failure_is_a_safe_500_that_never_writes() {
        let reply = post_upload(
            FakeReads::new(ReadOutcome::Fails, ReadOutcome::Fails),
            FakeWrites::new(WriteFixture::Row(AgentProfileRowWrite::NoSuchAgent)),
            FakeBlobs::new(),
            FakeSink::new(false),
        )
        .await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply.body, b"Internal Server Error");
        assert_eq!(reply.writes, 0);
    }

    #[tokio::test]
    async fn a_non_creator_is_forbidden_before_the_body_is_touched() {
        // Discriminating input: a body that the transport layer would REJECT if it ran
        // first (a non-multipart `application/json` body → `multipart_required` 400). The
        // caller can see the agent but did not create it, so the authority gate must refuse
        // FIRST with 403 — never 400. If the multipart parse ever migrates ahead of the
        // gate, this flips to 400 and the regression is caught. No blob put, no write.
        let blobs = FakeBlobs::new();
        let reply = post_with(
            FakeReads::new(present(workspace_only_facts()), present(creator_facts())),
            FakeWrites::new(WriteFixture::Row(AgentProfileRowWrite::NoSuchAgent)),
            blobs.clone(),
            FakeSink::new(false),
            "a1",
            true,
            "application/json",
            b"{}".to_vec(),
        )
        .await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN);
        assert_eq!(parsed(&reply), json!({ "error": "forbidden" }));
        assert_eq!(reply.writes, 0);
        assert_eq!(blobs.puts(), 0, "a refused caller never puts a blob");
    }

    #[tokio::test]
    async fn a_bundle_resolved_for_another_agent_is_a_safe_500() {
        let spliced = AgentDetailFacts::from_source(
            agent("a2"),
            user("viewer-1"),
            [],
            [],
            MaterializationStatus::Active,
            projection(Some("agents/a2/avatars/blob-0.png")),
        );
        let reply = post_upload(
            FakeReads::new(present(spliced.clone()), present(spliced)),
            FakeWrites::new(WriteFixture::Row(AgentProfileRowWrite::NoSuchAgent)),
            FakeBlobs::new(),
            FakeSink::new(false),
        )
        .await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply.writes, 0);
    }

    // ================= POST wire admission =================

    #[tokio::test]
    async fn a_non_multipart_body_is_multipart_required() {
        let reply = post_with(
            FakeReads::new(present(creator_facts()), present(creator_facts())),
            FakeWrites::new(WriteFixture::Row(AgentProfileRowWrite::NoSuchAgent)),
            FakeBlobs::new(),
            FakeSink::new(false),
            "a1",
            true,
            "application/json",
            b"{}".to_vec(),
        )
        .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert_eq!(
            parsed(&reply),
            json!({ "error": "multipart/form-data required" })
        );
        assert_eq!(reply.writes, 0);
    }

    #[tokio::test]
    async fn a_file_part_without_a_filename_is_file_field_required() {
        let reply = post_with(
            FakeReads::new(present(creator_facts()), present(creator_facts())),
            FakeWrites::new(WriteFixture::Row(AgentProfileRowWrite::NoSuchAgent)),
            FakeBlobs::new(),
            FakeSink::new(false),
            "a1",
            true,
            &format!("multipart/form-data; boundary={BOUNDARY}"),
            multipart_file(None, Some("image/png"), &[1, 2, 3]),
        )
        .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert_eq!(parsed(&reply), json!({ "error": "file field required" }));
        assert_eq!(reply.writes, 0);
    }

    #[tokio::test]
    async fn an_empty_payload_is_the_avatar_missing_400() {
        let reply = post_with(
            FakeReads::new(present(creator_facts()), present(creator_facts())),
            FakeWrites::new(WriteFixture::Row(AgentProfileRowWrite::NoSuchAgent)),
            FakeBlobs::new(),
            FakeSink::new(false),
            "a1",
            true,
            &format!("multipart/form-data; boundary={BOUNDARY}"),
            multipart_file(Some("a.png"), Some("image/png"), &[]),
        )
        .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert_eq!(
            parsed(&reply),
            json!({ "error": "Avatar file is empty.", "code": "avatar_missing" })
        );
        assert_eq!(reply.writes, 0);
    }

    #[tokio::test]
    async fn an_unsupported_type_is_the_415() {
        let reply = post_with(
            FakeReads::new(present(creator_facts()), present(creator_facts())),
            FakeWrites::new(WriteFixture::Row(AgentProfileRowWrite::NoSuchAgent)),
            FakeBlobs::new(),
            FakeSink::new(false),
            "a1",
            true,
            &format!("multipart/form-data; boundary={BOUNDARY}"),
            multipart_file(Some("a.txt"), Some("text/plain"), &[1, 2, 3]),
        )
        .await;
        assert_eq!(reply.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(
            parsed(&reply),
            json!({
                "error": "Avatar must be a PNG, JPEG, GIF, or WEBP image.",
                "code": "avatar_unsupported_type"
            })
        );
    }

    #[tokio::test]
    async fn a_payload_one_byte_over_the_cap_is_the_413() {
        let over = vec![0u8; 5 * 1024 * 1024 + 1];
        let reply = post_with(
            FakeReads::new(present(creator_facts()), present(creator_facts())),
            FakeWrites::new(WriteFixture::Row(AgentProfileRowWrite::NoSuchAgent)),
            FakeBlobs::new(),
            FakeSink::new(false),
            "a1",
            true,
            &format!("multipart/form-data; boundary={BOUNDARY}"),
            multipart_file(Some("a.png"), Some("image/png"), &over),
        )
        .await;
        assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(
            parsed(&reply),
            json!({ "error": "Avatar must be 5 MB or smaller.", "code": "avatar_too_large" })
        );
        assert_eq!(reply.writes, 0);
    }

    #[tokio::test]
    async fn a_cased_media_type_still_reaches_the_domain_allowlist() {
        // A cased `IMAGE/PNG` must pass the allowlist and reach the put, exactly as
        // `image/png` does: the transport applies NO normalizer, and the sole authority
        // `AvatarContentType::parse` lowercases before its exact match.
        let blobs = FakeBlobs::new();
        let reply = post_with(
            FakeReads::new(
                present(creator_facts()),
                present(projected_facts(Some("agents/a1/avatars/blob-0.png"))),
            ),
            FakeWrites::new(WriteFixture::Row(AgentProfileRowWrite::Committed {
                before: before(AgentAvatarRow::Absent),
                row: committed(present_avatar(FRESH)),
            })),
            blobs.clone(),
            FakeSink::new(false),
            "a1",
            true,
            &format!("multipart/form-data; boundary={BOUNDARY}"),
            multipart_file(Some("a.png"), Some("IMAGE/PNG"), &[1, 2, 3]),
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(
            blobs.puts(),
            1,
            "a cased-but-valid type still reaches the put"
        );
    }

    #[tokio::test]
    async fn a_parameterized_media_type_is_still_the_415() {
        // Parameters are never stripped: `IMAGE/PNG; CHARSET=X` reaches the domain validator,
        // which lowercases to `image/png; charset=x` — NOT the bare `image/png` its allowlist
        // admits — so it is still the unsupported-type 415. The transport passes the declared
        // type through unfolded; the sole authority judges it.
        let reply = post_with(
            FakeReads::new(present(creator_facts()), present(creator_facts())),
            FakeWrites::new(WriteFixture::Row(AgentProfileRowWrite::NoSuchAgent)),
            FakeBlobs::new(),
            FakeSink::new(false),
            "a1",
            true,
            &format!("multipart/form-data; boundary={BOUNDARY}"),
            multipart_file(Some("a.png"), Some("IMAGE/PNG; CHARSET=X"), &[1, 2, 3]),
        )
        .await;
        assert_eq!(reply.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(
            parsed(&reply),
            json!({
                "error": "Avatar must be a PNG, JPEG, GIF, or WEBP image.",
                "code": "avatar_unsupported_type"
            })
        );
        assert_eq!(reply.writes, 0);
    }

    #[tokio::test]
    async fn a_malformed_trailing_part_refuses_at_transport_even_after_a_valid_file() {
        // `formData()` decodes the WHOLE envelope before `form.get('file')`, so a valid first
        // `file` part followed by a truncated/malformed trailing part is the transport 400 —
        // the domain never runs, no blob is put, no row is written. This kills a regression
        // that returns on the first file without draining the rest.
        let blobs = FakeBlobs::new();
        let reply = post_with(
            FakeReads::new(present(creator_facts()), present(creator_facts())),
            FakeWrites::new(WriteFixture::Row(AgentProfileRowWrite::NoSuchAgent)),
            blobs.clone(),
            FakeSink::new(false),
            "a1",
            true,
            &format!("multipart/form-data; boundary={BOUNDARY}"),
            multipart_valid_file_then_truncated_part(),
        )
        .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert_eq!(
            parsed(&reply),
            json!({ "error": "multipart/form-data required" })
        );
        assert_eq!(blobs.puts(), 0, "a transport refusal never puts a blob");
        assert_eq!(reply.writes, 0);
    }

    #[tokio::test]
    async fn a_storage_put_failure_is_the_avatar_storage_failed_400_and_never_writes() {
        let reply = post_upload(
            FakeReads::new(present(creator_facts()), present(creator_facts())),
            FakeWrites::new(WriteFixture::Row(AgentProfileRowWrite::NoSuchAgent)),
            FakeBlobs::new().failing_put(),
            FakeSink::new(false),
        )
        .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert_eq!(
            parsed(&reply),
            json!({ "error": "avatar storage failed", "code": "avatar_storage_failed" })
        );
        assert_eq!(reply.writes, 0, "a failed put never reaches the row writer");
    }

    #[tokio::test]
    async fn a_row_vanished_after_put_cleans_the_fresh_orphan_and_is_a_404() {
        let blobs = FakeBlobs::new();
        let reply = post_upload(
            FakeReads::new(present(creator_facts()), present(creator_facts())),
            FakeWrites::new(WriteFixture::Row(AgentProfileRowWrite::NoSuchAgent)),
            blobs.clone(),
            FakeSink::new(false),
        )
        .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        assert_eq!(parsed(&reply), json!({ "error": "agent not found" }));
        assert_eq!(
            blobs.deleted(),
            vec![FRESH.to_owned()],
            "the fresh orphan is cleaned"
        );
        assert_eq!(reply.announces, 0);
    }

    #[tokio::test]
    async fn a_write_persistence_fault_after_put_is_a_safe_500_that_cleans_the_orphan() {
        let blobs = FakeBlobs::new();
        let reply = post_upload(
            FakeReads::new(present(creator_facts()), present(creator_facts())),
            FakeWrites::new(WriteFixture::Fault),
            blobs.clone(),
            FakeSink::new(false),
        )
        .await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply.body, b"Internal Server Error");
        assert_eq!(blobs.deleted(), vec![FRESH.to_owned()]);
        assert_eq!(reply.announces, 0);
    }

    #[tokio::test]
    async fn a_durable_commit_with_an_untrustworthy_witness_retains_the_fresh_blob_and_is_a_500() {
        // A committed row bound to another agent: the UPDATE persisted (may reference the
        // fresh blob), so the fresh blob is RETAINED (never deleted) and no announce fires.
        let mut other = before(AgentAvatarRow::Absent);
        other.agent_id = agent("agent-other");
        let blobs = FakeBlobs::new();
        let reply = post_upload(
            FakeReads::new(present(creator_facts()), present(creator_facts())),
            FakeWrites::new(WriteFixture::Row(AgentProfileRowWrite::Committed {
                before: other,
                row: committed(present_avatar(FRESH)),
            })),
            blobs.clone(),
            FakeSink::new(false),
        )
        .await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply.body, b"Internal Server Error");
        assert!(
            blobs.deleted().is_empty(),
            "a durable commit's fresh blob must never be deleted, even on a bad witness"
        );
        assert_eq!(reply.announces, 0);
    }

    #[tokio::test]
    async fn a_delete_that_races_the_upload_projection_is_a_safe_500() {
        // The write commits and announces, but the row is gone before the DTO can be
        // re-read: durable write, no trustworthy DTO → safe 500.
        let reply = post_upload(
            FakeReads::new(present(creator_facts()), ReadOutcome::Absent),
            FakeWrites::new(WriteFixture::Row(AgentProfileRowWrite::Committed {
                before: before(AgentAvatarRow::Absent),
                row: committed(present_avatar(FRESH)),
            })),
            FakeBlobs::new(),
            FakeSink::new(false),
        )
        .await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply.writes, 1);
        assert_eq!(
            reply.announces, 1,
            "it DID write and announce; only the projection failed"
        );
    }

    // ================= DELETE =================

    #[tokio::test]
    async fn a_clear_of_a_present_avatar_removes_the_blob_announces_and_returns_the_dto() {
        let blobs = FakeBlobs::new();
        let reply = delete_with(
            FakeReads::new(present(creator_facts()), present(projected_facts(None))),
            FakeWrites::new(WriteFixture::Row(AgentProfileRowWrite::Committed {
                before: before(present_avatar("agents/prior/blob-9")),
                row: committed(AgentAvatarRow::Absent),
            })),
            blobs.clone(),
            FakeSink::new(false),
            "a1",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        let body = parsed(&reply);
        assert_eq!(body["agent"]["id"], "a1");
        assert_eq!(body["agent"].get("avatar_url"), Some(&Value::Null));
        // The clear response has no top-level avatar_url mirror.
        assert!(body.get("avatar_url").is_none());
        assert_eq!(blobs.deleted(), vec!["agents/prior/blob-9".to_owned()]);
        assert_eq!(reply.writes, 1);
        assert_eq!(reply.announces, 1);
    }

    #[tokio::test]
    async fn a_clear_of_an_already_absent_avatar_is_a_no_op_that_still_returns_the_dto() {
        let blobs = FakeBlobs::new();
        let reply = delete_with(
            FakeReads::new(present(creator_facts()), present(projected_facts(None))),
            FakeWrites::new(WriteFixture::Row(AgentProfileRowWrite::Unchanged {
                before: before(AgentAvatarRow::Absent),
            })),
            blobs.clone(),
            FakeSink::new(false),
            "a1",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(parsed(&reply)["agent"]["id"], "a1");
        assert!(blobs.deleted().is_empty(), "a no-op deletes no blob");
        assert_eq!(reply.announces, 0, "a no-op owes no announce");
    }

    #[tokio::test]
    async fn a_clear_of_a_missing_row_is_a_404() {
        let reply = delete_with(
            FakeReads::new(present(creator_facts()), present(creator_facts())),
            FakeWrites::new(WriteFixture::Row(AgentProfileRowWrite::NoSuchAgent)),
            FakeBlobs::new(),
            FakeSink::new(false),
            "a1",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        assert_eq!(parsed(&reply), json!({ "error": "agent not found" }));
    }

    #[tokio::test]
    async fn a_clear_write_fault_is_a_safe_500() {
        let reply = delete_with(
            FakeReads::new(present(creator_facts()), present(creator_facts())),
            FakeWrites::new(WriteFixture::Fault),
            FakeBlobs::new(),
            FakeSink::new(false),
            "a1",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply.body, b"Internal Server Error");
    }

    #[tokio::test]
    async fn a_clear_by_a_non_creator_is_forbidden_and_never_writes() {
        let reply = delete_with(
            FakeReads::new(present(workspace_only_facts()), present(creator_facts())),
            FakeWrites::new(WriteFixture::Row(AgentProfileRowWrite::NoSuchAgent)),
            FakeBlobs::new(),
            FakeSink::new(false),
            "a1",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN);
        assert_eq!(parsed(&reply), json!({ "error": "forbidden" }));
        assert_eq!(reply.writes, 0);
    }

    #[tokio::test]
    async fn a_clear_without_a_principal_is_a_safe_500_that_never_reads() {
        let reply = delete_with(
            FakeReads::new(present(creator_facts()), present(creator_facts())),
            FakeWrites::new(WriteFixture::Row(AgentProfileRowWrite::NoSuchAgent)),
            FakeBlobs::new(),
            FakeSink::new(false),
            "a1",
            false,
        )
        .await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply.reads, 0);
        assert_eq!(reply.writes, 0);
    }
}
