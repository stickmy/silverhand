//! The stored facts one `GET /api/agents/{id}/runtime-state` read needs, and the
//! pure projection that redacts them for a viewer.
//!
//! This is the READ dual of the runtime-switch write saga: it mints no write
//! authority, holds no lifecycle state, and answers the route from ONE snapshot.
//! It composes the SAME visibility ACL as availability/detail
//! ([`crate::identity_access::capability::agent_view`]) with a projection of the agent's
//! participations and durable seats, so the route and its tests share one
//! authority without a driver.
//!
//! ## One snapshot, source-bound
//!
//! Everything the gate is judged against AND everything later projected comes off
//! ONE deferred snapshot: the agent's visibility arms + stage, its complete
//! participation set, and its durable seats with per-scope queue depth. Reading
//! the visibility authority and the payload together is what closes the
//! authority↔payload TOCTOU — an agent archived or reparented between two reads
//! can never let one read admit a caller the other read's payload should have
//! hidden. [`AgentRuntimeStateFacts::from_source`] seals the bundle to the ONE
//! decoded `agents.id` the adapter read back, and the route binds that to the id
//! it was asked for before it consults the ACL.
//!
//! ## Redaction rides the same snapshot
//!
//! Which conversations a viewer may see is the strict human-participation
//! authority, resolved per participation row IN the snapshot as
//! [`RuntimeParticipationFact::viewer_can_read`]. A row the viewer cannot read is
//! DROPPED whole by [`project`](AgentRuntimeStateFacts::project); a conversation
//! seat the viewer cannot read is removed entirely, because its mere existence
//! would leak that the agent runs there. The complete and visible participation
//! lists are the SAME filtered set split on one snapshot flag
//! ([`RuntimeParticipationFact::archived`]), so an absent seat can never desync
//! them.
//!
//! ## Focus is ephemeral, generation-guarded, applied last
//!
//! Presentation focus is an in-memory fact of a live process, so it is read AFTER
//! the snapshot and is deliberately ABSENT from the bundle. The caller resolves it
//! through the runtime custody/focus ports and hands it to [`project`] as a
//! [`PresentationFocus`]. The guard is [`agent_scope_generation`](AgentRuntimeStateFacts::agent_scope_generation):
//! a live focus is projected onto the agent-scope seat ONLY when the live
//! principal's generation matches the generation of the agent-scope seat in this
//! snapshot. A re-claim that raced the read advances the generation, so its focus
//! is ignored rather than spliced onto the stale seat.

use std::future::Future;

use crate::{
    conversation_messaging::messaging::collaboration::conversation_ref::ConversationRosterPolicy,
    foundation::identity::{AgentId, ConversationId, UserId},
    foundation::persistence::PersistenceFailure,
    host_runtime::runtime_credential::RuntimeGeneration,
    host_runtime::runtime_event::RuntimeObservedStatus,
    identity_access::agent_lifecycle::MaterializationStatus,
    identity_access::capability::agent_view::{AgentViewerArms, AgentVisibility},
};

/// One conversation the agent is a party to, plus the two snapshot-time facts the
/// projection needs but the wire never carries: whether the conversation is
/// archived (the complete-vs-visible split) and whether THIS viewer may read it
/// (redaction). Both are resolved in the SAME snapshot as the row, so neither can
/// drift from the participation set they qualify.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeParticipationFact {
    pub conversation: ConversationId,
    /// The workspace the conversation is placed in, echoed to the wire verbatim.
    /// `None` for a `direct_pair`. Not decoded to a [`crate::foundation::identity::WorkspaceId`]:
    /// it is a wire echo that drives no decision here, like the detail face's
    /// `host_id`.
    pub workspace_id: Option<String>,
    pub workspace_name: Option<String>,
    /// The `conversations.placement_kind` token (`workspace` | `direct_pair`),
    /// echoed verbatim. It is a CHECK-constrained closed column that drives no
    /// decision in this projection, so it passes through as the stored string
    /// rather than a decoded vocabulary — the same treatment the materialization
    /// projection gives it.
    pub placement_kind: String,
    pub roster_policy: ConversationRosterPolicy,
    pub channel_title: String,
    /// Whether the conversation is archived — the ONLY difference between the
    /// complete and the visible participation lists.
    pub archived: bool,
    /// Whether THIS viewer may read the conversation under the strict human
    /// participation authority. A `false` row is dropped from BOTH lists.
    pub viewer_can_read: bool,
}

