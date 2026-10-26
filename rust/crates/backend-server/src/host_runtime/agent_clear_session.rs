//! Authenticated write face for `POST /api/agents/{id}/clear-session`: clear the
//! AgentSlot runtime session an agent owns.
//!
//! The route's authority is the Agent aggregate, NOT any conversation currently used
//! as presentation focus, so it carries no conversation coordinate and writes no
//! conversation audit message. The
//! ladder composes two authorities:
//!
//!  1. VISIBILITY ([`can_view_agent`], active-only) — absence, a caller outside the
//!     ACL, and a non-active stage all collapse to ONE `404 {"error":"agent not
//!     found"}`. This is the anti-enumeration gate the agent-seat admission ladder
//!     does NOT carry: [`clear_agent_session`] checks *existence*, never the stage or
//!     the viewer ACL, so the route face owns it exactly as the sibling `stop`/`patch`
//!     faces do.
//!  2. AUTHORITY + SCOPE + SEAL + CLEAR — [`clear_agent_session`] runs the agent ladder
//!     (edit-only, ZERO conversation reads), refusing a non-creator `403` and a
//!     conversation-scoped agent `409 runtime_scope_mismatch`; only on success does it
//!     mint the sealed seat target from the row-owned scope and clear through the
//!     provider. The route never selects a scope or mints a target itself.
//!
//! Like the read faces, the human principal is proof the authenticating perimeter ran;
//! its ABSENCE is a composition fault → fail-closed `500` reading nothing. Mounting the
//! real perimeter is the composition root's job — done by the backend-app #2234 CP4 mount.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Extension, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use serde::Serialize;
use serde_json::json;
use silverhand_backend_core::{
    conversation_messaging::messaging::collaboration::{
        ClearSessionProviderRefusal, ClearSessionRefusal, RuntimeControlAdmission,
        RuntimeControlAdmissionRefusal, RuntimeControlPort, clear_agent_session,
    },
    foundation::identity::{AgentId, Principal, UserId},
    identity_access::agent_detail_read::{AgentDetailFacts, AgentDetailReadModel},
    identity_access::capability::agent_view::{VisibleStages, can_view_agent},
};

use crate::conversation_messaging::messaging::collaboration::runtime_control::{
    clear_session_success, placement_refusal,
};
use crate::foundation::error;

/// The product route template, in axum's `{id}` capture syntax.
pub const AGENT_CLEAR_SESSION_PATH: &str = "/api/agents/{id}/clear-session";

/// The anti-enumeration refusal string, pinned to the TypeScript handler.
const AGENT_NOT_FOUND: &str = "agent not found";
/// The authority-refusal string (`c.json({ error: 'forbidden' }, 403)`).
const FORBIDDEN: &str = "forbidden";
/// The composition-fault body: a bare `text/plain` `500`, never a JSON envelope.
const INTERNAL_ERROR_BODY: &str = "Internal Server Error";
/// The `409 runtime_scope_mismatch` message, pinned to the agent-endpoint TS handler
/// (distinct from the conversation face's own scope-mismatch message).
const SCOPE_MISMATCH_MESSAGE: &str =
    "conversation-scoped sessions must be cleared from a conversation";
const SCOPE_MISMATCH_CODE: &str = "runtime_scope_mismatch";
const CLEAR_TIMEOUT_CODE: &str = "clear_session_timeout";
const CLEAR_IN_PROGRESS_CODE: &str = "clear_session_in_progress";

struct StateInner<R, A, P> {
    reads: R,
    admission: A,
    provider: P,
}

struct ClearSessionState<R, A, P>(Arc<StateInner<R, A, P>>);

impl<R, A, P> Clone for ClearSessionState<R, A, P> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

pub fn router<R, A, P>(reads: R, admission: A, provider: P) -> Router
where
    R: AgentDetailReadModel + Send + Sync + 'static,
    A: RuntimeControlAdmission + Send + Sync + 'static,
    P: RuntimeControlPort + Send + Sync + 'static,
{
    let state = ClearSessionState(Arc::new(StateInner {
        reads,
        admission,
        provider,
    }));
    Router::new()
        .route(
            AGENT_CLEAR_SESSION_PATH,
            post(agent_clear_session::<R, A, P>),
        )
        .with_state(state)
}

