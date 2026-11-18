//! The stored facts one `GET /api/agents` read needs.
//!
//! The list face asks the SAME "who may see this agent" question as
//! [`crate::identity_access::capability::agent_view`] — once per row instead of once. This module is
//! the port for that: facts in, application structs out, no rule of its own and no
//! driver.
//!
//! ## The relation narrows; the capability decides
//!
//! A viewer-keyed query over the visibility relation can only produce CANDIDATES.
//! `can_view_agent` stays the decider and runs per row, so the list answer and the
//! single-agent answer are one authority evaluated a different number of times,
//! never a transposed second copy of the predicate.
//!
//! A candidate the capability then REFUSES on ACL grounds is not a product refusal:
//! nobody asked about that agent. It means the narrowing disagrees with the decider
//! — a fact-source defect — so it takes a named contract failure alongside the
//! lane's coordinate seals, never a quiet omission. The reverse direction, an agent
//! the relation never offered that the predicate would have admitted, is
//! unobservable from inside one request and is what the pair-matrix differential
//! exists to cover.
//!
//! ## Stage and scope stay here
//!
//! `?include_failed=1` / `?include_pending=1` widen the visible STAGE set, and
//! `?scope=owned` narrows by the agent's ACTUAL creator. Neither may reach the ACL:
//! a flag never adds an audience, and `owned` never short-circuits a viewer check.
//! Holding all three in core is what makes that inspectable in one place.
//!
//! ## Order is a fact, not a choice
//!
//! The TypeScript authority orders by `created_at ASC` and names no tiebreak, so a
//! tie is UNDEFINED here too. Making it a total order would be a product decision,
//! not a porting one: this port carries the observed clause verbatim, and its
//! fixtures use distinct timestamps rather than asserting an order the source does
//! not have.

use std::future::Future;

use crate::{
    foundation::identity::{AgentId, Principal, UserId},
    foundation::persistence::PersistenceFailure,
    identity_access::agent_detail_read::AgentDetailFacts,
    identity_access::capability::agent_view::{ViewRefused, VisibleStages, can_view_agent},
    identity_access::capability::decision::Decision,
};

/// Loads every agent one viewer MAY see, as full detail bundles ordered
/// `created_at ASC`.
///
/// The rows are CANDIDATES: the relation narrows to a faithful superset of the
/// ACL-admit set (stage-agnostic), and [`visible_agents`] is what decides each one.
/// Each bundle is the SAME [`AgentDetailFacts`] the single-agent read produces, so
/// one projector serves both faces.
pub trait AgentListReadModel {
    fn read_agent_candidates(
        &self,
        viewer: &UserId,
    ) -> impl Future<Output = Result<Vec<AgentDetailFacts>, PersistenceFailure>> + Send;
}

/// Which agents `?scope=owned` keeps. The default is every visible agent; `owned`
/// narrows by the row's ACTUAL creator, never by the ACL — a viewer admitted to an
/// agent it did not create is still filtered out under `owned`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ListScope {
    All,
    Owned,
}

/// A candidate the narrowing offered that the decider then refused on ACL grounds.
///
/// The relation is supposed to be a faithful superset: every id it yields must be
/// one [`can_view_agent`] admits (stage aside). If it is not — a viewer-keyed query
/// that admits a row the predicate rejects — the two authorities have drifted, and
/// that is a fact-source defect, never a product refusal (nobody asked about that
/// agent). It surfaces named, alongside the lane's coordinate seals, rather than as
/// a silently dropped row. A `StageNotVisible` refusal is NOT this: the stage
/// widener legitimately filters an admitted row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentListFactsMismatch {
    agent: AgentId,
}

impl AgentListFactsMismatch {
    /// The candidate whose ACL verdict disagreed with the narrowing.
    pub fn agent(&self) -> &AgentId {
        &self.agent
    }
}

/// Decide the list from its candidates: keep every one this viewer may see under
/// `stages`, in the order given, then apply `scope`.
///
/// The narrowing produced `candidates`; this re-runs the ONE visibility authority
/// per row so the list answer and the single-agent answer can never be two copies
/// of the predicate. The input order is carried verbatim — the port's
/// `created_at ASC` is the adapter's clause, and re-sorting here would be a second,
/// silent order. Returned bundles borrow `candidates`, so the caller keeps
/// ownership and the projector reads one snapshot.
pub fn visible_agents<'a>(
    viewer: &UserId,
    candidates: &'a [AgentDetailFacts],
    stages: &VisibleStages,
    scope: ListScope,
) -> Result<Vec<&'a AgentDetailFacts>, AgentListFactsMismatch> {
    let principal = Principal::human(viewer.clone());
    let mut visible = Vec::new();
    for candidate in candidates {
        match can_view_agent(
            &principal,
            candidate.source(),
            Some(candidate.visibility()),
            stages,
        ) {
            Decision::Allow => {
                if matches!(scope, ListScope::Owned) && candidate.created_by_user_id() != viewer {
                    continue;
                }
                visible.push(candidate);
            }
            // The stage widener legitimately hides an admitted row; not a drift.
            Decision::Denied(ViewRefused::StageNotVisible) => {}
            // The narrowing offered a row the ACL rejects: the two authorities
            // disagree, so the facts are defective — a named contract failure, never
            // a quiet omission.
            Decision::Denied(ViewRefused::NotAViewer | ViewRefused::AgentAbsent) => {
                return Err(AgentListFactsMismatch {
                    agent: candidate.source().clone(),
                });
            }
        }
    }
    Ok(visible)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        identity_access::agent_detail_read::AgentDetailProjection,
        identity_access::agent_lifecycle::MaterializationStatus,
        identity_access::agents::runtime::RuntimeKind,
    };

    fn user(id: &str) -> UserId {
        UserId::parse(id).expect("user id")
    }

    fn agent(id: &str) -> AgentId {
        AgentId::parse(id).expect("agent id")
    }

    fn projection(created_at: i64) -> AgentDetailProjection {
        AgentDetailProjection {
            name: "agent".to_string(),
            display_name: "Agent".to_string(),
            runtime: RuntimeKind::Qoder,
            model: None,
            reasoning_effort: None,
            service_tier: None,
            host_id: "h1".to_string(),
            host_display_label: None,
            host_name: None,
            description: None,
            session_scope: "agent".to_string(),
            avatar_storage_key: None,
            materialization_failed_code: None,
            materialization_failed_at: None,
            deletion_started_at: None,
            created_at,
            updated_at: created_at,
        }
    }

    /// A candidate the ACL rejects is the narrow/decide drift: a named contract
    /// failure carrying the offending id, not a dropped row.
    #[test]
    fn an_acl_refused_candidate_is_a_named_mismatch() {
        // Built with NO arm admitting `viewer`, so the predicate returns NotAViewer.
        let stranger_only = AgentDetailFacts::from_source(
            agent("a1"),
            user("someone-else"),
            [],
            [],
            MaterializationStatus::Active,
            projection(1),
        );
        let error = visible_agents(
            &user("viewer"),
            &[stranger_only],
            &VisibleStages::active_only(),
            ListScope::All,
        )
        .expect_err("a rejected candidate is a mismatch");
        assert_eq!(error.agent(), &agent("a1"));
    }
}
