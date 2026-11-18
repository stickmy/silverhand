//! Production runtime-control capability over exact durable seats and owning-host custody.

use std::{
    collections::HashMap,
    error::Error,
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use silverhand_backend_core::{
    conversation_messaging::messaging::collaboration::{
        BoundRuntimeControlSeat, ClearSessionOutcome, ClearSessionProviderRefusal,
        HardStopRuntimeOutcome, HardStopRuntimeProviderRefusal, RuntimeControlPort,
        RuntimeControlSeatPort, RuntimePlacementRefusal, RuntimeSeatTarget, RuntimeStartTarget,
        RuntimeUnavailable, RuntimeUnavailableReason, StartRuntimeOutcome,
        StartRuntimeProviderRefusal, seal_clear_session_commit,
    },
    conversation_messaging::runtime_delivery::{RuntimeDispatchReadPort, RuntimeDispatchTarget},
    foundation::identity::HostId,
    foundation::persistence::PersistenceFailure,
    host_runtime::runtime_credential::{
        RuntimeCustody, RuntimeCustodyPort, RuntimeFinalStatus, RuntimePrincipal, RuntimeScope,
    },
    host_runtime::scope_runtime_stop::{
        ScopeRuntimeStopPort, ScopeRuntimeStopRefusal, ScopeRuntimeStopTarget,
    },
    host_runtime::{
        lifecycle::{HostLifecycleStatus, lifecycle_status},
        read::ComputerReadModel,
    },
};
use tokio::sync::Notify;

use crate::{
    application_orchestration::daemon_delivery::{
        CommandRefusal, DaemonRefusalCode, HostDataPlaneAnswerMismatch, HostDataPlaneCustody,
        HostDataPlaneFailure, HostDataPlaneRequest, OwningHostDataPlane, command,
    },
    host_runtime::daemon_runtime_codecs::{
        runtime_clear_session as runtime_clear_session_codec, runtime_start as runtime_start_codec,
    },
    host_runtime::runtime_credential::{
        RuntimeCredentialAuthority, RuntimeDispatchFailure, RuntimeStartFailure,
        RuntimeStartHandoff, RuntimeStartTransport,
    },
    host_runtime::scope_runtime_stop::{ProductionScopeRuntimeStop, ScopeRuntimeStopFailure},
};

const CLEAR_EXIT_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) const RUNTIME_CAPABILITY_CODES: [&str; 6] = [
    "agent_deleting",
    "workspace_not_provisioned",
    "workspace_bundle_missing",
    "registry_unavailable",
    "runtime_binary_missing",
    "runtime_version_unsupported",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimeRefusalCode {
    AgentDeleting,
    WorkspaceNotProvisioned,
    WorkspaceBundleMissing,
    RegistryUnavailable,
    Unavailable(RuntimeUnavailableReason),
}

impl RuntimeRefusalCode {
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "agent_deleting" => Some(Self::AgentDeleting),
            "workspace_not_provisioned" => Some(Self::WorkspaceNotProvisioned),
            "workspace_bundle_missing" => Some(Self::WorkspaceBundleMissing),
            "registry_unavailable" => Some(Self::RegistryUnavailable),
            "runtime_binary_missing" => {
                Some(Self::Unavailable(RuntimeUnavailableReason::BinaryMissing))
            }
            "runtime_version_unsupported" => Some(Self::Unavailable(
                RuntimeUnavailableReason::VersionUnsupported,
            )),
            _ => None,
        }
    }
}

/// The runtime-unavailable reading of a daemon capability refusal, shared by the
/// manual start and the delivery-triggered start.
pub(crate) fn runtime_unavailable(
    refusal: &crate::foundation::daemon_transport::CapabilityRefusal,
) -> Option<RuntimeUnavailable> {
    match RuntimeRefusalCode::parse(refusal.code())? {
        RuntimeRefusalCode::Unavailable(reason) => Some(RuntimeUnavailable {
            reason,
            message: refusal.message().into(),
        }),
        _ => None,
    }
}

pub(crate) fn admits_runtime_refusal(raw: &str) -> bool {
    RUNTIME_CAPABILITY_CODES.contains(&raw)
}

#[derive(Clone, Default)]
pub(crate) struct RuntimeClearSessionWaiters {
    entries: Arc<Mutex<HashMap<RuntimeScope, Arc<ClearWaiter>>>>,
}

