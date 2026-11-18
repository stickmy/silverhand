//! The internal agent-profile WRITE routes — `POST /internal/agents/{aid}/profile`
//! and the dedicated avatar pair `POST`/`DELETE /internal/agents/{aid}/avatar`.
//!
//! ## Self-pin, admission, one write, reproject
//!
//! Every route sits behind the same self-pin credential layer as the read
//! ([`require_runtime_credential`](crate::host_runtime::runtime_credential::require_runtime_credential)):
//! `{aid}` is bound to the caller's own agent before the handler runs, so a non-self
//! caller is an indistinguishable `401` upstream and the handler takes NO path param —
//! the subject is `credential.agent()`, never a wire value. An absent credential is the
//! composition-fault safe `500`, never a `401` (that verdict is the layer's).
//!
//! `POST /profile`'s body is decoded by the request-admission checkpoint
//! ([`admit_profile_edit`](super::self_profile_write_admission::admit_profile_edit)) into a
//! fully-validated edit or a terminal refusal. The validated edit is mapped to the core's
//! [`InternalProfileEdit`] and applied by [`update_internal_profile`] as ONE row
//! unit-of-work firing EXACTLY ONE `agent_changed` announce; on success the response is
//! reprojected by RE-READING through the canonical profile read port (never from the write
//! fact), so it emits the single `AgentProfileDto` the CLI consumes. Per the ratified
//! single-UoW divergence, an avatar set/clear supplied via multipart is folded into that
//! SAME row write and still answers with `{ profile }`.
//!
//! ## The dedicated avatar pair echoes the FULL agent
//!
//! `POST /avatar` (a multipart file admitted by
//! [`admit_avatar_upload`](super::self_profile_write_admission::admit_avatar_upload)) and
//! `DELETE /avatar` are avatar-ONLY edits (`display_name`/`description` are left
//! untouched), applied through the SAME [`update_internal_profile`] row UoW. Their product
//! success body is the FULL `AgentDto` `agentService` echoes — NOT the narrow profile DTO —
//! so they reproject through #2225's single crate-root ACL-free projector face
//! ([`reproject_committed_agent`](crate::identity_access::agent_wire::reproject_committed_agent)), never a
//! forked second projector. `POST` answers `{ avatar_url, agent }` (the top-level
//! `avatar_url` echoes the DTO's own derived value); `DELETE` answers `{ agent }`. A
//! post-commit reproject that finds the row gone is a safe `500` (the face's `Vanished`),
//! never a `404` — only a PRE-write missing agent is `404`.

