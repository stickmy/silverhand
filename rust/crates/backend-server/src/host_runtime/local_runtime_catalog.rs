//! `GET /api/runtimes` — which agent runtimes the machine THIS process runs on has.
//!
//! Whether there is a machine to ask about at all is a DEPLOYMENT fact, and it is
//! decided ONCE, at composition, by which router is mounted — not per request:
//!
//!  - The embedded profile REQUIRES a co-located daemon host, so under it the machine
//!    this process runs on is a machine agents run on. It mounts [`detecting_router`],
//!    which holds the process's one runtime detector and projects a detection pass.
//!    (That the co-located host itself is a migration residual does not change this
//!    router's contract: the embedded profile is where a detector belongs.)
//!  - A standalone backend is control-plane only: it has no co-located host, so the
//!    machine it runs on is not one any agent runs on. It mounts
//!    [`control_plane_router`], which has no detector to hold and can only refuse with
//!    `503`. It cannot shell out to look for binaries it would have no use for, because
//!    it was never handed anything to shell out WITH.
//!
//! This is a structural port of the product's `if (standaloneMode(ctx)) return
//! hostRuntimeDisabledResponse(...)` guard. The control-plane refusal is the same
//! `503`, but instead of a runtime branch inside one handler — which would require the
//! detector to be present in BOTH deployments and rely on the branch never being
//! mis-taken — the two deployments are two routers, and the standalone composition
//! never constructs a detector in the first place. A control-plane server that probed
//! its own machine is not a bug this route can commit: there is no detector on the
//! `control_plane_router` to probe.
//!
//! The detection pass cannot fail: every way it could go wrong already has an arm, so
//! this route has no `500` path and a broken probe cannot take down the surface that
//! reports it.
//!
//! ## The detector is a process-lifetime singleton, not a route dependency
//!
//! One pass spawns a process per offered runtime, so the product caches a pass for
//! thirty seconds. That cache is a property of the machine being asked about, not of
//! any request or any route, so it lives in ONE component the composition root builds
//! once ([`silverhand_backend_filesystem::host_runtime::runtime_detection::MachineRuntimeDetector`])
//! and [`detecting_router`] merely receives. A cache built here — per factory, per
//! request, or as a second layer in front of the port — would either duplicate the
//! window or reset it, and either way the thirty seconds would stop meaning what it
//! says.
//!
//! ## This is the LOCAL machine, never a daemon's
//!
//! `host_runtime::host_control::runtime_catalog` answers the same question about a REMOTE machine, by
//! asking its daemon over the control channel, and its rows are bound to a
//! `(host, daemon instance)` coordinate. This face has no coordinate to bind: the
//! answer is about the process's own filesystem. The two must not share an
//! implementation — one machine's catalogue rendered as another's is exactly the
//! confusion the coordinate binding exists to prevent.
//!
//! ## The wire keeps a narrower vocabulary than the detection does
//!
//! `RuntimeCatalogResponseSchema` carries `detected_at`; this route's registrar sends
//! only `{ runtimes }`, because the field would tell a client the age of a cache entry
//! it cannot refresh. The port is likewise `detected_at`-free: a stamp nobody may read
//! is not a fact worth carrying to the boundary.
//!
//! ## Authentication
//!
//! Unlike the other three root faces, this one is NOT anonymous: in the product it sits
//! behind the bearer middleware (`app.use('/api/*')`) with no allowlist entry, so an
//! anonymous request is refused `401` before reaching the handler. The composition
//! mounts both routers behind the human-session perimeter accordingly; this module owns
//! only the answer a request receives once admitted.

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Serialize;
use silverhand_backend_core::identity_access::agents::runtime_catalog::{
    DetectedRuntime, RuntimeCatalogDetector, RuntimeDetection,
};

pub const RUNTIMES_PATH: &str = "/api/runtimes";

