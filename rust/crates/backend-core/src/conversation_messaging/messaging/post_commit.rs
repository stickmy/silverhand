//! What a committed send owes the world afterwards.
//!
//! Everything here happens after the writer has been released, so nothing it does
//! can change what was written. That is the whole shape of this module: the send is
//! already a fact, and these are attempts to tell people about it.
//!
//! Each capability is its own trait with its own failure. A single wide port would
//! make "one failure does not affect another" a claim about this file's control
//! flow; separate ports make it a property of the dependency graph, because no
//! attempt can be reached only through another.

use std::{collections::HashSet, error::Error, future::Future};

use super::{
    AgentId, CommittedMessageView, ConversationId, CreatedMessage, DirectPairOpenedFact,
    MainCommittedSend, MessageCreatedFact, MessageId, NotificationId, ParticipantHandle,
    PrincipalIdentity, ReplyCommittedSend, UserId, WorkspaceId,
    mention_cc::{CommittedMentionCopy, MentionCopyOutcome, MentionCopySkip},
};

/// What a committed send owes the world.
///
/// Two layers, because the facts have two shapes. Naming someone does not depend on
/// where the message landed, so mentions are common. Who receives the message does:
/// a main-line send reaches the conversation's participants, a reply reaches the
/// cluster's followers. Keeping those apart by type is what stops one path's
/// announcer from executing the other's plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PostCommitPlan {
    mentions: Option<MentionIntent>,
    delivery: DeliveryPlan,
    opened_direct_pairs: Vec<DirectPairOpenedFact>,
}

impl PostCommitPlan {
    /// Assembled by the unit of work that committed the send. A trust seam, stated
    /// plainly: this factory is public, so every crate that can call it is inside the
    /// same trusted boundary. What it does guarantee is that a fact already returned
    /// cannot be replaced or modified — not that one cannot be fabricated.
    pub fn committed(
        mentions: Option<MentionIntent>,
        delivery: DeliveryPlan,
        opened_direct_pairs: Vec<DirectPairOpenedFact>,
    ) -> Self {
        Self {
            mentions,
            delivery,
            opened_direct_pairs,
        }
    }

    /// What this send says about naming people, when it named anyone inside a
    /// workspace. Read-only, so a caller cannot add a name the send did not — nor
    /// take one away:
    ///
    /// ```compile_fail
    /// use silverhand_backend_core::conversation_messaging::messaging::post_commit::{MentionIntent, PostCommitPlan};
    ///
    /// fn tamper(plan: &PostCommitPlan, other: MentionIntent) {
    ///     *plan.mentions().unwrap() = other;
    /// }
    /// ```
    pub fn mentions(&self) -> Option<&MentionIntent> {
        self.mentions.as_ref()
    }

    pub fn delivery(&self) -> &DeliveryPlan {
        &self.delivery
    }

    /// Direct-pair rows this transaction created. They are facts to publish only
    /// after the transaction commits; a rollback never exposes this plan.
    fn opened_direct_pairs(&self) -> &[DirectPairOpenedFact] {
        &self.opened_direct_pairs
    }
}

/// Who receives the message, by the path it took.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeliveryPlan {
    MainLine(MainDeliveryPlan),
    ReplyCluster(ReplyAudience),
}

/// A main-line send's recipients, and which of them this send is allowed to start.
///
/// The two lists differ, and not by accident. An agent writing into its own
/// conversation is not delivered its own message and starts nobody: the runtime it
/// would start is the one already running to write. An agent writing into ANOTHER
/// conversation is a different case — the target's own session is a recipient there,
/// and idle recipients are started. A person's send excludes nobody and starts
/// whoever is idle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MainDeliveryPlan {
    recipients: Vec<AgentId>,
    start_eligible: Vec<AgentId>,
}

impl MainDeliveryPlan {
    pub fn committed(recipients: Vec<AgentId>, start_eligible: Vec<AgentId>) -> Self {
        Self {
            recipients,
            start_eligible,
        }
    }

    /// Who the message is owed to. Read-only for the same reason as the mentions: a
    /// recipient list a caller could edit would be a recipient list the send did not
    /// decide.
    ///
    /// ```compile_fail
    /// use silverhand_backend_core::conversation_messaging::messaging::post_commit::MainDeliveryPlan;
    ///
    /// fn tamper(plan: &MainDeliveryPlan) {
    ///     plan.recipients().push(todo!());
    /// }
    /// ```
    pub fn recipients(&self) -> &[AgentId] {
        &self.recipients
    }

    /// Which recipients this send may start if nothing is running for them. Whether
    /// one IS idle is volatile and is not decided here.
    pub fn start_eligible(&self) -> &[AgentId] {
        &self.start_eligible
    }
}

/// A reply's audience: the cluster's follower agents that still participate, minus
/// the sender.
///
/// Named for what it is rather than "the reply's delivery plan", because it carries
/// no delivery kind. Every recipient of a current reply is owed a turn — the kind is
/// the reply announcer's fixed contract, not something an adapter should have to
/// infer — while `observe` belongs to historical catch-up, which is written inside
/// the transaction and is no part of this.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplyAudience {
    recipients: Vec<AgentId>,
}

impl ReplyAudience {
    pub fn committed(recipients: Vec<AgentId>) -> Self {
        Self { recipients }
    }

    pub fn recipients(&self) -> &[AgentId] {
        &self.recipients
    }
}

/// What a send says about naming people, and nothing about who is entitled to know.
///
/// It carries the handles the BODY named and the coordinates of the message that
/// named them — facts of the send, settled when it committed. It deliberately does
/// NOT carry viewers: whether a person may see this conversation is a fact of the
/// moment the record is written, not of the moment the message was. Freezing it here
/// would let someone removed between the two still receive a directed notification
/// carrying the message's excerpt, and would also drag the reads it takes into the
/// send's own transaction, where a failed lookup for a non-essential notification
/// would roll back a message that was otherwise complete.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MentionIntent {
    handles: Vec<ParticipantHandle>,
    author: Option<UserId>,
    trigger_name: String,
    source_workspace: WorkspaceId,
    source_conversation: ConversationId,
    source_conversation_title: String,
    source_message: MessageId,
    excerpt: String,
}

impl MentionIntent {
    #[allow(clippy::too_many_arguments)]
    pub fn committed(
        handles: Vec<ParticipantHandle>,
        author: Option<UserId>,
        trigger_name: String,
        source_workspace: WorkspaceId,
        source_conversation: ConversationId,
        source_conversation_title: String,
        source_message: MessageId,
        excerpt: String,
    ) -> Self {
        Self {
            handles,
            author,
            trigger_name,
            source_workspace,
            source_conversation,
            source_conversation_title,
            source_message,
            excerpt,
        }
    }

    /// The human handles the body named, deduplicated where they were derived.
    pub fn handles(&self) -> &[ParticipantHandle] {
        &self.handles
    }

    /// Who sent the message, when a person did. The writer excludes them: being told
    /// you named someone is not a mention.
    pub fn author(&self) -> Option<&UserId> {
        self.author.as_ref()
    }

    /// Who named them, as a reader will see it: the sender's display name, or their
    /// name, or their id — whichever is the first that says something.
    pub fn trigger_name(&self) -> &str {
        &self.trigger_name
    }

    pub fn source_workspace(&self) -> &WorkspaceId {
        &self.source_workspace
    }

    pub fn source_conversation(&self) -> &ConversationId {
        &self.source_conversation
    }

    pub fn source_conversation_title(&self) -> &str {
        &self.source_conversation_title
    }

    pub fn source_message(&self) -> &MessageId {
        &self.source_message
    }

    pub fn excerpt(&self) -> &str {
        &self.excerpt
    }
}

/// One row that now exists: who it is for, what it says, and the identity and moment
/// the writing adapter supplied. Returned so the publication that follows has nothing
/// to look up — and only rows that were actually written are returned, so a person
/// who lost access before the write is told nothing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommittedMentionRow {
    id: NotificationId,
    created_at_ms: i64,
    viewer: UserId,
    mentioned_name: ParticipantHandle,
    intent: MentionIntent,
}

