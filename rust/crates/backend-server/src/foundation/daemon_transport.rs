//! Authenticated daemon attachment, delivery custody and raw websocket transport.

pub(crate) use crate::foundation::daemon_command as command;

use std::{
    collections::HashMap,
    error::Error,
    fmt,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

use axum::{
    Router,
    extract::{
        State, WebSocketUpgrade,
        ws::{CloseFrame, Message, WebSocket},
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;
use serde_json::{Value, json};
use silverhand_backend_core::foundation::identity::{DaemonInstanceId, HostId, UserId};
use tokio::{
    sync::{Notify, mpsc, oneshot, watch},
    time::Instant,
};

use crate::{foundation::bearer::strict_bearer_credential, foundation::error};

use super::daemon_metrics::{self, ObservedDaemonSocket};

use self::command::{CapabilityRefusalAdmission, EncodedDaemonCommand, IssuedDaemonCommandKind};

include!(concat!(env!("OUT_DIR"), "/daemon_admission.rs"));

const MAX_DAEMON_FRAME_BYTES: usize = 1024 * 1024;
const MAX_WORKSPACE_FILE_FRAME_BYTES: usize = 4 * 1024 * 1024;
/// The highest `protocol_version` this server will read out of a `ready` envelope
/// before knowing the schema, mirroring the deployed bound. Above it the number is
/// not treated as a version at all: an unbounded integer from an unauthenticated
/// schema is a corrupt-dialer signal, not an actionable "upgrade this daemon" fact.
const MAX_READABLE_PROTOCOL_VERSION: u32 = 100;
const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
/// The purge waits for runtime quiescence before removing the agent home, matching
/// the TS production data-plane budget rather than the ordinary control round trip.
const DEFAULT_LEASE_TTL: Duration = Duration::from_secs(60);
const DEFAULT_PING_INTERVAL: Duration = Duration::from_secs(30);
const DEFAULT_PONG_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_SHUTDOWN_GRACE: Duration = Duration::from_millis(1_500);

pub(crate) type DaemonOwnerFailure = Box<dyn Error + Send + Sync>;

/// The authenticated identity returned to the raw websocket perimeter.  The
/// credential store and verification policy remain behind the application port.
pub(crate) struct AuthenticatedDaemonHost {
    pub(crate) host_id: String,
    pub(crate) owner_user_id: UserId,
    pub(crate) credential_id: String,
}

pub(crate) trait DaemonHostAuthentication: Send + Sync {
    fn activate<'a>(
        &'a self,
        host: &'a str,
        owner: &'a UserId,
        credential: &'a str,
        now: i64,
    ) -> Pin<Box<dyn Future<Output = Result<bool, DaemonOwnerFailure>> + Send + 'a>>;

    fn authenticate<'a>(
        &'a self,
        token: String,
        observed_at: i64,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Option<AuthenticatedDaemonHost>, DaemonOwnerFailure>>
                + Send
                + 'a,
        >,
    >;
}

/// Host-admission effects are deliberately narrower than the admission owner:
/// the transport reports two edges and receives no ledger or decision object.
pub(crate) trait DaemonProtocolAdmission: Send + Sync {
    fn incompatible(
        &self,
        host_id: &str,
        connection_id: &str,
        offered_protocol_version: u32,
        expected_protocol_version: u32,
        observed_at: i64,
    );

    fn attached(&self, host_id: &str);
}

pub(crate) trait DaemonRuntimeHydration: Send + Sync {
    fn hydrate<'a>(
        &'a self,
        plane: AttachedHostDataPlane,
    ) -> Pin<Box<dyn Future<Output = Result<(), DaemonOwnerFailure>> + Send + 'a>>;
}

pub(crate) trait DaemonHostReadyEffect: Send + Sync {
    fn attached(&self, host: &HostId, connection_id: &str);
}

pub(crate) trait DaemonStatusSeedEffect: Send + Sync {
    fn attached(
        &self,
        host: HostId,
        daemon: DaemonInstanceId,
        connection_id: String,
        owner: UserId,
        authority: DaemonConnectionAuthority,
    );
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DaemonRuntimeEventDisposition {
    Continue,
    Close { code: u16, reason: &'static str },
}

pub(crate) trait DaemonRuntimeEventEffects: Send + Sync {
    fn handle<'a>(
        &'a self,
        host_id: &'a str,
        connection_id: &'a str,
        daemon: &'a DaemonInstanceId,
        kind: String,
        value: Value,
    ) -> Pin<Box<dyn Future<Output = DaemonRuntimeEventDisposition> + Send + 'a>>;

    fn detached(&self, host_id: &str, daemon: &DaemonInstanceId, connection_id: &str);

    fn release_connection<'a>(
        &'a self,
        connection_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;
}

#[derive(Clone)]
pub struct DaemonDeliveryConfig {
    command_timeout: Duration,
    lease_ttl: Duration,
    ping_interval: Duration,
    pong_timeout: Duration,
    shutdown_grace: Duration,
}

#[derive(Debug)]
pub enum DaemonDeliveryBuildError {
    Entropy(getrandom::Error),
}

impl fmt::Display for DaemonDeliveryBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("daemon delivery process epoch generation failed")
    }
}

impl Error for DaemonDeliveryBuildError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Entropy(source) => Some(source),
        }
    }
}

impl DaemonDeliveryConfig {
    pub fn production() -> Self {
        Self {
            command_timeout: DEFAULT_COMMAND_TIMEOUT,
            lease_ttl: DEFAULT_LEASE_TTL,
            ping_interval: DEFAULT_PING_INTERVAL,
            pong_timeout: DEFAULT_PONG_TIMEOUT,
            shutdown_grace: DEFAULT_SHUTDOWN_GRACE,
        }
    }

    fn lease_ttl_ms(&self) -> i64 {
        i64::try_from(self.lease_ttl.as_millis()).unwrap_or(i64::MAX)
    }
}

#[derive(Clone)]
pub(crate) struct DaemonTransport {
    registry: DaemonRegistry,
    config: DaemonDeliveryConfig,
}

impl DaemonTransport {
    pub(crate) fn new(config: DaemonDeliveryConfig) -> Result<Self, DaemonDeliveryBuildError> {
        Ok(Self {
            registry: DaemonRegistry::new()?,
            config,
        })
    }

    pub(crate) fn command_timeout(&self) -> Duration {
        self.config.command_timeout
    }

    pub(crate) fn lease_ttl_ms(&self) -> i64 {
        self.config.lease_ttl_ms()
    }

    pub(crate) fn owning_host_data_plane(&self) -> OwningHostDataPlane {
        OwningHostDataPlane {
            registry: self.registry.clone(),
        }
    }

    pub(crate) fn controller(&self) -> DaemonTransportController {
        DaemonTransportController {
            registry: self.registry.clone(),
            shutdown_grace: self.config.shutdown_grace,
        }
    }

    pub(crate) fn router(
        &self,
        authentication: Arc<dyn DaemonHostAuthentication>,
        admission: Arc<dyn DaemonProtocolAdmission>,
        hydration: Arc<dyn DaemonRuntimeHydration>,
        host_ready: Arc<dyn DaemonHostReadyEffect>,
        status_seed: Arc<dyn DaemonStatusSeedEffect>,
        runtime_events: Arc<dyn DaemonRuntimeEventEffects>,
    ) -> Router {
        Router::new()
            .route(HOST_WEBSOCKET_PATH, get(daemon_websocket))
            .with_state(DaemonWebsocketState {
                transport: self.clone(),
                authentication,
                admission,
                hydration,
                host_ready,
                status_seed,
                runtime_events,
            })
    }

    #[cfg(test)]
    pub(crate) fn registry_for_test(&self) -> &DaemonRegistry {
        &self.registry
    }
}

#[derive(Clone)]
struct DaemonWebsocketState {
    transport: DaemonTransport,
    authentication: Arc<dyn DaemonHostAuthentication>,
    admission: Arc<dyn DaemonProtocolAdmission>,
    hydration: Arc<dyn DaemonRuntimeHydration>,
    host_ready: Arc<dyn DaemonHostReadyEffect>,
    status_seed: Arc<dyn DaemonStatusSeedEffect>,
    runtime_events: Arc<dyn DaemonRuntimeEventEffects>,
}