/// The scope of one durable runtime seat. A conversation seat carries whether THIS
/// viewer may read its conversation, because a seat the viewer cannot read is
/// removed from the projection whole.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeSeatScopeFact {
    /// One semantic agent-wide runtime, serving any number of conversations. It is
    /// the seat live presentation focus attaches to.
    Agent,
    Conversation {
        conversation: ConversationId,
        viewer_can_read: bool,
    },
}

/// One durable runtime seat as stored, with its per-scope undelivered queue depth
/// resolved on the SAME snapshot. Focus is deliberately ABSENT: it is an ephemeral
/// in-memory fact read after the snapshot, generation-guarded, and applied at
/// projection time.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeSeatFact {
    pub scope: RuntimeSeatScopeFact,
    pub generation: RuntimeGeneration,
    pub status: RuntimeObservedStatus,
    /// The observed OS pid, a wire echo. `None` when no process is observed. Not a
    /// closed vocabulary or identity, so it passes through as the stored integer.
    pub pid: Option<i64>,
    pub cli_session_id: Option<String>,
    /// Undelivered inbox depth for this seat's scope, counted on the snapshot: the
    /// agent-wide total for an agent seat, the per-conversation count for a
    /// conversation seat.
    pub queue_depth: i64,
}

/// The stored facts one authenticated runtime-state read is decided and projected
/// from, read together on one snapshot.
///
/// Sealed to ONE decoded source id — the `agents.id` the adapter actually read
/// back, never the request echo. The visibility a caller is judged against and the
/// payload a value is projected from both hang off that one coordinate, and
/// [`source`](Self::source) is what the route binds to the id it was asked for
/// BEFORE the ACL decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentRuntimeStateFacts {
    source: AgentId,
    visibility: AgentVisibility,
    participations: Vec<RuntimeParticipationFact>,
    seats: Vec<RuntimeSeatFact>,
}

impl AgentRuntimeStateFacts {
    /// Build a runtime-state bundle from the ONE source id the row was read back
    /// with and the ONE decoded `creator`. The constructor is the only place the
    /// viewer arms are built: it seeds the creator arm and unions the direct-pair
    /// and workspace arms, exactly as the detail face does, so this face cannot
    /// drift from availability/detail on who may see an agent. `creator` is not
    /// echoed on this wire (runtime-state carries no `created_by_user_id`); it
    /// exists only to seed the ACL creator arm.
    pub fn from_source(
        source: AgentId,
        creator: UserId,
        direct_pair_human_endpoints: impl IntoIterator<Item = UserId>,
        shared_workspace_members: impl IntoIterator<Item = UserId>,
        materialization_status: MaterializationStatus,
        participations: Vec<RuntimeParticipationFact>,
        seats: Vec<RuntimeSeatFact>,
    ) -> Self {
        let visibility = AgentVisibility {
            viewers: AgentViewerArms::new(
                Some(creator),
                direct_pair_human_endpoints,
                shared_workspace_members,
            ),
            materialization_status,
        };
        Self {
            source,
            visibility,
            participations,
            seats,
        }
    }

    /// The decoded `agents.id` this bundle describes — the coordinate the route
    /// binds to the requested id before it consults the ACL.
    pub fn source(&self) -> &AgentId {
        &self.source
    }

    /// The visibility ACL and materialization stage the refusal is decided against.
    pub fn visibility(&self) -> &AgentVisibility {
        &self.visibility
    }

    /// The durable generation of the agent-scope seat in THIS snapshot, if one
    /// exists. The exact-generation focus guard compares the live principal's
    /// generation against this value: a live focus projects onto the agent-scope
    /// seat only on a match, so a re-claim between the snapshot and the live read
    /// can never splice a newer generation's focus onto this snapshot's seat.
    /// `None` when the agent has no agent-scope seat, in which case there is
    /// nothing for a live focus to attach to.
    pub fn agent_scope_generation(&self) -> Option<RuntimeGeneration> {
        self.seats.iter().find_map(|seat| match seat.scope {
            RuntimeSeatScopeFact::Agent => Some(seat.generation),
            RuntimeSeatScopeFact::Conversation { .. } => None,
        })
    }

