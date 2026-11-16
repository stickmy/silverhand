//! Generation-bound runtime identity and durable custody ports.
//!
//! A credential authenticates the process that owns one runtime slot. A
//! conversation slot carries its source conversation; an agent slot has no
//! ambient conversation. Request targets are deliberately absent from both.

use std::future::Future;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    foundation::identity::{AgentId, ConversationId, DaemonInstanceId},
    foundation::persistence::PersistenceFailure,
};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum RuntimeScope {
    ConversationSlot {
        agent: AgentId,
        source: ConversationId,
    },
    AgentSlot {
        agent: AgentId,
    },
}

impl RuntimeScope {
    pub fn agent(&self) -> &AgentId {
        match self {
            Self::ConversationSlot { agent, .. } | Self::AgentSlot { agent } => agent,
        }
    }

    pub fn source(&self) -> Option<&ConversationId> {
        match self {
            Self::ConversationSlot { source, .. } => Some(source),
            Self::AgentSlot { .. } => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RuntimeGeneration(i64);

impl RuntimeGeneration {
    pub fn new(value: i64) -> Result<Self, InvalidRuntimeGeneration> {
        if value < 0 {
            return Err(InvalidRuntimeGeneration);
        }
        Ok(Self(value))
    }

    pub fn as_i64(self) -> i64 {
        self.0
    }

    pub fn checked_next(self) -> Result<Self, RuntimeGenerationExhausted> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or(RuntimeGenerationExhausted)
    }
}

#[derive(Debug, Error)]
#[error("runtime generation must be nonnegative")]
pub struct InvalidRuntimeGeneration;

#[derive(Debug, Error)]
#[error("runtime generation is exhausted")]
pub struct RuntimeGenerationExhausted;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RuntimeCustody {
    generation: RuntimeGeneration,
    daemon_instance: DaemonInstanceId,
}

impl RuntimeCustody {
    pub fn new(generation: RuntimeGeneration, daemon_instance: DaemonInstanceId) -> Self {
        Self {
            generation,
            daemon_instance,
        }
    }

    pub fn generation(&self) -> RuntimeGeneration {
        self.generation
    }

    pub fn daemon_instance(&self) -> &DaemonInstanceId {
        &self.daemon_instance
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum RuntimePrincipal {
    ConversationSlot {
        agent: AgentId,
        source: ConversationId,
        custody: RuntimeCustody,
    },
    AgentSlot {
        agent: AgentId,
        custody: RuntimeCustody,
    },
}

impl RuntimePrincipal {
    pub fn for_scope(scope: RuntimeScope, custody: RuntimeCustody) -> Self {
        match scope {
            RuntimeScope::ConversationSlot { agent, source } => Self::ConversationSlot {
                agent,
                source,
                custody,
            },
            RuntimeScope::AgentSlot { agent } => Self::AgentSlot { agent, custody },
        }
    }

    pub fn agent(&self) -> &AgentId {
        match self {
            Self::ConversationSlot { agent, .. } | Self::AgentSlot { agent, .. } => agent,
        }
    }

    pub fn source(&self) -> Option<&ConversationId> {
        match self {
            Self::ConversationSlot { source, .. } => Some(source),
            Self::AgentSlot { .. } => None,
        }
    }

    pub fn custody(&self) -> &RuntimeCustody {
        match self {
            Self::ConversationSlot { custody, .. } | Self::AgentSlot { custody, .. } => custody,
        }
    }

    pub fn scope(&self) -> RuntimeScope {
        match self {
            Self::ConversationSlot { agent, source, .. } => RuntimeScope::ConversationSlot {
                agent: agent.clone(),
                source: source.clone(),
            },
            Self::AgentSlot { agent, .. } => RuntimeScope::AgentSlot {
                agent: agent.clone(),
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeEventTime(i64);

impl RuntimeEventTime {
    pub const fn from_unix_millis(value: i64) -> Self {
        Self(value)
    }

    pub const fn as_unix_millis(self) -> i64 {
        self.0
    }
}

/// SHA-256 of a runtime bearer: the only persisted form of the bearer.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RuntimeCredentialHash(String);

impl RuntimeCredentialHash {
    pub fn of_plaintext(plaintext: &str) -> Self {
        Self(URL_SAFE_NO_PAD.encode(Sha256::digest(plaintext.as_bytes())))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClaimRuntimeCustody {
    scope: RuntimeScope,
    daemon_instance: DaemonInstanceId,
    credential: RuntimeCredentialHash,
    observed_at: RuntimeEventTime,
}

impl ClaimRuntimeCustody {
    pub fn new(
        scope: RuntimeScope,
        daemon_instance: DaemonInstanceId,
        credential: RuntimeCredentialHash,
        observed_at: RuntimeEventTime,
    ) -> Self {
        Self {
            scope,
            daemon_instance,
            credential,
            observed_at,
        }
    }

    pub fn credential(&self) -> &RuntimeCredentialHash {
        &self.credential
    }

    pub fn scope(&self) -> &RuntimeScope {
        &self.scope
    }

    pub fn daemon_instance(&self) -> &DaemonInstanceId {
        &self.daemon_instance
    }

    pub fn observed_at(&self) -> RuntimeEventTime {
        self.observed_at
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClaimedRuntime {
    principal: RuntimePrincipal,
    newly_claimed: bool,
}

impl ClaimedRuntime {
    pub fn new(principal: RuntimePrincipal, newly_claimed: bool) -> Self {
        Self {
            principal,
            newly_claimed,
        }
    }

    pub fn principal(&self) -> &RuntimePrincipal {
        &self.principal
    }

    pub fn into_principal(self) -> RuntimePrincipal {
        self.principal
    }

    pub fn is_newly_claimed(&self) -> bool {
        self.newly_claimed
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeFinalStatus {
    Idle,
    Crashed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FinalizeRuntimeCustody {
    principal: RuntimePrincipal,
    status: RuntimeFinalStatus,
    observed_at: RuntimeEventTime,
}

impl FinalizeRuntimeCustody {
    pub fn new(
        principal: RuntimePrincipal,
        status: RuntimeFinalStatus,
        observed_at: RuntimeEventTime,
    ) -> Self {
        Self {
            principal,
            status,
            observed_at,
        }
    }

    pub fn principal(&self) -> &RuntimePrincipal {
        &self.principal
    }

    pub fn status(&self) -> RuntimeFinalStatus {
        self.status
    }

    pub fn observed_at(&self) -> RuntimeEventTime {
        self.observed_at
    }
}

pub trait RuntimeCustodyPort: Clone + Send + Sync + 'static {
    /// Reuses the current generation only when the same daemon still owns it.
    /// Otherwise the adapter must advance by one and atomically append the
    /// generation-claim event before returning the new principal. Either way the
    /// claim's credential is bound to the returned generation in the same
    /// transaction, and a new generation drops every older credential.
    fn claim_or_current(
        &self,
        claim: ClaimRuntimeCustody,
    ) -> impl Future<Output = Result<Option<ClaimedRuntime>, PersistenceFailure>> + Send;

    /// Returns the current claimed principal, or `None` for a missing or
    /// unclaimed slot. Persisted coordinate corruption is an infrastructure
    /// failure, never a missing-principal result.
    fn current(
        &self,
        scope: RuntimeScope,
    ) -> impl Future<Output = Result<Option<RuntimePrincipal>, PersistenceFailure>> + Send;

    /// Linearizes verification at the live-custody read. This prevents new
    /// admission after a committed finalization; it does not fence a product
    /// request that was admitted before finalization committed.
    fn is_current(
        &self,
        principal: RuntimePrincipal,
    ) -> impl Future<Output = Result<bool, PersistenceFailure>> + Send;

    /// Clears one exact generation+daemon custody, revokes its credentials and
    /// appends its finalization event in the same transaction. A stale principal
    /// returns `false` and cannot mutate its successor.
    fn finalize(
        &self,
        finalization: FinalizeRuntimeCustody,
    ) -> impl Future<Output = Result<bool, PersistenceFailure>> + Send;

    /// Resolves a credential to the principal of its slot, only while that slot
    /// is still claimed at the credential's generation.
    fn authenticate(
        &self,
        credential: RuntimeCredentialHash,
    ) -> impl Future<Output = Result<Option<RuntimePrincipal>, PersistenceFailure>> + Send;

    /// Whether the exact current principal still has at least one credential.
    fn holds_credential(
        &self,
        principal: RuntimePrincipal,
    ) -> impl Future<Output = Result<bool, PersistenceFailure>> + Send;

    /// Revokes one credential whose start was proven never to reach a process.
    fn revoke_credential(
        &self,
        credential: RuntimeCredentialHash,
    ) -> impl Future<Output = Result<(), PersistenceFailure>> + Send;
}

/// The read-only slice of custody the runtime-switch saga depends on.
///
/// The switch route consults live custody only to source-bind the AgentSlot presentation
/// focus before a clear (see [`crate::host_runtime::agent_runtime_switch`]); it never claims, linearizes,
/// or finalizes. Depending on the full [`RuntimeCustodyPort`] would hand the product route
/// the mutating capability: `claim_or_current` bypasses bearer reservation / token
/// activation, and `finalize` bypasses the credential authority's own token revoke, scope
/// guards, and presentation-focus cleanup. A credential authority therefore exposes only
/// this read face to the switch route, delegating to its existing public `current`, so the
/// mutating escape hatch can never leave the authority's boundary.
pub trait RuntimeCustodyReadPort: Clone + Send + Sync + 'static {
    /// Returns the current claimed principal, or `None` for a missing or unclaimed slot.
    /// Persisted coordinate corruption is an infrastructure failure, never a
    /// missing-principal result.
    fn current(
        &self,
        scope: RuntimeScope,
    ) -> impl Future<Output = Result<Option<RuntimePrincipal>, PersistenceFailure>> + Send;
}

/// Process-owned presentation focus for an exact live AgentSlot generation.
///
/// ConversationSlot already carries its semantic source in [`RuntimePrincipal`], so
/// this face deliberately answers only AgentSlot principals. Implementations must
/// re-check durable current custody before exposing a process-local entry: a stale map
/// entry may exist after a racing exit, but it can never become visible again.
pub trait AgentSlotPresentationFocusPort: Clone + Send + Sync + 'static {
    fn agent_slot_presentation_focus(
        &self,
        principal: RuntimePrincipal,
    ) -> impl Future<Output = Result<Option<ConversationId>, PersistenceFailure>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_is_nonnegative_and_cannot_wrap() {
        assert!(RuntimeGeneration::new(-1).is_err());
        assert_eq!(
            RuntimeGeneration::new(4)
                .unwrap()
                .checked_next()
                .unwrap()
                .as_i64(),
            5
        );
        assert!(
            RuntimeGeneration::new(i64::MAX)
                .unwrap()
                .checked_next()
                .is_err()
        );
    }
}
