//! The write gate: what a send is authorized against, and the refusals it answers
//! with.
//!
//! Every fact here is read INSIDE the atomic unit, so a concurrent change cannot
//! slip between a check and the write. The order the gate asks in is load-bearing,
//! and it is stated at the rule itself.

use std::future::Future;

use crate::foundation::persistence::PersistenceFailure;

use super::{AgentId, ConversationId, PrincipalIdentity, SendActor, SendMessageError, UserId};

/// One side of a direct pair. `deleted_at_ms` is the pair's own record of a
/// departure, kept because a principal row can be gone without the pair knowing
/// when.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversationEndpoint {
    pub principal: PrincipalIdentity,
    pub deleted_at_ms: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectPairEndpoints {
    pub a: ConversationEndpoint,
    pub b: ConversationEndpoint,
}

impl DirectPairEndpoints {
    fn both(&self) -> [&ConversationEndpoint; 2] {
        [&self.a, &self.b]
    }
}

/// Lifecycle and placement facts a write gate needs about its target, read inside
/// the atomic unit so a concurrent change cannot slip between check and write.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversationFacts {
    pub id: ConversationId,
    pub is_archived: bool,
    /// Present only for a direct pair. The endpoints themselves, not a verdict
    /// about them: a verdict from the store would be a second authority, free to
    /// drift from the one that answers the same question elsewhere.
    pub direct_pair: Option<DirectPairEndpoints>,
}

/// Why a write was refused, as the business reason and nothing else. How a reason
/// is reported — status, code, wording, what an operator should do next — belongs
/// to whichever boundary is speaking.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ConversationRefusal {
    #[error("conversation not found")]
    NotFound,
    #[error("conversation is archived")]
    Archived,
    #[error("the other endpoint of this direct conversation no longer exists")]
    EndpointGone,
    #[error("Conversation-scoped agents do not support DMs. Use a shared channel.")]
    DirectMessageScope,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ActorRefusal {
    #[error("agent not found")]
    AgentNotFound,
    #[error("agent is not a participant of this conversation")]
    AgentNotParticipant,
    #[error("user is not a participant of this conversation")]
    UserNotParticipant,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("target message not found in target conversation")]
pub struct ReplyTargetRefusal;

/// The named root is not a reply cluster of the target conversation. Unknown, from
/// another conversation, and "a reply, so not a root" are one answer: a sender
/// naming a cluster it cannot use learns nothing about what exists.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("no reply cluster of this conversation is named by that root")]
pub struct ThreadRootRefusal;

/// Endpoint existence and DM eligibility, read inside the write transaction.
/// Participation and historical visibility remain separate from write eligibility.
pub trait EndpointPrincipalReads {
    /// Whether this live endpoint may participate in a writable direct pair.
    fn endpoint_allows_direct_messages(
        &mut self,
        endpoint: &ConversationEndpoint,
    ) -> impl Future<Output = Result<bool, PersistenceFailure>> + Send;

    fn endpoint_principal_exists(
        &mut self,
        endpoint: &ConversationEndpoint,
    ) -> impl Future<Output = Result<bool, PersistenceFailure>> + Send;
}

/// Facts the write gate reads inside the atomic unit.
///
/// Reading participation outside the unit would let a concurrent revocation slip
/// between the check and the insert, so these are asked of the transaction's own
/// scope rather than passed in.
///
/// This capability is Messaging's own and stays narrow to the send gate's real
/// rule. The enrollment access port answers a different question — who may enrol
/// an agent — and must not be widened to carry participation facts.
pub trait PostAuthorityReads: EndpointPrincipalReads {
    fn conversation_facts(
        &mut self,
        conversation: &ConversationId,
    ) -> impl Future<Output = Result<Option<ConversationFacts>, PersistenceFailure>> + Send;

    fn agent_exists(
        &mut self,
        agent: &AgentId,
    ) -> impl Future<Output = Result<bool, PersistenceFailure>> + Send;

    /// Participation is placement-based: a workspace channel names its
    /// participants by roster row, a direct pair by its endpoints.
    fn is_agent_participant(
        &mut self,
        conversation: &ConversationId,
        agent: &AgentId,
    ) -> impl Future<Output = Result<bool, PersistenceFailure>> + Send;

    fn is_user_participant(
        &mut self,
        conversation: &ConversationId,
        user: &UserId,
    ) -> impl Future<Output = Result<bool, PersistenceFailure>> + Send;
}

