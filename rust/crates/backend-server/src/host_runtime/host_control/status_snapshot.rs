//! Typed request and result contract for the `host_status` command.
//!
//! Sibling of [`crate::host_runtime::host_control::runtime_catalog`] over the same transport
//! primitive, and deliberately not sharing a vocabulary with it. `host_status`
//! declares no daemon-business refusal codes, so its outcomes split on something
//! else entirely: whether the machine is *absent* (nothing to report, and the read
//! surface renders "offline since `last_seen_at`") or *present but unable to
//! answer* (a named reason the renderer shows as a fault). Folding those two into
//! one arm — the way the catalog folds its own three pending shapes — would make an
//! offline computer look broken.
//!
//! What this leaf owns: what the command sends, what its answer means, and how one
//! transport failure becomes one outcome of this operation. What it does not own:
//! when to ask, how often, or what to keep. That is the seed layer's single change
//! reason, not this one's.
//!
//! Two producer-domain refinements, neither of them schema-exact parity, and a
//! route signing this leaf must not describe them as such:
//!
//!   * `daemon_pid` / `started_at` / `uptime_ms` are `i64` where the TS contract
//!     says `z.number().int()`. Integral by contract on both sides; a fractional
//!     value is refused rather than truncated.
//!   * `active_agent_ids` is narrowed to [`AgentId`] and unparseable entries are
//!     dropped instead of refusing the whole answer. The only string `AgentId`
//!     refuses is the empty one, and the sole consumer of this set tests membership
//!     against registered agent ids — none of which can be empty — so a dropped
//!     entry could never have matched anything. Refusing instead would let one
//!     filler string turn a healthy computer into a fault.

use std::{collections::HashSet, time::Duration};

use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use silverhand_backend_core::foundation::identity::{AgentId, DaemonInstanceId, HostId};

#[cfg(test)]
use crate::application_orchestration::daemon_delivery::{CommandRefusal, DaemonRefusalCode};
use crate::application_orchestration::daemon_delivery::{
    HostDataPlaneCustody, HostDataPlaneFailure, HostDataPlaneRequest, OwningHostDataPlane, command,
};

use super::wire_number::js_safe_integer;

/// This operation's own budget. A control-plane round trip that reads facts the
/// daemon already holds, so it is far shorter than the catalog's probe.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

pub struct StatusSnapshotRequest {
    custody: HostDataPlaneCustody,
}

impl StatusSnapshotRequest {
    pub fn new(custody: HostDataPlaneCustody) -> Self {
        Self { custody }
    }
}

/// Only daemons that advertised the capability are asked: older servers refuse
/// unknown result fields, so a daemon must never volunteer them.
fn status_params(include_machine: bool) -> Value {
    if include_machine {
        json!({ "include_machine": true })
    } else {
        json!({})
    }
}

pub enum StatusSnapshotOutcome {
    Answered(StatusSnapshot),
    /// No daemon this server has agreed to speak to, or the custody moved before
    /// the frame left. Carries no reason on purpose: there is no fault to name, and
    /// the read surface answers from the host's own `last_seen_at` instead.
    Disconnected,
    /// A daemon was there and the question still went unanswered.
    Unreachable(UnreachableReason),
    /// The transport answered about a coordinate this request never asked about.
    /// No daemon caused it and no operator can act on it: it is a defect in this
    /// crate, kept out of the reportable arms so it can never be rendered as one.
    TransportContractViolation,
}

/// Why a present daemon could not be read. Closed, and each token is something the
/// renderer distinguishes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnreachableReason {
    Timeout,
    /// The daemon answered, but not with a status this server can interpret.
    /// `host_status` declares no typed refusals, so any refusal that arrives is
    /// itself drift — never a transient worth presenting as recoverable.
    ProtocolError,
    DaemonUnreachable,
}

impl UnreachableReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::ProtocolError => "protocol_error",
            Self::DaemonUnreachable => "daemon_unreachable",
        }
    }
}