#[derive(Clone)]
struct DaemonAttachment {
    host_id: String,
    connection_id: String,
    daemon_instance: DaemonInstanceId,
    host_name: String,
    commands: mpsc::Sender<ConnectionControl>,
}

/// Generation-bound typed command capability handed to context owners. The raw
/// sender, correlation state and wire frame remain private to this transport;
/// owners can submit only a sealed command vocabulary value.
#[derive(Clone)]
pub(crate) struct AttachedHostDataPlane {
    attachment: DaemonAttachment,
}

impl AttachedHostDataPlane {
    fn new(attachment: DaemonAttachment) -> Self {
        Self { attachment }
    }

    pub(crate) fn host_id(&self) -> &str {
        &self.attachment.host_id
    }

    pub(crate) fn connection_id(&self) -> &str {
        &self.attachment.connection_id
    }

    pub(crate) fn daemon_instance(&self) -> &DaemonInstanceId {
        &self.attachment.daemon_instance
    }

    pub(crate) async fn dispatch(
        &self,
        command: EncodedDaemonCommand,
        timeout: Duration,
    ) -> Result<Value, CommandDispatchError> {
        dispatch_attachment(&self.attachment, command, timeout).await
    }
}

#[derive(Clone)]
struct AttachedDaemon {
    daemon_instance: DaemonInstanceId,
    host_name: String,
}

#[derive(Clone)]
pub(crate) struct DaemonRegistry {
    state: Arc<Mutex<RegistryState>>,
    changed: Arc<Notify>,
    process_epoch: Arc<str>,
}

#[derive(Default)]
struct RegistryState {
    closing: bool,
    next_connection: u64,
    slots: HashMap<String, DaemonSlot>,
}

struct DaemonSlot {
    connection_id: String,
    commands: mpsc::Sender<ConnectionControl>,
    close: watch::Sender<Option<ConnectionClose>>,
    attached: Option<AttachedDaemon>,
}

#[derive(Clone)]
struct GenerationCloseControl {
    connection_id: String,
    sender: watch::Sender<Option<ConnectionClose>>,
}

impl DaemonRegistry {
    fn new() -> Result<Self, DaemonDeliveryBuildError> {
        let mut entropy = [0_u8; 16];
        getrandom::fill(&mut entropy).map_err(DaemonDeliveryBuildError::Entropy)?;
        Ok(Self::with_process_epoch(URL_SAFE_NO_PAD.encode(entropy)))
    }

    fn with_process_epoch(process_epoch: String) -> Self {
        Self {
            state: Arc::new(Mutex::new(RegistryState::default())),
            changed: Arc::new(Notify::new()),
            process_epoch: process_epoch.into(),
        }
    }

    fn reserve(
        &self,
        host_id: &str,
        commands: mpsc::Sender<ConnectionControl>,
        close: watch::Sender<Option<ConnectionClose>>,
    ) -> Result<String, ReserveConnectionError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| ReserveConnectionError::Poisoned)?;
        if state.closing {
            return Err(ReserveConnectionError::Closing);
        }
        if state.slots.contains_key(host_id) {
            return Err(ReserveConnectionError::Occupied);
        }
        state.next_connection = state
            .next_connection
            .checked_add(1)
            .ok_or(ReserveConnectionError::Exhausted)?;
        let connection_id = format!(
            "daemon-connection-{}-{}",
            self.process_epoch, state.next_connection
        );
        state.slots.insert(
            host_id.to_owned(),
            DaemonSlot {
                connection_id: connection_id.clone(),
                commands,
                close,
                attached: None,
            },
        );
        self.changed.notify_waiters();
        Ok(connection_id)
    }

    fn attach(
        &self,
        host_id: &str,
        connection_id: &str,
        attached: AttachedDaemon,
    ) -> Result<bool, DaemonRegistryError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| DaemonRegistryError::Poisoned)?;
        let Some(slot) = state.slots.get_mut(host_id) else {
            return Ok(false);
        };
        if slot.connection_id != connection_id {
            return Ok(false);
        }
        slot.attached = Some(attached);
        self.changed.notify_waiters();
        Ok(true)
    }

    fn attachment(&self, host_id: &str) -> Result<Option<DaemonAttachment>, DaemonRegistryError> {
        let state = self
            .state
            .lock()
            .map_err(|_| DaemonRegistryError::Poisoned)?;
        let Some(slot) = state.slots.get(host_id) else {
            return Ok(None);
        };
        let Some(attached) = slot.attached.clone() else {
            return Ok(None);
        };
        Ok(Some(DaemonAttachment {
            host_id: host_id.to_owned(),
            connection_id: slot.connection_id.clone(),
            daemon_instance: attached.daemon_instance,
            host_name: attached.host_name,
            commands: slot.commands.clone(),
        }))
    }

    /// Snapshot exactly the generation that holds `host_id` now.
    ///
    /// The connection id plus its private channel are one generation-bound capability.
    /// If that generation disappears after this lock is released, a replacement gets a
    /// different channel; sending through the captured value can therefore never close
    /// the replacement. Cleanup keeps the slot until the socket task actually exits, so
    /// two live transports are not briefly admitted for one host.
    fn current_close_control(
        &self,
        host_id: &str,
    ) -> Result<Option<GenerationCloseControl>, DaemonRegistryError> {
        Ok(self
            .state
            .lock()
            .map_err(|_| DaemonRegistryError::Poisoned)?
            .slots
            .get(host_id)
            .map(|slot| GenerationCloseControl {
                connection_id: slot.connection_id.clone(),
                sender: slot.close.clone(),
            }))
    }

    fn remove(&self, host_id: &str, connection_id: &str) {
        if let Ok(mut state) = self.state.lock()
            && state
                .slots
                .get(host_id)
                .is_some_and(|slot| slot.connection_id == connection_id)
        {
            state.slots.remove(host_id);
        }
        self.changed.notify_waiters();
    }

    fn active_count(&self) -> usize {
        self.state.lock().map_or(0, |state| state.slots.len())
    }

    fn attached_count(&self) -> usize {
        self.state.lock().map_or(0, |state| {
            state
                .slots
                .values()
                .filter(|slot| slot.attached.is_some())
                .count()
        })
    }

    /// Who holds this host's slot right now, reserved or attached.
    ///
    /// Deliberately narrower than [`Self::attachment`]: it answers only the
    /// question the admission rule asks, and carries neither the command sender nor
    /// the daemon instance, so no caller can grow a second use out of it.
    fn slot_connection_id(&self, host_id: &str) -> Result<Option<String>, DaemonRegistryError> {
        let state = self
            .state
            .lock()
            .map_err(|_| DaemonRegistryError::Poisoned)?;
        Ok(state
            .slots
            .get(host_id)
            .map(|slot| slot.connection_id.clone()))
    }
}

#[derive(Debug)]
enum ReserveConnectionError {
    Closing,
    Occupied,
    Exhausted,
    Poisoned,
}

#[derive(Debug)]
pub enum DaemonRegistryError {
    Poisoned,
}

impl fmt::Display for DaemonRegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("daemon attachment registry is unavailable")
    }
}

impl Error for DaemonRegistryError {}

#[derive(Clone)]
pub(crate) struct DaemonTransportController {
    registry: DaemonRegistry,
    shutdown_grace: Duration,
}

impl DaemonTransportController {
    pub(crate) fn owning_host_data_plane(&self) -> OwningHostDataPlane {
        OwningHostDataPlane {
            registry: self.registry.clone(),
        }
    }

    pub(crate) fn connection_authority(&self) -> DaemonConnectionAuthority {
        DaemonConnectionAuthority {
            registry: self.registry.clone(),
        }
    }
}

#[derive(Clone)]
pub(crate) struct DaemonConnectionAuthority {
    registry: DaemonRegistry,
}

impl DaemonConnectionAuthority {
    pub(crate) fn connection_id(
        &self,
        host: &HostId,
    ) -> Result<Option<String>, DaemonRegistryError> {
        self.registry.slot_connection_id(host.as_str())
    }

    pub(crate) async fn wait_for_same_connection(
        &self,
        host: &HostId,
        connection_id: &str,
        after: Duration,
    ) -> bool {
        wait_for_seed_retry_or_replacement(&self.registry, host, connection_id, after).await
    }

