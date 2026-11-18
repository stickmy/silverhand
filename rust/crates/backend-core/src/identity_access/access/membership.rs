//! Workspace roster lifecycle and capability-management application contract.
//!
//! This is the authority shared by the list, role, remove, leave and capability
//! faces.  The HTTP layer never reconstructs governance and the SQLite adapter
//! must replay every mutable verdict while holding its write transaction.

use std::{error::Error, future::Future};

use crate::{
    conversation_messaging::messaging::collaboration::membership::CommittedMembershipNotice,
    foundation::{
        identity::{AgentId, ConversationId, Principal, UserId, WorkspaceId},
        persistence::PersistenceFailure,
    },
};

use super::{Capability, WorkspaceRole};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceAgentRosterEntry {
    pub id: AgentId,
    pub name: String,
    pub display_name: String,
    pub runtime: String,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub service_tier: Option<String>,
    pub host_id: String,
    pub host_display_label: Option<String>,
    pub host_name: Option<String>,
    pub description: Option<String>,
    pub session_scope: String,
    pub created_by_user_id: UserId,
    pub avatar_storage_key: Option<String>,
    pub materialization_status: String,
    pub materialization_failed_code: Option<String>,
    pub materialization_failed_at: Option<i64>,
    pub deletion_started_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceHumanRosterEntry {
    pub id: UserId,
    pub name: String,
    pub display_name: String,
    pub avatar_storage_key: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub joined_at_ms: i64,
    pub role: WorkspaceRole,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceRoster {
    pub workspace_id: WorkspaceId,
    pub agents: Vec<WorkspaceAgentRosterEntry>,
    pub humans: Vec<WorkspaceHumanRosterEntry>,
    pub viewer_role: Option<WorkspaceRole>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceRosterRead {
    WorkspaceNotFound,
    Forbidden,
    Visible(WorkspaceRoster),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HumanRoleChange {
    pub workspace_id: WorkspaceId,
    pub member: WorkspaceHumanRosterEntry,
    pub committed: Option<WorkspaceViewerMembershipChanged>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceViewerMembershipChangeKind {
    RoleChanged,
    Removed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceViewerMembershipChanged {
    pub workspace_id: WorkspaceId,
    pub user_id: UserId,
    pub kind: WorkspaceViewerMembershipChangeKind,
    pub role: Option<WorkspaceRole>,
}

/// Cross-context removal receipt: every notice id and conversation coordinate
/// comes from the same committed transaction as the workspace edge removal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceMembershipChanged {
    pub workspace_id: WorkspaceId,
    pub notices: Vec<CommittedMembershipNotice>,
    pub viewer: Option<WorkspaceViewerMembershipChanged>,
}

pub trait WorkspaceMembershipAftermath: Clone + Send + Sync + 'static {
    type Error: Error + Send + Sync + 'static;

    fn announce(
        &self,
        changed: &WorkspaceMembershipChanged,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    fn announce_viewer(
        &self,
        changed: &WorkspaceViewerMembershipChanged,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HumanRoleChangeOutcome {
    WorkspaceNotFound,
    Forbidden,
    MemberNotFound,
    OwnerRoleChangeUnsupported,
    Updated(Box<HumanRoleChange>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceAgentRemovalOutcome {
    Removed(WorkspaceMembershipChanged),
    NoSuchWorkspaceOrMember,
    Forbidden,
    AgentNotOwned,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceAgentRemovalPreflight {
    Ready { runtime_stops: Vec<ConversationId> },
    NoSuchWorkspaceOrMember,
    Forbidden,
    AgentNotOwned,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceHumanRemovalOutcome {
    Removed(WorkspaceMembershipChanged),
    WorkspaceNotFound,
    Forbidden,
    MemberNotFound,
    OwnerRemovalUnsupported,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceLeaveOutcome {
    Left(WorkspaceMembershipChanged),
    WorkspaceNotFound,
    NotMember,
    OwnerRemovalUnsupported,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentCapabilitySet {
    pub workspace_id: WorkspaceId,
    pub agent_id: AgentId,
    pub capabilities: Vec<Capability>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CapabilityReadOutcome {
    WorkspaceNotFound,
    Forbidden,
    AgentNotMember,
    Found(AgentCapabilitySet),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CapabilityReplaceOutcome {
    WorkspaceNotFound,
    Forbidden,
    AgentNotMember,
    Replaced(AgentCapabilitySet),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapabilityReplacePreflight {
    Authorized,
    WorkspaceNotFound,
    Forbidden,
    AgentNotMember,
}

/// One canonical component boundary for workspace roster state.  Implementors
/// serialize mutations and replay authorization under the same lock as the
/// affected rows; callers never receive a database handle or mutable facts.
pub trait WorkspaceMembershipPort: Clone + Send + Sync + 'static {
    fn list_workspace_members(
        &self,
        principal: &Principal,
        workspace: &WorkspaceId,
    ) -> impl Future<Output = Result<WorkspaceRosterRead, PersistenceFailure>> + Send;

    fn change_human_role(
        &self,
        principal: &Principal,
        workspace: &WorkspaceId,
        user: &UserId,
        role: WorkspaceRole,
    ) -> impl Future<Output = Result<HumanRoleChangeOutcome, PersistenceFailure>> + Send;

    fn remove_workspace_agent(
        &self,
        principal: &Principal,
        workspace: &WorkspaceId,
        agent: &AgentId,
    ) -> impl Future<Output = Result<WorkspaceAgentRemovalOutcome, PersistenceFailure>> + Send;

    /// Read-only early gate for best-effort runtime stops. The write call below
    /// recomputes the affected conversation set and authorization in its tx.
    fn preflight_remove_workspace_agent(
        &self,
        principal: &Principal,
        workspace: &WorkspaceId,
        agent: &AgentId,
    ) -> impl Future<Output = Result<WorkspaceAgentRemovalPreflight, PersistenceFailure>> + Send;

    fn remove_workspace_human(
        &self,
        principal: &Principal,
        workspace: &WorkspaceId,
        user: &UserId,
    ) -> impl Future<Output = Result<WorkspaceHumanRemovalOutcome, PersistenceFailure>> + Send;

    fn leave_workspace(
        &self,
        viewer: &UserId,
        workspace: &WorkspaceId,
    ) -> impl Future<Output = Result<WorkspaceLeaveOutcome, PersistenceFailure>> + Send;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OwnershipTransferOutcome {
    WorkspaceNotFound,
    Forbidden,
    MemberNotFound,
    /// Both role edges, committed in one transaction.
    Transferred {
        previous_owner: WorkspaceViewerMembershipChanged,
        new_owner: WorkspaceViewerMembershipChanged,
    },
}

/// One-step owner handover of a regular workspace: the current owner names another
/// human member, who becomes owner; the previous owner becomes admin.
pub trait WorkspaceOwnershipTransferPort: Clone + Send + Sync + 'static {
    fn transfer_ownership(
        &self,
        owner: &UserId,
        workspace: &WorkspaceId,
        target: &UserId,
    ) -> impl Future<Output = Result<OwnershipTransferOutcome, PersistenceFailure>> + Send;
}

/// Capability list/replace lives on the existing agent-capability axis.  The
/// replacement is an atomic set write whose authorization and membership check
/// are replayed after the write lock is acquired.
pub trait WorkspaceCapabilityPort: Clone + Send + Sync + 'static {
    fn read_capabilities(
        &self,
        principal: &Principal,
        workspace: &WorkspaceId,
        agent: &AgentId,
    ) -> impl Future<Output = Result<CapabilityReadOutcome, PersistenceFailure>> + Send;

    /// Authorize a replace before its body is decoded. This is deliberately
    /// distinct from the weaker viewer-gated read operation.
    fn preflight_replace_capabilities(
        &self,
        principal: &Principal,
        workspace: &WorkspaceId,
        agent: &AgentId,
    ) -> impl Future<Output = Result<CapabilityReplacePreflight, PersistenceFailure>> + Send;

    fn replace_capabilities(
        &self,
        principal: &Principal,
        workspace: &WorkspaceId,
        agent: &AgentId,
        capabilities: &[Capability],
    ) -> impl Future<Output = Result<CapabilityReplaceOutcome, PersistenceFailure>> + Send;
}

/// The role floor shared by mutable workspace-roster decisions.
pub fn role_meets(actor: Option<WorkspaceRole>, floor: WorkspaceRole) -> bool {
    actor.is_some_and(|role| role.meets_floor(floor))
}

/// Capability editors are workspace admins, or a workspace member editing an
/// agent they created.  Membership is explicit in both arms.
pub fn may_edit_agent_capabilities(
    actor_role: Option<WorkspaceRole>,
    actor_is_member: bool,
    actor_created_agent: bool,
) -> bool {
    actor_is_member && (role_meets(actor_role, WorkspaceRole::Admin) || actor_created_agent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_editor_rule_keeps_governance_and_creator_arms_distinct() {
        assert!(may_edit_agent_capabilities(
            Some(WorkspaceRole::Admin),
            true,
            false
        ));
        assert!(may_edit_agent_capabilities(
            Some(WorkspaceRole::Member),
            true,
            true
        ));
        assert!(!may_edit_agent_capabilities(
            Some(WorkspaceRole::Member),
            true,
            false
        ));
        assert!(!may_edit_agent_capabilities(
            Some(WorkspaceRole::Admin),
            false,
            true
        ));
    }
}
