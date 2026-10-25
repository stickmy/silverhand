//! The two route adapters: same rules, different order.
//!
//! Each one is thin on purpose. An adapter decides only three things — WHEN it
//! authorizes relative to reading the body, how it words its own refusals, and which
//! [`SendActor`] variant it builds. Every rule about what a send IS lives in
//! [`super::admission`]; committing one lives in [`super::execute`]. Neither is
//! duplicated here, and neither can be, because an adapter has no rule of its own to
//! keep in sync.
//!
//! THE ORDERS ARE DIFFERENT AND BOTH ARE CONTRACTS. The human door completes
//! authorization before it has looked at a body: a channel that does not exist, or
//! one this person may not post to, is answered without regard to what they sent. The
//! agent door validates first and only then resolves a target and its capability. The
//! difference is observable — a request that breaks two rules is answered by whichever
//! gate its own door asks first — which is why the cross cases below are the real
//! test rather than the happy paths.

use silverhand_backend_core::{
    conversation_messaging::messaging::{
        AddressRefusal, CapabilityDenied, ChannelAccess, ChannelAccessRefusal, DirectLineAuthority,
        DirectPairAllocation, ReferencePositions, ResolvedStream, SendActor, SendCapability,
        SendDestination, SendMessageError, SendMessageOutcome, SendMessageUnitOfWork,
        TargetAddress,
    },
    foundation::identity::{ConversationId, UserId},
    host_runtime::runtime_credential::RuntimePrincipal,
};

use super::{
    admission::{PayloadRefusal, validate_agent_payload, validate_human_payload},
    aftermath::SendAftermath,
    execute::{ResolvedSendContext, execute},
};
use crate::foundation::cli_invoke_observer::CliInvokeRequestContext;

/// The machinery a send needs whichever door it came through: where a body may
/// address from, who may open a direct line, and the write unit itself.
///
/// Bundled because both doors need exactly these three and need them identically —
/// eight loose parameters was the lint's way of saying the same thing. Naming the set
/// once also makes "one use-case" visible at the call site: the doors differ in their
/// gates, never in the machinery.
pub(crate) struct SendPorts<'ports, P, A, U, F> {
    pub positions: &'ports P,
    pub authority: &'ports A,
    pub unit_of_work: &'ports U,
    pub aftermath: &'ports F,
}

/// The address named a stream this agent is in, and this door still acts only where the
/// session is anchored.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutOfSessionScope;

/// Whether this credential may act on that stream through this door.
///
/// A PURE FUNCTION, not a port, and that is a product decision rather than a convenience: its
/// whole input is the slot kind, the anchor the credential carries, and the conversation the
/// address resolved to — resolution has already proven participation, so there is nothing to
/// look up. Made a port it would become a replaceable behaviour, and a composition could then
/// decide that an anchored session may post anywhere, which is exactly the class of injection
/// seam the position rule was closed against.
///
/// CRATE-PRIVATE for the same reason. The ports around it are public because a composition has
/// to implement them; this is the route's own canonical rule, so there is no legitimate caller
/// outside — closing the replaceable port while leaving a public function would add a new public
/// surface for nothing.
///
/// It leaks nothing: reach is only asked once resolution has proven participation, so every
/// refusal here is about a stream the caller can already enumerate.
pub(crate) fn reach(
    credential: &RuntimePrincipal,
    stream: &ResolvedStream,
) -> Result<(), OutOfSessionScope> {
    match credential {
        // Unanchored, and participation was the resolver's answer, so there is nothing left to
        // judge.
        RuntimePrincipal::AgentSlot { .. } => Ok(()),
        RuntimePrincipal::ConversationSlot { source, .. } if *source == stream.conversation => {
            Ok(())
        }
        RuntimePrincipal::ConversationSlot { .. } => Err(OutOfSessionScope),
    }
}