    pub(crate) fn connected_host_name(&self, host: &HostId) -> Option<String> {
        self.registry
            .attachment(host.as_str())
            .ok()
            .flatten()
            .map(|attachment| attachment.host_name)
    }

    pub(crate) fn request_host_close(
        &self,
        host: &HostId,
        reason: HostCloseReason,
    ) -> Result<bool, DaemonRegistryError> {
        let current = self.registry.current_close_control(host.as_str())?;
        let Some(current) = current else {
            return Ok(false);
        };
        let reason = match reason {
            HostCloseReason::Revoked => "host_revoked",
            HostCloseReason::UserManaged => "host_disconnected",
        };
        let _ = current.sender.send(Some(ConnectionClose {
            expected_connection_id: Some(current.connection_id),
            reason,
        }));
        Ok(true)
    }
}

impl DaemonTransportController {
    pub(crate) fn active_connection_count(&self) -> usize {
        self.registry.active_count()
    }

    pub(crate) fn attached_connection_count(&self) -> usize {
        self.registry.attached_count()
    }

    /// Whether the delivery transport currently holds an attachment for one host.
    /// This reads the SAME slot the delivery dispatch gate consults — a slot is
    /// attached exactly when it carries a hydrated `daemon_instance` — so an
    /// availability read and the delivery path observe one fact rather than two
    /// projections that can drift.
    pub(crate) fn host_attached(&self, host_id: &str) -> Result<bool, DaemonRegistryError> {
        Ok(self.registry.attachment(host_id)?.is_some())
    }

    pub(crate) async fn close(&self) {
        let senders = {
            let Ok(mut state) = self.registry.state.lock() else {
                return;
            };
            state.closing = true;
            state
                .slots
                .values()
                .map(|slot| slot.close.clone())
                .collect::<Vec<_>>()
        };
        for sender in senders {
            let _ = sender.send(Some(ConnectionClose {
                expected_connection_id: None,
                reason: "server_shutdown",
            }));
        }
        let deadline = Instant::now() + self.shutdown_grace;
        while self.active_connection_count() != 0 {
            if tokio::time::timeout_at(deadline, self.registry.changed.notified())
                .await
                .is_err()
            {
                return;
            }
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum HostCloseReason {
    Revoked,
    UserManaged,
}

enum ConnectionControl {
    Command(CommandRequest),
    Hydrated(Result<AttachedDaemon, DaemonOwnerFailure>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ConnectionClose {
    expected_connection_id: Option<String>,
    reason: &'static str,
}

struct CommandRequest {
    kind: IssuedDaemonCommandKind,
    params: Value,
    capability_refusal: CapabilityRefusalAdmission,
    deadline: Instant,
    response: oneshot::Sender<Result<Value, CommandDispatchError>>,
}

fn command_expired_before_send(command: &CommandRequest) -> bool {
    command.response.is_closed() || Instant::now() >= command.deadline
}

trait CommandWireSink {
    async fn send(&mut self, message: Message, deadline: Instant) -> InterruptibleSend;
}

enum DequeuedCommandSend {
    Skipped,
    Sent {
        id: String,
        kind: IssuedDaemonCommandKind,
        capability_refusal: CapabilityRefusalAdmission,
        deadline: Instant,
        response: oneshot::Sender<Result<Value, CommandDispatchError>>,
    },
    Terminate,
    Close(ConnectionClose),
}

/// The sole dequeue-to-wire decision for a business command. The caller has
/// removed `command` from the FIFO, but this function alone decides whether it is
/// still admissible at the wire boundary. Tests drive the same branch through a
/// controlled sink, so removing the expiry/cancellation gate makes a late command
/// observable as a sink write.
async fn send_dequeued_command<S: CommandWireSink>(
    command: CommandRequest,
    next_command: &mut u64,
    sink: &mut S,
) -> DequeuedCommandSend {
    if command_expired_before_send(&command) {
        let _ = command.response.send(Err(CommandDispatchError::TimedOut));
        return DequeuedCommandSend::Skipped;
    }
    *next_command = match next_command.checked_add(1) {
        Some(next) => next,
        None => {
            let _ = command
                .response
                .send(Err(CommandDispatchError::ProvenNotDispatched));
            return DequeuedCommandSend::Skipped;
        }
    };
    let id = format!("rust-daemon-{next_command}");
    let frame = json!({
        "direction": "server_to_daemon",
        "type": "command",
        "id": id,
        "kind": command.kind.as_wire(),
        "params": command.params,
    });
    let encoded = frame.to_string();
    if encoded.len() > MAX_DAEMON_FRAME_BYTES {
        let _ = command
            .response
            .send(Err(CommandDispatchError::ProvenNotDispatched));
        return DequeuedCommandSend::Skipped;
    }
    match sink
        .send(Message::Text(encoded.into()), command.deadline)
        .await
    {
        InterruptibleSend::Sent => DequeuedCommandSend::Sent {
            id,
            kind: command.kind,
            capability_refusal: command.capability_refusal,
            deadline: command.deadline,
            response: command.response,
        },
        InterruptibleSend::Expired => {
            let _ = command.response.send(Err(CommandDispatchError::TimedOut));
            DequeuedCommandSend::Terminate
        }
        InterruptibleSend::TransportFailed | InterruptibleSend::Terminated => {
            let _ = command.response.send(Err(CommandDispatchError::Ambiguous));
            DequeuedCommandSend::Terminate
        }
        InterruptibleSend::Close(requested) => {
            let _ = command.response.send(Err(CommandDispatchError::Ambiguous));
            DequeuedCommandSend::Close(requested)
        }
    }
}

struct PendingCommand {
    kind: IssuedDaemonCommandKind,
    capability_refusal: CapabilityRefusalAdmission,
    response: oneshot::Sender<Result<Value, CommandDispatchError>>,
    deadline: Instant,
}

#[derive(Debug)]
pub(crate) enum CommandDispatchError {
    ProvenNotDispatched,
    Ambiguous,
    TimedOut,
    DaemonRefused(CommandRefusal),
    InvalidResponse,
}

impl fmt::Display for CommandDispatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ProvenNotDispatched => "daemon command was proven not dispatched",
            Self::Ambiguous => "daemon command outcome is ambiguous",
            Self::TimedOut => "daemon command timed out",
            Self::DaemonRefused(_) => "daemon refused the command",
            Self::InvalidResponse => "daemon returned an invalid command response",
        })
    }
}

impl Error for CommandDispatchError {}

/// The `ResponseError.code` union every command kind may be refused with. Closed
/// as a type so a token outside it can never reach a caller as a reportable
/// refusal: an unknown code is a corrupt frame, and the decode site fails the
/// connection over it instead of inventing a refusal the caller has no arm for.
/// Private to this module because that admission is all it is for — no caller,
/// here or in the TS contract, acts on which of the five arrived.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DaemonRefusalCode {
    UnknownIntent,
    InvalidParams,
    InternalError,
    ShuttingDown,
    RequestTooLarge,
}

impl DaemonRefusalCode {
    const ALL_WIRE: [&'static str; 5] = [
        "unknown_intent",
        "invalid_params",
        "internal_error",
        "shutting_down",
        "request_too_large",
    ];

    fn parse(raw: &str) -> Option<Self> {
        match Self::ALL_WIRE
            .iter()
            .position(|candidate| *candidate == raw)?
        {
            0 => Some(Self::UnknownIntent),
            1 => Some(Self::InvalidParams),
            2 => Some(Self::InternalError),
            3 => Some(Self::ShuttingDown),
            4 => Some(Self::RequestTooLarge),
            _ => None,
        }
    }
}

/// A refusal admitted by the transport. Generic daemon codes remain typed here;
/// capability codes stay opaque until the context owner decodes its own vocabulary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CommandRefusal {
    Response(DaemonRefusalCode),
    Capability(CapabilityRefusal),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CapabilityRefusal {
    code: Box<str>,
    /// The daemon's own `message` string. Retained (not discarded) so a consumer that
    /// mirrors TS `mapDataPlaneError` can surface the daemon's message rather than a
    /// canned one. Callers that prefer a product-authored message simply
    /// ignore it.
    message: Box<str>,
}