async fn agent_clear_session<R, A, P>(
    State(state): State<ClearSessionState<R, A, P>>,
    principal: Option<Extension<UserId>>,
    Path(agent_id): Path<String>,
) -> Response
where
    R: AgentDetailReadModel,
    A: RuntimeControlAdmission,
    P: RuntimeControlPort,
{
    let Some(Extension(actor)) = principal else {
        tracing::error!("agent-clear-session route reached without its human principal fact");
        return internal_error();
    };
    let viewer = Principal::human(actor.clone());

    // A malformed id names no agent — the same 404 an absent row gets.
    let Ok(agent) = AgentId::parse(agent_id) else {
        return not_found();
    };

    // Gate read: the one snapshot that decides visibility.
    let detail = match state.0.reads.read_agent_detail(&agent).await {
        Ok(detail) => detail,
        Err(cause) => {
            tracing::error!(
                error_chain =
                    crate::foundation::safe_log_field::safe_display(&(error::describe(&cause))),
                "agent-clear-session gate read failed"
            );
            return internal_error();
        }
    };

    // Bind the resolved row to the request before the gate: a bundle decoded for
    // another agent is a resolver defect, the lane's safe 500.
    if let Some(detail) = detail.as_ref()
        && detail.source() != &agent
    {
        tracing::error!(
            requested = agent.as_str(),
            resolved = detail.source().as_str(),
            "agent-clear-session resolved for the wrong agent"
        );
        return internal_error();
    }

    // VISIBILITY (active-only): absence, a caller outside the ACL, and a non-active
    // stage all collapse to one anti-enumeration 404 before any admission read.
    if !can_view_agent(
        &viewer,
        &agent,
        detail.as_ref().map(AgentDetailFacts::visibility),
        &VisibleStages::active_only(),
    )
    .is_allowed()
    {
        return not_found();
    }

    // AUTHORITY (edit) + SCOPE (agent-only) + SEAL + CLEAR. The core use case owns the
    // agent ladder, the row-owned scope, the sealed target mint, and the provider call.
    match clear_agent_session(&state.0.admission, &state.0.provider, actor, agent.clone()).await {
        Ok(Ok(outcome)) => clear_session_success(&outcome),
        Ok(Err(ClearSessionRefusal::Admission(refusal))) => admission_refusal(refusal),
        Ok(Err(ClearSessionRefusal::Provider(refusal))) => provider_refusal(refusal, &agent),
        Err(failure) => {
            tracing::error!(
                error_chain =
                    crate::foundation::safe_log_field::safe_display(&(error::describe(&failure))),
                "agent runtime clear-session failed"
            );
            internal_error()
        }
    }
}

/// Map the agent-ladder admission refusals. `ConversationNotFound`/`AgentNotMember`
/// are unreachable here — the agent ladder performs ZERO conversation reads — so their
/// appearance would mean the wrong ladder ran: a composition defect, fail closed.
fn admission_refusal(refusal: RuntimeControlAdmissionRefusal) -> Response {
    match refusal {
        RuntimeControlAdmissionRefusal::AgentNotFound => not_found(),
        RuntimeControlAdmissionRefusal::Forbidden => forbidden(),
        RuntimeControlAdmissionRefusal::RuntimeScopeMismatch => coded(
            StatusCode::CONFLICT,
            SCOPE_MISMATCH_CODE,
            SCOPE_MISMATCH_MESSAGE,
        ),
        RuntimeControlAdmissionRefusal::ConversationNotFound
        | RuntimeControlAdmissionRefusal::AgentNotMember => {
            tracing::error!("agent clear-session admission produced a conversation-scope refusal");
            internal_error()
        }
    }
}