/// Why the HUMAN door refused. Its own type rather than a shared one: a refusal a door
/// cannot produce has no business being representable in its answer, and a shared enum
/// would put an unanswerable arm in each door's wire mapping — a branch no test can
/// reach and every reader has to reason about.
///
/// Not `Clone`: a send failure carries a source chain, and duplicating one would invite
/// a second report of a single failure.
#[derive(Debug)]
pub(crate) enum HumanRefusal {
    Access(ChannelAccessRefusal),
    Authority(crate::foundation::error::BoundaryFailure),
    Payload(PayloadRefusal),
    Send(SendMessageError),
}

/// Why the AGENT door refused. The mirror of [`HumanRefusal`], and deliberately not the
/// same type: the two doors share every RULE and no refusal vocabulary.
#[derive(Debug)]
pub(crate) enum AgentRefusal {
    Payload(PayloadRefusal),
    Address(AddressRefusal),
    OutOfSessionScope,
    CapabilityDenied,
    Authority(crate::foundation::error::BoundaryFailure),
    Send(SendMessageError),
}

/// The HUMAN door: authorize, then admit, then commit.
///
/// Authorization first is not an optimization — it is what keeps a person who may not
/// post to a channel from learning anything about how their body was judged.
pub(crate) async fn human_send<A, P, D, U, F>(
    channel: &ConversationId,
    author: &UserId,
    raw_body: Option<serde_json::Value>,
    access: &A,
    ports: SendPorts<'_, P, D, U, F>,
) -> Result<SendMessageOutcome, HumanRefusal>
where
    A: ChannelAccess,
    P: ReferencePositions,
    D: DirectLineAuthority,
    U: SendMessageUnitOfWork,
    F: SendAftermath,
{
    access
        .may_post(channel, author)
        .await
        .map_err(|source| {
            HumanRefusal::Authority(crate::foundation::error::BoundaryFailure::new(source))
        })?
        .map_err(HumanRefusal::Access)?;

    let request = validate_human_payload(raw_body).map_err(HumanRefusal::Payload)?;

    // A person posts where the person is LOOKING, so this door can only express a
    // placement inside the conversation it authorized against — never a foreign stream.
    let destination = match request.reply_to() {
        Some(message) => SendDestination::Reply(message.clone()),
        None => SendDestination::MainLine,
    };

    // STAGE TWO — the content cap. This door has no rule to insert between the refine
    // and the cap, so it seals the moment it has read the destination: admit → seal, one
    // uninterrupted order, byte-identical to when the two stages were fused. Both stages
    // refuse as `HumanRefusal::Payload`, so the wire is unchanged whichever fired.
    let payload = request.seal().map_err(HumanRefusal::Payload)?;

    execute(
        ResolvedSendContext::resolved(
            SendActor::User(author.clone()),
            // BOTH coordinates are the path's channel, named here rather than taken from
            // the gate's answer. The gate has none to give.
            channel.clone(),
            channel.clone(),
            destination,
            // A human has no runtime, so there is nothing to be behind and no bypass
            // to ask for. The flag is `false` because the guard does not apply, not
            // because this door declined to offer it.
            false,
        ),
        payload,
        ports.positions,
        ports.authority,
        ports.unit_of_work,
        ports.aftermath,
    )
    .await
    .map_err(HumanRefusal::Send)
}

/// The AGENT door: admit, resolve, judge reach, ask for the grant, then commit.
///
/// FOUR AUTHORITIES IN A FIXED ORDER, each answering only what it owns. Validating first is
/// this door's contract: a runtime is told its payload is malformed before anything about where
/// it aimed, because that is the error it can correct without another round trip. Then the
/// address is resolved — which also proves participation — then the credential's reach, then
/// the grant.
///
/// THE ORDER IS NOT A TYPE GUARANTEE. Nothing here stops a future edit from asking the
/// capability before reach; the three failure types differ, and the code would still compile.
/// What holds the order are the cross counter-examples beside this function: a request that
/// fails two gates at once is answered by whichever comes first, so swapping two lines turns a
/// cell red.
///
/// The credential's own conversation decides REACH and nothing else. Where the message lands —
/// and therefore the coordinate a send is bound to — is the resolved TARGET, for both variants:
/// an anchored slot has target == source once reach has passed, and an unanchored one has no
/// ambient source to fall back on. Passing the credential's conversation as the send's source
/// would have aimed every unanchored send at a stream nobody named.
#[cfg(test)]
pub(crate) async fn agent_send<A, C, P, D, U, F>(
    credential: &RuntimePrincipal,
    raw_body: Option<serde_json::Value>,
    address: &A,
    capability: &C,
    ports: SendPorts<'_, P, D, U, F>,
) -> Result<SendMessageOutcome, AgentRefusal>
where
    A: TargetAddress,
    C: SendCapability,
    P: ReferencePositions,
    D: DirectLineAuthority,
    U: SendMessageUnitOfWork,
    F: SendAftermath,
{
    agent_send_observed(credential, raw_body, address, capability, ports, None).await
}