use axum::{
    Json, Router,
    extract::{Extension, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use serde::Serialize;
use silverhand_backend_core::{
    foundation::identity::AgentId,
    host_runtime::runtime_credential::RuntimePrincipal,
    identity_access::agent_avatar::{AgentAvatarBlobPointer, AgentAvatarBlobStore},
    identity_access::agent_detail_read::AgentDetailReadModel,
    identity_access::agent_profile_read::{
        AgentProfileReadModel, ProfileReadFault, read_self_profile,
    },
    identity_access::agent_profile_write::{
        AgentChangedAnnounce, AgentChangedSink, AgentProfileWriteError,
        AgentProfileWriteUnitOfWork, RequiredEdit,
    },
    identity_access::avatar::BlobCleanup,
    identity_access::internal_profile_write::{
        InternalAvatarEdit, InternalProfileEdit, InternalProfileWriteOutcome,
        update_internal_profile,
    },
};

use super::{
    agent_profile_wire::{AgentProfileDto, agent_profile_dto},
    self_profile::SELF_PROFILE_PATH,
    self_profile_write_admission::{
        AvatarAction, ValidatedProfileEdit, admit_avatar_upload, admit_profile_edit,
    },
};
use crate::{
    foundation::clock::ProcessClock,
    foundation::error,
    identity_access::agent_wire::{AgentDto, reproject_committed_agent},
};

/// The dedicated avatar route path. The `{aid}` segment is what the self-pin layer binds;
/// the handlers below take no path param and trust `credential.agent()`.
pub const SELF_AVATAR_PATH: &str = "/internal/agents/{aid}/avatar";

/// The unmapped-error body: a bare `text/plain` `500`, never a JSON envelope — the same
/// shape a thrown Hono handler yields.
const INTERNAL_ERROR_BODY: &str = "Internal Server Error";

/// The ports the write route drives: the canonical row writer, the avatar blob store (an
/// avatar set/clear rides the same profile write), the `agent_changed` sink, the profile
/// read port (for the post-commit reproject), and the process clock (the write instant the
/// writer stamps and the seal verifies). Cloneable so Axum can hold it in router state.
#[derive(Clone)]
pub struct InternalProfileWriteState<W, B, S, R, C> {
    pub writes: W,
    pub blobs: B,
    pub announce: S,
    pub reader: R,
    pub clock: C,
}

/// Mount the profile write route generic over its ports, so tests drive it with
/// driver-free fakes and the composition root wires the SQLite/blob/announce adapters AND
/// the runtime-credential layer that self-pins `{aid}`. `GET /profile` is the read lane's
/// (`self_profile_router`); Axum merges the two method handlers on the shared path.
pub fn internal_profile_write_router<W, B, S, R, C>(
    state: InternalProfileWriteState<W, B, S, R, C>,
) -> Router
where
    W: AgentProfileWriteUnitOfWork + Clone + Send + Sync + 'static,
    B: AgentAvatarBlobStore + Clone + Send + Sync + 'static,
    S: AgentChangedSink + Clone + Send + Sync + 'static,
    R: AgentProfileReadModel + Clone + Send + Sync + 'static,
    C: ProcessClock,
{
    Router::new()
        .route(SELF_PROFILE_PATH, post(post_profile::<W, B, S, R, C>))
        .with_state(state)
}

#[derive(Serialize)]
struct ProfileResponse {
    profile: AgentProfileDto,
}

async fn post_profile<W, B, S, R, C>(
    credential: Option<Extension<RuntimePrincipal>>,
    State(state): State<InternalProfileWriteState<W, B, S, R, C>>,
    request: Request,
) -> Response
where
    W: AgentProfileWriteUnitOfWork,
    B: AgentAvatarBlobStore,
    S: AgentChangedSink,
    R: AgentProfileReadModel,
    C: ProcessClock,
{
    let Some(subject) = pinned_subject(credential) else {
        return composition_fault("self-profile write");
    };
    let validated = match admit_profile_edit(request).await {
        Ok(validated) => validated,
        Err(refusal) => return refusal.into_response(),
    };
    let edit = profile_edit_to_core(validated);
    run_and_reproject(&state, &subject, edit).await
}

/// The ports the dedicated avatar routes drive. Identical to the profile-write ports
/// EXCEPT the read side: the avatar faces echo the FULL `AgentDto`, so they reproject
/// through the ACL-free full-agent read model ([`AgentDetailReadModel`]) and #2225's
/// shared projector, not the narrow profile read. Cloneable for Axum router state.
#[derive(Clone)]
pub struct InternalAvatarWriteState<W, B, S, DR, C> {
    pub writes: W,
    pub blobs: B,
    pub announce: S,
    pub reader: DR,
    pub clock: C,
}

/// Mount the dedicated avatar pair (`POST` set, `DELETE` clear) on the shared path,
/// generic over its ports. The self-pin credential layer is composed in front by the
/// composition root, exactly as for the profile write and read routes.
pub fn internal_avatar_write_router<W, B, S, DR, C>(
    state: InternalAvatarWriteState<W, B, S, DR, C>,
) -> Router
where
    W: AgentProfileWriteUnitOfWork + Clone + Send + Sync + 'static,
    B: AgentAvatarBlobStore + Clone + Send + Sync + 'static,
    S: AgentChangedSink + Clone + Send + Sync + 'static,
    DR: AgentDetailReadModel + Clone + Send + Sync + 'static,
    C: ProcessClock,
{
    Router::new()
        .route(
            SELF_AVATAR_PATH,
            post(post_avatar::<W, B, S, DR, C>).delete(delete_avatar::<W, B, S, DR, C>),
        )
        .with_state(state)
}

/// `POST /avatar`: `{ avatar_url, agent }`, the top-level `avatar_url` echoing the DTO's
/// own derived value exactly as the product does.
#[derive(Serialize)]
struct AvatarUploadResponse {
    avatar_url: Option<String>,
    agent: AgentDto,
}

/// `DELETE /avatar`: `{ agent }` — no top-level `avatar_url` (the cleared DTO carries the
/// null itself).
#[derive(Serialize)]
struct AvatarClearResponse {
    agent: AgentDto,
}

/// `POST /avatar` — a multipart file upload that SETS the avatar (only). Admitted by the
/// shared avatar checkpoint, folded into one row write, then reprojected to the full agent.
async fn post_avatar<W, B, S, DR, C>(
    credential: Option<Extension<RuntimePrincipal>>,
    State(state): State<InternalAvatarWriteState<W, B, S, DR, C>>,
    request: Request,
) -> Response
where
    W: AgentProfileWriteUnitOfWork,
    B: AgentAvatarBlobStore,
    S: AgentChangedSink,
    DR: AgentDetailReadModel,
    C: ProcessClock,
{
    let Some(subject) = pinned_subject(credential) else {
        return composition_fault("self-avatar upload");
    };
    let validated = match admit_avatar_upload(request).await {
        Ok(validated) => validated,
        Err(refusal) => return refusal.into_response(),
    };
    let avatar = InternalAvatarEdit::Set {
        content_type: validated.content_type(),
        bytes: validated.bytes().to_vec(),
        design: validated.design().cloned(),
    };
    run_avatar_edit(&state, &subject, avatar, "self-avatar upload", |dto| {
        Json(AvatarUploadResponse {
            avatar_url: dto.avatar_url(),
            agent: dto,
        })
        .into_response()
    })
    .await
}

/// `DELETE /avatar` — CLEAR the avatar (only). No request body; the clear is idempotent
/// (a clear on an already-absent avatar is the warranted no-op, still `200`).
async fn delete_avatar<W, B, S, DR, C>(
    credential: Option<Extension<RuntimePrincipal>>,
    State(state): State<InternalAvatarWriteState<W, B, S, DR, C>>,
) -> Response
where
    W: AgentProfileWriteUnitOfWork,
    B: AgentAvatarBlobStore,
    S: AgentChangedSink,
    DR: AgentDetailReadModel,
    C: ProcessClock,
{
    let Some(subject) = pinned_subject(credential) else {
        return composition_fault("self-avatar clear");
    };
    run_avatar_edit(
        &state,
        &subject,
        InternalAvatarEdit::Clear,
        "self-avatar clear",
        |dto| Json(AvatarClearResponse { agent: dto }).into_response(),
    )
    .await
}

/// Apply an avatar-only edit as one row write and, on a durable outcome, reproject the
/// FULL agent through #2225's shared ACL-free projector, then hand the DTO to `wrap` to
/// build this face's envelope. Every non-success is a terminal `Response` (404 pre-write,
/// `avatar_storage_failed` 400, or safe-500). `display_name`/`description` are left
/// untouched. `wrap` is injected (rather than returning `Result<AgentDto, Response>`) so no
/// oversized `Response` ever rides an `Err` — the two faces differ ONLY in the envelope
/// they build around the shared full DTO.
async fn run_avatar_edit<W, B, S, DR, C, F>(
    state: &InternalAvatarWriteState<W, B, S, DR, C>,
    subject: &AgentId,
    avatar: InternalAvatarEdit,
    operation: &'static str,
    wrap: F,
) -> Response
where
    W: AgentProfileWriteUnitOfWork,
    B: AgentAvatarBlobStore,
    S: AgentChangedSink,
    DR: AgentDetailReadModel,
    C: ProcessClock,
    F: FnOnce(AgentDto) -> Response,
{
    let edit = InternalProfileEdit {
        display_name: RequiredEdit::Leave,
        description: RequiredEdit::Leave,
        avatar,
    };
    let now_ms = state.clock.now_unix_millis();
    let outcome = update_internal_profile(
        &state.writes,
        &state.blobs,
        &state.announce,
        subject,
        edit,
        now_ms,
    )
    .await;

    let committed_id = match outcome {
        InternalProfileWriteOutcome::Committed(effect) => {
            let (agent_id, announce, cleanup) = effect.into_parts();
            note_announce(subject, announce);
            note_cleanup(subject, cleanup);
            agent_id
        }
        // The warranted no-op (clear on an absent avatar): nothing changed, but the product
        // still answers with the current agent.
        InternalProfileWriteOutcome::NoOp { agent_id } => agent_id,
        InternalProfileWriteOutcome::NoSuchAgent { orphan_cleanup } => {
            note_cleanup(subject, orphan_cleanup);
            return not_found();
        }
        InternalProfileWriteOutcome::StorageFailed(failure) => {
            tracing::error!(
                error_chain =
                    crate::foundation::safe_log_field::safe_display(&(error::describe(&failure))),
                "internal-avatar write: avatar blob storage failed"
            );
            return avatar_storage_failed();
        }
        InternalProfileWriteOutcome::WriteFaulted {
            cause,
            orphan_cleanup,
        } => {
            note_cleanup(subject, orphan_cleanup);
            note_write_fault(subject, &cause);
            return internal_error();
        }
        InternalProfileWriteOutcome::CommitUntrustworthy { retained_fresh } => {
            note_untrustworthy(subject, retained_fresh);
            return internal_error();
        }
    };

    // The row is durably written; reproject the full agent through #2225's shared face. A
    // row gone before this read is the face's `Vanished` → safe-500, never a 404 (that
    // would disguise a durable commit as un-written and invite a retry).
    match reproject_committed_agent(&state.reader, &committed_id).await {
        Ok(dto) => wrap(dto),
        Err(fault) => fault.into_safe_500(operation, &committed_id),
    }
}

/// The pinned subject, read from the credential the self-pin layer injected. Absent means
/// the layer was not composed in front of this route.
fn pinned_subject(credential: Option<Extension<RuntimePrincipal>>) -> Option<AgentId> {
    credential.map(|Extension(principal)| principal.agent().clone())
}

/// Apply the edit as one row write and, on success, reproject by re-reading the profile.
async fn run_and_reproject<W, B, S, R, C>(
    state: &InternalProfileWriteState<W, B, S, R, C>,
    subject: &AgentId,
    edit: InternalProfileEdit,
) -> Response
where
    W: AgentProfileWriteUnitOfWork,
    B: AgentAvatarBlobStore,
    S: AgentChangedSink,
    R: AgentProfileReadModel,
    C: ProcessClock,
{
    let now_ms = state.clock.now_unix_millis();
    let outcome = update_internal_profile(
        &state.writes,
        &state.blobs,
        &state.announce,
        subject,
        edit,
        now_ms,
    )
    .await;

    match outcome {
        InternalProfileWriteOutcome::Committed(effect) => {
            // The announce and blob-cleanup are best-effort residues: a failure is logged
            // but never downgrades the committed write (the row is authoritative and
            // clients coalesce the refresh). Reproject from the read port, not the fact.
            let (agent_id, announce, cleanup) = effect.into_parts();
            note_announce(subject, announce);
            note_cleanup(subject, cleanup);
            reproject(&state.reader, &agent_id).await
        }
        // The one warranted no-op (avatar clear on an absent avatar, no text): nothing
        // changed, but the product still answers `200` with the current profile.
        InternalProfileWriteOutcome::NoOp { agent_id } => reproject(&state.reader, &agent_id).await,
        // The row vanished before the write — the product `404`. Any fresh blob was
        // already cleaned by the core; log its fate.
        InternalProfileWriteOutcome::NoSuchAgent { orphan_cleanup } => {
            note_cleanup(subject, orphan_cleanup);
            not_found()
        }
        // The avatar blob store faulted with nothing written and nothing persisted. The
        // product maps this to a stable detail-free `400` (TS `agentServiceErrorStatusMap`:
        // `avatar_storage_failed` → 400), NOT a bare `500`. Zero write, zero announce.
        InternalProfileWriteOutcome::StorageFailed(failure) => {
            tracing::error!(
                error_chain =
                    crate::foundation::safe_log_field::safe_display(&(error::describe(&failure))),
                "internal-profile write: avatar blob storage failed"
            );
            avatar_storage_failed()
        }
        // A pre-commit fault, or a durable-but-untrustworthy commit: both are the lane's
        // safe `500`. The core already carried the orphan/retained-blob discipline.
        InternalProfileWriteOutcome::WriteFaulted {
            cause,
            orphan_cleanup,
        } => {
            note_cleanup(subject, orphan_cleanup);
            note_write_fault(subject, &cause);
            internal_error()
        }
        InternalProfileWriteOutcome::CommitUntrustworthy { retained_fresh } => {
            note_untrustworthy(subject, retained_fresh);
            internal_error()
        }
    }
}

/// Re-read the just-written profile and project it into `{ profile }`. This runs ONLY
/// after a durable `Committed`/`NoOp` outcome, so the row is known to exist: an `Ok(None)`
/// here means it vanished between the write and the read, which is NOT a product `404`
/// (that would disguise a durable commit as un-written and invite a retry) — it is a safe
/// `500`, exactly like a persistence or source-binding fault. The only legitimate `404` is
/// the PRE-write `NoSuchAgent`, handled by the caller.
async fn reproject<R>(reader: &R, subject: &AgentId) -> Response
where
    R: AgentProfileReadModel,
{
    match read_self_profile(reader, subject).await {
        Ok(Some(fact)) => Json(ProfileResponse {
            profile: agent_profile_dto(&fact),
        })
        .into_response(),
        // A durable commit whose row is now unreadable: safe-500, never a 404.
        Ok(None) => {
            tracing::error!(
                agent = subject.as_str(),
                "internal-profile write committed but the row vanished before the reproject read"
            );
            internal_error()
        }
        Err(fault) => {
            let cause: &dyn std::error::Error = match &fault {
                ProfileReadFault::Persistence(failure) => failure,
                ProfileReadFault::Source(violation) => violation,
            };
            tracing::error!(
                error_chain =
                    crate::foundation::safe_log_field::safe_display(&(error::describe(cause))),
                "internal-profile write: post-commit reproject failed"
            );
            internal_error()
        }
    }
}

fn profile_edit_to_core(validated: ValidatedProfileEdit) -> InternalProfileEdit {
    let display_name = match validated.display_name() {
        Some(display_name) => RequiredEdit::Set(display_name.as_str().to_owned()),
        None => RequiredEdit::Leave,
    };
    let description = match validated.description() {
        Some(description) => RequiredEdit::Set(description.as_str().to_owned()),
        None => RequiredEdit::Leave,
    };
    let avatar = match validated.avatar() {
        AvatarAction::Leave => InternalAvatarEdit::Leave,
        AvatarAction::Clear => InternalAvatarEdit::Clear,
        AvatarAction::Set(validated) => InternalAvatarEdit::Set {
            content_type: validated.content_type(),
            bytes: validated.bytes().to_vec(),
            design: None,
        },
    };
    InternalProfileEdit {
        display_name,
        description,
        avatar,
    }
}

fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": "agent not found" })),
    )
        .into_response()
}

