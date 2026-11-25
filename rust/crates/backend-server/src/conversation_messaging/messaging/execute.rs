//! Execute: the one place a settled request becomes a committed send.
//!
//! The second of the use-case's two orderable stages. [`super::admission`] decided
//! what the request IS without knowing whose it was; this stage knows whose it is and
//! does nothing else to the payload. It is the only caller of
//! [`SendMessage::prepare`] and the only caller of the write unit, so both doors
//! reach one transaction boundary rather than one each.
//!
//! HUMAN AND AGENT ARE TWO PRINCIPALS, NOT TWO BUSINESSES (@shiki's requirement).
//! The two route adapters differ in when they authorize, how they word a refusal, and
//! which [`SendActor`] variant they build — never in what a send IS. That is why this
//! stage takes the actor as data: it has no arm per principal, so there is no place
//! for the two paths to drift.

use silverhand_backend_core::{
    conversation_messaging::messaging::{
        DirectLineAuthority, FromKind, MessageReference, PrincipalKind, ReferencePositions,
        SendActor, SendDestination, SendMessage, SendMessageDraft, SendMessageError,
        SendMessageOutcome, SendMessageUnitOfWork,
    },
    foundation::identity::{AgentId, ConversationId, UserId},
};

use super::admission::ValidatedSendPayload;
use super::aftermath::SendAftermath;

/// The resolved input a route adapter hands to [`execute`]: who is writing, where
/// from and to, and whether they asked to skip the freshness guard.
///
/// NAMED FOR WHAT IT IS, not for what it would be nice to imply.
/// An earlier version called this "authorized" and its doc claimed that
/// assembling one meant authorization had happened — a promise the type does not keep,
/// because a struct any module can build proves nothing about what ran before it. The
/// fields are private and there is one narrow constructor, so the value cannot be
/// assembled by accident inside this crate; that is the whole of the guarantee.
///
/// What authorization ORDER actually holds is proven where it is observable: by the
/// two doors' cross counter-examples in [`super::adapters`], where a request breaking
/// two rules is answered by whichever gate its own door asks first. A Rust type cannot
/// witness a call that already happened, so this one does not pretend to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ResolvedSendContext {
    actor: SendActor,
    source_conversation: ConversationId,
    target_conversation: ConversationId,
    destination: SendDestination,
    freshness_bypass: bool,
}

impl ResolvedSendContext {
    /// Built by a route adapter once it has resolved where the send goes. Deliberately
    /// takes no evidence of authorization, because it could not check any.
    pub(crate) fn resolved(
        actor: SendActor,
        source_conversation: ConversationId,
        target_conversation: ConversationId,
        destination: SendDestination,
        freshness_bypass: bool,
    ) -> Self {
        Self {
            actor,
            source_conversation,
            target_conversation,
            destination,
            freshness_bypass,
        }
    }
}

/// Turns a resolved, settled request into a committed fact, then invokes exactly one
/// required aftermath before returning the still-successful outcome.
///
/// Derivation of what the body addressed happens inside `prepare`, before any
/// transaction: a 64KiB body must not be parsed while the writer is held, and
/// authorization for a direct line must not be asked from inside it.
pub(crate) async fn execute<U>(
    context: ResolvedSendContext,
    payload: ValidatedSendPayload,
    positions: &impl ReferencePositions,
    authority: &impl DirectLineAuthority,
    unit_of_work: &U,
    aftermath: &impl SendAftermath,
) -> Result<SendMessageOutcome, SendMessageError>
where
    U: SendMessageUnitOfWork,
{
    let draft = SendMessageDraft {
        source_conversation: context.source_conversation,
        target_conversation: context.target_conversation,
        actor: context.actor,
        content: payload.content().clone(),
        destination: context.destination,
        attachment_refs: payload.attachments().to_vec(),
        freshness_bypass: context.freshness_bypass,
    };

    let source_conversation = draft.source_conversation.clone();
    let command = SendMessage::prepare(draft, positions, authority)
        .await?
        .with_client_message_id(payload.client_message_id)
        .with_options(payload.options);
    let outcome = unit_of_work.send_message(command).await?;
    if !outcome.is_replay() {
        aftermath.announce(&outcome).await;
    }
    report_unresolved_references(&source_conversation, &outcome);
    Ok(outcome)
}

