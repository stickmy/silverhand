//! Durable inbox custody and daemon delivery facts.

use std::future::Future;

use thiserror::Error;

use crate::{
    conversation_messaging::messaging::{
        AddressedPrincipals, FromKind, MessageContent, PrincipalKind, ReferencePositions,
    },
    foundation::identity::{AgentId, ConversationId, UserId, WorkspaceId},
    foundation::persistence::PersistenceFailure,
    host_runtime::runtime_credential::{RuntimePrincipal, RuntimeScope},
    identity_access::agents::{runtime::RuntimeKind, session_scope::AgentSessionScope},
};

pub const MAX_DELIVERIES_PER_BATCH: usize = 20;
pub const MAX_ENCODED_DELIVERY_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct DeliveryLeaseOwner(String);

impl DeliveryLeaseOwner {
    pub fn parse(raw: impl Into<String>) -> Result<Self, EmptyDeliveryLeaseOwner> {
        let raw = raw.into();
        if raw.is_empty() {
            return Err(EmptyDeliveryLeaseOwner);
        }
        Ok(Self(raw))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Error)]
#[error("delivery lease owner must not be empty")]
pub struct EmptyDeliveryLeaseOwner;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct InboxDeliveryId(i64);

impl InboxDeliveryId {
    pub fn parse(raw: i64) -> Result<Self, InvalidInboxDeliveryId> {
        (raw > 0).then_some(Self(raw)).ok_or(InvalidInboxDeliveryId)
    }

    pub fn as_i64(self) -> i64 {
        self.0
    }
}

#[derive(Debug, Error)]
#[error("inbox delivery id must be positive")]
pub struct InvalidInboxDeliveryId;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeliveryInstruction {
    UserForRecipient,
    UserForOther,
    NonSystemMention,
    System,
    Other,
}