struct ClearWaiter {
    principal: Option<RuntimePrincipal>,
    exited: AtomicBool,
    notify: Notify,
}

impl RuntimeClearSessionWaiters {
    fn begin(
        &self,
        scope: RuntimeScope,
        principal: Option<RuntimePrincipal>,
    ) -> Option<ClearWaitTicket> {
        let entry = Arc::new(ClearWaiter {
            principal,
            exited: AtomicBool::new(false),
            notify: Notify::new(),
        });
        let mut entries = self.entries.lock().expect("clear waiter mutex poisoned");
        if entries.contains_key(&scope) {
            return None;
        }
        entries.insert(scope.clone(), Arc::clone(&entry));
        Some(ClearWaitTicket {
            owner: self.clone(),
            scope,
            entry,
        })
    }

    pub(crate) fn notify_exit(&self, principal: &RuntimePrincipal) {
        let entry = self
            .entries
            .lock()
            .expect("clear waiter mutex poisoned")
            .get(&principal.scope())
            .cloned();
        let Some(entry) = entry else { return };
        if entry.principal.as_ref() != Some(principal) {
            return;
        }
        entry.exited.store(true, Ordering::Release);
        entry.notify.notify_waiters();
    }
}

struct ClearWaitTicket {
    owner: RuntimeClearSessionWaiters,
    scope: RuntimeScope,
    entry: Arc<ClearWaiter>,
}

impl ClearWaitTicket {
    async fn wait(&self) -> Result<(), ()> {
        let notified = self.entry.notify.notified();
        if self.entry.exited.load(Ordering::Acquire) {
            return Ok(());
        }
        tokio::time::timeout(CLEAR_EXIT_TIMEOUT, notified)
            .await
            .map(|_| ())
            .map_err(|_| ())
    }
}

impl Drop for ClearWaitTicket {
    fn drop(&mut self) {
        let mut entries = self
            .owner
            .entries
            .lock()
            .expect("clear waiter mutex poisoned");
        if entries
            .get(&self.scope)
            .is_some_and(|current| Arc::ptr_eq(current, &self.entry))
        {
            entries.remove(&self.scope);
        }
    }
}

#[derive(Clone)]
pub struct ProductionRuntimeControl<C, D, H> {
    credentials: RuntimeCredentialAuthority<C>,
    seats: D,
    computers: H,
    data_plane: OwningHostDataPlane,
    stop: ProductionScopeRuntimeStop<H>,
    waiters: RuntimeClearSessionWaiters,
    timeout: Duration,
}

impl<C, D, H> crate::application_orchestration::daemon_delivery::DaemonDeliveryRuntime<C, D, H>
where
    C: RuntimeCustodyPort,
    D: RuntimeDispatchReadPort + RuntimeControlSeatPort,
    H: ComputerReadModel,
{
    pub fn runtime_control(&self) -> ProductionRuntimeControl<C, D, H> {
        ProductionRuntimeControl::new(
            self.credentials(),
            self.data(),
            self.host_credentials(),
            self.owning_host_data_plane(),
            self.scope_runtime_stop(),
            self.clear_session_waiters(),
            self.command_timeout(),
        )
    }
}

impl<C, D, H> ProductionRuntimeControl<C, D, H> {
    pub(crate) fn new(
        credentials: RuntimeCredentialAuthority<C>,
        seats: D,
        computers: H,
        data_plane: OwningHostDataPlane,
        stop: ProductionScopeRuntimeStop<H>,
        waiters: RuntimeClearSessionWaiters,
        timeout: Duration,
    ) -> Self {
        Self {
            credentials,
            seats,
            computers,
            data_plane,
            stop,
            waiters,
            timeout,
        }
    }
}

#[derive(Debug)]
pub enum RuntimeControlAdapterFailure {
    Persistence(PersistenceFailure),
    HostFactsMismatch,
    SeatFactsMismatch,
    DispatchTargetMismatch,
    MissingSeat,
    StaleSettlement,
    InvalidRequest,
    NotDispatched,
    Ambiguous,
    TimedOut,
    DaemonRefused,
    ProtocolCorruption,
    RegistryUnavailable,
    AuthorityMismatch,
    Infrastructure(Box<dyn Error + Send + Sync>),
    Stop(ScopeRuntimeStopFailure),
}