impl CapabilityRefusal {
    pub(crate) fn code(&self) -> &str {
        &self.code
    }

    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

impl CommandRefusal {
    fn decode(capability_refusal: CapabilityRefusalAdmission, value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        let message = object.get("message").and_then(Value::as_str);
        if object.len() != 2 || message.is_none() {
            return None;
        }
        let message = message?;
        let code = object.get("code").and_then(Value::as_str)?;
        if let Some(code) = DaemonRefusalCode::parse(code) {
            return Some(Self::Response(code));
        }
        if capability_refusal(code) {
            return Some(Self::Capability(CapabilityRefusal {
                code: code.into(),
                message: message.into(),
            }));
        }
        None
    }

    #[cfg(test)]
    pub(crate) fn capability(code: &str) -> Self {
        Self::Capability(CapabilityRefusal {
            code: code.into(),
            message: "daemon capability refusal".into(),
        })
    }

    #[cfg(test)]
    pub(crate) fn capability_with_message(code: &str, message: &str) -> Self {
        Self::Capability(CapabilityRefusal {
            code: code.into(),
            message: message.into(),
        })
    }
}

impl DaemonAttachment {
    #[tracing::instrument(
        target = "silverhand::diagnostics",
        name = "daemon.command",
        skip_all,
        fields(
            elapsed_ms = tracing::field::Empty,
            outcome = tracing::field::Empty,
            operation_finished = tracing::field::Empty,
            command = command.kind().as_wire(),
        )
    )]
    async fn command(
        &self,
        command: EncodedDaemonCommand,
        timeout: Duration,
    ) -> Result<Value, CommandDispatchError> {
        let observation = silverhand_backend_observability::Operation::current();
        let result = async {
            let (kind, params, capability_refusal) = command.into_wire_parts();
            let started = Instant::now();
            let result = async {
                let deadline = Instant::now()
                    .checked_add(timeout)
                    .ok_or(CommandDispatchError::TimedOut)?;
                let (response, receive) = oneshot::channel();
                self.commands
                    .try_send(ConnectionControl::Command(CommandRequest {
                        kind,
                        params,
                        capability_refusal,
                        deadline,
                        response,
                    }))
                    .map_err(|_| CommandDispatchError::ProvenNotDispatched)?;
                match tokio::time::timeout_at(deadline, receive).await {
                    Ok(Ok(result)) => result,
                    Ok(Err(_)) => Err(CommandDispatchError::Ambiguous),
                    Err(_) => Err(CommandDispatchError::TimedOut),
                }
            }
            .await;
            let outcome = match &result {
                Ok(_) => "ok",
                Err(CommandDispatchError::TimedOut) => "timeout",
                Err(
                    CommandDispatchError::ProvenNotDispatched | CommandDispatchError::Ambiguous,
                ) => "disconnected",
                Err(_) => "error",
            };
            metrics::histogram!(
                "silverhand_daemon_command_duration_seconds",
                "command" => kind.as_wire(),
                "outcome" => outcome,
            )
            .record(started.elapsed().as_secs_f64());
            result
        }
        .await;
        observation.finish(&result);
        result
    }
}

/// One owning-host data-plane round trip. Crate-private on purpose: a raw command pairs a
/// kind with a free-form payload and a budget, so anything able to build one could
/// send a legal kind with a payload that kind has no contract for. Only the typed
/// command adapters may state that pairing.
pub(crate) struct HostDataPlaneRequest {
    pub(crate) custody: HostDataPlaneCustody,
    pub(crate) command: EncodedDaemonCommand,
    pub(crate) timeout: Duration,
}

/// The host and daemon instance captured together from the live registry.
/// Fields and constructor are private: a route cannot cross-wire host A with an
/// instance it observed on host B, and a command cannot change the host after
/// resolution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostDataPlaneCustody {
    host: HostId,
    instance: DaemonInstanceId,
    connection_id: String,
}

impl HostDataPlaneCustody {
    pub(crate) fn captured(
        host: HostId,
        instance: DaemonInstanceId,
        connection_id: String,
    ) -> Self {
        Self {
            host,
            instance,
            connection_id,
        }
    }

    pub fn host(&self) -> &HostId {
        &self.host
    }

    pub fn daemon_instance(&self) -> &DaemonInstanceId {
        &self.instance
    }

    pub(crate) fn connection_id(&self) -> &str {
        &self.connection_id
    }
}

/// A daemon answer together with the coordinate it was answered on. The pair is
/// the transport's own — the host is the registry key whose slot served the
/// command, the instance the custody that slot was attached with — and the fields
/// are private with no way to build one from parts.
///
/// The only opening is [`Self::into_result_for`], so a consumer cannot take a
/// value out without naming the coordinate it awaited: one host's legal answer can
/// never be filed under another host's key, not even when both machines happen to
/// spell their daemon instance the same way.
pub(crate) struct HostDataPlaneAnswer {
    host: HostId,
    instance: DaemonInstanceId,
    connection_id: String,
    result: Value,
}

impl HostDataPlaneAnswer {
    pub(crate) fn into_result_for(
        self,
        custody: &HostDataPlaneCustody,
    ) -> Result<Value, HostDataPlaneAnswerMismatch> {
        if self.host != custody.host {
            return Err(HostDataPlaneAnswerMismatch::Host);
        }
        if self.connection_id != custody.connection_id {
            return Err(HostDataPlaneAnswerMismatch::ConnectionGeneration);
        }
        if self.instance != custody.instance {
            return Err(HostDataPlaneAnswerMismatch::DaemonInstance);
        }
        Ok(self.result)
    }
}

/// Which coordinate of an answer failed to match the request that awaited it.
/// Never a refusal an operation may report: an operation asked about one machine's
/// custody and was handed another's, which no caller can act on and no daemon
/// caused. It is this crate's own contract failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HostDataPlaneAnswerMismatch {
    Host,
    ConnectionGeneration,
    DaemonInstance,
}

#[derive(Debug)]
pub(crate) enum HostDataPlaneFailure {
    NoCurrentDaemon,
    ConnectionGenerationMismatch,
    DaemonInstanceMismatch,
    NotDispatched,
    Ambiguous,
    TimedOut,
    /// The daemon answered with a code the closed set admits for this kind. Which
    /// code it was is deliberately not carried: no reader in this crate — and no
    /// consumer in this crate distinguishes them. What the closed set is
    /// load-bearing for is the arm itself: a refusal leaves the connection alive,
    /// and anything outside the set does not.
    Refused(CommandRefusal),
    ProtocolCorruption,
    RegistryUnavailable,
}

impl HostDataPlaneFailure {
    fn from_dispatch(error: CommandDispatchError) -> Self {
        match error {
            CommandDispatchError::ProvenNotDispatched => Self::NotDispatched,
            CommandDispatchError::Ambiguous => Self::Ambiguous,
            CommandDispatchError::TimedOut => Self::TimedOut,
            CommandDispatchError::DaemonRefused(refusal) => Self::Refused(refusal),
            CommandDispatchError::InvalidResponse => Self::ProtocolCorruption,
        }
    }
}

impl fmt::Display for HostDataPlaneFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::NoCurrentDaemon => "host has no attached daemon",
            Self::ConnectionGenerationMismatch => {
                "host daemon connection generation changed before dispatch"
            }
            Self::DaemonInstanceMismatch => "host daemon custody changed before dispatch",
            Self::NotDispatched => "host command was proven not dispatched",
            Self::Ambiguous => "host command outcome is ambiguous",
            Self::TimedOut => "host command timed out",
            Self::Refused(_) => "daemon refused the host command",
            Self::ProtocolCorruption => "daemon answered the host command with a corrupt frame",
            Self::RegistryUnavailable => "daemon attachment registry is unavailable",
        })
    }
}

impl Error for HostDataPlaneFailure {}

