//! Typed request and result contract for the `host_runtime_catalog` command.
//!
//! The transport correlates, refuses a mismatched connection generation before it sends
//! anything, and decodes the daemon's refusal code as a closed set. This leaf owns
//! only what this one command sends, what its answer means, and how a refusal code
//! maps to this operation's outcome — an unknown code is protocol corruption and
//! never a refusal this operation can report. Sharing the transport primitive with
//! the status seed must not merge the two payload or result vocabularies.
//!
//! Every fact this leaf produces is bound to the resolved host connection generation it was
//! answered on, and its rows can only be read by naming that coordinate.
//!
//! `detected_at` is an `i64` where the TS contract says `z.number()`. That is a
//! producer-domain refinement, not exact schema parity: the daemon stamps epoch
//! milliseconds and every consumer stores and compares them as integers, so a
//! fractional value is refused rather than truncated. A route signing this leaf
//! must not describe it as schema-exact.

use std::{collections::BTreeMap, time::Duration};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use silverhand_backend_core::identity_access::agents::{
    runtime::RuntimeKind, runtime_capability::is_reasoning_effort_token,
};

#[cfg(test)]
use crate::application_orchestration::daemon_delivery::{CommandRefusal, DaemonRefusalCode};
use crate::application_orchestration::daemon_delivery::{
    HostDataPlaneCustody, HostDataPlaneFailure, HostDataPlaneRequest, OwningHostDataPlane, command,
};

use super::wire_number::js_safe_integer;

/// This operation's own budget: one probe may shell out to every runtime binary
/// on the machine, so it is far longer than a control-plane round trip.
const PROBE_TIMEOUT: Duration = Duration::from_secs(45);
/// Inventory plus one model probe, which the daemon bounds at 15 seconds.
const MODEL_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(75);

pub struct RuntimeCatalogRequest {
    custody: HostDataPlaneCustody,
    pub force: bool,
    models_for: Option<RuntimeKind>,
}

impl RuntimeCatalogRequest {
    pub fn new(custody: HostDataPlaneCustody, force: bool) -> Self {
        Self {
            custody,
            force,
            models_for: None,
        }
    }

    pub fn with_models_for(mut self, runtime: Option<RuntimeKind>) -> Self {
        self.models_for = runtime;
        self
    }
}

pub enum RuntimeCatalogOutcome {
    Detected(RuntimeCatalog),
    /// The custody the caller observed is not the one serving the host now, so
    /// there is nothing to answer for it yet.
    AttachPending,
    Timeout,
    DaemonInternalError,
    /// The transport answered about a coordinate this request never asked about.
    /// No daemon caused it and no operator can act on it: it is a defect in this
    /// crate, kept out of the refusal arms so it can never be reported as one.
    TransportContractViolation,
}

/// A catalog together with the coordinate that produced it. Private fields with no
/// constructor, and rows readable only through [`Self::rows_for`], so a projector
/// cannot pair one host's rows with another host's key — including when two
/// machines spell their daemon instance identically.
///
/// ```compile_fail
/// use silverhand_backend_server::application_orchestration::daemon_delivery::HostDataPlaneCustody;
/// use silverhand_backend_server::host_runtime::host_control::runtime_catalog::RuntimeCatalog;
///
/// fn forge(custody: HostDataPlaneCustody) -> RuntimeCatalog {
///     RuntimeCatalog {
///         custody,
///         runtimes: Vec::new(),
///         detected_at: 1,
///     }
/// }
/// ```
///
/// ```
/// use silverhand_backend_server::application_orchestration::daemon_delivery::HostDataPlaneCustody;
/// use silverhand_backend_server::host_runtime::host_control::runtime_catalog::{
///     CatalogCoordinateMismatch, CatalogView, RuntimeCatalog,
/// };
///
/// fn read<'a>(
///     catalog: &'a RuntimeCatalog,
///     custody: &HostDataPlaneCustody,
/// ) -> Result<CatalogView<'a>, CatalogCoordinateMismatch> {
///     catalog.rows_for(custody)
/// }
/// ```
pub struct RuntimeCatalog {
    custody: HostDataPlaneCustody,
    runtimes: Vec<DetectedRuntime>,
    detected_at: i64,
    model_discovery: Option<Box<ModelDiscovery>>,
}