impl fmt::Display for RuntimeControlAdapterFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Persistence(_) => "runtime-control persistence failed",
            Self::HostFactsMismatch => "runtime-control host facts changed coordinate",
            Self::SeatFactsMismatch => "runtime-control seat facts changed coordinate",
            Self::DispatchTargetMismatch => "runtime start facts changed coordinate",
            Self::MissingSeat => "runtime-control seat is missing",
            Self::StaleSettlement => "runtime-control settlement was superseded",
            Self::InvalidRequest => "runtime-control request is outside the command schema",
            Self::NotDispatched => "runtime-control command was proven not dispatched",
            Self::Ambiguous => "runtime-control command outcome is ambiguous",
            Self::TimedOut => "runtime-control command timed out",
            Self::DaemonRefused => "daemon refused runtime-control command",
            Self::ProtocolCorruption => "daemon returned an invalid runtime-control response",
            Self::RegistryUnavailable => "daemon attachment registry is unavailable",
            Self::AuthorityMismatch => "runtime-control answer crossed its captured custody",
            Self::Infrastructure(_) => "runtime-control infrastructure failed",
            Self::Stop(_) => "runtime stop failed",
        })
    }
}

impl Error for RuntimeControlAdapterFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Persistence(source) => Some(source),
            Self::Stop(source) => Some(source),
            Self::Infrastructure(source) => Some(source.as_ref()),
            _ => None,
        }
    }
}

impl From<PersistenceFailure> for RuntimeControlAdapterFailure {
    fn from(source: PersistenceFailure) -> Self {
        Self::Persistence(source)
    }
}

impl<C, D, H> ProductionRuntimeControl<C, D, H>
where
    C: RuntimeCustodyPort,
    D: RuntimeDispatchReadPort + RuntimeControlSeatPort,
    H: ComputerReadModel,
{
    async fn seat(
        &self,
        target: &RuntimeSeatTarget,
    ) -> Result<BoundRuntimeControlSeat, RuntimeControlAdapterFailure> {
        // The read path is unauthorized and keyed on the row-owned scope; the
        // admission seal stays at this `RuntimeControlPort` boundary and is only
        // handed to the durable write path (`settle_clear`).
        let scope = target.scope();
        self.seats
            .snapshot(scope)
            .await
            .map_err(|source| RuntimeControlAdapterFailure::Infrastructure(Box::new(source)))?
            .ok_or(RuntimeControlAdapterFailure::MissingSeat)?
            .bind(scope)
            .map_err(|_| RuntimeControlAdapterFailure::SeatFactsMismatch)
    }

    async fn placement(
        &self,
        host: &HostId,
    ) -> Result<Result<(), RuntimePlacementRefusal>, RuntimeControlAdapterFailure> {
        let Some(computer) = self
            .computers
            .read_computer(host.clone())
            .await
            .map_err(RuntimeControlAdapterFailure::Persistence)?
        else {
            return Ok(Err(RuntimePlacementRefusal::HostMissing));
        };
        if &computer.id != host {
            return Err(RuntimeControlAdapterFailure::HostFactsMismatch);
        }
        Ok(match lifecycle_status(&computer.lifecycle()) {
            HostLifecycleStatus::Pending | HostLifecycleStatus::Attached => {
                Err(RuntimePlacementRefusal::HostPendingAttach)
            }
            HostLifecycleStatus::Revoked => Err(RuntimePlacementRefusal::HostRevoked),
            HostLifecycleStatus::Active => Ok(()),
        })
    }

    fn resolve_custody(
        &self,
        before: &BoundRuntimeControlSeat,
    ) -> Result<Result<HostDataPlaneCustody, RuntimePlacementRefusal>, RuntimeControlAdapterFailure>
    {
        let custody = self
            .data_plane
            .resolve(before.host())
            .map_err(|_| RuntimeControlAdapterFailure::RegistryUnavailable)?;
        let Some(custody) = custody else {
            return Ok(Err(RuntimePlacementRefusal::HostAttachPending));
        };
        if before.daemon_instance() != Some(custody.daemon_instance()) {
            return Ok(Err(RuntimePlacementRefusal::HostAttachPending));
        }
        Ok(Ok(custody))
    }
}

