//! The create-agent unit of work: `POST /api/agents`.
//!
//! This orchestrates the create saga end to end, over the durable primitives in
//! [`crate::application_orchestration::agent_materialization`] (reserve / activate-and-provision / fail / reopen /
//! idempotent lookup), the host-side materialize command
//! ([`crate::host_runtime::agent_materialize`]), the placement rule ([`crate::host_runtime`]), the
//! shared handle namespace ([`crate::identity_access::human_auth_flows::HandleNamespaceRead`]) and id
//! minting (`crate::conversation_messaging::messaging::identity`). It owns the DECISION at each step; the row
//! writes, the host command and the reads are ports.
//!
//! ## What this module is NOT
//!
//! It does not project the created agent to the wire, run the post-commit effects
//! (announce / deliver / start / direct-pairs-changed), parse the request body, or map an
//! outcome to an HTTP status. Those belong to the route face: this returns a typed
//! [`CreateAgentOutcome`] carrying only the facts the route needs, and every internal
//! break is a [`CreateAgentFault`] the boundary folds to a detail-free `500`.
//!
//! ## Deliberate divergences from the TypeScript create path
//!
//! - **No pre-reserve generation reconfirm.** TS re-reads the daemon connection id between
//!   its online check and the dispatch to catch a reconnect. Here the saga captures the
//!   daemon custody ONCE, before it reserves a row, and the dispatch lands only on that
//!   captured custody — a daemon that dropped or was replaced in between fails closed to
//!   `host_attach_pending`. The capture is one authority for both the fail-fast offline
//!   check and the command, so there is no second connectivity fact to reconfirm and no
//!   window in which the gate and the dispatch could disagree about which daemon owns the
//!   host. An offline capture inserts no pending row a synchronous create would then fail;
//!   an offline retry reopens nothing and leaves the prior failure fact untouched.
//! - **`dependent_row_insert_failed` is not a product code.** A dependent-graph write
//!   failure surfaces as a `MaterializationFault` → safe `500`, matching the DELETE
//!   face, rather than a `materializeFailed` code.
//! - **A genuine materialize transport/protocol fault is a safe `500`, not a flip to
//!   `failed`.** The [`AgentMaterializePort`] already turns the product-meaningful daemon
//!   and transport refusals (including no-connection and timeout) into
//!   [`AgentMaterializeRefusal`] arms; what remains in its error channel is
//!   ambiguous-or-broken (the command may have landed, or our own encode/registry is
//!   down). None of those is a safe product answer, so the row is left pending for the
//!   boot-time stale sweep rather than stamped `failed` and offered for a naive retry.

use std::future::Future;

use thiserror::Error;

use crate::application_orchestration::agent_materialization::{FailOutcome, fail_materialization};
use crate::{
    application_orchestration::agent_materialization::{
        AgentArmProvisioningPort, AgentMaterializationLookup, AgentMaterializationWrite,
        IdempotencyKey, IdempotentReservation, ProvisionedActivation, ReopenOutcome,
        ReserveAgentInput, ReserveOutcome, find_idempotent_reservation, home_workspace_id_for,
        provision_and_activate, reopen_failed_materialization, reserve_agent,
    },
    conversation_messaging::messaging::{
        MessageId,
        collaboration::RuntimePlacementRefusal,
        environment::{EnvironmentUnavailable, SendEntropy},
        mint_agent_id, mint_conversation_id,
        post_commit::{EffectFailure, InboxEnsureOutcome},
    },
    foundation::identity::{AgentId, ConversationId, HostId, UserId, WorkspaceId},
    foundation::name_canonicalization::is_ecmascript_trim_whitespace,
    foundation::persistence::PersistenceFailure,
    host_runtime::agent_materialize::{
        AgentMaterializePort, AgentMaterializeRefusal, AgentMaterializeTarget,
        AgentMaterializeTemplate,
    },
    host_runtime::{
        lifecycle::{HostLifecycleStatus, lifecycle_status},
        read::ComputerReadModel,
    },
    identity_access::agent_lifecycle::MaterializationStatus,
    identity_access::agents::{
        runtime::RuntimeKind, runtime_capability::runtime_default_model,
        session_scope::AgentSessionScope,
    },
    identity_access::human_auth_flows::HandleNamespaceRead,
    identity_access::user_identity::ParticipantHandle,
};

/// The already-admitted create request, in the core's own vocabulary.
///
/// Format legality (the handle grammar, the reasoning-effort/fast-mode allowlist) is the
/// route's admission concern, settled before this value is built; this carries only what
/// the write persists. `model` is not yet defaulted — [`create_agent`] normalizes it once
/// at the top so both the idempotency compare and the reserve see the same value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreateAgentInput {
    pub name: ParticipantHandle,
    pub runtime: RuntimeKind,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub fast_mode: Option<bool>,
    pub description: Option<String>,
    /// Absent means "auto-place on the caller's one ready host"; present names an explicit
    /// host the caller must own.
    pub host_id: Option<HostId>,
    /// Absent defaults to [`AgentSessionScope::Conversation`], the create-time default.
    pub session_scope: Option<AgentSessionScope>,
}

/// The create-time model default, applied exactly once. A non-blank explicit model is kept;
/// an absent or ECMAScript-whitespace-only one falls back to the runtime's forced default
/// (only Qoder carries one). Mirrors `normalizeCreateAgentInput` and its `model.trim()` —
/// blank is the ECMAScript `String.prototype.trim` set, so a lone U+FEFF is blank and falls
/// back while a lone U+0085 (NEL, outside that set) is kept verbatim.
fn normalize_model(runtime: RuntimeKind, model: Option<String>) -> Option<String> {
    match model {
        Some(model) if !model.chars().all(is_ecmascript_trim_whitespace) => Some(model),
        _ => runtime_default_model(runtime).map(str::to_owned),
    }
}

/// The metadata fields the idempotency guard compares, in the order it reports the first
/// disagreement. `template_choice` is omitted on purpose: the create path only ever writes
/// the `default` template, so both sides are trivially equal and it can never differ.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetadataField {
    Name,
    Runtime,
    Model,
    Description,
    HostId,
    SessionScope,
}

impl MetadataField {
    /// The wire token the route reports as `mismatched_field`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Runtime => "runtime",
            Self::Model => "model",
            Self::Description => "description",
            Self::HostId => "host_id",
            Self::SessionScope => "session_scope",
        }
    }
}

/// The product-meaningful reasons a host declined to materialize, as the route reports them.
/// The closed set is nine (`dependent_row_insert_failed` is deliberately not here — it is a
/// safe `500`). Every arm carries its stable wire token, which is also the
/// `materialization_failed_code` stamped on the row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MaterializeFailureCode {
    DaemonInternalError,
    HostAttachPending,
    MaterializeTimeout,
    AgentDirCollision,
    FsQuotaExceeded,
    FsUnavailable,
    UnsupportedTemplate,
    UnsupportedRuntime,
    DaemonShuttingDown,
}

impl MaterializeFailureCode {
    /// The stable persisted/wire token.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DaemonInternalError => "daemon_internal_error",
            Self::HostAttachPending => "host_attach_pending",
            Self::MaterializeTimeout => "materialize_timeout",
            Self::AgentDirCollision => "agent_dir_collision",
            Self::FsQuotaExceeded => "fs_quota_exceeded",
            Self::FsUnavailable => "fs_unavailable",
            Self::UnsupportedTemplate => "unsupported_template",
            Self::UnsupportedRuntime => "unsupported_runtime",
            Self::DaemonShuttingDown => "daemon_shutting_down",
        }
    }

    fn from_refusal(refusal: AgentMaterializeRefusal) -> Self {
        match refusal {
            AgentMaterializeRefusal::HostAttachPending => Self::HostAttachPending,
            AgentMaterializeRefusal::FsQuotaExceeded => Self::FsQuotaExceeded,
            AgentMaterializeRefusal::FsUnavailable => Self::FsUnavailable,
            AgentMaterializeRefusal::AgentDirCollision => Self::AgentDirCollision,
            AgentMaterializeRefusal::UnsupportedTemplate => Self::UnsupportedTemplate,
            AgentMaterializeRefusal::UnsupportedRuntime => Self::UnsupportedRuntime,
            AgentMaterializeRefusal::DaemonShuttingDown => Self::DaemonShuttingDown,
            AgentMaterializeRefusal::DaemonInternalError => Self::DaemonInternalError,
            AgentMaterializeRefusal::MaterializeTimeout => Self::MaterializeTimeout,
            // A freshly minted agent id can never be mid-deletion, so this arm is
            // unreachable on the create path; it folds to the daemon-internal default,
            // exactly as the TypeScript wire map's default clause does.
            AgentMaterializeRefusal::AgentDeleting => Self::DaemonInternalError,
        }
    }
}