pub(crate) async fn agent_send_observed<A, C, P, D, U, F>(
    credential: &RuntimePrincipal,
    raw_body: Option<serde_json::Value>,
    address: &A,
    capability: &C,
    ports: SendPorts<'_, P, D, U, F>,
    audit: Option<&CliInvokeRequestContext>,
) -> Result<SendMessageOutcome, AgentRefusal>
where
    A: TargetAddress,
    C: SendCapability,
    P: ReferencePositions,
    D: DirectLineAuthority,
    U: SendMessageUnitOfWork,
    F: SendAftermath,
{
    let request = validate_agent_payload(raw_body).map_err(AgentRefusal::Payload)?;

    let allocation = match credential {
        RuntimePrincipal::AgentSlot { .. } => DirectPairAllocation::Permitted,
        RuntimePrincipal::ConversationSlot { .. } => DirectPairAllocation::Forbidden,
    };
    let stream = address
        .resolve(
            credential.agent(),
            request.target(),
            allocation,
            silverhand_backend_core::conversation_messaging::messaging::DeliveryIntent::Write,
        )
        .await
        .map_err(|source| {
            AgentRefusal::Authority(crate::foundation::error::BoundaryFailure::new(source))
        })?
        .map_err(AgentRefusal::Address)?;

    if let Some(audit) = audit {
        audit.set_conversation(&stream.conversation);
    }
    reach(credential, &stream).map_err(|OutOfSessionScope| AgentRefusal::OutOfSessionScope)?;

    capability
        .may_send_messages(credential.agent(), &stream.conversation)
        .await
        .map_err(|source| {
            AgentRefusal::Authority(crate::foundation::error::BoundaryFailure::new(source))
        })?
        .map_err(|CapabilityDenied| AgentRefusal::CapabilityDenied)?;

    if let (Some(audit), Some(reason)) = (audit, request.stale_resolution_reason()) {
        audit.set_stale_reason(reason);
    }

    let destination = match stream.thread_root {
        Some(root) => SendDestination::Thread(root),
        None => SendDestination::MainLine,
    };
    let freshness_bypass = request.freshness_bypass();

    execute(
        ResolvedSendContext::resolved(
            SendActor::Agent(credential.agent().clone()),
            stream.conversation.clone(),
            stream.conversation,
            destination,
            freshness_bypass,
        ),
        request.into_payload(),
        ports.positions,
        ports.authority,
        ports.unit_of_work,
        ports.aftermath,
    )
    .await
    .map_err(AgentRefusal::Send)
}

