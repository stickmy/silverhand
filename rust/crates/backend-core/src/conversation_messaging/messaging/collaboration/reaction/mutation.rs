use std::{error::Error, future::Future};

use crate::{
    conversation_messaging::messaging::MessageId,
    foundation::identity::{AgentId, ConversationId, UserId},
};

use super::{InvalidReactionEmoji, ReactionCount, ReactionEmoji};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Reactor {
    Human(UserId),
    Agent(AgentId),
}

impl Reactor {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Human(_) => "user",
            Self::Agent(_) => "agent",
        }
    }

    pub fn id(&self) -> &str {
        match self {
            Self::Human(id) => id.as_str(),
            Self::Agent(id) => id.as_str(),
        }
    }
}

/// The caller identity plus the only ambient coordinate an anchored runtime may use.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReactionActor {
    Human(UserId),
    Agent {
        id: AgentId,
        source: Option<ConversationId>,
    },
}

impl ReactionActor {
    pub fn reactor(&self) -> Reactor {
        match self {
            Self::Human(id) => Reactor::Human(id.clone()),
            Self::Agent { id, .. } => Reactor::Agent(id.clone()),
        }
    }

    pub fn source(&self) -> Option<&ConversationId> {
        match self {
            Self::Human(_) | Self::Agent { source: None, .. } => None,
            Self::Agent {
                source: Some(source),
                ..
            } => Some(source),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReactionChange {
    Add,
    Remove,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReactionMutation {
    actor: ReactionActor,
    message: MessageId,
    emoji: ReactionEmojiCandidate,
    change: ReactionChange,
    occurred_at_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ReactionEmojiCandidate {
    Validated(ReactionEmoji),
    Invalid(InvalidReactionEmoji),
}

impl ReactionMutation {
    pub fn human(
        actor: UserId,
        message: MessageId,
        emoji: ReactionEmoji,
        change: ReactionChange,
        occurred_at_ms: i64,
    ) -> Self {
        Self {
            actor: ReactionActor::Human(actor),
            message,
            emoji: ReactionEmojiCandidate::Validated(emoji),
            change,
            occurred_at_ms,
        }
    }

    /// Agent emoji semantics are intentionally deferred until the mutation port
    /// has resolved target/source/participation/capability in the published order.
    pub fn agent(
        actor: AgentId,
        source: Option<ConversationId>,
        message: MessageId,
        emoji: Result<ReactionEmoji, InvalidReactionEmoji>,
        change: ReactionChange,
        occurred_at_ms: i64,
    ) -> Self {
        Self {
            actor: ReactionActor::Agent { id: actor, source },
            message,
            emoji: match emoji {
                Ok(emoji) => ReactionEmojiCandidate::Validated(emoji),
                Err(error) => ReactionEmojiCandidate::Invalid(error),
            },
            change,
            occurred_at_ms,
        }
    }

    pub fn actor(&self) -> &ReactionActor {
        &self.actor
    }
    pub fn message(&self) -> &MessageId {
        &self.message
    }
    pub fn validate_emoji(&self) -> Result<ReactionEmoji, InvalidReactionEmoji> {
        match &self.emoji {
            ReactionEmojiCandidate::Validated(emoji) => Ok(emoji.clone()),
            ReactionEmojiCandidate::Invalid(error) => Err(*error),
        }
    }
    pub fn change(&self) -> ReactionChange {
        self.change
    }
    pub fn occurred_at_ms(&self) -> i64 {
        self.occurred_at_ms
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ReactionMutationRefusal {
    #[error("reaction emoji is outside the published grammar")]
    InvalidReaction(InvalidReactionEmoji),
    #[error("reaction target is outside the runtime's visible scope")]
    TargetHidden,
    #[error("message not found")]
    MessageNotFound,
    #[error("system messages cannot receive reactions")]
    SystemMessageNotReactable,
    #[error("conversation not found")]
    ConversationNotFound,
    #[error("caller cannot react in this conversation")]
    Forbidden,
    #[error("runtime lacks the send_messages capability")]
    CapabilityDenied,
    #[error("conversation is archived")]
    ConversationArchived,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReactionChangedFact {
    message: MessageId,
    conversation: ConversationId,
    thread_root: Option<MessageId>,
    emoji: ReactionEmoji,
    change: ReactionChange,
    reactor: Reactor,
    count: ReactionCount,
}

impl ReactionChangedFact {
    #[allow(clippy::too_many_arguments)]
    pub fn committed(
        message: MessageId,
        conversation: ConversationId,
        thread_root: Option<MessageId>,
        emoji: ReactionEmoji,
        change: ReactionChange,
        reactor: Reactor,
        count: ReactionCount,
    ) -> Self {
        Self {
            message,
            conversation,
            thread_root,
            emoji,
            change,
            reactor,
            count,
        }
    }
    pub fn message(&self) -> &MessageId {
        &self.message
    }
    pub fn conversation(&self) -> &ConversationId {
        &self.conversation
    }
    pub fn thread_root(&self) -> Option<&MessageId> {
        self.thread_root.as_ref()
    }
    pub fn emoji(&self) -> &ReactionEmoji {
        &self.emoji
    }
    pub fn change(&self) -> ReactionChange {
        self.change
    }
    pub fn reactor(&self) -> &Reactor {
        &self.reactor
    }
    pub fn count(&self) -> ReactionCount {
        self.count
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommittedReaction {
    Changed(ReactionChangedFact),
    Unchanged {
        emoji: ReactionEmoji,
        count: ReactionCount,
    },
}

impl CommittedReaction {
    pub fn committed_change(fact: ReactionChangedFact) -> Self {
        Self::Changed(fact)
    }

    pub fn unchanged(emoji: ReactionEmoji, count: ReactionCount) -> Self {
        Self::Unchanged { emoji, count }
    }

    pub fn changed(&self) -> bool {
        matches!(self, Self::Changed(_))
    }
    pub fn emoji(&self) -> &ReactionEmoji {
        match self {
            Self::Changed(fact) => fact.emoji(),
            Self::Unchanged { emoji, .. } => emoji,
        }
    }
    pub fn count(&self) -> ReactionCount {
        match self {
            Self::Changed(fact) => fact.count(),
            Self::Unchanged { count, .. } => *count,
        }
    }
    pub fn fact(&self) -> Option<&ReactionChangedFact> {
        match self {
            Self::Changed(fact) => Some(fact),
            Self::Unchanged { .. } => None,
        }
    }
}

pub trait ReactionMutationPort {
    type Error: Error + Send + Sync + 'static;
    fn mutate(
        &self,
        mutation: ReactionMutation,
    ) -> impl Future<
        Output = Result<Result<CommittedReaction, ReactionMutationRefusal>, Self::Error>,
    > + Send;
}

/// Post-commit owner. A publication fault remains typed and source-preserving so
/// the request boundary can audit it exactly once; it cannot turn a committed
/// write into a refusal or request compensation.
pub trait ReactionAftermath {
    fn announce(
        &self,
        fact: &ReactionChangedFact,
    ) -> impl Future<Output = Result<(), ReactionPublicationFailure>> + Send;
}

#[derive(Debug, thiserror::Error)]
#[error("the committed reaction could not be published")]
pub struct ReactionPublicationFailure {
    #[source]
    source: Box<dyn Error + Send + Sync + 'static>,
}

impl ReactionPublicationFailure {
    pub fn from_source(error: impl Error + Send + Sync + 'static) -> Self {
        Self {
            source: Box::new(error),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReactionMutationOutcome {
    changed: bool,
    emoji: ReactionEmoji,
    count: ReactionCount,
}

impl ReactionMutationOutcome {
    pub fn changed(&self) -> bool {
        self.changed
    }
    pub fn emoji(&self) -> &ReactionEmoji {
        &self.emoji
    }
    pub fn count(&self) -> ReactionCount {
        self.count
    }
}

#[derive(Debug)]
pub struct ReactionExecution {
    outcome: ReactionMutationOutcome,
    publication_failure: Option<ReactionPublicationFailure>,
}

impl ReactionExecution {
    pub fn outcome(&self) -> &ReactionMutationOutcome {
        &self.outcome
    }

    pub fn publication_failure(&self) -> Option<&ReactionPublicationFailure> {
        self.publication_failure.as_ref()
    }
}

pub async fn change_reaction<P: ReactionMutationPort>(
    port: &P,
    aftermath: &impl ReactionAftermath,
    mutation: ReactionMutation,
) -> Result<Result<ReactionExecution, ReactionMutationRefusal>, P::Error> {
    let committed = match port.mutate(mutation).await? {
        Ok(committed) => committed,
        Err(refusal) => return Ok(Err(refusal)),
    };
    let publication_failure = if let Some(fact) = committed.fact() {
        aftermath.announce(fact).await.err()
    } else {
        None
    };
    Ok(Ok(ReactionExecution {
        outcome: ReactionMutationOutcome {
            changed: committed.changed(),
            emoji: committed.emoji().clone(),
            count: committed.count(),
        },
        publication_failure,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_oversized_agent_emoji_retains_the_deferred_error_verdict() {
        let raw = "x".repeat(1024 * 1024);
        let verdict = ReactionEmoji::parse(&raw);
        assert!(verdict.is_err());
        let mutation = ReactionMutation::agent(
            AgentId::parse("agent-1").unwrap(),
            Some(ConversationId::parse("conversation-1").unwrap()),
            MessageId::parse("message-1").unwrap(),
            verdict,
            ReactionChange::Add,
            1,
        );
        drop(raw);
        assert_eq!(
            mutation.validate_emoji(),
            Err(InvalidReactionEmoji::TooLong)
        );
    }
}
