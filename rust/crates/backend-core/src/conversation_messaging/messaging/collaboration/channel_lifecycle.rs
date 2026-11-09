//! Channel detail and lifecycle operation contracts.
//!
//! This leaf starts with the detail read. The row is a complete, placement-aware
//! conversation identity; the viewer facts are kept beside it until the request
//! coordinate is bound. Public visibility may widen admission, but it cannot
//! erase malformed persisted facts or turn a successful read into membership.

use std::{error::Error, future::Future};

use crate::{
    conversation_messaging::messaging::{
        DirectPairParticipants,
        collaboration::channel_directory::{ChannelVisibility, DirectoryRoster},
        conversation_title::CanonicalConversationTitle,
    },
    foundation::identity::{AgentId, ConversationId, UserId, WorkspaceId},
    identity_access::access::{WorkspaceRole, role_governs_workspace},
};

mod mutation;
pub use mutation::*;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ChannelLifecyclePlacement {
    Workspace(WorkspaceChannelPlacement),
    DirectPair(DirectPairParticipants),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceChannelPlacement {
    workspace: WorkspaceId,
    roster: DirectoryRoster,
}

impl WorkspaceChannelPlacement {
    pub fn workspace(&self) -> &WorkspaceId {
        &self.workspace
    }

    pub const fn roster(&self) -> DirectoryRoster {
        self.roster
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum LifecycleTitle {
    Addressed(CanonicalConversationTitle),
    DirectPair,
}

/// One strictly decoded conversation row shared by lifecycle reads and writes.
///
/// Workspace rows carry the single canonical title/key value. Direct pairs have
/// no address and therefore carry the published empty stored title. The private
/// fields prevent a later adapter from changing placement/roster/visibility as
/// independent coordinates after this gate.
///
/// The public read path is the positive control for the two privacy fences:
///
/// ```
/// use silverhand_backend_core::conversation_messaging::messaging::collaboration::channel_lifecycle::ChannelLifecycleRow;
/// fn inspect(row: &ChannelLifecycleRow) {
///     let _ = (row.conversation(), row.visibility());
/// }
/// ```
///
/// A selected row's source coordinate cannot be relabelled after decode:
///
/// ```compile_fail
/// use silverhand_backend_core::{
///     foundation::identity::ConversationId,
///     conversation_messaging::messaging::collaboration::channel_lifecycle::ChannelLifecycleRow,
/// };
/// fn relabel(mut row: ChannelLifecycleRow) {
///     row.conversation = ConversationId::parse("foreign").unwrap();
/// }
/// ```
///
/// Nor can visibility be widened independently of the sealed placement/roster:
///
/// ```compile_fail
/// use silverhand_backend_core::conversation_messaging::messaging::collaboration::{
///     channel_directory::ChannelVisibility,
///     channel_lifecycle::ChannelLifecycleRow,
/// };
/// fn widen(mut row: ChannelLifecycleRow) {
///     row.visibility = ChannelVisibility::Public;
/// }
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChannelLifecycleRow {
    conversation: ConversationId,
    placement: ChannelLifecyclePlacement,
    title: LifecycleTitle,
    spawned_by_agent: Option<AgentId>,
    archived_at: Option<i64>,
    latest_message_seq: i64,
    created_at: i64,
    updated_at: i64,
    visibility: ChannelVisibility,
}

impl ChannelLifecycleRow {
    #[allow(clippy::too_many_arguments)]
    pub fn workspace_placed(
        conversation: ConversationId,
        workspace: WorkspaceId,
        roster: DirectoryRoster,
        title: CanonicalConversationTitle,
        spawned_by_agent: Option<AgentId>,
        archived_at: Option<i64>,
        latest_message_seq: i64,
        created_at: i64,
        updated_at: i64,
        visibility: ChannelVisibility,
    ) -> Result<Self, ChannelLifecycleProjectionError> {
        if roster == DirectoryRoster::Fixed && visibility != ChannelVisibility::Private {
            return Err(ChannelLifecycleProjectionError::PublicFixedRoster);
        }
        if latest_message_seq < 0 {
            return Err(ChannelLifecycleProjectionError::NegativeLatestMessageSeq);
        }
        Ok(Self {
            conversation,
            placement: ChannelLifecyclePlacement::Workspace(WorkspaceChannelPlacement {
                workspace,
                roster,
            }),
            title: LifecycleTitle::Addressed(title),
            spawned_by_agent,
            archived_at,
            latest_message_seq,
            created_at,
            updated_at,
            visibility,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn direct_pair(
        conversation: ConversationId,
        participants: DirectPairParticipants,
        stored_title: String,
        spawned_by_agent: Option<AgentId>,
        archived_at: Option<i64>,
        latest_message_seq: i64,
        created_at: i64,
        updated_at: i64,
        visibility: ChannelVisibility,
    ) -> Result<Self, ChannelLifecycleProjectionError> {
        if !stored_title.is_empty() {
            return Err(ChannelLifecycleProjectionError::DirectPairStoredTitle);
        }
        if visibility != ChannelVisibility::Private {
            return Err(ChannelLifecycleProjectionError::PublicFixedRoster);
        }
        if latest_message_seq < 0 {
            return Err(ChannelLifecycleProjectionError::NegativeLatestMessageSeq);
        }
        Ok(Self {
            conversation,
            placement: ChannelLifecyclePlacement::DirectPair(participants),
            title: LifecycleTitle::DirectPair,
            spawned_by_agent,
            archived_at,
            latest_message_seq,
            created_at,
            updated_at,
            visibility,
        })
    }

    pub fn conversation(&self) -> &ConversationId {
        &self.conversation
    }

    pub fn placement(&self) -> &ChannelLifecyclePlacement {
        &self.placement
    }

    pub fn workspace(&self) -> Option<&WorkspaceId> {
        match &self.placement {
            ChannelLifecyclePlacement::Workspace(value) => Some(value.workspace()),
            ChannelLifecyclePlacement::DirectPair(_) => None,
        }
    }

    pub const fn roster(&self) -> DirectoryRoster {
        match &self.placement {
            ChannelLifecyclePlacement::Workspace(value) => value.roster(),
            ChannelLifecyclePlacement::DirectPair(_) => DirectoryRoster::Fixed,
        }
    }

    pub fn title(&self) -> &str {
        match &self.title {
            LifecycleTitle::Addressed(value) => value.title(),
            LifecycleTitle::DirectPair => "",
        }
    }

    pub fn title_key(&self) -> Option<&str> {
        match &self.title {
            LifecycleTitle::Addressed(value) => Some(value.key()),
            LifecycleTitle::DirectPair => None,
        }
    }

    pub fn spawned_by_agent(&self) -> Option<&AgentId> {
        self.spawned_by_agent.as_ref()
    }

    pub const fn archived_at(&self) -> Option<i64> {
        self.archived_at
    }

    pub const fn latest_message_seq(&self) -> i64 {
        self.latest_message_seq
    }

    pub const fn created_at(&self) -> i64 {
        self.created_at
    }

    pub const fn updated_at(&self) -> i64 {
        self.updated_at
    }

    pub const fn visibility(&self) -> ChannelVisibility {
        self.visibility
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChannelLeaveBlockedReason {
    NotMember,
    ChannelArchived,
    ChannelOwner,
    LastHumanMember,
}

impl ChannelLeaveBlockedReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotMember => "not_member",
            Self::ChannelArchived => "channel_archived",
            Self::ChannelOwner => "channel_owner",
            Self::LastHumanMember => "last_human_member",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChannelLeaveEligibility {
    Allowed,
    Blocked(ChannelLeaveBlockedReason),
}

impl ChannelLeaveEligibility {
    pub const fn can_leave(self) -> bool {
        matches!(self, Self::Allowed)
    }

    pub const fn blocked_reason(self) -> Option<ChannelLeaveBlockedReason> {
        match self {
            Self::Allowed => None,
            Self::Blocked(reason) => Some(reason),
        }
    }
}

/// The viewer-specific detail fact before request-coordinate binding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChannelDetailFact {
    source_viewer: UserId,
    row: ChannelLifecycleRow,
    is_member: bool,
    leave: ChannelLeaveEligibility,
    can_update_visibility: bool,
}

impl ChannelDetailFact {
    pub fn projected(
        source_viewer: UserId,
        row: ChannelLifecycleRow,
        is_member: bool,
        workspace_role: Option<WorkspaceRole>,
        human_member_count: u64,
    ) -> Result<Self, ChannelLifecycleProjectionError> {
        if row.workspace().is_none() && workspace_role.is_some() {
            return Err(ChannelLifecycleProjectionError::WorkspaceRoleWithoutWorkspace);
        }
        let leave = leave_eligibility(&row, is_member, workspace_role, human_member_count);
        let can_update_visibility = row.roster() == DirectoryRoster::Open
            && row.workspace().is_some()
            && role_governs_workspace(workspace_role);
        Ok(Self {
            source_viewer,
            row,
            is_member,
            leave,
            can_update_visibility,
        })
    }

    pub fn bind(
        self,
        requested_viewer: &UserId,
        requested_conversation: &ConversationId,
    ) -> Result<BoundChannelDetail, ChannelDetailIdentityMismatch> {
        if &self.source_viewer != requested_viewer
            || self.row.conversation() != requested_conversation
        {
            return Err(ChannelDetailIdentityMismatch);
        }
        Ok(BoundChannelDetail(self))
    }
}

fn leave_eligibility(
    row: &ChannelLifecycleRow,
    is_member: bool,
    workspace_role: Option<WorkspaceRole>,
    human_member_count: u64,
) -> ChannelLeaveEligibility {
    if row.roster() != DirectoryRoster::Open || !is_member {
        return ChannelLeaveEligibility::Blocked(ChannelLeaveBlockedReason::NotMember);
    }
    if row.archived_at().is_some() {
        return ChannelLeaveEligibility::Blocked(ChannelLeaveBlockedReason::ChannelArchived);
    }
    if row.visibility() == ChannelVisibility::Private
        && workspace_role == Some(WorkspaceRole::Owner)
    {
        return ChannelLeaveEligibility::Blocked(ChannelLeaveBlockedReason::ChannelOwner);
    }
    if row.visibility() == ChannelVisibility::Private && human_member_count <= 1 {
        return ChannelLeaveEligibility::Blocked(ChannelLeaveBlockedReason::LastHumanMember);
    }
    ChannelLeaveEligibility::Allowed
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundChannelDetail(ChannelDetailFact);

impl BoundChannelDetail {
    pub fn viewer(&self) -> &UserId {
        &self.0.source_viewer
    }

    pub fn row(&self) -> &ChannelLifecycleRow {
        &self.0.row
    }

    pub const fn is_member(&self) -> bool {
        self.0.is_member
    }

    pub const fn leave(&self) -> ChannelLeaveEligibility {
        self.0.leave
    }

    pub const fn can_update_visibility(&self) -> bool {
        self.0.can_update_visibility
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ChannelLifecycleProjectionError {
    #[error("a fixed-roster conversation cannot be public")]
    PublicFixedRoster,
    #[error("a direct pair must retain the empty stored title")]
    DirectPairStoredTitle,
    #[error("a workspace role was attached to a workspace-less conversation")]
    WorkspaceRoleWithoutWorkspace,
    #[error("latest_message_seq must be non-negative")]
    NegativeLatestMessageSeq,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("channel detail fact does not belong to the requested viewer/conversation")]
pub struct ChannelDetailIdentityMismatch;

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ChannelDetailRefusal {
    #[error("channel not found")]
    NotFound,
    #[error("viewer cannot read channel")]
    Forbidden,
}

pub trait ChannelDetailReadPort {
    type Error: Error + Send + Sync + 'static;

    fn detail(
        &self,
        viewer: &UserId,
        conversation: &ConversationId,
    ) -> impl Future<Output = Result<Result<ChannelDetailFact, ChannelDetailRefusal>, Self::Error>> + Send;
}

#[cfg(test)]
mod tests {
    use crate::{
        conversation_messaging::messaging::PrincipalIdentity,
        foundation::identity::{AgentId, UserId},
    };

    use super::*;

    fn conversation(raw: &str) -> ConversationId {
        ConversationId::parse(raw).unwrap()
    }

    fn workspace_row(
        roster: DirectoryRoster,
        visibility: ChannelVisibility,
        archived_at: Option<i64>,
    ) -> ChannelLifecycleRow {
        ChannelLifecycleRow::workspace_placed(
            conversation("c"),
            WorkspaceId::parse("w").unwrap(),
            roster,
            CanonicalConversationTitle::parse("General").unwrap(),
            None,
            archived_at,
            0,
            1,
            2,
            visibility,
        )
        .unwrap()
    }

    #[test]
    fn exit_reason_priority_matches_the_single_product_rule() {
        let viewer = UserId::parse("u").unwrap();
        let cases = [
            (
                workspace_row(DirectoryRoster::Open, ChannelVisibility::Private, None),
                false,
                Some(WorkspaceRole::Owner),
                1,
                ChannelLeaveEligibility::Blocked(ChannelLeaveBlockedReason::NotMember),
            ),
            (
                workspace_row(DirectoryRoster::Open, ChannelVisibility::Private, Some(5)),
                true,
                Some(WorkspaceRole::Owner),
                1,
                ChannelLeaveEligibility::Blocked(ChannelLeaveBlockedReason::ChannelArchived),
            ),
            (
                workspace_row(DirectoryRoster::Open, ChannelVisibility::Private, None),
                true,
                Some(WorkspaceRole::Owner),
                1,
                ChannelLeaveEligibility::Blocked(ChannelLeaveBlockedReason::ChannelOwner),
            ),
            (
                workspace_row(DirectoryRoster::Open, ChannelVisibility::Private, None),
                true,
                Some(WorkspaceRole::Member),
                1,
                ChannelLeaveEligibility::Blocked(ChannelLeaveBlockedReason::LastHumanMember),
            ),
            (
                workspace_row(DirectoryRoster::Open, ChannelVisibility::Public, None),
                true,
                Some(WorkspaceRole::Owner),
                1,
                ChannelLeaveEligibility::Allowed,
            ),
        ];
        for (row, is_member, role, count, expected) in cases {
            let fact =
                ChannelDetailFact::projected(viewer.clone(), row, is_member, role, count).unwrap();
            assert_eq!(fact.leave, expected);
        }
    }

    #[test]
    fn fixed_roster_never_claims_leave_or_visibility_mutation() {
        let row = workspace_row(DirectoryRoster::Fixed, ChannelVisibility::Private, None);
        let fact = ChannelDetailFact::projected(
            UserId::parse("u").unwrap(),
            row,
            true,
            Some(WorkspaceRole::Owner),
            2,
        )
        .unwrap();
        assert_eq!(
            fact.leave,
            ChannelLeaveEligibility::Blocked(ChannelLeaveBlockedReason::NotMember)
        );
        assert!(!fact.can_update_visibility);
    }

    #[test]
    fn direct_pair_is_a_closed_workspace_less_private_arm() {
        let participants = DirectPairParticipants::between(
            PrincipalIdentity::Human(UserId::parse("u").unwrap()),
            PrincipalIdentity::Agent(AgentId::parse("a").unwrap()),
        )
        .unwrap();
        let row = ChannelLifecycleRow::direct_pair(
            conversation("pair"),
            participants,
            String::new(),
            None,
            None,
            0,
            1,
            2,
            ChannelVisibility::Private,
        )
        .unwrap();
        assert!(row.workspace().is_none());
        assert_eq!(row.roster(), DirectoryRoster::Fixed);
        assert_eq!(row.title(), "");
        assert_eq!(row.title_key(), None);
        assert_eq!(
            ChannelDetailFact::projected(
                UserId::parse("u").unwrap(),
                row,
                false,
                Some(WorkspaceRole::Member),
                0,
            ),
            Err(ChannelLifecycleProjectionError::WorkspaceRoleWithoutWorkspace)
        );
    }
}