/// The wire coordinates a create/echo response carries. All three are `None` for an
/// agent-scoped agent (its onboarding rides a creator↔agent direct line that is not a
/// coordinate the create response publishes) and all three are `Some` for a
/// conversation-scoped one (its personal home workspace and `Onboarding` DM).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreatedCoordinates {
    pub agent_home_workspace_id: Option<WorkspaceId>,
    pub dm_channel_id: Option<ConversationId>,
    pub onboarding_message_id: Option<MessageId>,
}

impl CreatedCoordinates {
    fn agent_scoped() -> Self {
        Self {
            agent_home_workspace_id: None,
            dm_channel_id: None,
            onboarding_message_id: None,
        }
    }

    fn conversation_scoped(
        home_workspace: WorkspaceId,
        dm_channel: ConversationId,
        onboarding_message_id: MessageId,
    ) -> Self {
        Self {
            agent_home_workspace_id: Some(home_workspace),
            dm_channel_id: Some(dm_channel),
            onboarding_message_id: Some(onboarding_message_id),
        }
    }
}

/// The committed arm of a fresh creation. It carries the wire coordinates AND the facts the
/// post-commit effects consume: the destination line the onboarding message landed on, that
/// message's id, and — for the agent arm — whether the creator↔agent direct pair was minted
/// (so `direct_pairs_changed` fires only when a pair actually opened).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CreatedArm {
    Agent {
        direct_line: ConversationId,
        onboarding_message_id: MessageId,
        pair_created: bool,
    },
    Conversation {
        home_workspace: WorkspaceId,
        dm_channel: ConversationId,
        onboarding_message_id: MessageId,
    },
}

impl CreatedArm {
    /// The onboarding message id, whichever arm committed.
    pub fn onboarding_message_id(&self) -> &MessageId {
        match self {
            Self::Agent {
                onboarding_message_id,
                ..
            }
            | Self::Conversation {
                onboarding_message_id,
                ..
            } => onboarding_message_id,
        }
    }

    /// The conversation the onboarding message was delivered on — the direct line for an
    /// agent-scoped agent, the home DM for a conversation-scoped one. This is where the
    /// route delivers the onboarding message and starts the runtime.
    pub fn destination(&self) -> &ConversationId {
        match self {
            Self::Agent { direct_line, .. } => direct_line,
            Self::Conversation { dm_channel, .. } => dm_channel,
        }
    }

    /// The wire coordinates this arm projects.
    pub fn coordinates(&self) -> CreatedCoordinates {
        match self {
            Self::Agent { .. } => CreatedCoordinates::agent_scoped(),
            Self::Conversation {
                home_workspace,
                dm_channel,
                onboarding_message_id,
            } => CreatedCoordinates::conversation_scoped(
                home_workspace.clone(),
                dm_channel.clone(),
                onboarding_message_id.clone(),
            ),
        }
    }
}

/// A fresh creation: the agent's identity plus the committed arm. The route reprojects the
/// agent by id, attaches [`CreatedArm::coordinates`], and fires the post-commit effects.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreatedAgent {
    pub agent_id: AgentId,
    pub arm: CreatedArm,
}

/// An idempotent replay of an already-`active` create: the identity plus the wire
/// coordinates, and NO effects (they fired on the original creation). Reprojected by the
/// route exactly like a creation, but returned as `200` rather than `201`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EchoedAgent {
    pub agent_id: AgentId,
    pub coordinates: CreatedCoordinates,
}

/// Every product answer the create unit of work can reach. Faults are not here — they are
/// [`CreateAgentFault`], a safe `500`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CreateAgentOutcome {
    /// A row was reserved, materialized on its host, and its dependent graph committed —
    /// `201`. Effects fire.
    Created(CreatedAgent),
    /// The `(creator, key)` pair already named a fully `active` agent — `200`, same payload
    /// shape, no effects.
    Echo(EchoedAgent),
    /// The reservation is still `pending_materialization` — `409 materialization_in_progress`.
    InProgress,
    /// The reservation is `failed` and this was not a retry — `409
    /// materialization_previously_failed`.
    PreviouslyFailed { failed_code: String, failed_at: i64 },
    /// A prior reservation for this key disagrees with the request on a field — `400
    /// idempotency_metadata_mismatch`.
    MetadataMismatch { field: MetadataField },
    /// A retry named a `(creator, key)` with no reservation — `404 retry_target_not_found`.
    RetryTargetNotFound,
    /// A retry raced another transition and the row was no longer `failed` — `409
    /// retry_target_not_failed`.
    RetryTargetNotFailed,
    /// The chosen handle is already held in the shared agents∪users namespace — `409
    /// agent_name_taken`.
    NameTaken,
    /// Placement resolution refused — status per the route's placement map.
    PlacementRefused(RuntimePlacementRefusal),
    /// No daemon is currently attached to the selected host; no row was reserved — `503
    /// host_attach_pending`.
    HostAttachPending,
    /// The host declined to materialize; the row was stamped `failed` with this code —
    /// status per the route's materialize map.
    MaterializeFailed { code: MaterializeFailureCode },
}