/// The standalone refusal, pinned to `hostRuntimeDisabledResponse(c, 'runtime
/// detection', 503)`. The message is templated in the product over the action name;
/// this route is its only caller in this crate, so the rendered sentence is spelled
/// once here rather than behind a helper with one call site.
const RUNTIME_DETECTION_DISABLED: &str = "runtime detection requires a daemon-host \
runtime; standalone server mode is control-plane only until D2";
const HOST_RUNTIME_DISABLED_CODE: &str = "host_runtime_disabled";

/// The embedded face: holds the process's one runtime detector and projects a
/// detection pass. The detector is the ENTIRE state — there is no deployment flag to
/// read, because being handed a detector at all is what "this deployment has a machine
/// to ask about" means.
pub fn detecting_router<D>(detector: D) -> Router
where
    D: RuntimeCatalogDetector + Clone + Send + Sync + 'static,
{
    Router::new()
        .route(RUNTIMES_PATH, get(detecting::<D>))
        .with_state(detector)
}

/// The control-plane face: no detector field, no generic slot, no state. It can only
/// refuse, so a standalone composition that mounts this can never — by construction —
/// probe the machine it runs on.
pub fn control_plane_router() -> Router {
    Router::new().route(RUNTIMES_PATH, get(control_plane_refusal))
}

async fn detecting<D>(State(detector): State<D>) -> Response
where
    D: RuntimeCatalogDetector,
{
    let catalog = detector.detect_runtimes().await;
    Json(RuntimesResponse {
        runtimes: catalog.entries().iter().map(runtime_dto).collect(),
    })
    .into_response()
}

async fn control_plane_refusal() -> Response {
    disabled()
}

/// Success body: `{ "runtimes": [RuntimeInfo] }` — the offered catalogue in the offered
/// order, which is a property of the value the detector returns rather than of this
/// projection.
#[derive(Serialize)]
struct RuntimesResponse {
    runtimes: Vec<RuntimeInfoDto>,
}

/// The wire projection of one offered runtime, field-for-field `RuntimeInfoSchema`.
#[derive(Serialize)]
struct RuntimeInfoDto {
    kind: &'static str,
    state: &'static str,
    reason: &'static str,
    installed: bool,
    binary: Option<String>,
    version: Option<String>,
}

fn runtime_dto(entry: &DetectedRuntime) -> RuntimeInfoDto {
    let (state, reason) = availability(&entry.detection);
    RuntimeInfoDto {
        kind: entry.kind.as_str(),
        state,
        reason,
        // Read from the detection rather than from the arm matched above: which
        // outcomes count as installed, and which carry a binary or a version, is the
        // fact's own answer. This projection only decides how the outcome is SPELLED.
        installed: entry.detection.installed(),
        binary: entry.detection.binary().map(str::to_string),
        version: entry.detection.version().map(str::to_string),
    }
}

/// The two wire tokens for one detection outcome, exhaustively — so a new arm on the
/// detection matrix is a compile error here rather than a silently unrendered state.
///
/// Pinned to `RuntimeAvailabilityStateSchema` and `RuntimeDetectionReasonSchema`.
/// `unknown` / `probe_failed` is kept distinct from `absent` / `not_found` on purpose:
/// a broken probe must never be reported as a machine without the runtime.
fn availability(detection: &RuntimeDetection) -> (&'static str, &'static str) {
    match detection {
        RuntimeDetection::Resolved { .. } => ("present", "resolved"),
        RuntimeDetection::VersionUnavailable { .. } => {
            ("present_unverified", "version_unavailable")
        }
        RuntimeDetection::Absent => ("absent", "not_found"),
        RuntimeDetection::ProbeFailed => ("unknown", "probe_failed"),
    }
}

/// The refusal body: this face carries `code`, not the `error_code` the host resource
/// faces use.
#[derive(Serialize)]
struct DisabledBody {
    error: &'static str,
    code: &'static str,
}

