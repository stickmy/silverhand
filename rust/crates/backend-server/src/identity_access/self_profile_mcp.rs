//! The internal agent-profile MCP routes: read/replace an agent's MCP server
//! configuration and run a one-shot live probe against one of its servers.
//!
//! ## Three self-pinned routes, no DB
//!
//! `GET`/`POST /internal/agents/{aid}/profile/mcp` and
//! `POST /internal/agents/{aid}/profile/mcp/probe` never touch the database: they
//! dispatch to the agent's owning host over a correlated RPC data-plane
//! ([`AgentProfileMcpDataPlane`]). Like every route in this module they sit behind
//! the runtime-credential layer, which pins `{aid}` to the token's own agent — so
//! the product's `aid required` (400), `agent not found` (404) and `canEditAgent`
//! (403) pre-checks all collapse into the layer's indistinguishable `401`, and the
//! handler trusts `credential.agent()` and takes no path param.
//!
//! ## What this lane owns, and what it shares
//!
//! This checkpoint owns the internal consumer surface: the self-pinned routing, the
//! credential-composition guard, and the POST config-replace path (its `mcp.json`
//! validation via [`parse_mcp_config`] and its write response). The HTTP wire it
//! shares with the public management faces — the probe request grammar and its one
//! stateless rule, the probe-outcome projection, the data-plane fault→HTTP mapping,
//! and the raw-config read body — lives in the single crate-private
//! [`wire`](crate::identity_access::public_profile_mcp::wire) vocabulary both routers consume;
//! neither re-declares it. It does NOT own the transport: the production
//! [`AgentProfileMcpDataPlane`] adapter is #2231's, reusing #2224's correlated-RPC
//! data-plane root, wired at the backend-app #2234 CP7b mount. Deterministic fakes
//! prove the handler orchestration but not mounting or effect-completeness; the
//! backend-app composition gate proves those over the real daemon wire.
//!
//! ## A probe failure is a 200, not an HTTP error
//!
//! A probe that reaches the host but fails to connect/handshake/list tools is a
//! DOMAIN fact: it rides the `200` body as `{ ok: false, phase }`. Only a
//! host-transport fault (the RPC itself could not complete) becomes an HTTP error
//! carrying `{ error, code }` at the mapped status.

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Extension, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Serialize;
use silverhand_backend_core::{
    host_runtime::runtime_credential::RuntimePrincipal,
    identity_access::agent_profile_mcp::AgentProfileMcpDataPlane,
    identity_access::mcp_config::parse_mcp_config,
};

use crate::foundation::wire_admission::ValidatedJson;
use crate::identity_access::public_profile_mcp::wire::{
    ProbeRequest, ReadConfigResponse, data_plane_error, probe_outcome_wire,
};

/// The product route template for reading/replacing the config. `{aid}` is consumed
/// by the credential layer, which self-pins it.
pub const PROFILE_MCP_PATH: &str = "/internal/agents/{aid}/profile/mcp";

/// The product route template for the live probe.
pub const PROFILE_MCP_PROBE_PATH: &str = "/internal/agents/{aid}/profile/mcp/probe";

/// The unmapped-error body: a bare `text/plain` `500`, never a JSON envelope.
const INTERNAL_ERROR_BODY: &str = "Internal Server Error";

/// Mount the three MCP routes generic over the data-plane port, so tests drive them
/// with a driver-free fake and the composition root wires the #2231 adapter AND the
/// runtime-credential layer that self-pins `{aid}`. This builder yields the bare
/// routes; the composition root (backend-app, #2234 CP7b) registers them and composes
/// that layer in front.
pub fn profile_mcp_router<D>(data_plane: D) -> Router
where
    D: AgentProfileMcpDataPlane + Clone + Send + Sync + 'static,
{
    Router::new()
        .route(
            PROFILE_MCP_PATH,
            get(read_config::<D>).post(write_config::<D>),
        )
        .route(PROFILE_MCP_PROBE_PATH, post(probe::<D>))
        .with_state(data_plane)
}

/// The composition-fault `500`: a route reached without its credential layer must
/// not dispatch. Never a `401` — that verdict is the layer's alone.
fn missing_credential(route: &str) -> Response {
    tracing::error!(
        route,
        "internal MCP route reached without its runtime credential"
    );
    (StatusCode::INTERNAL_SERVER_ERROR, INTERNAL_ERROR_BODY).into_response()
}