/// The avatar blob store faulted with nothing written: the product's stable detail-free
/// `400 { error, code }` (TS `agentServiceErrorBody` under `avatar_storage_failed` → 400).
/// The cause is confined to the boundary log; the wire never carries it.
fn avatar_storage_failed() -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({
            "error": "avatar storage failed",
            "code": "avatar_storage_failed"
        })),
    )
        .into_response()
}

fn internal_error() -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, INTERNAL_ERROR_BODY).into_response()
}

fn composition_fault(route: &str) -> Response {
    tracing::error!("{route} route reached without its runtime credential");
    internal_error()
}

/// Log a failed `agent_changed` announce; a delivered one is silent. Best-effort: never
/// changes the response.
fn note_announce(subject: &AgentId, announce: AgentChangedAnnounce) {
    if let AgentChangedAnnounce::Failed(failure) = announce {
        tracing::warn!(
            agent = subject.as_str(),
            error_chain =
                crate::foundation::safe_log_field::safe_display(&(error::describe(&failure))),
            "internal-profile write committed but the agent_changed announce failed"
        );
    }
}

/// Log a failed best-effort blob cleanup; a clean/nothing-to-clean is silent.
fn note_cleanup(subject: &AgentId, cleanup: BlobCleanup) {
    if let BlobCleanup::Failed(failure) = cleanup {
        tracing::warn!(
            agent = subject.as_str(),
            error_chain =
                crate::foundation::safe_log_field::safe_display(&(error::describe(&failure))),
            "internal-profile write: best-effort avatar blob cleanup failed (possible orphan)"
        );
    }
}

fn note_write_fault(subject: &AgentId, cause: &AgentProfileWriteError) {
    tracing::error!(
        agent = subject.as_str(),
        cause = ?cause,
        "internal-profile write faulted before a trustworthy commit"
    );
}