/// The author addressed someone this send could not reach.
///
/// AT WARN AND PER SEND, matching the reference: it is an ANOMALY somebody may have to
/// explain after the fact — "I told the agent and nothing happened" — and a running install's
/// default level is `warn`, so an `info` line would not be on disk when that question is
/// asked. Per send rather than per reference, because one send is one event.
///
/// A SUPPLEMENT to the sender's own answer, never a substitute: both success contracts carry
/// the tokens back to whoever typed them, and a log is not feedback. It is also not a second
/// audience for the diagnostic — nothing here reaches another principal or any
/// conversation-wide payload.
///
/// IT ASKS THE OUTCOME, NOT THE WIRE. Both facts it needs — whether anything went unreached,
/// and which tokens those were — come from `SendMessageOutcome` itself. Routing either
/// question through a success contract's projector would invert the dependency: a wire payload
/// is one audience's view of this stage's result, so it may depend on the use-case and never
/// the other way round.
///
/// THREE AUDIENCES, ONE FACT. The two success contracts share their element projector with
/// each other; an operator is the third audience and projects the same ordered
/// `&[MessageReference]` for itself. What is shared is the fact, not anybody's projector.
///
/// PRIVACY-SAFE BY CONSTRUCTION: a reference is a kind and a handle, so this line carries the
/// author's own tokens and can say nothing about who exists — unknown and
/// exists-but-unreachable are indistinguishable in it.
///
/// Reported HERE, at the stage both doors converge on, so neither adapter can add a second
/// report or lose this one.
fn report_unresolved_references(source: &ConversationId, outcome: &SendMessageOutcome) {
    let unresolved = outcome.unresolved_references();
    if unresolved.is_empty() {
        return;
    }
    let message = outcome.committed().message();
    let (actor_kind, actor_id) = match message.from_kind {
        FromKind::Agent => ("agent", message.from_agent_id.as_ref().map(AgentId::as_str)),
        FromKind::User => ("user", message.from_user_id.as_ref().map(UserId::as_str)),
        // Not reachable from either door — neither builds a system actor — and reported
        // rather than assumed away, because a silent `unknown` would be worse than a name.
        FromKind::System => ("system", None),
    };
    tracing::warn!(
        event = "send.references_unresolved",
        source_conversation_id = source.as_str(),
        target_conversation_id = message.conversation_id.as_str(),
        message_id = message.id.as_str(),
        actor_kind,
        actor_id,
        unresolved_json = unresolved_tokens(unresolved).as_str(),
        "references addressed nobody in the target"
    );
}

