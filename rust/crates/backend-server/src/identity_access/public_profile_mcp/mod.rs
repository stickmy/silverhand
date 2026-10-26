//! Public management-plane agent profile-MCP faces:
//! `GET /api/agents/{id}/profile/mcp` (read the raw `mcp.json`) and
//! `POST /api/agents/{id}/profile/mcp/probe` (one-shot live probe).
//!
//! ## The two-authority ladder — BOTH faces are edit-class
//!
//! Both sit behind the authenticated human perimeter and the same two-authority ladder
//! the sibling management writes ([`crate::identity_access::agent_patch`]) use:
//!
//!  1. VISIBILITY ([`can_view_agent`], active-only) — absence, a caller outside the ACL,
//!     and a non-active stage all collapse to ONE `404 {"error":"agent not found"}`, so
//!     the surface is never an existence oracle.
//!  2. AUTHORITY ([`can_edit_agent`]) — only the creator (or system) may proceed; anyone
//!     who can merely *see* the agent gets `403 {"error":"forbidden"}`.
//!
//! The read is edit-gated too, not merely visibility-gated: a raw `mcp.json` can carry
//! secrets (env tokens, HTTP auth headers) and a probe connects to the owner's endpoint
//! WITH those secrets, so a co-workspace teammate who is not the creator must be refused
//! both.
//!
//! ## Transport is borrowed, never owned
//!
//! These faces dispatch through the shared
//! [`AgentProfileMcpDataPlane`](silverhand_backend_core::identity_access::agent_profile_mcp) port — the
//! SAME production instance the internal family uses, constructed once by the
//! composition root and cloned in. This factory takes an already-built `D`; it never
//! constructs one, so mounting cannot be induced into standing up a second data plane.
//! The HTTP wire (probe grammar, outcome projection, fault→status mapping, raw body) is
//! the shared crate-private [`wire`] vocabulary, spoken identically by both families.
//!
//! ## A probe failure is a 200, not an HTTP error
//!
//! A probe that reaches the host but fails to connect/handshake/list tools is a DOMAIN
//! fact: it rides the `200` body as `{ ok: false, phase }`. Only a host-transport fault
//! (the RPC itself could not complete) becomes an HTTP error at the mapped status.
//!
//! ## Error order is not a parity gate
//!
//! For the probe, the framework runs `ValidatedJson` extraction (the body admission)
//! BEFORE the handler body runs the access ladder, so a structural or semantic body
//! fault refuses (`400`/`422`) regardless of the access verdict. This mirrors the
//! ratified `agent_patch` decision (shiki): no anti-enumeration contract depends on
//! access preceding admission — a body-shape `422` is identical whether or not the agent
//! exists, so it leaks nothing. The GET read carries no body and runs the ladder
//! directly.

pub(crate) mod wire;

