//! Crate-private shared HTTP-wire vocabulary for the agent profile-MCP routes.
//!
//! The self-scoped internal family (`identity_access::self_profile_mcp`) and the public
//! management faces (`identity_access::public_profile_mcp`) speak the SAME wire: the probe request
//! grammar and its one stateless rule, the probe-outcome projection, the data-plane
//! fault→HTTP mapping, and the raw-config read body. This module is the single home for
//! that vocabulary so neither router re-declares it and neither copies the grammar.
//!
//! It owns ONLY the HTTP wire. The transport-neutral domain (the
//! [`AgentProfileMcpDataPlane`](silverhand_backend_core::identity_access::agent_profile_mcp) port, the
//! probe command/outcome, the closed fault taxonomy) stays in `backend-core`, and the
//! data-plane authority, placement, and transport keep their existing owners — nothing
//! here dispatches or resolves a host.

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use garde::Validate;
use serde::{Deserialize, Serialize};
use silverhand_backend_core::identity_access::agent_profile_mcp::{
    McpDataPlaneFault, McpDataPlaneFaultCode, McpProbeCommand, McpProbeOutcome, McpProbePhase,
    McpProbeServerInfo, McpProbeTool,
};

use crate::foundation::wire_patch::RequiredPatchField;

// ===========================================================================
// GET — raw read body.
// ===========================================================================

/// `{ raw: string | null }` — the verbatim `mcp.json` text, or `null` when the file
/// does not exist (the normal empty state, never a `404`).
#[derive(Serialize)]
pub(crate) struct ReadConfigResponse {
    pub(crate) raw: Option<String>,
}

// ===========================================================================
// POST probe — request admission.
// ===========================================================================

/// The probe request: `{ server_name, include_tools? }`. `server_name` is the
/// RAW user-visible name (never the `silverhand_<scope>__` runtime spelling).
///
/// Admission rides the frozen [`ValidatedJson`](crate::foundation::wire_admission::ValidatedJson)
/// model: a malformed body is a native `400`; a missing/duplicate key, a
/// wrong-typed field, a present `include_tools: null`, or a top-level NON-object shape
/// (an array such as `["srv", true]`, a scalar, `null`) is a native `422` — the shared
/// `ObjectOnly` floor (#2246) baked into `ValidatedJson` rejects a top-level array
/// BEFORE serde can bind its elements positionally into a dispatch. An empty
/// `server_name` decodes fine and then fails the [`server_name_present`] `garde` rule,
/// so it is the semantic `422 { "error": "invalid input" }` (no `code`, no `issues`).
#[derive(Deserialize, Validate)]
pub(crate) struct ProbeRequest {
    #[garde(custom(server_name_present))]
    server_name: String,
    // Tri-state via the frozen `RequiredPatchField`: absent → `Missing` (defaults to
    // `false`), present `true`/`false` → `Value`, present `null` → a native `422`
    // decode error. `#[serde(default)]` supplies `Missing` when the key is absent.
    #[serde(default)]
    #[garde(skip)]
    include_tools: RequiredPatchField<bool>,
}

impl ProbeRequest {
    /// Map the admitted request onto the transport-neutral domain command. An absent
    /// `include_tools` defaults to `false`; the raw `server_name` is bound verbatim.
    pub(crate) fn into_command(self) -> McpProbeCommand {
        McpProbeCommand {
            server_name: self.server_name,
            include_tools: match self.include_tools {
                RequiredPatchField::Missing => false,
                RequiredPatchField::Value(value) => value,
            },
        }
    }
}

/// `garde` rule: `server_name` must be non-empty. This is a RAW `!is_empty()` check —
/// NOT trimmed — mirroring the product's `z.string().min(1)` (a character count, not a
/// trimmed length), so a whitespace-only name (`" "`) is accepted and bound to the
/// probe command verbatim.
fn server_name_present(value: &str, _: &()) -> garde::Result {
    if value.is_empty() {
        Err(garde::Error::new("server_name must not be empty"))
    } else {
        Ok(())
    }
}

// ===========================================================================
// POST probe — outcome projection.
// ===========================================================================

/// `{ ok, phase, message?, server_info?, tools? }` — the `McpProbeResult` the CLI and
/// the agent pane read. Absent optional fields are OMITTED (never explicit `null`),
/// mirroring the product's `.strict()` optionals. `message` is already redacted
/// upstream.
#[derive(Serialize)]
pub(crate) struct ProbeOutcomeWire {
    ok: bool,
    phase: ProbePhaseWire,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    server_info: Option<ProbeServerInfoWire>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ProbeToolWire>>,
}

/// The probe phase, serialized to the product's snake_case tokens
/// (`parse`/`not_found`/`connect`/`initialize`/`tools`).
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum ProbePhaseWire {
    Parse,
    NotFound,
    Connect,
    Initialize,
    Tools,
}

#[derive(Serialize)]
struct ProbeServerInfoWire {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
}

#[derive(Serialize)]
struct ProbeToolWire {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
}