#[cfg(test)]
mod tests {
    use std::{
        convert::Infallible,
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use silverhand_backend_core::conversation_messaging::messaging::{
        AddressedPrincipals, ConversationRefusal, MessageContent, MessageId, SendMessage,
    };
    use silverhand_backend_core::foundation::persistence::PersistenceFailure;
    use silverhand_backend_core::{
        conversation_messaging::messaging::ThreadRootId,
        foundation::cli_invoke_audit::CliStaleResolutionReason,
        foundation::identity::{AgentId, DaemonInstanceId},
        host_runtime::runtime_credential::{RuntimeCustody, RuntimeGeneration},
    };

    use super::*;

    /// What one command asked for, in the terms these two doors can differ in.
    #[derive(Clone, Debug, Eq, PartialEq)]
    struct Recorded {
        actor: SendActor,
        source: ConversationId,
        target: ConversationId,
        destination: SendDestination,
        freshness_bypass: bool,
    }

    /// Counts and records every command it was asked to commit. A refusal that reaches
    /// it is a refusal that happened too late; a command that reaches it can be read
    /// back field by field.
    #[derive(Default)]
    struct CountingUnitOfWork {
        calls: AtomicUsize,
        commands: Mutex<Vec<Recorded>>,
    }

    impl CountingUnitOfWork {
        fn commands(&self) -> Vec<Recorded> {
            self.commands.lock().unwrap().clone()
        }

        fn actors(&self) -> Vec<SendActor> {
            self.commands()
                .into_iter()
                .map(|command| command.actor)
                .collect()
        }

        fn destinations(&self) -> Vec<SendDestination> {
            self.commands()
                .into_iter()
                .map(|command| command.destination)
                .collect()
        }
    }

    impl SendMessageUnitOfWork for CountingUnitOfWork {
        async fn send_message(
            &self,
            command: SendMessage,
        ) -> Result<SendMessageOutcome, SendMessageError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.commands.lock().unwrap().push(Recorded {
                actor: command.actor().clone(),
                source: command.source_conversation().clone(),
                target: command.target_conversation().clone(),
                destination: command.destination().clone(),
                freshness_bypass: command.freshness_bypass(),
            });
            Err(ConversationRefusal::NotFound.into())
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

    struct NoAftermath;

    impl SendAftermath for NoAftermath {
        async fn announce(&self, _outcome: &SendMessageOutcome) {}
    }

    /// The gate, and ONLY the gate. It is handed `elsewhere()` so it would answer with a
    /// foreign conversation if the port let it — and the port does not, which is what
    /// `the_human_gate_cannot_move_the_send_it_authorized` reads back.
    struct Channel(Result<(), ChannelAccessRefusal>);

    impl ChannelAccess for Channel {
        type Error = Infallible;

        async fn may_post(
            &self,
            _channel: &ConversationId,
            _author: &UserId,
        ) -> Result<Result<(), ChannelAccessRefusal>, Self::Error> {
            let _would_rather_answer_with = elsewhere();
            Ok(self.0)
        }
    }

    /// A resolver that answers with a fixed stream, so a test can say what an address
    /// resolved TO without restating the grammar that reads one.
    struct Address(Result<ResolvedStream, AddressRefusal>);

    impl Address {
        fn main_line() -> Self {
            Self(Ok(ResolvedStream {
                conversation: elsewhere(),
                thread_root: None,
            }))
        }

        fn cluster() -> Self {
            Self(Ok(ResolvedStream {
                conversation: elsewhere(),
                thread_root: Some(ThreadRootId::parse("root-1").unwrap()),
            }))
        }

        fn refuses(refusal: AddressRefusal) -> Self {
            Self(Err(refusal))
        }
    }

    impl TargetAddress for Address {
        type Error = Infallible;

        async fn resolve(
            &self,
            _agent: &AgentId,
            _target: &str,
            _allocation: DirectPairAllocation,
            _intent: silverhand_backend_core::conversation_messaging::messaging::DeliveryIntent,
        ) -> Result<Result<ResolvedStream, AddressRefusal>, Self::Error> {
            Ok(self.0.clone())
        }
    }

    /// The grant, and only the grant. It cannot name a bucket, so a test cannot accidentally
    /// assert one this door does not ask for.
    struct Grant(Result<(), CapabilityDenied>);

    impl SendCapability for Grant {
        type Error = Infallible;

        async fn may_send_messages(
            &self,
            _agent: &AgentId,
            _conversation: &ConversationId,
        ) -> Result<Result<(), CapabilityDenied>, Self::Error> {
            Ok(self.0)
        }
    }

    /// An unanchored credential reaches wherever the resolver says the agent participates.
    fn custody() -> RuntimeCustody {
        RuntimeCustody::new(
            RuntimeGeneration::new(1).unwrap(),
            DaemonInstanceId::parse("daemon-1").unwrap(),
        )
    }

    fn agent_slot() -> RuntimePrincipal {
        RuntimePrincipal::AgentSlot {
            agent: agent(),
            custody: custody(),
        }
    }

    /// An anchored one may act only in the conversation it names.
    fn anchored_to(source: ConversationId) -> RuntimePrincipal {
        RuntimePrincipal::ConversationSlot {
            agent: agent(),
            source,
            custody: custody(),
        }
    }

    fn ports<'a>(
        unit: &'a CountingUnitOfWork,
    ) -> SendPorts<'a, NoPositions, GrantsNothing, CountingUnitOfWork, NoAftermath> {
        SendPorts {
            positions: &NoPositions,
            authority: &GrantsNothing,
            unit_of_work: unit,
            aftermath: &NoAftermath,
        }
    }