impl CommittedMentionRow {
    /// Reported by whichever store wrote the row. A trust seam on the same honest
    /// terms as the fact's: the factory is public, so any crate that can reach it is
    /// inside the same boundary; what is guaranteed is that a row handed to the
    /// publication cannot be edited on the way there.
    pub fn committed(
        id: NotificationId,
        created_at_ms: i64,
        viewer: UserId,
        mentioned_name: ParticipantHandle,
        intent: MentionIntent,
    ) -> Self {
        Self {
            id,
            created_at_ms,
            viewer,
            mentioned_name,
            intent,
        }
    }

    pub fn id(&self) -> &NotificationId {
        &self.id
    }

    pub fn created_at_ms(&self) -> i64 {
        self.created_at_ms
    }

    pub fn viewer(&self) -> &UserId {
        &self.viewer
    }

    pub fn mentioned_name(&self) -> &ParticipantHandle {
        &self.mentioned_name
    }

    pub fn intent(&self) -> &MentionIntent {
        &self.intent
    }
}

/// One attempt failed. The cause is kept, because "telling someone failed" is only
/// actionable with the reason attached.
#[derive(Debug, thiserror::Error)]
#[error("a post-commit effect could not be completed")]
pub struct EffectFailure {
    #[source]
    source: Box<dyn Error + Send + Sync + 'static>,
}

impl EffectFailure {
    pub fn from_source<E>(source: E) -> Self
    where
        E: Error + Send + Sync + 'static,
    {
        Self {
            source: Box::new(source),
        }
    }
}

/// The attempts a committed send makes, in the order they are made.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum EffectStep {
    /// The author's view of their direct lines is stale — a line may be new, and an
    /// existing one has a new preview and a new position. First, because a copy
    /// nobody can see the line for cannot be read.
    DirectLineInvalidation,
    CopyPublished,
    AuthorNotified,
    RuntimeStarted,
    CopyDelivered,
    /// An addressed agent that received no copy, and why.
    SkipRecorded,
    /// The message itself, published to the conversation it landed in.
    MessagePublished,
    /// The durable record that a person was named. A write of its own, in its own
    /// transaction, after the send has already committed.
    MentionedViewerRecorded,
    ViewerNotified,
    /// What every reader's unread state now is. It READS the record above, so it
    /// runs after that write has been attempted.
    UnreadPublished,
    /// The conversation's latest-message summary, to every participant.
    LatestPublished,
    /// Each reader the message should interrupt, told now.
    AlertPublished,
    MainDelivered,
    /// Only for recipients that are not already running.
    IdleStarted,
    /// One follower of a reply's cluster is owed the reply. Distinct from
    /// [`MainDelivered`](EffectStep::MainDelivered): a reply is delivered to the
    /// cluster's followers, never the conversation's participants, and no idle start
    /// follows it — a reply wakes its followers, it does not start the room.
    ReplyDelivered,
}

/// A copy landed on a direct line, so the AUTHOR's view of their lines is out of
/// date: the line may be new, and even an old one now has a different preview and a
/// different place in the list.
///
/// The audience is the author, not the recipient: an agent is not a viewer of the
/// list, so passing it here would leave an adapter unable to address anyone.
pub trait DirectLineInvalidation {
    fn direct_lines_changed(
        &self,
        author: &UserId,
        line: &ConversationId,
    ) -> impl Future<Output = Result<(), EffectFailure>> + Send;
}

/// The copy itself, published to whoever is watching the line it landed on.
pub trait MessagePublication {
    fn publish_copy(
        &self,
        copy: &CommittedMentionCopy,
    ) -> impl Future<Output = Result<(), EffectFailure>> + Send;
}

/// The author's own record that their mention left the room.
pub trait CopyNotification {
    fn notify_author(
        &self,
        author: &UserId,
        agent: &AgentId,
        copy: &CommittedMentionCopy,
    ) -> impl Future<Output = Result<(), EffectFailure>> + Send;
}

/// The recipient may not be running. A runtime is located by the conversation it
/// serves as well as by the agent, because a conversation-scoped one cannot be
/// found by agent alone.
pub trait RuntimeStart {
    fn start(
        &self,
        conversation: &ConversationId,
        agent: &AgentId,
    ) -> impl Future<Output = Result<(), EffectFailure>> + Send;
}

/// The recipient is owed this message now. Independent of starting it: delivery
/// enqueues on its own and the runtime decides what to do about it, so a start that
/// failed does not mean a delivery that cannot be attempted.
pub trait RuntimeDelivery {
    fn deliver(
        &self,
        agent: &AgentId,
        copy: &CommittedMentionCopy,
    ) -> impl Future<Output = Result<(), EffectFailure>> + Send;
}

/// Result of automatic delivery admission; a skipped message remains in history.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InboxEnsureOutcome {
    Ready,
    SkippedArchived,
}

/// Ensures durable inbox custody for a recipient selected from effect-time live
/// participation. Membership and task aftermath deliberately use this fresh
/// gate so a recipient revoked after commit receives nothing.
pub trait LiveMessageInboxEnsure: Clone + Send + Sync + 'static {
    fn ensure_for_live_recipient(
        &self,
        conversation: &ConversationId,
        agent: &AgentId,
        message: &MessageId,
    ) -> impl Future<Output = Result<InboxEnsureOutcome, EffectFailure>> + Send;
}

/// Ensures durable inbox custody for a recipient already sealed by the commit.
/// Rename witnesses own their recipient set; schedule writers own their exact
/// inbox row. Neither may be re-authorized against mutable post-commit roster
/// state before dispatch.
pub trait SealedMessageInboxEnsure: Clone + Send + Sync + 'static {
    fn ensure_for_sealed_recipient(
        &self,
        conversation: &ConversationId,
        agent: &AgentId,
        message: &MessageId,
    ) -> impl Future<Output = Result<InboxEnsureOutcome, EffectFailure>> + Send;
}

/// The durable record that a person was named in a message they can see.
///
/// This is a WRITE, after the send committed and in a transaction of its own, so it
/// has its own custody: if it fails the message stands and stays published, and the
/// named person simply has no directed item in their badge. Nothing already
/// committed may be undone for it, and no later attempt may be skipped because of
/// it.
pub trait MentionedViewerRecord {
    /// Resolves the intent's handles against the CURRENT state — who those names
    /// belong to now, and who may read that conversation now — excludes the author,
    /// and writes a row for each remaining reader in a transaction of its own.
    ///
    /// Entitlement is decided here rather than carried in, so someone removed between
    /// the commit and this write receives nothing. Returns the rows that were actually
    /// written, which may be none.
    fn record_mentioned_viewers(
        &self,
        intent: &MentionIntent,
    ) -> impl Future<Output = Result<Vec<CommittedMentionRow>, EffectFailure>> + Send;
}

/// Tells the named person's client that the row above exists. It takes the committed
/// row, because "the row is there but nobody was told" is a state that has to be
/// expressible: a capability answering only "did both halves work" would lose it.
pub trait ViewerNotification {
    fn notify_viewer(
        &self,
        mention: &CommittedMentionRow,
    ) -> impl Future<Output = Result<(), EffectFailure>> + Send;
}

/// The message, published to the conversation it landed in.
///
/// Path-neutral: a reply carries its cluster projections on the same frame and a
/// main-line send carries none, but both hand the adapter the same
/// [`CommittedMessageView`] — there is one published frame, not one per path. The
/// acknowledgement projects from that same view.
pub trait CommittedMessagePublication {
    fn publish_message(
        &self,
        committed: CommittedMessageView<'_>,
    ) -> impl Future<Output = Result<(), EffectFailure>> + Send;
}

/// Everyone's unread state after this message. It reads the mention rows, which is
/// why it cannot run before they are written.
pub trait UnreadPublication {
    fn publish_unread(
        &self,
        conversation: &ConversationId,
    ) -> impl Future<Output = Result<(), EffectFailure>> + Send;
}

/// The conversation's latest-message summary after this send, to every human
/// participant including the sender.
pub trait LatestPublication {
    fn publish_latest(
        &self,
        latest: &super::collaboration::conversation_latest::ConversationLatest,
    ) -> impl Future<Output = Result<(), EffectFailure>> + Send;
}

/// The per-reader alerts this message owes, decided by
/// [`alert_reason`](super::alert::alert_reason) over the committed participants.
/// `named` is the human handles the body addressed.
pub trait AlertPublication {
    fn publish_alerts(
        &self,
        message: &CreatedMessage,
        named: &[ParticipantHandle],
    ) -> impl Future<Output = Result<(), EffectFailure>> + Send;
}