/// An internal break in the create unit of work. Every arm is a safe `500` at the boundary,
/// never a product answer.
#[derive(Debug, Error)]
pub enum CreateAgentFault {
    #[error(transparent)]
    Materialization(
        #[from] crate::application_orchestration::agent_materialization::MaterializationFault,
    ),
    #[error(transparent)]
    Persistence(#[from] PersistenceFailure),
    #[error(transparent)]
    Environment(#[from] EnvironmentUnavailable),
    /// The host command's own error channel: ambiguous delivery, a poisoned registry, or a
    /// corrupt/mis-custodied answer. Detail is dropped — none of it is a safe product
    /// answer, and the row is left pending for the boot sweep.
    #[error("agent materialize command failed at the transport boundary")]
    MaterializeTransport,
    /// A row that must exist for the graph to be consistent was absent (e.g. an `active`
    /// conversation-scoped row with no home DM, or a lost provisioning whose reservation
    /// vanished). The store's own prior fact is inconsistent, so it is a safe `500`.
    #[error("agent graph is internally inconsistent: {reason}")]
    CorruptGraph { reason: &'static str },
}

/// The two post-commit reads an idempotent echo of a conversation-scoped create needs to
/// rebuild its coordinates: the agent's home DM channel and the first onboarding system
/// message on it. Only the echo path reads these — a fresh create carries them out of the
/// provisioning seal — so this is a narrow read port, not the agent projection authority.
pub trait AgentGraphRead: Send + Sync {
    fn find_home_dm_channel(
        &self,
        agent: &AgentId,
    ) -> impl Future<Output = Result<Option<ConversationId>, PersistenceFailure>> + Send;

    fn find_first_onboarding_message(
        &self,
        dm: &ConversationId,
    ) -> impl Future<Output = Result<Option<MessageId>, PersistenceFailure>> + Send;
}

/// The durable half of onboarding delivery: ensure the `conversation_inboxes` row that owes
/// the freshly created agent its onboarding turn, sealed by the SAME participation authority
/// the send path uses. It exists so the create route's onboarding leg is a genuine
/// enqueue-then-wake (mirroring the reference `deliverMessage`), not a bare wake against an
/// empty inbox: the runtime does not read a committed inbox on boot, so without this row the
/// wake finds nothing pending and the agent is never handed its turn.
///
/// The implementation must prove, against committed rows and NOT the route-supplied ids as if
/// they were a relationship, that the message belongs to the conversation AND the agent is a
/// current participant of it (roster row for a workspace placement, or a direct-pair
/// endpoint). No witness → a typed failure the caller logs and drops; it must never write a
/// cross-conversation inbox debt. Idempotent: an already-present row is a no-op success. This
/// is a best-effort POST-COMMIT authority — the create response is already sealed, so a
/// failure here is never rolled back into the provisioning transaction.
pub trait OnboardingInboxEnsure: Clone + Send + Sync + 'static {
    fn ensure(
        &self,
        conversation: &ConversationId,
        agent: &AgentId,
        onboarding_message: &MessageId,
    ) -> impl Future<Output = Result<InboxEnsureOutcome, EffectFailure>> + Send;
}

/// The authorities the create unit of work drives. Each field is a distinct bounded
/// authority; the reserve/lookup pair is one adapter because they are one store. The
/// `materialize` port owns both the attachment gate (`capture`) and the command dispatch,
/// so the gate and the command speak one truth about which daemon holds the host.
pub struct CreateAgentPorts<'a, C, H, M, R, V, G, E> {
    pub computers: &'a C,
    pub handles: &'a H,
    pub materialization: &'a M,
    pub materialize: &'a R,
    pub provisioning: &'a V,
    pub graph: &'a G,
    pub entropy: &'a E,
}

/// The one entry point. `creator` is the authenticated principal, `key` the parsed
/// `Idempotency-Key`, `retry` the `?retry=true` flag, `now_ms` the request clock.
pub async fn create_agent<C, H, M, R, V, G, E>(
    ports: &CreateAgentPorts<'_, C, H, M, R, V, G, E>,
    creator: &UserId,
    key: &IdempotencyKey,
    mut input: CreateAgentInput,
    retry: bool,
    now_ms: i64,
) -> Result<CreateAgentOutcome, CreateAgentFault>
where
    C: ComputerReadModel,
    H: HandleNamespaceRead,
    M: AgentMaterializationLookup + AgentMaterializationWrite,
    R: AgentMaterializePort,
    V: AgentArmProvisioningPort,
    G: AgentGraphRead,
    E: SendEntropy,
{
    // Normalize once so the idempotency compare and the reserve read the same model.
    input.model = normalize_model(input.runtime, input.model.take());

    if let Some(existing) = find_idempotent_reservation(ports.materialization, creator, key).await?
    {
        return dispatch_existing(ports, creator, key, &input, retry, now_ms, existing).await;
    }

    // A retry names an existing reservation to re-drive; without one there is nothing to
    // retry.
    if retry {
        return Ok(CreateAgentOutcome::RetryTargetNotFound);
    }

    // Fresh: placement → handle → capture custody → reserve → materialize.
    let host_id = match resolve_placement(ports.computers, creator, input.host_id.as_ref()).await? {
        Ok(host_id) => host_id,
        Err(refusal) => return Ok(CreateAgentOutcome::PlacementRefused(refusal)),
    };

    if ports.handles.is_handle_taken(&input.name).await? {
        return Ok(CreateAgentOutcome::NameTaken);
    }

    // Capture the exact daemon that owns the host BEFORE the first persistent mutation. The
    // command below dispatches only to this captured custody, so a daemon that drops or is
    // replaced between here and the dispatch is refused, never silently retargeted. No daemon
    // attached is the fast offline case: no row is reserved.
    let Some(captured) = ports
        .materialize
        .capture(&host_id)
        .await
        .map_err(|_| CreateAgentFault::MaterializeTransport)?
    else {
        return Ok(CreateAgentOutcome::HostAttachPending);
    };

    let agent_id = mint_agent_id(ports.entropy)?;
    let reserve_input = ReserveAgentInput {
        agent_id: agent_id.clone(),
        name: input.name.clone(),
        runtime: input.runtime,
        model: input.model.clone(),
        reasoning_effort: input.reasoning_effort.clone(),
        fast_mode: input.fast_mode,
        host_id: host_id.clone(),
        description: input.description.clone(),
        session_scope: input
            .session_scope
            .unwrap_or(AgentSessionScope::Conversation),
        created_by_user_id: creator.clone(),
        idempotency_key: key.clone(),
    };

    let reserved = match reserve_agent(ports.materialization, &reserve_input, now_ms).await? {
        ReserveOutcome::Reserved(reserved) => reserved,
        ReserveOutcome::ConstraintConflict => return Ok(CreateAgentOutcome::NameTaken),
    };

    let target = materialize_target(
        reserved.host_id().clone(),
        reserved.agent_id().clone(),
        reserved.name(),
        reserved.runtime(),
        reserved.model(),
        reserved.description(),
    );
    materialize_and_provision(
        ports,
        creator,
        key,
        &input,
        reserved.agent_id(),
        captured,
        target,
        now_ms,
    )
    .await
}

/// Drive an existing reservation for this `(creator, key)`: the idempotent replay and the
/// retry re-drive.
async fn dispatch_existing<C, H, M, R, V, G, E>(
    ports: &CreateAgentPorts<'_, C, H, M, R, V, G, E>,
    creator: &UserId,
    key: &IdempotencyKey,
    input: &CreateAgentInput,
    retry: bool,
    now_ms: i64,
    existing: IdempotentReservation,
) -> Result<CreateAgentOutcome, CreateAgentFault>
where
    C: ComputerReadModel,
    H: HandleNamespaceRead,
    M: AgentMaterializationLookup + AgentMaterializationWrite,
    R: AgentMaterializePort,
    V: AgentArmProvisioningPort,
    G: AgentGraphRead,
    E: SendEntropy,
{
    match classify_idempotent(&existing, input, retry) {
        IdempotentClass::Terminal(outcome) => Ok(outcome),
        IdempotentClass::EchoActive => build_echo(ports, &existing).await,
        IdempotentClass::Reopen => {
            // Capture the daemon BEFORE reopening the failed row. An offline retry reopens
            // nothing and leaves the original failure code and stamp untouched, so a later
            // `?retry` still sees the true prior failure rather than a row we churned back to
            // pending and could not drive.
            let Some(captured) = ports
                .materialize
                .capture(existing.host_id())
                .await
                .map_err(|_| CreateAgentFault::MaterializeTransport)?
            else {
                return Ok(CreateAgentOutcome::HostAttachPending);
            };
            match reopen_failed_materialization(ports.materialization, existing.agent_id(), now_ms)
                .await?
            {
                ReopenOutcome::Lost => Ok(CreateAgentOutcome::RetryTargetNotFailed),
                ReopenOutcome::Reopened(_) => {
                    let target = materialize_target(
                        existing.host_id().clone(),
                        existing.agent_id().clone(),
                        existing.name(),
                        existing.runtime(),
                        existing.model(),
                        existing.description(),
                    );
                    materialize_and_provision(
                        ports,
                        creator,
                        key,
                        input,
                        existing.agent_id(),
                        captured,
                        target,
                        now_ms,
                    )
                    .await
                }
            }
        }
    }
}

/// The pure classification of an existing reservation against the request. The metadata
/// mismatch is checked FIRST, before the status is read, exactly as the TypeScript does.
enum IdempotentClass {
    Terminal(CreateAgentOutcome),
    /// `active` → the caller builds the echo (scope decides whether reads are needed).
    EchoActive,
    /// `failed` + retry → the caller reopens and re-materializes.
    Reopen,
}

fn classify_idempotent(
    existing: &IdempotentReservation,
    input: &CreateAgentInput,
    retry: bool,
) -> IdempotentClass {
    if let Some(field) = compare_metadata(existing, input) {
        return IdempotentClass::Terminal(CreateAgentOutcome::MetadataMismatch { field });
    }
    match existing.materialization_status() {
        MaterializationStatus::Active => IdempotentClass::EchoActive,
        MaterializationStatus::PendingMaterialization => {
            IdempotentClass::Terminal(CreateAgentOutcome::InProgress)
        }
        MaterializationStatus::Failed => {
            if retry {
                IdempotentClass::Reopen
            } else {
                // `seal` proved a `failed` reservation carries a non-empty code and a stamp,
                // so these reads are total — the product decision no longer defaults over a
                // possibly-forged empty code or zero clock.
                IdempotentClass::Terminal(CreateAgentOutcome::PreviouslyFailed {
                    failed_code: existing
                        .materialization_failed_code()
                        .expect("sealed failed reservation carries its failure code")
                        .to_owned(),
                    failed_at: existing
                        .materialization_failed_at()
                        .expect("sealed failed reservation carries its failure timestamp"),
                })
            }
        }
    }
}

/// Compare the request against the reservation, returning the first field that disagrees in
/// the fixed order. `host_id` disagrees only when the request NAMES a host different from
/// the reservation's (an absent host defaults to the reserved one); `session_scope` defaults
/// to `conversation`. The model was already normalized on `input`.
fn compare_metadata(
    existing: &IdempotentReservation,
    input: &CreateAgentInput,
) -> Option<MetadataField> {
    if existing.name() != input.name.as_str() {
        return Some(MetadataField::Name);
    }
    if existing.runtime() != input.runtime {
        return Some(MetadataField::Runtime);
    }
    if existing.model() != input.model.as_deref() {
        return Some(MetadataField::Model);
    }
    if existing.description() != input.description.as_deref() {
        return Some(MetadataField::Description);
    }
    if let Some(requested) = input.host_id.as_ref()
        && requested != existing.host_id()
    {
        return Some(MetadataField::HostId);
    }
    let requested_scope = input
        .session_scope
        .unwrap_or(AgentSessionScope::Conversation);
    if requested_scope != existing.session_scope() {
        return Some(MetadataField::SessionScope);
    }
    None
}

/// Build the idempotent echo of an `active` reservation. An agent-scoped row publishes
/// all-null coordinates and needs no read; a conversation-scoped one re-reads its home DM
/// and the onboarding message on it — a missing row is a broken graph.
async fn build_echo<C, H, M, R, V, G, E>(
    ports: &CreateAgentPorts<'_, C, H, M, R, V, G, E>,
    existing: &IdempotentReservation,
) -> Result<CreateAgentOutcome, CreateAgentFault>
where
    G: AgentGraphRead,
{
    let agent_id = existing.agent_id().clone();
    let coordinates = match existing.session_scope() {
        AgentSessionScope::Agent => CreatedCoordinates::agent_scoped(),
        AgentSessionScope::Conversation => {
            let dm_channel = ports.graph.find_home_dm_channel(&agent_id).await?.ok_or(
                CreateAgentFault::CorruptGraph {
                    reason: "active conversation-scoped agent has no home DM channel",
                },
            )?;
            let onboarding_message_id = ports
                .graph
                .find_first_onboarding_message(&dm_channel)
                .await?
                .ok_or(CreateAgentFault::CorruptGraph {
                    reason: "active conversation-scoped agent's home DM has no onboarding message",
                })?;
            CreatedCoordinates::conversation_scoped(
                home_workspace_id_for(&agent_id),
                dm_channel,
                onboarding_message_id,
            )
        }
    };
    Ok(CreateAgentOutcome::Echo(EchoedAgent {
        agent_id,
        coordinates,
    }))
}

/// Send the host materialize command, then activate-and-provision the reserved row. A
/// refusal stamps the row `failed` and reports the code; a lost activation re-reads the row
/// as an idempotent dispatch; a transport fault is a safe `500` with the row left pending.
#[allow(clippy::too_many_arguments)]
async fn materialize_and_provision<C, H, M, R, V, G, E>(
    ports: &CreateAgentPorts<'_, C, H, M, R, V, G, E>,
    creator: &UserId,
    key: &IdempotencyKey,
    input: &CreateAgentInput,
    agent_id: &AgentId,
    captured: R::Captured,
    target: AgentMaterializeTarget,
    now_ms: i64,
) -> Result<CreateAgentOutcome, CreateAgentFault>
where
    C: ComputerReadModel,
    H: HandleNamespaceRead,
    M: AgentMaterializationLookup + AgentMaterializationWrite,
    R: AgentMaterializePort,
    V: AgentArmProvisioningPort,
    G: AgentGraphRead,
    E: SendEntropy,
{
    match ports
        .materialize
        .materialize(captured, &target)
        .await
        .map_err(|_| CreateAgentFault::MaterializeTransport)?
    {
        Ok(_receipt) => {}
        Err(refusal) => {
            let code = MaterializeFailureCode::from_refusal(refusal);
            // Stamp the row `failed` with the code. A `Lost` flip means a concurrent dispatch
            // for the same key already moved the row past `pending|failed`, so our own `code`
            // is not the answer the caller is owed — re-read the winner and report its state.
            return match fail_materialization(
                ports.materialization,
                agent_id,
                code.as_str(),
                now_ms,
                now_ms,
            )
            .await?
            {
                FailOutcome::Failed(_sealed) => Ok(CreateAgentOutcome::MaterializeFailed { code }),
                FailOutcome::Lost => resolve_lost(ports, creator, key, input, now_ms).await,
            };
        }
    }

    // The host landed the home; flip the reservation `active` and provision its dependent
    // graph in one transaction, over three freshly minted candidate ids.
    let candidate_direct_line = mint_conversation_id(ports.entropy)?;
    let candidate_dm_channel = mint_conversation_id(ports.entropy)?;
    let onboarding_message_id = MessageId::mint(ports.entropy)?;

    match provision_and_activate(
        ports.provisioning,
        agent_id,
        candidate_direct_line,
        candidate_dm_channel,
        onboarding_message_id,
        now_ms,
    )
    .await?
    {
        ProvisionedActivation::Agent(arm) => Ok(CreateAgentOutcome::Created(CreatedAgent {
            agent_id: arm.agent_id().clone(),
            arm: CreatedArm::Agent {
                direct_line: arm.direct_line().clone(),
                onboarding_message_id: arm.onboarding_message_id().clone(),
                pair_created: arm.pair_created(),
            },
        })),
        ProvisionedActivation::Conversation(arm) => Ok(CreateAgentOutcome::Created(CreatedAgent {
            agent_id: arm.agent_id().clone(),
            arm: CreatedArm::Conversation {
                home_workspace: arm.home_workspace().clone(),
                dm_channel: arm.dm_channel().clone(),
                onboarding_message_id: arm.onboarding_message_id().clone(),
            },
        })),
        // The pending→active flip matched no row: a concurrent request for the same key won.
        // Re-read the reservation and answer as a non-retry idempotent dispatch would.
        ProvisionedActivation::Lost => resolve_lost(ports, creator, key, input, now_ms).await,
    }
}

/// A lost activation means another dispatch for the same `(creator, key)` already moved the
/// row. Re-read it and produce the outcome a fresh, non-retry idempotent dispatch would —
/// the winner's `active` echo, its still-`pending` in-progress, or its `failed` report.
async fn resolve_lost<C, H, M, R, V, G, E>(
    ports: &CreateAgentPorts<'_, C, H, M, R, V, G, E>,
    creator: &UserId,
    key: &IdempotencyKey,
    input: &CreateAgentInput,
    _now_ms: i64,
) -> Result<CreateAgentOutcome, CreateAgentFault>
where
    M: AgentMaterializationLookup + AgentMaterializationWrite,
    G: AgentGraphRead,
{
    let existing = find_idempotent_reservation(ports.materialization, creator, key)
        .await?
        .ok_or(CreateAgentFault::CorruptGraph {
            reason: "activation lost its race yet no reservation remains for the key",
        })?;
    match classify_idempotent(&existing, input, false) {
        IdempotentClass::Terminal(outcome) => Ok(outcome),
        IdempotentClass::EchoActive => build_echo(ports, &existing).await,
        // retry=false never yields Reopen.
        IdempotentClass::Reopen => Err(CreateAgentFault::CorruptGraph {
            reason: "non-retry idempotent dispatch reached the reopen path",
        }),
    }
}

/// Resolve the create-time placement host. An explicit host must exist, be lifecycle-`Active`
/// and be owned by the creator (refusals ordered as `assertOwnedActiveHost`: missing →
/// revoked → not-ready → not-owned). An absent host auto-places on the creator's single
/// ready host — none is unbound, more than one is ambiguous.
async fn resolve_placement<C: ComputerReadModel>(
    computers: &C,
    creator: &UserId,
    requested: Option<&HostId>,
) -> Result<Result<HostId, RuntimePlacementRefusal>, PersistenceFailure> {
    match requested {
        Some(host_id) => {
            let Some(record) = computers.read_computer(host_id.clone()).await? else {
                return Ok(Err(RuntimePlacementRefusal::HostMissing));
            };
            match lifecycle_status(&record.lifecycle()) {
                HostLifecycleStatus::Revoked => {
                    return Ok(Err(RuntimePlacementRefusal::HostRevoked));
                }
                HostLifecycleStatus::Pending | HostLifecycleStatus::Attached => {
                    return Ok(Err(RuntimePlacementRefusal::HostPendingAttach));
                }
                HostLifecycleStatus::Active => {}
            }
            if record.owner.as_ref() != Some(creator) {
                return Ok(Err(RuntimePlacementRefusal::HostNotOwned));
            }
            Ok(Ok(record.id))
        }
        None => {
            let owned = computers.list_for_owner(creator.clone()).await?;
            let mut ready = owned.into_iter().filter(|record| {
                record.owner.as_ref() == Some(creator)
                    && lifecycle_status(&record.lifecycle()).placeable()
            });
            match (ready.next(), ready.next()) {
                (None, _) => Ok(Err(RuntimePlacementRefusal::LocalHostUnbound)),
                (Some(_), Some(_)) => Ok(Err(RuntimePlacementRefusal::LocalHostAmbiguous)),
                (Some(one), None) => Ok(Ok(one.id)),
            }
        }
    }
}

/// The host-side materialize command for one reserved row. The template is always
/// `default` on the create path, and the home workspace is the deterministic `wsah_<agent>`.
fn materialize_target(
    host: HostId,
    agent: AgentId,
    name: &str,
    runtime: RuntimeKind,
    model: Option<&str>,
    description: Option<&str>,
) -> AgentMaterializeTarget {
    let agent_home_workspace = home_workspace_id_for(&agent);
    AgentMaterializeTarget::reserved(
        host,
        agent,
        agent_home_workspace,
        name.to_owned(),
        runtime,
        model.map(str::to_owned),
        description.map(str::to_owned),
        AgentMaterializeTemplate::Default,
    )
}

#[cfg(test)]
mod tests {
    // Each test starts from a fully-valid default harness and overrides the one or two
    // fields its scenario turns on; struct-update syntax for a single field of an
    // eight-field harness would read worse than the reassignment.
    #![allow(clippy::field_reassign_with_default)]

    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};