fn disabled() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(DisabledBody {
            error: RUNTIME_DETECTION_DISABLED,
            code: HOST_RUNTIME_DISABLED_CODE,
        }),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering::SeqCst},
    };

    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use serde_json::{Value, json};
    use silverhand_backend_core::identity_access::agents::runtime_catalog::RuntimeCatalog;
    use tower::ServiceExt;

    use super::*;

    /// A detector that answers with a fixed pass and counts how often it was asked.
    #[derive(Clone)]
    struct FakeDetector {
        catalog: RuntimeCatalog,
        passes: Arc<AtomicUsize>,
    }

    impl RuntimeCatalogDetector for FakeDetector {
        async fn detect_runtimes(&self) -> RuntimeCatalog {
            self.passes.fetch_add(1, SeqCst);
            self.catalog.clone()
        }
    }

    /// One pass covering all four outcomes, in catalogue order.
    fn every_outcome() -> RuntimeCatalog {
        RuntimeCatalog::from_detections([
            RuntimeDetection::Resolved {
                binary: "/usr/local/bin/claude".to_string(),
                version: "2.1.3".to_string(),
            },
            RuntimeDetection::VersionUnavailable {
                binary: "/usr/local/bin/codex".to_string(),
            },
            RuntimeDetection::Absent,
            RuntimeDetection::ProbeFailed,
            RuntimeDetection::Absent,
            RuntimeDetection::Absent,
            RuntimeDetection::Absent,
        ])
    }

    struct Reply {
        status: StatusCode,
        body: Value,
        passes: usize,
    }

    /// Probe the EMBEDDED face, which holds the detector and counts its passes.
    async fn detect(catalog: RuntimeCatalog) -> Reply {
        let passes = Arc::new(AtomicUsize::new(0));
        let detector = FakeDetector {
            catalog,
            passes: passes.clone(),
        };
        let response = detecting_router(detector)
            .oneshot(Request::get(RUNTIMES_PATH).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        Reply {
            status,
            body: serde_json::from_slice(&body).unwrap(),
            passes: passes.load(SeqCst),
        }
    }

    /// Every outcome's wire spelling, in the offered order, with each one's evidence
    /// and no other runtime present.
    #[tokio::test]
    async fn each_offered_runtime_is_reported_with_the_evidence_its_outcome_carries() {
        let reply = detect(every_outcome()).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(
            reply.body,
            json!({
                "runtimes": [
                    {
                        "kind": "claude",
                        "state": "present",
                        "reason": "resolved",
                        "installed": true,
                        "binary": "/usr/local/bin/claude",
                        "version": "2.1.3"
                    },
                    {
                        "kind": "codex",
                        "state": "present_unverified",
                        "reason": "version_unavailable",
                        "installed": true,
                        "binary": "/usr/local/bin/codex",
                        "version": null
                    },
                    {
                        "kind": "qoder",
                        "state": "absent",
                        "reason": "not_found",
                        "installed": false,
                        "binary": null,
                        "version": null
                    },
                    {
                        "kind": "opencode",
                        "state": "unknown",
                        "reason": "probe_failed",
                        "installed": false,
                        "binary": null,
                        "version": null
                    },
                    {"kind":"pi","state":"absent","reason":"not_found","installed":false,"binary":null,"version":null},
                    {"kind":"grok","state":"absent","reason":"not_found","installed":false,"binary":null,"version":null},
                    {"kind":"antigravity","state":"absent","reason":"not_found","installed":false,"binary":null,"version":null}
                ]
            })
        );
        assert_eq!(reply.passes, 1);
    }

    /// The control-plane face refuses with the product's exact bytes — and there is no
    /// detector to probe: `control_plane_router` takes none, so "probes nothing" is a
    /// property of its TYPE rather than a branch that must be tested for every input.
    /// The only way to reach a detection pass is to mount the OTHER router, which a
    /// standalone composition does not.
    #[tokio::test]
    async fn a_control_plane_server_refuses_with_the_disabled_body() {
        let response = control_plane_router()
            .oneshot(Request::get(RUNTIMES_PATH).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            body,
            json!({
                "error": "runtime detection requires a daemon-host runtime; \
            standalone server mode is control-plane only until D2",
                "code": "host_runtime_disabled"
            })
        );
    }
}