/// The message is owed to one recipient. Per recipient, from the plan, so an
/// adapter has nobody to look up and no policy left to re-decide.
pub trait MainDelivery {
    fn deliver_to(
        &self,
        recipient: &AgentId,
        message: &CreatedMessage,
    ) -> impl Future<Output = Result<(), EffectFailure>> + Send;
}

/// The reply is owed to one follower of its cluster. A separate capability from
/// [`MainDelivery`], because the plan is a different one: the recipients are the
/// cluster's followers, and every one of them is woken (a turn), not the
/// conversation's participants served by the main-line fan-out. Per recipient, from
/// the audience, so an adapter has nobody to look up and no policy left to re-decide.
pub trait ReplyDelivery {
    fn deliver_to(
        &self,
        recipient: &AgentId,
        message: &CreatedMessage,
    ) -> impl Future<Output = Result<(), EffectFailure>> + Send;
}

/// A recipient that is not running is started afterwards. Delivery does not depend
/// on it: an idle runtime is started because nobody is there to receive, not in
/// order to receive.
pub trait IdleStart {
    /// Asked of one eligible recipient. Whether it is idle is decided HERE, at the
    /// moment of asking, because it is a volatile fact.
    fn start_if_idle(
        &self,
        recipient: &AgentId,
        conversation: &ConversationId,
    ) -> impl Future<Output = Result<(), EffectFailure>> + Send;
}

/// An addressed agent that got no copy is a fact worth keeping: "nothing was
/// copied" alone cannot distinguish a member from a stranger.
pub trait MentionSkipRecord {
    fn record_skip(
        &self,
        skipped: &SkippedMention<'_>,
    ) -> impl Future<Output = Result<(), EffectFailure>> + Send;
}

#[derive(Clone, Copy, Debug)]
pub struct SkippedMention<'facts> {
    pub handle: &'facts ParticipantHandle,
    pub reason: MentionCopySkip,
    pub agent: Option<&'facts AgentId>,
    pub source_conversation: &'facts ConversationId,
    pub source_message: &'facts MessageId,
}

/// What was attempted and what did not work. Not a `Result`: the send happened, and
/// no attempt here can unhappen it.
#[derive(Debug, Default)]
pub struct EffectReport {
    pub attempted: Vec<(MessageId, EffectStep)>,
    pub failed: Vec<(MessageId, EffectStep, EffectFailure)>,
}

/// The capabilities one send's aftermath needs, held together only for the call.
/// Each is its own type parameter, so an implementation may combine them and this
/// orchestration still cannot make one attempt depend on another.
pub struct SendEffects<'ports, I, P, N, S, D, R> {
    pub direct_lines: &'ports I,
    pub publication: &'ports P,
    pub notification: &'ports N,
    pub start: &'ports S,
    pub delivery: &'ports D,
    pub skips: &'ports R,
}

/// Tells the world about a committed send.
///
/// Every attempt is made, in order, whatever the ones before it did: they answer
/// different questions and none of them supplies another. Per copy, because the
/// order that matters is the order within one copy's own story — the line exists,
/// the copy is published, the author is told, the recipient is started, the copy is
/// delivered.
pub async fn announce_send<I, P, N, S, D, R>(
    fact: &MessageCreatedFact,
    ports: SendEffects<'_, I, P, N, S, D, R>,
) -> EffectReport
where
    I: DirectLineInvalidation,
    P: MessagePublication,
    N: CopyNotification,
    S: RuntimeStart,
    D: RuntimeDelivery,
    R: MentionSkipRecord,
{
    let mut report = EffectReport::default();
    let mut announced_lines = HashSet::new();
    if let Some(author) = &fact.message.from_user_id {
        // Only a person's mention is copied. Pair-open facts are independent of
        // the sender, though, and are covered below even when there is no author.
        for outcome in &fact.mention_copies {
            match outcome {
                MentionCopyOutcome::Copied {
                    agent,
                    copy,
                    handle: _,
                } => {
                    let id = copy.id.clone();
                    attempt(
                        &mut report,
                        &id,
                        EffectStep::DirectLineInvalidation,
                        ports
                            .direct_lines
                            .direct_lines_changed(author, &copy.conversation_id),
                    )
                    .await;
                    announced_lines.insert(copy.conversation_id.clone());
                    attempt(
                        &mut report,
                        &id,
                        EffectStep::CopyPublished,
                        ports.publication.publish_copy(copy),
                    )
                    .await;
                    attempt(
                        &mut report,
                        &id,
                        EffectStep::AuthorNotified,
                        ports.notification.notify_author(author, agent, copy),
                    )
                    .await;
                    attempt(
                        &mut report,
                        &id,
                        EffectStep::RuntimeStarted,
                        ports.start.start(&copy.conversation_id, agent),
                    )
                    .await;
                    attempt(
                        &mut report,
                        &id,
                        EffectStep::CopyDelivered,
                        ports.delivery.deliver(agent, copy),
                    )
                    .await;
                }
                MentionCopyOutcome::Skipped {
                    handle,
                    reason,
                    agent,
                } => {
                    attempt(
                        &mut report,
                        &fact.message.id,
                        EffectStep::SkipRecorded,
                        ports.skips.record_skip(&SkippedMention {
                            handle,
                            reason: *reason,
                            agent: agent.as_ref(),
                            source_conversation: &fact.message.conversation_id,
                            source_message: &fact.message.id,
                        }),
                    )
                    .await;
                }
            }
        }
    }

    // A transaction-aware mint records the rows it created instead of publishing
    // before commit. A mention copy already invalidated its line above; any future
    // transactional creator that did not also create a copy is still covered here.
    for opened in fact.post_commit.opened_direct_pairs() {
        if announced_lines.contains(opened.conversation()) {
            continue;
        }
        for participant in opened.participants().both() {
            let PrincipalIdentity::Human(viewer) = participant else {
                continue;
            };
            attempt(
                &mut report,
                &fact.message.id,
                EffectStep::DirectLineInvalidation,
                ports
                    .direct_lines
                    .direct_lines_changed(viewer, opened.conversation()),
            )
            .await;
        }
    }
    report
}

/// The capabilities a main-line send's aftermath needs.
pub struct MainEffects<'ports, P, R, V, U, D, S> {
    pub publication: &'ports P,
    pub mentions: &'ports R,
    pub viewers: &'ports V,
    pub unread: &'ports U,
    pub delivery: &'ports D,
    pub idle: &'ports S,
}

/// Tells the world about a committed main-line send.
///
/// It takes the view, not the fact, so there is no way to hand it a reply.
///
/// Stated as the type relation, with a local stand-in for this function's own parameter. The
/// earlier version called the real announcer with `MainEffects<'_, (), …>`, and unit implements
/// none of the effect traits — so the example failed on those bounds no matter which view it
/// was given, and would have stayed green with the views made interchangeable.
/// What the gate must isolate is the ARGUMENT's type and nothing else.
///
/// ```compile_fail
/// use silverhand_backend_core::conversation_messaging::messaging::{MainCommittedSend, MessageCreatedFact};
///
/// fn main_input(_: MainCommittedSend<'_>) {}
///
/// fn announce(fact: &MessageCreatedFact) {
///     main_input(fact.reply_cluster().expect("a reply"));
/// }
/// ```
///
/// The positive neighbour, which is what makes that failure meaningful: the same parameter
/// accepts this path's own view.
///
/// ```
/// use silverhand_backend_core::conversation_messaging::messaging::{MainCommittedSend, MessageCreatedFact};
///
/// fn main_input(_: MainCommittedSend<'_>) {}
///
/// fn announce(fact: &MessageCreatedFact) {
///     if let Some(view) = fact.main_line() {
///         main_input(view);
///     }
/// }
/// ```
///
/// The order is load-bearing in one place and one place only: a named person's
/// durable mention row is written before unread state is published, because the
/// badge's directed count READS that row — publishing first sends a value that
/// predates the mention, and the client does not ask again. Everything else is
/// ordered for readability, and each attempt is isolated: per named viewer and per
/// recipient, so one person's failure does not cost another theirs.
pub async fn announce_main_send<P, R, V, U, D, S>(
    send: MainCommittedSend<'_>,
    ports: MainEffects<'_, P, R, V, U, D, S>,
) -> EffectReport
where
    P: CommittedMessagePublication,
    R: MentionedViewerRecord,
    V: ViewerNotification,
    U: UnreadPublication,
    D: MainDelivery,
    S: IdleStart,
{
    let mut report = EffectReport::default();
    let message = send.message();
    let delivery = send.delivery();
    let id = message.id.clone();
    let conversation = message.conversation_id.clone();

    announce_publication_prefix(
        &mut report,
        send.published(),
        send.mentions(),
        ports.publication,
        ports.mentions,
        ports.viewers,
        ports.unread,
    )
    .await;

    for recipient in delivery.recipients() {
        attempt(
            &mut report,
            &id,
            EffectStep::MainDelivered,
            ports.delivery.deliver_to(recipient, message),
        )
        .await;
    }
    // After delivery, and only for those the plan allows: a runtime is started
    // because nobody was there to receive, not in order to receive.
    for recipient in delivery.start_eligible() {
        attempt(
            &mut report,
            &id,
            EffectStep::IdleStarted,
            ports.idle.start_if_idle(recipient, &conversation),
        )
        .await;
    }
    report
}

