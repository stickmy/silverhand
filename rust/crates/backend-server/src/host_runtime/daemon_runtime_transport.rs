//! Runtime-owner adapters over the sealed generation-bound daemon data plane.

use std::{
    error::Error,
    fmt,
    time::{Duration, SystemTime},
};

use serde_json::{Value, json};
use silverhand_backend_core::{
    conversation_messaging::runtime_delivery::{
        ClaimPendingInbox, ClaimedInboxBatch, DeliveryInstruction, DeliveryLeaseOwner,
        InboxDeliveryId, RuntimeDeliveryFailureReason, RuntimeDispatchReadPort,
        RuntimeDispatchTarget, RuntimeInboxCustodyPort,
    },
    foundation::persistence::PersistenceFailure,
    host_runtime::runtime_credential::RuntimePrincipal,
};

use crate::{
    conversation_messaging::messaging::references::MarkdownReferencePositions,
    foundation::daemon_transport::{
        AttachedHostDataPlane, CommandDispatchError, CommandRefusal, DaemonFeatures,
    },
    host_runtime::daemon_runtime_codecs::{
        runtime_start,
        runtime_stop::{self, RuntimeStopCodecError, RuntimeStopCommand},
        spawn_options, workspace_wire,
    },
    host_runtime::runtime_control::runtime_unavailable,
    host_runtime::runtime_credential::{
        RuntimeDispatchFailure, RuntimeStartHandoff, RuntimeStartTransport, RuntimeStopTransport,
    },
};
use silverhand_backend_core::conversation_messaging::messaging::collaboration::RuntimeUnavailable;

#[derive(Clone)]
pub(crate) struct StartTransport<D> {
    data: D,
    plane: AttachedHostDataPlane,
    target: RuntimeDispatchTarget,
    timeout: Duration,
}

impl<D> StartTransport<D> {
    pub(crate) fn new(
        plane: AttachedHostDataPlane,
        target: RuntimeDispatchTarget,
        timeout: Duration,
        data: D,
    ) -> Self {
        Self {
            plane,
            target,
            timeout,
            data,
        }
    }
}

impl<D: RuntimeDispatchReadPort> RuntimeStartTransport for StartTransport<D> {
    type Error = StartTransportError;

    async fn dispatch(
        &self,
        handoff: &RuntimeStartHandoff,
    ) -> Result<(), RuntimeDispatchFailure<Self::Error>> {
        // Recheck after credential admission, immediately before dispatch. This is
        // the automatic start path's own admission, independent of inbox claiming.
        let current = self
            .data
            .resolve_dispatch_target(
                self.target.conversation.clone(),
                handoff.principal().agent().clone(),
            )
            .await
            .map_err(|error| {
                RuntimeDispatchFailure::ProvenNotDispatched(StartTransportError::Persistence(error))
            })?;
        if current.is_none() {
            return Err(RuntimeDispatchFailure::ProvenNotDispatched(
                StartTransportError::TargetUnavailable,
            ));
        }
        let params =
            runtime_start::encode(&self.target, handoff, self.plane.features()).map_err(|_| {
                RuntimeDispatchFailure::ProvenNotDispatched(StartTransportError::InvalidRequest)
            })?;
        match self
            .plane
            .dispatch(
                crate::foundation::daemon_command::runtime_start(
                    params,
                    crate::host_runtime::runtime_control::admits_runtime_refusal,
                ),
                self.timeout,
            )
            .await
        {
            Ok(result)
                if runtime_start::decode_result(&result)
                    == Some(runtime_start::RuntimeStartResult::Started) =>
            {
                Ok(())
            }
            Ok(result)
                if runtime_start::decode_result(&result)
                    == Some(runtime_start::RuntimeStartResult::InitCooldown) =>
            {
                Err(RuntimeDispatchFailure::ProvenNotDispatched(
                    StartTransportError::Refused,
                ))
            }
            Ok(_) => Err(RuntimeDispatchFailure::Ambiguous(
                StartTransportError::InvalidResponse,
            )),
            Err(CommandDispatchError::ProvenNotDispatched) => Err(
                RuntimeDispatchFailure::ProvenNotDispatched(StartTransportError::Transport),
            ),
            Err(CommandDispatchError::DaemonRefused(CommandRefusal::Capability(refusal)))
                if let Some(unavailable) = runtime_unavailable(&refusal) =>
            {
                Err(RuntimeDispatchFailure::ProvenNotDispatched(
                    StartTransportError::RuntimeUnavailable(unavailable),
                ))
            }
            Err(_) => Err(RuntimeDispatchFailure::Ambiguous(
                StartTransportError::Transport,
            )),
        }
    }
}