/// The one production owner for correlated outbound commands to a named host.
/// It resolves a generation-bound custody from the live daemon registry, and the
/// raw round trip remains crate-private so route schemas stay in typed adapters.
///
/// ```compile_fail
/// use silverhand_backend_server::application_orchestration::daemon_delivery::HostDataPlaneRequest;
///
/// fn forge() -> HostDataPlaneRequest {
///     unreachable!()
/// }
/// ```
///
/// ```compile_fail
/// use silverhand_backend_server::application_orchestration::daemon_delivery::OwningHostDataPlane;
///
/// // Names no crate-private type, so nothing here fails for a naming reason. The
/// // round trip stays unreachable while ANY part of it — the parameter, the answer,
/// // the failure, or the method — is crate-private: publishing the whole face is
/// // what turns this green.
/// async fn send(dispatch: &OwningHostDataPlane) {
///     let _ = dispatch.dispatch(unreachable!()).await;
/// }
/// ```
///
/// ```
/// use silverhand_backend_core::foundation::identity::HostId;
/// use silverhand_backend_server::{
///     application_orchestration::daemon_delivery::{
///         DaemonDeliveryController, OwningHostDataPlane,
///     },
///     host_runtime::host_control::runtime_catalog::{
///         self, RuntimeCatalogOutcome, RuntimeCatalogRequest,
///     },
/// };
///
/// async fn probe(dispatch: &OwningHostDataPlane) -> RuntimeCatalogOutcome {
///     let _ = DaemonDeliveryController::owning_host_data_plane;
///     let custody = dispatch
///         .resolve(&HostId::parse("host-1").unwrap())
///         .unwrap()
///         .expect("host is attached");
///     runtime_catalog::probe(
///         dispatch,
///         RuntimeCatalogRequest::new(custody, true),
///     )
///     .await
/// }
/// ```
#[derive(Clone)]
pub struct OwningHostDataPlane {
    registry: DaemonRegistry,
}

impl OwningHostDataPlane {
    pub(crate) fn capture_attached(
        &self,
        host: &str,
    ) -> Result<Option<AttachedHostDataPlane>, DaemonRegistryError> {
        Ok(self
            .registry
            .attachment(host)?
            .map(AttachedHostDataPlane::new))
    }

    /// Resolve the attached custody for `host`. `None` is the ordinary offline
    /// answer. A later replacement is caught again inside `dispatch` before send.
    pub fn resolve(
        &self,
        host: &HostId,
    ) -> Result<Option<HostDataPlaneCustody>, DaemonRegistryError> {
        Ok(self
            .registry
            .attachment(host.as_str())?
            .map(|attachment| HostDataPlaneCustody {
                host: host.clone(),
                instance: attachment.daemon_instance,
                connection_id: attachment.connection_id,
            }))
    }

    pub(crate) async fn dispatch(
        &self,
        command: HostDataPlaneRequest,
    ) -> Result<HostDataPlaneAnswer, HostDataPlaneFailure> {
        let host = command.custody.host;
        let expected_instance = command.custody.instance;
        let expected_connection_id = command.custody.connection_id;
        let started = Instant::now();
        let admitted = (|| {
            let attachment = self
                .registry
                .attachment(host.as_str())
                .map_err(|_| HostDataPlaneFailure::RegistryUnavailable)?
                .ok_or(HostDataPlaneFailure::NoCurrentDaemon)?;
            if attachment.connection_id != expected_connection_id {
                return Err(HostDataPlaneFailure::ConnectionGenerationMismatch);
            }
            if attachment.daemon_instance != expected_instance {
                return Err(HostDataPlaneFailure::DaemonInstanceMismatch);
            }

            Ok(attachment)
        })();
        let attachment = match admitted {
            Ok(attachment) => attachment,
            Err(error) => {
                let outcome = if matches!(error, HostDataPlaneFailure::RegistryUnavailable) {
                    "error"
                } else {
                    "disconnected"
                };
                metrics::histogram!(
                    "silverhand_daemon_command_duration_seconds",
                    "command" => command.command.kind().as_wire(),
                    "outcome" => outcome,
                )
                .record(started.elapsed().as_secs_f64());
                return Err(error);
            }
        };
        let result = dispatch_attachment(&attachment, command.command, command.timeout)
            .await
            .map_err(HostDataPlaneFailure::from_dispatch)?;
        Ok(HostDataPlaneAnswer {
            // The lookup either matched this key's slot or there was no attachment
            // at all, so this is the host that served the command, not a coordinate
            // a consumer may restate later.
            host,
            instance: attachment.daemon_instance,
            connection_id: attachment.connection_id,
            result,
        })
    }
}

/// The only low-level attachment command call in production. Registry-backed and
/// pre-attach callers both hand this boundary an already sealed command; no sibling
/// can reopen or recombine its wire kind and payload.
async fn dispatch_attachment(
    attachment: &DaemonAttachment,
    command: EncodedDaemonCommand,
    timeout: Duration,
) -> Result<Value, CommandDispatchError> {
    attachment.command(command, timeout).await
}

enum ConnectionInput {
    Close(ConnectionClose),
    Control(ConnectionControl),
    Terminated,
}

enum InterruptibleSend {
    Sent,
    Expired,
    TransportFailed,
    Close(ConnectionClose),
    Terminated,
}

/// Wait for this private connection generation's close signal. A mismatched id can
/// only be produced by a faulty in-process caller or a test; ignoring it preserves
/// the generation boundary instead of turning the private channel into host lookup.
async fn matching_connection_close(
    close: &mut watch::Receiver<Option<ConnectionClose>>,
    connection_id: &str,
) -> Option<ConnectionClose> {
    loop {
        close.changed().await.ok()?;
        let requested = close.borrow_and_update().clone();
        if let Some(requested) = requested
            && requested
                .expected_connection_id
                .as_deref()
                .is_none_or(|expected| expected == connection_id)
        {
            return Some(requested);
        }
    }
}

/// The custody lane is deliberately separate from the bounded business-command
/// FIFO. `biased` is load-bearing: when both are ready after a stalled connection
/// resumes, revocation is observed before any queued command.
async fn next_connection_input(
    close: &mut watch::Receiver<Option<ConnectionClose>>,
    connection_id: &str,
    commands: &mut mpsc::Receiver<ConnectionControl>,
) -> ConnectionInput {
    tokio::select! {
        biased;
        requested = matching_connection_close(close, connection_id) => requested
            .map(ConnectionInput::Close)
            .unwrap_or(ConnectionInput::Terminated),
        received = commands.recv() => received
            .map(ConnectionInput::Control)
            .unwrap_or(ConnectionInput::Terminated),
    }
}

/// A command write that is still backpressured is not allowed to hide a later
/// custody revocation. Cancelling the in-flight send is conservatively ambiguous;
/// the close is then written and no further business command is dequeued.
async fn send_command_interruptibly(
    socket: &mut ObservedDaemonSocket,
    close: &mut watch::Receiver<Option<ConnectionClose>>,
    connection_id: &str,
    message: Message,
    deadline: Instant,
) -> InterruptibleSend {
    let send = socket.send(message);
    tokio::pin!(send);
    tokio::select! {
        biased;
        requested = matching_connection_close(close, connection_id) => requested
            .map(InterruptibleSend::Close)
            .unwrap_or(InterruptibleSend::Terminated),
        _ = tokio::time::sleep_until(deadline) => InterruptibleSend::Expired,
        result = &mut send => if result.is_ok() {
            InterruptibleSend::Sent
        } else {
            InterruptibleSend::TransportFailed
        },
    }
}

struct WebSocketCommandSink<'a> {
    socket: &'a mut ObservedDaemonSocket,
    close: &'a mut watch::Receiver<Option<ConnectionClose>>,
    connection_id: &'a str,
}

impl CommandWireSink for WebSocketCommandSink<'_> {
    async fn send(&mut self, message: Message, deadline: Instant) -> InterruptibleSend {
        send_command_interruptibly(
            self.socket,
            self.close,
            self.connection_id,
            message,
            deadline,
        )
        .await
    }
}

async fn send_connection_close(socket: &mut ObservedDaemonSocket, close: ConnectionClose) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code: 1001,
            reason: close.reason.into(),
        })))
        .await;
}