impl<C, D, H> RuntimeControlPort for ProductionRuntimeControl<C, D, H>
where
    C: RuntimeCustodyPort,
    D: RuntimeDispatchReadPort + RuntimeControlSeatPort,
    H: ComputerReadModel,
{
    type Error = RuntimeControlAdapterFailure;

    async fn start(
        &self,
        target: &RuntimeStartTarget,
    ) -> Result<Result<StartRuntimeOutcome, StartRuntimeProviderRefusal>, Self::Error> {
        // The start intent carries no scope: the arm is owned by the agent row and the
        // presentation origin is read-derived from the participation authority. A
        // non-participant candidate resolves to no seat (zero wire).
        let dispatch = self
            .seats
            .resolve_start_dispatch(target.agent().clone(), target.presentation_origin().clone())
            .await?
            .ok_or(RuntimeControlAdapterFailure::MissingSeat)?;
        let host = HostId::parse(dispatch.host_id.clone())
            .map_err(|_| RuntimeControlAdapterFailure::DispatchTargetMismatch)?;
        // Exact two-axis bind against the resolved fact. Now non-vacuous: the
        // conversation is read-derived (not echoed) and the arm is row-owned, so a
        // fabricated focus cannot slip through the AgentSlot arm.
        if dispatch.conversation != *target.presentation_origin()
            || dispatch.plan.scope.agent() != target.agent()
            || dispatch
                .plan
                .scope
                .source()
                .is_some_and(|source| source != target.presentation_origin())
        {
            return Err(RuntimeControlAdapterFailure::DispatchTargetMismatch);
        }
        if let Err(refusal) = self.placement(&host).await? {
            return Ok(Err(StartRuntimeProviderRefusal::Placement(refusal)));
        }
        let Some(custody) = self
            .data_plane
            .resolve(&host)
            .map_err(|_| RuntimeControlAdapterFailure::RegistryUnavailable)?
        else {
            return Ok(Err(StartRuntimeProviderRefusal::Placement(
                RuntimePlacementRefusal::HostAttachPending,
            )));
        };
        let transport = ProductionStartTransport {
            data_plane: self.data_plane.clone(),
            custody: custody.clone(),
            target: dispatch,
            timeout: self.timeout,
        };
        match self
            .credentials
            .dispatch_start(
                transport.target.plan.scope.clone(),
                custody.daemon_instance().clone(),
                transport.target.conversation.clone(),
                &transport,
            )
            .await
        {
            Ok(_) => Ok(Ok(StartRuntimeOutcome)),
            Err(RuntimeStartFailure::ProvenNotDispatched(StartDispatchError::Cooldown)) => {
                Ok(Ok(StartRuntimeOutcome))
            }
            Err(RuntimeStartFailure::ProvenNotDispatched(StartDispatchError::Product(refusal))) => {
                Ok(Err(refusal))
            }
            Err(error) => Err(RuntimeControlAdapterFailure::Infrastructure(Box::new(
                error,
            ))),
        }
    }

    async fn hard_stop(
        &self,
        target: &RuntimeSeatTarget,
    ) -> Result<Result<HardStopRuntimeOutcome, HardStopRuntimeProviderRefusal>, Self::Error> {
        let before = self.seat(target).await?;
        let stop_target = ScopeRuntimeStopTarget::new(
            before.host().clone(),
            before.scope().clone(),
            before.generation(),
        );
        match self.stop.hard_stop(&stop_target).await {
            Ok(Ok(_)) => Ok(Ok(HardStopRuntimeOutcome)),
            Ok(Err(refusal)) => Ok(Err(HardStopRuntimeProviderRefusal::Placement(
                map_stop_refusal(refusal),
            ))),
            Err(source) => Err(RuntimeControlAdapterFailure::Stop(source)),
        }
    }

    async fn clear_session(
        &self,
        target: &RuntimeSeatTarget,
    ) -> Result<Result<ClearSessionOutcome, ClearSessionProviderRefusal>, Self::Error> {
        let before = self.seat(target).await?;
        if let Err(refusal) = self.placement(before.host()).await? {
            return Ok(Err(ClearSessionProviderRefusal::Placement(refusal)));
        }
        let principal = before.daemon_instance().map(|daemon| {
            RuntimePrincipal::for_scope(
                before.scope().clone(),
                RuntimeCustody::new(before.generation(), daemon.clone()),
            )
        });
        let Some(ticket) = self
            .waiters
            .begin(before.scope().clone(), principal.clone())
        else {
            return Ok(Err(ClearSessionProviderRefusal::InProgress));
        };
        if let Some(principal) = principal {
            let custody = match self.resolve_custody(&before)? {
                Ok(custody) => custody,
                Err(refusal) => {
                    return Ok(Err(ClearSessionProviderRefusal::Placement(refusal)));
                }
            };
            let cleared = match clear_captured(
                &self.data_plane,
                custody,
                before.scope(),
                before.generation(),
                self.timeout,
            )
            .await?
            {
                ClearDispatchOutcome::Cleared(cleared) => cleared,
                ClearDispatchOutcome::AttachPending => {
                    return Ok(Err(ClearSessionProviderRefusal::Placement(
                        RuntimePlacementRefusal::HostAttachPending,
                    )));
                }
            };
            if cleared {
                if ticket.wait().await.is_err() {
                    return Ok(Err(ClearSessionProviderRefusal::Timeout));
                }
            } else {
                let _ = self
                    .credentials
                    .finalize(principal, RuntimeFinalStatus::Idle)
                    .await?;
            }
        }
        let receipt = self
            .seats
            .settle_clear(target, &before)
            .await
            .map_err(|source| RuntimeControlAdapterFailure::Infrastructure(Box::new(source)))?
            .ok_or(RuntimeControlAdapterFailure::StaleSettlement)?;
        seal_clear_session_commit(&before, receipt)
            .map(Ok)
            .map_err(|_| RuntimeControlAdapterFailure::StaleSettlement)
    }
}

