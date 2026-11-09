//! Authenticated public byte fetch for `GET /api/agents/{id}/avatar`: the raw avatar
//! image for one agent. Public like the profile: any signed-in human reads it for an
//! active agent ([`agent_read_tier`]).
//!
//!  1. TIER — an absent row, and a pending or failed one, are one
//!     `404 {"error":"agent not found"}`, decided before the serve-ref or blob is
//!     touched.
//!  2. SERVE — only for a readable agent: read the row's own id plus the
//!     `(storage_key, content_type)` pair as one typed shape, bind the read-back id to
//!     the request (a SECOND source-bind, over the separate serve-ref port), and serve
//!     the bytes. An agent with no row and a row with no avatar are BOTH the distinct
//!     `404 {"error":"avatar not found"}` (the caller is allowed to know the agent
//!     exists, so this second 404 is not an oracle). A below-write-path corruption, a
//!     serve fact resolved for another agent, a DANGLING pointer (the row asserts bytes
//!     that the backend no longer holds), or a hard blob-backend read fault are all the
//!     lane's safe `500`, with the cause logged — matching the TypeScript route, whose
//!     uncaught `blobStorage.serve` throws on a missing file rather than answering 404.
//!
//! It reads TWICE — the detail facts for the gate, then the narrow serve-ref for the
//! bytes. Each read is INDEPENDENTLY source-bound to the request so a
//! cross-wired adapter can never splice one agent's bytes onto another's authorized
//! response. There is no ETag/304: the response is `Cache-Control: private, no-store`,
//! and the wire cache-buster lives in the DTO's `avatar_url` query, not here.
//!
//! As in the sibling read routes the human principal is proof the authenticating
//! perimeter ran; its ABSENCE is a composition fault (a fail-closed `500` that reads
//! nothing), and mounting the real bearer perimeter is the composition root's job.

