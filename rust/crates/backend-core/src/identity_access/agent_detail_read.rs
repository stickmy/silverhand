//! The stored facts one `GET /api/agents/{id}` read needs.
//!
//! Detail composes the SAME visibility ACL as availability
//! ([`crate::identity_access::capability::agent_view`]) with a projection of the agent's own row.
//! It holds no rule of its own: facts in, application structs out, so the route
//! and its tests share one authority without a driver.
//!
//! ## One row, one snapshot
//!
//! Unlike availability there is no second host-side read: everything the detail
//! wire shows comes off the agent row and its host `LEFT JOIN`, read on ONE
//! deferred snapshot together with the two set-shaped ACL arms. The visibility a
//! caller is judged against and the values later projected to the wire therefore
//! come from ONE view of the database.
//!
//! ## The ACL input and the wire echo cannot disagree
//!
//! The creator and the stage are each ONE decoded value the ACL judges and the
//! wire echoes. Rather than trust two copies to stay equal, the bundle holds one
//! of each and hands both faces the same value:
//!
//! * The creator is a single [`UserId`] input to [`from_source`](AgentDetailFacts::from_source).
//!   The constructor is the ONLY place viewer arms are built, so it seeds the ACL
//!   creator arm from that id, and [`created_by_user_id`](AgentDetailFacts::created_by_user_id)
//!   returns the same field. The projection carries no creator of its own, so an
//!   adapter cannot admit one human and render another — the splice is
//!   unconstructable, not merely discouraged.
//! * The stage gate and the wire's `materialization_status` are the same
//!   [`MaterializationStatus`](crate::identity_access::agent_lifecycle::MaterializationStatus), read
//!   back off [`AgentVisibility`].

use std::future::Future;

use crate::{
    foundation::identity::{AgentId, UserId},
    foundation::persistence::PersistenceFailure,
    identity_access::agent_lifecycle::MaterializationStatus,
    identity_access::agents::runtime::RuntimeKind,
    identity_access::capability::agent_view::{AgentViewerArms, AgentVisibility},
};

/// The wire-facing fields of one agent-detail read that the visibility gate does
/// not already carry.
///
/// `created_by_user_id` is deliberately ABSENT: the creator is one decoded id
/// threaded through [`AgentDetailFacts::from_source`] to both the ACL creator arm
/// and the wire, so it lives on the bundle, never on a projection that could carry
/// a second copy. `materialization_status` is likewise absent — it is read from
/// [`AgentVisibility::materialization_status`], the value the stage gate uses.
///
/// The host label and avatar URL are NOT resolved here: they are wire derivations
/// (a coalesce and a URL template) owned by the projection layer that turns these
/// raw facts into the DTO, so this struct stays the pure stored row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentDetailProjection {
    pub name: String,
    pub display_name: String,
    /// The closed runtime set, decoded — not a pass-through string like its
    /// neighbours. `runtime` is a public wire vocabulary two faces render (list and
    /// detail), so a stored value outside the set is corruption that must fail closed
    /// here rather than reach the wire as a token no other consumer accepts.
    pub runtime: RuntimeKind,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub service_tier: Option<String>,
    pub host_id: String,
    /// `hosts.display_label` via the `LEFT JOIN`; absent when the host row is gone.
    pub host_display_label: Option<String>,
    /// `hosts.name` via the `LEFT JOIN`; absent when the host row is gone.
    pub host_name: Option<String>,
    pub description: Option<String>,
    pub session_scope: String,
    /// The blob coordinate that decides whether the wire carries an avatar URL. The
    /// bytes never reach here — only whether one exists.
    pub avatar_storage_key: Option<String>,
    pub materialization_failed_code: Option<String>,
    pub materialization_failed_at: Option<i64>,
    pub deletion_started_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// The stored facts one authenticated agent-detail read is decided and projected
/// from, read together on one snapshot.
///
/// Sealed to ONE decoded source id — the `agents.id` the adapter actually read
/// back, never the request echo. Both the ACL a caller is judged against and the
/// row a value is projected from hang off that one coordinate, and
/// [`source`](Self::source) is what the route binds to the id it was asked for
/// BEFORE the ACL decision and BEFORE it renders the wire `id`.
///
/// Sealed likewise to ONE decoded `creator`: [`from_source`](Self::from_source) is
/// the sole constructor, it seeds the ACL creator arm from that id, and
/// [`created_by_user_id`](Self::created_by_user_id) returns the same field. There
/// is no second creator anywhere, so a bundle that admits one human and renders
/// another cannot be built — the ACL/wire splice is a type error, not a runtime
/// check.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentDetailFacts {
    source: AgentId,
    creator: UserId,
    visibility: AgentVisibility,
    projection: AgentDetailProjection,
}