fn map_stop_refusal(refusal: ScopeRuntimeStopRefusal) -> RuntimePlacementRefusal {
    match refusal {
        ScopeRuntimeStopRefusal::HostMissing => RuntimePlacementRefusal::HostMissing,
        ScopeRuntimeStopRefusal::HostPendingAttach => RuntimePlacementRefusal::HostPendingAttach,
        ScopeRuntimeStopRefusal::HostRevoked => RuntimePlacementRefusal::HostRevoked,
        ScopeRuntimeStopRefusal::HostAttachPending => RuntimePlacementRefusal::HostAttachPending,
    }
}

struct ProductionStartTransport {
    data_plane: OwningHostDataPlane,
    custody: HostDataPlaneCustody,
    target: RuntimeDispatchTarget,
    timeout: Duration,
}

#[derive(Debug)]
enum StartDispatchError {
    Cooldown,
    Product(StartRuntimeProviderRefusal),
    Hard(&'static str),
}

impl fmt::Display for StartDispatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cooldown => formatter.write_str("runtime start is in initialization cooldown"),
            Self::Product(_) => formatter.write_str("runtime start was refused"),
            Self::Hard(message) => formatter.write_str(message),
        }
    }
}

impl Error for StartDispatchError {}

impl RuntimeStartTransport for ProductionStartTransport {
    type Error = StartDispatchError;

    async fn dispatch(
        &self,
        handoff: &RuntimeStartHandoff,
    ) -> Result<(), RuntimeDispatchFailure<Self::Error>> {
        let features = self.data_plane.features(&self.custody).unwrap_or_default();
        let params =
            runtime_start_codec::encode(&self.target, handoff, features).map_err(|_| {
                RuntimeDispatchFailure::ProvenNotDispatched(StartDispatchError::Hard(
                    "runtime_start request is not wire-safe",
                ))
            })?;
        let expected = self.custody.clone();
        let result = self
            .data_plane
            .dispatch(HostDataPlaneRequest {
                custody: self.custody.clone(),
                command: command::runtime_start(params, admits_runtime_refusal),
                timeout: self.timeout,
            })
            .await;
        let answer = match result {
            Ok(answer) => answer,
            Err(
                HostDataPlaneFailure::NoCurrentDaemon
                | HostDataPlaneFailure::ConnectionGenerationMismatch
                | HostDataPlaneFailure::DaemonInstanceMismatch,
            ) => {
                return Err(RuntimeDispatchFailure::ProvenNotDispatched(
                    StartDispatchError::Product(StartRuntimeProviderRefusal::Placement(
                        RuntimePlacementRefusal::HostAttachPending,
                    )),
                ));
            }
            Err(HostDataPlaneFailure::NotDispatched) => {
                return Err(RuntimeDispatchFailure::ProvenNotDispatched(
                    StartDispatchError::Hard("runtime_start was not dispatched"),
                ));
            }
            Err(HostDataPlaneFailure::Refused(refusal)) => {
                let error = map_start_refusal(refusal).unwrap_or(StartDispatchError::Hard(
                    "runtime_start refusal vocabulary is invalid",
                ));
                return Err(RuntimeDispatchFailure::ProvenNotDispatched(error));
            }
            Err(HostDataPlaneFailure::TimedOut) => {
                return Err(RuntimeDispatchFailure::Ambiguous(StartDispatchError::Hard(
                    "runtime_start timed out",
                )));
            }
            Err(HostDataPlaneFailure::Ambiguous) => {
                return Err(RuntimeDispatchFailure::Ambiguous(StartDispatchError::Hard(
                    "runtime_start outcome is ambiguous",
                )));
            }
            Err(HostDataPlaneFailure::ProtocolCorruption) => {
                return Err(RuntimeDispatchFailure::Ambiguous(StartDispatchError::Hard(
                    "runtime_start response is invalid",
                )));
            }
            Err(HostDataPlaneFailure::RegistryUnavailable) => {
                return Err(RuntimeDispatchFailure::ProvenNotDispatched(
                    StartDispatchError::Hard("runtime_start registry is unavailable"),
                ));
            }
        };
        let answer = answer.into_result_for(&expected).map_err(
            |_mismatch: HostDataPlaneAnswerMismatch| {
                RuntimeDispatchFailure::Ambiguous(StartDispatchError::Hard(
                    "runtime_start answer crossed custody",
                ))
            },
        )?;
        match runtime_start_codec::decode_result(&answer) {
            Some(runtime_start_codec::RuntimeStartResult::Started) => Ok(()),
            Some(runtime_start_codec::RuntimeStartResult::InitCooldown) => Err(
                RuntimeDispatchFailure::ProvenNotDispatched(StartDispatchError::Cooldown),
            ),
            None => Err(RuntimeDispatchFailure::Ambiguous(StartDispatchError::Hard(
                "runtime_start response is invalid",
            ))),
        }
    }
}