    use super::*;
    use crate::application_orchestration::agent_materialization::{
        ActivationFlipRow, IdempotentReservationRow, MaterializationFlipRow, RawAgentArmGraph,
        RawConversationAgentEdge, RawConversationArmGraph, RawConversationUserEdge,
        RawCreatorIdentity, RawHomeWorkspaceRow, RawOnboardingDmRow, RawProvisionOutcome,
        RawWorkspaceOwnerEdge, ReserveRecord, ReservedAgentRow, agent_home_workspace_description,
    };
    use crate::conversation_messaging::messaging::agent_onboarding::{
        ONBOARDING_DM_TITLE, ONBOARDING_DM_TITLE_KEY, OnboardingMetadataDigest,
        OnboardingSystemMessage, OnboardingWriteWitness,
    };
    use crate::conversation_messaging::messaging::{
        DirectPairParticipants, ParticipantHandle as MessageHandle, PrincipalIdentity,
    };
    use crate::host_runtime::agent_materialize::AgentMaterializeReceipt;
    use crate::host_runtime::computer::{ComputerRecord, HostKind, ProvisioningSource};

    const NOW: i64 = 1_000;
    const CREATOR: &str = "u1";
    const AGENT_HANDLE: &str = "agentbot";

    fn creator() -> UserId {
        UserId::parse(CREATOR).unwrap()
    }