impl AgentDetailFacts {
    /// Build a detail bundle from the ONE source id the row was read back with and
    /// the ONE decoded `creator`. The constructor is the only place viewer arms are
    /// built: it seeds the creator arm from `creator` and unions the other two arms,
    /// so the human the ACL admits as creator and the human the wire renders are the
    /// same value by construction. The visibility and projection both describe the
    /// `source` row; naming the source explicitly is what lets the route refuse a
    /// bundle resolved for another agent.
    pub fn from_source(
        source: AgentId,
        creator: UserId,
        direct_pair_human_endpoints: impl IntoIterator<Item = UserId>,
        shared_workspace_members: impl IntoIterator<Item = UserId>,
        materialization_status: MaterializationStatus,
        projection: AgentDetailProjection,
    ) -> Self {
        let visibility = AgentVisibility {
            viewers: AgentViewerArms::new(
                Some(creator.clone()),
                direct_pair_human_endpoints,
                shared_workspace_members,
            ),
            materialization_status,
        };
        Self {
            source,
            creator,
            visibility,
            projection,
        }
    }

    /// The decoded `agents.id` this bundle describes — the coordinate the route
    /// binds to the requested id before it consults the ACL or renders the wire.
    pub fn source(&self) -> &AgentId {
        &self.source
    }

    /// The ONE decoded creator: both the ACL creator arm (via the viewer arms this
    /// bundle was built with) and the wire's `created_by_user_id`. There is no
    /// second copy to drift from it.
    pub fn created_by_user_id(&self) -> &UserId {
        &self.creator
    }

    /// The visibility ACL and materialization stage the refusal is decided against.
    pub fn visibility(&self) -> &AgentVisibility {
        &self.visibility
    }

    /// The wire-facing fields the gate does not already carry.
    pub fn projection(&self) -> &AgentDetailProjection {
        &self.projection
    }
}

/// Loads the stored facts for one agent-detail read — the agent row, its host
/// `LEFT JOIN`, and its resolved visibility ACL, on one snapshot.
pub trait AgentDetailReadModel {
    /// `None` when the row is absent — a fact the ACL renders as "not a viewer of a
    /// row that is not there", never a failed read.
    fn read_agent_detail(
        &self,
        agent: &AgentId,
    ) -> impl Future<Output = Result<Option<AgentDetailFacts>, PersistenceFailure>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        foundation::identity::Principal,
        identity_access::capability::agent_view::{VisibleStages, can_view_agent},
    };

    fn user(id: &str) -> UserId {
        UserId::parse(id).expect("user id")
    }

    fn agent(id: &str) -> AgentId {
        AgentId::parse(id).expect("agent id")
    }

    fn projection() -> AgentDetailProjection {
        AgentDetailProjection {
            name: "agent-one".to_string(),
            display_name: "Agent One".to_string(),
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
            created_at: 1,
            updated_at: 2,
        }
    }

    fn admits(facts: &AgentDetailFacts, who: &str) -> bool {
        can_view_agent(
            &Principal::human(user(who)),
            facts.source(),
            Some(facts.visibility()),
            &VisibleStages::active_only(),
        )
        .is_allowed()
    }

    /// The creator the ACL admits and the creator the wire renders are ONE value:
    /// there is a single `creator` input, so the human who gets in via the creator
    /// arm is exactly the human `created_by_user_id` reports. No construction can
    /// make the two disagree — the projection carries no second creator to set.
    #[test]
    fn the_creator_arm_and_the_wire_creator_are_one_value() {
        let facts = AgentDetailFacts::from_source(
            agent("a1"),
            user("creator-a"),
            [],
            [],
            MaterializationStatus::Active,
            projection(),
        );
        // The wire echoes the creator input …
        assert_eq!(facts.created_by_user_id().as_str(), "creator-a");
        // … and that same id is what the ACL creator arm admits.
        assert!(admits(&facts, "creator-a"));
        assert!(!admits(&facts, "stranger"));
    }

    /// Admitting a caller through a DIFFERENT arm never moves the wire creator: it
    /// stays the row's creator, not whoever happened to be admitted. This is what a
    /// splice would violate — admit one human, render another — and it cannot be
    /// expressed.
    #[test]
    fn the_wire_creator_is_the_row_creator_not_the_admitted_viewer() {
        let facts = AgentDetailFacts::from_source(
            agent("a1"),
            user("creator-a"),
            [],
            [user("member-b")],
            MaterializationStatus::Active,
            projection(),
        );
        // A workspace member is admitted …
        assert!(admits(&facts, "member-b"));
        // … but the wire still renders the creator, not the viewer.
        assert_eq!(facts.created_by_user_id().as_str(), "creator-a");
    }
}