async fn daemon_websocket(
    State(state): State<DaemonWebsocketState>,
    websocket: Result<WebSocketUpgrade, axum::extract::ws::rejection::WebSocketUpgradeRejection>,
    headers: HeaderMap,
) -> Response {
    daemon_metrics::connection("upgrade", "attempt");
    let websocket = match websocket {
        Ok(websocket) => websocket,
        Err(error) => {
            daemon_metrics::connection("upgrade", "invalid_upgrade");
            return error.into_response();
        }
    };
    let DaemonWebsocketState {
        transport,
        authentication,
        admission,
        hydration,
        host_ready,
        status_seed,
        runtime_events,
    } = state;
    let Some(token) = strict_bearer_credential(&headers) else {
        daemon_metrics::connection("upgrade", "auth_401");
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let authenticated = authentication
        .authenticate(token.to_owned(), signed_unix_millis(SystemTime::now()))
        .await;
    let AuthenticatedDaemonHost {
        host_id,
        owner_user_id,
        credential_id,
    } = match authenticated {
        Ok(Some(authenticated)) => authenticated,
        Ok(None) => {
            daemon_metrics::connection("upgrade", "auth_401");
            return StatusCode::UNAUTHORIZED.into_response();
        }
        Err(cause) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(error::describe(cause.as_ref()))
                ),
                "daemon host credential authentication failed"
            );
            daemon_metrics::connection("upgrade", "error");
            return (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response();
        }
    };
    let (commands, receive) = mpsc::channel(32);
    let (close, close_receive) = watch::channel(None);
    let connection_id = match transport
        .registry
        .reserve(&host_id, commands.clone(), close)
    {
        Ok(id) => id,
        Err(ReserveConnectionError::Occupied) => {
            daemon_metrics::connection("upgrade", "already_connected");
            return StatusCode::CONFLICT.into_response();
        }
        Err(ReserveConnectionError::Closing) => {
            daemon_metrics::connection("upgrade", "server_closing");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        Err(ReserveConnectionError::Exhausted | ReserveConnectionError::Poisoned) => {
            tracing::error!("daemon attachment registry reservation failed");
            daemon_metrics::connection("upgrade", "error");
            return (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response();
        }
    };
    websocket
        .max_message_size(MAX_WORKSPACE_FILE_FRAME_BYTES)
        .max_frame_size(MAX_WORKSPACE_FILE_FRAME_BYTES)
        .on_upgrade(move |socket| {
            connection(
                socket,
                transport,
                authentication,
                admission,
                hydration,
                host_ready,
                status_seed,
                runtime_events,
                AuthenticatedHostConnection {
                    host_id,
                    owner_user_id,
                    credential_id,
                    connection_id,
                },
                commands,
                receive,
                close_receive,
            )
        })
}

struct AuthenticatedHostConnection {
    host_id: String,
    owner_user_id: UserId,
    credential_id: String,
    connection_id: String,
}

#[allow(clippy::too_many_arguments)]
async fn connection(
    socket: WebSocket,
    transport: DaemonTransport,
    authentication: Arc<dyn DaemonHostAuthentication>,
    admission: Arc<dyn DaemonProtocolAdmission>,
    hydration: Arc<dyn DaemonRuntimeHydration>,
    host_ready: Arc<dyn DaemonHostReadyEffect>,
    status_seed: Arc<dyn DaemonStatusSeedEffect>,
    runtime_events: Arc<dyn DaemonRuntimeEventEffects>,
    authenticated: AuthenticatedHostConnection,
    control: mpsc::Sender<ConnectionControl>,
    mut commands: mpsc::Receiver<ConnectionControl>,
    mut close: watch::Receiver<Option<ConnectionClose>>,
) {
    let mut socket = ObservedDaemonSocket::new(socket);
    let AuthenticatedHostConnection {
        host_id,
        owner_user_id,
        credential_id,
        connection_id,
    } = authenticated;
    let mut pending: HashMap<String, PendingCommand> = HashMap::new();
    let (timeout_control, mut timed_out_commands) = mpsc::unbounded_channel();
    let mut next_command = 0_u64;
    let mut compatible: Option<AttachedDaemon> = None;
    let mut attached: Option<DaemonInstanceId> = None;
    let mut heartbeat = tokio::time::interval_at(
        Instant::now() + transport.config.ping_interval,
        transport.config.ping_interval,
    );
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut awaiting_pong: Option<(i64, Instant)> = None;
    loop {
        tokio::select! {
            Some(id) = timed_out_commands.recv() => {
                if let Some(pending_command) = pending.remove(&id) {
                    if Instant::now() < pending_command.deadline {
                        pending.insert(id, pending_command);
                        continue;
                    }
                    let _ = pending_command.response.send(Err(CommandDispatchError::TimedOut));
                }
            }
            received_control = next_connection_input(&mut close, &connection_id, &mut commands) => match received_control {
                ConnectionInput::Close(requested) => {
                    send_connection_close(&mut socket, requested).await;
                    break;
                }
                ConnectionInput::Control(ConnectionControl::Command(command)) if compatible.is_some() => {
                    let mut sink = WebSocketCommandSink {
                        socket: &mut socket,
                        close: &mut close,
                        connection_id: &connection_id,
                    };
                    let sent = send_dequeued_command(command, &mut next_command, &mut sink).await;
                    let (id, kind, capability_refusal, deadline, response) = match sent {
                        DequeuedCommandSend::Skipped => continue,
                        DequeuedCommandSend::Sent { id, kind, capability_refusal, deadline, response } => {
                            (id, kind, capability_refusal, deadline, response)
                        }
                        DequeuedCommandSend::Terminate => break,
                        DequeuedCommandSend::Close(requested) => {
                            send_connection_close(&mut socket, requested).await;
                            break;
                        }
                    };
                    let timeout_id = id.clone();
                    let timeout_sender = timeout_control.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep_until(deadline).await;
                        let _ = timeout_sender.send(timeout_id);
                    });
                    pending.insert(id, PendingCommand {
                        kind,
                        capability_refusal,
                        response,
                        deadline,
                    });
                }
                ConnectionInput::Control(ConnectionControl::Command(command)) => {
                    let _ = command.response.send(Err(CommandDispatchError::ProvenNotDispatched));
                }
                ConnectionInput::Control(ConnectionControl::Hydrated(Ok(ready))) => {
                    if compatible
                        .as_ref()
                        .is_none_or(|current| current.daemon_instance != ready.daemon_instance)
                    {
                        break;
                    }
                    let current = match transport.registry.slot_connection_id(&host_id) {
                        Ok(current) => current,
                        Err(cause) => {
                            tracing::error!(
                                error_chain = crate::foundation::safe_log_field::safe_display(&error::describe(&cause)),
                                "daemon generation lookup failed before activation"
                            );
                            break;
                        }
                    };
                    if current.as_deref() != Some(&connection_id) {
                        break;
                    }

                    let activated = tokio::select! {
                        biased;
                        requested = matching_connection_close(&mut close, &connection_id) => {
                            if let Some(requested) = requested {
                                send_connection_close(&mut socket, requested).await;
                            }
                            break;
                        },
                        result = authentication.activate(&host_id, &owner_user_id, &credential_id, signed_unix_millis(SystemTime::now())) => result,
                    };
                    match activated {
                        Ok(true) => {}
                        Ok(false) => {
                            let _ = socket.send(Message::Close(Some(CloseFrame {
                                code: 1008,
                                reason: "host_revoked".into(),
                            }))).await;
                            break;
                        }
                        Err(cause) => {
                            tracing::error!(
                                error_chain = crate::foundation::safe_log_field::safe_display(&error::describe(cause.as_ref())),
                                "daemon activation failed"
                            );
                            let _ = socket.send(Message::Close(Some(CloseFrame {
                                code: 1011,
                                reason: "host_activation_failed".into(),
                            }))).await;
                            break;
                        }
                    }

                    match transport.registry.attach(&host_id, &connection_id, ready.clone()) {
                        Ok(true) => {
                            attached = Some(ready.daemon_instance.clone());
                            admission.attached(&host_id);
                            let seed_host = HostId::parse(host_id.clone())
                                .expect("authenticated host ids are validated at lookup");
                            host_ready.attached(&seed_host, &connection_id);
                            status_seed.attached(
                                seed_host,
                                ready.daemon_instance,
                                connection_id.clone(),
                                owner_user_id.clone(),
                                transport.controller().connection_authority(),
                            );
                        }
                        Ok(false) => break,
                        Err(cause) => {
                            tracing::error!(
                                error_chain = crate::foundation::safe_log_field::safe_display(&(error::describe(&cause))),
                                "daemon attachment registry failed after hydration"
                            );
                            let _ = socket.send(Message::Close(Some(CloseFrame {
                                code: 1011,
                                reason: "attachment_registry_failed".into(),
                            }))).await;
                            break;
                        }
                    }
                }
                ConnectionInput::Control(ConnectionControl::Hydrated(Err(cause))) => {
                    tracing::error!(
                        error_chain = crate::foundation::safe_log_field::safe_display(&(error::describe(cause.as_ref()))),
                        "daemon runtime hydration failed"
                    );
                    let _ = socket.send(Message::Close(Some(CloseFrame {
                        code: 1011,
                        reason: "runtime_hydration_failed".into(),
                    }))).await;
                    break;
                }
                ConnectionInput::Terminated => break,
            },
            _ = heartbeat.tick() => {
                let ts = signed_unix_millis(SystemTime::now()).max(0);
                let frame = json!({
                    "direction": "server_to_daemon",
                    "type": "heartbeat",
                    "kind": "ping",
                    "ts": ts,
                });
                if socket.send(Message::Text(frame.to_string().into())).await.is_err() {
                    break;
                }
                awaiting_pong = Some((ts, Instant::now() + transport.config.pong_timeout));
            }
            _ = async {
                if let Some((_, deadline)) = awaiting_pong {
                    tokio::time::sleep_until(deadline).await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {
                let _ = socket.send(Message::Close(Some(CloseFrame {
                    code: 1008,
                    reason: "heartbeat_timeout".into(),
                }))).await;
                break;
            }
            inbound = socket.recv() => match inbound {
                Some(Ok(Message::Text(text))) => {
                    let offered = offered_ready_protocol_version(&text);
                    if let Some(version) = offered
                        && version != OfferedReadyVersion::Readable(OUTBOUND_PROTOCOL_VERSION)
                    {
                        let actual = match version {
                            OfferedReadyVersion::Readable(value) => value.to_string(),
                            OfferedReadyVersion::Invalid => "invalid".to_owned(),
                        };
                        metrics::counter!(
                            "silverhand_host_ws_protocol_mismatch_total",
                            "actual_version" => actual,
                            "supported_version" => OUTBOUND_PROTOCOL_VERSION.to_string(),
                        ).increment(1);
                    }

                    // Only actionable numeric versions change per-host admission state.
                    if let Some(OfferedReadyVersion::Readable(offered)) = offered
                        && offered != OUTBOUND_PROTOCOL_VERSION
                    {
                        admission.incompatible(
                            &host_id,
                            &connection_id,
                            offered,
                            OUTBOUND_PROTOCOL_VERSION,
                            signed_unix_millis(SystemTime::now()),
                        );
                        let _ = socket.send(Message::Close(Some(CloseFrame {
                            code: 1002,
                            reason: UPDATE_REQUIRED_CLOSE_REASON.into(),
                        }))).await;
                        break;
                    }
                    let decoded = parse_daemon_frame(&text);
                    if !daemon_frame_fits_budget(text.len(), decoded.as_ref().ok(), &pending) {
                        let _ = socket.send(Message::Close(Some(CloseFrame {
                            code: 1009,
                            reason: "frame_too_large".into(),
                        }))).await;
                        break;
                    }
                    match decoded {
                        Ok(DaemonFrame::Ready(ReadyFrame {
                            protocol_version,
                            instance_id,
                            host_descriptor,
                        })) => {
                            if protocol_version != OUTBOUND_PROTOCOL_VERSION {
                                let _ = socket.send(Message::Close(Some(CloseFrame {
                                    code: 1002,
                                    reason: UPDATE_REQUIRED_CLOSE_REASON.into(),
                                }))).await;
                                break;
                            }
                            let Ok(instance) = DaemonInstanceId::parse(instance_id) else {
                                break;
                            };
                            if compatible.is_some() {
                                break;
                            }
                            let ready = AttachedDaemon {
                                daemon_instance: instance.clone(),
                                host_name: host_descriptor.name,
                            };
                            socket.ready(protocol_version);
                            compatible = Some(ready.clone());
                            let attachment = AttachedHostDataPlane::new(DaemonAttachment {
                                host_id: host_id.clone(),
                                connection_id: connection_id.clone(),
                                daemon_instance: instance,
                                host_name: ready.host_name.clone(),
                                commands: control.clone(),
                            });
                            let hydration = Arc::clone(&hydration);
                            let hydrate_control = control.clone();
                            tokio::spawn(async move {
                                let result = hydration.hydrate(attachment).await.map(|()| ready);
                                let _ = hydrate_control.send(ConnectionControl::Hydrated(result)).await;
                            });
                        }
                        Ok(DaemonFrame::ShuttingDown(frame)) => {
                            tracing::warn!(reason = crate::foundation::safe_log_field::safe_display(&(frame.reason)), "daemon announced shutdown");
                            let _ = socket.send(Message::Close(Some(CloseFrame {
                                code: 1001,
                                reason: "daemon_shutting_down".into(),
                            }))).await;
                            break;
                        }
                        Ok(DaemonFrame::CommandResponse(CommandResponseFrame { id, kind, result, error })) => {
                            let Some(pending_command) = pending.remove(&id) else { continue; };
                            if pending_command.kind.as_wire() != kind {
                                let _ = pending_command.response.send(Err(CommandDispatchError::InvalidResponse));
                                let _ = socket.send(Message::Close(Some(CloseFrame {
                                    code: 1002,
                                    reason: "command_response_kind_mismatch".into(),
                                }))).await;
                                break;
                            }
                            let settled = match (result, error) {
                                (Some(result), None) => Some(Ok(result)),
                                (None, Some(error)) => CommandRefusal::decode(
                                    pending_command.capability_refusal,
                                    &error,
                                ).map(|refusal| Err(CommandDispatchError::DaemonRefused(refusal))),
                                _ => None,
                            };
                            let Some(settled) = settled else {
                                let _ = pending_command.response.send(Err(CommandDispatchError::InvalidResponse));
                                let _ = socket.send(Message::Close(Some(CloseFrame {
                                    code: 1002,
                                    reason: "invalid_command_response".into(),
                                }))).await;
                                break;
                            };
                            let _ = pending_command.response.send(settled);
                        }
                        Ok(DaemonFrame::Runtime { kind, value }) => {
                            let Some(instance) = attached.as_ref() else {
                                let _ = socket.send(Message::Close(Some(CloseFrame {
                                    code: 1008,
                                    reason: "attachment_not_hydrated".into(),
                                }))).await;
                                break;
                            };
                            match runtime_events
                                .handle(&host_id, &connection_id, instance, kind, value)
                                .await
                            {
                                DaemonRuntimeEventDisposition::Continue => {}
                                DaemonRuntimeEventDisposition::Close { code, reason } => {
                                    let _ = socket.send(Message::Close(Some(CloseFrame {
                                        code,
                                        reason: reason.into(),
                                    }))).await;
                                    break;
                                }
                            }
                        }
                        Ok(DaemonFrame::HeartbeatPing(frame)) => {
                            let response = json!({
                                "direction": "server_to_daemon",
                                "type": "heartbeat",
                                "kind": "pong",
                                "ts": signed_unix_millis(SystemTime::now()).max(0),
                                "echo_ts": frame.ts,
                            });
                            if socket.send(Message::Text(response.to_string().into())).await.is_err() {
                                break;
                            }
                        }
                        Ok(DaemonFrame::HeartbeatPong(frame)) => {
                            if awaiting_pong.is_some_and(|(ts, _)| ts == frame.echo_ts) {
                                awaiting_pong = None;
                            }
                        }
                        Err(_) => {
                            let _ = socket.send(Message::Close(Some(CloseFrame {
                                code: 1002,
                                reason: "invalid_daemon_frame".into(),
                            }))).await;
                            break;
                        }
                    }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(Message::Ping(payload))) => {
                    if socket.send(Message::Pong(payload)).await.is_err() { break; }
                }
                Some(Ok(Message::Binary(_))) => break,
                Some(Ok(Message::Pong(_))) => {}
            }
        }
    }
    for (_, pending) in pending {
        let _ = pending.response.send(Err(CommandDispatchError::Ambiguous));
    }
    if let Some(instance) = attached
        .as_ref()
        .or_else(|| compatible.as_ref().map(|ready| &ready.daemon_instance))
    {
        runtime_events.detached(&host_id, instance, &connection_id);
    }
    transport.registry.remove(&host_id, &connection_id);
    runtime_events.release_connection(&connection_id).await;
}

async fn wait_for_seed_retry_or_replacement(
    registry: &DaemonRegistry,
    host: &HostId,
    connection_id: &str,
    after: Duration,
) -> bool {
    let deadline = Instant::now() + after;
    loop {
        let changed = registry.changed.notified();
        if registry
            .slot_connection_id(host.as_str())
            .ok()
            .flatten()
            .as_deref()
            != Some(connection_id)
        {
            return false;
        }
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return registry
                .slot_connection_id(host.as_str())
                .ok()
                .flatten()
                .as_deref() == Some(connection_id),
            _ = changed => {}
        }
    }
}