impl RuntimeCatalog {
    pub fn rows_for(
        &self,
        custody: &HostDataPlaneCustody,
    ) -> Result<CatalogView<'_>, CatalogCoordinateMismatch> {
        if self.custody.host() != custody.host() {
            return Err(CatalogCoordinateMismatch::Host);
        }
        if self.custody.daemon_instance() != custody.daemon_instance() {
            return Err(CatalogCoordinateMismatch::DaemonInstance);
        }
        if self.custody != *custody {
            return Err(CatalogCoordinateMismatch::ConnectionGeneration);
        }
        Ok(CatalogView {
            runtimes: &self.runtimes,
            detected_at: self.detected_at,
            model_discovery: self.model_discovery.as_deref(),
        })
    }
}

/// What a reader gets once it has named the coordinate it is filing under. Carries
/// no coordinate of its own, so there is nothing here to misattribute.
pub struct CatalogView<'a> {
    pub runtimes: &'a [DetectedRuntime],
    pub detected_at: i64,
    pub model_discovery: Option<&'a ModelDiscovery>,
}

/// Which coordinate a reader named that the catalog is not about.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CatalogCoordinateMismatch {
    Host,
    ConnectionGeneration,
    DaemonInstance,
}

pub struct DetectedRuntime {
    pub kind: RuntimeKind,
    pub state: RuntimeAvailability,
    pub reason: RuntimeDetectionReason,
    pub installed: bool,
    pub binary: Option<String>,
    pub version: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeAvailability {
    Present,
    PresentUnverified,
    Absent,
    Unknown,
}

impl RuntimeAvailability {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Present => "present",
            Self::PresentUnverified => "present_unverified",
            Self::Absent => "absent",
            Self::Unknown => "unknown",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "present" => Some(Self::Present),
            "present_unverified" => Some(Self::PresentUnverified),
            "absent" => Some(Self::Absent),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeDetectionReason {
    Resolved,
    VersionUnavailable,
    NotFound,
    ProbeFailed,
}

impl RuntimeDetectionReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Resolved => "resolved",
            Self::VersionUnavailable => "version_unavailable",
            Self::NotFound => "not_found",
            Self::ProbeFailed => "probe_failed",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "resolved" => Some(Self::Resolved),
            "version_unavailable" => Some(Self::VersionUnavailable),
            "not_found" => Some(Self::NotFound),
            "probe_failed" => Some(Self::ProbeFailed),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ModelDiscovery {
    runtime: String,
    status: ModelDiscoveryStatus,
    models: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reasoning_by_model: Option<BTreeMap<String, ModelReasoning>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    default_model: Option<String>,
    detected_at: i64,
    cached: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ModelReasoning {
    supported_efforts: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    default_effort: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum ModelDiscoveryStatus {
    Ok,
    Unsupported,
    Error,
}

impl ModelDiscovery {
    fn unsupported(runtime: RuntimeKind, detected_at: i64) -> Self {
        Self {
            runtime: runtime.as_str().to_owned(),
            status: ModelDiscoveryStatus::Unsupported,
            models: Vec::new(),
            reasoning_by_model: None,
            default_model: None,
            detected_at,
            cached: false,
        }
    }

    fn decode(value: Value, requested: Option<RuntimeKind>) -> Option<Self> {
        let result: Self = serde_json::from_value(value).ok()?;
        if RuntimeKind::parse(&result.runtime) != requested
            || requested.is_none()
            || !js_safe_integer(result.detected_at)
            || result.models.len() > 5000
            || result.models.iter().any(|id| {
                id.is_empty() || id.encode_utf16().count() > 512 || id.chars().any(char::is_control)
            })
            || result
                .default_model
                .as_ref()
                .is_some_and(|id| !result.models.contains(id))
            || result.reasoning_by_model.as_ref().is_some_and(|entries| {
                entries.iter().any(|(id, reasoning)| {
                    !result.models.contains(id)
                        || reasoning.supported_efforts.len() > 64
                        || !reasoning
                            .supported_efforts
                            .iter()
                            .all(|token| is_reasoning_effort_token(token))
                        || reasoning
                            .default_effort
                            .as_ref()
                            .is_some_and(|token| !reasoning.supported_efforts.contains(token))
                })
            })
            || (!matches!(result.status, ModelDiscoveryStatus::Ok) && !result.models.is_empty())
        {
            return None;
        }
        Some(result)
    }
}

pub async fn probe(
    dispatch: &OwningHostDataPlane,
    request: RuntimeCatalogRequest,
) -> RuntimeCatalogOutcome {
    let requested = request.models_for;
    // A daemon without discovery strict-rejects `models_for`; answer unsupported instead.
    let models_for = requested.filter(|_| {
        dispatch
            .features(&request.custody)
            .is_ok_and(|features| features.model_discovery)
    });
    let mut params = json!({ "force": request.force });
    if let Some(runtime) = models_for {
        params["models_for"] = json!(runtime.as_str());
    }
    let answer = match dispatch
        .dispatch(HostDataPlaneRequest {
            custody: request.custody.clone(),
            command: command::runtime_catalog(params),
            timeout: if models_for.is_some() {
                MODEL_DISCOVERY_TIMEOUT
            } else {
                PROBE_TIMEOUT
            },
        })
        .await
    {
        Ok(answer) => answer,
        Err(failure) => return outcome_for(failure),
    };
    // The coordinate is bound before the payload is looked at, so an answer about
    // another machine or another custody cannot hide behind a decode failure.
    let Ok(result) = answer.into_result_for(&request.custody) else {
        return RuntimeCatalogOutcome::TransportContractViolation;
    };
    // Discovery is advisory: a malformed one is dropped without failing the catalog.
    let discovered = result
        .get("model_discovery")
        .and_then(|value| ModelDiscovery::decode(value.clone(), models_for));
    let Some((runtimes, detected_at)) = CatalogWire::decode(result) else {
        return RuntimeCatalogOutcome::DaemonInternalError;
    };
    let model_discovery = match (requested, models_for) {
        (Some(runtime), None) => Some(ModelDiscovery::unsupported(runtime, detected_at)),
        _ => discovered,
    };
    RuntimeCatalogOutcome::Detected(RuntimeCatalog {
        custody: request.custody,
        runtimes,
        detected_at,
        model_discovery: model_discovery.map(Box::new),
    })
}

fn outcome_for(failure: HostDataPlaneFailure) -> RuntimeCatalogOutcome {
    match failure {
        HostDataPlaneFailure::NoCurrentDaemon
        | HostDataPlaneFailure::ConnectionGenerationMismatch
        | HostDataPlaneFailure::DaemonInstanceMismatch
        | HostDataPlaneFailure::NotDispatched => RuntimeCatalogOutcome::AttachPending,
        HostDataPlaneFailure::TimedOut => RuntimeCatalogOutcome::Timeout,
        // The daemon was reachable and answered, but not with a catalog. TS folds
        // exactly this set into `daemon_internal_error` (502).
        HostDataPlaneFailure::Refused(_)
        | HostDataPlaneFailure::Ambiguous
        | HostDataPlaneFailure::ProtocolCorruption
        | HostDataPlaneFailure::RegistryUnavailable => RuntimeCatalogOutcome::DaemonInternalError,
    }
}

#[derive(Deserialize)]
struct CatalogWire {
    runtimes: Vec<RuntimeRowWire>,
    detected_at: i64,
}

impl CatalogWire {
    fn decode(value: Value) -> Option<(Vec<DetectedRuntime>, i64)> {
        let wire = serde_json::from_value::<Self>(value).ok()?;
        if !js_safe_integer(wire.detected_at) {
            return None;
        }
        let runtimes = wire
            .runtimes
            .into_iter()
            .map(RuntimeRowWire::decode)
            .collect::<Option<Vec<_>>>()?;
        Some((runtimes, wire.detected_at))
    }
}

#[derive(Deserialize)]
struct RuntimeRowWire {
    kind: String,
    state: String,
    reason: String,
    installed: bool,
    binary: Value,
    version: Value,
}

impl RuntimeRowWire {
    fn decode(self) -> Option<DetectedRuntime> {
        Some(DetectedRuntime {
            kind: RuntimeKind::parse(&self.kind)?,
            state: RuntimeAvailability::parse(&self.state)?,
            reason: RuntimeDetectionReason::parse(&self.reason)?,
            installed: self.installed,
            binary: nullable_text(self.binary)?,
            version: nullable_text(self.version)?,
        })
    }
}

fn nullable_text(value: Value) -> Option<Option<String>> {
    match value {
        Value::Null => Some(None),
        Value::String(text) => Some(Some(text)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_transport_failure_maps_to_one_outcome_of_this_operation() {
        // The transport tells "no daemon", "not the daemon you meant" and "provably
        // not sent" apart, and this operation deliberately does not: to a caller
        // waiting for a machine to be reachable they are one answer.
        for pending in [
            HostDataPlaneFailure::NoCurrentDaemon,
            HostDataPlaneFailure::ConnectionGenerationMismatch,
            HostDataPlaneFailure::DaemonInstanceMismatch,
            HostDataPlaneFailure::NotDispatched,
        ] {
            assert!(matches!(
                outcome_for(pending),
                RuntimeCatalogOutcome::AttachPending
            ));
        }
        assert!(matches!(
            outcome_for(HostDataPlaneFailure::TimedOut),
            RuntimeCatalogOutcome::Timeout
        ));
        // A refusal says the same thing about this one question as a round trip that
        // failed rather than being refused: the machine cannot say what it has
        // installed. TS folds the same set into `daemon_internal_error` (502).
        for unanswerable in [
            HostDataPlaneFailure::Refused(CommandRefusal::Response(
                DaemonRefusalCode::ShuttingDown,
            )),
            HostDataPlaneFailure::Ambiguous,
            HostDataPlaneFailure::ProtocolCorruption,
            HostDataPlaneFailure::RegistryUnavailable,
        ] {
            assert!(matches!(
                outcome_for(unanswerable),
                RuntimeCatalogOutcome::DaemonInternalError
            ));
        }
    }

    fn row(overrides: Value) -> Value {
        let mut row = json!({
            "kind": "claude",
            "state": "present",
            "reason": "resolved",
            "installed": true,
            "binary": "/usr/bin/claude",
            "version": "1.2.3",
        });
        for (key, value) in overrides.as_object().unwrap() {
            row[key] = value.clone();
        }
        row
    }

    fn catalog(rows: Value) -> Value {
        json!({ "runtimes": rows, "detected_at": 1_700_000_000_000_i64 })
    }

    #[test]
    fn a_catalog_decodes_all_availability_and_reason_tokens() {
        let (runtimes, detected_at) = CatalogWire::decode(catalog(json!([
            row(json!({})),
            row(json!({ "kind": "codex", "state": "present_unverified", "reason": "version_unavailable", "version": Value::Null })),
            row(json!({ "kind": "opencode", "state": "absent", "reason": "not_found", "installed": false, "binary": Value::Null })),
            row(json!({ "kind": "gemini", "state": "unknown", "reason": "probe_failed" })),
        ])))
        .expect("catalog decodes");
        assert_eq!(detected_at, 1_700_000_000_000);
        assert_eq!(
            runtimes
                .iter()
                .map(|runtime| (
                    runtime.kind.as_str(),
                    runtime.state.as_str(),
                    runtime.reason.as_str(),
                    runtime.installed,
                    runtime.binary.clone(),
                    runtime.version.clone(),
                ))
                .collect::<Vec<_>>(),
            vec![
                (
                    "claude",
                    "present",
                    "resolved",
                    true,
                    Some("/usr/bin/claude".to_owned()),
                    Some("1.2.3".to_owned())
                ),
                (
                    "codex",
                    "present_unverified",
                    "version_unavailable",
                    true,
                    Some("/usr/bin/claude".to_owned()),
                    None
                ),
                (
                    "opencode",
                    "absent",
                    "not_found",
                    false,
                    None,
                    Some("1.2.3".to_owned())
                ),
                (
                    "gemini",
                    "unknown",
                    "probe_failed",
                    true,
                    Some("/usr/bin/claude".to_owned()),
                    Some("1.2.3".to_owned())
                ),
            ]
        );
    }

    #[test]
    fn a_token_outside_a_closed_set_is_never_reported_as_a_catalog() {
        for override_value in [
            json!({ "kind": "banana" }),
            json!({ "state": "installed" }),
            json!({ "reason": "timeout" }),
        ] {
            assert!(
                CatalogWire::decode(catalog(json!([row(override_value.clone())]))).is_none(),
                "expected {override_value} to be refused"
            );
        }
    }

    #[test]
    fn zod_style_unknown_fields_are_stripped_but_required_fields_stay_required() {
        let mut widened = row(json!({}));
        widened["unexpected"] = json!(true);
        assert!(CatalogWire::decode(catalog(json!([widened]))).is_some());

        let mut missing = row(json!({}));
        missing.as_object_mut().unwrap().remove("version");
        assert!(CatalogWire::decode(catalog(json!([missing]))).is_none());

        let mut wrong_type = row(json!({}));
        wrong_type["binary"] = json!(7);
        assert!(CatalogWire::decode(catalog(json!([wrong_type]))).is_none());

        let mut widened_catalog = catalog(json!([]));
        widened_catalog["unexpected"] = json!(true);
        assert!(CatalogWire::decode(widened_catalog).is_some());

        assert!(CatalogWire::decode(json!({ "runtimes": [] })).is_none());
        assert!(CatalogWire::decode(json!({ "runtimes": [], "detected_at": 1.5 })).is_none());
    }
    #[test]
    fn model_effort_metadata_round_trips_without_using_the_static_product_table() {
        let valid = json!({"runtime":"codex","status":"ok","models":["custom"],"default_model":"custom",
            "reasoning_by_model":{"custom":{"supported_efforts":["future-tier"],"default_effort":"future-tier"}},
            "detected_at":1,"cached":false});
        let kind = Some(RuntimeKind::Codex);
        let decoded = ModelDiscovery::decode(valid.clone(), kind).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), valid);
        for patch in [
            json!({"default_model":"foreign"}),
            json!({"reasoning_by_model":{"foreign":{"supported_efforts":[]}}}),
            json!({"reasoning_by_model":{"custom":{"supported_efforts":["BAD"]}}}),
            json!({"reasoning_by_model":{"custom":{"supported_efforts":["high"],"default_effort":"max"}}}),
        ] {
            let mut value = valid.clone();
            value
                .as_object_mut()
                .unwrap()
                .extend(patch.as_object().unwrap().clone());
            assert!(ModelDiscovery::decode(value, kind).is_none());
        }
    }

    #[test]
    fn model_discovery_is_bound_to_the_requested_runtime_and_validated() {
        let valid = json!({"runtime":"pi","status":"ok","models":["provider/custom"],"detected_at":1,"cached":false});
        let pi = RuntimeKind::parse("pi");
        assert!(ModelDiscovery::decode(valid.clone(), pi).is_some());
        assert!(ModelDiscovery::decode(valid.clone(), None).is_none());
        assert!(ModelDiscovery::decode(valid.clone(), RuntimeKind::parse("codex")).is_none());
        for patch in [
            json!({"status":"invalid"}),
            json!({"status":"error"}),
            json!({"models":[""]}),
            json!({"models":["bad\nmodel"]}),
            json!({"models":["x".repeat(513)]}),
        ] {
            let mut value = valid.clone();
            value
                .as_object_mut()
                .unwrap()
                .extend(patch.as_object().unwrap().clone());
            assert!(ModelDiscovery::decode(value, pi).is_none());
        }
        let mut empty = valid;
        empty["models"] = json!([]);
        assert!(ModelDiscovery::decode(empty.clone(), pi).is_some());
        empty["status"] = json!("unsupported");
        assert!(ModelDiscovery::decode(empty, pi).is_some());
    }
}