/// Host facts together with the coordinate that produced them. Private fields with
/// no constructor, and readable only through [`Self::facts_for`], so a projector
/// cannot serve one machine's facts under another machine's key — including when
/// two machines spell their daemon instance identically.
///
/// ```compile_fail
/// use std::collections::HashSet;
///
/// use silverhand_backend_core::foundation::identity::{DaemonInstanceId, HostId};
/// use silverhand_backend_server::host_runtime::host_control::status_snapshot::StatusSnapshot;
///
/// fn forge() -> StatusSnapshot {
///     StatusSnapshot {
///         host: HostId::parse("host-2").unwrap(),
///         instance: DaemonInstanceId::parse("instance-1").unwrap(),
///         os: "macOS 14.5".to_owned(),
///         os_arch: None,
///         daemon_pid: 1,
///         daemon_version: "1.0.0".to_owned(),
///         started_at: 1,
///         uptime_ms_at_answer: 1,
///         machine: Box::default(),
///         active_agents: HashSet::new(),
///     }
/// }
/// ```
///
/// ```
/// use silverhand_backend_core::foundation::identity::{DaemonInstanceId, HostId};
/// use silverhand_backend_server::host_runtime::host_control::status_snapshot::{
///     SnapshotCoordinateMismatch, SnapshotView, StatusSnapshot,
/// };
///
/// fn read<'a>(
///     snapshot: &'a StatusSnapshot,
///     host: &HostId,
///     instance: &DaemonInstanceId,
/// ) -> Result<SnapshotView<'a>, SnapshotCoordinateMismatch> {
///     snapshot.facts_for(host, instance)
/// }
/// ```
pub struct StatusSnapshot {
    host: HostId,
    instance: DaemonInstanceId,
    os: String,
    os_arch: Option<String>,
    daemon_pid: i64,
    daemon_version: String,
    started_at: i64,
    uptime_ms_at_answer: i64,
    machine: Box<MachineFacts>,
    active_agents: HashSet<AgentId>,
}

/// Machine-wide facts a daemon reports when asked (`host_status.machine.v1`).
/// Cosmetic: an unusable fact is dropped on its own rather than faulting the host,
/// and a used/total pair is kept only whole and consistent.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MachineFacts {
    pub hostname: Option<String>,
    pub ip: Option<String>,
    pub memory: Option<Usage>,
    pub disk: Option<Usage>,
    pub system_uptime_ms_at_answer: Option<i64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Usage {
    pub total_bytes: i64,
    pub used_bytes: i64,
}

impl StatusSnapshot {
    pub fn facts_for(
        &self,
        host: &HostId,
        instance: &DaemonInstanceId,
    ) -> Result<SnapshotView<'_>, SnapshotCoordinateMismatch> {
        if &self.host != host {
            return Err(SnapshotCoordinateMismatch::Host);
        }
        if &self.instance != instance {
            return Err(SnapshotCoordinateMismatch::DaemonInstance);
        }
        Ok(SnapshotView {
            os: &self.os,
            os_arch: self.os_arch.as_deref(),
            daemon_pid: self.daemon_pid,
            daemon_version: &self.daemon_version,
            started_at: self.started_at,
            uptime_ms_at_answer: self.uptime_ms_at_answer,
            machine: &self.machine,
            active_agents: &self.active_agents,
        })
    }
}

/// What a reader gets once it has named the coordinate it is filing under. Carries
/// no coordinate of its own, so there is nothing here to misattribute.
///
/// `uptime_ms_at_answer` is named for what it is — the daemon's own reading at the
/// moment it answered — and is not the number a status surface may serve. Uptime
/// grows while a snapshot is held, so serving it verbatim would report a frozen
/// value that also carries the daemon's clock. Deriving the served figure belongs
/// to whoever holds the snapshot over time.
pub struct SnapshotView<'a> {
    pub os: &'a str,
    pub os_arch: Option<&'a str>,
    pub daemon_pid: i64,
    pub daemon_version: &'a str,
    pub started_at: i64,
    pub uptime_ms_at_answer: i64,
    pub machine: &'a MachineFacts,
    pub active_agents: &'a HashSet<AgentId>,
}

/// Which coordinate a reader named that the snapshot is not about.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotCoordinateMismatch {
    Host,
    DaemonInstance,
}

pub async fn probe(
    dispatch: &OwningHostDataPlane,
    request: StatusSnapshotRequest,
) -> StatusSnapshotOutcome {
    let custody = &request.custody;
    let include_machine = dispatch.reports_machine_facts(custody).unwrap_or(false);
    let answer = match dispatch
        .dispatch(HostDataPlaneRequest {
            custody: custody.clone(),
            command: command::host_status(status_params(include_machine)),
            timeout: PROBE_TIMEOUT,
        })
        .await
    {
        Ok(answer) => answer,
        Err(failure) => return outcome_for(failure),
    };
    // The coordinate is bound before the payload is looked at, so an answer about
    // another machine or another custody cannot hide behind a decode failure.
    let Ok(result) = answer.into_result_for(custody) else {
        return StatusSnapshotOutcome::TransportContractViolation;
    };
    match StatusWire::decode(
        result,
        custody.host().clone(),
        custody.daemon_instance().clone(),
    ) {
        Some(snapshot) => StatusSnapshotOutcome::Answered(snapshot),
        None => StatusSnapshotOutcome::Unreachable(UnreachableReason::ProtocolError),
    }
}