/// Map the clear-session provider refusals. The messages carry the `agent/{id}`
/// coordinate (no conversation), pinned to the agent-endpoint TS handler; placement
/// refusals map through the ONE shared [`placement_refusal`] authority.
fn provider_refusal(refusal: ClearSessionProviderRefusal, agent: &AgentId) -> Response {
    match refusal {
        ClearSessionProviderRefusal::Placement(refusal) => placement_refusal(refusal),
        ClearSessionProviderRefusal::Timeout => coded(
            StatusCode::SERVICE_UNAVAILABLE,
            CLEAR_TIMEOUT_CODE,
            format!(
                "clear-session for agent/{} timed out waiting for runtime_exit projection landed",
                agent.as_str()
            ),
        ),
        ClearSessionProviderRefusal::InProgress => coded(
            StatusCode::CONFLICT,
            CLEAR_IN_PROGRESS_CODE,
            format!(
                "clear-session is already in progress for agent/{}",
                agent.as_str()
            ),
        ),
    }
}

/// The flat `{ "error": ... }` refusal body for the static refusals.
#[derive(Serialize)]
struct ErrorBody {
    error: &'static str,
}

fn not_found() -> Response {
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

fn internal_error() -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, INTERNAL_ERROR_BODY).into_response()
}

fn coded(status: StatusCode, code: &'static str, message: impl Into<String>) -> Response {
    let message = message.into();
    (status, Json(json!({ "error": message, "code": code }))).into_response()
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
        http::{Request, StatusCode},
    };
    use serde_json::{Value, json};
    use silverhand_backend_core::{
        conversation_messaging::messaging::collaboration::{
            ClearSessionOutcome, ClearSessionProviderRefusal, CliSessionId, HardStopRuntimeOutcome,
            HardStopRuntimeProviderRefusal, RuntimeControlAdmission, RuntimeControlAdmissionScope,
            RuntimePlacementRefusal, RuntimeSeatTarget, RuntimeStartTarget, StartRuntimeOutcome,
            StartRuntimeProviderRefusal,
        },
        foundation::identity::{AgentId, UserId},
        foundation::persistence::PersistenceFailure,
        identity_access::agent_lifecycle::MaterializationStatus,
        identity_access::agents::{runtime::RuntimeKind, session_scope::AgentSessionScope},
    };
    use tower::ServiceExt;

    use super::*;
    use silverhand_backend_core::identity_access::agent_detail_read::AgentDetailProjection;

    fn user(id: &str) -> UserId {
        UserId::parse(id).expect("user id")
    }

    fn agent(id: &str) -> AgentId {
        AgentId::parse(id).expect("agent id")
    }

    // ---- read fake (gate visibility) ----

    #[derive(Clone)]
    enum DetailOutcome {
        Present(Box<AgentDetailFacts>),
        Absent,
        Fails,
    }

    #[derive(Clone)]
    struct FakeReads {
        detail: DetailOutcome,
        calls: Arc<AtomicUsize>,
    }

    impl FakeReads {
        fn new(detail: DetailOutcome) -> Self {
            Self {
                detail,
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl AgentDetailReadModel for FakeReads {
        async fn read_agent_detail(
            &self,
            agent: &AgentId,
        ) -> Result<Option<AgentDetailFacts>, PersistenceFailure> {
            assert_eq!(agent.as_str(), "a1");
            self.calls.fetch_add(1, SeqCst);
            match &self.detail {
                DetailOutcome::Present(facts) => Ok(Some(facts.as_ref().clone())),
                DetailOutcome::Absent => Ok(None),
                DetailOutcome::Fails => {
                    Err(PersistenceFailure::from_source(io::Error::other("boom")))
                }
            }
        }
    }

    // ---- admission fake (agent ladder only) ----

    #[derive(Debug, thiserror::Error)]
    #[error("admission driver failed")]
    struct AdmissionFailure;

    #[derive(Clone)]
    struct FakeAdmission {
        exists: bool,
        may_edit: bool,
        scope: Option<AgentSessionScope>,
        snapshots: Arc<AtomicUsize>,
    }

    struct FakeScope {
        config: FakeAdmission,
    }

    impl RuntimeControlAdmission for FakeAdmission {
        type Error = AdmissionFailure;
        type Scope = FakeScope;

        async fn begin_snapshot(&self) -> Result<Self::Scope, Self::Error> {
            self.snapshots.fetch_add(1, SeqCst);
            Ok(FakeScope {
                config: self.clone(),
            })
        }
    }

    impl RuntimeControlAdmissionScope for FakeScope {
        type Error = AdmissionFailure;

        async fn conversation_exists(
            &mut self,
            _conversation: &silverhand_backend_core::foundation::identity::ConversationId,
        ) -> Result<bool, Self::Error> {
            unreachable!("the agent ladder performs zero conversation reads")
        }

        async fn agent_exists(&mut self, agent: &AgentId) -> Result<bool, Self::Error> {
            assert_eq!(agent.as_str(), "a1");
            Ok(self.config.exists)
        }

        async fn agent_is_member(
            &mut self,
            _conversation: &silverhand_backend_core::foundation::identity::ConversationId,
            _agent: &AgentId,
        ) -> Result<bool, Self::Error> {
            unreachable!("the agent ladder performs zero conversation reads")
        }

        async fn actor_may_post(
            &mut self,
            _conversation: &silverhand_backend_core::foundation::identity::ConversationId,
            _actor: &UserId,
        ) -> Result<bool, Self::Error> {
            unreachable!("the agent ladder performs zero conversation reads")
        }

        async fn actor_may_edit_agent(
            &mut self,
            actor: &UserId,
            agent: &AgentId,
        ) -> Result<bool, Self::Error> {
            assert_eq!(actor.as_str(), "viewer-1");
            assert_eq!(agent.as_str(), "a1");
            Ok(self.config.may_edit)
        }

        async fn agent_session_scope(
            &mut self,
            agent: &AgentId,
        ) -> Result<Option<AgentSessionScope>, Self::Error> {
            assert_eq!(agent.as_str(), "a1");
            Ok(self.config.scope)
        }

        async fn finish(self) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    // ---- provider fake ----

    #[derive(Clone, Copy)]
    enum ProviderMode {
        Success,
        ClearRefusal(ClearSessionProviderRefusal),
    }

    #[derive(Debug, thiserror::Error)]
    #[error("provider driver failed")]
    struct ProviderFailure;

    #[derive(Clone)]
    struct FakeProvider {
        mode: ProviderMode,
        clears: Arc<Mutex<usize>>,
    }

    impl FakeProvider {
        fn new(mode: ProviderMode) -> Self {
            Self {
                mode,
                clears: Arc::new(Mutex::new(0)),
            }
        }
    }

    impl RuntimeControlPort for FakeProvider {
        type Error = ProviderFailure;

        async fn start(
            &self,
            _target: &RuntimeStartTarget,
        ) -> Result<Result<StartRuntimeOutcome, StartRuntimeProviderRefusal>, Self::Error> {
            unreachable!("clear-session never starts")
        }

        async fn hard_stop(
            &self,
            _target: &RuntimeSeatTarget,
        ) -> Result<Result<HardStopRuntimeOutcome, HardStopRuntimeProviderRefusal>, Self::Error>
        {
            unreachable!("clear-session never hard-stops")
        }

        async fn clear_session(
            &self,
            target: &RuntimeSeatTarget,
        ) -> Result<Result<ClearSessionOutcome, ClearSessionProviderRefusal>, Self::Error> {
            assert_eq!(target.actor().as_str(), "viewer-1");
            assert!(
                matches!(target.scope(), silverhand_backend_core::host_runtime::runtime_credential::RuntimeScope::AgentSlot { agent } if agent.as_str() == "a1")
            );
            *self.clears.lock().unwrap() += 1;
            match self.mode {
                ProviderMode::Success => Ok(Ok(ClearSessionOutcome::committed(
                    Some(CliSessionId::parse("cli-previous").unwrap()),
                    true,
                ))),
                ProviderMode::ClearRefusal(refusal) => Ok(Err(refusal)),
            }
        }
    }

    // ---- fixtures ----

    fn projection(session_scope: &str) -> AgentDetailProjection {
        AgentDetailProjection {
            name: "agent-one".to_owned(),
            display_name: "Base Name".to_owned(),
            runtime: RuntimeKind::Codex,
            model: Some("gpt-5.6-sol".to_owned()),
            reasoning_effort: Some("ultra".to_owned()),
            fast_mode: Some(true),
            host_id: "h1".to_owned(),
            host_display_label: Some("Host One".to_owned()),
            host_name: Some("host-one".to_owned()),
            description: Some("base desc".to_owned()),
            session_scope: session_scope.to_owned(),
            avatar_storage_key: None,
            materialization_failed_code: None,
            materialization_failed_at: None,
            deletion_started_at: None,
            created_at: 1000,
            updated_at: 2000,
        }
    }

    /// Creator `viewer-1`, active, agent-scoped — passes the active-only view gate.
    fn creator_active() -> AgentDetailFacts {
        AgentDetailFacts::from_source(
            agent("a1"),
            user("viewer-1"),
            [],
            [],
            MaterializationStatus::Active,
            projection("agent"),
        )
    }

    fn present(facts: AgentDetailFacts) -> DetailOutcome {
        DetailOutcome::Present(Box::new(facts))
    }

    fn admission(exists: bool, may_edit: bool, scope: Option<AgentSessionScope>) -> FakeAdmission {
        FakeAdmission {
            exists,
            may_edit,
            scope,
            snapshots: Arc::new(AtomicUsize::new(0)),
        }
    }

    struct Reply {
        status: StatusCode,
        body: String,
        detail_reads: usize,
        admission_reads: usize,
        clears: usize,
    }

    async fn clear_with(
        reads: FakeReads,
        admission: FakeAdmission,
        provider: FakeProvider,
        path_id: &str,
        with_principal: bool,
    ) -> Reply {
        let detail_calls = reads.calls.clone();
        let admission_calls = admission.snapshots.clone();
        let clear_calls = provider.clears.clone();
        let uri = format!("/api/agents/{path_id}/clear-session");
        let mut request = Request::post(&uri);
        if with_principal {
            request = request.extension(user("viewer-1"));
        }
        let response = router(reads, admission, provider)
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        Reply {
            status,
            body: String::from_utf8(bytes.to_vec()).unwrap(),
            detail_reads: detail_calls.load(SeqCst),
            admission_reads: admission_calls.load(SeqCst),
            clears: *clear_calls.lock().unwrap(),
        }
    }

    fn parsed(body: &str) -> Value {
        serde_json::from_str::<Value>(body).unwrap()
    }

    // ---- the ladder ----

    #[tokio::test]
    async fn a_missing_principal_is_a_safe_500_that_never_reads() {
        let reply = clear_with(
            FakeReads::new(present(creator_active())),
            admission(true, true, Some(AgentSessionScope::Agent)),
            FakeProvider::new(ProviderMode::Success),
            "a1",
            false,
        )
        .await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply.body, "Internal Server Error");
        assert_eq!(reply.detail_reads, 0);
        assert_eq!(reply.admission_reads, 0);
        assert_eq!(reply.clears, 0);
    }

    #[tokio::test]
    async fn absent_non_active_and_invisible_agents_are_404_before_admission() {
        for detail in [
            DetailOutcome::Absent,
            present(AgentDetailFacts::from_source(
                agent("a1"),
                user("viewer-1"),
                [],
                [],
                MaterializationStatus::Failed,
                projection("agent"),
            )),
            present(AgentDetailFacts::from_source(
                agent("a1"),
                user("someone-else"),
                [],
                [],
                MaterializationStatus::Active,
                projection("agent"),
            )),
        ] {
            let reply = clear_with(
                FakeReads::new(detail),
                admission(true, true, Some(AgentSessionScope::Agent)),
                FakeProvider::new(ProviderMode::Success),
                "a1",
                true,
            )
            .await;
            assert_eq!(reply.status, StatusCode::NOT_FOUND);
            assert_eq!(parsed(&reply.body), json!({"error":"agent not found"}));
            assert_eq!(reply.admission_reads, 0);
            assert_eq!(reply.clears, 0);
        }
    }

    #[tokio::test]
    async fn a_bundle_resolved_for_another_agent_is_a_safe_500() {
        let spliced = AgentDetailFacts::from_source(
            agent("a2"),
            user("viewer-1"),
            [],
            [],
            MaterializationStatus::Active,
            projection("agent"),
        );
        let reply = clear_with(
            FakeReads::new(present(spliced)),
            admission(true, true, Some(AgentSessionScope::Agent)),
            FakeProvider::new(ProviderMode::Success),
            "a1",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply.body, "Internal Server Error");
        assert_eq!(reply.admission_reads, 0);
        assert_eq!(reply.clears, 0);
    }

    #[tokio::test]
    async fn a_gate_read_failure_is_a_safe_500() {
        let reply = clear_with(
            FakeReads::new(DetailOutcome::Fails),
            admission(true, true, Some(AgentSessionScope::Agent)),
            FakeProvider::new(ProviderMode::Success),
            "a1",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply.body, "Internal Server Error");
        assert_eq!(reply.admission_reads, 0);
        assert_eq!(reply.clears, 0);
    }

    #[tokio::test]
    async fn a_viewer_who_is_not_the_creator_is_forbidden_and_never_clears() {
        // viewer-1 can SEE it (shared-workspace member) but is not the creator.
        let facts = AgentDetailFacts::from_source(
            agent("a1"),
            user("creator-user"),
            [],
            [user("viewer-1")],
            MaterializationStatus::Active,
            projection("agent"),
        );
        let reply = clear_with(
            FakeReads::new(present(facts)),
            admission(true, false, Some(AgentSessionScope::Agent)),
            FakeProvider::new(ProviderMode::Success),
            "a1",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN);
        assert_eq!(parsed(&reply.body), json!({ "error": "forbidden" }));
        assert_eq!(reply.clears, 0);
    }

    #[tokio::test]
    async fn a_conversation_scoped_agent_is_a_409_runtime_scope_mismatch_and_never_clears() {
        let reply = clear_with(
            FakeReads::new(present(creator_active())),
            admission(true, true, Some(AgentSessionScope::Conversation)),
            FakeProvider::new(ProviderMode::Success),
            "a1",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::CONFLICT);
        assert_eq!(
            parsed(&reply.body),
            json!({
                "error": "conversation-scoped sessions must be cleared from a conversation",
                "code": "runtime_scope_mismatch",
            })
        );
        assert_eq!(reply.clears, 0);
    }

    #[tokio::test]
    async fn a_cleared_session_is_a_200_with_the_previous_id_and_running_flag() {
        let reply = clear_with(
            FakeReads::new(present(creator_active())),
            admission(true, true, Some(AgentSessionScope::Agent)),
            FakeProvider::new(ProviderMode::Success),
            "a1",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(
            parsed(&reply.body),
            json!({
                "ok": true,
                "prev_cli_session_id": "cli-previous",
                "was_running": true,
            })
        );
        assert_eq!(reply.clears, 1);
    }

    #[tokio::test]
    async fn a_provider_timeout_is_a_503_with_the_agent_scoped_message() {
        let reply = clear_with(
            FakeReads::new(present(creator_active())),
            admission(true, true, Some(AgentSessionScope::Agent)),
            FakeProvider::new(ProviderMode::ClearRefusal(
                ClearSessionProviderRefusal::Timeout,
            )),
            "a1",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            parsed(&reply.body),
            json!({
                "error": "clear-session for agent/a1 timed out waiting for runtime_exit projection landed",
                "code": "clear_session_timeout",
            })
        );
    }

    #[tokio::test]
    async fn a_provider_in_progress_is_a_409_with_the_agent_scoped_message() {
        let reply = clear_with(
            FakeReads::new(present(creator_active())),
            admission(true, true, Some(AgentSessionScope::Agent)),
            FakeProvider::new(ProviderMode::ClearRefusal(
                ClearSessionProviderRefusal::InProgress,
            )),
            "a1",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::CONFLICT);
        assert_eq!(
            parsed(&reply.body),
            json!({
                "error": "clear-session is already in progress for agent/a1",
                "code": "clear_session_in_progress",
            })
        );
    }

    #[tokio::test]
    async fn a_provider_placement_refusal_maps_through_the_canonical_mapper() {
        let reply = clear_with(
            FakeReads::new(present(creator_active())),
            admission(true, true, Some(AgentSessionScope::Agent)),
            FakeProvider::new(ProviderMode::ClearRefusal(
                ClearSessionProviderRefusal::Placement(RuntimePlacementRefusal::HostRevoked),
            )),
            "a1",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::CONFLICT);
        assert_eq!(
            parsed(&reply.body),
            json!({ "error": "daemon host has been revoked", "code": "host_revoked" })
        );
    }
}