use axum::{
    Json, Router,
    extract::{Extension, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Serialize;
use silverhand_backend_core::{
    foundation::identity::{AgentId, Principal, UserId},
    identity_access::agent_detail_read::{AgentDetailFacts, AgentDetailReadModel},
    identity_access::agent_profile_mcp::AgentProfileMcpDataPlane,
    identity_access::capability::{
        agent_authority::{AgentAuthority, can_edit_agent},
        agent_view::{VisibleStages, can_view_agent},
    },
};

use crate::foundation::error;
use crate::foundation::wire_admission::ValidatedJson;
use wire::{ProbeRequest, ReadConfigResponse, data_plane_error, probe_outcome_wire};

/// `GET`/`POST` share the config path; the probe is a distinct suffix.
pub const AGENT_PROFILE_MCP_PATH: &str = "/api/agents/{id}/profile/mcp";
/// The live-probe route template.
pub const AGENT_PROFILE_MCP_PROBE_PATH: &str = "/api/agents/{id}/profile/mcp/probe";

/// The anti-enumeration refusal string, pinned to the TypeScript handler.
const AGENT_NOT_FOUND: &str = "agent not found";
/// The authority-refusal string (`c.json({ error: 'forbidden' }, 403)`).
const FORBIDDEN: &str = "forbidden";
/// The unmapped-error body: a bare `text/plain` `500`, never a JSON envelope.
const INTERNAL_ERROR_BODY: &str = "Internal Server Error";

#[derive(Clone)]
struct ProfileMcpState<R, D> {
    reads: R,
    data_plane: D,
}

/// Mount the two public MCP faces generic over the authority read model and the
/// data-plane port. The composition root supplies the real read model and the SAME
/// shared `ProductionAgentProfileMcpDataPlane` the internal family uses (cloned in), and
/// composes the authenticating perimeter that injects `Extension<UserId>` in front.
pub fn router<R, D>(reads: R, data_plane: D) -> Router
where
    R: AgentDetailReadModel + Clone + Send + Sync + 'static,
    D: AgentProfileMcpDataPlane + Clone + Send + Sync + 'static,
{
    Router::new()
        .route(AGENT_PROFILE_MCP_PATH, get(read_config::<R, D>))
        .route(AGENT_PROFILE_MCP_PROBE_PATH, post(probe::<R, D>))
        .with_state(ProfileMcpState { reads, data_plane })
}

/// Run the shared visibility→404 / edit→403 ladder, returning the resolved [`AgentId`]
/// on success or the terminal refusal to short-circuit with. The refusal is a
/// [`Box<Response>`] so the common `Ok` arm stays cheap (`result_large_err`), matching
/// the crate's admission-helper convention. A malformed id names no agent (the same
/// `404` an absent row gets); a read failure or a row resolved for the wrong agent is
/// the lane's safe `500`.
async fn authorize<R>(reads: &R, viewer: UserId, agent_id: String) -> Result<AgentId, Box<Response>>
where
    R: AgentDetailReadModel + Send + Sync,
{
    let principal = Principal::human(viewer);

    let Ok(agent) = AgentId::parse(agent_id) else {
        return Err(Box::new(not_found()));
    };

    let detail = match reads.read_agent_detail(&agent).await {
        Ok(detail) => detail,
        Err(cause) => {
            tracing::error!(
                error_chain =
                    crate::foundation::safe_log_field::safe_display(&(error::describe(&cause))),
                "profile-mcp pre-dispatch read failed"
            );
            return Err(Box::new(internal_error()));
        }
    };

    // A bundle decoded for another agent is a resolver defect, the lane's safe 500.
    if let Some(detail) = detail.as_ref()
        && detail.source() != &agent
    {
        tracing::error!(
            requested = agent.as_str(),
            resolved = detail.source().as_str(),
            "profile-mcp resolved for the wrong agent"
        );
        return Err(Box::new(internal_error()));
    }

    // VISIBILITY (active-only): absence, non-membership, and a non-active stage all
    // collapse to one anti-enumeration 404.
    let decision = can_view_agent(
        &principal,
        &agent,
        detail.as_ref().map(AgentDetailFacts::visibility),
        &VisibleStages::active_only(),
    );
    let Some(detail) = detail.filter(|_| decision.is_allowed()) else {
        return Err(Box::new(not_found()));
    };

    // AUTHORITY: only the creator (or system) may read secrets or trigger a probe; a
    // viewer who is not the creator gets 403.
    let authority = AgentAuthority {
        created_by_user_id: Some(detail.created_by_user_id().clone()),
    };
    if !can_edit_agent(&principal, &agent, Some(&authority)).is_allowed() {
        return Err(Box::new(forbidden()));
    }

    Ok(agent)
}

async fn read_config<R, D>(
    State(state): State<ProfileMcpState<R, D>>,
    Extension(viewer): Extension<UserId>,
    Path(agent_id): Path<String>,
) -> Response
where
    R: AgentDetailReadModel + Send + Sync,
    D: AgentProfileMcpDataPlane + Send + Sync,
{
    let agent = match authorize(&state.reads, viewer, agent_id).await {
        Ok(agent) => agent,
        Err(response) => return *response,
    };
    match state.data_plane.read_config(&agent).await {
        Ok(raw) => Json(ReadConfigResponse { raw }).into_response(),
        Err(fault) => data_plane_error(fault),
    }
}

async fn probe<R, D>(
    State(state): State<ProfileMcpState<R, D>>,
    Extension(viewer): Extension<UserId>,
    Path(agent_id): Path<String>,
    ValidatedJson(request): ValidatedJson<ProbeRequest>,
) -> Response
where
    R: AgentDetailReadModel + Send + Sync,
    D: AgentProfileMcpDataPlane + Send + Sync,
{
    let agent = match authorize(&state.reads, viewer, agent_id).await {
        Ok(agent) => agent,
        Err(response) => return *response,
    };
    let command = request.into_command();
    match state.data_plane.probe(&agent, &command).await {
        // A connect/handshake/tools failure is NOT an HTTP error — it rides the 200 body
        // as `{ ok: false, phase }`. Only a transport fault (the `Err` arm) is.
        Ok(outcome) => Json(probe_outcome_wire(&outcome)).into_response(),
        Err(fault) => data_plane_error(fault),
    }
}

// ===========================================================================
// Static refusal responses.
// ===========================================================================

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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::{Request, header::CONTENT_TYPE},
    };
    use serde_json::{Value, json};
    use silverhand_backend_core::{
        foundation::identity::{AgentId, UserId},
        foundation::persistence::PersistenceFailure,
        identity_access::agent_detail_read::{AgentDetailProjection, AgentDetailReadModel},
        identity_access::agent_lifecycle::MaterializationStatus,
        identity_access::agent_profile_mcp::{
            AgentProfileMcpDataPlane, McpBytesWritten, McpDataPlaneFault, McpDataPlaneFaultCode,
            McpProbeCommand, McpProbeOutcome, McpProbePhase, McpProbeServerInfo, McpProbeTool,
        },
        identity_access::agents::runtime::RuntimeKind,
    };
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };
    use tower::ServiceExt;

    fn user(id: &str) -> UserId {
        UserId::parse(id).expect("user id")
    }

    fn agent(id: &str) -> AgentId {
        AgentId::parse(id).expect("agent id")
    }

    // ---- read-model fake (the authority ladder) ----

    #[derive(Clone)]
    enum ReadOutcome {
        Present(Box<AgentDetailFacts>),
        Absent,
        Fails,
    }

    #[derive(Clone)]
    struct FakeReads {
        outcome: ReadOutcome,
    }

    impl FakeReads {
        fn present(facts: AgentDetailFacts) -> Self {
            Self {
                outcome: ReadOutcome::Present(Box::new(facts)),
            }
        }
        fn absent() -> Self {
            Self {
                outcome: ReadOutcome::Absent,
            }
        }
        fn fails() -> Self {
            Self {
                outcome: ReadOutcome::Fails,
            }
        }
    }

    impl AgentDetailReadModel for FakeReads {
        async fn read_agent_detail(
            &self,
            _agent: &AgentId,
        ) -> Result<Option<AgentDetailFacts>, PersistenceFailure> {
            match &self.outcome {
                ReadOutcome::Present(facts) => Ok(Some(facts.as_ref().clone())),
                ReadOutcome::Absent => Ok(None),
                ReadOutcome::Fails => Err(PersistenceFailure::from_source(std::io::Error::other(
                    "boom",
                ))),
            }
        }
    }

    fn projection() -> AgentDetailProjection {
        AgentDetailProjection {
            name: "agent-one".to_owned(),
            display_name: "Agent One".to_owned(),
            runtime: RuntimeKind::Codex,
            model: Some("gpt-5.6-sol".to_owned()),
            reasoning_effort: Some("ultra".to_owned()),
            fast_mode: Some(true),
            host_id: "h1".to_owned(),
            host_display_label: Some("Host One".to_owned()),
            host_name: Some("host-one".to_owned()),
            description: Some("desc".to_owned()),
            session_scope: "agent".to_owned(),
            avatar_storage_key: None,
            materialization_failed_code: None,
            materialization_failed_at: None,
            deletion_started_at: None,
            created_at: 1000,
            updated_at: 2000,
        }
    }

    /// Creator == viewer, active: passes both gates.
    fn creator_facts() -> AgentDetailFacts {
        AgentDetailFacts::from_source(
            agent("a1"),
            user("viewer-1"),
            [],
            [],
            MaterializationStatus::Active,
            projection(),
        )
    }

    /// A workspace-member arm: `viewer-1` may SEE it but did not create it, so authority
    /// refuses (403).
    fn workspace_only_facts() -> AgentDetailFacts {
        AgentDetailFacts::from_source(
            agent("a1"),
            user("creator-user"),
            [],
            [user("viewer-1")],
            MaterializationStatus::Active,
            projection(),
        )
    }

    /// A caller in no arm at all: creator is someone else, no membership → 404.
    fn stranger_facts() -> AgentDetailFacts {
        AgentDetailFacts::from_source(
            agent("a1"),
            user("someone-else"),
            [],
            [],
            MaterializationStatus::Active,
            projection(),
        )
    }

    /// Creator == viewer but a non-active stage → the same anti-enumeration 404.
    fn non_active_facts() -> AgentDetailFacts {
        AgentDetailFacts::from_source(
            agent("a1"),
            user("viewer-1"),
            [],
            [],
            MaterializationStatus::Failed,
            projection(),
        )
    }

    /// A bundle resolved for a DIFFERENT agent — a resolver defect, the safe 500.
    fn spliced_facts() -> AgentDetailFacts {
        AgentDetailFacts::from_source(
            agent("a2"),
            user("viewer-1"),
            [],
            [],
            MaterializationStatus::Active,
            projection(),
        )
    }

    // ---- data-plane fake (transport) ----

    #[derive(Clone)]
    struct FakeDataPlane {
        inner: Arc<FakeInner>,
    }

    struct FakeInner {
        read: Mutex<Result<Option<String>, McpDataPlaneFault>>,
        probe: Mutex<Result<McpProbeOutcome, McpDataPlaneFault>>,
        consulted: AtomicBool,
        asked_for: Mutex<Vec<String>>,
        probed_command: Mutex<Option<(String, bool)>>,
    }

    impl FakeDataPlane {
        fn new() -> Self {
            Self {
                inner: Arc::new(FakeInner {
                    read: Mutex::new(Ok(None)),
                    probe: Mutex::new(Ok(outcome(true, McpProbePhase::Tools))),
                    consulted: AtomicBool::new(false),
                    asked_for: Mutex::new(Vec::new()),
                    probed_command: Mutex::new(None),
                }),
            }
        }
        fn on_read(self, result: Result<Option<String>, McpDataPlaneFault>) -> Self {
            *self.inner.read.lock().unwrap() = result;
            self
        }
        fn on_probe(self, result: Result<McpProbeOutcome, McpDataPlaneFault>) -> Self {
            *self.inner.probe.lock().unwrap() = result;
            self
        }
        fn consulted(&self) -> bool {
            self.inner.consulted.load(Ordering::SeqCst)
        }
        fn asked_for(&self) -> Vec<String> {
            self.inner.asked_for.lock().unwrap().clone()
        }
        fn probed_command(&self) -> Option<(String, bool)> {
            self.inner.probed_command.lock().unwrap().clone()
        }
        fn record(&self, agent: &AgentId) {
            self.inner.consulted.store(true, Ordering::SeqCst);
            self.inner
                .asked_for
                .lock()
                .unwrap()
                .push(agent.as_str().to_string());
        }
    }

    impl AgentProfileMcpDataPlane for FakeDataPlane {
        async fn read_config(&self, agent: &AgentId) -> Result<Option<String>, McpDataPlaneFault> {
            self.record(agent);
            self.inner.read.lock().unwrap().clone()
        }

        async fn write_config(
            &self,
            agent: &AgentId,
            _raw: &str,
        ) -> Result<McpBytesWritten, McpDataPlaneFault> {
            // Not a public D2 face; recorded so an accidental call is visible.
            self.record(agent);
            Ok(McpBytesWritten::new(0).unwrap())
        }

        async fn probe(
            &self,
            agent: &AgentId,
            command: &McpProbeCommand,
        ) -> Result<McpProbeOutcome, McpDataPlaneFault> {
            self.record(agent);
            *self.inner.probed_command.lock().unwrap() =
                Some((command.server_name.clone(), command.include_tools));
            self.inner.probe.lock().unwrap().clone()
        }
    }

    fn outcome(ok: bool, phase: McpProbePhase) -> McpProbeOutcome {
        McpProbeOutcome {
            ok,
            phase,
            message: None,
            server_info: None,
            tools: None,
        }
    }

    fn fault(code: McpDataPlaneFaultCode) -> McpDataPlaneFault {
        McpDataPlaneFault {
            code,
            message: format!("{code:?} happened"),
        }
    }

    /// Drive one request through the bare router with the human principal injected (as
    /// the perimeter would), isolating the handler from the authenticating layer.
    async fn drive(
        reads: FakeReads,
        data_plane: FakeDataPlane,
        method: &str,
        path: &str,
        with_principal: bool,
        body: Body,
    ) -> Response {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(CONTENT_TYPE, "application/json");
        if with_principal {
            request = request.extension(user("viewer-1"));
        }
        router(reads, data_plane)
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap()
    }

    async fn body_json(response: Response) -> Value {
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    }

    // ================= GET read =================

    #[tokio::test]
    async fn a_present_config_is_returned_to_the_creator() {
        let data_plane = FakeDataPlane::new().on_read(Ok(Some("{\"mcpServers\":{}}".to_string())));
        let response = drive(
            FakeReads::present(creator_facts()),
            data_plane.clone(),
            "GET",
            "/api/agents/a1/profile/mcp",
            true,
            Body::empty(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            body_json(response).await,
            json!({ "raw": "{\"mcpServers\":{}}" })
        );
        assert_eq!(data_plane.asked_for(), vec!["a1".to_string()]);
    }

    #[tokio::test]
    async fn an_absent_config_is_null_not_a_404() {
        let response = drive(
            FakeReads::present(creator_facts()),
            FakeDataPlane::new().on_read(Ok(None)),
            "GET",
            "/api/agents/a1/profile/mcp",
            true,
            Body::empty(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_json(response).await, json!({ "raw": Value::Null }));
    }

    #[tokio::test]
    async fn a_read_transport_fault_is_the_mapped_status_and_envelope() {
        let response = drive(
            FakeReads::present(creator_facts()),
            FakeDataPlane::new().on_read(Err(fault(McpDataPlaneFaultCode::HostAttachPending))),
            "GET",
            "/api/agents/a1/profile/mcp",
            true,
            Body::empty(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            body_json(response).await,
            json!({ "error": "HostAttachPending happened", "code": "host_attach_pending" })
        );
    }

    // ================= the ladder (shared by both faces) =================

    #[tokio::test]
    async fn a_missing_principal_is_a_safe_500_that_never_dispatches() {
        for (method, path, body) in [
            ("GET", "/api/agents/a1/profile/mcp", Body::empty()),
            (
                "POST",
                "/api/agents/a1/profile/mcp/probe",
                Body::from(json!({ "server_name": "srv" }).to_string()),
            ),
        ] {
            let data_plane = FakeDataPlane::new();
            let response = drive(
                FakeReads::present(creator_facts()),
                data_plane.clone(),
                method,
                path,
                false,
                body,
            )
            .await;
            assert_eq!(
                response.status(),
                StatusCode::INTERNAL_SERVER_ERROR,
                "{method} {path} without a principal is a safe 500"
            );
            assert!(
                serde_json::from_slice::<Value>(
                    &to_bytes(response.into_body(), usize::MAX).await.unwrap()
                )
                .is_err(),
                "the composition-fault 500 is a bare text rejection, never JSON"
            );
            assert!(!data_plane.consulted(), "{method} {path} must not dispatch");
        }
    }

    #[tokio::test]
    async fn a_malformed_id_is_the_anti_enumeration_404_and_never_dispatches() {
        let data_plane = FakeDataPlane::new();
        let response = drive(
            FakeReads::absent(),
            data_plane.clone(),
            "GET",
            "/api/agents/%20/profile/mcp",
            true,
            Body::empty(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            body_json(response).await,
            json!({ "error": "agent not found" })
        );
        assert!(!data_plane.consulted());
    }

    #[tokio::test]
    async fn the_access_ladder_refuses_every_non_creator_arm_before_dispatch() {
        // Every arm of the shared visibility→404 / edit→403 ladder, driven through the
        // body-less GET so the ladder is isolated from admission. The anti-enumeration
        // contract IS the collapse: an absent row, a caller outside the ACL, and a
        // non-active stage are ONE indistinguishable `404`. A visible non-creator is the
        // edit gate's `403` (a raw config carries secrets). A read failure and a
        // wrong-agent bundle are the lane's safe, bare-text `500`. In every arm the
        // refusal is terminal BEFORE the data plane is consulted.
        struct Case {
            what: &'static str,
            reads: FakeReads,
            status: StatusCode,
            /// `Some` JSON envelope, or `None` for the bare non-JSON `500`.
            body: Option<Value>,
        }
        let not_found = json!({ "error": "agent not found" });
        let cases = [
            Case {
                what: "an absent row",
                reads: FakeReads::absent(),
                status: StatusCode::NOT_FOUND,
                body: Some(not_found.clone()),
            },
            Case {
                what: "a caller outside the ACL",
                reads: FakeReads::present(stranger_facts()),
                status: StatusCode::NOT_FOUND,
                body: Some(not_found.clone()),
            },
            Case {
                what: "a non-active stage",
                reads: FakeReads::present(non_active_facts()),
                status: StatusCode::NOT_FOUND,
                body: Some(not_found.clone()),
            },
            Case {
                what: "a viewer who can see it but is not the creator (edit gate)",
                reads: FakeReads::present(workspace_only_facts()),
                status: StatusCode::FORBIDDEN,
                body: Some(json!({ "error": "forbidden" })),
            },
            Case {
                what: "a pre-dispatch read failure",
                reads: FakeReads::fails(),
                status: StatusCode::INTERNAL_SERVER_ERROR,
                body: None,
            },
            Case {
                what: "a bundle resolved for another agent",
                reads: FakeReads::present(spliced_facts()),
                status: StatusCode::INTERNAL_SERVER_ERROR,
                body: None,
            },
        ];
        for case in cases {
            let data_plane = FakeDataPlane::new();
            let response = drive(
                case.reads,
                data_plane.clone(),
                "GET",
                "/api/agents/a1/profile/mcp",
                true,
                Body::empty(),
            )
            .await;
            assert_eq!(response.status(), case.status, "{}", case.what);
            assert!(!data_plane.consulted(), "{} must never dispatch", case.what);
            match case.body {
                Some(expected) => {
                    assert_eq!(body_json(response).await, expected, "{}", case.what)
                }
                None => assert!(
                    serde_json::from_slice::<Value>(
                        &to_bytes(response.into_body(), usize::MAX).await.unwrap()
                    )
                    .is_err(),
                    "{} is a bare non-JSON 500",
                    case.what
                ),
            }
        }
    }

    #[tokio::test]
    async fn a_valid_probe_defaults_include_tools_false_and_projects_the_outcome() {
        let data_plane = FakeDataPlane::new().on_probe(Ok(McpProbeOutcome {
            ok: true,
            phase: McpProbePhase::Initialize,
            message: None,
            server_info: Some(McpProbeServerInfo {
                name: Some("srv".to_string()),
                version: None,
            }),
            tools: None,
        }));
        let response = drive(
            FakeReads::present(creator_facts()),
            data_plane.clone(),
            "POST",
            "/api/agents/a1/profile/mcp/probe",
            true,
            Body::from(json!({ "server_name": "srv" }).to_string()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            body_json(response).await,
            json!({ "ok": true, "phase": "initialize", "server_info": { "name": "srv" } })
        );
        assert_eq!(
            data_plane.probed_command(),
            Some(("srv".to_string(), false))
        );
    }

    #[tokio::test]
    async fn a_probe_with_tools_projects_the_tool_list_and_snake_case_phase() {
        let data_plane = FakeDataPlane::new().on_probe(Ok(McpProbeOutcome {
            ok: true,
            phase: McpProbePhase::Tools,
            message: None,
            server_info: None,
            tools: Some(vec![
                McpProbeTool {
                    name: "t1".to_string(),
                    description: Some("does a thing".to_string()),
                },
                McpProbeTool {
                    name: "t2".to_string(),
                    description: None,
                },
            ]),
        }));
        let response = drive(
            FakeReads::present(creator_facts()),
            data_plane.clone(),
            "POST",
            "/api/agents/a1/profile/mcp/probe",
            true,
            Body::from(json!({ "server_name": "srv", "include_tools": true }).to_string()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            body_json(response).await,
            json!({
                "ok": true,
                "phase": "tools",
                "tools": [
                    { "name": "t1", "description": "does a thing" },
                    { "name": "t2" }
                ]
            })
        );
        assert_eq!(data_plane.probed_command(), Some(("srv".to_string(), true)));
    }

    #[tokio::test]
    async fn a_handshake_failure_is_a_200_body_not_an_http_error() {
        let data_plane = FakeDataPlane::new().on_probe(Ok(McpProbeOutcome {
            ok: false,
            phase: McpProbePhase::Connect,
            message: Some("could not connect".to_string()),
            server_info: None,
            tools: None,
        }));
        let response = drive(
            FakeReads::present(creator_facts()),
            data_plane,
            "POST",
            "/api/agents/a1/profile/mcp/probe",
            true,
            Body::from(json!({ "server_name": "srv" }).to_string()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            body_json(response).await,
            json!({ "ok": false, "phase": "connect", "message": "could not connect" })
        );
    }

    #[tokio::test]
    async fn a_probe_transport_fault_is_the_mapped_status() {
        let data_plane =
            FakeDataPlane::new().on_probe(Err(fault(McpDataPlaneFaultCode::DaemonTimeout)));
        let response = drive(
            FakeReads::present(creator_facts()),
            data_plane,
            "POST",
            "/api/agents/a1/profile/mcp/probe",
            true,
            Body::from(json!({ "server_name": "srv" }).to_string()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(body_json(response).await["code"], "daemon_timeout");
    }

    #[tokio::test]
    async fn a_probe_edit_gate_refuses_a_non_creator_before_dispatch() {
        let data_plane = FakeDataPlane::new();
        let response = drive(
            FakeReads::present(workspace_only_facts()),
            data_plane.clone(),
            "POST",
            "/api/agents/a1/profile/mcp/probe",
            true,
            Body::from(json!({ "server_name": "srv" }).to_string()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(
            !data_plane.consulted(),
            "a non-creator probe never reaches the data plane"
        );
    }

    #[tokio::test]
    async fn an_empty_server_name_is_the_semantic_422_before_dispatch() {
        let data_plane = FakeDataPlane::new();
        let response = drive(
            FakeReads::present(creator_facts()),
            data_plane.clone(),
            "POST",
            "/api/agents/a1/profile/mcp/probe",
            true,
            Body::from(json!({ "server_name": "" }).to_string()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            body_json(response).await,
            json!({ "error": "invalid input" })
        );
        assert!(!data_plane.consulted());
    }

    #[tokio::test]
    async fn a_whitespace_only_server_name_is_admitted_and_bound_verbatim() {
        let data_plane = FakeDataPlane::new();
        let response = drive(
            FakeReads::present(creator_facts()),
            data_plane.clone(),
            "POST",
            "/api/agents/a1/profile/mcp/probe",
            true,
            Body::from(json!({ "server_name": " " }).to_string()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(data_plane.probed_command(), Some((" ".to_string(), false)));
    }

    #[tokio::test]
    async fn a_duplicate_server_name_is_natively_rejected_before_dispatch() {
        let data_plane = FakeDataPlane::new();
        let response = drive(
            FakeReads::present(creator_facts()),
            data_plane.clone(),
            "POST",
            "/api/agents/a1/profile/mcp/probe",
            true,
            Body::from(r#"{"server_name":"first","server_name":"second"}"#),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(!data_plane.consulted());
    }

    #[tokio::test]
    async fn a_structurally_invalid_probe_is_a_native_rejection_before_dispatch() {
        let semantic = json!({ "error": "invalid input" });
        let native_422 = [
            json!({}).to_string(),
            json!({ "server_name": "srv", "extra": 1 }).to_string(),
            json!({ "server_name": 7 }).to_string(),
            json!({ "server_name": "srv", "include_tools": null }).to_string(),
            json!({ "server_name": "srv", "include_tools": "yes" }).to_string(),
        ];
        for body in native_422 {
            let data_plane = FakeDataPlane::new();
            let response = drive(
                FakeReads::present(creator_facts()),
                data_plane.clone(),
                "POST",
                "/api/agents/a1/profile/mcp/probe",
                true,
                Body::from(body.clone()),
            )
            .await;
            assert_eq!(
                response.status(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "body {body:?} is a native 422"
            );
            assert_ne!(
                body_json(response).await,
                semantic,
                "body {body:?} is a native rejection, not the semantic envelope"
            );
            assert!(!data_plane.consulted(), "body {body:?} never dispatches");
        }

        // Malformed JSON is the native 400 syntax rejection.
        let data_plane = FakeDataPlane::new();
        let response = drive(
            FakeReads::present(creator_facts()),
            data_plane.clone(),
            "POST",
            "/api/agents/a1/profile/mcp/probe",
            true,
            Body::from("not json"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(!data_plane.consulted());
    }

    #[tokio::test]
    async fn a_value_shaped_probe_array_is_a_native_reject_and_never_dispatches() {
        // The sharp accepted-set falsifier: `["srv", true]` is a two-element array whose
        // elements are VALID values for `ProbeRequest`'s fields IN ORDER. A bare serde
        // derive would bind them positionally and DISPATCH — the array-shaped bypass. The
        // frozen `ValidatedJson` decodes through the shared `ObjectOnly` floor (#2246), so
        // a top-level array is a native structural reject BEFORE decode: a 422, never the
        // semantic envelope, and the data plane is never consulted.
        let data_plane = FakeDataPlane::new();
        let response = drive(
            FakeReads::present(creator_facts()),
            data_plane.clone(),
            "POST",
            "/api/agents/a1/profile/mcp/probe",
            true,
            Body::from(json!(["srv", true]).to_string()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_ne!(
            body_json(response).await,
            json!({ "error": "invalid input" }),
            "an array-shaped body is a native structural reject, not the semantic refusal"
        );
        assert!(!data_plane.consulted());
    }

    #[tokio::test]
    async fn an_absent_row_on_probe_is_the_404_after_body_admission() {
        // A well-formed body clears extraction; the ladder then 404s the absent row.
        let data_plane = FakeDataPlane::new();
        let response = drive(
            FakeReads::absent(),
            data_plane.clone(),
            "POST",
            "/api/agents/a1/profile/mcp/probe",
            true,
            Body::from(json!({ "server_name": "srv" }).to_string()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            body_json(response).await,
            json!({ "error": "agent not found" })
        );
        assert!(!data_plane.consulted());
    }
}