fn outcome_for(failure: HostDataPlaneFailure) -> StatusSnapshotOutcome {
    match failure {
        // TS reaches the same answer by three routes — no admitted connection, a
        // custody replaced between resolution and send, and a socket already closed
        // at send — and reports all three without a reason.
        //
        // Two shapes fold in here that the TS transport cannot produce, because its
        // command queue is unbounded and its correlation counter cannot exhaust: a
        // full control channel and an exhausted command id. Both mean the frame
        // provably never left, which is what this arm says; both are retried by the
        // caller and neither is ever cached.
        HostDataPlaneFailure::NoCurrentDaemon
        | HostDataPlaneFailure::ConnectionGenerationMismatch
        | HostDataPlaneFailure::DaemonInstanceMismatch
        | HostDataPlaneFailure::NotDispatched => StatusSnapshotOutcome::Disconnected,
        HostDataPlaneFailure::TimedOut => {
            StatusSnapshotOutcome::Unreachable(UnreachableReason::Timeout)
        }
        // The daemon answered and this server could not read a status out of it.
        HostDataPlaneFailure::Refused(_) | HostDataPlaneFailure::ProtocolCorruption => {
            StatusSnapshotOutcome::Unreachable(UnreachableReason::ProtocolError)
        }
        // Neither proven answered nor proven not sent. TS has no lock to poison and
        // no ambiguous settle, and folds every residual shape into the same reason.
        HostDataPlaneFailure::Ambiguous | HostDataPlaneFailure::RegistryUnavailable => {
            StatusSnapshotOutcome::Unreachable(UnreachableReason::DaemonUnreachable)
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusWire {
    os: String,
    /// Absent on a daemon old enough not to report it. Nested so an absent key and
    /// an explicit null stay distinguishable: `z.string().optional()` admits the
    /// former and refuses the latter.
    #[serde(default, deserialize_with = "present_arch")]
    os_arch: Option<Option<String>>,
    daemon_pid: i64,
    daemon_version: String,
    started_at: i64,
    uptime_ms: i64,
    active_agent_ids: Vec<String>,
    #[serde(default)]
    hostname: Option<Value>,
    #[serde(default)]
    ip: Option<Value>,
    #[serde(default)]
    memory_total_bytes: Option<Value>,
    #[serde(default)]
    memory_used_bytes: Option<Value>,
    #[serde(default)]
    disk_total_bytes: Option<Value>,
    #[serde(default)]
    disk_used_bytes: Option<Value>,
    #[serde(default)]
    system_uptime_ms: Option<Value>,
}

fn machine_text(value: Option<Value>) -> Option<String> {
    match value? {
        Value::String(text) if !text.is_empty() => Some(text),
        _ => None,
    }
}

fn machine_count(value: Option<&Value>) -> Option<i64> {
    value?
        .as_i64()
        .filter(|count| *count >= 0 && js_safe_integer(*count))
}

fn machine_usage(total: Option<&Value>, used: Option<&Value>) -> Option<Usage> {
    let total_bytes = machine_count(total)?;
    let used_bytes = machine_count(used)?;
    (used_bytes <= total_bytes).then_some(Usage {
        total_bytes,
        used_bytes,
    })
}

impl StatusWire {
    fn decode(value: Value, host: HostId, instance: DaemonInstanceId) -> Option<StatusSnapshot> {
        let wire = serde_json::from_value::<Self>(value).ok()?;
        if ![wire.daemon_pid, wire.started_at, wire.uptime_ms]
            .into_iter()
            .all(js_safe_integer)
        {
            return None;
        }
        // The daemon always composes something, falling back to platform and
        // release. A blank string is a daemon bug, and it is refused here rather
        // than rendered as an empty cell.
        if wire.os.is_empty() {
            return None;
        }
        let os_arch = match wire.os_arch {
            None => None,
            Some(Some(arch)) => Some(arch),
            // A daemon that has the field and blanked it is off-contract in a way
            // an absent key is not.
            Some(None) => return None,
        };
        let machine = MachineFacts {
            hostname: machine_text(wire.hostname),
            ip: machine_text(wire.ip),
            memory: machine_usage(
                wire.memory_total_bytes.as_ref(),
                wire.memory_used_bytes.as_ref(),
            ),
            disk: machine_usage(
                wire.disk_total_bytes.as_ref(),
                wire.disk_used_bytes.as_ref(),
            ),
            system_uptime_ms_at_answer: machine_count(wire.system_uptime_ms.as_ref()),
        };
        Some(StatusSnapshot {
            host,
            instance,
            os: wire.os,
            os_arch,
            daemon_pid: wire.daemon_pid,
            daemon_version: wire.daemon_version,
            started_at: wire.started_at,
            uptime_ms_at_answer: wire.uptime_ms,
            machine: Box::new(machine),
            active_agents: wire
                .active_agent_ids
                .into_iter()
                .filter_map(|raw| AgentId::parse(raw).ok())
                .collect(),
        })
    }
}

/// The real decoder, opened to this crate's tests and nowhere else. The seed layer's
/// tests must hold a snapshot, and a hand-built one could carry facts no wire could
/// produce; `cfg(test)` cannot reach a build that ships, so the seal stands.
#[cfg(test)]
pub(crate) fn decode_for_tests(
    value: Value,
    host: HostId,
    instance: DaemonInstanceId,
) -> Option<StatusSnapshot> {
    StatusWire::decode(value, host, instance)
}

/// Runs only for a key that is present, so the field's own default is what an
/// absent key means and this only has to report what a present one carried.
fn present_arch<'de, D>(deserializer: D) -> Result<Option<Option<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> HostId {
        HostId::parse("host-1").unwrap()
    }

    fn instance() -> DaemonInstanceId {
        DaemonInstanceId::parse("instance-1").unwrap()
    }

    fn status(overrides: Value) -> Value {
        let mut status = json!({
            "os": "macOS 14.5",
            "os_arch": "arm64",
            "daemon_pid": 4_242,
            "daemon_version": "1.2.3",
            "started_at": 1_700_000_000_000_i64,
            "uptime_ms": 90_000,
            "active_agent_ids": ["agent-1"],
        });
        for (key, value) in overrides.as_object().unwrap() {
            status[key] = value.clone();
        }
        status
    }

    fn decode(value: Value) -> Option<StatusSnapshot> {
        StatusWire::decode(value, host(), instance())
    }

    #[test]
    fn every_transport_failure_maps_to_one_outcome_of_this_operation() {
        // The catalog folds these three into "keep waiting for a machine"; this
        // operation reports them as an absent machine, because its read surface has
        // something to say about an absent one and nothing to say about a fault.
        for absent in [
            HostDataPlaneFailure::NoCurrentDaemon,
            HostDataPlaneFailure::ConnectionGenerationMismatch,
            HostDataPlaneFailure::DaemonInstanceMismatch,
            HostDataPlaneFailure::NotDispatched,
        ] {
            assert!(matches!(
                outcome_for(absent),
                StatusSnapshotOutcome::Disconnected
            ));
        }
        assert!(matches!(
            outcome_for(HostDataPlaneFailure::TimedOut),
            StatusSnapshotOutcome::Unreachable(UnreachableReason::Timeout)
        ));
        // A refusal for a command with no typed refusals is drift, and it says the
        // same thing about this question as an unreadable frame: the daemon spoke
        // and this server cannot act on what it said.
        for spoke_unreadably in [
            HostDataPlaneFailure::Refused(CommandRefusal::Response(
                DaemonRefusalCode::ShuttingDown,
            )),
            HostDataPlaneFailure::ProtocolCorruption,
        ] {
            assert!(matches!(
                outcome_for(spoke_unreadably),
                StatusSnapshotOutcome::Unreachable(UnreachableReason::ProtocolError)
            ));
        }
        for residual in [
            HostDataPlaneFailure::Ambiguous,
            HostDataPlaneFailure::RegistryUnavailable,
        ] {
            assert!(matches!(
                outcome_for(residual),
                StatusSnapshotOutcome::Unreachable(UnreachableReason::DaemonUnreachable)
            ));
        }
    }

    #[test]
    fn a_well_formed_status_answers_only_for_the_coordinate_it_was_answered_on() {
        let snapshot = decode(status(json!({}))).expect("status decodes");
        let view = snapshot
            .facts_for(&host(), &instance())
            .expect("its own coordinate reads");
        assert_eq!(view.os, "macOS 14.5");
        assert_eq!(view.os_arch, Some("arm64"));
        assert_eq!(view.daemon_pid, 4_242);
        assert_eq!(view.daemon_version, "1.2.3");
        assert_eq!(view.started_at, 1_700_000_000_000);
        assert_eq!(view.uptime_ms_at_answer, 90_000);
        assert_eq!(
            view.active_agents,
            &HashSet::from([AgentId::parse("agent-1").unwrap()])
        );

        assert!(matches!(
            snapshot.facts_for(&HostId::parse("host-2").unwrap(), &instance()),
            Err(SnapshotCoordinateMismatch::Host)
        ));
        assert!(matches!(
            snapshot.facts_for(&host(), &DaemonInstanceId::parse("instance-2").unwrap()),
            Err(SnapshotCoordinateMismatch::DaemonInstance)
        ));
    }

    #[test]
    fn an_absent_architecture_is_forward_compatible_but_a_blanked_one_is_not() {
        let mut older = status(json!({}));
        older.as_object_mut().unwrap().remove("os_arch");
        let snapshot = decode(older).expect("an older daemon still decodes");
        assert_eq!(
            snapshot.facts_for(&host(), &instance()).unwrap().os_arch,
            None
        );

        assert!(decode(status(json!({ "os_arch": Value::Null }))).is_none());
        assert!(decode(status(json!({ "os_arch": 64 }))).is_none());
    }

    #[test]
    fn a_widened_incomplete_or_fractional_status_is_refused_rather_than_defaulted() {
        let mut widened = status(json!({}));
        widened["unexpected"] = json!(true);
        assert!(decode(widened).is_none());

        for required in [
            "os",
            "daemon_pid",
            "daemon_version",
            "started_at",
            "uptime_ms",
            "active_agent_ids",
        ] {
            let mut missing = status(json!({}));
            missing.as_object_mut().unwrap().remove(required);
            assert!(
                decode(missing).is_none(),
                "expected a status missing {required} to be refused"
            );
        }

        // Integral by contract on both sides: a fractional reading is refused, not
        // rounded into a pid or a timestamp.
        for fractional in [
            json!({ "daemon_pid": 4_242.5 }),
            json!({ "started_at": 1_700_000_000_000.5_f64 }),
            json!({ "uptime_ms": 90_000.5 }),
        ] {
            assert!(
                decode(status(fractional.clone())).is_none(),
                "expected {fractional} to be refused"
            );
        }

        // The daemon helper always composes something; a blank is a daemon bug and
        // must not reach a renderer as an empty cell.
        assert!(decode(status(json!({ "os": "" }))).is_none());
        assert!(decode(status(json!({ "active_agent_ids": "agent-1" }))).is_none());
        assert!(decode(status(json!({ "active_agent_ids": [7] }))).is_none());
    }

    #[test]
    fn machine_facts_are_optional_and_an_unusable_one_is_dropped_alone() {
        let older = decode(status(json!({}))).expect("a status without machine facts decodes");
        assert_eq!(
            older.facts_for(&host(), &instance()).unwrap().machine,
            &MachineFacts::default()
        );

        let snapshot = decode(status(json!({
            "hostname": "studio.local",
            "ip": "10.0.0.7",
            "memory_total_bytes": 64_000,
            "memory_used_bytes": 32_000,
            "disk_total_bytes": 1_000,
            "disk_used_bytes": 400,
            "system_uptime_ms": 5_000,
        })))
        .expect("machine facts decode");
        assert_eq!(
            snapshot.facts_for(&host(), &instance()).unwrap().machine,
            &MachineFacts {
                hostname: Some("studio.local".to_owned()),
                ip: Some("10.0.0.7".to_owned()),
                memory: Some(Usage {
                    total_bytes: 64_000,
                    used_bytes: 32_000
                }),
                disk: Some(Usage {
                    total_bytes: 1_000,
                    used_bytes: 400
                }),
                system_uptime_ms_at_answer: Some(5_000),
            }
        );

        let damaged = decode(status(json!({
            "hostname": "",
            "ip": 7,
            "memory_total_bytes": 100,
            "memory_used_bytes": 200,
            "disk_total_bytes": 1_000,
            "system_uptime_ms": -1,
        })))
        .expect("bad machine facts never fault the host");
        assert_eq!(
            damaged.facts_for(&host(), &instance()).unwrap().machine,
            &MachineFacts::default()
        );
    }

    #[test]
    fn machine_facts_are_requested_only_from_a_daemon_that_advertised_them() {
        assert_eq!(status_params(false), json!({}));
        assert_eq!(status_params(true), json!({ "include_machine": true }));
    }

    #[test]
    fn an_id_no_registration_could_match_is_dropped_instead_of_faulting_the_host() {
        let snapshot = decode(status(
            json!({ "active_agent_ids": ["agent-1", "", "agent-2", "agent-1"] }),
        ))
        .expect("one unusable entry does not cost the whole answer");
        assert_eq!(
            snapshot
                .facts_for(&host(), &instance())
                .unwrap()
                .active_agents,
            &HashSet::from([
                AgentId::parse("agent-1").unwrap(),
                AgentId::parse("agent-2").unwrap(),
            ])
        );
    }
}