    fn conversation() -> ConversationId {
        ConversationId::parse("conv-1").unwrap()
    }

    /// A stream that is NOT the one the credential lives in — the whole point of an
    /// absolute address.
    fn elsewhere() -> ConversationId {
        ConversationId::parse("conv-2").unwrap()
    }

    fn user() -> UserId {
        UserId::parse("user-1").unwrap()
    }

    fn agent() -> AgentId {
        AgentId::parse("agent-1").unwrap()
    }

    fn malformed() -> Option<serde_json::Value> {
        Some(serde_json::json!({"content": 7}))
    }

    fn human_body() -> Option<serde_json::Value> {
        Some(serde_json::json!({"content": "hello"}))
    }

    fn agent_body() -> Option<serde_json::Value> {
        Some(serde_json::json!({"content": "hello", "target": "#conv:conv-2"}))
    }

    fn assert_payload_invalid(refusal: AgentRefusal) {
        match refusal {
            AgentRefusal::Payload(PayloadRefusal::Invalid) => {}
            other => panic!("expected a payload refusal, got {other:?}"),
        }
    }

    /// THE HUMAN CROSS. The body is malformed AND the caller may not post. If the
    /// orders were the same, this would answer the payload refusal; the human door
    /// answers access, and the write unit is never reached.
    #[tokio::test]
    async fn the_human_door_answers_access_before_it_judges_a_body() {
        let unit = CountingUnitOfWork::default();

        for refusal in [
            ChannelAccessRefusal::NotFound,
            ChannelAccessRefusal::Forbidden,
        ] {
            let outcome = human_send(
                &conversation(),
                &user(),
                malformed(),
                &Channel(Err(refusal)),
                ports(&unit),
            )
            .await;

            assert!(
                matches!(outcome, Err(HumanRefusal::Access(answered)) if answered == refusal),
                "the human door judged the body before deciding access"
            );
        }
        assert_eq!(unit.calls.load(Ordering::SeqCst), 0);
    }

    /// THE AGENT CROSS, and it is the mirror image. The body is malformed AND the
    /// target is unusable. The agent door answers the payload, because it validates
    /// first — so the very same pair of faults gets a different answer at each door.
    #[tokio::test]
    async fn the_agent_door_judges_a_body_before_it_resolves_a_target() {
        let unit = CountingUnitOfWork::default();

        for refusal in [AddressRefusal::Malformed, AddressRefusal::Unresolved] {
            let outcome = agent_send(
                &agent_slot(),
                malformed(),
                &Address::refuses(refusal),
                &Grant(Ok(())),
                ports(&unit),
            )
            .await;

            assert!(
                matches!(outcome, Err(AgentRefusal::Payload(_))),
                "the agent door resolved a target before judging the body"
            );
        }
        assert_eq!(unit.calls.load(Ordering::SeqCst), 0);
    }

    /// Every refusal, from either gate of either door, stops before the write unit.
    #[tokio::test]
    async fn no_refusal_reaches_the_write_unit() {
        let unit = CountingUnitOfWork::default();

        let _ = human_send(
            &conversation(),
            &user(),
            human_body(),
            &Channel(Err(ChannelAccessRefusal::NotFound)),
            ports(&unit),
        )
        .await;
        let _ = human_send(
            &conversation(),
            &user(),
            malformed(),
            &Channel(Ok(())),
            ports(&unit),
        )
        .await;
        let _ = agent_send(
            &agent_slot(),
            malformed(),
            &Address::main_line(),
            &Grant(Ok(())),
            ports(&unit),
        )
        .await;
        let _ = agent_send(
            &agent_slot(),
            agent_body(),
            &Address::refuses(AddressRefusal::Unresolved),
            &Grant(Ok(())),
            ports(&unit),
        )
        .await;

        assert_eq!(unit.calls.load(Ordering::SeqCst), 0);
    }