    /// Project the snapshot facts to the wire read model, applying redaction and
    /// the resolved presentation focus. Pure: every input is a fact already read,
    /// so the same bundle and focus always yield the same view.
    ///
    /// * `participations` — every conversation the viewer may read.
    /// * `visible_participations` — the same set minus archived conversations.
    /// * `seats` — the durable seats, with each unreadable conversation seat
    ///   removed whole and the agent seat carrying the generation-guarded focus.
    pub fn project(&self, focus: PresentationFocus) -> AgentRuntimeStateView {
        let participations = self
            .participations
            .iter()
            .filter(|fact| fact.viewer_can_read)
            .map(RuntimeParticipationView::from_fact)
            .collect();
        // The visible list is the complete list minus archived — the SAME
        // readability filter, one extra predicate. Deriving both from one fact set
        // is why an absent seat cannot make membership and activity disagree.
        let visible_participations = self
            .participations
            .iter()
            .filter(|fact| fact.viewer_can_read && !fact.archived)
            .map(RuntimeParticipationView::from_fact)
            .collect();
        let seats = self
            .seats
            .iter()
            .filter_map(|seat| self.project_seat(seat, &focus))
            .collect();
        AgentRuntimeStateView {
            participations,
            visible_participations,
            seats,
        }
    }

    fn project_seat(
        &self,
        seat: &RuntimeSeatFact,
        focus: &PresentationFocus,
    ) -> Option<RuntimeSeatView> {
        let (scope, seat_focus) = match &seat.scope {
            RuntimeSeatScopeFact::Agent => {
                // The agent-scope seat is the only seat live focus attaches to; the
                // caller has already applied the exact-generation guard, so an
                // ignored (raced or absent) focus arrives here as `None`.
                let focus_view = match focus {
                    PresentationFocus::None => None,
                    PresentationFocus::Live {
                        conversation,
                        viewer_can_read,
                    } => Some(if *viewer_can_read {
                        RuntimeFocusView::Conversation(conversation.clone())
                    } else {
                        // The focus exists but the viewer may not read it: reveal
                        // that the agent is focused somewhere WITHOUT naming where.
                        RuntimeFocusView::Other
                    }),
                };
                (RuntimeSeatScopeView::Agent, focus_view)
            }
            RuntimeSeatScopeFact::Conversation {
                conversation,
                viewer_can_read,
            } => {
                // A conversation seat the viewer cannot read is removed whole — its
                // existence alone would leak that the agent runs in that
                // conversation. A readable conversation seat is always focused on
                // its own conversation.
                if !viewer_can_read {
                    return None;
                }
                (
                    RuntimeSeatScopeView::Conversation(conversation.clone()),
                    Some(RuntimeFocusView::Conversation(conversation.clone())),
                )
            }
        };
        Some(RuntimeSeatView {
            scope,
            generation: seat.generation,
            status: seat.status,
            pid: seat.pid,
            cli_session_id: seat.cli_session_id.clone(),
            focus: seat_focus,
            queue_depth: seat.queue_depth,
        })
    }
}

/// The generation-guarded live presentation focus for the agent-scope seat,
/// resolved by the caller AFTER the snapshot from the in-memory runtime host.
///
/// `None` covers every case in which no live focus may be shown: there is no
/// agent-scope seat, the seat has no current live process, the live generation no
/// longer matches this snapshot's seat (a re-claim raced the read), or the process
/// reports no focus. `Live` is a focus on the matching generation, with its
/// viewer-readability already resolved so [`AgentRuntimeStateFacts::project`] stays
/// pure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PresentationFocus {
    None,
    Live {
        conversation: ConversationId,
        viewer_can_read: bool,
    },
}

/// One wire-facing participation row after redaction: the readable/archived facts
/// have already done their filtering, so this carries only what the DTO renders.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeParticipationView {
    pub conversation: ConversationId,
    pub workspace_id: Option<String>,
    pub workspace_name: Option<String>,
    pub placement_kind: String,
    pub roster_policy: ConversationRosterPolicy,
    pub channel_title: String,
}

impl RuntimeParticipationView {
    fn from_fact(fact: &RuntimeParticipationFact) -> Self {
        Self {
            conversation: fact.conversation.clone(),
            workspace_id: fact.workspace_id.clone(),
            workspace_name: fact.workspace_name.clone(),
            placement_kind: fact.placement_kind.clone(),
            roster_policy: fact.roster_policy,
            channel_title: fact.channel_title.clone(),
        }
    }
}

/// The wire scope of a projected seat — the redaction-carrying arm of
/// [`RuntimeSeatScopeFact`] collapsed to its identity once the drop decision is made.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeSeatScopeView {
    Agent,
    Conversation(ConversationId),
}

/// A projected seat's presentation focus: a named conversation the viewer may
/// read, or `Other` when a focus exists but its conversation is redacted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeFocusView {
    Conversation(ConversationId),
    Other,
}