enum DaemonFrame {
    Ready(ReadyFrame),
    ShuttingDown(ShuttingDownFrame),
    HeartbeatPing(HeartbeatPingFrame),
    HeartbeatPong(HeartbeatPongFrame),
    Runtime { kind: String, value: Value },
    CommandResponse(CommandResponseFrame),
}

#[derive(Deserialize)]
struct HeartbeatPingFrame {
    ts: i64,
}

#[derive(Deserialize)]
struct HeartbeatPongFrame {
    echo_ts: i64,
}

#[derive(Deserialize)]
struct ReadyFrame {
    protocol_version: u32,
    instance_id: String,
    host_descriptor: ReadyHostDescriptor,
}

#[derive(Deserialize)]
struct ReadyHostDescriptor {
    name: String,
}

#[derive(Deserialize)]
struct ShuttingDownFrame {
    reason: String,
}

#[derive(Deserialize)]
struct CommandResponseFrame {
    id: String,
    kind: String,
    result: Option<Value>,
    error: Option<Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OfferedReadyVersion {
    Readable(u32),
    Invalid,
}

/// Inspect the ready envelope before strict schema parsing so old or malformed
/// ready frames remain observable. Invalid versions use one fixed metric bucket;
/// only readable incompatible versions may change per-host admission state.
/// Non-ready frames are outside this observation and return `None`.
fn offered_ready_protocol_version(raw: &str) -> Option<OfferedReadyVersion> {
    let value = serde_json::from_str::<Value>(raw).ok()?;
    if value.get("direction").and_then(Value::as_str) != Some("daemon_to_server")
        || value.get("type").and_then(Value::as_str) != Some("event")
        || value.get("kind").and_then(Value::as_str) != Some("ready")
    {
        return None;
    }
    let version = value
        .get("protocol_version")
        .and_then(Value::as_u64)
        .and_then(|version| u32::try_from(version).ok())
        .filter(|version| *version <= MAX_READABLE_PROTOCOL_VERSION);

    Some(match version {
        Some(version) => OfferedReadyVersion::Readable(version),
        None => OfferedReadyVersion::Invalid,
    })
}

fn parse_daemon_frame(raw: &str) -> Result<DaemonFrame, DaemonFrameError> {
    let value = serde_json::from_str::<Value>(raw).map_err(DaemonFrameError::Json)?;
    if value.get("direction").and_then(Value::as_str) != Some("daemon_to_server") {
        return Err(DaemonFrameError::Unsupported);
    }
    let frame_type = value.get("type").and_then(Value::as_str);
    let kind = value.get("kind").and_then(Value::as_str);
    match (frame_type, kind) {
        (Some("heartbeat"), Some("ping"))
            if exact_keys(&value, &["direction", "type", "kind", "ts"])
                && value
                    .get("ts")
                    .and_then(Value::as_i64)
                    .is_some_and(|ts| ts >= 0) =>
        {
            serde_json::from_value(value)
                .map(DaemonFrame::HeartbeatPing)
                .map_err(DaemonFrameError::Json)
        }
        (Some("heartbeat"), Some("pong"))
            if exact_keys(&value, &["direction", "type", "kind", "ts", "echo_ts"])
                && value
                    .get("ts")
                    .and_then(Value::as_i64)
                    .is_some_and(|ts| ts >= 0)
                && value
                    .get("echo_ts")
                    .and_then(Value::as_i64)
                    .is_some_and(|ts| ts >= 0) =>
        {
            serde_json::from_value(value)
                .map(DaemonFrame::HeartbeatPong)
                .map_err(DaemonFrameError::Json)
        }
        (Some("event"), Some("ready")) if valid_ready_frame(&value) => {
            serde_json::from_value(value)
                .map(DaemonFrame::Ready)
                .map_err(DaemonFrameError::Json)
        }
        (Some("event"), Some("shutting_down"))
            if exact_keys(&value, &["direction", "type", "kind", "reason"])
                && value.get("reason").is_some_and(Value::is_string) =>
        {
            serde_json::from_value(value)
                .map(DaemonFrame::ShuttingDown)
                .map_err(DaemonFrameError::Json)
        }
        (Some("event"), Some(kind))
            if OUTBOUND_DAEMON_EVENT_KINDS.contains(&kind)
                && !matches!(kind, "ready" | "shutting_down") =>
        {
            Ok(DaemonFrame::Runtime {
                kind: kind.to_owned(),
                value,
            })
        }
        (Some("command_response"), Some(_))
            if valid_command_response_envelope(&value)
                && (exact_keys(&value, &["direction", "type", "id", "kind", "result"])
                    || exact_keys(&value, &["direction", "type", "id", "kind", "error"])) =>
        {
            serde_json::from_value(value)
                .map(DaemonFrame::CommandResponse)
                .map_err(DaemonFrameError::Json)
        }
        _ => Err(DaemonFrameError::Unsupported),
    }
}

fn exact_keys(value: &Value, expected: &[&str]) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    object.len() == expected.len() && expected.iter().all(|key| object.contains_key(*key))
}

