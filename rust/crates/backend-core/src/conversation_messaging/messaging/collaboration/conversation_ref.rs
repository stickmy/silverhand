//! Workspace-conversation identity projection.
//!
//! This is deliberately narrower than a generic conversation descriptor.  A
//! direct pair has its own complete identity face and cannot be represented by
//! this value, even when its human endpoint may read that conversation.

use std::{error::Error, future::Future};

use crate::foundation::identity::{ConversationId, UserId, WorkspaceId};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConversationRosterPolicy {
    Open,
    Fixed,
}

impl ConversationRosterPolicy {
    pub fn parse(value: &str) -> Result<Self, UnknownConversationRosterPolicy> {
        match value {
            "open" => Ok(Self::Open),
            "fixed" => Ok(Self::Fixed),
            _ => Err(UnknownConversationRosterPolicy),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Fixed => "fixed",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("stored conversation roster policy is outside the published closed set")]
pub struct UnknownConversationRosterPolicy;

/// A projected workspace-conversation reference before its identity has been
/// bound back to the request that selected it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceConversationRef {
    conversation: ConversationId,
    roster: ConversationRosterPolicy,
    title: String,
    workspace: WorkspaceId,
    archived_at: Option<i64>,
}

impl WorkspaceConversationRef {
    pub fn projected(
        conversation: ConversationId,
        roster: ConversationRosterPolicy,
        title: String,
        workspace: WorkspaceId,
        archived_at: Option<i64>,
    ) -> Self {
        Self {
            conversation,
            roster,
            title,
            workspace,
            archived_at,
        }
    }

    /// Seals the returned projection to the request coordinate before any wire
    /// value can be emitted.  The port fact carries its own identity precisely
    /// so an A request with B's projection is detectable rather than silently
    /// rendered as a mixed DTO.
    pub fn bind(
        self,
        requested: &ConversationId,
    ) -> Result<BoundWorkspaceConversationRef, ConversationRefIdentityMismatch> {
        if &self.conversation != requested {
            return Err(ConversationRefIdentityMismatch);
        }
        Ok(BoundWorkspaceConversationRef(self))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundWorkspaceConversationRef(WorkspaceConversationRef);

impl BoundWorkspaceConversationRef {
    pub fn conversation(&self) -> &ConversationId {
        &self.0.conversation
    }

    pub const fn roster(&self) -> ConversationRosterPolicy {
        self.0.roster
    }

    pub fn title(&self) -> &str {
        &self.0.title
    }

    pub fn workspace(&self) -> &WorkspaceId {
        &self.0.workspace
    }

    pub const fn archived_at(&self) -> Option<i64> {
        self.0.archived_at
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("conversation-reference projection identity differs from the requested conversation")]
pub struct ConversationRefIdentityMismatch;

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ConversationRefRefusal {
    #[error("conversation not found")]
    NotFound,
    #[error("viewer cannot read conversation")]
    Forbidden,
}

pub trait ConversationRefReadPort {
    type Error: Error + Send + Sync + 'static;

    fn resolve(
        &self,
        viewer: &UserId,
        conversation: &ConversationId,
    ) -> impl Future<
        Output = Result<Result<WorkspaceConversationRef, ConversationRefRefusal>, Self::Error>,
    > + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roster_tokens_are_one_owned_bijection() {
        for (token, policy) in [
            ("open", ConversationRosterPolicy::Open),
            ("fixed", ConversationRosterPolicy::Fixed),
        ] {
            assert_eq!(ConversationRosterPolicy::parse(token), Ok(policy));
            assert_eq!(policy.as_str(), token);
        }
        assert!(ConversationRosterPolicy::parse("public").is_err());
    }
}