fn map_start_refusal(refusal: CommandRefusal) -> Option<StartDispatchError> {
    match refusal {
        CommandRefusal::Capability(refusal) => match RuntimeRefusalCode::parse(refusal.code())? {
            RuntimeRefusalCode::WorkspaceNotProvisioned => Some(StartDispatchError::Product(
                StartRuntimeProviderRefusal::WorkspaceNotProvisioned,
            )),
            RuntimeRefusalCode::WorkspaceBundleMissing => Some(StartDispatchError::Product(
                StartRuntimeProviderRefusal::WorkspaceBundleMissing,
            )),
            RuntimeRefusalCode::RegistryUnavailable => Some(StartDispatchError::Product(
                StartRuntimeProviderRefusal::RegistryUnavailable,
            )),
            RuntimeRefusalCode::Unavailable(reason) => Some(StartDispatchError::Product(
                StartRuntimeProviderRefusal::RuntimeUnavailable(RuntimeUnavailable {
                    reason,
                    message: refusal.message().into(),
                }),
            )),
            RuntimeRefusalCode::AgentDeleting => None,
        },
        CommandRefusal::Response(DaemonRefusalCode::ShuttingDown) => Some(
            StartDispatchError::Product(StartRuntimeProviderRefusal::ShuttingDown),
        ),
        _ => None,
    }
}

pub(crate) async fn clear_captured(
    data_plane: &OwningHostDataPlane,
    custody: HostDataPlaneCustody,
    scope: &RuntimeScope,
    generation: silverhand_backend_core::host_runtime::runtime_credential::RuntimeGeneration,
    timeout: Duration,
) -> Result<ClearDispatchOutcome, RuntimeControlAdapterFailure> {
    let params = runtime_clear_session_codec::encode(scope, generation)
        .map_err(|_| RuntimeControlAdapterFailure::InvalidRequest)?;
    let expected = custody.clone();
    let answer = data_plane
        .dispatch(HostDataPlaneRequest {
            custody,
            command: command::runtime_clear_session(params),
            timeout,
        })
        .await;
    let answer = match answer {
        Ok(answer) => answer,
        Err(
            HostDataPlaneFailure::NoCurrentDaemon
            | HostDataPlaneFailure::ConnectionGenerationMismatch
            | HostDataPlaneFailure::DaemonInstanceMismatch,
        ) => return Ok(ClearDispatchOutcome::AttachPending),
        Err(HostDataPlaneFailure::NotDispatched) => {
            return Err(RuntimeControlAdapterFailure::NotDispatched);
        }
        Err(HostDataPlaneFailure::Ambiguous) => {
            return Err(RuntimeControlAdapterFailure::Ambiguous);
        }
        Err(HostDataPlaneFailure::TimedOut) => return Err(RuntimeControlAdapterFailure::TimedOut),
        Err(HostDataPlaneFailure::Refused(_)) => {
            return Err(RuntimeControlAdapterFailure::DaemonRefused);
        }
        Err(HostDataPlaneFailure::ProtocolCorruption) => {
            return Err(RuntimeControlAdapterFailure::ProtocolCorruption);
        }
        Err(HostDataPlaneFailure::RegistryUnavailable) => {
            return Err(RuntimeControlAdapterFailure::RegistryUnavailable);
        }
    };
    let result =
        answer
            .into_result_for(&expected)
            .map_err(|_mismatch: HostDataPlaneAnswerMismatch| {
                RuntimeControlAdapterFailure::AuthorityMismatch
            })?;
    runtime_clear_session_codec::decode_result(&result)
        .map(ClearDispatchOutcome::Cleared)
        .ok_or(RuntimeControlAdapterFailure::ProtocolCorruption)
}