    fn key() -> IdempotencyKey {
        IdempotencyKey::parse("idem-key-1").unwrap()
    }

    fn handle(raw: &str) -> ParticipantHandle {
        ParticipantHandle::parse(raw).unwrap()
    }

    /// The onboarding constructors speak the messaging crate's own handle newtype,
    /// which is a distinct type from the create input's `user_identity` handle.
    fn msg_handle(raw: &str) -> MessageHandle {
        MessageHandle::parse(raw).unwrap()
    }

    // ---- Fakes ----------------------------------------------------------------

    /// Distinct entropy per call, so the four minted ids never collide.
    struct CountingEntropy {
        next: std::sync::atomic::AtomicU8,
    }

    impl CountingEntropy {
        fn new() -> Self {
            Self {
                next: std::sync::atomic::AtomicU8::new(1),
            }
        }
    }

    impl SendEntropy for CountingEntropy {
        fn fill(&self, buffer: &mut [u8]) -> Result<(), EnvironmentUnavailable> {
            let base = self.next.fetch_add(1, Relaxed);
            for (offset, byte) in buffer.iter_mut().enumerate() {
                *byte = base.wrapping_add(offset as u8);
            }
            Ok(())
        }
    }

    #[derive(Clone)]
    struct FakeComputers {
        records: Vec<ComputerRecord>,
    }

    impl ComputerReadModel for FakeComputers {
        async fn list_for_owner(
            &self,
            _owner: UserId,
        ) -> Result<Vec<ComputerRecord>, PersistenceFailure> {
            Ok(self.records.clone())
        }

        async fn read_computer(
            &self,
            computer: HostId,
        ) -> Result<Option<ComputerRecord>, PersistenceFailure> {
            Ok(self.records.iter().find(|r| r.id == computer).cloned())
        }
    }

    /// Build a computer with the stamps that select a given lifecycle status.
    fn computer(
        id: &str,
        owner: Option<&str>,
        activated: bool,
        finalized: bool,
        revoked: bool,
    ) -> ComputerRecord {
        ComputerRecord {
            id: HostId::parse(id).unwrap(),
            name: id.to_owned(),
            kind: HostKind::Remote,
            created_at: 1,
            owner: owner.map(|o| UserId::parse(o).unwrap()),
            installation_id: None,
            last_seen_at: None,
            activated_at: activated.then_some(1),
            finalized_at: finalized.then_some(1),
            revoked_at: revoked.then_some(1),
            provisioning_source: ProvisioningSource::UserInvite,
            display_label: None,
        }
    }

    fn active_host(id: &str, owner: &str) -> ComputerRecord {
        computer(id, Some(owner), true, true, false)
    }

    #[derive(Clone)]
    struct FakeHandles {
        taken: bool,
    }

    impl HandleNamespaceRead for FakeHandles {
        async fn is_handle_taken(
            &self,
            _handle: &ParticipantHandle,
        ) -> Result<bool, PersistenceFailure> {
            Ok(self.taken)
        }
    }

    struct FakeMaterialization {
        existing: Option<IdempotentReservationRow>,
        reserve_conflict: bool,
        reopen: Option<MaterializationFlipRow>,
        /// When set, `fail` reports the guarded flip changed no row (`FailOutcome::Lost`) and
        /// the next idempotency lookup returns this winner.
        lost_winner: Option<IdempotentReservationRow>,
        reserve_called: AtomicBool,
        fail_called: AtomicBool,
        reopen_called: AtomicBool,
        find_calls: AtomicUsize,
        captured_model: Mutex<Option<Option<String>>>,
    }

    impl Default for FakeMaterialization {
        fn default() -> Self {
            Self {
                existing: None,
                reserve_conflict: false,
                reopen: None,
                lost_winner: None,
                reserve_called: AtomicBool::new(false),
                fail_called: AtomicBool::new(false),
                reopen_called: AtomicBool::new(false),
                find_calls: AtomicUsize::new(0),
                captured_model: Mutex::new(None),
            }
        }
    }

    impl AgentMaterializationLookup for FakeMaterialization {
        async fn find_by_idempotency(
            &self,
            _creator: &UserId,
            _key: &IdempotencyKey,
        ) -> Result<Option<IdempotentReservationRow>, PersistenceFailure> {
            // The first lookup is the entry probe; a later one is a `resolve_lost` re-read,
            // which must observe the concurrent winner rather than the original absence.
            let previous = self.find_calls.fetch_add(1, Relaxed);
            if previous >= 1 && self.lost_winner.is_some() {
                Ok(self.lost_winner.clone())
            } else {
                Ok(self.existing.clone())
            }
        }
    }

    impl AgentMaterializationWrite for FakeMaterialization {
        async fn reserve(
            &self,
            input: &ReserveAgentInput,
            now_ms: i64,
        ) -> Result<ReserveRecord, PersistenceFailure> {
            self.reserve_called.store(true, Relaxed);
            *self.captured_model.lock().unwrap() = Some(input.model.clone());
            if self.reserve_conflict {
                return Ok(ReserveRecord::ConstraintConflict);
            }
            // Mirror the request faithfully so the domain seal replays clean.
            Ok(ReserveRecord::Inserted(Box::new(ReservedAgentRow {
                agent_id: input.agent_id.clone(),
                name: input.name.as_str().to_owned(),
                display_name: input.name.as_str().to_owned(),
                runtime: input.runtime.as_str().to_owned(),
                model: input.model.clone(),
                reasoning_effort: input.reasoning_effort.clone(),
                fast_mode: input.fast_mode.map(i64::from),
                host_id: input.host_id.as_str().to_owned(),
                description: input.description.clone(),
                session_scope: input.session_scope.as_str().to_owned(),
                created_by_user_id: input.created_by_user_id.as_str().to_owned(),
                materialization_status: "pending_materialization".to_owned(),
                idempotency_key: Some(input.idempotency_key.as_str().to_owned()),
                created_at: now_ms,
                updated_at: now_ms,
            })))
        }

        async fn fail(
            &self,
            agent: &AgentId,
            failed_code: &str,
            failed_at: i64,
            now_ms: i64,
        ) -> Result<Option<MaterializationFlipRow>, PersistenceFailure> {
            self.fail_called.store(true, Relaxed);
            if self.lost_winner.is_some() {
                // The guarded `pending|failed → failed` CAS matched no row: a concurrent
                // dispatch already moved it. The caller must re-read, not report our code.
                return Ok(None);
            }
            Ok(Some(MaterializationFlipRow {
                agent_id: agent.clone(),
                materialization_status: "failed".to_owned(),
                materialization_failed_code: Some(failed_code.to_owned()),
                materialization_failed_at: Some(failed_at),
                updated_at: now_ms,
            }))
        }

        async fn reopen_failed(
            &self,
            _agent: &AgentId,
            _now_ms: i64,
        ) -> Result<Option<MaterializationFlipRow>, PersistenceFailure> {
            self.reopen_called.store(true, Relaxed);
            Ok(self.reopen.clone())
        }

        async fn mark_stale_failed(
            &self,
            _cutoff_created_at: i64,
            _failed_at: i64,
            _now_ms: i64,
        ) -> Result<Vec<MaterializationFlipRow>, PersistenceFailure> {
            Ok(Vec::new())
        }
    }

    #[derive(Clone)]
    enum MaterializeScript {
        Ok,
        Refuse(AgentMaterializeRefusal),
        Transport,
    }

    #[derive(Clone)]
    struct FakeMaterialize {
        script: MaterializeScript,
        /// Whether a daemon is captured for the host. `false` is the offline case: `capture`
        /// returns `None` and the saga must reserve nothing.
        captured: bool,
    }

    impl FakeMaterialize {
        fn online(script: MaterializeScript) -> Self {
            Self {
                script,
                captured: true,
            }
        }
    }

    #[derive(Debug)]
    struct FakeMaterializeError;