#[derive(Debug)]
pub(crate) enum StartTransportError {
    TargetUnavailable,
    Persistence(PersistenceFailure),
    Transport,
    Refused,
    RuntimeUnavailable(RuntimeUnavailable),
    InvalidRequest,
    InvalidResponse,
}

impl fmt::Display for StartTransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::TargetUnavailable => "runtime_start target is no longer available",
            Self::Persistence(_) => "runtime_start admission failed",
            Self::Transport => "runtime_start transport failed",
            Self::Refused => "daemon refused runtime_start",
            Self::RuntimeUnavailable(_) => "runtime CLI is unavailable on the daemon host",
            Self::InvalidRequest => "runtime_start request is outside its wire schema",
            Self::InvalidResponse => "daemon returned an invalid runtime_start response",
        })
    }
}

impl Error for StartTransportError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Persistence(error) => Some(error),
            _ => None,
        }
    }
}

#[derive(Clone)]
pub(crate) struct StopTransport {
    plane: AttachedHostDataPlane,
    timeout: Duration,
}

impl StopTransport {
    pub(crate) fn new(plane: AttachedHostDataPlane, timeout: Duration) -> Self {
        Self { plane, timeout }
    }
}

impl RuntimeStopTransport for StopTransport {
    type Error = StopTransportError;

    async fn stop_exact(&self, principal: &RuntimePrincipal) -> Result<(), Self::Error> {
        let encoded =
            RuntimeStopCommand::encode(&principal.scope(), principal.custody().generation())
                .map_err(StopTransportError::InvalidRequest)?;
        let result = self
            .plane
            .dispatch(
                crate::foundation::daemon_command::runtime_stop(encoded.into_params()),
                self.timeout,
            )
            .await
            .map_err(StopTransportError::Command)?;
        if !runtime_stop::valid_result(&result) {
            return Err(StopTransportError::InvalidResponse);
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) enum StopTransportError {
    Command(CommandDispatchError),
    InvalidRequest(RuntimeStopCodecError),
    InvalidResponse,
}

impl fmt::Display for StopTransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Command(_) => "runtime_stop transport failed",
            Self::InvalidRequest(_) => "runtime_stop target is not wire-safe",
            Self::InvalidResponse => "daemon returned an invalid runtime_stop response",
        })
    }
}

impl Error for StopTransportError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Command(source) => Some(source),
            Self::InvalidRequest(source) => Some(source),
            Self::InvalidResponse => None,
        }
    }
}

pub(crate) struct DeliveryTransport<D> {
    plane: AttachedHostDataPlane,
    target: RuntimeDispatchTarget,
    data: D,
    timeout: Duration,
    lease_ttl_ms: i64,
}

impl<D> DeliveryTransport<D> {
    pub(crate) fn new(
        plane: AttachedHostDataPlane,
        target: RuntimeDispatchTarget,
        data: D,
        timeout: Duration,
        lease_ttl_ms: i64,
    ) -> Self {
        Self {
            plane,
            target,
            data,
            timeout,
            lease_ttl_ms,
        }
    }
}