/// A direct pair whose either side has departed accepts no further writes.
///
/// Asked of both endpoints rather than of "the other one": it is a property of
/// the conversation, so a survivor and a bystander are told the same thing. A
/// departure counts twice over — the pair's own tombstone, and the absence of the
/// principal row — because either can be the surviving record of it.
pub async fn direct_pair_is_read_only_for_survivor(
    endpoints: &DirectPairEndpoints,
    reads: &mut impl EndpointPrincipalReads,
) -> Result<bool, PersistenceFailure> {
    for endpoint in endpoints.both() {
        if endpoint.deleted_at_ms.is_some() || !reads.endpoint_principal_exists(endpoint).await? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The write gate: lifecycle first, then participation, then the direct-pair
/// narrowing. The order is load-bearing — an archived conversation answers with its
/// lifecycle rather than leaking whether the caller is a participant, and the
/// narrowing goes last so only a participant learns that a pair's peer is gone.
pub async fn assert_post_authority(
    reads: &mut impl PostAuthorityReads,
    conversation: &ConversationId,
    actor: &SendActor,
) -> Result<ConversationFacts, SendMessageError> {
    let facts = reads
        .conversation_facts(conversation)
        .await?
        .ok_or(ConversationRefusal::NotFound)?;

    if facts.is_archived {
        return Err(ConversationRefusal::Archived.into());
    }

    match actor {
        SendActor::Agent(agent) => {
            if !reads.agent_exists(agent).await? {
                return Err(ActorRefusal::AgentNotFound.into());
            }
            if !reads.is_agent_participant(conversation, agent).await? {
                return Err(ActorRefusal::AgentNotParticipant.into());
            }
        }
        // No existence check mirroring the agent arm: participation is already
        // false for an id that names nobody, so a second lookup would add a
        // second refusal for a case this one covers.
        SendActor::User(user) => {
            if !reads.is_user_participant(conversation, user).await? {
                return Err(ActorRefusal::UserNotParticipant.into());
            }
        }
    }

    if let Some(endpoints) = &facts.direct_pair
        && direct_pair_is_read_only_for_survivor(endpoints, reads).await?
    {
        return Err(ConversationRefusal::EndpointGone.into());
    }
    if let Some(endpoints) = &facts.direct_pair {
        for endpoint in endpoints.both() {
            if !reads.endpoint_allows_direct_messages(endpoint).await? {
                return Err(ConversationRefusal::DirectMessageScope.into());
            }
        }
    }

    Ok(facts)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn conversation() -> ConversationId {
        ConversationId::parse("conv-1").unwrap()
    }

    #[derive(Default)]
    struct FakeReads {
        facts: Option<ConversationFacts>,
        known_agents: HashSet<String>,
        known_users: HashSet<String>,
        agent_participants: HashSet<String>,
        user_participants: HashSet<String>,
    }

    impl EndpointPrincipalReads for FakeReads {
        async fn endpoint_allows_direct_messages(
            &mut self,
            _endpoint: &ConversationEndpoint,
        ) -> Result<bool, PersistenceFailure> {
            Ok(true)
        }

        async fn endpoint_principal_exists(
            &mut self,
            endpoint: &ConversationEndpoint,
        ) -> Result<bool, PersistenceFailure> {
            Ok(match &endpoint.principal {
                PrincipalIdentity::Agent(agent) => self.known_agents.contains(agent.as_str()),
                PrincipalIdentity::Human(user) => self.known_users.contains(user.as_str()),
            })
        }
    }

    impl PostAuthorityReads for FakeReads {
        async fn conversation_facts(
            &mut self,
            _conversation: &ConversationId,
        ) -> Result<Option<ConversationFacts>, PersistenceFailure> {
            Ok(self.facts.clone())
        }

        async fn agent_exists(&mut self, agent: &AgentId) -> Result<bool, PersistenceFailure> {
            Ok(self.known_agents.contains(agent.as_str()))
        }

        async fn is_agent_participant(
            &mut self,
            _conversation: &ConversationId,
            agent: &AgentId,
        ) -> Result<bool, PersistenceFailure> {
            Ok(self.agent_participants.contains(agent.as_str()))
        }

        async fn is_user_participant(
            &mut self,
            _conversation: &ConversationId,
            user: &UserId,
        ) -> Result<bool, PersistenceFailure> {
            Ok(self.user_participants.contains(user.as_str()))
        }
    }

    fn open_conversation() -> ConversationFacts {
        ConversationFacts {
            id: conversation(),
            is_archived: false,
            direct_pair: None,
        }
    }

    fn live_pair() -> DirectPairEndpoints {
        DirectPairEndpoints {
            a: ConversationEndpoint {
                principal: PrincipalIdentity::Agent(AgentId::parse("agent-1").unwrap()),
                deleted_at_ms: None,
            },
            b: ConversationEndpoint {
                principal: PrincipalIdentity::Human(UserId::parse("user-1").unwrap()),
                deleted_at_ms: None,
            },
        }
    }

    fn agent_reads() -> FakeReads {
        FakeReads {
            facts: Some(open_conversation()),
            known_agents: HashSet::from(["agent-1".to_owned()]),
            known_users: HashSet::from(["user-1".to_owned()]),
            agent_participants: HashSet::from(["agent-1".to_owned()]),
            ..FakeReads::default()
        }
    }

    fn agent() -> SendActor {
        SendActor::Agent(AgentId::parse("agent-1").unwrap())
    }

    #[tokio::test]
    async fn an_archived_conversation_refuses_before_participation() {
        let mut reads = FakeReads {
            facts: Some(ConversationFacts {
                is_archived: true,
                ..open_conversation()
            }),
            ..FakeReads::default()
        };

        let error = assert_post_authority(&mut reads, &conversation(), &agent())
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            SendMessageError::Conversation(ConversationRefusal::Archived)
        ));
    }

    #[tokio::test]
    async fn a_missing_conversation_answers_not_found() {
        let mut reads = FakeReads::default();

        let error = assert_post_authority(&mut reads, &conversation(), &agent())
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            SendMessageError::Conversation(ConversationRefusal::NotFound)
        ));
    }

    #[tokio::test]
    async fn an_unknown_agent_and_a_non_participant_are_distinct_answers() {
        let mut unknown = FakeReads {
            facts: Some(open_conversation()),
            ..FakeReads::default()
        };
        let mut known_but_outside = FakeReads {
            facts: Some(open_conversation()),
            known_agents: HashSet::from(["agent-1".to_owned()]),
            ..FakeReads::default()
        };

        assert!(matches!(
            assert_post_authority(&mut unknown, &conversation(), &agent())
                .await
                .unwrap_err(),
            SendMessageError::Actor(ActorRefusal::AgentNotFound)
        ));
        assert!(matches!(
            assert_post_authority(&mut known_but_outside, &conversation(), &agent())
                .await
                .unwrap_err(),
            SendMessageError::Actor(ActorRefusal::AgentNotParticipant)
        ));
    }

    #[tokio::test]
    async fn a_non_participant_user_is_named_as_a_user() {
        let mut reads = FakeReads {
            facts: Some(open_conversation()),
            ..FakeReads::default()
        };

        let error = assert_post_authority(
            &mut reads,
            &conversation(),
            &SendActor::User(UserId::parse("user-1").unwrap()),
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            SendMessageError::Actor(ActorRefusal::UserNotParticipant)
        ));
    }

    #[tokio::test]
    async fn a_pair_with_a_tombstoned_endpoint_refuses_its_surviving_participant() {
        let mut endpoints = live_pair();
        endpoints.b.deleted_at_ms = Some(1);
        let mut reads = FakeReads {
            facts: Some(ConversationFacts {
                direct_pair: Some(endpoints),
                ..open_conversation()
            }),
            ..agent_reads()
        };

        let error = assert_post_authority(&mut reads, &conversation(), &agent())
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            SendMessageError::Conversation(ConversationRefusal::EndpointGone)
        ));
    }

    /// A pair can outlive its peer's row without ever being stamped, so absence
    /// of the principal is a departure in its own right.
    #[tokio::test]
    async fn a_pair_whose_peer_row_is_gone_refuses_without_a_tombstone() {
        let mut reads = FakeReads {
            facts: Some(ConversationFacts {
                direct_pair: Some(live_pair()),
                ..open_conversation()
            }),
            known_users: HashSet::new(),
            ..agent_reads()
        };

        let error = assert_post_authority(&mut reads, &conversation(), &agent())
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            SendMessageError::Conversation(ConversationRefusal::EndpointGone)
        ));
    }

    #[tokio::test]
    async fn a_live_pair_accepts_its_participant() {
        let mut reads = FakeReads {
            facts: Some(ConversationFacts {
                direct_pair: Some(live_pair()),
                ..open_conversation()
            }),
            ..agent_reads()
        };

        let facts = assert_post_authority(&mut reads, &conversation(), &agent())
            .await
            .unwrap();

        assert_eq!(facts.id, conversation());
    }

    #[tokio::test]
    async fn an_open_conversation_with_a_participating_actor_passes() {
        let mut reads = agent_reads();

        let facts = assert_post_authority(&mut reads, &conversation(), &agent())
            .await
            .unwrap();

        assert_eq!(facts.id, conversation());
    }
}