/// The capabilities a reply's aftermath needs.
///
/// The same shape as [`MainEffects`] up to one difference that is the whole point:
/// there is no idle start. A reply's end is its followers being woken, never the
/// conversation being started; and delivery is [`ReplyDelivery`], not
/// [`MainDelivery`], because the plan is the cluster's followers rather than the
/// conversation's participants.
pub struct ReplyEffects<'ports, P, R, V, U, D> {
    pub publication: &'ports P,
    pub mentions: &'ports R,
    pub viewers: &'ports V,
    pub unread: &'ports U,
    pub delivery: &'ports D,
}

/// Tells the world about a committed reply.
///
/// It takes the reply view, not the fact, so there is no way to hand it a main-line send.
///
/// The mirror of [`announce_main_send`]'s gate, and stated the same way for the same reason:
/// calling the real announcer with unit ports would fail on the effect bounds rather than on
/// the view, which is a gate that cannot notice the views becoming interchangeable.
///
/// ```compile_fail
/// use silverhand_backend_core::conversation_messaging::messaging::{MessageCreatedFact, ReplyCommittedSend};
///
/// fn reply_input(_: ReplyCommittedSend<'_>) {}
///
/// fn announce(fact: &MessageCreatedFact) {
///     reply_input(fact.main_line().expect("a main-line send"));
/// }
/// ```
///
/// And its positive neighbour:
///
/// ```
/// use silverhand_backend_core::conversation_messaging::messaging::{MessageCreatedFact, ReplyCommittedSend};
///
/// fn reply_input(_: ReplyCommittedSend<'_>) {}
///
/// fn announce(fact: &MessageCreatedFact) {
///     if let Some(view) = fact.reply_cluster() {
///         reply_input(view);
///     }
/// }
/// ```
///
/// The one load-bearing ordering is shared with the main line: a named person's
/// durable mention row is written before unread is published, because the badge's
/// directed count reads that row. What differs is the end: delivery reaches the
/// cluster's followers and stops — no main-line fan-out, and no idle start. Each
/// attempt is isolated per named viewer and per recipient.
pub async fn announce_reply_send<P, R, V, U, D>(
    send: ReplyCommittedSend<'_>,
    ports: ReplyEffects<'_, P, R, V, U, D>,
) -> EffectReport
where
    P: CommittedMessagePublication,
    R: MentionedViewerRecord,
    V: ViewerNotification,
    U: UnreadPublication,
    D: ReplyDelivery,
{
    let mut report = EffectReport::default();
    let message = send.message();
    let id = message.id.clone();

    announce_publication_prefix(
        &mut report,
        send.published(),
        send.mentions(),
        ports.publication,
        ports.mentions,
        ports.viewers,
        ports.unread,
    )
    .await;

    // The reply's own plan: the cluster's followers, each woken. No main-line
    // fan-out and no idle start follow — that divergence is why this is not
    // `announce_main_send`.
    for recipient in send.audience().recipients() {
        attempt(
            &mut report,
            &id,
            EffectStep::ReplyDelivered,
            ports.delivery.deliver_to(recipient, message),
        )
        .await;
    }
    report
}

/// The publication prefix both paths share, verbatim: publish the message, then —
/// for a named send — write each durable mention row before notifying its viewer,
/// then publish unread. It is one helper rather than two copies because the order is
/// load-bearing and identical: a named person's row is written before unread is
/// published, since the badge's directed count reads that row. Neither public
/// announcer may drift from the other on this prefix; each only appends its own
/// delivery after it. This is deliberately not a wide effects service locator — it
/// owns exactly the shared prefix and nothing path-specific.
async fn announce_publication_prefix<P, R, V, U>(
    report: &mut EffectReport,
    published: CommittedMessageView<'_>,
    mentions: Option<&MentionIntent>,
    publication: &P,
    mention_record: &R,
    viewers: &V,
    unread: &U,
) where
    P: CommittedMessagePublication,
    R: MentionedViewerRecord,
    V: ViewerNotification,
    U: UnreadPublication,
{
    let message = published.message();
    let id = message.id.clone();
    let conversation = message.conversation_id.clone();

    attempt(
        report,
        &id,
        EffectStep::MessagePublished,
        publication.publish_message(published),
    )
    .await;

    if let Some(intent) = mentions {
        report
            .attempted
            .push((id.clone(), EffectStep::MentionedViewerRecorded));
        match mention_record.record_mentioned_viewers(intent).await {
            // Only rows that were written are published, and one person's publication
            // failing does not reach the next person's.
            Ok(rows) => {
                for row in &rows {
                    attempt(
                        report,
                        &id,
                        EffectStep::ViewerNotified,
                        viewers.notify_viewer(row),
                    )
                    .await;
                }
            }
            Err(failure) => {
                report
                    .failed
                    .push((id.clone(), EffectStep::MentionedViewerRecorded, failure));
            }
        }
    }

    attempt(
        report,
        &id,
        EffectStep::UnreadPublished,
        unread.publish_unread(&conversation),
    )
    .await;
}