    /// Both doors, once past their own gates, reach ONE write unit and are told apart
    /// there only by which principal is sending.
    #[tokio::test]
    async fn both_doors_converge_on_one_write_unit() {
        let unit = CountingUnitOfWork::default();

        let _ = human_send(
            &conversation(),
            &user(),
            human_body(),
            &Channel(Ok(())),
            ports(&unit),
        )
        .await;
        let _ = agent_send(
            &agent_slot(),
            agent_body(),
            &Address::main_line(),
            &Grant(Ok(())),
            ports(&unit),
        )
        .await;

        assert_eq!(unit.calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            unit.actors(),
            vec![SendActor::User(user()), SendActor::Agent(agent())]
        );
    }

    /// A SEND WITHOUT AN ADDRESS IS NOT A SEND, and the address is a body field.
    ///
    /// The counter-example for taking it from any coordinate that happens to be at
    /// hand: a door that substituted its own path parameter, or the conversation the
    /// credential lives in, would accept all three of these bodies and deliver them
    /// somewhere the caller never named.
    #[tokio::test]
    async fn the_agent_door_refuses_a_body_that_names_no_target() {
        let unit = CountingUnitOfWork::default();

        // The three ways a body fails to name an address — the field absent, present
        // `null`, present empty — are ONE refusal now: a `target` rule failed. The door
        // no longer publishes which, only that the body was not a send it can act on.
        for body in [
            serde_json::json!({"content": "hello"}),
            serde_json::json!({"content": "hello", "target": null}),
            serde_json::json!({"content": "hello", "target": ""}),
        ] {
            let outcome = agent_send(
                &agent_slot(),
                Some(body.clone()),
                &Address::main_line(),
                &Grant(Ok(())),
                ports(&unit),
            )
            .await;

            assert_payload_invalid(outcome.expect_err("a send with no address was accepted"));
        }
        assert_eq!(unit.calls.load(Ordering::SeqCst), 0);
    }

    /// A VALUE-SHAPED POSITIONAL ARRAY IS NOT AN ENVELOPE. Serde's derived struct
    /// `Deserialize` binds a JSON array to a struct's fields positionally, so WITHOUT the
    /// shared object floor `["hello"]` would decode as `{content:"hello"}` and `["hello",
    /// "#conv:conv-2"]` as `{content, target}` — an array-shaped body slipping into a send
    /// authority. Both send doors decode through the shared `ObjectOnly<T>`, so each array is
    /// the one minimal payload refusal instead, and — the human door with access already
    /// GRANTED, so the body floor is the only thing that can stop it — the write unit is
    /// never reached. This asserts the accepted set narrowed and authority zero-reach, not a
    /// typed `JsonRejection` source chain (these Value-first doors fold every fault to the
    /// same refusal).
    #[tokio::test]
    async fn a_value_shaped_positional_array_is_refused_at_both_send_doors() {
        let unit = CountingUnitOfWork::default();

        let human = human_send(
            &conversation(),
            &user(),
            Some(serde_json::json!(["hello"])),
            &Channel(Ok(())),
            ports(&unit),
        )
        .await;
        assert!(
            matches!(human, Err(HumanRefusal::Payload(PayloadRefusal::Invalid))),
            "a value-shaped array bound the human envelope positionally"
        );

        let agent = agent_send(
            &agent_slot(),
            Some(serde_json::json!(["hello", "#conv:conv-2"])),
            &Address::main_line(),
            &Grant(Ok(())),
            ports(&unit),
        )
        .await;
        assert_payload_invalid(agent.expect_err("a value-shaped array bound the agent envelope"));

        assert_eq!(unit.calls.load(Ordering::SeqCst), 0);
    }