impl<D> RuntimeStartTransport for DeliveryTransport<D>
where
    D: RuntimeInboxCustodyPort,
{
    type Error = DeliveryTransportError;

    async fn dispatch(
        &self,
        handoff: &RuntimeStartHandoff,
    ) -> Result<(), RuntimeDispatchFailure<Self::Error>> {
        let owner = DeliveryLeaseOwner::parse(self.plane.connection_id().to_owned())
            .expect("registry connection ids are non-empty");
        let batch = self
            .data
            .claim_pending(
                self.plane.host_id().to_owned(),
                handoff.principal().clone(),
                self.target.conversation.clone(),
                owner,
                signed_unix_millis(SystemTime::now()),
                self.lease_ttl_ms,
                &MarkdownReferencePositions,
            )
            .await
            .map_err(|source| {
                RuntimeDispatchFailure::ProvenNotDispatched(DeliveryTransportError::Persistence(
                    source,
                ))
            })?;
        let batch = match batch {
            ClaimPendingInbox::Claimed(batch) => batch,
            ClaimPendingInbox::Empty | ClaimPendingInbox::Archived => {
                return Err(RuntimeDispatchFailure::ProvenNotDispatched(
                    DeliveryTransportError::NoPending,
                ));
            }
            ClaimPendingInbox::StaleCustody => {
                return Err(RuntimeDispatchFailure::ProvenNotDispatched(
                    DeliveryTransportError::StaleCustody,
                ));
            }
            ClaimPendingInbox::Transitioning => {
                return Err(RuntimeDispatchFailure::ProvenNotDispatched(
                    DeliveryTransportError::Transitioning,
                ));
            }
        };
        let params = runtime_deliver_params(&self.target, handoff, &batch, self.plane.features());
        match self
            .plane
            .dispatch(
                crate::foundation::daemon_command::runtime_deliver(
                    params,
                    crate::host_runtime::runtime_control::admits_runtime_refusal,
                ),
                self.timeout,
            )
            .await
        {
            Ok(result) => match delivery_disposition(&result) {
                DeliveryDisposition::Accepted => Ok(()),
                DeliveryDisposition::Refused => {
                    rollback_batch(&self.data, self.plane.host_id(), &batch).await?;
                    Err(RuntimeDispatchFailure::ProvenNotDispatched(
                        DeliveryTransportError::DaemonRefused,
                    ))
                }
                DeliveryDisposition::Invalid => Err(RuntimeDispatchFailure::Ambiguous(
                    DeliveryTransportError::InvalidResponse,
                )),
            },
            Err(CommandDispatchError::ProvenNotDispatched) => {
                rollback_batch(&self.data, self.plane.host_id(), &batch).await?;
                Err(RuntimeDispatchFailure::ProvenNotDispatched(
                    DeliveryTransportError::Transport,
                ))
            }
            Err(CommandDispatchError::DaemonRefused(refusal)) => {
                rollback_batch(&self.data, self.plane.host_id(), &batch).await?;
                let error = match &refusal {
                    CommandRefusal::Capability(refusal) => {
                        runtime_unavailable(refusal).map(DeliveryTransportError::RuntimeUnavailable)
                    }
                    CommandRefusal::Response(_) => None,
                };
                Err(RuntimeDispatchFailure::ProvenNotDispatched(
                    error.unwrap_or(DeliveryTransportError::DaemonRefused),
                ))
            }
            Err(_) => Err(RuntimeDispatchFailure::Ambiguous(
                DeliveryTransportError::Transport,
            )),
        }
    }
}

async fn rollback_batch<D: RuntimeInboxCustodyPort>(
    data: &D,
    host_id: &str,
    batch: &ClaimedInboxBatch,
) -> Result<(), RuntimeDispatchFailure<DeliveryTransportError>> {
    data.settle_failed(
        host_id.to_owned(),
        batch.principal().clone(),
        batch.conversation().clone(),
        batch.owner().clone(),
        batch.delivery_ids(),
        RuntimeDeliveryFailureReason::WriteFailed,
        signed_unix_millis(SystemTime::now()),
    )
    .await
    .map_err(|source| {
        RuntimeDispatchFailure::Ambiguous(DeliveryTransportError::Persistence(source))
    })?;
    Ok(())
}

#[derive(Debug)]
pub(crate) enum DeliveryTransportError {
    NoPending,
    StaleCustody,
    Transitioning,
    DaemonRefused,
    RuntimeUnavailable(RuntimeUnavailable),
    InvalidResponse,
    Transport,
    Persistence(PersistenceFailure),
}

impl fmt::Display for DeliveryTransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::NoPending => "runtime inbox has no pending delivery",
            Self::StaleCustody => "runtime custody changed before inbox claim",
            Self::Transitioning => "runtime transition fenced the inbox claim",
            Self::DaemonRefused => "daemon refused runtime delivery custody",
            Self::RuntimeUnavailable(_) => "runtime CLI is unavailable on the daemon host",
            Self::InvalidResponse => "daemon returned an invalid runtime delivery response",
            Self::Transport => "runtime delivery transport failed",
            Self::Persistence(_) => "runtime delivery persistence failed",
        })
    }
}

impl Error for DeliveryTransportError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Persistence(source) => Some(source),
            _ => None,
        }
    }
}

