//! How much of an agent a signed-in human may read by id.
//!
//! On the by-id read faces existence is not secret: any human on this server reads
//! the public tier of an active agent they name. The privileged tier is exactly
//! [`can_view_agent`], so this rule widens no write, list, or sub-resource face —
//! those keep calling the visibility rule directly.

use crate::{
    foundation::identity::{AgentId, Principal, UserId},
    identity_access::{
        agent_lifecycle::MaterializationStatus,
        capability::agent_view::{AgentVisibility, VisibleStages, can_view_agent},
    },
};

/// Which projection a by-id read renders.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadTier {
    Public,
    Privileged,
}

impl ReadTier {
    /// The wire token of the response's `view` field.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Privileged => "privileged",
        }
    }
}

/// The tier `viewer` reads `subject` at, or `None` for the not-found answer.
///
/// `stages` widens only the privileged tier; the public tier is active-only, so a
/// pending or failed row stays a 404 to everyone outside the visibility ACL.
pub fn agent_read_tier(
    viewer: &UserId,
    subject: &AgentId,
    agent: Option<&AgentVisibility>,
    stages: &VisibleStages,
) -> Option<ReadTier> {
    let principal = Principal::human(viewer.clone());
    if can_view_agent(&principal, subject, agent, stages).is_allowed() {
        return Some(ReadTier::Privileged);
    }
    agent
        .filter(|agent| agent.materialization_status == MaterializationStatus::Active)
        .map(|_| ReadTier::Public)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity_access::capability::agent_view::AgentViewerArms;

    fn user(id: &str) -> UserId {
        UserId::parse(id).expect("user id")
    }

    fn subject() -> AgentId {
        AgentId::parse("a1").expect("agent id")
    }

    fn owned_by(owner: &str, status: MaterializationStatus) -> AgentVisibility {
        AgentVisibility {
            viewers: AgentViewerArms::new(Some(user(owner)), [], []),
            materialization_status: status,
        }
    }

    fn every_stage() -> VisibleStages {
        VisibleStages::including([
            MaterializationStatus::PendingMaterialization,
            MaterializationStatus::Active,
            MaterializationStatus::Failed,
        ])
    }

    #[test]
    fn a_viewer_in_the_acl_reads_the_privileged_tier() {
        let agent = owned_by("owner", MaterializationStatus::Active);
        assert_eq!(
            agent_read_tier(
                &user("owner"),
                &subject(),
                Some(&agent),
                &VisibleStages::active_only()
            ),
            Some(ReadTier::Privileged)
        );
    }

    #[test]
    fn anyone_else_reads_an_active_agent_publicly() {
        let agent = owned_by("owner", MaterializationStatus::Active);
        assert_eq!(
            agent_read_tier(
                &user("stranger"),
                &subject(),
                Some(&agent),
                &VisibleStages::active_only()
            ),
            Some(ReadTier::Public)
        );
    }

    #[test]
    fn the_stage_widener_never_reaches_the_public_tier() {
        for status in [
            MaterializationStatus::PendingMaterialization,
            MaterializationStatus::Failed,
        ] {
            let agent = owned_by("owner", status);
            assert_eq!(
                agent_read_tier(&user("stranger"), &subject(), Some(&agent), &every_stage()),
                None
            );
            assert_eq!(
                agent_read_tier(&user("owner"), &subject(), Some(&agent), &every_stage()),
                Some(ReadTier::Privileged)
            );
        }
    }

    #[test]
    fn an_absent_agent_is_not_found_for_everyone() {
        assert_eq!(
            agent_read_tier(&user("owner"), &subject(), None, &every_stage()),
            None
        );
    }
}