    /// EACH DOOR CARRIES THE DESTINATION ITS OWN BODY NAMED, and the two are different
    /// coordinates: the human door names a message to reply to inside the conversation
    /// it authorized against, the agent door's cluster comes from the address it
    /// resolved. A door that hard-coded the main line would record four main lines
    /// here.
    #[tokio::test]
    async fn each_door_carries_the_destination_its_own_body_named() {
        let unit = CountingUnitOfWork::default();

        let _ = human_send(
            &conversation(),
            &user(),
            human_body(),
            &Channel(Ok(())),
            ports(&unit),
        )
        .await;
        let _ = human_send(
            &conversation(),
            &user(),
            Some(serde_json::json!({"content": "hello", "reply_to_message_id": "msg-9"})),
            &Channel(Ok(())),
            ports(&unit),
        )
        .await;
        let _ = agent_send(
            &agent_slot(),
            agent_body(),
            &Address::main_line(),
            &Grant(Ok(())),
            ports(&unit),
        )
        .await;
        let _ = agent_send(
            &agent_slot(),
            agent_body(),
            &Address::cluster(),
            &Grant(Ok(())),
            ports(&unit),
        )
        .await;

        assert_eq!(
            unit.destinations(),
            vec![
                SendDestination::MainLine,
                SendDestination::Reply(MessageId::parse("msg-9").unwrap()),
                SendDestination::MainLine,
                SendDestination::Thread(ThreadRootId::parse("root-1").unwrap()),
            ]
        );
    }

    /// THE HUMAN GATE CANNOT MOVE THE SEND IT AUTHORIZED.
    ///
    /// The gate is asked about the path's channel and answers yes or no. It holds a second,
    /// foreign conversation and has no way to hand it back — the port's answer carries no
    /// coordinate at all — so "authorize against A, write into B" is not a mistake this
    /// door can make. Both of the command's coordinates are the channel the caller named.
    #[tokio::test]
    async fn the_human_gate_cannot_move_the_send_it_authorized() {
        let unit = CountingUnitOfWork::default();

        let _ = human_send(
            &conversation(),
            &user(),
            human_body(),
            &Channel(Ok(())),
            ports(&unit),
        )
        .await;

        let commands = unit.commands();
        assert_eq!(commands[0].source, conversation());
        assert_eq!(commands[0].target, conversation());
        assert_ne!(commands[0].target, elsewhere());
    }

    /// A SEND IS BOUND TO WHERE IT LANDED, and the credential's own conversation is nowhere in
    /// the command.
    ///
    /// Both coordinates are the RESOLVED stream. An anchored slot has target == source by the
    /// time reach has passed, and an unanchored one has no ambient source at all — so passing
    /// the credential's conversation would have bound every unanchored send to a stream nobody
    /// named, and bound a cross-stream anchored send to one it was refused from.
    /// The credential's conversation decides REACH and nothing
    /// else. A human's two coordinates are the same because that door cannot address a foreign
    /// stream at all.
    #[tokio::test]
    async fn a_send_is_bound_to_the_stream_it_landed_in() {
        let unit = CountingUnitOfWork::default();

        // Unanchored: nothing but the resolved stream could have been used.
        let _ = agent_send(
            &agent_slot(),
            agent_body(),
            &Address::main_line(),
            &Grant(Ok(())),
            ports(&unit),
        )
        .await;
        // Anchored to the stream it resolved to, which is the only anchored send reach admits.
        let _ = agent_send(
            &anchored_to(elsewhere()),
            agent_body(),
            &Address::main_line(),
            &Grant(Ok(())),
            ports(&unit),
        )
        .await;
        let _ = human_send(
            &conversation(),
            &user(),
            human_body(),
            &Channel(Ok(())),
            ports(&unit),
        )
        .await;

        let commands = unit.commands();
        for (index, command) in commands.iter().take(2).enumerate() {
            assert_eq!(command.source, elsewhere(), "agent send {index}");
            assert_eq!(command.target, elsewhere(), "agent send {index}");
            // The credential's own conversation never becomes a coordinate of the send.
            assert_ne!(command.source, conversation(), "agent send {index}");
        }
        assert_eq!(commands[2].source, conversation());
        assert_eq!(commands[2].target, conversation());
    }