/// One durable seat after projection: its scope, its stored runtime facts, its
/// per-scope queue depth, and the focus resolved for it (`None` on an agent seat
/// with no live focus).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeSeatView {
    pub scope: RuntimeSeatScopeView,
    pub generation: RuntimeGeneration,
    pub status: RuntimeObservedStatus,
    pub pid: Option<i64>,
    pub cli_session_id: Option<String>,
    pub focus: Option<RuntimeFocusView>,
    pub queue_depth: i64,
}

/// The two orthogonal read models the wire returns: where the agent participates
/// (complete and visible) and which semantic runtimes exist. Neither is derived
/// from the other, so an absent seat cannot erase membership and an agent seat
/// cannot fan out into one row per conversation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentRuntimeStateView {
    pub participations: Vec<RuntimeParticipationView>,
    pub visible_participations: Vec<RuntimeParticipationView>,
    pub seats: Vec<RuntimeSeatView>,
}

/// Loads the source-bound runtime-state bundle on one snapshot, and answers the
/// one post-snapshot conversation-readability question the live focus needs.
pub trait AgentRuntimeStateReadModel {
    /// The whole source-bound bundle on ONE snapshot: the agent's visibility ACL
    /// arms + stage, its COMPLETE participation set (each row carrying its archived
    /// flag and THIS viewer's readability), and its durable seats with per-scope
    /// queue depth. `None` when the agent row is absent — the ACL's absence arm,
    /// never a failed read. `viewer` is threaded in because the per-conversation
    /// redaction facts are resolved in the same snapshot as the payload; a viewer
    /// who turns out to be unauthorized on the agent simply had the redaction
    /// computed for nothing, which is the price of one consistent snapshot.
    fn read_agent_runtime_state(
        &self,
        agent: &AgentId,
        viewer: &UserId,
    ) -> impl Future<Output = Result<Option<AgentRuntimeStateFacts>, PersistenceFailure>> + Send;

    /// Whether `viewer` may read `conversation` under the SAME strict human
    /// participation authority that redacts the participation set — asked once for
    /// the live focus conversation, whose identity is only known AFTER the
    /// snapshot. A read fault is the caller's safe 500, never a silent `false`.
    fn read_conversation_visibility(
        &self,
        viewer: &UserId,
        conversation: &ConversationId,
    ) -> impl Future<Output = Result<bool, PersistenceFailure>> + Send;
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

    fn conversation(id: &str) -> ConversationId {
        ConversationId::parse(id).expect("conversation id")
    }

    fn generation(value: i64) -> RuntimeGeneration {
        RuntimeGeneration::new(value).expect("generation")
    }

    /// A participation fact for `id`, readable/archived as told.
    fn participation(id: &str, archived: bool, viewer_can_read: bool) -> RuntimeParticipationFact {
        RuntimeParticipationFact {
            conversation: conversation(id),
            workspace_id: Some("w1".to_string()),
            workspace_name: Some("Workspace One".to_string()),
            placement_kind: "workspace".to_string(),
            roster_policy: ConversationRosterPolicy::Open,
            channel_title: format!("channel {id}"),
            archived,
            viewer_can_read,
        }
    }

    fn agent_seat(seat_generation: i64) -> RuntimeSeatFact {
        RuntimeSeatFact {
            scope: RuntimeSeatScopeFact::Agent,
            generation: generation(seat_generation),
            status: RuntimeObservedStatus::Active,
            pid: Some(4242),
            cli_session_id: Some("cli-1".to_string()),
            queue_depth: 3,
        }
    }

    fn conversation_seat(id: &str, viewer_can_read: bool) -> RuntimeSeatFact {
        RuntimeSeatFact {
            scope: RuntimeSeatScopeFact::Conversation {
                conversation: conversation(id),
                viewer_can_read,
            },
            generation: generation(1),
            status: RuntimeObservedStatus::Idle,
            pid: None,
            cli_session_id: None,
            queue_depth: 1,
        }
    }

    fn facts(
        participations: Vec<RuntimeParticipationFact>,
        seats: Vec<RuntimeSeatFact>,
    ) -> AgentRuntimeStateFacts {
        AgentRuntimeStateFacts::from_source(
            agent("a1"),
            user("creator"),
            [],
            [],
            MaterializationStatus::Active,
            participations,
            seats,
        )
    }

    /// The bundle seeds the SAME visibility ACL as availability/detail: the creator
    /// arm admits the creator, and a stranger is refused with the anti-enumeration
    /// arm — this face cannot drift from the shared authority.
    #[test]
    fn the_creator_arm_is_seeded_like_the_shared_visibility_authority() {
        let facts = facts(vec![], vec![]);
        let allowed = can_view_agent(
            &Principal::human(user("creator")),
            facts.source(),
            Some(facts.visibility()),
            &VisibleStages::active_only(),
        );
        assert!(allowed.is_allowed());
        let refused = can_view_agent(
            &Principal::human(user("stranger")),
            facts.source(),
            Some(facts.visibility()),
            &VisibleStages::active_only(),
        );
        assert!(!refused.is_allowed());
    }

