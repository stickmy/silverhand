//! Authority to ACT on an agent, as distinct from seeing one.
//!
//! One reason to change: who may change an agent or read its private state.
//! Visibility ([`super::agent_view`]) has its own reason — its audience widens
//! with co-membership, and this one must not follow. Co-membership grants
//! collaboration, not authority over a teammate's agent or access to its files.
//!
//! Every product surface composes visibility and authority as a ladder:
//! visibility answers first so an unauthorized caller cannot tell a real agent
//! from an invented one, and authority answers second.

use crate::{
    foundation::identity::{AgentId, Principal, UserId},
    identity_access::capability::decision::Decision,
};

/// The facts authority reads: who created the agent, and nothing else.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentAuthority {
    pub created_by_user_id: Option<UserId>,
}

/// Why authority was refused. The arms stay distinct because the surfaces render
/// them differently — one face carries an explicit code alongside its status —
/// and because an operator reading a refusal needs to know which of the two
/// authority arms failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthorityRefused {
    /// The row is absent. A fact, not a failed read.
    AgentAbsent,
    /// A human who did not create this agent.
    NotTheCreator,
    /// An agent principal acting on an agent other than itself.
    NotItself,
}

/// May this principal change this agent?
pub fn can_edit_agent(
    principal: &Principal,
    subject: &AgentId,
    agent: Option<&AgentAuthority>,
) -> Decision<AuthorityRefused> {
    match principal {
        // System bypasses, mirroring visibility: it reads no row to decide.
        Principal::System => Decision::Allow,
        Principal::Human { user_id } => match agent {
            None => Decision::Denied(AuthorityRefused::AgentAbsent),
            Some(agent) if agent.created_by_user_id.as_ref() == Some(user_id) => Decision::Allow,
            Some(_) => Decision::Denied(AuthorityRefused::NotTheCreator),
        },
        Principal::Agent { agent_id } => match agent {
            None => Decision::Denied(AuthorityRefused::AgentAbsent),
            Some(_) if agent_id == subject => Decision::Allow,
            Some(_) => Decision::Denied(AuthorityRefused::NotItself),
        },
    }
}

/// May this principal read this agent's own files?
///
/// A named alias of [`can_edit_agent`] on purpose: an agent's files are its
/// private cognition, so the rule is owner-or-self today, and this is the single
/// cut point for the agreed future relaxation (an owner opting one workspace's
/// tree into being visible to that workspace's members). When that lands it
/// widens here and nowhere else.
pub fn can_view_agent_files(
    principal: &Principal,
    subject: &AgentId,
    agent: Option<&AgentAuthority>,
) -> Decision<AuthorityRefused> {
    can_edit_agent(principal, subject, agent)
}

/// May this principal read or write this agent's custom environment variables?
///
/// Strictly human-creator only: unlike [`can_edit_agent`], an agent's own
/// runtime token (`Principal::Agent`) must not read or mutate its stored env
/// config through the management plane.
pub fn can_manage_agent_env(
    principal: &Principal,
    agent: Option<&AgentAuthority>,
) -> Decision<AuthorityRefused> {
    match principal {
        Principal::Human { user_id } => match agent {
            None => Decision::Denied(AuthorityRefused::AgentAbsent),
            Some(agent) if agent.created_by_user_id.as_ref() == Some(user_id) => Decision::Allow,
            Some(_) => Decision::Denied(AuthorityRefused::NotTheCreator),
        },
        Principal::Agent { .. } | Principal::System => {
            Decision::Denied(AuthorityRefused::NotTheCreator)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(id: &str) -> UserId {
        UserId::parse(id).expect("user id")
    }

    fn agent_id(id: &str) -> AgentId {
        AgentId::parse(id).expect("agent id")
    }

    fn created_by(creator: &str) -> AgentAuthority {
        AgentAuthority {
            created_by_user_id: Some(user(creator)),
        }
    }

    #[test]
    fn the_system_acts_without_reading_the_row() {
        assert!(can_edit_agent(&Principal::System, &agent_id("a1"), None).is_allowed());
    }

    #[test]
    fn an_absent_row_refuses_every_named_principal() {
        for principal in [
            Principal::human(user("u1")),
            Principal::agent(agent_id("a1")),
        ] {
            assert_eq!(
                can_edit_agent(&principal, &agent_id("a1"), None).denial(),
                Some(&AuthorityRefused::AgentAbsent)
            );
        }
    }

    #[test]
    fn only_the_creator_among_humans_may_act() {
        let agent = created_by("owner");
        assert!(
            can_edit_agent(
                &Principal::human(user("owner")),
                &agent_id("a1"),
                Some(&agent)
            )
            .is_allowed()
        );
        assert_eq!(
            can_edit_agent(
                &Principal::human(user("stranger")),
                &agent_id("a1"),
                Some(&agent)
            ),
            Decision::Denied(AuthorityRefused::NotTheCreator)
        );
    }

    #[test]
    fn a_row_with_no_recorded_creator_admits_no_human() {
        let ownerless = AgentAuthority {
            created_by_user_id: None,
        };
        assert_eq!(
            can_edit_agent(
                &Principal::human(user("owner")),
                &agent_id("a1"),
                Some(&ownerless)
            ),
            Decision::Denied(AuthorityRefused::NotTheCreator)
        );
    }

    #[test]
    fn manage_agent_env_allows_only_human_creator_and_refuses_self_and_others() {
        let authority = created_by("owner");
        assert!(
            can_manage_agent_env(&Principal::human(user("owner")), Some(&authority)).is_allowed()
        );
        assert_eq!(
            can_manage_agent_env(&Principal::human(user("teammate")), Some(&authority)),
            Decision::Denied(AuthorityRefused::NotTheCreator)
        );
        assert_eq!(
            can_manage_agent_env(&Principal::agent(agent_id("a1")), Some(&authority)),
            Decision::Denied(AuthorityRefused::NotTheCreator)
        );
        assert_eq!(
            can_manage_agent_env(&Principal::System, Some(&authority)),
            Decision::Denied(AuthorityRefused::NotTheCreator)
        );
        assert_eq!(
            can_manage_agent_env(&Principal::human(user("owner")), None),
            Decision::Denied(AuthorityRefused::AgentAbsent)
        );
    }

    #[test]
    fn an_agent_acts_only_on_itself() {
        let agent = created_by("owner");
        let itself = Principal::agent(agent_id("a1"));
        assert!(can_edit_agent(&itself, &agent_id("a1"), Some(&agent)).is_allowed());
        assert_eq!(
            can_edit_agent(&itself, &agent_id("a2"), Some(&agent)),
            Decision::Denied(AuthorityRefused::NotItself)
        );
    }

    #[test]
    fn the_files_face_admits_only_the_creator_or_agent_itself() {
        let agent = created_by("owner");
        for (principal, subject, expected) in [
            (
                Principal::human(user("owner")),
                agent_id("a1"),
                Decision::Allow,
            ),
            (
                Principal::human(user("stranger")),
                agent_id("a1"),
                Decision::Denied(AuthorityRefused::NotTheCreator),
            ),
            (
                Principal::agent(agent_id("a1")),
                agent_id("a1"),
                Decision::Allow,
            ),
            (
                Principal::agent(agent_id("a1")),
                agent_id("a2"),
                Decision::Denied(AuthorityRefused::NotItself),
            ),
        ] {
            assert_eq!(
                can_view_agent_files(&principal, &subject, Some(&agent)),
                expected
            );
        }
    }
}
