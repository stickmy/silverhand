//! The reaction write UoW and its placement-aware authorization.

use silverhand_backend_core::{
    conversation_messaging::messaging::PrincipalIdentity,
    conversation_messaging::messaging::collaboration::reaction::{
        CommittedReaction, ReactionActor, ReactionChangedFact, ReactionMutation,
        ReactionMutationPort, ReactionMutationRefusal,
    },
};
use sqlx::{Sqlite, Transaction};

use super::{
    ConversationFacts, ReactionStoreError, SqliteReactionStore, conversation_facts,
    endpoint_exists, message_facts,
};

impl ReactionMutationPort for SqliteReactionStore {
    type Error = ReactionStoreError;

    async fn mutate(
        &self,
        mutation: ReactionMutation,
    ) -> Result<Result<CommittedReaction, ReactionMutationRefusal>, Self::Error> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let Some(message) = message_facts(&mut tx, mutation.message()).await? else {
            let refusal = if matches!(mutation.actor(), ReactionActor::Agent { .. }) {
                ReactionMutationRefusal::TargetHidden
            } else {
                ReactionMutationRefusal::MessageNotFound
            };
            return rollback_refusal(tx, refusal).await;
        };
        if mutation
            .actor()
            .source()
            .is_some_and(|source| source != &message.conversation)
        {
            return rollback_refusal(tx, ReactionMutationRefusal::TargetHidden).await;
        }

        let Some(conversation) = conversation_facts(&mut tx, &message.conversation).await? else {
            let refusal = if matches!(mutation.actor(), ReactionActor::Agent { .. }) {
                ReactionMutationRefusal::TargetHidden
            } else {
                ReactionMutationRefusal::ConversationNotFound
            };
            return rollback_refusal(tx, refusal).await;
        };
        match reaction_access(
            &mut tx,
            mutation.actor(),
            &message.conversation,
            &conversation,
        )
        .await?
        {
            ReactionAccess::Allowed => {}
            ReactionAccess::Forbidden => {
                let refusal = if matches!(mutation.actor(), ReactionActor::Agent { .. }) {
                    ReactionMutationRefusal::TargetHidden
                } else {
                    ReactionMutationRefusal::Forbidden
                };
                return rollback_refusal(tx, refusal).await;
            }
            ReactionAccess::CapabilityDenied => {
                return rollback_refusal(tx, ReactionMutationRefusal::CapabilityDenied).await;
            }
        }
        let emoji = match mutation.validate_emoji() {
            Ok(emoji) => emoji,
            Err(error) => {
                return rollback_refusal(tx, ReactionMutationRefusal::InvalidReaction(error)).await;
            }
        };
        if message.from_kind == "system" {
            return rollback_refusal(tx, ReactionMutationRefusal::SystemMessageNotReactable).await;
        }
        if !matches!(message.from_kind.as_str(), "user" | "agent") {
            return Err(ReactionStoreError::InvalidMessageAuthor);
        }
        if conversation.closed {
            return rollback_refusal(tx, ReactionMutationRefusal::ConversationClosed).await;
        }
        if conversation.archived {
            return rollback_refusal(tx, ReactionMutationRefusal::ConversationArchived).await;
        }

        let reactor = mutation.actor().reactor();
        let changed = match mutation.change() {
            silverhand_backend_core::conversation_messaging::messaging::collaboration::reaction::ReactionChange::Add => {
                sqlx::query(
                    "INSERT INTO message_reactions \
                     (id, message_id, reactor_kind, reactor_id, emoji, created_at) \
                     VALUES (lower(hex(randomblob(16))), ?, ?, ?, ?, ?) \
                     ON CONFLICT (message_id, reactor_kind, reactor_id, emoji) DO NOTHING",
                )
                .bind(mutation.message().as_str())
                .bind(reactor.kind())
                .bind(reactor.id())
                .bind(emoji.as_str())
                .bind(mutation.occurred_at_ms())
                .execute(&mut *tx)
                .await?
                .rows_affected()
                    == 1
            }
            silverhand_backend_core::conversation_messaging::messaging::collaboration::reaction::ReactionChange::Remove => {
                sqlx::query(
                    "DELETE FROM message_reactions WHERE message_id = ? \
                     AND reactor_kind = ? AND reactor_id = ? AND emoji = ?",
                )
                .bind(mutation.message().as_str())
                .bind(reactor.kind())
                .bind(reactor.id())
                .bind(emoji.as_str())
                .execute(&mut *tx)
                .await?
                .rows_affected()
                    == 1
            }
        };
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM message_reactions WHERE message_id = ? AND emoji = ?",
        )
        .bind(mutation.message().as_str())
        .bind(emoji.as_str())
        .fetch_one(&mut *tx)
        .await?;
        let count =
            silverhand_backend_core::conversation_messaging::messaging::collaboration::reaction::ReactionCount::new(count)
                .map_err(|_| ReactionStoreError::InvalidCount)?;
        let committed = if changed {
            CommittedReaction::committed_change(ReactionChangedFact::committed(
                mutation.message().clone(),
                message.conversation,
                message.thread_root,
                emoji,
                mutation.change(),
                reactor,
                count,
            ))
        } else {
            CommittedReaction::unchanged(emoji, count)
        };
        tx.commit().await?;
        Ok(Ok(committed))
    }
}