fn valid_ready_frame(value: &Value) -> bool {
    if !exact_keys(
        value,
        &[
            "direction",
            "type",
            "kind",
            "protocol_version",
            "daemon_version",
            "daemon_id",
            "instance_id",
            "host_descriptor",
            "capabilities",
            "daemon_pid",
            "started_at",
        ],
    ) {
        return false;
    }
    let Some(host) = value.get("host_descriptor") else {
        return false;
    };
    exact_keys(host, &["name", "kind"])
        && host
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|name| !name.is_empty())
        && matches!(
            host.get("kind").and_then(Value::as_str),
            Some("local" | "remote")
        )
        && value.get("daemon_version").is_some_and(Value::is_string)
        && value
            .get("daemon_id")
            .and_then(Value::as_str)
            .is_some_and(|id| !id.is_empty())
        && value
            .get("instance_id")
            .and_then(Value::as_str)
            .is_some_and(|id| !id.is_empty())
        && value
            .get("capabilities")
            .and_then(Value::as_array)
            .is_some_and(|items| items.iter().all(Value::is_string))
}

/// Large results require a live request on THIS connection, not a claimed wire kind.
fn daemon_frame_fits_budget(
    bytes: usize,
    frame: Option<&DaemonFrame>,
    pending: &HashMap<String, PendingCommand>,
) -> bool {
    if bytes <= MAX_DAEMON_FRAME_BYTES {
        return true;
    }
    if bytes > MAX_WORKSPACE_FILE_FRAME_BYTES {
        return false;
    }
    let Some(DaemonFrame::CommandResponse(response)) = frame else {
        return false;
    };
    response.result.is_some()
        && response.error.is_none()
        && pending.get(&response.id).is_some_and(|command| {
            matches!(
                command.kind,
                IssuedDaemonCommandKind::AgentWorkspaceFileRead
            ) && command.kind.as_wire() == response.kind
                && Instant::now() < command.deadline
        })
}

fn valid_command_response_envelope(value: &Value) -> bool {
    value
        .get("id")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.is_empty())
        && value
            .get("kind")
            .and_then(Value::as_str)
            .and_then(IssuedDaemonCommandKind::from_wire)
            .is_some()
}

#[derive(Debug)]
enum DaemonFrameError {
    Json(serde_json::Error),
    Unsupported,
}

impl fmt::Display for DaemonFrameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid daemon frame")
    }
}

impl Error for DaemonFrameError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Json(source) => Some(source),
            Self::Unsupported => None,
        }
    }
}

fn signed_unix_millis(time: SystemTime) -> i64 {
    match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_millis()).unwrap_or(i64::MAX),
        Err(error) => -i64::try_from(error.duration().as_millis()).unwrap_or(i64::MAX),
    }
}

#[cfg(test)]
#[path = "daemon_transport_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "daemon_activation_close_tests.rs"]
mod activation_close_tests;
