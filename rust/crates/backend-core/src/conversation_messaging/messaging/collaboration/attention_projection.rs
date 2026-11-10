//! Tiered-attention cold projections.
//!
//! These are read-side values only. They own neither preference writes nor
//! Activity/observation state: badges and modes are two faces of the same
//! tiered-attention authority, while Activity remains a separate projection.

use super::attention_preference::ChannelAttentionMode;
use crate::foundation::identity::{ConversationId, UserId, WorkspaceId};
use std::{error::Error, future::Future};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct AttentionCount(i64);

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("an attention count cannot be negative")]
pub struct NegativeAttentionCount;

impl AttentionCount {
    pub fn new(value: i64) -> Result<Self, NegativeAttentionCount> {
        if value < 0 {
            Err(NegativeAttentionCount)
        } else {
            Ok(Self(value))
        }
    }

    pub fn get(self) -> i64 {
        self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttentionBadgeRollup {
    by_workspace: Vec<WorkspaceAttentionCount>,
    direct_pairs: AttentionCount,
    total: AttentionCount,
    muted_workspaces: Vec<WorkspaceId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceAttentionCount {
    workspace: WorkspaceId,
    count: AttentionCount,
}

impl WorkspaceAttentionCount {
    pub fn projected(workspace: WorkspaceId, count: AttentionCount) -> Self {
        Self { workspace, count }
    }

    pub fn workspace(&self) -> &WorkspaceId {
        &self.workspace
    }

    pub fn count(&self) -> AttentionCount {
        self.count
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum AttentionBadgeProjectionError {
    #[error("one workspace appeared more than once in an attention badge rollup")]
    DuplicateWorkspace,
    #[error("the attention badge total overflowed")]
    TotalOverflow,
}

impl AttentionBadgeRollup {
    pub fn projected(
        mut by_workspace: Vec<WorkspaceAttentionCount>,
        direct_pairs: AttentionCount,
        mut muted_workspaces: Vec<WorkspaceId>,
    ) -> Result<Self, AttentionBadgeProjectionError> {
        by_workspace
            .sort_by(|left, right| left.workspace().as_str().cmp(right.workspace().as_str()));
        if by_workspace
            .windows(2)
            .any(|pair| pair[0].workspace() == pair[1].workspace())
        {
            return Err(AttentionBadgeProjectionError::DuplicateWorkspace);
        }
        muted_workspaces.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        muted_workspaces.dedup();
        let total = by_workspace
            .iter()
            .try_fold(direct_pairs.get(), |total, value| {
                total
                    .checked_add(value.count().get())
                    .ok_or(AttentionBadgeProjectionError::TotalOverflow)
            })?;
        // Safe: every summand is non-negative, and the checked sum succeeded.
        let total = AttentionCount::new(total).expect("non-negative checked sum");
        Ok(Self {
            by_workspace,
            direct_pairs,
            total,
            muted_workspaces,
        })
    }

    pub fn by_workspace(&self) -> &[WorkspaceAttentionCount] {
        &self.by_workspace
    }

    /// Direct pairs have no workspace, so they are summed beside `by_workspace`.
    pub fn direct_pairs(&self) -> AttentionCount {
        self.direct_pairs
    }

    pub fn total(&self) -> AttentionCount {
        self.total
    }

    pub fn muted_workspaces(&self) -> &[WorkspaceId] {
        &self.muted_workspaces
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChannelAttentionTier {
    conversation: ConversationId,
    mode: ChannelAttentionMode,
    directed_unread_count: AttentionCount,
}

impl ChannelAttentionTier {
    pub fn projected(
        conversation: ConversationId,
        mode: ChannelAttentionMode,
        directed_unread_count: AttentionCount,
    ) -> Self {
        Self {
            conversation,
            mode,
            directed_unread_count,
        }
    }

    pub fn conversation(&self) -> &ConversationId {
        &self.conversation
    }

    pub fn mode(&self) -> ChannelAttentionMode {
        self.mode
    }

    pub fn effective_mode(&self, workspace_muted: bool) -> ChannelAttentionMode {
        if workspace_muted {
            ChannelAttentionMode::Mute
        } else {
            self.mode
        }
    }

    pub fn directed_unread_count(&self) -> AttentionCount {
        self.directed_unread_count
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceAttentionModes {
    muted: bool,
    channels: Vec<ChannelAttentionTier>,
}

impl WorkspaceAttentionModes {
    pub fn projected(muted: bool, channels: Vec<ChannelAttentionTier>) -> Self {
        Self { muted, channels }
    }

    pub fn muted(&self) -> bool {
        self.muted
    }

    pub fn channels(&self) -> &[ChannelAttentionTier] {
        &self.channels
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttentionModesRefusal {
    Forbidden,
}

pub trait AttentionProjectionPort {
    type Error: Error + Send + Sync + 'static;

    fn badges(
        &self,
        viewer: &UserId,
    ) -> impl Future<Output = Result<AttentionBadgeRollup, Self::Error>> + Send;

    fn modes(
        &self,
        viewer: &UserId,
        workspace: &WorkspaceId,
    ) -> impl Future<
        Output = Result<Result<WorkspaceAttentionModes, AttentionModesRefusal>, Self::Error>,
    > + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn badge_rollup_derives_one_checked_total_and_deduplicates_muted_ids() {
        let w1 = WorkspaceId::parse("w1").unwrap();
        let w2 = WorkspaceId::parse("w2").unwrap();
        let rollup = AttentionBadgeRollup::projected(
            vec![
                WorkspaceAttentionCount::projected(w2.clone(), AttentionCount::new(3).unwrap()),
                WorkspaceAttentionCount::projected(w1.clone(), AttentionCount::new(2).unwrap()),
            ],
            AttentionCount::new(4).unwrap(),
            vec![w2.clone(), w1.clone(), w2.clone()],
        )
        .unwrap();
        assert_eq!(rollup.total().get(), 9);
        assert_eq!(rollup.direct_pairs().get(), 4);
        assert_eq!(rollup.muted_workspaces(), [w1.clone(), w2.clone()]);
        assert_eq!(
            rollup
                .by_workspace()
                .iter()
                .map(|row| (row.workspace().as_str(), row.count().get()))
                .collect::<Vec<_>>(),
            [("w1", 2), ("w2", 3)]
        );
        assert!(AttentionCount::new(-1).is_err());

        let duplicate = AttentionBadgeRollup::projected(
            vec![
                WorkspaceAttentionCount::projected(
                    WorkspaceId::parse("w1").unwrap(),
                    AttentionCount::new(1).unwrap(),
                ),
                WorkspaceAttentionCount::projected(
                    WorkspaceId::parse("w1").unwrap(),
                    AttentionCount::new(2).unwrap(),
                ),
            ],
            AttentionCount::new(0).unwrap(),
            vec![],
        );
        assert_eq!(
            duplicate,
            Err(AttentionBadgeProjectionError::DuplicateWorkspace)
        );
        assert_eq!(
            AttentionBadgeRollup::projected(
                vec![
                    WorkspaceAttentionCount::projected(w1, AttentionCount::new(i64::MAX).unwrap()),
                    WorkspaceAttentionCount::projected(w2, AttentionCount::new(1).unwrap()),
                ],
                AttentionCount::new(0).unwrap(),
                vec![]
            ),
            Err(AttentionBadgeProjectionError::TotalOverflow)
        );
        assert_eq!(
            AttentionBadgeRollup::projected(
                vec![WorkspaceAttentionCount::projected(
                    WorkspaceId::parse("w1").unwrap(),
                    AttentionCount::new(1).unwrap(),
                )],
                AttentionCount::new(i64::MAX).unwrap(),
                vec![]
            ),
            Err(AttentionBadgeProjectionError::TotalOverflow)
        );
    }

    #[test]
    fn effective_tier_has_no_independent_projection_channel() {
        let tier = ChannelAttentionTier::projected(
            ConversationId::parse("c").unwrap(),
            ChannelAttentionMode::Mentions,
            AttentionCount::new(0).unwrap(),
        );
        assert_eq!(tier.effective_mode(false), ChannelAttentionMode::Mentions);
        assert_eq!(tier.effective_mode(true), ChannelAttentionMode::Mute);
    }
}