/// The tokens an operator is shown: `{kind, handle}` per reference, in the body's own
/// first-occurrence order.
///
/// NAMED FOR WHAT THE SINK CAN CARRY. The reference implementation records a structured
/// `unresolved: [{kind, handle}]`; a `tracing` field is a primitive, a string or a `Debug`
/// rendering, so this crate's subscriber has no nested array to record into — reaching one
/// takes the unstable `valuable` path, which needs a workspace-wide build flag this slice may
/// not add. The projection here IS the structured array; what the field name admits is that
/// its value arrives as that array's JSON TEXT. Calling it `unresolved` would have let a
/// reader take a string for the reference's array, which is the one thing a parity claim may
/// not do.
fn unresolved_tokens(references: &[MessageReference]) -> String {
    let tokens: Vec<serde_json::Value> = references
        .iter()
        .map(|reference| {
            let kind = match reference.kind() {
                PrincipalKind::Agent => "agent",
                PrincipalKind::Human => "human",
            };
            serde_json::json!({ "kind": kind, "handle": reference.handle().as_str() })
        })
        .collect();
    serde_json::Value::Array(tokens).to_string()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use silverhand_backend_core::foundation::persistence::PersistenceFailure;
    use silverhand_backend_core::{
        conversation_messaging::messaging::{
            AddressedPrincipals, CreatedMessage, FromKind, MessageContent, MessageCreatedFact,
            MessageId, MessageSeq,
            post_commit::{DeliveryPlan, MainDeliveryPlan, PostCommitPlan},
        },
        foundation::identity::{AgentId, UserId},
    };

    use super::*;
    use crate::conversation_messaging::messaging::admission::validate_human_payload;

    struct RefusingUnitOfWork;

    impl SendMessageUnitOfWork for RefusingUnitOfWork {
        async fn send_message(
            &self,
            _command: SendMessage,
        ) -> Result<SendMessageOutcome, SendMessageError> {
            Err(silverhand_backend_core::conversation_messaging::messaging::ConversationRefusal::NotFound.into())
        }
    }

    struct NoPositions;

    impl ReferencePositions for NoPositions {
        fn autolink_spans(&self, _content: &MessageContent) -> Vec<std::ops::Range<usize>> {
            Vec::new()
        }
    }

    struct GrantsNothing;

    impl DirectLineAuthority for GrantsNothing {
        async fn direct_lines(
            &self,
            _author: &UserId,
            _addressed: &AddressedPrincipals,
        ) -> Result<Vec<AgentId>, PersistenceFailure> {
            Ok(Vec::new())
        }
    }

    #[derive(Default)]
    struct CountingAftermath(AtomicUsize);

    impl SendAftermath for CountingAftermath {
        async fn announce(&self, _outcome: &SendMessageOutcome) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct SuccessfulUnit(SendMessageOutcome);

    impl SendMessageUnitOfWork for SuccessfulUnit {
        async fn send_message(
            &self,
            _command: SendMessage,
        ) -> Result<SendMessageOutcome, SendMessageError> {
            Ok(self.0.clone())
        }
    }

    fn committed_outcome() -> SendMessageOutcome {
        let message = CreatedMessage {
            client_message_id: None,
            id: MessageId::parse("message-1").unwrap(),
            conversation_id: conversation(),
            from_kind: FromKind::User,
            from_agent_id: None,
            from_user_id: Some(UserId::parse("user-1").unwrap()),
            author: None,
            content: MessageContent::parse("hello").unwrap(),
            attachments: Vec::new(),
            seq: MessageSeq::try_from(1).unwrap(),
            created_at_ms: 1,
            thread_root_id: None,
            options: None,
        };
        SendMessageOutcome::committed_send(
            MessageCreatedFact::committed(
                message,
                None,
                PostCommitPlan::committed(
                    None,
                    DeliveryPlan::MainLine(MainDeliveryPlan::committed(Vec::new(), Vec::new())),
                    Vec::new(),
                ),
                false,
                Vec::new(),
            ),
            Vec::new(),
        )
    }

    fn conversation() -> ConversationId {
        ConversationId::parse("conv-1").unwrap()
    }

    fn context(actor: SendActor) -> ResolvedSendContext {
        ResolvedSendContext::resolved(
            actor,
            conversation(),
            conversation(),
            SendDestination::MainLine,
            false,
        )
    }

    fn settled() -> ValidatedSendPayload {
        validate_human_payload(Some(serde_json::json!({"content": "hello"})))
            .unwrap()
            .seal()
            .unwrap()
    }

    /// A successful write reaches exactly one required aftermath before its outcome is
    /// returned. A refused write reaches none, so post-commit effects cannot run for a
    /// fact that does not exist.
    #[tokio::test]
    async fn only_a_committed_send_reaches_the_single_aftermath() {
        let aftermath = CountingAftermath::default();
        let expected = committed_outcome();
        let actual = execute(
            context(SendActor::User(UserId::parse("user-1").unwrap())),
            settled(),
            &NoPositions,
            &GrantsNothing,
            &SuccessfulUnit(expected.clone()),
            &aftermath,
        )
        .await
        .unwrap();
        assert_eq!(actual, expected);
        assert_eq!(aftermath.0.load(Ordering::SeqCst), 1);

        let _ = execute(
            context(SendActor::User(UserId::parse("user-1").unwrap())),
            settled(),
            &NoPositions,
            &GrantsNothing,
            &RefusingUnitOfWork,
            &aftermath,
        )
        .await;
        assert_eq!(aftermath.0.load(Ordering::SeqCst), 1);
    }
}