async fn rollback_refusal<T>(
    tx: Transaction<'_, Sqlite>,
    refusal: T,
) -> Result<Result<CommittedReaction, T>, ReactionStoreError> {
    tx.rollback().await?;
    Ok(Err(refusal))
}

enum ReactionAccess {
    Allowed,
    Forbidden,
    CapabilityDenied,
}

async fn reaction_access(
    tx: &mut Transaction<'_, Sqlite>,
    actor: &ReactionActor,
    conversation: &silverhand_backend_core::foundation::identity::ConversationId,
    facts: &ConversationFacts,
) -> Result<ReactionAccess, ReactionStoreError> {
    match facts.placement.as_str() {
        "workspace" => {
            let workspace = facts
                .workspace
                .as_deref()
                .filter(|id| !id.is_empty())
                .ok_or(ReactionStoreError::InvalidConversationPlacement)?;
            match actor {
                ReactionActor::Human(user) => {
                    let allowed: i64 = sqlx::query_scalar(
                        "SELECT EXISTS(SELECT 1 FROM workspace_users WHERE workspace_id = ? AND user_id = ?) \
                         AND EXISTS(SELECT 1 FROM conversation_users WHERE conversation_id = ? AND user_id = ?)",
                    ).bind(workspace).bind(user.as_str()).bind(conversation.as_str()).bind(user.as_str())
                    .fetch_one(&mut **tx).await?;
                    Ok(if allowed != 0 {
                        ReactionAccess::Allowed
                    } else {
                        ReactionAccess::Forbidden
                    })
                }
                ReactionActor::Agent { id, .. } => {
                    let participant: i64 = sqlx::query_scalar(
                        "SELECT EXISTS(SELECT 1 FROM agents WHERE id = ?) \
                         AND EXISTS(SELECT 1 FROM conversation_agents WHERE conversation_id = ? AND agent_id = ?)",
                    ).bind(id.as_str()).bind(conversation.as_str()).bind(id.as_str())
                    .fetch_one(&mut **tx).await?;
                    if participant == 0 {
                        return Ok(ReactionAccess::Forbidden);
                    }
                    let capability = crate::foundation::capability::agent_has_workspace_capability(
                        &mut **tx,
                        workspace,
                        id.as_str(),
                        "send_messages",
                    )
                    .await?;
                    Ok(if capability {
                        ReactionAccess::Allowed
                    } else {
                        ReactionAccess::CapabilityDenied
                    })
                }
            }
        }
        "direct_pair" => {
            let endpoints = [facts.endpoint_a.as_ref(), facts.endpoint_b.as_ref()];
            if endpoints.iter().any(|endpoint| endpoint.is_none()) {
                return Err(ReactionStoreError::InvalidConversationPlacement);
            }
            let participates =
                endpoints
                    .iter()
                    .flatten()
                    .any(|endpoint| match (&endpoint.principal, actor) {
                        (PrincipalIdentity::Human(endpoint), ReactionActor::Human(actor)) => {
                            endpoint == actor
                        }
                        (PrincipalIdentity::Agent(endpoint), ReactionActor::Agent { id, .. }) => {
                            endpoint == id
                        }
                        _ => false,
                    });
            if !participates {
                return Ok(ReactionAccess::Forbidden);
            }
            for endpoint in endpoints.into_iter().flatten() {
                if endpoint.deleted_at.is_some()
                    || !endpoint_exists(tx, &endpoint.principal).await?
                {
                    return Ok(ReactionAccess::Forbidden);
                }
            }
            Ok(ReactionAccess::Allowed)
        }
        _ => Err(ReactionStoreError::InvalidConversationPlacement),
    }
}
