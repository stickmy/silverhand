//! Persisted half of one host-status read: the computer and its registrations.
//!
//! The SQLite adapter resolves both on one snapshot. The bundle keeps every agent
//! bound to the computer coordinate it came from, and this module binds that
//! coordinate plus the existing computer-access authority before exposing rows.

use std::future::Future;

use thiserror::Error;

use crate::{
    foundation::identity::{AgentId, HostId, UserId},
    foundation::persistence::PersistenceFailure,
    identity_access::agent_lifecycle::MaterializationStatus,
    identity_access::agents::runtime::RuntimeKind,
};

use super::{computer::ComputerRecord, read::computer_read_tier};
use crate::identity_access::capability::read_tier::ReadTier;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostStatusAgentFact {
    pub id: AgentId,
    pub computer: HostId,
    pub name: String,
    pub display_name: String,
    pub runtime: RuntimeKind,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub fast_mode: Option<bool>,
    pub avatar_storage_key: Option<String>,
    pub updated_at: i64,
    pub materialization_status: MaterializationStatus,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostStatusStoredFacts {
    computer: ComputerRecord,
    agents: Vec<HostStatusAgentFact>,
}

impl HostStatusStoredFacts {
    pub fn new(
        computer: ComputerRecord,
        agents: impl IntoIterator<Item = HostStatusAgentFact>,
    ) -> Self {
        Self {
            computer,
            agents: agents.into_iter().collect(),
        }
    }
}

pub trait HostStatusStoredReadModel: Clone + Send + Sync + 'static {
    fn read_host_status(
        &self,
        computer: &HostId,
    ) -> impl Future<Output = Result<Option<HostStatusStoredFacts>, PersistenceFailure>> + Send;
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum HostStatusFactsMismatch {
    #[error("host-status facts describe computer {found:?} but the read is about {expected:?}")]
    Computer { expected: HostId, found: HostId },
    #[error("host-status facts for computer {computer:?} list agent {agent:?} under {found:?}")]
    Agent {
        computer: HostId,
        agent: AgentId,
        found: HostId,
    },
}

/// A status read admitted at some tier. A face that probes the daemon admits only
/// [`ReadTier::Privileged`].
pub struct ReadableHostStatus<'a> {
    pub tier: ReadTier,
    pub computer: &'a ComputerRecord,
    /// The public tier lists only active agents, matching what a by-id agent read
    /// would show the same viewer.
    pub agents: Vec<&'a HostStatusAgentFact>,
}

pub fn readable_host_status<'a>(
    viewer: &UserId,
    requested: &HostId,
    facts: Option<&'a HostStatusStoredFacts>,
) -> Result<Option<ReadableHostStatus<'a>>, HostStatusFactsMismatch> {
    let Some(facts) = bound(requested, facts)? else {
        return Ok(None);
    };
    Ok(
        computer_read_tier(viewer, &facts.computer).map(|tier| ReadableHostStatus {
            tier,
            computer: &facts.computer,
            agents: facts
                .agents
                .iter()
                .filter(|agent| {
                    tier == ReadTier::Privileged
                        || agent.materialization_status == MaterializationStatus::Active
                })
                .collect(),
        }),
    )
}

fn bound<'a>(
    requested: &HostId,
    facts: Option<&'a HostStatusStoredFacts>,
) -> Result<Option<&'a HostStatusStoredFacts>, HostStatusFactsMismatch> {
    let Some(facts) = facts else {
        return Ok(None);
    };
    if &facts.computer.id != requested {
        return Err(HostStatusFactsMismatch::Computer {
            expected: requested.clone(),
            found: facts.computer.id.clone(),
        });
    }
    for agent in &facts.agents {
        if agent.computer != facts.computer.id {
            return Err(HostStatusFactsMismatch::Agent {
                computer: facts.computer.id.clone(),
                agent: agent.id.clone(),
                found: agent.computer.clone(),
            });
        }
    }
    Ok(Some(facts))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_runtime::computer::{HostKind, ProvisioningSource};

    fn host(id: &str) -> HostId {
        HostId::parse(id).unwrap()
    }

    fn facts(owner: &str, agent_host: &str) -> HostStatusStoredFacts {
        HostStatusStoredFacts::new(
            ComputerRecord {
                id: host("host-1"),
                name: "one".to_owned(),
                kind: HostKind::Remote,
                created_at: 1,
                owner: Some(UserId::parse(owner).unwrap()),
                installation_id: None,
                last_seen_at: None,
                activated_at: Some(1),
                finalized_at: Some(1),
                revoked_at: None,
                provisioning_source: ProvisioningSource::UserInvite,
                display_label: None,
            },
            [HostStatusAgentFact {
                id: AgentId::parse("agent-1").unwrap(),
                computer: host(agent_host),
                name: "helper".to_owned(),
                display_name: "Helper".to_owned(),
                runtime: RuntimeKind::Codex,
                model: None,
                reasoning_effort: None,
                fast_mode: None,
                avatar_storage_key: None,
                updated_at: 1,
                materialization_status: MaterializationStatus::Active,
            }],
        )
    }

    #[test]
    fn a_missing_computer_is_no_answer() {
        let viewer = UserId::parse("owner").unwrap();
        assert!(
            readable_host_status(&viewer, &host("host-1"), None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_foreign_viewer_reads_the_public_tier_and_the_owner_privileged() {
        let facts = facts("owner", "host-1");
        let tier = |viewer: &str| {
            readable_host_status(
                &UserId::parse(viewer).unwrap(),
                &host("host-1"),
                Some(&facts),
            )
            .unwrap()
            .map(|read| read.tier)
        };
        assert_eq!(tier("owner"), Some(ReadTier::Privileged));
        assert_eq!(tier("other"), Some(ReadTier::Public));
    }

    #[test]
    fn a_cross_wired_registration_is_an_internal_mismatch() {
        let viewer = UserId::parse("other").unwrap();
        assert!(matches!(
            readable_host_status(&viewer, &host("host-1"), Some(&facts("owner", "host-2"))),
            Err(HostStatusFactsMismatch::Agent { .. })
        ));
    }

    #[test]
    fn the_public_tier_lists_only_active_agents() {
        let mut facts = facts("owner", "host-1");
        let template = facts.agents[0].clone();
        for (id, status) in [
            (
                "agent-pending",
                MaterializationStatus::PendingMaterialization,
            ),
            ("agent-failed", MaterializationStatus::Failed),
        ] {
            let mut agent = template.clone();
            agent.id = AgentId::parse(id).unwrap();
            agent.materialization_status = status;
            facts.agents.push(agent);
        }
        let ids = |viewer: &str| -> Vec<String> {
            readable_host_status(
                &UserId::parse(viewer).unwrap(),
                &host("host-1"),
                Some(&facts),
            )
            .unwrap()
            .unwrap()
            .agents
            .iter()
            .map(|agent| agent.id.as_str().to_owned())
            .collect()
        };
        assert_eq!(ids("other"), vec!["agent-1"]);
        assert_eq!(
            ids("owner"),
            vec!["agent-1", "agent-pending", "agent-failed"]
        );
    }
}