// ===========================================================================
// GET — read the config.
// ===========================================================================

async fn read_config<D>(
    credential: Option<Extension<RuntimePrincipal>>,
    State(data_plane): State<D>,
) -> Response
where
    D: AgentProfileMcpDataPlane,
{
    let Some(Extension(credential)) = credential else {
        return missing_credential(PROFILE_MCP_PATH);
    };
    match data_plane.read_config(credential.agent()).await {
        Ok(raw) => Json(ReadConfigResponse { raw }).into_response(),
        Err(fault) => data_plane_error(fault),
    }
}

// ===========================================================================
// POST — replace the config.
// ===========================================================================

/// `{ ok: true, bytes_written: number }` — the daemon's byte count for the written
/// `mcp.json`. `bytes_written` is an `i64` sourced from the port's
/// [`McpBytesWritten`](silverhand_backend_core::identity_access::agent_profile_mcp::McpBytesWritten),
/// which has already guaranteed a JS-safe value, so the wire number is always exactly
/// representable by a JavaScript consumer. (Internal-only: the public management faces
/// carry no config write.)
#[derive(Serialize)]
struct WriteConfigResponse {
    ok: bool,
    bytes_written: i64,
}

async fn write_config<D>(
    credential: Option<Extension<RuntimePrincipal>>,
    State(data_plane): State<D>,
    body: Bytes,
) -> Response
where
    D: AgentProfileMcpDataPlane,
{
    let Some(Extension(credential)) = credential else {
        return missing_credential(PROFILE_MCP_PATH);
    };

    // The body is read as verbatim text (as the product's `c.req.text()` does): the
    // user's own formatting is what gets persisted. A non-UTF-8 body decodes lossily
    // and simply fails the parse below as invalid config, never a transport error.
    let raw = String::from_utf8_lossy(&body).into_owned();

    // The backend is the PRIMARY validation authority: a reject never crosses the
    // wire to the host. The error string is the product-authored envelope.
    if let Err(message) = parse_mcp_config(&raw) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": message, "code": "invalid_config" })),
        )
            .into_response();
    }

    // The VERBATIM raw is persisted (not a re-serialization of the parsed config).
    match data_plane.write_config(credential.agent(), &raw).await {
        Ok(bytes_written) => Json(WriteConfigResponse {
            ok: true,
            bytes_written: bytes_written.get(),
        })
        .into_response(),
        Err(fault) => data_plane_error(fault),
    }
}

// ===========================================================================
// POST probe — a one-shot live probe.
// ===========================================================================