use axum::{
    Json, Router,
    body::Body,
    extract::{Extension, Path, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Serialize;
use silverhand_backend_core::{
    foundation::identity::{AgentId, UserId},
    identity_access::agent_avatar::{
        AgentAvatarBlobRead, AgentAvatarFetchOutcome, AgentAvatarServeRefRead, fetch_agent_avatar,
    },
    identity_access::agent_detail_read::{AgentDetailFacts, AgentDetailReadModel},
    identity_access::avatar::AvatarContentType,
    identity_access::capability::{agent_view::VisibleStages, read_tier::agent_read_tier},
};

use crate::foundation::error;

/// The product route template, in axum's `{id}` capture syntax.
pub const AGENT_AVATAR_PATH: &str = "/api/agents/{id}/avatar";

/// The not-found refusal, pinned to the TypeScript handler
/// (`c.json({ error: 'agent not found' }, 404)`).
const AGENT_NOT_FOUND: &str = "agent not found";

/// The distinct refusal for an admitted caller whose agent simply has no avatar —
/// the TypeScript `c.json({ error: 'avatar not found' }, 404)`.
const AVATAR_NOT_FOUND: &str = "avatar not found";

/// The framework default for an unmapped failure: a bare `text/plain` `500`.
const INTERNAL_ERROR_BODY: &str = "Internal Server Error";

#[derive(Clone)]
struct AvatarState<R, S, B> {
    cold_read: R,
    serve_ref: S,
    blobs: B,
}

pub fn router<R, S, B>(cold_read: R, serve_ref: S, blobs: B) -> Router
where
    R: AgentDetailReadModel + Clone + Send + Sync + 'static,
    S: AgentAvatarServeRefRead,
    B: AgentAvatarBlobRead,
{
    Router::new()
        .route(AGENT_AVATAR_PATH, get(agent_avatar::<R, S, B>))
        .with_state(AvatarState {
            cold_read,
            serve_ref,
            blobs,
        })
}

async fn agent_avatar<R, S, B>(
    Path(agent_id): Path<String>,
    viewer: Option<Extension<UserId>>,
    State(state): State<AvatarState<R, S, B>>,
) -> Response
where
    R: AgentDetailReadModel,
    S: AgentAvatarServeRefRead,
    B: AgentAvatarBlobRead,
{
    let Some(Extension(viewer)) = viewer else {
        tracing::error!("agent-avatar route reached without its human principal fact");
        return internal_error();
    };

    // A malformed id names no agent — the same 404 an absent row gets.
    let Ok(agent) = AgentId::parse(agent_id) else {
        return agent_not_found();
    };

    // VISIBILITY, read and judged BEFORE the avatar is touched.
    let detail = match state.cold_read.read_agent_detail(&agent).await {
        Ok(detail) => detail,
        Err(cause) => {
            tracing::error!(
                error_chain =
                    crate::foundation::safe_log_field::safe_display(&(error::describe(&cause))),
                "agent-avatar detail read failed"
            );
            return internal_error();
        }
    };

    // Bind the resolved row to the request before the gate: a bundle decoded for
    // another agent would gate one row and serve another's avatar — a resolver
    // defect that takes the lane's safe 500.
    if let Some(detail) = detail.as_ref()
        && detail.source() != &agent
    {
        tracing::error!(
            requested = agent.as_str(),
            resolved = detail.source().as_str(),
            "agent-avatar resolved for the wrong agent"
        );
        return internal_error();
    }

    // The avatar fetch has no stage wideners: the default active-only view, exactly
    // as the TypeScript route runs `canViewAgent` with no flags.
    let stages = VisibleStages::active_only();
    let readable = agent_read_tier(
        &viewer,
        &agent,
        detail.as_ref().map(AgentDetailFacts::visibility),
        &stages,
    )
    .is_some();
    if !readable {
        return agent_not_found();
    }

    // Admitted: only now is the serve-ref read and the blob touched.
    match fetch_agent_avatar(&state.serve_ref, &state.blobs, &agent).await {
        Ok(AgentAvatarFetchOutcome::Found {
            bytes,
            content_type,
            filename,
        }) => serve_avatar(bytes, content_type, &filename),
        Ok(AgentAvatarFetchOutcome::NotFound) => avatar_not_found(),
        Ok(AgentAvatarFetchOutcome::StorageUnavailable { cause }) => {
            tracing::error!(
                error_chain =
                    crate::foundation::safe_log_field::safe_display(&(error::describe(&cause))),
                "agent-avatar blob unavailable (dangling pointer or read fault)"
            );
            internal_error()
        }
        Err(cause) => {
            tracing::error!(
                error_chain =
                    crate::foundation::safe_log_field::safe_display(&(error::describe(&cause))),
                "agent-avatar serve-ref read or source-bind failed"
            );
            internal_error()
        }
    }
}

/// Serve the raw avatar bytes with the stored content type, the same header set the
/// `users` fetch and the TypeScript blob `serve` produce: the allowlisted
/// `Content-Type`, an explicit `Content-Length`, an `inline` disposition carrying the
/// download filename (ASCII and UTF-8 forms), and `Cache-Control: private, no-store`.
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

/// The flat refusal body, mirroring the product `{ "error": ... }`.
#[derive(Serialize)]
struct ErrorBody {
    error: &'static str,
}

fn agent_not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(ErrorBody {
            error: AGENT_NOT_FOUND,
        }),
    )
        .into_response()
}