pub fn classify_delivery_instruction(
    from: FromKind,
    content: &MessageContent,
    recipient_agent_name: &str,
    positions: &impl ReferencePositions,
) -> DeliveryInstruction {
    let references = AddressedPrincipals::derive(content, positions);
    let addresses_recipient = references.iter().any(|reference| {
        reference.kind() == PrincipalKind::Agent
            && reference.handle().as_str() == recipient_agent_name
    });
    match from {
        FromKind::User if references.is_empty() || addresses_recipient => {
            DeliveryInstruction::UserForRecipient
        }
        FromKind::User => DeliveryInstruction::UserForOther,
        FromKind::Agent if addresses_recipient => DeliveryInstruction::NonSystemMention,
        FromKind::Agent => DeliveryInstruction::Other,
        FromKind::System => DeliveryInstruction::System,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClaimedInboxDelivery {
    id: InboxDeliveryId,
    seq: i64,
    presentation: String,
    instruction: DeliveryInstruction,
}

impl ClaimedInboxDelivery {
    pub fn decoded(
        id: InboxDeliveryId,
        seq: i64,
        presentation: String,
        instruction: DeliveryInstruction,
    ) -> Self {
        Self {
            id,
            seq,
            presentation,
            instruction,
        }
    }

    pub fn id(&self) -> InboxDeliveryId {
        self.id
    }

    pub fn seq(&self) -> i64 {
        self.seq
    }

    pub fn presentation(&self) -> &str {
        &self.presentation
    }

    pub fn instruction(&self) -> DeliveryInstruction {
        self.instruction
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClaimedInboxBatch {
    principal: RuntimePrincipal,
    conversation: ConversationId,
    owner: DeliveryLeaseOwner,
    deliveries: Vec<ClaimedInboxDelivery>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClaimPendingInbox {
    Archived,
    Empty,
    StaleCustody,
    Transitioning,
    Claimed(ClaimedInboxBatch),
}

impl ClaimedInboxBatch {
    pub fn claimed(
        principal: RuntimePrincipal,
        conversation: ConversationId,
        owner: DeliveryLeaseOwner,
        deliveries: Vec<ClaimedInboxDelivery>,
    ) -> Option<Self> {
        (!deliveries.is_empty()).then_some(Self {
            principal,
            conversation,
            owner,
            deliveries,
        })
    }

    pub fn principal(&self) -> &RuntimePrincipal {
        &self.principal
    }

    pub fn conversation(&self) -> &ConversationId {
        &self.conversation
    }

    pub fn owner(&self) -> &DeliveryLeaseOwner {
        &self.owner
    }

    pub fn deliveries(&self) -> &[ClaimedInboxDelivery] {
        &self.deliveries
    }

    pub fn delivery_ids(&self) -> Vec<InboxDeliveryId> {
        self.deliveries
            .iter()
            .map(ClaimedInboxDelivery::id)
            .collect()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeDeliveryFailureReason {
    ChildExit,
    InitWatchdog,
    WriteFailed,
    DriverRefused,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InboxSettlementDisposition {
    Applied,
    Duplicate,
    Stale,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeMember {
    Agent {
        id: AgentId,
        name: String,
        display_name: String,
        runtime: RuntimeKind,
    },
    Human {
        id: UserId,
        name: String,
        display_name: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimePlacement {
    Workspace,
    DirectPair,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeRoster {
    Open,
    Fixed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeWorkspaceRef {
    id: WorkspaceId,
    name: String,
}

impl RuntimeWorkspaceRef {
    pub fn decoded(id: WorkspaceId, name: String) -> Self {
        Self { id, name }
    }

    pub fn id(&self) -> &WorkspaceId {
        &self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeAgentSpec {
    pub id: AgentId,
    pub name: String,
    pub runtime: RuntimeKind,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub service_tier: Option<String>,
    pub cli_session_id: Option<String>,
    pub env_vars: Vec<crate::identity_access::agent_env::AgentEnvVar>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeSessionSpec {
    pub conversation: ConversationId,
    pub placement: RuntimePlacement,
    pub roster: RuntimeRoster,
    pub title: String,
    pub spawned_by_agent: Option<AgentId>,
    pub members: Vec<RuntimeMember>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeSpawnPlan {
    pub scope: RuntimeScope,
    pub agent: RuntimeAgentSpec,
    pub workspace: Option<RuntimeWorkspaceRef>,
    pub workspace_memory_index: Vec<RuntimeWorkspaceRef>,
    pub session: Option<RuntimeSessionSpec>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeDispatchTarget {
    pub host_id: String,
    pub conversation: ConversationId,
    pub plan: RuntimeSpawnPlan,
}

pub trait RuntimeDispatchReadPort: Clone + Send + Sync + 'static {
    fn resolve_dispatch_target(
        &self,
        conversation: ConversationId,
        agent: AgentId,
    ) -> impl Future<Output = Result<Option<RuntimeDispatchTarget>, PersistenceFailure>> + Send;

    /// Resolve a runtime-*start* dispatch fact for an explicit start intent.
    ///
    /// Unlike [`Self::resolve_dispatch_target`] — which the delivery loop drives with a
    /// conversation it has already established the agent has pending input in — a start
    /// intent's `candidate_origin` is caller-supplied and unproven. The implementation
    /// therefore validates the agent's participation against the canonical authority and
    /// **read-derives** the returned origin from the matched row rather than echoing the
    /// candidate. The scope arm is owned by the agent row, never the caller. A
    /// non-participant resolves to `None` (zero wire).
    fn resolve_start_dispatch(
        &self,
        agent: AgentId,
        candidate_origin: ConversationId,
    ) -> impl Future<Output = Result<Option<RuntimeDispatchTarget>, PersistenceFailure>> + Send;

    /// Returns every currently claimed slot placed on one host. Hydration must
    /// see old daemon generations as well as the newly attached daemon so a
    /// process restart cannot strand custody behind the previous instance id.
    fn current_custodies_for_host(
        &self,
        host_id: String,
    ) -> impl Future<Output = Result<Vec<RuntimePrincipal>, PersistenceFailure>> + Send;
}

pub trait RuntimeInboxCustodyPort: Clone + Send + Sync + 'static {
    #[allow(clippy::too_many_arguments)]
    fn claim_pending<P: ReferencePositions + Sync>(
        &self,
        host_id: String,
        principal: RuntimePrincipal,
        conversation: ConversationId,
        owner: DeliveryLeaseOwner,
        now_ms: i64,
        lease_ttl_ms: i64,
        reference_positions: &P,
    ) -> impl Future<Output = Result<ClaimPendingInbox, PersistenceFailure>> + Send;

    fn settle_consumed(
        &self,
        host_id: String,
        principal: RuntimePrincipal,
        conversation: ConversationId,
        owner: DeliveryLeaseOwner,
        delivery_ids: Vec<InboxDeliveryId>,
        observed_at_ms: i64,
    ) -> impl Future<Output = Result<InboxSettlementDisposition, PersistenceFailure>> + Send;

    #[allow(clippy::too_many_arguments)]
    fn settle_failed(
        &self,
        host_id: String,
        principal: RuntimePrincipal,
        conversation: ConversationId,
        owner: DeliveryLeaseOwner,
        delivery_ids: Vec<InboxDeliveryId>,
        reason: RuntimeDeliveryFailureReason,
        observed_at_ms: i64,
    ) -> impl Future<Output = Result<InboxSettlementDisposition, PersistenceFailure>> + Send;

    fn release_owner(
        &self,
        owner: DeliveryLeaseOwner,
    ) -> impl Future<Output = Result<u64, PersistenceFailure>> + Send;
}

pub fn runtime_scope_for(
    agent: AgentId,
    conversation: ConversationId,
    session_scope: &str,
) -> Result<RuntimeScope, InvalidRuntimeSessionScope> {
    match AgentSessionScope::parse(session_scope).ok_or(InvalidRuntimeSessionScope)? {
        AgentSessionScope::Conversation => Ok(RuntimeScope::ConversationSlot {
            agent,
            source: conversation,
        }),
        AgentSessionScope::Agent => Ok(RuntimeScope::AgentSlot { agent }),
    }
}

#[derive(Debug, Error)]
#[error("persisted runtime session scope is unsupported")]
pub struct InvalidRuntimeSessionScope;

#[cfg(test)]
mod runtime_scope_for_tests {
    use super::*;

    fn agent() -> AgentId {
        AgentId::parse("a1").unwrap()
    }

    fn conversation() -> ConversationId {
        ConversationId::parse("c1").unwrap()
    }

    #[test]
    fn each_declared_scope_routes_to_its_seat_coordinate() {
        assert_eq!(
            runtime_scope_for(agent(), conversation(), "conversation").unwrap(),
            RuntimeScope::ConversationSlot {
                agent: agent(),
                source: conversation(),
            }
        );
        assert_eq!(
            runtime_scope_for(agent(), conversation(), "agent").unwrap(),
            RuntimeScope::AgentSlot { agent: agent() }
        );
    }

    #[test]
    fn an_unknown_token_is_a_fault_not_a_default_seat() {
        // The decoder owns no grammar of its own: any token the sole
        // `AgentSessionScope::parse` authority rejects must surface as a fault
        // rather than latch a default seat.
        assert!(runtime_scope_for(agent(), conversation(), "workspace").is_err());
        assert!(runtime_scope_for(agent(), conversation(), "").is_err());
    }
}