fn note_untrustworthy(subject: &AgentId, retained_fresh: Option<AgentAvatarBlobPointer>) {
    tracing::error!(
        agent = subject.as_str(),
        retained_fresh = retained_fresh.as_ref().map(AgentAvatarBlobPointer::as_str),
        "internal-profile write committed durably but the witness was untrustworthy; \
         a fresh avatar blob (if any) was retained to avoid a dangling pointer"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::{Request as HttpRequest, header::AUTHORIZATION},
    };
    use serde_json::{Value, json};
    use silverhand_backend_core::{
        conversation_messaging::messaging::AvatarRevision,
        foundation::identity::{DaemonInstanceId, UserId},
        foundation::persistence::PersistenceFailure,
        host_runtime::runtime_credential::{
            RuntimeCustody, RuntimeGeneration, RuntimePrincipal, RuntimeScope,
        },
        identity_access::agent_avatar::{
            AgentAvatarBlobRead, AgentAvatarRow, OwnedAgentAvatarBlob,
        },
        identity_access::agent_detail_read::{AgentDetailFacts, AgentDetailProjection},
        identity_access::agent_lifecycle::MaterializationStatus,
        identity_access::agent_profile_read::AgentProfileFact,
        identity_access::agent_profile_write::{
            AdmittedAgentProfileEdit, AgentChangedAnnounceFailure, AgentProfileRowWrite,
            AgentRowBefore, AgentTuningContext, CommittedAgentRow,
        },
        identity_access::agents::runtime::RuntimeKind,
        identity_access::avatar::{AvatarBlobReadOutcome, AvatarContentType, AvatarStorageFailure},
    };
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    use tower::ServiceExt;

    const NOW: i64 = 1_700_000_000_123;
    const FRESH: &str = "agents/mint/blob-0";

    fn agent() -> AgentId {
        AgentId::parse("a1").unwrap()
    }

    fn ptr(raw: &str) -> AgentAvatarBlobPointer {
        AgentAvatarBlobPointer::parse(raw).unwrap()
    }

    // ---- clock ----

    #[derive(Clone, Copy)]
    struct FixedClock;
    impl ProcessClock for FixedClock {
        fn now_unix_millis(&self) -> i64 {
            NOW
        }
    }

    // ---- blob store ----

    #[derive(Clone)]
    struct FakeBlobs {
        next: Arc<AtomicUsize>,
        deletes: Arc<Mutex<Vec<String>>>,
        put_fails: bool,
    }
    impl FakeBlobs {
        fn new() -> Self {
            Self {
                next: Arc::new(AtomicUsize::new(0)),
                deletes: Arc::new(Mutex::new(Vec::new())),
                put_fails: false,
            }
        }
        /// A store whose `put` always faults — used to prove the `avatar_storage_failed`
        /// → detail-free `400` mapping, with zero write and zero announce.
        fn failing_put() -> Self {
            Self {
                put_fails: true,
                ..Self::new()
            }
        }
        fn deleted(&self) -> Vec<String> {
            self.deletes.lock().unwrap().clone()
        }
        /// How many blobs were `put` — a Set write puts the fresh blob FIRST, so a zero
        /// count proves the write core's blob leg never ran.
        fn puts(&self) -> usize {
            self.next.load(Ordering::SeqCst)
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
            if self.put_fails {
                return Err(AvatarStorageFailure::from_source(std::io::Error::other(
                    "blob put failed",
                )));
            }
            let n = self.next.fetch_add(1, Ordering::SeqCst);
            Ok(ptr(&format!("agents/mint/blob-{n}")))
        }
        async fn delete(&self, blob: &OwnedAgentAvatarBlob) -> Result<(), AvatarStorageFailure> {
            self.deletes
                .lock()
                .unwrap()
                .push(blob.pointer().as_str().to_owned());
            Ok(())
        }
    }

    // ---- writer ----

    #[allow(clippy::large_enum_variant)]
    #[derive(Clone)]
    enum FakeWrites {
        Unexpected,
        Row(AgentProfileRowWrite),
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
            match self {
                Self::Unexpected => panic!("refused request must not call the row writer"),
                Self::Row(row) => Ok(row.clone()),
            }
        }
    }

    // ---- announce sink ----

    #[derive(Clone)]
    struct FakeSink {
        calls: Arc<AtomicUsize>,
    }
    impl FakeSink {
        fn new() -> Self {
            Self {
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }
    impl AgentChangedSink for FakeSink {
        async fn agent_changed(&self, _agent: &AgentId) -> Result<(), AgentChangedAnnounceFailure> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    // ---- reader (reproject source) ----

    #[derive(Clone)]
    struct FakeReader {
        fact: Option<AgentProfileFact>,
        consulted: Arc<AtomicUsize>,
    }
    impl FakeReader {
        fn with(fact: Option<AgentProfileFact>) -> Self {
            Self {
                fact,
                consulted: Arc::new(AtomicUsize::new(0)),
            }
        }
        fn reads(&self) -> usize {
            self.consulted.load(Ordering::SeqCst)
        }
    }
    impl AgentProfileReadModel for FakeReader {
        async fn read_agent_profile(
            &self,
            _agent: &AgentId,
        ) -> Result<Option<AgentProfileFact>, PersistenceFailure> {
            self.consulted.fetch_add(1, Ordering::SeqCst);
            Ok(self.fact.clone())
        }
        async fn read_agent_profile_by_handle(
            &self,
            _handle: &str,
        ) -> Result<Option<AgentProfileFact>, PersistenceFailure> {
            unreachable!("the write route never reads by handle")
        }
    }

    // ---- builders ----

    fn ctx() -> AgentTuningContext {
        AgentTuningContext {
            runtime: RuntimeKind::Claude,
            model: None,
            reasoning_effort: None,
        }
    }

    fn before(display_name: &str, avatar: AgentAvatarRow) -> AgentRowBefore {
        AgentRowBefore {
            agent_id: agent(),
            display_name: display_name.to_owned(),
            description: None,
            service_tier: None,
            avatar,
            context: ctx(),
        }
    }

    fn committed(display_name: &str, avatar: AgentAvatarRow) -> CommittedAgentRow {
        CommittedAgentRow {
            id: agent(),
            display_name: display_name.to_owned(),
            description: None,
            model: None,
            reasoning_effort: None,
            service_tier: None,
            avatar,
            updated_at: NOW,
        }
    }

    fn present(raw: &str) -> AgentAvatarRow {
        AgentAvatarRow::Present {
            pointer: ptr(raw),
            content_type: AvatarContentType::Png,
        }
    }

    fn fact(name: &str, avatar: Option<i64>) -> AgentProfileFact {
        AgentProfileFact {
            id: agent(),
            name: name.to_owned(),
            display_name: name.to_owned(),
            description: Some("a bio".to_owned()),
            avatar: avatar.map(AvatarRevision::at),
        }
    }

    fn principal(id: &str) -> RuntimePrincipal {
        RuntimePrincipal::for_scope(
            RuntimeScope::AgentSlot {
                agent: AgentId::parse(id).unwrap(),
            },
            RuntimeCustody::new(
                RuntimeGeneration::new(1).unwrap(),
                DaemonInstanceId::parse("daemon-1").unwrap(),
            ),
        )
    }

    fn state(
        writes: AgentProfileRowWrite,
        reader: FakeReader,
    ) -> (
        InternalProfileWriteState<FakeWrites, FakeBlobs, FakeSink, FakeReader, FixedClock>,
        FakeBlobs,
        FakeSink,
    ) {
        state_with_blobs(writes, reader, FakeBlobs::new())
    }

    fn state_with_blobs(
        writes: AgentProfileRowWrite,
        reader: FakeReader,
        blobs: FakeBlobs,
    ) -> (
        InternalProfileWriteState<FakeWrites, FakeBlobs, FakeSink, FakeReader, FixedClock>,
        FakeBlobs,
        FakeSink,
    ) {
        let sink = FakeSink::new();
        let state = InternalProfileWriteState {
            writes: FakeWrites::Row(writes),
            blobs: blobs.clone(),
            announce: sink.clone(),
            reader,
            clock: FixedClock,
        };
        (state, blobs, sink)
    }

    /// A multipart body with the given text parts (name, value) and an optional file part
    /// under `file`. Built by hand so the boundary's real `multer` decode runs.
    fn multipart_body(parts: &[(&str, &str)], file: Option<&[u8]>) -> (String, Vec<u8>) {
        const BOUNDARY: &str = "X-TEST-BOUNDARY";
        let mut body: Vec<u8> = Vec::new();
        for (name, value) in parts {
            body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
            body.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
            );
            body.extend_from_slice(value.as_bytes());
            body.extend_from_slice(b"\r\n");
        }
        if let Some(bytes) = file {
            body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
            body.extend_from_slice(
                b"Content-Disposition: form-data; name=\"file\"; filename=\"avatar.png\"\r\n",
            );
            body.extend_from_slice(b"Content-Type: image/png\r\n\r\n");
            body.extend_from_slice(bytes);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
        (format!("multipart/form-data; boundary={BOUNDARY}"), body)
    }

    /// A tiny valid PNG header (enough bytes to pass the size floor; MIME is declared).
    fn png_bytes() -> Vec<u8> {
        vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 1, 2, 3, 4]
    }

    /// A body whose FIRST `file` part is complete and valid, then a second part opened but
    /// truncated — its data never terminates with a closing boundary, so the multipart
    /// parser hits end-of-stream mid-part and faults. Proves the whole-envelope drain: the
    /// valid first file must not shortcut past the malformed tail.
    fn multipart_valid_file_then_truncated_part() -> (String, Vec<u8>) {
        const BOUNDARY: &str = "X-TEST-BOUNDARY";
        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        body.extend_from_slice(
            b"Content-Disposition: form-data; name=\"file\"; filename=\"avatar.png\"\r\n",
        );
        body.extend_from_slice(b"Content-Type: image/png\r\n\r\n");
        body.extend_from_slice(&png_bytes());
        // Open a second part, then cut the stream off inside its data with no closing
        // boundary — a premature end the parser cannot complete.
        body.extend_from_slice(format!("\r\n--{BOUNDARY}\r\n").as_bytes());
        body.extend_from_slice(b"Content-Disposition: form-data; name=\"junk\"\r\n\r\n");
        body.extend_from_slice(b"DANGLING-NO-CLOSING-BOUNDARY");
        (format!("multipart/form-data; boundary={BOUNDARY}"), body)
    }

    async fn body_json(response: Response) -> Value {
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    }

    fn router(
        state: InternalProfileWriteState<FakeWrites, FakeBlobs, FakeSink, FakeReader, FixedClock>,
    ) -> Router {
        internal_profile_write_router(state)
    }

    /// A POST to `/internal/agents/a1/profile` carrying the given multipart body and the
    /// self-pinned credential for `a1`.
    fn profile_post(content_type: String, body: Vec<u8>) -> HttpRequest<Body> {
        HttpRequest::builder()
            .method("POST")
            .uri("/internal/agents/a1/profile")
            .header("content-type", content_type)
            .extension(principal("a1"))
            .body(Body::from(body))
            .unwrap()
    }

    // ================= POST /profile — success projection =================

    #[tokio::test]
    async fn post_profile_combined_text_and_avatar_projects_the_profile_envelope() {
        // before Absent; combined display_name + avatar Set. The committed row names the
        // fresh pointer so the core seal passes; the reader returns the post-write fact.
        let writes = AgentProfileRowWrite::Committed {
            before: before("Old", AgentAvatarRow::Absent),
            row: committed("New", present(FRESH)),
        };
        let reader = FakeReader::with(Some(fact("agent-one", Some(NOW))));
        let (state, blobs, sink) = state(writes, reader.clone());
        let (content_type, body) = multipart_body(
            &[("display_name", "New"), ("avatar_action", "set")],
            Some(&png_bytes()),
        );
        let response = router(state)
            .oneshot(profile_post(content_type, body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(sink.calls(), 1, "a combined edit announces exactly once");
        assert!(
            blobs.deleted().is_empty(),
            "no prior blob to clean on a first set"
        );
        assert!(
            reader.reads() >= 1,
            "the response reprojects from the read port"
        );
        assert_eq!(
            body_json(response).await,
            json!({
                "profile": {
                    "id": "a1",
                    "name": "agent-one",
                    "display_name": "agent-one",
                    "description": "a bio",
                    "avatar_url": format!("/api/agents/a1/avatar?v={NOW}")
                }
            })
        );
    }

    #[tokio::test]
    async fn post_profile_clear_present_avatar_cleans_the_blob_and_projects_profile() {
        // avatar_action=clear with a prior blob present: the row clears, the prior blob is
        // cleaned, and the envelope is `{ profile }` with a null avatar_url.
        let writes = AgentProfileRowWrite::Committed {
            before: before("Name", present("agents/prior/blob-9")),
            row: committed("Name", AgentAvatarRow::Absent),
        };
        let reader = FakeReader::with(Some(fact("agent-one", None)));
        let (state, blobs, sink) = state(writes, reader);
        let (content_type, body) = multipart_body(&[("avatar_action", "clear")], None);
        let response = router(state)
            .oneshot(profile_post(content_type, body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(sink.calls(), 1);
        assert_eq!(blobs.deleted(), vec!["agents/prior/blob-9".to_owned()]);
        assert_eq!(
            body_json(response).await,
            json!({
                "profile": {
                    "id": "a1",
                    "name": "agent-one",
                    "display_name": "agent-one",
                    "description": "a bio",
                    "avatar_url": Value::Null
                }
            })
        );
    }

    #[tokio::test]
    async fn post_profile_clear_absent_avatar_is_a_noop_but_still_200() {
        // avatar_action=clear on an already-absent avatar, no text: the warranted no-op,
        // still `200` with the current profile, no announce, no blob touched.
        let writes = AgentProfileRowWrite::Unchanged {
            before: before("Name", AgentAvatarRow::Absent),
        };
        let reader = FakeReader::with(Some(fact("agent-one", None)));
        let (state, blobs, sink) = state(writes, reader);
        let (content_type, body) = multipart_body(&[("avatar_action", "clear")], None);
        let response = router(state)
            .oneshot(profile_post(content_type, body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(sink.calls(), 0, "a genuine no-op announces nothing");
        assert!(blobs.deleted().is_empty());
        assert_eq!(
            body_json(response).await["profile"].get("avatar_url"),
            Some(&Value::Null)
        );
    }

    // ================= POST /profile — admission =================

    #[tokio::test]
    async fn post_profile_with_nothing_to_update_is_the_admission_400_and_never_writes() {
        // No text and no avatar_action → admission's NothingToUpdate, before any write.
        let writes = AgentProfileRowWrite::NoSuchAgent; // must never be consulted
        let reader = FakeReader::with(None);
        let (mut state, blobs, sink) = state(writes, reader.clone());
        state.writes = FakeWrites::Unexpected;
        let (content_type, body) = multipart_body(&[("ignored", "x")], None);
        let response = router(state)
            .oneshot(profile_post(content_type, body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_json(response).await,
            json!({ "error": "no fields to update" })
        );
        assert_eq!(sink.calls(), 0);
        assert!(blobs.deleted().is_empty());
        assert_eq!(reader.reads(), 0, "a refused request never reprojects");
    }

    // ================= POST /profile — outcome → status =================

    #[tokio::test]
    async fn a_pre_write_missing_agent_is_a_404() {
        // The only legitimate 404: the row does not exist BEFORE the write.
        let writes = AgentProfileRowWrite::NoSuchAgent;
        let reader = FakeReader::with(None);
        let (state, _blobs, sink) = state(writes, reader.clone());
        let (content_type, body) = multipart_body(&[("display_name", "New")], None);
        let response = router(state)
            .oneshot(profile_post(content_type, body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            body_json(response).await,
            json!({ "error": "agent not found" })
        );
        assert_eq!(sink.calls(), 0);
        assert_eq!(reader.reads(), 0, "a pre-write 404 never reprojects");
    }

    #[tokio::test]
    async fn a_blob_put_failure_is_the_avatar_storage_400_and_never_writes() {
        // The blob store faults on put: the stable detail-free 400, zero write/announce,
        // and no reproject. The row writer must never be consulted.
        let writes = AgentProfileRowWrite::NoSuchAgent; // must never be reached
        let reader = FakeReader::with(None);
        let (mut state, _blobs, sink) =
            state_with_blobs(writes, reader.clone(), FakeBlobs::failing_put());
        state.writes = FakeWrites::Unexpected;
        let (content_type, body) = multipart_body(&[("avatar_action", "set")], Some(&png_bytes()));
        let response = router(state)
            .oneshot(profile_post(content_type, body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_json(response).await,
            json!({ "error": "avatar storage failed", "code": "avatar_storage_failed" })
        );
        assert_eq!(sink.calls(), 0, "a storage fault never announces");
        assert_eq!(reader.reads(), 0, "and never reprojects");
    }

    #[tokio::test]
    async fn a_committed_then_vanished_row_is_a_safe_500() {
        // A durable Committed whose row is unreadable at reproject: safe-500, NEVER a 404
        // (a 404 would disguise a durable commit as un-written and invite a retry).
        let writes = AgentProfileRowWrite::Committed {
            before: before("Old", AgentAvatarRow::Absent),
            row: committed("New", AgentAvatarRow::Absent),
        };
        let reader = FakeReader::with(None); // the row vanished after the commit
        let (state, _blobs, sink) = state(writes, reader.clone());
        let (content_type, body) = multipart_body(&[("display_name", "New")], None);
        let response = router(state)
            .oneshot(profile_post(content_type, body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(sink.calls(), 1, "the durable commit still announced once");
        assert_eq!(
            reader.reads(),
            1,
            "the reproject was attempted and found nothing"
        );
    }

    #[tokio::test]
    async fn an_untrustworthy_commit_is_a_safe_500() {
        // A commit whose before-image names another agent: durable but untrustworthy.
        let mut other = before("Old", AgentAvatarRow::Absent);
        other.agent_id = AgentId::parse("someone-else").unwrap();
        let writes = AgentProfileRowWrite::Committed {
            before: other,
            row: committed("New", AgentAvatarRow::Absent),
        };
        let reader = FakeReader::with(Some(fact("agent-one", None)));
        let (state, _blobs, sink) = state(writes, reader.clone());
        let (content_type, body) = multipart_body(&[("display_name", "New")], None);
        let response = router(state)
            .oneshot(profile_post(content_type, body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(sink.calls(), 0, "an untrustworthy commit never announces");
        assert_eq!(reader.reads(), 0, "and never reprojects");
    }

    // ================= self-pin wiring =================

    #[tokio::test]
    async fn an_absent_credential_is_a_safe_500_and_never_writes() {
        let writes = AgentProfileRowWrite::NoSuchAgent;
        let reader = FakeReader::with(None);
        let (mut state, _blobs, sink) = state(writes, reader.clone());
        state.writes = FakeWrites::Unexpected;
        let (content_type, body) = multipart_body(&[("display_name", "New")], None);
        // No credential extension: the self-pin layer was not composed in front.
        let request = HttpRequest::builder()
            .method("POST")
            .uri("/internal/agents/a1/profile")
            .header("content-type", content_type)
            .body(Body::from(body))
            .unwrap();
        let response = router(state).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(sink.calls(), 0);
        assert_eq!(reader.reads(), 0);
    }

    /// A faithful self-pin authority stand-in (the production rule is in
    /// `runtime_credential.rs`): admit ONLY when the route `{aid}` equals the enrolled
    /// agent. Proves the write route sits BEHIND the credential layer.
    #[derive(Clone)]
    struct PinnedAuthority {
        enrolled: AgentId,
    }
    impl crate::host_runtime::runtime_credential::AuthenticateRuntimeCredential for PinnedAuthority {
        async fn authenticate_runtime(
            &self,
            _bearer: crate::host_runtime::runtime_credential::RuntimeBearer,
            route_agent: AgentId,
        ) -> Result<Option<RuntimePrincipal>, PersistenceFailure> {
            if route_agent == self.enrolled {
                Ok(Some(principal(self.enrolled.as_str())))
            } else {
                Ok(None)
            }
        }
    }

    #[tokio::test]
    async fn a_non_self_aid_is_401_before_any_write() {
        let writes = AgentProfileRowWrite::NoSuchAgent;
        let reader = FakeReader::with(None);
        let (mut state, _blobs, sink) = state(writes, reader.clone());
        state.writes = FakeWrites::Unexpected;
        let authority = PinnedAuthority { enrolled: agent() };
        let router = crate::host_runtime::runtime_credential::require_runtime_credential(
            router(state),
            authority,
        );
        let (content_type, body) = multipart_body(&[("display_name", "New")], None);
        let request = HttpRequest::builder()
            .method("POST")
            .uri("/internal/agents/someone-else/profile")
            .header("content-type", content_type)
            .header(AUTHORIZATION, "Bearer test-secret")
            .body(Body::from(body))
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(sink.calls(), 0, "a non-self caller never reaches the write");
        assert_eq!(reader.reads(), 0);
    }

    // ================= dedicated avatar pair — full-agent echo =================

    // ---- full-agent detail reader (the avatar reproject source) ----

    #[derive(Clone)]
    struct FakeDetailReader {
        facts: Option<AgentDetailFacts>,
        reads: Arc<AtomicUsize>,
    }
    impl FakeDetailReader {
        fn with(facts: Option<AgentDetailFacts>) -> Self {
            Self {
                facts,
                reads: Arc::new(AtomicUsize::new(0)),
            }
        }
        fn reads(&self) -> usize {
            self.reads.load(Ordering::SeqCst)
        }
    }
    impl AgentDetailReadModel for FakeDetailReader {
        async fn read_agent_detail(
            &self,
            _agent: &AgentId,
        ) -> Result<Option<AgentDetailFacts>, PersistenceFailure> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            Ok(self.facts.clone())
        }
    }

    /// A full detail projection whose avatar column is `avatar_key`. The derived
    /// `avatar_url` is a cache-busted URL when the key is non-empty, else null.
    fn detail_projection(avatar_key: Option<&str>) -> AgentDetailProjection {
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
            description: Some("a bio".to_owned()),
            session_scope: "agent".to_owned(),
            avatar_storage_key: avatar_key.map(str::to_owned),
            materialization_failed_code: None,
            materialization_failed_at: None,
            deletion_started_at: None,
            created_at: 1000,
            updated_at: NOW,
        }
    }

    /// A detail bundle source-bound to `a1` (the pinned subject) so the reproject's
    /// source-binding passes.
    fn detail_facts(avatar_key: Option<&str>) -> AgentDetailFacts {
        AgentDetailFacts::from_source(
            agent(),
            UserId::parse("creator-1").unwrap(),
            [],
            [],
            MaterializationStatus::Active,
            detail_projection(avatar_key),
        )
    }

    fn avatar_state(
        writes: AgentProfileRowWrite,
        reader: FakeDetailReader,
        blobs: FakeBlobs,
    ) -> (
        InternalAvatarWriteState<FakeWrites, FakeBlobs, FakeSink, FakeDetailReader, FixedClock>,
        FakeBlobs,
        FakeSink,
    ) {
        let sink = FakeSink::new();
        let state = InternalAvatarWriteState {
            writes: FakeWrites::Row(writes),
            blobs: blobs.clone(),
            announce: sink.clone(),
            reader,
            clock: FixedClock,
        };
        (state, blobs, sink)
    }

    fn avatar_router(
        state: InternalAvatarWriteState<
            FakeWrites,
            FakeBlobs,
            FakeSink,
            FakeDetailReader,
            FixedClock,
        >,
    ) -> Router {
        internal_avatar_write_router(state)
    }

    fn avatar_post(content_type: String, body: Vec<u8>) -> HttpRequest<Body> {
        HttpRequest::builder()
            .method("POST")
            .uri("/internal/agents/a1/avatar")
            .header("content-type", content_type)
            .extension(principal("a1"))
            .body(Body::from(body))
            .unwrap()
    }

    fn avatar_delete() -> HttpRequest<Body> {
        HttpRequest::builder()
            .method("DELETE")
            .uri("/internal/agents/a1/avatar")
            .extension(principal("a1"))
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn post_avatar_sets_and_echoes_the_full_agent_with_top_level_avatar_url() {
        // A first set (prior Absent): the row commits the fresh pointer, the reader returns
        // the post-write full detail, and the envelope is `{ avatar_url, agent }`.
        let writes = AgentProfileRowWrite::Committed {
            before: before("Name", AgentAvatarRow::Absent),
            row: committed("Name", present(FRESH)),
        };
        let reader = FakeDetailReader::with(Some(detail_facts(Some("avatars/a1.png"))));
        let (state, blobs, sink) = avatar_state(writes, reader.clone(), FakeBlobs::new());
        let (content_type, body) = multipart_body(&[], Some(&png_bytes()));
        let response = avatar_router(state)
            .oneshot(avatar_post(content_type, body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(sink.calls(), 1, "a set announces exactly once");
        assert!(
            blobs.deleted().is_empty(),
            "no prior blob to clean on a first set"
        );
        assert!(
            reader.reads() >= 1,
            "the response reprojects the full agent"
        );
        let value = body_json(response).await;
        let expected_url = format!("/api/agents/a1/avatar?v={NOW}");
        // The top-level avatar_url echoes the DTO's own derived value.
        assert_eq!(value["avatar_url"], json!(expected_url));
        assert_eq!(value["agent"]["avatar_url"], json!(expected_url));
        // The FULL AgentDto, not the narrow profile: fields the profile DTO never carries.
        assert_eq!(value["agent"]["id"], json!("a1"));
        assert_eq!(value["agent"]["runtime"], json!("claude"));
        assert_eq!(value["agent"]["host_id"], json!("h1"));
        assert_eq!(value["agent"]["materialization_status"], json!("active"));
        assert_eq!(value["agent"]["created_by_user_id"], json!("creator-1"));
        assert_eq!(value["agent"]["updated_at"], json!(NOW));
        // This is the agent echo, never the narrow `{ profile }` envelope.
        assert_eq!(value.get("profile"), None);
    }

    #[tokio::test]
    async fn post_avatar_with_a_prior_blob_cleans_it() {
        // A set that replaces a present avatar: the prior blob is best-effort cleaned.
        let writes = AgentProfileRowWrite::Committed {
            before: before("Name", present("agents/prior/blob-9")),
            row: committed("Name", present(FRESH)),
        };
        let reader = FakeDetailReader::with(Some(detail_facts(Some("avatars/a1.png"))));
        let (state, blobs, _sink) = avatar_state(writes, reader, FakeBlobs::new());
        let (content_type, body) = multipart_body(&[], Some(&png_bytes()));
        let response = avatar_router(state)
            .oneshot(avatar_post(content_type, body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(blobs.deleted(), vec!["agents/prior/blob-9".to_owned()]);
    }

    #[tokio::test]
    async fn delete_avatar_clears_present_and_echoes_the_full_agent_without_top_level_avatar_url() {
        // A clear of a present avatar: the row clears, the prior blob is cleaned, and the
        // envelope is `{ agent }` with a null agent avatar_url and NO top-level avatar_url.
        let writes = AgentProfileRowWrite::Committed {
            before: before("Name", present("agents/prior/blob-9")),
            row: committed("Name", AgentAvatarRow::Absent),
        };
        let reader = FakeDetailReader::with(Some(detail_facts(None)));
        let (state, blobs, sink) = avatar_state(writes, reader, FakeBlobs::new());
        let response = avatar_router(state).oneshot(avatar_delete()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(sink.calls(), 1);
        assert_eq!(blobs.deleted(), vec!["agents/prior/blob-9".to_owned()]);
        let value = body_json(response).await;
        assert_eq!(
            value.get("avatar_url"),
            None,
            "DELETE carries no top-level avatar_url"
        );
        assert_eq!(
            value["agent"].get("avatar_url"),
            Some(&Value::Null),
            "the cleared agent's avatar_url is null"
        );
        assert_eq!(value["agent"]["id"], json!("a1"));
        assert_eq!(value["agent"]["runtime"], json!("claude"));
    }

    #[tokio::test]
    async fn delete_avatar_on_absent_is_a_noop_still_200() {
        // A clear on an already-absent avatar: the warranted no-op, still `200 { agent }`,
        // no announce, no blob touched.
        let writes = AgentProfileRowWrite::Unchanged {
            before: before("Name", AgentAvatarRow::Absent),
        };
        let reader = FakeDetailReader::with(Some(detail_facts(None)));
        let (state, blobs, sink) = avatar_state(writes, reader, FakeBlobs::new());
        let response = avatar_router(state).oneshot(avatar_delete()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(sink.calls(), 0, "a genuine no-op announces nothing");
        assert!(blobs.deleted().is_empty());
        assert_eq!(
            body_json(response).await["agent"].get("avatar_url"),
            Some(&Value::Null)
        );
    }

    #[tokio::test]
    async fn post_avatar_missing_file_is_the_admission_400() {
        // No `file` part → the shared admission's FileFieldRequired, before any write.
        let writes = AgentProfileRowWrite::NoSuchAgent; // must never be consulted
        let reader = FakeDetailReader::with(None);
        let (mut state, blobs, sink) = avatar_state(writes, reader.clone(), FakeBlobs::new());
        state.writes = FakeWrites::Unexpected;
        let (content_type, body) = multipart_body(&[("ignored", "x")], None);
        let response = avatar_router(state)
            .oneshot(avatar_post(content_type, body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_json(response).await,
            json!({ "error": "file field required" })
        );
        assert_eq!(sink.calls(), 0);
        assert!(blobs.deleted().is_empty());
        assert_eq!(reader.reads(), 0, "a refused request never reprojects");
    }

    /// The whole-envelope discriminator at the ROUTE: a valid FIRST `file` followed by a
    /// truncated trailing part is the admission `400`, and NOTHING downstream is touched —
    /// zero blob put, zero row write, zero announce, zero reproject. The admission consumes
    /// every part to true EOF, so the malformed tail refuses BEFORE `run_avatar_edit` is
    /// even entered; a Set write puts its blob first and Committed announces + reprojects,
    /// so the refused request must not reach the row writer either.
    #[tokio::test]
    async fn post_avatar_valid_file_then_truncated_trailing_part_refuses_with_zero_effect() {
        // If the write core were ever reached, this Committed outcome would put, announce,
        // and reproject — so its side effects staying zero is the proof it was not.
        let writes = AgentProfileRowWrite::Committed {
            before: before("Name", AgentAvatarRow::Absent),
            row: committed("Name", present(FRESH)),
        };
        let reader = FakeDetailReader::with(Some(detail_facts(Some("avatars/a1.png"))));
        let (mut state, blobs, sink) = avatar_state(writes, reader.clone(), FakeBlobs::new());
        state.writes = FakeWrites::Unexpected;
        let (content_type, body) = multipart_valid_file_then_truncated_part();
        let response = avatar_router(state)
            .oneshot(avatar_post(content_type, body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_json(response).await,
            json!({ "error": "multipart/form-data required" })
        );
        assert_eq!(blobs.puts(), 0, "no blob was put");
        assert!(blobs.deleted().is_empty(), "no blob was deleted");
        assert_eq!(sink.calls(), 0, "no agent_changed was announced");
        assert_eq!(reader.reads(), 0, "no reproject read was issued");
    }

    #[tokio::test]
    async fn post_avatar_pre_write_missing_agent_is_404() {
        // The only legitimate 404: the row does not exist BEFORE the write. Any fresh blob
        // put ahead of the write is orphan-cleaned by the core.
        let writes = AgentProfileRowWrite::NoSuchAgent;
        let reader = FakeDetailReader::with(None);
        let (state, _blobs, sink) = avatar_state(writes, reader.clone(), FakeBlobs::new());
        let (content_type, body) = multipart_body(&[], Some(&png_bytes()));
        let response = avatar_router(state)
            .oneshot(avatar_post(content_type, body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            body_json(response).await,
            json!({ "error": "agent not found" })
        );
        assert_eq!(sink.calls(), 0);
        assert_eq!(reader.reads(), 0, "a pre-write 404 never reprojects");
    }

    #[tokio::test]
    async fn post_avatar_blob_put_fault_is_the_avatar_storage_400() {
        // The blob store faults on put: the stable detail-free 400, zero write/announce and
        // no reproject. The row writer must never be consulted.
        let writes = AgentProfileRowWrite::NoSuchAgent; // must never be reached
        let reader = FakeDetailReader::with(None);
        let (mut state, _blobs, sink) =
            avatar_state(writes, reader.clone(), FakeBlobs::failing_put());
        state.writes = FakeWrites::Unexpected;
        let (content_type, body) = multipart_body(&[], Some(&png_bytes()));
        let response = avatar_router(state)
            .oneshot(avatar_post(content_type, body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_json(response).await,
            json!({ "error": "avatar storage failed", "code": "avatar_storage_failed" })
        );
        assert_eq!(sink.calls(), 0, "a storage fault never announces");
        assert_eq!(reader.reads(), 0, "and never reprojects");
    }

    #[tokio::test]
    async fn post_avatar_committed_then_vanished_is_a_safe_500() {
        // A durable Committed whose row is unreadable at reproject: the projector face's
        // `Vanished` → safe-500, NEVER a 404.
        let writes = AgentProfileRowWrite::Committed {
            before: before("Name", AgentAvatarRow::Absent),
            row: committed("Name", present(FRESH)),
        };
        let reader = FakeDetailReader::with(None); // the row vanished after the commit
        let (state, _blobs, sink) = avatar_state(writes, reader.clone(), FakeBlobs::new());
        let (content_type, body) = multipart_body(&[], Some(&png_bytes()));
        let response = avatar_router(state)
            .oneshot(avatar_post(content_type, body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(sink.calls(), 1, "the durable commit still announced once");
        assert_eq!(
            reader.reads(),
            1,
            "the reproject was attempted and found nothing"
        );
    }

    #[tokio::test]
    async fn delete_avatar_absent_credential_is_a_safe_500_and_never_writes() {
        let writes = AgentProfileRowWrite::NoSuchAgent;
        let reader = FakeDetailReader::with(None);
        let (mut state, _blobs, sink) = avatar_state(writes, reader.clone(), FakeBlobs::new());
        state.writes = FakeWrites::Unexpected;
        // No credential extension: the self-pin layer was not composed in front.
        let request = HttpRequest::builder()
            .method("DELETE")
            .uri("/internal/agents/a1/avatar")
            .body(Body::empty())
            .unwrap();
        let response = avatar_router(state).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(sink.calls(), 0);
        assert_eq!(reader.reads(), 0);
    }

    #[tokio::test]
    async fn a_non_self_aid_avatar_is_401_before_any_write() {
        // The avatar pair sits BEHIND the same self-pin credential layer: a non-self `{aid}`
        // is the layer's single indistinguishable 401, never reaching the handler.
        let writes = AgentProfileRowWrite::NoSuchAgent;
        let reader = FakeDetailReader::with(None);
        let (mut state, _blobs, sink) = avatar_state(writes, reader.clone(), FakeBlobs::new());
        state.writes = FakeWrites::Unexpected;
        let authority = PinnedAuthority { enrolled: agent() };
        let router = crate::host_runtime::runtime_credential::require_runtime_credential(
            avatar_router(state),
            authority,
        );
        let (content_type, body) = multipart_body(&[], Some(&png_bytes()));
        let request = HttpRequest::builder()
            .method("POST")
            .uri("/internal/agents/someone-else/avatar")
            .header("content-type", content_type)
            .header(AUTHORIZATION, "Bearer test-secret")
            .body(Body::from(body))
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(sink.calls(), 0, "a non-self caller never reaches the write");
        assert_eq!(reader.reads(), 0);
    }
}