fn probe_phase_wire(phase: McpProbePhase) -> ProbePhaseWire {
    match phase {
        McpProbePhase::Parse => ProbePhaseWire::Parse,
        McpProbePhase::NotFound => ProbePhaseWire::NotFound,
        McpProbePhase::Connect => ProbePhaseWire::Connect,
        McpProbePhase::Initialize => ProbePhaseWire::Initialize,
        McpProbePhase::Tools => ProbePhaseWire::Tools,
    }
}

/// Project a transport-neutral outcome onto its wire body.
pub(crate) fn probe_outcome_wire(outcome: &McpProbeOutcome) -> ProbeOutcomeWire {
    ProbeOutcomeWire {
        ok: outcome.ok,
        phase: probe_phase_wire(outcome.phase),
        message: outcome.message.clone(),
        server_info: outcome.server_info.as_ref().map(probe_server_info_wire),
        tools: outcome
            .tools
            .as_ref()
            .map(|tools| tools.iter().map(probe_tool_wire).collect()),
    }
}

fn probe_server_info_wire(info: &McpProbeServerInfo) -> ProbeServerInfoWire {
    ProbeServerInfoWire {
        name: info.name.clone(),
        version: info.version.clone(),
    }
}

fn probe_tool_wire(tool: &McpProbeTool) -> ProbeToolWire {
    ProbeToolWire {
        name: tool.name.clone(),
        description: tool.description.clone(),
    }
}

// ===========================================================================
// Data-plane fault → HTTP mapping.
// ===========================================================================

/// Render a data-plane fault as the product's `{ error, code }` envelope at the status
/// its code maps to. `code` is a CLOSED taxonomy (#2231's adapter already folded any
/// unclassified upstream error into `DaemonInternalError`), so both the status and the
/// stable wire token are exhaustive matches — nothing an adapter produces can widen the
/// CLI's `code` union or leak a raw upstream string.
pub(crate) fn data_plane_error(fault: McpDataPlaneFault) -> Response {
    (
        data_plane_status(fault.code),
        Json(serde_json::json!({
            "error": fault.message,
            "code": data_plane_wire_code(fault.code),
        })),
    )
        .into_response()
}

/// The HTTP status for each closed fault code, matching the product's
/// `mapDataPlaneError` result statuses (`workspace-data-plane.ts`). Exhaustive by
/// construction: a new code forces a new arm here, so the code→status tail can never
/// silently fall through.
pub(crate) fn data_plane_status(code: McpDataPlaneFaultCode) -> StatusCode {
    match code {
        McpDataPlaneFaultCode::InvalidConfig => StatusCode::BAD_REQUEST,
        McpDataPlaneFaultCode::AgentDeleting => StatusCode::CONFLICT,
        McpDataPlaneFaultCode::HostAttachPending | McpDataPlaneFaultCode::FsUnavailable => {
            StatusCode::SERVICE_UNAVAILABLE
        }
        McpDataPlaneFaultCode::DaemonTimeout => StatusCode::GATEWAY_TIMEOUT,
        McpDataPlaneFaultCode::DaemonInternalError => StatusCode::BAD_GATEWAY,
    }
}

/// The stable wire token echoed in the CLI's `{ error, code }`, matching the TS
/// data-plane code strings. Exhaustive: the token union is the contract.
pub(crate) fn data_plane_wire_code(code: McpDataPlaneFaultCode) -> &'static str {
    match code {
        McpDataPlaneFaultCode::HostAttachPending => "host_attach_pending",
        McpDataPlaneFaultCode::DaemonTimeout => "daemon_timeout",
        McpDataPlaneFaultCode::DaemonInternalError => "daemon_internal_error",
        McpDataPlaneFaultCode::FsUnavailable => "fs_unavailable",
        McpDataPlaneFaultCode::InvalidConfig => "invalid_config",
        McpDataPlaneFaultCode::AgentDeleting => "agent_deleting",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every closed fault code maps to its product HTTP status AND its stable wire
    /// token. The route tests additionally exercise response rendering and admission.
    #[test]
    fn every_fault_code_maps_to_its_status_and_wire_token() {
        use McpDataPlaneFaultCode::*;
        let table = [
            (InvalidConfig, StatusCode::BAD_REQUEST, "invalid_config"),
            (AgentDeleting, StatusCode::CONFLICT, "agent_deleting"),
            (
                HostAttachPending,
                StatusCode::SERVICE_UNAVAILABLE,
                "host_attach_pending",
            ),
            (
                FsUnavailable,
                StatusCode::SERVICE_UNAVAILABLE,
                "fs_unavailable",
            ),
            (DaemonTimeout, StatusCode::GATEWAY_TIMEOUT, "daemon_timeout"),
            (
                DaemonInternalError,
                StatusCode::BAD_GATEWAY,
                "daemon_internal_error",
            ),
        ];
        for (code, status, wire) in table {
            assert_eq!(data_plane_status(code), status, "status for {code:?}");
            assert_eq!(data_plane_wire_code(code), wire, "wire token for {code:?}");
        }
    }
}
