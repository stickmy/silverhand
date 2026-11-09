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

use super::{
    computer::ComputerRecord,
    read::{OwnedComputer, find_owned_computer},
};

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

pub struct VisibleHostStatus<'a> {
    owned: OwnedComputer,
    agents: &'a [HostStatusAgentFact],
}

impl VisibleHostStatus<'_> {
    pub fn computer(&self) -> &ComputerRecord {
        self.owned.record()
    }

    pub fn agents(&self) -> &[HostStatusAgentFact] {
        self.agents
    }
}

pub fn visible_host_status<'a>(
    viewer: &UserId,
    requested: &HostId,
    facts: Option<&'a HostStatusStoredFacts>,
) -> Result<Option<VisibleHostStatus<'a>>, HostStatusFactsMismatch> {
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
    Ok(
        find_owned_computer(viewer, Some(facts.computer.clone())).map(|owned| VisibleHostStatus {
            owned,
            agents: &facts.agents,
        }),
    )
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
    fn missing_and_foreign_are_one_answer() {
        let viewer = UserId::parse("owner").unwrap();
        assert!(
            visible_host_status(&viewer, &host("host-1"), None)
                .unwrap()
                .is_none()
        );
        assert!(
            visible_host_status(&viewer, &host("host-1"), Some(&facts("other", "host-1")))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_cross_wired_registration_is_an_internal_mismatch() {
        let viewer = UserId::parse("owner").unwrap();
        assert!(matches!(
            visible_host_status(&viewer, &host("host-1"), Some(&facts("owner", "host-2"))),
            Err(HostStatusFactsMismatch::Agent { .. })
        ));
    }
}