fn runtime_deliver_params(
    target: &RuntimeDispatchTarget,
    handoff: &RuntimeStartHandoff,
    batch: &ClaimedInboxBatch,
    features: DaemonFeatures,
) -> Value {
    let deliveries = batch
        .deliveries()
        .iter()
        .map(|delivery| {
            json!({
                "delivery_id": delivery.id().as_i64(),
                "seq": delivery.seq(),
                "presentation": {
                    "body": delivery.presentation(),
                    "instruction": instruction_wire(delivery.instruction()),
                }
            })
        })
        .collect::<Vec<_>>();
    json!({
        "conversation_id": target.conversation.as_str(),
        "agent_id": handoff.principal().agent().as_str(),
        "runtime_generation": handoff.principal().custody().generation().as_i64(),
        "target": {
            "conversation_id": target.conversation.as_str(),
            "workspace": target.plan.workspace.as_ref().map(workspace_wire),
        },
        "delivery_ids": batch.delivery_ids().into_iter().map(InboxDeliveryId::as_i64).collect::<Vec<_>>(),
        "deliveries": deliveries,
        "kind": "message",
        "text": compose_delivery_text(batch),
        "spawn_options": spawn_options(&target.plan, handoff.daemon_auth_token(), features),
    })
}

fn instruction_wire(instruction: DeliveryInstruction) -> &'static str {
    match instruction {
        DeliveryInstruction::UserForRecipient => "user_for_recipient",
        DeliveryInstruction::UserForOther => "user_for_other",
        DeliveryInstruction::NonSystemMention => "non_system_mention",
        DeliveryInstruction::System => "system",
        DeliveryInstruction::Other => "other",
    }
}

pub(crate) fn compose_delivery_text(batch: &ClaimedInboxBatch) -> String {
    let deliveries = batch.deliveries();
    let header = if deliveries.len() == 1 {
        "You have one message waiting since you were last active:".to_owned()
    } else {
        format!(
            "You have {} messages waiting since you were last active (oldest first):",
            deliveries.len()
        )
    };
    let body = deliveries
        .iter()
        .map(|delivery| delivery.presentation())
        .collect::<Vec<_>>()
        .join("\n\n---\n\n");
    let instruction = if deliveries
        .iter()
        .any(|delivery| delivery.instruction() == DeliveryInstruction::UserForRecipient)
    {
        "Respond as appropriate — call `silverhand message send` with a brief reply, progress update, blocker question, or final summary before stopping. Complete ALL your work before stopping."
    } else if deliveries
        .iter()
        .any(|delivery| delivery.instruction() == DeliveryInstruction::UserForOther)
    {
        "The user message appears addressed to another agent. Do not call `silverhand message send` unless you own or were assigned the relevant task, or must flag a concrete blocker. Complete ALL work you choose to take before stopping."
    } else if deliveries
        .iter()
        .any(|delivery| delivery.instruction() == DeliveryInstruction::NonSystemMention)
    {
        "Respond as appropriate — call `silverhand message send` before stopping. Complete ALL your work before stopping."
    } else if deliveries
        .iter()
        .all(|delivery| delivery.instruction() == DeliveryInstruction::System)
    {
        "Respond as appropriate. System messages do not require a reply unless they explicitly ask for one."
    } else {
        "Respond as appropriate."
    };
    format!("{header}\n\n{body}\n\n{instruction}")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DeliveryDisposition {
    Accepted,
    Refused,
    Invalid,
}

pub(crate) fn delivery_disposition(value: &Value) -> DeliveryDisposition {
    match value.get("disposition").and_then(Value::as_str) {
        Some("spawning" | "admitted" | "deferred")
            if exact_keys(value, &["disposition", "pid", "started_at"])
                && value.get("pid").and_then(Value::as_i64).is_some()
                && value.get("started_at").and_then(Value::as_i64).is_some() =>
        {
            DeliveryDisposition::Accepted
        }
        Some("refused") => match value.get("reason").and_then(Value::as_str) {
            Some("startup_pending" | "draining")
                if exact_keys(value, &["disposition", "reason"]) =>
            {
                DeliveryDisposition::Refused
            }
            Some("cooldown")
                if exact_keys(value, &["disposition", "reason", "cooldown_until"])
                    && value
                        .get("cooldown_until")
                        .and_then(Value::as_i64)
                        .is_some_and(|value| value > 0) =>
            {
                DeliveryDisposition::Refused
            }
            _ => DeliveryDisposition::Invalid,
        },
        _ => DeliveryDisposition::Invalid,
    }
}

fn exact_keys(value: &Value, keys: &[&str]) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    object.len() == keys.len() && keys.iter().all(|key| object.contains_key(*key))
}

fn signed_unix_millis(time: SystemTime) -> i64 {
    match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_millis()).unwrap_or(i64::MAX),
        Err(error) => -i64::try_from(error.duration().as_millis()).unwrap_or(i64::MAX),
    }
}