pub(crate) enum ClearDispatchOutcome {
    Cleared(bool),
    AttachPending,
}

#[cfg(test)]
mod tests {
    use silverhand_backend_core::foundation::identity::{
        AgentId, ConversationId, DaemonInstanceId,
    };

    use super::*;

    fn principal(scope: RuntimeScope, generation: i64, daemon: &str) -> RuntimePrincipal {
        RuntimePrincipal::for_scope(
            scope,
            RuntimeCustody::new(
                silverhand_backend_core::host_runtime::runtime_credential::RuntimeGeneration::new(
                    generation,
                )
                .unwrap(),
                DaemonInstanceId::parse(daemon).unwrap(),
            ),
        )
    }

    #[test]
    fn runtime_unavailable_refusals_map_to_a_product_refusal_carrying_the_daemon_text() {
        for (code, reason) in [
            (
                "runtime_binary_missing",
                RuntimeUnavailableReason::BinaryMissing,
            ),
            (
                "runtime_version_unsupported",
                RuntimeUnavailableReason::VersionUnsupported,
            ),
        ] {
            assert!(admits_runtime_refusal(code));
            let Some(StartDispatchError::Product(refusal)) =
                map_start_refusal(CommandRefusal::capability_with_message(
                    code,
                    "Pi is not installed on this computer.",
                ))
            else {
                panic!("{code} must be a product refusal");
            };
            assert_eq!(
                refusal,
                StartRuntimeProviderRefusal::RuntimeUnavailable(RuntimeUnavailable {
                    reason,
                    message: "Pi is not installed on this computer.".into(),
                })
            );
            assert_eq!(reason.code(), code);
        }
        assert!(
            map_start_refusal(CommandRefusal::Response(DaemonRefusalCode::InternalError)).is_none()
        );
    }

    #[tokio::test]
    async fn waiter_is_exact_single_flight_and_latches_exit_before_await() {
        let waiters = RuntimeClearSessionWaiters::default();
        let scope = RuntimeScope::ConversationSlot {
            agent: AgentId::parse("agent-a").unwrap(),
            source: ConversationId::parse("conversation-a").unwrap(),
        };
        let exact = principal(scope.clone(), 7, "daemon-a");
        let ticket = waiters.begin(scope.clone(), Some(exact.clone())).unwrap();
        assert!(waiters.begin(scope.clone(), Some(exact.clone())).is_none());

        waiters.notify_exit(&principal(scope.clone(), 8, "daemon-a"));
        waiters.notify_exit(&principal(scope.clone(), 7, "daemon-b"));
        assert!(!ticket.entry.exited.load(Ordering::Acquire));
        waiters.notify_exit(&exact);
        ticket.wait().await.unwrap();

        drop(ticket);
        assert!(waiters.begin(scope, Some(exact)).is_some());
    }

    #[tokio::test]
    async fn late_old_exit_does_not_wake_successor_attempt() {
        let waiters = RuntimeClearSessionWaiters::default();
        let scope = RuntimeScope::AgentSlot {
            agent: AgentId::parse("agent-a").unwrap(),
        };
        let old = principal(scope.clone(), 7, "daemon-a");
        drop(waiters.begin(scope.clone(), Some(old.clone())).unwrap());
        let successor = principal(scope.clone(), 8, "daemon-b");
        let ticket = waiters.begin(scope, Some(successor)).unwrap();
        waiters.notify_exit(&old);
        assert!(!ticket.entry.exited.load(Ordering::Acquire));
    }
}