async fn attempt(
    report: &mut EffectReport,
    message: &MessageId,
    step: EffectStep,
    effect: impl Future<Output = Result<(), EffectFailure>>,
) {
    report.attempted.push((message.clone(), step));
    if let Err(failure) = effect.await {
        report.failed.push((message.clone(), step, failure));
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, sync::Mutex};

    use super::*;
    use crate::conversation_messaging::messaging::{
        CreatedMessage, FromKind, MessageContent, MessageSeq,
        mention_cc::{MENTION_COPY_SUBTYPE, MentionCopyMetadata},
    };

    impl EffectReport {
        pub fn steps_for(&self, message: &MessageId) -> Vec<EffectStep> {
            self.attempted
                .iter()
                .filter(|(id, _)| id == message)
                .map(|(_, step)| *step)
                .collect()
        }

        pub fn failed_steps(&self) -> Vec<EffectStep> {
            self.failed.iter().map(|(_, step, _)| *step).collect()
        }
    }

    #[derive(Debug, thiserror::Error)]
    #[error("this effect was told to fail")]
    struct Told;

    /// One port set that records every call in order and fails the steps it was
    /// told to. Recording through a shared cell keeps the trait signatures honest:
    /// the ports take `&self`, because an attempt is not allowed to hold the
    /// orchestration open.
    #[derive(Default)]
    struct Recorder {
        calls: Mutex<Vec<(String, EffectStep)>>,
        failing: HashSet<EffectStep>,
        /// Individual call keys that fail, whatever their step. Lets a test fail one
        /// recipient's delivery while its neighbour's still succeeds — the falsifier
        /// for per-recipient (not merely per-step) isolation.
        failing_keys: HashSet<String>,
        /// Which named handles the store still finds entitled when it writes. Empty
        /// means everyone lost access between the commit and the write.
        entitled: HashSet<String>,
    }

    impl Recorder {
        fn failing(steps: &[EffectStep]) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                failing: steps.iter().copied().collect(),
                failing_keys: HashSet::new(),
                entitled: HashSet::from(["viewer-1".to_owned(), "viewer-2".to_owned()]),
            }
        }

        fn entitling(handles: &[&str]) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                failing: HashSet::new(),
                failing_keys: HashSet::new(),
                entitled: handles.iter().map(|handle| (*handle).to_owned()).collect(),
            }
        }

        /// The recorder from [`entitling`](Recorder::entitling) with everyone
        /// entitled, but one specific call key told to fail.
        fn failing_one_key(key: &str) -> Self {
            let mut recorder = Self::entitling(&["viewer-1", "viewer-2"]);
            recorder.failing_keys.insert(key.to_owned());
            recorder
        }

        fn answer(&self, id: &str, step: EffectStep) -> Result<(), EffectFailure> {
            self.calls.lock().unwrap().push((id.to_owned(), step));
            if self.failing.contains(&step) || self.failing_keys.contains(id) {
                return Err(EffectFailure::from_source(Told));
            }
            Ok(())
        }

        fn steps_of(&self, id: &str) -> Vec<EffectStep> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(called, _)| called == id)
                .map(|(_, step)| *step)
                .collect()
        }

        fn called(&self, step: EffectStep) -> bool {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .any(|(_, called)| *called == step)
        }
    }

    impl DirectLineInvalidation for Recorder {
        async fn direct_lines_changed(
            &self,
            author: &UserId,
            line: &ConversationId,
        ) -> Result<(), EffectFailure> {
            self.answer(
                &format!("{}|{}", author.as_str(), line.as_str()),
                EffectStep::DirectLineInvalidation,
            )
        }
    }

    impl MessagePublication for Recorder {
        async fn publish_copy(&self, copy: &CommittedMentionCopy) -> Result<(), EffectFailure> {
            self.answer(copy.id.as_str(), EffectStep::CopyPublished)
        }
    }

    impl CopyNotification for Recorder {
        async fn notify_author(
            &self,
            _author: &UserId,
            _agent: &AgentId,
            copy: &CommittedMentionCopy,
        ) -> Result<(), EffectFailure> {
            self.answer(copy.id.as_str(), EffectStep::AuthorNotified)
        }
    }

    impl RuntimeStart for Recorder {
        async fn start(
            &self,
            conversation: &ConversationId,
            agent: &AgentId,
        ) -> Result<(), EffectFailure> {
            self.answer(
                &format!("{}|{}", conversation.as_str(), agent.as_str()),
                EffectStep::RuntimeStarted,
            )
        }
    }

    impl RuntimeDelivery for Recorder {
        async fn deliver(
            &self,
            _agent: &AgentId,
            copy: &CommittedMentionCopy,
        ) -> Result<(), EffectFailure> {
            self.answer(copy.id.as_str(), EffectStep::CopyDelivered)
        }
    }

    impl MentionSkipRecord for Recorder {
        async fn record_skip(&self, skipped: &SkippedMention<'_>) -> Result<(), EffectFailure> {
            self.answer(skipped.handle.as_str(), EffectStep::SkipRecorded)
        }
    }

    fn ports(
        recorder: &Recorder,
    ) -> SendEffects<'_, Recorder, Recorder, Recorder, Recorder, Recorder, Recorder> {
        SendEffects {
            direct_lines: recorder,
            publication: recorder,
            notification: recorder,
            start: recorder,
            delivery: recorder,
            skips: recorder,
        }
    }

    fn copy(id: &str, line: &str) -> CommittedMentionCopy {
        CommittedMentionCopy {
            id: MessageId::parse(id).unwrap(),
            conversation_id: ConversationId::parse(line).unwrap(),
            from_kind: FromKind::System,
            content: "copied".to_owned(),
            metadata: MentionCopyMetadata {
                subtype: MENTION_COPY_SUBTYPE,
                source_workspace_id: WorkspaceId::parse("ws-1").unwrap(),
                source_workspace_name: "Space".to_owned(),
                source_conversation_id: ConversationId::parse("conv-1").unwrap(),
                source_conversation_title: "Channel".to_owned(),
                source_message_id: MessageId::parse("source-message").unwrap(),
                source_from_kind: FromKind::User,
                source_from_user_id: UserId::parse("user-1").unwrap(),
                source_message_content: "ping".to_owned(),
            },
            seq: MessageSeq::try_from(1).unwrap(),
            created_at_ms: 7,
            cc_source_message_id: MessageId::parse("source-message").unwrap(),
        }
    }

    fn intent_for(handles: &[&str]) -> MentionIntent {
        MentionIntent::committed(
            handles
                .iter()
                .map(|handle| ParticipantHandle::parse(*handle).unwrap())
                .collect(),
            Some(UserId::parse("author-1").unwrap()),
            "Sender".to_owned(),
            WorkspaceId::parse("ws-1").unwrap(),
            ConversationId::parse("conv-1").unwrap(),
            "Channel".to_owned(),
            MessageId::parse("source-message").unwrap(),
            "ping".to_owned(),
        )
    }

    fn main_plan(viewers: &[&str], recipients: &[&str]) -> PostCommitPlan {
        let recipients: Vec<AgentId> = recipients
            .iter()
            .map(|id| AgentId::parse(*id).unwrap())
            .collect();
        PostCommitPlan::committed(
            if viewers.is_empty() {
                None
            } else {
                Some(intent_for(viewers))
            },
            DeliveryPlan::MainLine(MainDeliveryPlan::committed(recipients.clone(), recipients)),
            Vec::new(),
        )
    }

    fn fact_with(outcomes: Vec<MentionCopyOutcome>, plan: PostCommitPlan) -> MessageCreatedFact {
        MessageCreatedFact::committed(created_message(), None, plan, false, outcomes)
    }

    fn created_message() -> CreatedMessage {
        CreatedMessage {
            client_message_id: None,
            id: MessageId::parse("source-message").unwrap(),
            conversation_id: ConversationId::parse("conv-1").unwrap(),
            from_kind: FromKind::User,
            from_agent_id: None,
            from_user_id: Some(UserId::parse("user-1").unwrap()),
            author: None,
            content: MessageContent::parse("ping").unwrap(),
            attachments: Vec::new(),
            seq: MessageSeq::try_from(1).unwrap(),
            created_at_ms: 7,
            thread_root_id: None,
            options: None,
        }
    }

    fn fact(outcomes: Vec<MentionCopyOutcome>) -> MessageCreatedFact {
        MessageCreatedFact::committed(
            created_message(),
            None,
            PostCommitPlan::committed(
                None,
                DeliveryPlan::MainLine(MainDeliveryPlan::committed(Vec::new(), Vec::new())),
                Vec::new(),
            ),
            false,
            outcomes,
        )
    }

    /// A committed reply projection for the order/routing tests: these care only that
    /// the fact is a reply carrying *some* valid entry, so the entry is a minimal stub
    /// whose only asserted field is `reply_count`.
    fn reply_projection(
        root: &str,
        reply_count: i64,
    ) -> crate::conversation_messaging::messaging::ThreadReplyFact {
        use crate::conversation_messaging::messaging::{MessageId, ThreadListEntry, ThreadRootId};
        let entry = ThreadListEntry::committed(
            ThreadRootId::parse(root).unwrap(),
            None,
            "root body",
            0,
            MessageSeq::try_from(1).unwrap(),
            reply_count,
            0,
            MessageId::parse("reply-1").unwrap(),
            Vec::new(),
            0,
        );
        crate::conversation_messaging::messaging::ThreadReplyFact::committed(entry)
    }

    fn copied(id: &str, line: &str, agent: &str, handle: &str) -> MentionCopyOutcome {
        MentionCopyOutcome::Copied {
            handle: ParticipantHandle::parse(handle).unwrap(),
            agent: AgentId::parse(agent).unwrap(),
            copy: Box::new(copy(id, line)),
        }
    }

    const IN_ORDER: [EffectStep; 5] = [
        EffectStep::DirectLineInvalidation,
        EffectStep::CopyPublished,
        EffectStep::AuthorNotified,
        EffectStep::RuntimeStarted,
        EffectStep::CopyDelivered,
    ];

    #[tokio::test]
    async fn one_copy_is_announced_in_order() {
        let recorder = default_recorder();

        let report = announce_send(
            &fact(vec![copied("copy-1", "line-1", "agent-1", "Alice")]),
            ports(&recorder),
        )
        .await;

        assert_eq!(
            report.steps_for(&MessageId::parse("copy-1").unwrap()),
            IN_ORDER
        );
        assert!(report.failed.is_empty());
    }

    #[tokio::test]
    async fn a_transaction_recorded_pair_is_covered_even_without_a_copy() {
        let recorder = default_recorder();
        let participants =
            crate::conversation_messaging::messaging::DirectPairParticipants::between(
                PrincipalIdentity::Human(UserId::parse("user-1").unwrap()),
                PrincipalIdentity::Agent(AgentId::parse("agent-1").unwrap()),
            )
            .unwrap();
        let opened = DirectPairOpenedFact::committed(
            ConversationId::parse("line-new").unwrap(),
            participants,
        );
        let plan = PostCommitPlan::committed(
            None,
            DeliveryPlan::MainLine(MainDeliveryPlan::committed(Vec::new(), Vec::new())),
            vec![opened],
        );

        let report = announce_send(&fact_with(Vec::new(), plan), ports(&recorder)).await;

        assert_eq!(
            recorder.steps_of("user-1|line-new"),
            vec![EffectStep::DirectLineInvalidation]
        );
        assert_eq!(
            report.steps_for(&MessageId::parse("source-message").unwrap()),
            vec![EffectStep::DirectLineInvalidation]
        );
        assert!(report.failed.is_empty());
    }

    /// Every step is attempted whatever the ones before it did: they answer
    /// different questions, and none of them supplies another.
    #[tokio::test]
    async fn any_single_failure_leaves_the_rest_attempted() {
        for failing in IN_ORDER {
            let recorder = Recorder::failing(&[failing]);

            let report = announce_send(
                &fact(vec![copied("copy-1", "line-1", "agent-1", "Alice")]),
                ports(&recorder),
            )
            .await;

            assert_eq!(
                report.steps_for(&MessageId::parse("copy-1").unwrap()),
                IN_ORDER,
                "{failing:?} truncated the announcement"
            );
            assert_eq!(report.failed_steps(), vec![failing]);
            assert!(recorder.called(EffectStep::CopyDelivered));
        }
    }

    /// A failure is only actionable with its reason, so the cause survives the
    /// boundary rather than being flattened into "an effect failed".
    #[tokio::test]
    async fn a_failure_keeps_the_cause_it_was_given() {
        let recorder = Recorder::failing(&[EffectStep::CopyPublished]);

        let report = announce_send(
            &fact(vec![copied("copy-1", "line-1", "agent-1", "Alice")]),
            ports(&recorder),
        )
        .await;

        let (_, step, failure) = &report.failed[0];
        assert_eq!(*step, EffectStep::CopyPublished);
        assert_eq!(
            std::error::Error::source(failure).unwrap().to_string(),
            "this effect was told to fail"
        );
    }

    #[tokio::test]
    async fn every_failure_at_once_still_leaves_the_send_a_fact() {
        let recorder = Recorder::failing(&IN_ORDER);
        let committed = fact(vec![copied("copy-1", "line-1", "agent-1", "Alice")]);

        let report = announce_send(&committed, ports(&recorder)).await;

        assert_eq!(report.failed_steps(), IN_ORDER);
        // The fact is untouched: it is an argument, not an outcome, and nothing here
        // returns a failure the caller could mistake for the send failing.
        assert_eq!(committed.message.id.as_str(), "source-message");
        assert_eq!(committed.mention_copies.len(), 1);
    }

    /// A copy that did not happen is still reported, and its failure to be recorded
    /// does not touch the copies that did.
    #[tokio::test]
    async fn a_skipped_mention_is_recorded_and_its_failure_is_contained() {
        let recorder = Recorder::failing(&[EffectStep::SkipRecorded]);

        let report = announce_send(
            &fact(vec![
                MentionCopyOutcome::Skipped {
                    handle: ParticipantHandle::parse("Bob").unwrap(),
                    reason: MentionCopySkip::AlreadyMember,
                    agent: Some(AgentId::parse("agent-2").unwrap()),
                },
                copied("copy-1", "line-1", "agent-1", "Alice"),
            ]),
            ports(&recorder),
        )
        .await;

        assert!(recorder.called(EffectStep::SkipRecorded));
        assert_eq!(report.failed_steps(), vec![EffectStep::SkipRecorded]);
        assert_eq!(
            report.steps_for(&MessageId::parse("copy-1").unwrap()),
            IN_ORDER
        );
    }

    /// With more than one copy, the order that matters is the order within each
    /// copy's own story — and a step failing for one does not disturb another's.
    #[tokio::test]
    async fn each_copy_keeps_its_own_order() {
        let recorder = Recorder::failing(&[EffectStep::CopyPublished]);

        announce_send(
            &fact(vec![
                copied("copy-1", "line-1", "agent-1", "Alice"),
                copied("copy-2", "line-2", "agent-2", "Bob"),
            ]),
            ports(&recorder),
        )
        .await;

        // Each copy's own steps, in order. The line and the agent are recorded under
        // their own identities, so the per-copy sequence is the message's.
        for id in ["copy-1", "copy-2"] {
            assert_eq!(
                recorder.steps_of(id),
                vec![
                    EffectStep::CopyPublished,
                    EffectStep::AuthorNotified,
                    EffectStep::CopyDelivered
                ],
                "copy {id} lost its order"
            );
        }
        // Both arguments of each call are pinned: the author's view is invalidated
        // for the line the copy landed on, and the runtime is located by that same
        // line together with the agent.
        for line in ["line-1", "line-2"] {
            assert_eq!(
                recorder.steps_of(&format!("user-1|{line}")),
                vec![EffectStep::DirectLineInvalidation]
            );
        }
        for (line, agent) in [("line-1", "agent-1"), ("line-2", "agent-2")] {
            assert_eq!(
                recorder.steps_of(&format!("{line}|{agent}")),
                vec![EffectStep::RuntimeStarted]
            );
        }
    }

    impl CommittedMessagePublication for Recorder {
        async fn publish_message(
            &self,
            committed: CommittedMessageView<'_>,
        ) -> Result<(), EffectFailure> {
            self.answer(
                committed.message().id.as_str(),
                EffectStep::MessagePublished,
            )
        }
    }

    impl MentionedViewerRecord for Recorder {
        async fn record_mentioned_viewers(
            &self,
            intent: &MentionIntent,
        ) -> Result<Vec<CommittedMentionRow>, EffectFailure> {
            self.answer(
                intent.source_message().as_str(),
                EffectStep::MentionedViewerRecorded,
            )?;
            // What a store does: decide entitlement NOW, then mint an identity and
            // stamp a moment per row it wrote. `entitled` stands in for that decision.
            Ok(intent
                .handles()
                .iter()
                .filter(|handle| self.entitled.contains(handle.as_str()))
                .map(|handle| {
                    CommittedMentionRow::committed(
                        NotificationId::parse(format!("row-{}", handle.as_str())).unwrap(),
                        11,
                        UserId::parse(format!("user-{}", handle.as_str())).unwrap(),
                        handle.clone(),
                        intent.clone(),
                    )
                })
                .collect())
        }
    }

    impl ViewerNotification for Recorder {
        async fn notify_viewer(&self, mention: &CommittedMentionRow) -> Result<(), EffectFailure> {
            self.answer(
                &format!(
                    "{}|{}",
                    mention.viewer().as_str(),
                    mention.intent().source_message().as_str()
                ),
                EffectStep::ViewerNotified,
            )
        }
    }

    impl UnreadPublication for Recorder {
        async fn publish_unread(&self, conversation: &ConversationId) -> Result<(), EffectFailure> {
            self.answer(conversation.as_str(), EffectStep::UnreadPublished)
        }
    }

    impl MainDelivery for Recorder {
        async fn deliver_to(
            &self,
            recipient: &AgentId,
            message: &CreatedMessage,
        ) -> Result<(), EffectFailure> {
            self.answer(
                &format!("{}|{}", recipient.as_str(), message.id.as_str()),
                EffectStep::MainDelivered,
            )
        }
    }

    impl IdleStart for Recorder {
        async fn start_if_idle(
            &self,
            recipient: &AgentId,
            conversation: &ConversationId,
        ) -> Result<(), EffectFailure> {
            self.answer(
                &format!("{}|{}", recipient.as_str(), conversation.as_str()),
                EffectStep::IdleStarted,
            )
        }
    }

    impl ReplyDelivery for Recorder {
        async fn deliver_to(
            &self,
            recipient: &AgentId,
            message: &CreatedMessage,
        ) -> Result<(), EffectFailure> {
            self.answer(
                &format!("{}|{}", recipient.as_str(), message.id.as_str()),
                EffectStep::ReplyDelivered,
            )
        }
    }

    fn default_recorder() -> Recorder {
        Recorder::entitling(&["viewer-1", "viewer-2"])
    }

    fn main_ports(
        recorder: &Recorder,
    ) -> MainEffects<'_, Recorder, Recorder, Recorder, Recorder, Recorder, Recorder> {
        MainEffects {
            publication: recorder,
            mentions: recorder,
            viewers: recorder,
            unread: recorder,
            delivery: recorder,
            idle: recorder,
        }
    }

    fn reply_ports(
        recorder: &Recorder,
    ) -> ReplyEffects<'_, Recorder, Recorder, Recorder, Recorder, Recorder> {
        ReplyEffects {
            publication: recorder,
            mentions: recorder,
            viewers: recorder,
            unread: recorder,
            delivery: recorder,
        }
    }

    /// A committed reply: a cluster projection, a follower audience, and — for the
    /// mention prefix — an optional intent. Its message is the same `source-message`
    /// the main-line helpers use, so the two announcers can be compared on one id.
    fn reply_fact(followers: &[&str], viewers: &[&str]) -> MessageCreatedFact {
        let followers: Vec<AgentId> = followers
            .iter()
            .map(|id| AgentId::parse(*id).unwrap())
            .collect();
        MessageCreatedFact {
            thread: Some(reply_projection("root-1", 2)),
            post_commit: PostCommitPlan::committed(
                if viewers.is_empty() {
                    None
                } else {
                    Some(intent_for(viewers))
                },
                DeliveryPlan::ReplyCluster(ReplyAudience::committed(followers)),
                Vec::new(),
            ),
            ..fact(Vec::new())
        }
    }

    /// The main line's own order, ending in delivery and THEN the conditional start
    /// of whoever was idle — delivery does not wait on a runtime, and a runtime is
    /// started because nobody was there to receive.
    #[tokio::test]
    async fn a_main_line_send_is_announced_in_its_own_order() {
        let recorder = default_recorder();

        let committed = fact_with(Vec::new(), main_plan(&["viewer-1"], &["agent-1"]));

        let report = announce_main_send(
            committed.main_line().expect("a main-line send"),
            main_ports(&recorder),
        )
        .await;

        assert_eq!(
            report.steps_for(&MessageId::parse("source-message").unwrap()),
            vec![
                EffectStep::MessagePublished,
                EffectStep::MentionedViewerRecorded,
                EffectStep::ViewerNotified,
                EffectStep::UnreadPublished,
                EffectStep::MainDelivered,
                EffectStep::IdleStarted,
            ]
        );
        assert!(report.failed.is_empty());
    }

    /// The row is written before unread is published, because the badge's directed
    /// count reads it. Anything after a failure still runs.
    #[tokio::test]
    async fn unread_is_published_after_the_mention_row_is_attempted() {
        for failing in [
            EffectStep::MessagePublished,
            EffectStep::MentionedViewerRecorded,
            EffectStep::ViewerNotified,
            EffectStep::UnreadPublished,
            EffectStep::MainDelivered,
            EffectStep::IdleStarted,
        ] {
            let recorder = Recorder::failing(&[failing]);

            let committed = fact_with(Vec::new(), main_plan(&["viewer-1"], &["agent-1"]));
            let report = announce_main_send(
                committed.main_line().expect("a main-line send"),
                main_ports(&recorder),
            )
            .await;

            let steps = report.steps_for(&MessageId::parse("source-message").unwrap());
            let recorded = steps
                .iter()
                .position(|step| *step == EffectStep::MentionedViewerRecorded)
                .expect("the mention record must be attempted");
            let unread = steps
                .iter()
                .position(|step| *step == EffectStep::UnreadPublished)
                .expect("unread must be published");
            assert!(
                recorded < unread,
                "{failing:?} disturbed the one order that carries weight: {steps:?}"
            );
            assert!(
                steps.contains(&EffectStep::MainDelivered)
                    && steps.contains(&EffectStep::IdleStarted),
                "{failing:?} truncated the announcement: {steps:?}"
            );
            assert_eq!(report.failed_steps(), vec![failing]);
        }
    }

    /// A record that fails has no fact to publish — there is no value to pass — and
    /// the person's notification is simply not attempted. Everything else still is.
    #[tokio::test]
    async fn a_failed_record_leaves_nothing_to_notify_with() {
        let recorder = Recorder::failing(&[EffectStep::MentionedViewerRecorded]);

        let committed = fact_with(Vec::new(), main_plan(&["viewer-1"], &["agent-1"]));

        let report = announce_main_send(
            committed.main_line().expect("a main-line send"),
            main_ports(&recorder),
        )
        .await;

        assert!(!recorder.called(EffectStep::ViewerNotified));
        assert!(recorder.called(EffectStep::UnreadPublished));
        assert_eq!(
            report.failed_steps(),
            vec![EffectStep::MentionedViewerRecorded]
        );
    }

    /// The record is one act — the store decides entitlement and writes what it may —
    /// while telling each person is their own. One person's notification failing does
    /// not cost another theirs.
    #[tokio::test]
    async fn one_persons_publication_failure_does_not_cost_another_theirs() {
        let recorder = Recorder::failing(&[EffectStep::ViewerNotified]);
        let committed = fact_with(
            Vec::new(),
            main_plan(&["viewer-1", "viewer-2"], &["agent-1"]),
        );

        let report = announce_main_send(
            committed.main_line().expect("a main-line send"),
            main_ports(&recorder),
        )
        .await;

        // Keyed by the message: the publication of the message itself, then one record
        // for the whole send rather than one per name.
        assert_eq!(
            recorder.steps_of("source-message"),
            vec![
                EffectStep::MessagePublished,
                EffectStep::MentionedViewerRecorded
            ],
            "the record is asked once for the whole send"
        );
        for viewer in ["viewer-1", "viewer-2"] {
            assert_eq!(
                recorder.steps_of(&format!("user-{viewer}|source-message")),
                vec![EffectStep::ViewerNotified],
                "{viewer} was not told separately"
            );
        }
        assert_eq!(
            report.failed_steps(),
            vec![EffectStep::ViewerNotified, EffectStep::ViewerNotified]
        );
    }

    /// No recorded rows means no viewer publication; the real store owns the
    /// entitlement decision, exercised separately by the SQLite revocation tests.
    #[tokio::test]
    async fn an_empty_record_result_notifies_nobody() {
        let recorder = Recorder::entitling(&[]);
        let committed = fact_with(Vec::new(), main_plan(&["viewer-1"], &["agent-1"]));

        let report = announce_main_send(
            committed.main_line().expect("a main-line send"),
            main_ports(&recorder),
        )
        .await;

        assert!(recorder.called(EffectStep::MentionedViewerRecorded));
        assert!(
            !recorder.called(EffectStep::ViewerNotified),
            "a person who is no longer a reader was told anyway"
        );
        // The rest of the aftermath is unaffected: nothing here was about them.
        assert!(recorder.called(EffectStep::UnreadPublished));
        assert!(report.failed.is_empty());
    }

    /// The two paths answer different questions about the same conversation, so their
    /// plans are different values — and a reply cannot be handed to the main-line
    /// announcer, because the view it needs does not exist for one.
    #[test]
    fn a_reply_and_a_main_line_send_reach_different_recipients() {
        let participants = ["agent-1", "agent-2"];
        let main = fact_with(Vec::new(), main_plan(&[], &participants));
        let reply = MessageCreatedFact {
            thread: Some(reply_projection("root-1", 1)),
            post_commit: PostCommitPlan::committed(
                None,
                // Only one participant follows the cluster.
                DeliveryPlan::ReplyCluster(ReplyAudience::committed(vec![
                    AgentId::parse("agent-2").unwrap(),
                ])),
                Vec::new(),
            ),
            ..fact(Vec::new())
        };

        assert!(main.reply_cluster().is_none());
        assert!(
            reply.main_line().is_none(),
            "a reply offered a main-line view"
        );

        let main_recipients = main.main_line().unwrap().delivery().recipients().to_vec();
        let reply_audience = reply
            .reply_cluster()
            .unwrap()
            .audience()
            .recipients()
            .to_vec();
        assert_eq!(
            main_recipients,
            vec![
                AgentId::parse("agent-1").unwrap(),
                AgentId::parse("agent-2").unwrap()
            ]
        );
        assert_ne!(main_recipients, reply_audience);
    }

    /// The reply's own order: publish (the frame carries its cluster projections),
    /// the shared mention prefix, unread, and then delivery to the cluster's
    /// followers. It ends there — the two places it diverges from the main line are
    /// that there is no main-line fan-out and no idle start.
    #[tokio::test]
    async fn a_reply_is_announced_in_its_own_order() {
        let recorder = default_recorder();
        let committed = reply_fact(&["agent-2"], &["viewer-1"]);

        let report = announce_reply_send(
            committed.reply_cluster().expect("a reply"),
            reply_ports(&recorder),
        )
        .await;

        let id = MessageId::parse("source-message").unwrap();
        assert_eq!(
            report.steps_for(&id),
            vec![
                EffectStep::MessagePublished,
                EffectStep::MentionedViewerRecorded,
                EffectStep::ViewerNotified,
                EffectStep::UnreadPublished,
                EffectStep::ReplyDelivered,
            ]
        );
        assert!(
            !report
                .steps_for(&id)
                .iter()
                .any(|step| matches!(step, EffectStep::MainDelivered | EffectStep::IdleStarted)),
            "a reply used the main-line fan-out or started the room: {:?}",
            report.steps_for(&id)
        );
        assert!(report.failed.is_empty());
    }

    /// The one load-bearing order — the mention row before unread — holds under any
    /// single failure, and that failure isolates: the reply is still delivered to
    /// its follower, and the room is still never started.
    #[tokio::test]
    async fn a_replys_load_bearing_order_holds_and_a_failure_isolates() {
        for failing in [
            EffectStep::MessagePublished,
            EffectStep::MentionedViewerRecorded,
            EffectStep::ViewerNotified,
            EffectStep::UnreadPublished,
            EffectStep::ReplyDelivered,
        ] {
            let recorder = Recorder::failing(&[failing]);
            let committed = reply_fact(&["agent-2"], &["viewer-1"]);

            let report = announce_reply_send(
                committed.reply_cluster().expect("a reply"),
                reply_ports(&recorder),
            )
            .await;

            let steps = report.steps_for(&MessageId::parse("source-message").unwrap());
            let recorded = steps
                .iter()
                .position(|step| *step == EffectStep::MentionedViewerRecorded)
                .expect("the mention record must be attempted");
            let unread = steps
                .iter()
                .position(|step| *step == EffectStep::UnreadPublished)
                .expect("unread must be published");
            assert!(
                recorded < unread,
                "{failing:?} disturbed the one order that carries weight: {steps:?}"
            );
            assert!(
                steps.contains(&EffectStep::ReplyDelivered),
                "{failing:?} truncated the reply's delivery: {steps:?}"
            );
            assert!(
                !steps.contains(&EffectStep::IdleStarted),
                "{failing:?} started the room a reply must never start: {steps:?}"
            );
            assert_eq!(report.failed_steps(), vec![failing]);
        }
    }

    /// Every follower in the audience is owed the reply, each on its own attempt.
    #[tokio::test]
    async fn a_reply_reaches_every_follower_in_its_audience() {
        let recorder = default_recorder();
        let committed = reply_fact(&["agent-2", "agent-3"], &[]);

        let report = announce_reply_send(
            committed.reply_cluster().expect("a reply"),
            reply_ports(&recorder),
        )
        .await;

        let id = MessageId::parse("source-message").unwrap();
        let delivered = report
            .steps_for(&id)
            .into_iter()
            .filter(|step| *step == EffectStep::ReplyDelivered)
            .count();
        assert_eq!(delivered, 2, "one attempt was owed per follower");
        assert!(
            !report.steps_for(&id).contains(&EffectStep::IdleStarted),
            "a reply started the room"
        );
        assert!(report.failed.is_empty());
    }

    /// Per-recipient, not merely per-step, isolation: one follower's delivery fails
    /// and the OTHER follower is still attempted and still succeeds. A single batched
    /// `deliver_to(&[recipients])` would fail both together — the loop that gives each
    /// recipient its own attempt is what this pins.
    #[tokio::test]
    async fn one_followers_delivery_failure_does_not_cost_another_theirs() {
        // Only agent-2's delivery key is told to fail; agent-3's is untouched.
        let recorder = Recorder::failing_one_key("agent-2|source-message");
        let committed = reply_fact(&["agent-2", "agent-3"], &[]);

        let report = announce_reply_send(
            committed.reply_cluster().expect("a reply"),
            reply_ports(&recorder),
        )
        .await;

        let id = MessageId::parse("source-message").unwrap();
        // Both followers were attempted — the failing one did not truncate the loop.
        let delivered = report
            .steps_for(&id)
            .into_iter()
            .filter(|step| *step == EffectStep::ReplyDelivered)
            .count();
        assert_eq!(
            delivered, 2,
            "the failing follower cost the other its attempt"
        );
        // Exactly one delivery failed — agent-2's — and agent-3's succeeded.
        assert_eq!(report.failed_steps(), vec![EffectStep::ReplyDelivered]);
        let (failed_key, _, _) = report
            .failed
            .iter()
            .find(|(_, step, _)| *step == EffectStep::ReplyDelivered)
            .map(|(id, step, failure)| (id.as_str().to_owned(), step, failure))
            .expect("the failed delivery");
        assert_eq!(
            failed_key, "source-message",
            "the failure is recorded against the reply message"
        );
        assert!(recorder.calls.lock().unwrap().contains(&(
            "agent-3|source-message".to_owned(),
            EffectStep::ReplyDelivered
        )));
    }

    /// The reply analog of the main line's all-at-once grid: every reply port fails
    /// simultaneously and the flow is still not truncated, the two forbidden steps
    /// never appear, and the send stays a fact. The one legitimately unreached step is
    /// `ViewerNotified`: a record that failed produced no row to notify with (the same
    /// invariant `a_failed_record_leaves_nothing_to_notify_with` pins on the main line).
    #[tokio::test]
    async fn every_reply_effect_failing_at_once_still_leaves_it_a_fact() {
        let recorder = Recorder::failing(&[
            EffectStep::MessagePublished,
            EffectStep::MentionedViewerRecorded,
            EffectStep::ViewerNotified,
            EffectStep::UnreadPublished,
            EffectStep::ReplyDelivered,
        ]);
        let committed = reply_fact(&["agent-2"], &["viewer-1"]);

        let report = announce_reply_send(
            committed.reply_cluster().expect("a reply"),
            reply_ports(&recorder),
        )
        .await;

        let id = MessageId::parse("source-message").unwrap();
        let steps = report.steps_for(&id);
        // Every independent step was still attempted despite its predecessor failing.
        for step in [
            EffectStep::MessagePublished,
            EffectStep::MentionedViewerRecorded,
            EffectStep::UnreadPublished,
            EffectStep::ReplyDelivered,
        ] {
            assert!(steps.contains(&step), "{step:?} was truncated: {steps:?}");
        }
        // The one unreached step, and why: the failed record left no row to notify.
        assert!(
            !recorder.called(EffectStep::ViewerNotified),
            "a failed record still produced a notification"
        );
        // Neither forbidden step ever appears on a reply, even under total failure.
        assert!(
            !steps
                .iter()
                .any(|step| matches!(step, EffectStep::MainDelivered | EffectStep::IdleStarted)),
            "a reply fanned out or started the room under failure: {steps:?}"
        );
        // Every attempted step failed — none silently succeeded.
        assert_eq!(
            report.failed_steps(),
            vec![
                EffectStep::MessagePublished,
                EffectStep::MentionedViewerRecorded,
                EffectStep::UnreadPublished,
                EffectStep::ReplyDelivered,
            ]
        );
        // The fact is an argument, not an outcome: total effect failure does not
        // reverse the committed reply.
        assert_eq!(committed.message.id.as_str(), "source-message");
        assert!(
            committed.thread.is_some(),
            "the reply stopped being a reply"
        );
    }
}