    /// REACH IS DECIDED BY THE CREDENTIAL, and the two variants answer the same address
    /// differently. A single principal shape could not express both.
    #[tokio::test]
    async fn an_anchored_credential_may_act_only_where_it_is_anchored() {
        let unit = CountingUnitOfWork::default();

        let refused = agent_send(
            &anchored_to(conversation()),
            agent_body(),
            &Address::main_line(),
            &Grant(Ok(())),
            ports(&unit),
        )
        .await;
        assert!(matches!(refused, Err(AgentRefusal::OutOfSessionScope)));
        assert_eq!(unit.calls.load(Ordering::SeqCst), 0);

        let allowed = agent_send(
            &agent_slot(),
            agent_body(),
            &Address::main_line(),
            &Grant(Ok(())),
            ports(&unit),
        )
        .await;
        assert!(
            matches!(allowed, Err(AgentRefusal::Send(_))),
            "it reached the write unit"
        );
        assert_eq!(unit.calls.load(Ordering::SeqCst), 1);
    }

    /// ONLY A RUNTIME CAN ASK TO SKIP THE FRESHNESS GUARD, and only by saying so.
    ///
    /// The human door has no such field, so the same member in a human body is an
    /// unknown one — stripped, exactly as the reference's schemas strip unknown members
    /// rather than refusing them. Reading it there would hand a person an escape hatch
    /// from a guard that never applied to them.
    #[tokio::test]
    async fn only_a_runtime_can_ask_to_skip_the_freshness_guard() {
        let unit = CountingUnitOfWork::default();

        let _ = agent_send(
            &agent_slot(),
            Some(serde_json::json!({
                "content": "hello",
                "target": "#conv:conv-2",
                "freshness_bypass": true
            })),
            &Address::main_line(),
            &Grant(Ok(())),
            ports(&unit),
        )
        .await;
        let _ = agent_send(
            &agent_slot(),
            agent_body(),
            &Address::main_line(),
            &Grant(Ok(())),
            ports(&unit),
        )
        .await;
        let _ = human_send(
            &conversation(),
            &user(),
            Some(serde_json::json!({"content": "hello", "freshness_bypass": true})),
            &Channel(Ok(())),
            ports(&unit),
        )
        .await;

        assert_eq!(
            unit.commands()
                .into_iter()
                .map(|command| command.freshness_bypass)
                .collect::<Vec<_>>(),
            vec![true, false, false]
        );
    }

    #[tokio::test]
    async fn stale_audit_fact_is_set_only_after_target_reach_and_capability() {
        let unit = CountingUnitOfWork::default();
        let body = || {
            Some(serde_json::json!({
                "content": "hello",
                "target": "#conv:conv-2",
                "stale_event_id": "evt-secret",
                "stale_resolution_reason": "revised"
            }))
        };

        let target_refused = CliInvokeRequestContext::default();
        let _ = agent_send_observed(
            &agent_slot(),
            body(),
            &Address::refuses(AddressRefusal::Unresolved),
            &Grant(Ok(())),
            ports(&unit),
            Some(&target_refused),
        )
        .await;
        assert_eq!(target_refused.stale_reason(), None);

        let capability_refused = CliInvokeRequestContext::default();
        let _ = agent_send_observed(
            &agent_slot(),
            body(),
            &Address::main_line(),
            &Grant(Err(CapabilityDenied)),
            ports(&unit),
            Some(&capability_refused),
        )
        .await;
        assert_eq!(capability_refused.stale_reason(), None);

        let authorized = CliInvokeRequestContext::default();
        let _ = agent_send_observed(
            &agent_slot(),
            body(),
            &Address::main_line(),
            &Grant(Ok(())),
            ports(&unit),
            Some(&authorized),
        )
        .await;
        assert_eq!(
            authorized.stale_reason(),
            Some(CliStaleResolutionReason::Revised)
        );
    }
}