async fn probe<D>(
    credential: Option<Extension<RuntimePrincipal>>,
    State(data_plane): State<D>,
    ValidatedJson(request): ValidatedJson<ProbeRequest>,
) -> Response
where
    D: AgentProfileMcpDataPlane,
{
    let Some(Extension(credential)) = credential else {
        return missing_credential(PROFILE_MCP_PROBE_PATH);
    };

    let command = request.into_command();

    match data_plane.probe(credential.agent(), &command).await {
        // A connect/handshake/tools failure is NOT an HTTP error — it rides the 200
        // body as `{ ok: false, phase }`. Only a transport fault (the `Err` arm) is.
        Ok(outcome) => Json(probe_outcome_wire(&outcome)).into_response(),
        Err(fault) => data_plane_error(fault),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::{
            Request,
            header::{AUTHORIZATION, CONTENT_TYPE},
        },
    };
    use serde_json::{Value, json};
    use silverhand_backend_core::{
        foundation::identity::{AgentId, DaemonInstanceId},
        foundation::persistence::PersistenceFailure,
        host_runtime::runtime_credential::{
            RuntimeCustody, RuntimeGeneration, RuntimePrincipal, RuntimeScope,
        },
        identity_access::agent_profile_mcp::{
            McpBytesWritten, McpDataPlaneFault, McpDataPlaneFaultCode, McpProbeCommand,
            McpProbeOutcome, McpProbePhase, McpProbeServerInfo, McpProbeTool,
        },
    };
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };
    use tower::ServiceExt;

    use crate::host_runtime::runtime_credential::{
        AuthenticateRuntimeCredential, RuntimeBearer, require_runtime_credential,
    };

    /// The scripted outcome each data-plane method returns, plus a record of whether
    /// — and for which agent — it was consulted, so a test can prove the handler
    /// dispatches for the pinned self and only after admission.
    #[derive(Clone)]
    struct FakeDataPlane {
        inner: Arc<FakeInner>,
    }

    struct FakeInner {
        read: Mutex<Result<Option<String>, McpDataPlaneFault>>,
        write: Mutex<Result<McpBytesWritten, McpDataPlaneFault>>,
        probe: Mutex<Result<McpProbeOutcome, McpDataPlaneFault>>,
        consulted: AtomicBool,
        asked_for: Mutex<Vec<String>>,
        wrote_raw: Mutex<Option<String>>,
        probed_command: Mutex<Option<(String, bool)>>,
    }

    impl FakeDataPlane {
        fn new() -> Self {
            Self {
                inner: Arc::new(FakeInner {
                    read: Mutex::new(Ok(None)),
                    write: Mutex::new(Ok(McpBytesWritten::new(0).expect("0 is representable"))),
                    probe: Mutex::new(Ok(outcome(true, McpProbePhase::Tools))),
                    consulted: AtomicBool::new(false),
                    asked_for: Mutex::new(Vec::new()),
                    wrote_raw: Mutex::new(None),
                    probed_command: Mutex::new(None),
                }),
            }
        }

        fn on_read(self, result: Result<Option<String>, McpDataPlaneFault>) -> Self {
            *self.inner.read.lock().unwrap() = result;
            self
        }

        fn on_write(self, result: Result<McpBytesWritten, McpDataPlaneFault>) -> Self {
            *self.inner.write.lock().unwrap() = result;
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

        fn wrote_raw(&self) -> Option<String> {
            self.inner.wrote_raw.lock().unwrap().clone()
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
            raw: &str,
        ) -> Result<McpBytesWritten, McpDataPlaneFault> {
            self.record(agent);
            *self.inner.wrote_raw.lock().unwrap() = Some(raw.to_string());
            self.inner.write.lock().unwrap().clone()
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

    fn agent(id: &str) -> AgentId {
        AgentId::parse(id).expect("agent id")
    }

    fn principal(id: &str) -> RuntimePrincipal {
        RuntimePrincipal::for_scope(
            RuntimeScope::AgentSlot { agent: agent(id) },
            RuntimeCustody::new(
                RuntimeGeneration::new(1).unwrap(),
                DaemonInstanceId::parse("daemon-1").unwrap(),
            ),
        )
    }

    /// Drive a request against the bare router with the principal already injected
    /// (as the layer would), isolating the handler from the credential layer.
    ///
    /// Every request carries `Content-Type: application/json`: the probe route now
    /// admits through the frozen `ValidatedJson` extractor, which `415`s a body with no
    /// declared JSON type. The GET/config-write routes ignore the header (GET has no
    /// body; the config write reads raw `Bytes` verbatim), so tagging every body — all
    /// of which are JSON payloads to this router — is uniform and behavior-neutral.
    async fn injected(
        data_plane: FakeDataPlane,
        method: &str,
        path: &str,
        credential: Option<RuntimePrincipal>,
        body: Body,
    ) -> Response {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(CONTENT_TYPE, "application/json");
        if let Some(credential) = credential {
            request = request.extension(credential);
        }
        profile_mcp_router(data_plane)
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap()
    }

    async fn body_json(response: Response) -> Value {
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    }

    // ---- GET read ----

    #[tokio::test]
    async fn a_present_config_is_returned_and_the_pinned_self_is_read() {
        let data_plane = FakeDataPlane::new().on_read(Ok(Some("{\"mcpServers\":{}}".to_string())));
        let response = injected(
            data_plane.clone(),
            "GET",
            "/internal/agents/a1/profile/mcp",
            Some(principal("a1")),
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
        let data_plane = FakeDataPlane::new().on_read(Ok(None));
        let response = injected(
            data_plane,
            "GET",
            "/internal/agents/a1/profile/mcp",
            Some(principal("a1")),
            Body::empty(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_json(response).await, json!({ "raw": Value::Null }));
    }

    #[tokio::test]
    async fn a_read_transport_fault_is_the_mapped_status_and_envelope() {
        let data_plane =
            FakeDataPlane::new().on_read(Err(fault(McpDataPlaneFaultCode::HostAttachPending)));
        let response = injected(
            data_plane,
            "GET",
            "/internal/agents/a1/profile/mcp",
            Some(principal("a1")),
            Body::empty(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            body_json(response).await,
            json!({ "error": "HostAttachPending happened", "code": "host_attach_pending" })
        );
    }

    // ---- POST write ----

    #[tokio::test]
    async fn a_valid_config_is_written_verbatim() {
        let data_plane = FakeDataPlane::new().on_write(Ok(McpBytesWritten::new(42).unwrap()));
        // Deliberately idiosyncratic whitespace: it must be persisted verbatim.
        let raw = "{\n  \"mcpServers\": {\n    \"a\": { \"command\": \"x\" }\n  }\n}";
        let response = injected(
            data_plane.clone(),
            "POST",
            "/internal/agents/a1/profile/mcp",
            Some(principal("a1")),
            Body::from(raw),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            body_json(response).await,
            json!({ "ok": true, "bytes_written": 42 })
        );
        assert_eq!(data_plane.wrote_raw().as_deref(), Some(raw));
        assert_eq!(data_plane.asked_for(), vec!["a1".to_string()]);
    }

    #[tokio::test]
    async fn the_js_safe_max_byte_count_serializes_as_an_exact_number() {
        // The port already refuses anything past JsSafeInteger::MAX at construction
        // (proven in backend-core), so the largest value that can reach the wire is
        // MAX itself; assert it renders as the exact integer a JS consumer preserves.
        let max: i64 = 9_007_199_254_740_991;
        let data_plane =
            FakeDataPlane::new().on_write(Ok(McpBytesWritten::new(max as u64).unwrap()));
        let response = injected(
            data_plane,
            "POST",
            "/internal/agents/a1/profile/mcp",
            Some(principal("a1")),
            Body::from("{ \"mcpServers\": {} }"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            body_json(response).await,
            json!({ "ok": true, "bytes_written": max })
        );
    }

    #[tokio::test]
    async fn an_invalid_config_is_a_400_before_any_dispatch() {
        let data_plane = FakeDataPlane::new();
        let response = injected(
            data_plane.clone(),
            "POST",
            "/internal/agents/a1/profile/mcp",
            Some(principal("a1")),
            Body::from("{ not json"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = body_json(response).await;
        assert_eq!(body["code"], "invalid_config");
        assert!(body["error"].as_str().unwrap().starts_with("invalid JSON"));
        // Validation precedes dispatch: the data-plane was never consulted.
        assert!(!data_plane.consulted());
    }

    #[tokio::test]
    async fn an_empty_body_is_the_empty_config_and_is_written() {
        // parse_mcp_config admits an empty/whitespace body as the empty config, so a
        // blank POST is a valid clear-to-empty, dispatched verbatim.
        let data_plane = FakeDataPlane::new().on_write(Ok(McpBytesWritten::new(0).unwrap()));
        let response = injected(
            data_plane.clone(),
            "POST",
            "/internal/agents/a1/profile/mcp",
            Some(principal("a1")),
            Body::from("   "),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(data_plane.wrote_raw().as_deref(), Some("   "));
    }

    #[tokio::test]
    async fn a_write_daemon_fault_is_the_mapped_status() {
        let data_plane =
            FakeDataPlane::new().on_write(Err(fault(McpDataPlaneFaultCode::AgentDeleting)));
        let response = injected(
            data_plane,
            "POST",
            "/internal/agents/a1/profile/mcp",
            Some(principal("a1")),
            Body::from("{\"mcpServers\":{}}"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(body_json(response).await["code"], "agent_deleting");
    }

    // ---- POST probe ----

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
        let response = injected(
            data_plane.clone(),
            "POST",
            "/internal/agents/a1/profile/mcp/probe",
            Some(principal("a1")),
            Body::from(json!({ "server_name": "srv" }).to_string()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        // include_tools defaulted to false; absent optionals are omitted, and
        // server_info emits only its present field.
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
        let response = injected(
            data_plane.clone(),
            "POST",
            "/internal/agents/a1/profile/mcp/probe",
            Some(principal("a1")),
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
        let response = injected(
            data_plane,
            "POST",
            "/internal/agents/a1/profile/mcp/probe",
            Some(principal("a1")),
            Body::from(json!({ "server_name": "srv" }).to_string()),
        )
        .await;
        // A reached-but-failed probe is a domain fact on the 200, never an HTTP error.
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
        let response = injected(
            data_plane,
            "POST",
            "/internal/agents/a1/profile/mcp/probe",
            Some(principal("a1")),
            Body::from(json!({ "server_name": "srv" }).to_string()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(body_json(response).await["code"], "daemon_timeout");
    }

    #[tokio::test]
    async fn a_duplicate_server_name_is_natively_rejected() {
        // The pre-migration handler parsed to `Value` first to reproduce JSON.parse's
        // last-wins and admitted the LAST `server_name`. Under the frozen `ValidatedJson`
        // model the derived `Deserialize` REJECTS a duplicate known field as a native
        // `422` before the handler runs — no dispatch. The raw JSON is hand-written
        // because `json!` cannot express a duplicate key.
        let data_plane = FakeDataPlane::new();
        let response = injected(
            data_plane.clone(),
            "POST",
            "/internal/agents/a1/profile/mcp/probe",
            Some(principal("a1")),
            Body::from(r#"{"server_name":"first","server_name":"second"}"#),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(!data_plane.consulted(), "a duplicate key never dispatches");
    }

    #[tokio::test]
    async fn a_structurally_invalid_probe_is_a_native_rejection_before_dispatch() {
        // Structural faults ride axum's own `Json` rejection, forwarded verbatim: a
        // malformed body is a `400`; a missing/unknown/duplicate key, a wrong-typed
        // field, or a present `include_tools: null` is a `422`. None reaches the port,
        // and none carries the semantic `{ "error": "invalid input" }` envelope.
        let semantic = json!({ "error": "invalid input" });
        let native_422 = [
            json!({}).to_string(),                   // missing server_name
            json!({ "server_name": 7 }).to_string(), // wrong-typed name
            json!({ "server_name": "srv", "include_tools": null }).to_string(), // present null
            json!({ "server_name": "srv", "include_tools": "yes" }).to_string(), // wrong-typed flag
        ];
        for body in native_422 {
            let data_plane = FakeDataPlane::new();
            let response = injected(
                data_plane.clone(),
                "POST",
                "/internal/agents/a1/profile/mcp/probe",
                Some(principal("a1")),
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

        // Malformed JSON is the native `400` syntax rejection.
        let data_plane = FakeDataPlane::new();
        let response = injected(
            data_plane.clone(),
            "POST",
            "/internal/agents/a1/profile/mcp/probe",
            Some(principal("a1")),
            Body::from("not json"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(!data_plane.consulted(), "malformed JSON never dispatches");
    }

    #[tokio::test]
    async fn an_empty_server_name_is_the_semantic_invalid_input_before_dispatch() {
        // A present-but-empty `server_name` decodes fine, then fails the `garde` rule:
        // the frozen semantic refusal is `422 { "error": "invalid input" }` with NO
        // `code` (the pre-migration `code: "invalid_input"` discriminator is gone) and
        // no `issues[]`. It never dispatches.
        let data_plane = FakeDataPlane::new();
        let response = injected(
            data_plane.clone(),
            "POST",
            "/internal/agents/a1/profile/mcp/probe",
            Some(principal("a1")),
            Body::from(json!({ "server_name": "" }).to_string()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            body_json(response).await,
            json!({ "error": "invalid input" })
        );
        assert!(!data_plane.consulted(), "an empty name never dispatches");
    }

    #[tokio::test]
    async fn a_whitespace_only_server_name_is_admitted_and_bound_verbatim() {
        // `server_name` is a RAW `!is_empty()` check — not trimmed — so a whitespace-only
        // name passes admission and reaches the port verbatim, matching the product's
        // `z.string().min(1)` (a character count). include_tools is absent → false.
        let data_plane = FakeDataPlane::new();
        let response = injected(
            data_plane.clone(),
            "POST",
            "/internal/agents/a1/profile/mcp/probe",
            Some(principal("a1")),
            Body::from(json!({ "server_name": " " }).to_string()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(data_plane.probed_command(), Some((" ".to_string(), false)));
    }

    #[tokio::test]
    async fn a_value_shaped_probe_array_is_a_native_reject_and_never_dispatches() {
        // The sharp accepted-set falsifier: `["srv", true]` is a two-element array whose
        // elements are VALID values for `ProbeRequest`'s fields IN ORDER (`server_name:
        // String`, `include_tools: bool`). A bare serde derive would bind them positionally
        // (`server_name = "srv"`, `include_tools = true`), pass `garde`, and DISPATCH — the
        // exact array-shaped-body bypass. The frozen `ValidatedJson` now decodes through the
        // shared `ObjectOnly` object floor (#2246), so a top-level array is a native
        // structural reject BEFORE decode: a `422`, never the semantic `{ "error":
        // "invalid input" }` envelope, and the data-plane is never consulted. Not a
        // type-mismatched array like `[1, 2]` — this one would otherwise decode cleanly.
        let data_plane = FakeDataPlane::new();
        let response = injected(
            data_plane.clone(),
            "POST",
            "/internal/agents/a1/profile/mcp/probe",
            Some(principal("a1")),
            Body::from(json!(["srv", true]).to_string()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_ne!(
            body_json(response).await,
            json!({ "error": "invalid input" }),
            "an array-shaped body is a native structural reject, not the semantic refusal"
        );
        assert!(
            !data_plane.consulted(),
            "an array-shaped body never reaches the data-plane"
        );
    }

    // ---- composition + self-pin ----

    #[tokio::test]
    async fn an_absent_credential_is_a_safe_500_and_never_dispatches() {
        for (method, path, body) in [
            ("GET", "/internal/agents/a1/profile/mcp", Body::empty()),
            (
                "POST",
                "/internal/agents/a1/profile/mcp",
                Body::from("{\"mcpServers\":{}}"),
            ),
            (
                "POST",
                "/internal/agents/a1/profile/mcp/probe",
                Body::from(json!({ "server_name": "srv" }).to_string()),
            ),
        ] {
            let data_plane = FakeDataPlane::new();
            let response = injected(data_plane.clone(), method, path, None, body).await;
            assert_eq!(
                response.status(),
                StatusCode::INTERNAL_SERVER_ERROR,
                "{method} {path} without a credential is a safe 500"
            );
            assert!(
                !data_plane.consulted(),
                "{method} {path} must not dispatch without a credential"
            );
        }
    }

    /// A faithful stand-in for the runtime-credential authority: it self-pins,
    /// admitting a request ONLY when the route's `{aid}` equals its one enrolled
    /// agent. Proves the WIRING — the layer is in front of the routes and runs
    /// before the handler.
    #[derive(Clone)]
    struct PinnedAuthority {
        enrolled: AgentId,
    }

    impl AuthenticateRuntimeCredential for PinnedAuthority {
        async fn authenticate_runtime(
            &self,
            _bearer: RuntimeBearer,
            route_agent: AgentId,
        ) -> Result<Option<RuntimePrincipal>, PersistenceFailure> {
            if route_agent == self.enrolled {
                Ok(Some(principal(self.enrolled.as_str())))
            } else {
                Ok(None)
            }
        }
    }

    async fn behind_layer(data_plane: FakeDataPlane, path_aid: &str) -> Response {
        let authority = PinnedAuthority {
            enrolled: agent("a1"),
        };
        let router = require_runtime_credential(profile_mcp_router(data_plane), authority);
        let request = Request::builder()
            .method("GET")
            .uri(format!("/internal/agents/{path_aid}/profile/mcp"))
            .header(AUTHORIZATION, "Bearer test-secret")
            .body(Body::empty())
            .unwrap();
        router.oneshot(request).await.unwrap()
    }

    #[tokio::test]
    async fn a_non_self_aid_is_401_before_the_data_plane() {
        let data_plane = FakeDataPlane::new().on_read(Ok(None));
        let response = behind_layer(data_plane.clone(), "someone-else").await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(
            !data_plane.consulted(),
            "a non-self caller must never reach the data-plane"
        );
    }

    #[tokio::test]
    async fn the_pinned_self_reaches_the_data_plane() {
        let data_plane = FakeDataPlane::new().on_read(Ok(None));
        let response = behind_layer(data_plane.clone(), "a1").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(data_plane.consulted());
        assert_eq!(data_plane.asked_for(), vec!["a1".to_string()]);
    }
}
