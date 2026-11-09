//! Conversation roster membership: read, enrollment, self join/leave and removal.
//!
//! The port is deliberately conversation-owned even though some verdicts read a
//! workspace role.  A mutation is one application unit: membership edge, audit
//! message, read baseline and attention preference commit together, followed by
//! publication outside the transaction by the eventual composition adapter.

use std::{error::Error, future::Future};

use crate::foundation::{
    identity::{AgentId, ConversationId, Principal, UserId, WorkspaceId},
    name_canonicalization::is_ecmascript_trim_whitespace,
    persistence::PersistenceFailure,
};

use super::{
    super::{MessageId, ParticipantHandle},
    channel_lifecycle::ChannelDetailFact,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConversationMember {
    Agent {
        id: AgentId,
        name: String,
        display_name: String,
        runtime: String,
    },
    Human {
        id: UserId,
        name: String,
        display_name: String,
        avatar_storage_key: Option<String>,
        updated_at: i64,
    },
}

impl ConversationMember {
    pub fn id(&self) -> &str {
        match self {
            Self::Agent { id, .. } => id.as_str(),
            Self::Human { id, .. } => id.as_str(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemberRemovability {
    pub member_is_agent: bool,
    pub id: String,
    pub can_remove: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversationRoster {
    pub conversation_id: ConversationId,
    pub members: Vec<ConversationMember>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConversationRosterRead {
    ConversationNotFound,
    Forbidden,
    Visible {
        roster: ConversationRoster,
        removability: Vec<MemberRemovability>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConversationMemberCandidate {
    Agent(AgentId),
    Human(UserId),
    Handle(NormalizedMemberHandle),
}

/// One identity that survived caller-aware handle narrowing. This crosses the
/// domain boundary only when more than one candidate remains.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AmbiguousMemberCandidate {
    Agent {
        id: AgentId,
        display_name: String,
        handle: String,
    },
    Human {
        id: UserId,
        display_name: String,
        handle: String,
    },
}

/// A member-address handle after the UI-friendly `@` prefix and ECMAScript
/// surrounding whitespace have been normalized, then checked against the one
/// stored participant grammar.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NormalizedMemberHandle(ParticipantHandle);

impl NormalizedMemberHandle {
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl TryFrom<String> for NormalizedMemberHandle {
    type Error = InvalidNormalizedMemberHandle;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        let trimmed = raw.trim_matches(is_ecmascript_trim_whitespace);
        let value = trimmed.strip_prefix('@').unwrap_or(trimmed);
        ParticipantHandle::parse(value.to_owned())
            .map(Self)
            .map_err(|_| InvalidNormalizedMemberHandle)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("member handle is outside the stored participant grammar")]
pub struct InvalidNormalizedMemberHandle;

/// Immutable identities carried by a membership audit row. `kind` uses the
/// established metadata vocabulary (`user` for an acting human, `human` for a
/// human roster member).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MembershipAuditParticipant {
    pub kind: &'static str,
    pub id: String,
    pub name: String,
    pub display_name: Option<String>,
}

impl MembershipAuditParticipant {
    pub fn system() -> Self {
        Self {
            kind: "system",
            id: "system".to_owned(),
            name: "system".to_owned(),
            display_name: None,
        }
    }

    fn label(&self) -> String {
        match self.kind {
            "system" => "system".to_owned(),
            "agent" => format!("<silverhand://agent/{}>", self.name),
            "user" | "human" => format!("<silverhand://human/{}>", self.name),
            _ => unreachable!("audit participants are constructed from a closed kind"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MembershipAudit {
    pub subtype: &'static str,
    pub content: String,
    pub actor: MembershipAuditParticipant,
    pub member: MembershipAuditParticipant,
}

/// Durable coordinates for the system-message half of a membership change.
/// The message id comes from the committed insert, never from request input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommittedMembershipNotice {
    pub conversation_id: ConversationId,
    pub workspace_id: WorkspaceId,
    pub message_id: MessageId,
    /// Complete immutable system-message post-image. Membership effects publish
    /// and deliver this committed value without a post-commit row re-read.
    pub content: String,
    pub metadata: String,
    pub seq: i64,
    pub created_at: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConversationMembershipChangeKind {
    MemberAdded,
    MemberRemoved,
    SelfJoined,
    SelfLeft,
}

/// The post-commit fact consumed by the eventual composition adapter. A fresh
/// admin add/remove carries its notice; a self join/leave deliberately does not.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversationMembershipChanged {
    pub conversation_id: ConversationId,
    pub workspace_id: WorkspaceId,
    pub kind: ConversationMembershipChangeKind,
    pub notice: Option<CommittedMembershipNotice>,
    /// A human auto-enrolled by conversation add also needs its workspace list
    /// invalidated. This is absent for an already-enrolled member.
    pub workspace_enrolled_human: Option<UserId>,
}

/// Narrow mount input for event publication and notice delivery. Publication
/// failure cannot uncommit or reinterpret the membership write.
pub trait ConversationMembershipAftermath: Clone + Send + Sync + 'static {
    type Error: Error + Send + Sync + 'static;

    fn announce(
        &self,
        changed: &ConversationMembershipChanged,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// Best-effort runtime stop is an explicit mount dependency because a
/// conversation-scoped agent's process is owned by its membership edge.
pub trait MembershipRuntimeStop: Clone + Send + Sync + 'static {
    type Error: Error + Send + Sync + 'static;

    fn stop(
        &self,
        conversation: &ConversationId,
        agent: &AgentId,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConversationAgentRemovalPreflight {
    Ready { stop_runtime: bool },
    ConversationNotFound,
    Unsupported,
    Forbidden,
    CandidateNotFound,
}

pub fn member_added_audit(
    actor: MembershipAuditParticipant,
    member: MembershipAuditParticipant,
) -> MembershipAudit {
    MembershipAudit {
        subtype: "conversation_member_added",
        content: format!(
            "{} added {} to this conversation.",
            actor.label(),
            member.label()
        ),
        actor,
        member,
    }
}

pub fn member_removed_audit(
    actor: MembershipAuditParticipant,
    member: MembershipAuditParticipant,
) -> MembershipAudit {
    let content = if actor.kind == "user" && member.kind == "human" && actor.id == member.id {
        format!("{} left this conversation.", member.label())
    } else {
        format!(
            "{} removed {} from this conversation.",
            actor.label(),
            member.label()
        )
    };
    MembershipAudit {
        subtype: "conversation_member_removed",
        content,
        actor,
        member,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConversationMemberMutation {
    Changed {
        roster: ConversationRoster,
        committed: Option<Box<ConversationMembershipChanged>>,
    },
    ConversationNotFound,
    Unsupported,
    Forbidden,
    CandidateNotFound,
    HandleAmbiguous {
        candidates: Vec<AmbiguousMemberCandidate>,
    },
    CandidateNotWorkspaceMember,
    CandidateNotOwned,
    SelfTarget,
    TargetOutranksActor,
    ChannelArchived,
    ChannelOwner,
    LastHumanMember,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChannelMembershipRefusal {
    ChannelNotFound,
    Forbidden,
    NotMember,
    ChannelArchived,
    ChannelOwner,
    LastHumanMember,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ChannelMembershipOutcome {
    Changed {
        channel: Box<ChannelDetailFact>,
        committed: Option<Box<ConversationMembershipChanged>>,
    },
    Refused(ChannelMembershipRefusal),
}

pub trait ConversationMembershipPort: Clone + Send + Sync + 'static {
    fn list_conversation_members(
        &self,
        principal: &Principal,
        conversation: &ConversationId,
    ) -> impl Future<Output = Result<ConversationRosterRead, PersistenceFailure>> + Send;

    fn add_conversation_member(
        &self,
        principal: &Principal,
        conversation: &ConversationId,
        candidate: ConversationMemberCandidate,
        now_unix_ms: i64,
    ) -> impl Future<Output = Result<ConversationMemberMutation, PersistenceFailure>> + Send;

    fn join_public_channel(
        &self,
        viewer: &UserId,
        conversation: &ConversationId,
        now_unix_ms: i64,
    ) -> impl Future<Output = Result<ChannelMembershipOutcome, PersistenceFailure>> + Send;

    fn leave_channel(
        &self,
        viewer: &UserId,
        conversation: &ConversationId,
    ) -> impl Future<Output = Result<ChannelMembershipOutcome, PersistenceFailure>> + Send;

    /// Read-only early gate for the awaited runtime stop. The mutation repeats
    /// every authorization and membership decision under its write lock.
    fn preflight_remove_conversation_agent(
        &self,
        viewer: &UserId,
        conversation: &ConversationId,
        agent: &AgentId,
    ) -> impl Future<Output = Result<ConversationAgentRemovalPreflight, PersistenceFailure>> + Send;

    fn remove_conversation_agent(
        &self,
        viewer: &UserId,
        conversation: &ConversationId,
        agent: &AgentId,
    ) -> impl Future<Output = Result<ConversationMemberMutation, PersistenceFailure>> + Send;

    fn remove_conversation_human(
        &self,
        viewer: &UserId,
        conversation: &ConversationId,
        user: &UserId,
    ) -> impl Future<Output = Result<ConversationMemberMutation, PersistenceFailure>> + Send;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ManageMembersDenied;

/// Consumer-owned capability door for the internal agent face.  The grant is
/// evaluated on the resolved destination, never the credential's ambient slot.
pub trait ManageConversationMembersCapability: Clone + Send + Sync + 'static {
    fn may_manage_conversation_members(
        &self,
        agent: &AgentId,
        conversation: &ConversationId,
    ) -> impl Future<Output = Result<Result<(), ManageMembersDenied>, PersistenceFailure>> + Send;
}

/// Shared exit guard for leave, third-party removal and read-side advisory.
pub fn channel_exit_refusal(
    archived: bool,
    private_channel: bool,
    target_is_workspace_owner: bool,
    human_member_count: i64,
) -> Option<ChannelMembershipRefusal> {
    if archived {
        Some(ChannelMembershipRefusal::ChannelArchived)
    } else if private_channel && target_is_workspace_owner {
        Some(ChannelMembershipRefusal::ChannelOwner)
    } else if private_channel && human_member_count <= 1 {
        Some(ChannelMembershipRefusal::LastHumanMember)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_guard_order_is_archived_then_owner_then_last_human() {
        assert_eq!(
            channel_exit_refusal(true, true, true, 1),
            Some(ChannelMembershipRefusal::ChannelArchived)
        );
        assert_eq!(
            channel_exit_refusal(false, true, true, 1),
            Some(ChannelMembershipRefusal::ChannelOwner)
        );
        assert_eq!(
            channel_exit_refusal(false, true, false, 1),
            Some(ChannelMembershipRefusal::LastHumanMember)
        );
        assert_eq!(channel_exit_refusal(false, false, true, 1), None);
    }

    #[test]
    fn audit_language_uses_canonical_references_and_self_leave_wording() {
        let actor = MembershipAuditParticipant {
            kind: "user",
            id: "alice-id".to_owned(),
            name: "alice".to_owned(),
            display_name: Some("Alice".to_owned()),
        };
        let agent = MembershipAuditParticipant {
            kind: "agent",
            id: "bot-id".to_owned(),
            name: "bot".to_owned(),
            display_name: None,
        };

        let added = member_added_audit(actor.clone(), agent.clone());
        assert_eq!(added.subtype, "conversation_member_added");
        assert_eq!(
            added.content,
            "<silverhand://human/alice> added <silverhand://agent/bot> to this conversation."
        );

        let removed = member_removed_audit(actor.clone(), agent);
        assert_eq!(removed.subtype, "conversation_member_removed");
        assert_eq!(
            removed.content,
            "<silverhand://human/alice> removed <silverhand://agent/bot> from this conversation."
        );

        let departing = MembershipAuditParticipant {
            kind: "human",
            ..actor.clone()
        };
        let left = member_removed_audit(actor, departing);
        assert_eq!(
            left.content,
            "<silverhand://human/alice> left this conversation."
        );
    }
}