fn avatar_not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(ErrorBody {
            error: AVATAR_NOT_FOUND,
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
            Arc,
            atomic::{AtomicUsize, Ordering::SeqCst},
        },
    };

    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode, header},
    };
    use serde_json::{Value, json};
    use silverhand_backend_core::{
        foundation::identity::{AgentId, UserId},
        foundation::persistence::PersistenceFailure,
        identity_access::agent_avatar::{
            AgentAvatarBlobPointer, AgentAvatarBlobRead, AgentAvatarRow, AgentAvatarServeRef,
            OwnedAgentAvatarBlob,
        },
        identity_access::agent_detail_read::{AgentDetailFacts, AgentDetailProjection},
        identity_access::agent_lifecycle::MaterializationStatus,
        identity_access::agents::runtime::RuntimeKind,
        identity_access::avatar::{AvatarBlobReadOutcome, AvatarContentType, AvatarStorageFailure},
    };
    use tower::ServiceExt;

    use super::*;

    fn user(id: &str) -> UserId {
        UserId::parse(id).expect("user id")
    }

    fn agent(id: &str) -> AgentId {
        AgentId::parse(id).expect("agent id")
    }

    /// Which ACL arm admits `viewer-1`, plus one that admits only OTHER people.
    enum Arm {
        Creator,
        NobodyRelevant,
    }

    fn arms(arm: Arm) -> (UserId, Vec<UserId>, Vec<UserId>) {
        match arm {
            Arm::Creator => (user("viewer-1"), vec![], vec![]),
            Arm::NobodyRelevant => (
                user("someone-else"),
                vec![user("pair-peer")],
                vec![user("member")],
            ),
        }
    }

    fn projection() -> AgentDetailProjection {
        AgentDetailProjection {
            name: "agent-one".to_string(),
            display_name: "Agent One".to_string(),
            runtime: RuntimeKind::Claude,
            model: None,
            reasoning_effort: None,
            fast_mode: None,
            host_id: "h1".to_string(),
            host_display_label: None,
            host_name: Some("host-one".to_string()),
            description: None,
            session_scope: "agent".to_string(),
            avatar_storage_key: Some("agents/a1/avatars/blob-0.png".to_string()),
            materialization_failed_code: None,
            materialization_failed_at: None,
            deletion_started_at: None,
            created_at: 1000,
            updated_at: 2000,
        }
    }

    fn facts(source: &str, arm: Arm, status: MaterializationStatus) -> AgentDetailFacts {
        let (creator, direct_pair, workspace) = arms(arm);
        AgentDetailFacts::from_source(
            agent(source),
            creator,
            direct_pair,
            workspace,
            status,
            projection(),
        )
    }

    #[derive(Clone)]
    enum ViewOutcome {
        Present(Box<AgentDetailFacts>),
        Absent,
        Fails,
    }

    #[derive(Clone)]
    struct FakeDetail(ViewOutcome);
    impl AgentDetailReadModel for FakeDetail {
        async fn read_agent_detail(
            &self,
            _agent: &AgentId,
        ) -> Result<Option<AgentDetailFacts>, PersistenceFailure> {
            match &self.0 {
                ViewOutcome::Present(facts) => Ok(Some(facts.as_ref().clone())),
                ViewOutcome::Absent => Ok(None),
                ViewOutcome::Fails => {
                    Err(PersistenceFailure::from_source(io::Error::other("gone")))
                }
            }
        }
    }

    /// The serve-ref read, counting calls so a test can prove the ACL gate runs FIRST
    /// (a refused caller never reaches this read).
    #[derive(Clone)]
    struct FakeServeRef {
        outcome: Result<AgentAvatarServeRef, ()>,
        calls: Arc<AtomicUsize>,
    }
    impl FakeServeRef {
        fn new(outcome: Result<AgentAvatarServeRef, ()>) -> Self {
            Self {
                outcome,
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }
    }
    impl AgentAvatarServeRefRead for FakeServeRef {
        async fn read_agent_avatar_serve_ref(
            &self,
            _agent: &AgentId,
        ) -> Result<AgentAvatarServeRef, PersistenceFailure> {
            self.calls.fetch_add(1, SeqCst);
            self.outcome
                .clone()
                .map_err(|()| PersistenceFailure::from_source(io::Error::other("ref down")))
        }
    }

    #[derive(Clone)]
    struct FakeBlob {
        outcome: Option<AvatarBlobReadOutcome>,
        fail: bool,
        calls: Arc<AtomicUsize>,
    }
    impl FakeBlob {
        fn found(bytes: Vec<u8>) -> Self {
            Self {
                outcome: Some(AvatarBlobReadOutcome::Found(bytes)),
                fail: false,
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }
        fn missing() -> Self {
            Self {
                outcome: Some(AvatarBlobReadOutcome::Missing),
                fail: false,
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }
        fn faulty() -> Self {
            Self {
                outcome: None,
                fail: true,
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }
        fn idle() -> Self {
            Self {
                outcome: None,
                fail: false,
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }
    }
    impl AgentAvatarBlobRead for FakeBlob {
        async fn read(
            &self,
            _blob: &OwnedAgentAvatarBlob,
        ) -> Result<AvatarBlobReadOutcome, AvatarStorageFailure> {
            self.calls.fetch_add(1, SeqCst);
            if self.fail {
                #[derive(Debug, thiserror::Error)]
                #[error("read fault")]
                struct ReadFault;
                return Err(AvatarStorageFailure::from_source(ReadFault));
            }
            Ok(self
                .outcome
                .clone()
                .unwrap_or(AvatarBlobReadOutcome::Missing))
        }
    }

    /// A present, source-bound serve fact: the row read back its own id (`source`) and
    /// carries a valid agent-owned pointer + content type. The `source` is the identity
    /// the core binds against the requested id before it touches the blob.
    fn serve_row(source: &str) -> AgentAvatarServeRef {
        AgentAvatarServeRef::Row {
            source: agent(source),
            avatar: AgentAvatarRow::Present {
                pointer: AgentAvatarBlobPointer::parse("agents/a1/avatars/blob-0.png").unwrap(),
                content_type: AvatarContentType::Png,
            },
        }
    }

    /// The common case: the serve fact's read-back source matches the requested `a1`.
    fn present_ref() -> AgentAvatarServeRef {
        serve_row("a1")
    }

    struct Reply {
        status: StatusCode,
        headers: axum::http::HeaderMap,
        body: Vec<u8>,
        serve_ref_calls: usize,
        blob_calls: usize,
    }

    async fn get(
        view: ViewOutcome,
        serve_ref: Result<AgentAvatarServeRef, ()>,
        blobs: FakeBlob,
        path_id: &str,
        with_principal: bool,
    ) -> Reply {
        let refs = FakeServeRef::new(serve_ref);
        let refs_probe = refs.clone();
        let blob_probe = blobs.clone();
        let uri = format!("/api/agents/{path_id}/avatar");
        let mut request = Request::get(&uri);
        if with_principal {
            request = request.extension(user("viewer-1"));
        }
        let response = router(FakeDetail(view), refs, blobs)
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        Reply {
            status,
            headers,
            body: body.to_vec(),
            serve_ref_calls: refs_probe.calls.load(SeqCst),
            blob_calls: blob_probe.calls.load(SeqCst),
        }
    }

    fn present(source: &str, arm: Arm) -> ViewOutcome {
        ViewOutcome::Present(Box::new(facts(source, arm, MaterializationStatus::Active)))
    }

    fn json_body(reply: &Reply) -> Value {
        serde_json::from_slice(&reply.body).unwrap()
    }

    #[tokio::test]
    async fn an_admitted_caller_gets_the_bytes_with_serve_headers() {
        let reply = get(
            present("a1", Arm::Creator),
            Ok(present_ref()),
            FakeBlob::found(vec![1, 2, 3, 4]),
            "a1",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.body, vec![1, 2, 3, 4]);
        assert_eq!(reply.headers[header::CONTENT_TYPE], "image/png");
        assert_eq!(reply.headers[header::CONTENT_LENGTH], "4");
        assert_eq!(reply.headers[header::CACHE_CONTROL], "private, no-store");
        assert_eq!(
            reply.headers[header::CONTENT_DISPOSITION],
            "inline; filename=\"avatar.png\"; filename*=UTF-8''avatar.png"
        );
        // No ETag: the cache-buster lives in the DTO's avatar_url query, not here.
        assert!(!reply.headers.contains_key(header::ETAG));
    }

    #[tokio::test]
    async fn an_admitted_caller_with_no_avatar_is_avatar_not_found() {
        let reply = get(
            present("a1", Arm::Creator),
            Ok(AgentAvatarServeRef::Row {
                source: agent("a1"),
                avatar: AgentAvatarRow::Absent,
            }),
            FakeBlob::idle(),
            "a1",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        assert_eq!(json_body(&reply), json!({ "error": "avatar not found" }));
        assert_eq!(reply.blob_calls, 0, "an absent ref never touches the blob");
    }

    #[tokio::test]
    async fn a_clean_missing_blob_is_a_storage_inconsistency_not_a_404() {
        // A present row asserts its bytes exist (parity with TS: a present ref calls
        // `blobStorage.serve` directly, and `stat`/`readFile` ENOENT throws uncaught →
        // 500). A dangling pointer is storage inconsistency, never `avatar not found`.
        let reply = get(
            present("a1", Arm::Creator),
            Ok(present_ref()),
            FakeBlob::missing(),
            "a1",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply.body, b"Internal Server Error");
        assert_eq!(
            reply.blob_calls, 1,
            "the present row was resolved against the blob"
        );
    }

    #[tokio::test]
    async fn a_serve_ref_for_another_agent_is_a_safe_500_with_the_blob_untouched() {
        // P0-1 liar-reader guard: the URL asks about `a1` and the ACL admits `a1`, but a
        // cross-wired serve-ref adapter hands back a fact whose read-back source is `a2`.
        // The core binds `source == requested` BEFORE the blob, so this is a safe-500 and
        // the blob is never read — no ACL-A / bytes-B splice.
        let reply = get(
            present("a1", Arm::Creator),
            Ok(serve_row("a2")),
            FakeBlob::found(vec![7, 7, 7]),
            "a1",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply.body, b"Internal Server Error");
        assert_eq!(reply.serve_ref_calls, 1, "the serve-ref read did run");
        assert_eq!(
            reply.blob_calls, 0,
            "the source-bind refuses before the mismatched bytes are served"
        );
    }

    #[tokio::test]
    async fn a_serve_ref_corruption_is_a_safe_500_that_never_reads_the_blob() {
        let reply = get(
            present("a1", Arm::Creator),
            Err(()),
            FakeBlob::idle(),
            "a1",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply.body, b"Internal Server Error");
        assert_eq!(reply.blob_calls, 0);
    }

    #[tokio::test]
    async fn a_hard_blob_fault_is_a_safe_500() {
        let reply = get(
            present("a1", Arm::Creator),
            Ok(present_ref()),
            FakeBlob::faulty(),
            "a1",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply.body, b"Internal Server Error");
    }

    #[tokio::test]
    async fn the_avatar_is_public_for_an_active_agent() {
        let reply = get(
            present("a1", Arm::NobodyRelevant),
            Ok(present_ref()),
            FakeBlob::found(vec![9]),
            "a1",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.body, vec![9]);
    }

    #[tokio::test]
    async fn a_hidden_stage_is_agent_not_found_before_the_serve_ref() {
        let reply = get(
            ViewOutcome::Present(Box::new(facts(
                "a1",
                Arm::NobodyRelevant,
                MaterializationStatus::Failed,
            ))),
            Ok(present_ref()),
            FakeBlob::found(vec![9]),
            "a1",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        assert_eq!(json_body(&reply), json!({ "error": "agent not found" }));
        assert_eq!(reply.serve_ref_calls, 0);
        assert_eq!(reply.blob_calls, 0);
    }

    #[tokio::test]
    async fn an_absent_agent_is_agent_not_found_and_reads_no_avatar() {
        let reply = get(
            ViewOutcome::Absent,
            Ok(present_ref()),
            FakeBlob::found(vec![9]),
            "a1",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        assert_eq!(json_body(&reply), json!({ "error": "agent not found" }));
        assert_eq!(reply.serve_ref_calls, 0);
    }

    #[tokio::test]
    async fn a_hidden_stage_is_agent_not_found_under_the_default_view() {
        let hidden = ViewOutcome::Present(Box::new(facts(
            "a1",
            Arm::Creator,
            MaterializationStatus::PendingMaterialization,
        )));
        let reply = get(hidden, Ok(present_ref()), FakeBlob::idle(), "a1", true).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        assert_eq!(json_body(&reply), json!({ "error": "agent not found" }));
        assert_eq!(reply.serve_ref_calls, 0);
    }

    #[tokio::test]
    async fn a_malformed_id_is_agent_not_found() {
        let reply = get(
            present("a1", Arm::Creator),
            Ok(present_ref()),
            FakeBlob::idle(),
            "",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        assert_eq!(json_body(&reply), json!({ "error": "agent not found" }));
    }

    #[tokio::test]
    async fn a_missing_principal_is_a_safe_500_that_never_reads() {
        let reply = get(
            present("a1", Arm::Creator),
            Ok(present_ref()),
            FakeBlob::idle(),
            "a1",
            false,
        )
        .await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply.serve_ref_calls, 0);
    }

    #[tokio::test]
    async fn a_detail_read_failure_is_a_safe_500() {
        let reply = get(
            ViewOutcome::Fails,
            Ok(present_ref()),
            FakeBlob::idle(),
            "a1",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply.serve_ref_calls, 0);
    }

    #[tokio::test]
    async fn a_bundle_resolved_for_another_agent_is_a_safe_500() {
        // The URL asks about `a1`, the resolver returns `a2`: the gate would judge one
        // row and serve another's avatar, so the source-bind refuses it.
        let reply = get(
            present("a2", Arm::Creator),
            Ok(present_ref()),
            FakeBlob::idle(),
            "a1",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply.serve_ref_calls, 0);
    }
}