    /// A conversation the viewer cannot read is dropped from BOTH participation
    /// lists — redaction is not a wire annotation, the row is gone.
    #[test]
    fn an_unreadable_conversation_is_dropped_from_both_lists() {
        let view = facts(
            vec![
                participation("readable", false, true),
                participation("hidden", false, false),
            ],
            vec![],
        )
        .project(PresentationFocus::None);
        let ids: Vec<&str> = view
            .participations
            .iter()
            .map(|row| row.conversation.as_str())
            .collect();
        assert_eq!(ids, ["readable"]);
        let visible_ids: Vec<&str> = view
            .visible_participations
            .iter()
            .map(|row| row.conversation.as_str())
            .collect();
        assert_eq!(visible_ids, ["readable"]);
    }

    /// The visible list is the complete list minus archived, both drawn from ONE
    /// readable-filtered set: an archived readable conversation stays in the
    /// complete list and drops only from the visible one.
    #[test]
    fn the_visible_list_is_the_complete_list_minus_archived() {
        let view = facts(
            vec![
                participation("live", false, true),
                participation("archived", true, true),
            ],
            vec![],
        )
        .project(PresentationFocus::None);
        let complete: Vec<&str> = view
            .participations
            .iter()
            .map(|row| row.conversation.as_str())
            .collect();
        assert_eq!(complete, ["live", "archived"]);
        let visible: Vec<&str> = view
            .visible_participations
            .iter()
            .map(|row| row.conversation.as_str())
            .collect();
        assert_eq!(visible, ["live"]);
    }

    /// A conversation seat the viewer cannot read is removed whole; a readable one
    /// survives and is focused on its own conversation.
    #[test]
    fn an_unreadable_conversation_seat_is_removed_whole() {
        let view = facts(
            vec![],
            vec![
                conversation_seat("seen", true),
                conversation_seat("hidden", false),
            ],
        )
        .project(PresentationFocus::None);
        assert_eq!(view.seats.len(), 1);
        assert_eq!(
            view.seats[0].scope,
            RuntimeSeatScopeView::Conversation(conversation("seen"))
        );
        assert_eq!(
            view.seats[0].focus,
            Some(RuntimeFocusView::Conversation(conversation("seen")))
        );
    }

    /// The agent seat projects a readable live focus as a named conversation.
    #[test]
    fn a_readable_live_focus_names_the_conversation() {
        let view = facts(vec![], vec![agent_seat(9)]).project(PresentationFocus::Live {
            conversation: conversation("focus"),
            viewer_can_read: true,
        });
        assert_eq!(view.seats.len(), 1);
        assert_eq!(view.seats[0].scope, RuntimeSeatScopeView::Agent);
        assert_eq!(
            view.seats[0].focus,
            Some(RuntimeFocusView::Conversation(conversation("focus")))
        );
    }

    /// A live focus the viewer cannot read collapses to `Other`: the agent is
    /// focused somewhere, but the wire never names where.
    #[test]
    fn an_unreadable_live_focus_collapses_to_other() {
        let view = facts(vec![], vec![agent_seat(9)]).project(PresentationFocus::Live {
            conversation: conversation("secret"),
            viewer_can_read: false,
        });
        assert_eq!(view.seats[0].focus, Some(RuntimeFocusView::Other));
    }

    /// With no live focus (absent, raced away, or generation-mismatched — all
    /// arriving as `None`), the agent seat's focus is null.
    #[test]
    fn an_agent_seat_without_live_focus_projects_null() {
        let view = facts(vec![], vec![agent_seat(9)]).project(PresentationFocus::None);
        assert_eq!(view.seats[0].focus, None);
    }

    /// The agent-scope generation is the guard input: it reads off the agent seat,
    /// and is `None` when the agent has only conversation seats (nothing for a live
    /// focus to attach to).
    #[test]
    fn the_agent_scope_generation_reads_off_the_agent_seat() {
        assert_eq!(
            facts(vec![], vec![conversation_seat("c1", true), agent_seat(7)])
                .agent_scope_generation(),
            Some(generation(7))
        );
        assert_eq!(
            facts(vec![], vec![conversation_seat("c1", true)]).agent_scope_generation(),
            None
        );
    }
}