    impl std::fmt::Display for FakeMaterializeError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("scripted materialize transport fault")
        }
    }

    impl std::error::Error for FakeMaterializeError {}

    impl AgentMaterializePort for FakeMaterialize {
        type Error = FakeMaterializeError;
        type Captured = ();

        async fn capture(&self, _host: &HostId) -> Result<Option<()>, Self::Error> {
            Ok(self.captured.then_some(()))
        }

        async fn materialize(
            &self,
            _captured: (),
            _target: &AgentMaterializeTarget,
        ) -> Result<Result<AgentMaterializeReceipt, AgentMaterializeRefusal>, Self::Error> {
            match &self.script {
                MaterializeScript::Ok => Ok(Ok(AgentMaterializeReceipt::admitted(
                    "/opaque".to_owned(),
                    NOW,
                ))),
                MaterializeScript::Refuse(refusal) => Ok(Err(*refusal)),
                MaterializeScript::Transport => Err(FakeMaterializeError),
            }
        }
    }

    /// A provisioning port that returns faithful raw after-images built from the very
    /// candidate ids the use case minted and passed in, so the seal replays clean.
    struct FakeProvision {
        scope: &'static str,
        lost: bool,
    }

    fn faithful_onboarding_witness(intent: &OnboardingSystemMessage) -> OnboardingWriteWitness {
        OnboardingWriteWitness {
            id: intent.id().as_str().to_owned(),
            conversation: intent.conversation().as_str().to_owned(),
            content: intent.content().to_owned(),
            created_at_ms: intent.created_at_ms(),
            from_kind: "system".to_owned(),
            from_agent_id: None,
            from_user_id: None,
            author_id: None,
            author_name: None,
            author_display_name: None,
            thread_root_id: None,
            cc_source_message_id: None,
            metadata: OnboardingMetadataDigest {
                subtype: Some("onboarding".to_owned()),
                has_extra_keys: false,
            },
            seq: 1,
            head_latest_seq: 1,
        }
    }

    impl AgentArmProvisioningPort for FakeProvision {
        async fn provision_and_activate(
            &self,
            agent: &AgentId,
            candidate_direct_line: ConversationId,
            candidate_dm_channel: ConversationId,
            onboarding_message_id: MessageId,
            now_ms: i64,
        ) -> Result<RawProvisionOutcome, PersistenceFailure> {
            if self.lost {
                return Ok(RawProvisionOutcome::Lost);
            }
            let flip = ActivationFlipRow {
                agent_id: agent.clone(),
                name: AGENT_HANDLE.to_owned(),
                materialization_status: "active".to_owned(),
                session_scope: self.scope.to_owned(),
                created_by_user_id: CREATOR.to_owned(),
                created_at: 10,
                updated_at: now_ms,
            };
            if self.scope == "agent" {
                let intent = OnboardingSystemMessage::agent_direct_line(
                    candidate_direct_line.clone(),
                    onboarding_message_id,
                    &msg_handle(AGENT_HANDLE),
                    &msg_handle(CREATOR),
                    now_ms,
                );
                Ok(RawProvisionOutcome::Agent(Box::new(RawAgentArmGraph {
                    flip,
                    creator: RawCreatorIdentity {
                        source_user_id: CREATOR.to_owned(),
                        name: CREATOR.to_owned(),
                    },
                    direct_line: candidate_direct_line.clone(),
                    direct_line_participants: DirectPairParticipants::between(
                        PrincipalIdentity::Human(creator()),
                        PrincipalIdentity::Agent(agent.clone()),
                    )
                    .unwrap(),
                    pair_created: true,
                    onboarding: faithful_onboarding_witness(&intent),
                })))
            } else {
                let home = home_workspace_id_for(agent);
                let intent = OnboardingSystemMessage::conversation_dm(
                    candidate_dm_channel.clone(),
                    onboarding_message_id,
                    &msg_handle(AGENT_HANDLE),
                    now_ms,
                );
                Ok(RawProvisionOutcome::Conversation(Box::new(
                    RawConversationArmGraph {
                        flip,
                        home_workspace: RawHomeWorkspaceRow {
                            id: home.as_str().to_owned(),
                            name: AGENT_HANDLE.to_owned(),
                            name_key: None,
                            description: Some(agent_home_workspace_description(AGENT_HANDLE)),
                            owner_agent_id: Some(agent.as_str().to_owned()),
                            owner_user_id: Some(CREATOR.to_owned()),
                            archived_at: None,
                            created_at: now_ms,
                            updated_at: now_ms,
                        },
                        dm_channel: RawOnboardingDmRow {
                            id: candidate_dm_channel.as_str().to_owned(),
                            workspace_id: Some(home.as_str().to_owned()),
                            placement_kind: "workspace".to_owned(),
                            roster_policy: "fixed".to_owned(),
                            title: ONBOARDING_DM_TITLE.to_owned(),
                            title_key: Some(ONBOARDING_DM_TITLE_KEY.to_owned()),
                            visibility: "private".to_owned(),
                            spawned_by_agent_id: None,
                            created_at: now_ms,
                            updated_at: now_ms,
                        },
                        agent_on_dm: RawConversationAgentEdge {
                            conversation_id: candidate_dm_channel.as_str().to_owned(),
                            agent_id: agent.as_str().to_owned(),
                            joined_at: now_ms,
                        },
                        creator_workspace_owner: RawWorkspaceOwnerEdge {
                            workspace_id: home.as_str().to_owned(),
                            user_id: CREATOR.to_owned(),
                            role: "owner".to_owned(),
                            joined_at: now_ms,
                        },
                        creator_on_dm: RawConversationUserEdge {
                            conversation_id: candidate_dm_channel.as_str().to_owned(),
                            user_id: CREATOR.to_owned(),
                            joined_at: now_ms,
                        },
                        onboarding: faithful_onboarding_witness(&intent),
                    },
                )))
            }
        }

        async fn fail_after_rolled_back_provision(
            &self,
            _agent: &AgentId,
            _failed_at: i64,
            _now_ms: i64,
        ) -> Result<Option<MaterializationFlipRow>, PersistenceFailure> {
            Ok(None)
        }
    }

    #[derive(Clone, Default)]
    struct FakeGraph {
        dm_channel: Option<ConversationId>,
        onboarding: Option<MessageId>,
    }

    impl AgentGraphRead for FakeGraph {
        async fn find_home_dm_channel(
            &self,
            _agent: &AgentId,
        ) -> Result<Option<ConversationId>, PersistenceFailure> {
            Ok(self.dm_channel.clone())
        }

        async fn find_first_onboarding_message(
            &self,
            _dm: &ConversationId,
        ) -> Result<Option<MessageId>, PersistenceFailure> {
            Ok(self.onboarding.clone())
        }
    }

    // ---- Harness --------------------------------------------------------------

    struct Harness {
        computers: FakeComputers,
        handles: FakeHandles,
        materialization: FakeMaterialization,
        materialize: FakeMaterialize,
        provisioning: FakeProvision,
        graph: FakeGraph,
        entropy: CountingEntropy,
    }

    impl Default for Harness {
        fn default() -> Self {
            Self {
                computers: FakeComputers {
                    records: vec![active_host("host-1", CREATOR)],
                },
                handles: FakeHandles { taken: false },
                materialization: FakeMaterialization::default(),
                materialize: FakeMaterialize::online(MaterializeScript::Ok),
                provisioning: FakeProvision {
                    scope: "conversation",
                    lost: false,
                },
                graph: FakeGraph::default(),
                entropy: CountingEntropy::new(),
            }
        }
    }

    impl Harness {
        async fn run(
            &self,
            input: CreateAgentInput,
            retry: bool,
        ) -> Result<CreateAgentOutcome, CreateAgentFault> {
            let ports = CreateAgentPorts {
                computers: &self.computers,
                handles: &self.handles,
                materialization: &self.materialization,
                materialize: &self.materialize,
                provisioning: &self.provisioning,
                graph: &self.graph,
                entropy: &self.entropy,
            };
            create_agent(&ports, &creator(), &key(), input, retry, NOW).await
        }
    }

    fn base_input() -> CreateAgentInput {
        CreateAgentInput {
            name: handle(AGENT_HANDLE),
            runtime: RuntimeKind::Codex,
            model: None,
            reasoning_effort: None,
            fast_mode: None,
            description: None,
            host_id: None,
            session_scope: None,
        }
    }

    fn existing_row(status: &str, scope: &str) -> IdempotentReservationRow {
        // A `failed` row always carries its failure columns; every other status carries
        // neither. Mirror that here so the sealed witness is coherent, exactly as the store's
        // own `pending|failed → failed` flip leaves it.
        let (failed_code, failed_at) = if status == "failed" {
            (Some("fs_unavailable".to_owned()), Some(7))
        } else {
            (None, None)
        };
        IdempotentReservationRow {
            agent_id: AgentId::parse("agent-existing").unwrap(),
            created_by_user_id: CREATOR.to_owned(),
            idempotency_key: Some("idem-key-1".to_owned()),
            materialization_status: status.to_owned(),
            materialization_failed_code: failed_code,
            materialization_failed_at: failed_at,
            name: AGENT_HANDLE.to_owned(),
            runtime: "codex".to_owned(),
            model: None,
            description: None,
            host_id: "host-1".to_owned(),
            session_scope: scope.to_owned(),
        }
    }

    // ---- Fresh-path outcomes --------------------------------------------------

    #[tokio::test]
    async fn fresh_conversation_scope_creates_with_home_coordinates() {
        let harness = Harness::default();
        let outcome = harness.run(base_input(), false).await.unwrap();
        let CreateAgentOutcome::Created(created) = outcome else {
            panic!("expected Created, got {outcome:?}");
        };
        let CreatedArm::Conversation {
            home_workspace,
            dm_channel,
            onboarding_message_id,
        } = &created.arm
        else {
            panic!("expected a conversation arm");
        };
        assert_eq!(*home_workspace, home_workspace_id_for(&created.agent_id));
        let coords = created.arm.coordinates();
        assert_eq!(
            coords.agent_home_workspace_id.as_ref(),
            Some(home_workspace)
        );
        assert_eq!(coords.dm_channel_id.as_ref(), Some(dm_channel));
        assert_eq!(
            coords.onboarding_message_id.as_ref(),
            Some(onboarding_message_id)
        );
        assert_eq!(created.arm.destination(), dm_channel);
    }

    #[tokio::test]
    async fn fresh_agent_scope_creates_with_all_null_coordinates_but_effect_data() {
        let mut harness = Harness::default();
        harness.provisioning = FakeProvision {
            scope: "agent",
            lost: false,
        };
        let mut input = base_input();
        input.session_scope = Some(AgentSessionScope::Agent);
        let outcome = harness.run(input, false).await.unwrap();
        let CreateAgentOutcome::Created(created) = outcome else {
            panic!("expected Created, got {outcome:?}");
        };
        let CreatedArm::Agent {
            direct_line,
            pair_created,
            ..
        } = &created.arm
        else {
            panic!("expected an agent arm");
        };
        assert!(*pair_created);
        // The wire coordinates are all-null for an agent-scoped agent …
        let coords = created.arm.coordinates();
        assert_eq!(coords.agent_home_workspace_id, None);
        assert_eq!(coords.dm_channel_id, None);
        assert_eq!(coords.onboarding_message_id, None);
        // … but the effect destination is the direct line.
        assert_eq!(created.arm.destination(), direct_line);
    }

    #[tokio::test]
    async fn create_model_normalization_reaches_the_reserve() {
        // BOM is ECMAScript whitespace; NEL is not. Exercise normalization through
        // the create use case and observe what its reserve port receives.
        for (runtime, model, expected) in [
            (RuntimeKind::Qoder, None, Some("Ultimate")),
            (RuntimeKind::Qoder, Some("\u{feff}"), Some("Ultimate")),
            (RuntimeKind::Qoder, Some("\u{0085}"), Some("\u{0085}")),
            (RuntimeKind::Codex, Some("\u{feff}"), None),
            (RuntimeKind::Codex, Some("gpt-5.6-sol"), Some("gpt-5.6-sol")),
        ] {
            let mut harness = Harness::default();
            harness.provisioning = FakeProvision {
                scope: "agent",
                lost: false,
            };
            let mut input = base_input();
            input.runtime = runtime;
            input.session_scope = Some(AgentSessionScope::Agent);
            input.model = model.map(str::to_owned);
            harness.run(input, false).await.unwrap();
            assert_eq!(
                *harness.materialization.captured_model.lock().unwrap(),
                Some(expected.map(str::to_owned)),
                "runtime {runtime:?}, model {model:?}"
            );
        }
    }

    #[tokio::test]
    async fn provisioning_lost_rereads_and_echoes_the_race_winner() {
        let mut harness = Harness::default();
        harness.provisioning = FakeProvision {
            scope: "conversation",
            lost: true,
        };
        // The winner left an active agent-scoped reservation (echo needs no graph read).
        harness.materialization.lost_winner = Some(existing_row("active", "agent"));
        let mut input = base_input();
        input.session_scope = Some(AgentSessionScope::Agent);
        let outcome = harness.run(input, false).await.unwrap();
        let CreateAgentOutcome::Echo(echo) = outcome else {
            panic!("expected Echo after a lost race, got {outcome:?}");
        };
        assert!(harness.materialization.reserve_called.load(Relaxed));
        assert_eq!(harness.materialization.find_calls.load(Relaxed), 2);
        assert_eq!(echo.agent_id.as_str(), "agent-existing");
        assert_eq!(echo.coordinates.agent_home_workspace_id, None);
        assert_eq!(echo.coordinates.dm_channel_id, None);
        assert_eq!(echo.coordinates.onboarding_message_id, None);
    }

    // ---- Placement ------------------------------------------------------------

    #[tokio::test]
    async fn explicit_missing_host_is_refused_missing() {
        let harness = Harness::default();
        let mut input = base_input();
        input.host_id = Some(HostId::parse("no-such-host").unwrap());
        assert_eq!(
            harness.run(input, false).await.unwrap(),
            CreateAgentOutcome::PlacementRefused(RuntimePlacementRefusal::HostMissing)
        );
    }

    #[tokio::test]
    async fn explicit_foreign_host_is_refused_not_owned() {
        let mut harness = Harness::default();
        harness.computers = FakeComputers {
            records: vec![active_host("host-1", "someone-else")],
        };
        let mut input = base_input();
        input.host_id = Some(HostId::parse("host-1").unwrap());
        assert_eq!(
            harness.run(input, false).await.unwrap(),
            CreateAgentOutcome::PlacementRefused(RuntimePlacementRefusal::HostNotOwned)
        );
    }

    #[tokio::test]
    async fn explicit_revoked_host_is_refused_before_ownership() {
        let mut harness = Harness::default();
        // Revoked AND foreign: revoked wins, matching assertOwnedActiveHost's order.
        harness.computers = FakeComputers {
            records: vec![computer("host-1", Some("someone-else"), true, true, true)],
        };
        let mut input = base_input();
        input.host_id = Some(HostId::parse("host-1").unwrap());
        assert_eq!(
            harness.run(input, false).await.unwrap(),
            CreateAgentOutcome::PlacementRefused(RuntimePlacementRefusal::HostRevoked)
        );
    }

    #[tokio::test]
    async fn explicit_not_yet_active_host_is_refused_pending() {
        let mut harness = Harness::default();
        harness.computers = FakeComputers {
            records: vec![computer("host-1", Some(CREATOR), true, false, false)],
        };
        let mut input = base_input();
        input.host_id = Some(HostId::parse("host-1").unwrap());
        assert_eq!(
            harness.run(input, false).await.unwrap(),
            CreateAgentOutcome::PlacementRefused(RuntimePlacementRefusal::HostPendingAttach)
        );
    }

    #[tokio::test]
    async fn auto_placement_with_no_ready_host_is_unbound() {
        let mut harness = Harness::default();
        harness.computers = FakeComputers { records: vec![] };
        assert_eq!(
            harness.run(base_input(), false).await.unwrap(),
            CreateAgentOutcome::PlacementRefused(RuntimePlacementRefusal::LocalHostUnbound)
        );
    }

    #[tokio::test]
    async fn auto_placement_with_two_ready_hosts_is_ambiguous() {
        let mut harness = Harness::default();
        harness.computers = FakeComputers {
            records: vec![
                active_host("host-1", CREATOR),
                active_host("host-2", CREATOR),
            ],
        };
        assert_eq!(
            harness.run(base_input(), false).await.unwrap(),
            CreateAgentOutcome::PlacementRefused(RuntimePlacementRefusal::LocalHostAmbiguous)
        );
    }

    // ---- Name, connectivity, reserve conflict ---------------------------------

    #[tokio::test]
    async fn a_taken_handle_is_refused_before_reserving() {
        let mut harness = Harness::default();
        harness.handles = FakeHandles { taken: true };
        assert_eq!(
            harness.run(base_input(), false).await.unwrap(),
            CreateAgentOutcome::NameTaken
        );
        assert!(
            !harness.materialization.reserve_called.load(Relaxed),
            "a taken handle must not reserve a row"
        );
    }

    #[tokio::test]
    async fn an_offline_host_is_pending_and_reserves_nothing() {
        let mut harness = Harness::default();
        harness.materialize = FakeMaterialize {
            script: MaterializeScript::Ok,
            captured: false,
        };
        assert_eq!(
            harness.run(base_input(), false).await.unwrap(),
            CreateAgentOutcome::HostAttachPending
        );
        assert!(
            !harness.materialization.reserve_called.load(Relaxed),
            "an offline host must not reserve a row a synchronous create would then fail"
        );
    }

    #[tokio::test]
    async fn a_reserve_constraint_conflict_is_name_taken() {
        let mut harness = Harness::default();
        harness.materialization.reserve_conflict = true;
        assert_eq!(
            harness.run(base_input(), false).await.unwrap(),
            CreateAgentOutcome::NameTaken
        );
    }

    // ---- Materialize refusals & transport -------------------------------------

    #[tokio::test]
    async fn a_materialize_refusal_flips_failed_and_reports_the_code() {
        let mut harness = Harness::default();
        harness.materialize = FakeMaterialize::online(MaterializeScript::Refuse(
            AgentMaterializeRefusal::FsQuotaExceeded,
        ));
        assert_eq!(
            harness.run(base_input(), false).await.unwrap(),
            CreateAgentOutcome::MaterializeFailed {
                code: MaterializeFailureCode::FsQuotaExceeded
            }
        );
        assert!(harness.materialization.fail_called.load(Relaxed));
    }

    #[tokio::test]
    async fn a_materialize_transport_fault_does_not_stamp_failed() {
        let mut harness = Harness::default();
        harness.materialize = FakeMaterialize::online(MaterializeScript::Transport);
        let fault = harness.run(base_input(), false).await.unwrap_err();
        assert!(matches!(fault, CreateAgentFault::MaterializeTransport));
        assert!(
            !harness.materialization.fail_called.load(Relaxed),
            "an ambiguous transport fault must not stamp the row failed"
        );
    }

    // ---- Idempotent dispatch --------------------------------------------------

    #[tokio::test]
    async fn active_agent_scope_echoes_all_null_coordinates() {
        let mut harness = Harness::default();
        harness.materialization.existing = Some(existing_row("active", "agent"));
        // No graph exists for the agent-scoped response coordinates.
        let mut input = base_input();
        input.session_scope = Some(AgentSessionScope::Agent);
        let outcome = harness.run(input, false).await.unwrap();
        let CreateAgentOutcome::Echo(echo) = outcome else {
            panic!("expected Echo, got {outcome:?}");
        };
        assert_eq!(echo.coordinates.agent_home_workspace_id, None);
        assert_eq!(echo.coordinates.dm_channel_id, None);
        assert_eq!(echo.coordinates.onboarding_message_id, None);
    }

    #[tokio::test]
    async fn active_conversation_scope_echoes_from_graph_reads() {
        let mut harness = Harness::default();
        harness.materialization.existing = Some(existing_row("active", "conversation"));
        harness.graph = FakeGraph {
            dm_channel: Some(ConversationId::parse("dm-echo").unwrap()),
            onboarding: Some(MessageId::parse("msg-echo").unwrap()),
        };
        let outcome = harness.run(base_input(), false).await.unwrap();
        let CreateAgentOutcome::Echo(echo) = outcome else {
            panic!("expected Echo, got {outcome:?}");
        };
        assert_eq!(
            echo.coordinates.dm_channel_id,
            Some(ConversationId::parse("dm-echo").unwrap())
        );
        assert_eq!(
            echo.coordinates.agent_home_workspace_id,
            Some(WorkspaceId::parse("wsah_agent-existing").unwrap())
        );
        assert_eq!(
            echo.coordinates.onboarding_message_id,
            Some(MessageId::parse("msg-echo").unwrap())
        );
    }

    #[tokio::test]
    async fn active_conversation_scope_requires_both_graph_coordinates() {
        for (dm, reason) in [
            (
                None,
                "active conversation-scoped agent has no home DM channel",
            ),
            (
                Some("dm-echo"),
                "active conversation-scoped agent's home DM has no onboarding message",
            ),
        ] {
            let mut harness = Harness::default();
            harness.materialization.existing = Some(existing_row("active", "conversation"));
            harness.graph.dm_channel = dm.map(|id| ConversationId::parse(id).unwrap());
            let fault = harness.run(base_input(), false).await.unwrap_err();
            assert!(
                matches!(fault, CreateAgentFault::CorruptGraph { reason: actual } if actual == reason)
            );
        }
    }

    #[tokio::test]
    async fn a_pending_reservation_is_in_progress() {
        let mut harness = Harness::default();
        harness.materialization.existing =
            Some(existing_row("pending_materialization", "conversation"));
        assert_eq!(
            harness.run(base_input(), false).await.unwrap(),
            CreateAgentOutcome::InProgress
        );
    }

    #[tokio::test]
    async fn a_failed_reservation_without_retry_reports_previously_failed() {
        let mut harness = Harness::default();
        let mut row = existing_row("failed", "conversation");
        row.materialization_failed_code = Some("fs_unavailable".to_owned());
        row.materialization_failed_at = Some(42);
        harness.materialization.existing = Some(row);
        assert_eq!(
            harness.run(base_input(), false).await.unwrap(),
            CreateAgentOutcome::PreviouslyFailed {
                failed_code: "fs_unavailable".to_owned(),
                failed_at: 42,
            }
        );
    }

    #[tokio::test]
    async fn a_runtime_metadata_mismatch_is_reported() {
        let mut harness = Harness::default();
        harness.materialization.existing = Some(existing_row("active", "conversation"));
        let mut input = base_input();
        input.runtime = RuntimeKind::Claude; // reserved as codex
        assert_eq!(
            harness.run(input, false).await.unwrap(),
            CreateAgentOutcome::MetadataMismatch {
                field: MetadataField::Runtime
            }
        );
    }

    #[tokio::test]
    async fn a_retry_with_no_reservation_is_retry_target_not_found() {
        let harness = Harness::default();
        assert_eq!(
            harness.run(base_input(), true).await.unwrap(),
            CreateAgentOutcome::RetryTargetNotFound
        );
    }

    #[tokio::test]
    async fn a_retry_that_loses_the_reopen_is_retry_target_not_failed() {
        let mut harness = Harness::default();
        harness.materialization.existing = Some(existing_row("failed", "conversation"));
        harness.materialization.reopen = None; // reopen CAS lost
        assert_eq!(
            harness.run(base_input(), true).await.unwrap(),
            CreateAgentOutcome::RetryTargetNotFailed
        );
    }

    #[tokio::test]
    async fn a_retry_against_an_offline_host_reopens_nothing() {
        // Capture happens BEFORE the reopen. An offline retry must leave the failed row and
        // its original code/stamp untouched rather than churn it back to pending.
        let mut harness = Harness::default();
        harness.materialization.existing = Some(existing_row("failed", "conversation"));
        harness.materialize = FakeMaterialize {
            script: MaterializeScript::Ok,
            captured: false,
        };
        assert_eq!(
            harness.run(base_input(), true).await.unwrap(),
            CreateAgentOutcome::HostAttachPending
        );
        assert!(
            !harness.materialization.reopen_called.load(Relaxed),
            "an offline retry must not reopen the failed row"
        );
    }

    #[tokio::test]
    async fn a_lost_fail_flip_re_reads_and_echoes_the_active_winner() {
        // The refusal's `fail` flip loses to a concurrent dispatch that already activated the
        // row; the caller must report the winner's `active` echo, never its own dropped code.
        let mut harness = Harness::default();
        harness.materialize = FakeMaterialize::online(MaterializeScript::Refuse(
            AgentMaterializeRefusal::FsQuotaExceeded,
        ));
        harness.materialization.lost_winner = Some(existing_row("active", "conversation"));
        harness.graph = FakeGraph {
            dm_channel: Some(ConversationId::parse("dm-winner").unwrap()),
            onboarding: Some(MessageId::parse("msg-winner").unwrap()),
        };
        let outcome = harness.run(base_input(), false).await.unwrap();
        let CreateAgentOutcome::Echo(echo) = outcome else {
            panic!("expected the winner's Echo, got {outcome:?}");
        };
        assert_eq!(
            echo.coordinates.dm_channel_id,
            Some(ConversationId::parse("dm-winner").unwrap())
        );
    }

    #[tokio::test]
    async fn a_lost_fail_flip_reports_the_winners_failure_not_our_own() {
        // The winner independently stamped the row `failed` with a different code; we must
        // report the winner's code and stamp, not the `FsQuotaExceeded` our refusal carried.
        let mut harness = Harness::default();
        harness.materialize = FakeMaterialize::online(MaterializeScript::Refuse(
            AgentMaterializeRefusal::FsQuotaExceeded,
        ));
        harness.materialization.lost_winner = Some(existing_row("failed", "conversation"));
        assert_eq!(
            harness.run(base_input(), false).await.unwrap(),
            CreateAgentOutcome::PreviouslyFailed {
                failed_code: "fs_unavailable".to_owned(),
                failed_at: 7,
            }
        );
    }

    #[tokio::test]
    async fn a_failed_witness_missing_its_code_is_a_fault_not_an_empty_default() {
        let mut harness = Harness::default();
        let mut row = existing_row("failed", "conversation");
        row.materialization_failed_code = None; // incoherent: failed with no code
        row.materialization_failed_at = Some(9);
        harness.materialization.existing = Some(row);
        let fault = harness.run(base_input(), false).await.unwrap_err();
        assert!(
            matches!(fault, CreateAgentFault::Materialization(_)),
            "an incoherent failed witness must fault at seal, not default an empty code"
        );
    }
}
