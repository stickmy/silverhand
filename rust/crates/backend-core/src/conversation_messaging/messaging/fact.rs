//! The single business fact of one committed send, and the views that read it.
//!
//! Both the acknowledgement and the published event project from the value here,
//! which is why they cannot disagree. The two typed views are what make "announce a
//! reply as if it were a main-line send" something nobody can write: each path has
//! its own type, and the only source of either is the fact itself.

use super::{
    AgentId, ConversationId, MessageAuthor, MessageContent, MessageId, MessageReference,
    MessageSeq, ThreadRootId, UserId, attachments, mention_cc, post_commit,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FromKind {
    User,
    Agent,
    System,
}

/// Why a principal follows a reply cluster. Recorded because the reason outlives
/// the event: a follower added by a mention is not the same fact as one who
/// replied.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThreadFollowerSource {
    Reply,
    TargetAuthor,
    Mention,
}

/// The single business fact of one committed send. Both the acknowledgement and
/// the published event are projections of this value, which is why they cannot
/// disagree.
///
/// `pub(super)` on the fields, not `pub`: when this type was declared in `messaging`
/// itself its private fields were already readable by every module in that subtree,
/// so this is the visibility it always had rather than a widening of it. What stays
/// out of reach is anything outside the crate, which is what the `compile_fail`
/// proofs on [`MessageCreatedFact::main_line`] depend on.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MessageCreatedFact {
    pub(super) message: CreatedMessage,
    pub(super) thread: Option<ThreadReplyFact>,
    pub(super) post_commit: post_commit::PostCommitPlan,
    pub(super) freshness_bypassed: bool,
    pub(super) mention_copies: Vec<mention_cc::MentionCopyOutcome>,
}

impl MessageCreatedFact {
    /// Assembled by the unit of work that committed the send.
    ///
    /// A trust seam, stated rather than pretended away: this factory is public, so
    /// every crate that can call it is inside the same trusted boundary — a fact can
    /// be fabricated by anyone with that reach. What is ruled out is different and
    /// narrower: a fact that has been returned cannot be replaced or modified, because
    /// every field is read-only from here on.
    pub fn committed(
        message: CreatedMessage,
        thread: Option<ThreadReplyFact>,
        post_commit: post_commit::PostCommitPlan,
        freshness_bypassed: bool,
        mention_copies: Vec<mention_cc::MentionCopyOutcome>,
    ) -> Self {
        Self {
            message,
            thread,
            post_commit,
            freshness_bypassed,
            mention_copies,
        }
    }

    pub fn message(&self) -> &CreatedMessage {
        &self.message
    }

    /// Present only for a reply.
    pub fn thread(&self) -> Option<&ThreadReplyFact> {
        self.thread.as_ref()
    }

    /// The guard was skipped at the sender's request. Carried for the post-commit
    /// record, not for the reply to the caller.
    pub fn freshness_bypassed(&self) -> bool {
        self.freshness_bypassed
    }

    /// What became of each agent this message addressed that could not read it.
    /// Empty when the send addressed nobody outside its own conversation.
    pub fn mention_copies(&self) -> &[mention_cc::MentionCopyOutcome] {
        &self.mention_copies
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreatedMessage {
    pub client_message_id: Option<String>,
    pub id: MessageId,
    pub conversation_id: ConversationId,
    pub from_kind: FromKind,
    pub from_agent_id: Option<AgentId>,
    pub from_user_id: Option<UserId>,
    pub author: Option<MessageAuthor>,
    pub content: MessageContent,
    pub attachments: Vec<attachments::AttachmentSnapshot>,
    pub seq: MessageSeq,
    pub created_at_ms: i64,
    pub thread_root_id: Option<ThreadRootId>,
    /// Answer options the author attached; a fresh message is always unanswered.
    pub options: Option<super::collaboration::message_options::MessageOptions>,
}

/// One send's whole answer: what was committed, and what only its sender is told.
///
/// TWO AUDIENCES, kept apart by the type. [`MessageCreatedFact`] is the committed payload the
/// acknowledgement AND the conversation-wide publication both project from, so it must contain
/// nothing that may not be published. `unresolved_references` is the opposite: a diagnostic about
/// this ATTEMPT — which of the sender's references named nobody — and it is NEVER PUBLISHED: it
/// reaches no other principal and rides no conversation-facing payload. Stated as an audience
/// rather than as "nobody else reads it", because the success boundary also reports the author's
/// own tokens once at WARN for an operator, and that observation surface is not a wire audience
/// (@Alice, whose boundary text this is, authorized the narrowing).
///
/// So it lives here rather than on the fact. Putting it there would have left "do not publish this
/// field" as a rule every present and future announcer has to remember, and a payload that fans out
/// to a whole conversation is the worst place to keep a rule like that. Announcers take
/// [`committed`](Self::committed) and cannot reach the rest.
///
/// The empty vector is a TOTAL answer — "every reference this body carried named somebody" — and
/// not an absence. Only the wire decides to omit it when empty.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SendMessageOutcome {
    replay: bool,
    committed: MessageCreatedFact,
    unresolved_references: Vec<MessageReference>,
}

impl SendMessageOutcome {
    pub fn as_replay(mut self) -> Self {
        self.replay = true;
        self
    }
    pub fn is_replay(&self) -> bool {
        self.replay
    }
    /// Assembled by the unit of work that committed the send, from what it read inside the same
    /// atomic unit. The unresolved set is decided there, against the TARGET conversation's roster
    /// and the mention copies this send actually made — never re-derived afterwards, when a
    /// membership change could answer differently about a message that is already a fact.
    pub fn committed_send(
        committed: MessageCreatedFact,
        unresolved_references: Vec<MessageReference>,
    ) -> Self {
        Self {
            replay: false,
            committed,
            unresolved_references,
        }
    }

    /// What every reader of this send may see.
    pub fn committed(&self) -> &MessageCreatedFact {
        &self.committed
    }

    /// The publication path CANNOT REACH the sender-only half, and that is a type fact rather
    /// than a habit of the current implementation. Both announcers take a committed VIEW as
    /// their only input, and only the committed half can produce one.
    ///
    /// Stated as the type relation itself, with a local stand-in for the announcer's parameter.
    /// Calling the real generic announcer here would prove nothing: its effect ports cannot be
    /// inferred from a `todo!()`, so the example would fail for that reason whatever the first
    /// argument was — a gate that stays green while the isolation is broken.
    ///
    /// ```compile_fail
    /// use silverhand_backend_core::conversation_messaging::messaging::{MainCommittedSend, SendMessageOutcome};
    ///
    /// fn publication_input(_: MainCommittedSend<'_>) {}
    ///
    /// fn leak(outcome: &SendMessageOutcome) {
    ///     publication_input(outcome);
    /// }
    /// ```
    ///
    /// and the sender-only half is not a reply view either:
    ///
    /// ```compile_fail
    /// use silverhand_backend_core::conversation_messaging::messaging::{ReplyCommittedSend, SendMessageOutcome};
    ///
    /// fn reply_publication_input(_: ReplyCommittedSend<'_>) {}
    ///
    /// fn leak(outcome: &SendMessageOutcome) {
    ///     reply_publication_input(outcome.unresolved_references());
    /// }
    /// ```
    ///
    /// THE POSITIVE NEIGHBOUR, which is what makes those two mean anything: the same parameter
    /// accepts a view taken from the committed half, so the failures above are the argument's
    /// type and not a missing name or an uninferable port.
    ///
    /// ```
    /// use silverhand_backend_core::conversation_messaging::messaging::{
    ///     MainCommittedSend, ReplyCommittedSend, SendMessageOutcome,
    /// };
    ///
    /// fn publication_input(_: MainCommittedSend<'_>) {}
    /// fn reply_publication_input(_: ReplyCommittedSend<'_>) {}
    ///
    /// fn accepted(outcome: &SendMessageOutcome) {
    ///     if let Some(view) = outcome.committed().main_line() {
    ///         publication_input(view);
    ///     }
    ///     if let Some(view) = outcome.committed().reply_cluster() {
    ///         reply_publication_input(view);
    ///     }
    /// }
    /// ```
    ///
    /// What only its sender may see, in the body's own first-occurrence order.
    pub fn unresolved_references(&self) -> &[MessageReference] {
        &self.unresolved_references
    }

    // THERE IS NO WAY TO TAKE ONE HALF AND DROP THE OTHER, deliberately. A consuming accessor for
    // the committed half would be a one-step path back to the old shape that silently discards the
    // total unresolved answer — the sender-only boundary this type exists to establish, handed back
    // to whoever consumes a send result (review of this change). A consumer keeps the
    // outcome and borrows the half it needs.
}

/// What a reply changed about its cluster, projected inside the atomic unit that
/// committed it: the cluster's list entry, computed from the rows this reply just
/// wrote so it already includes itself.
///
/// This is the one committed reply projection, and it carries [`ThreadListEntry`] by
/// value rather than pointing at a query to run later. The publication frame and the
/// acknowledgement both borrow this same value (see [`CommittedMessageView`]), and a
/// cold read-side projection re-derives an equal entry from the same authoritative
/// rows — but none of them re-reads to build *this* reply's frame, so a later rename,
/// a newer reply, or a read failure cannot change or withhold what was published.
///
/// A single private field with a public [`committed`](ThreadReplyFact::committed)
/// seam and read-only accessors: the unit of work that computed the projection
/// inside the atomic unit is the only thing that assembles one, and nothing
/// downstream can fabricate or mutate it. Neither `reply_count` nor the root
/// identity is stored twice — both are read straight off the carried entry, so the
/// value delivery/audience routes by and the value publication/ACK place are the
/// same cluster. There is no separate root field a caller could set to a different
/// cluster than the entry describes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ThreadReplyFact {
    list_entry: ThreadListEntry,
}

impl ThreadReplyFact {
    /// Assembled by the unit of work that committed the reply, from the facts it
    /// read inside the same atomic unit — the [`ThreadListEntry`] it projected
    /// there, so the entry the frame carries is the committed one. The entry is the
    /// fact's only root authority: there is no second root parameter, so
    /// `root-A + entry(root-B)` cannot be expressed at construction.
    ///
    /// A caller outside this crate cannot assemble one field by field:
    ///
    /// ```compile_fail
    /// use silverhand_backend_core::conversation_messaging::messaging::ThreadReplyFact;
    ///
    /// let fact = ThreadReplyFact {
    ///     list_entry: todo!(),
    /// };
    /// ```
    ///
    /// nor mutate one that already exists:
    ///
    /// ```compile_fail
    /// use silverhand_backend_core::conversation_messaging::messaging::ThreadReplyFact;
    ///
    /// fn tamper(fact: &mut ThreadReplyFact) {
    ///     fact.list_entry = todo!();
    /// }
    /// ```
    ///
    /// nor pass a root identity beside the entry — the arity itself refuses a
    /// mismatched root, so the divergence has no way to enter:
    ///
    /// ```compile_fail
    /// use silverhand_backend_core::conversation_messaging::messaging::{ThreadReplyFact, ThreadRootId};
    ///
    /// let fact = ThreadReplyFact::committed(ThreadRootId::parse("root-A").unwrap(), todo!());
    /// ```
    pub fn committed(list_entry: ThreadListEntry) -> Self {
        Self { list_entry }
    }

    /// The cluster this reply belongs to, read off the carried entry — the fact's
    /// single root authority, the same identity delivery and audience route by.
    pub fn thread_root_id(&self) -> &ThreadRootId {
        self.list_entry.root_id()
    }

    /// The absolute count of participant replies in the cluster, already including
    /// this reply. Read off the carried entry, so it can never disagree with the
    /// placed entry's own count.
    pub fn reply_count(&self) -> i64 {
        self.list_entry.reply_count()
    }

    /// The cluster's list entry as projected inside this reply's atomic unit — the
    /// value the publication frame and the acknowledgement both carry.
    pub fn list_entry(&self) -> &ThreadListEntry {
        &self.list_entry
    }
}

/// The largest a thread's root preview may be, counted in UTF-16 code units.
///
/// This is the wire contract's unit, not Rust's: the published preview is measured
/// the way the client that renders it measures a string, so a value that fits here
/// fits there. See [`thread_root_preview`] for the one boundary case where the two
/// string models cannot agree.
pub const THREAD_PREVIEW_MAX_UTF16: usize = 160;

/// The root preview a thread-list entry publishes: the root's content truncated to
/// at most [`THREAD_PREVIEW_MAX_UTF16`] UTF-16 code units, with no trimming,
/// normalization, or ellipsis — the raw prefix, exactly as the reader will measure
/// it.
///
/// The one place the two string models diverge, stated rather than hidden: the wire
/// counts UTF-16 code units and a client is free to cut an astral scalar in half,
/// keeping a lone surrogate as its 160th unit. A Rust `String` is UTF-8 and cannot
/// hold a lone surrogate, so when the limit would fall inside an astral scalar this
/// stops at the scalar boundary before it (159 units) rather than emitting one that
/// cannot be a `str`. Every boundary that lands between scalars is identical to a
/// UTF-16 slice; only an astral scalar straddling the exact limit differs, by the
/// one unit that has no UTF-8 spelling.
pub fn thread_root_preview(content: &str) -> String {
    let mut units = 0usize;
    let mut end = content.len();
    for (offset, ch) in content.char_indices() {
        let width = ch.len_utf16();
        if units + width > THREAD_PREVIEW_MAX_UTF16 {
            end = offset;
            break;
        }
        units += width;
    }
    content[..end].to_owned()
}

/// The value [`ThreadReplyFact`] carries: the cluster as it is placed and previewed,
/// without the per-viewer follow bit.
///
/// Projected inside the reply's atomic unit from the authoritative
/// `(reply_count, last_reply_*)` rows, so the entry a reply produces already counts
/// itself and can never show a future-seq value. `root_author` and every replier are the one shared
/// [`MessageAuthor`] contract, so a deleted actor keeps its send-time snapshot here
/// exactly as it does on a message byline. The per-viewer `viewer_following` bit is
/// deliberately absent: it is not a property of the cluster, and this value is the
/// viewer-invariant subset every recipient of the broadcast frame shares.
///
/// Private fields with a public [`committed`](ThreadListEntry::committed) seam and
/// read-only accessors, for the same reason [`ThreadReplyFact`] has them: the adapter
/// that read the projection inside the atomic unit is the only thing that assembles
/// one, and the preview rule is enforced here — the seam takes raw content and
/// applies [`thread_root_preview`], so no caller can publish an over-long or
/// arbitrarily-truncated preview.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ThreadListEntry {
    root_id: ThreadRootId,
    root_author: Option<MessageAuthor>,
    root_preview: String,
    root_created_at_ms: i64,
    root_seq: MessageSeq,
    reply_count: i64,
    last_reply_at_ms: i64,
    last_reply_message_id: MessageId,
    repliers: Vec<MessageAuthor>,
    replier_total: i64,
}

impl ThreadListEntry {
    /// Assembled by the adapter that projected the cluster inside the same atomic
    /// unit as the reply. The preview is derived here from the raw root content, so
    /// the [`THREAD_PREVIEW_MAX_UTF16`] rule cannot be bypassed or duplicated.
    ///
    /// A caller outside this crate cannot assemble one field by field:
    ///
    /// ```compile_fail
    /// use silverhand_backend_core::conversation_messaging::messaging::ThreadListEntry;
    ///
    /// let entry = ThreadListEntry {
    ///     root_id: todo!(),
    ///     root_author: None,
    ///     root_preview: String::new(),
    ///     root_created_at_ms: 0,
    ///     root_seq: todo!(),
    ///     reply_count: 0,
    ///     last_reply_at_ms: 0,
    ///     last_reply_message_id: todo!(),
    ///     repliers: Vec::new(),
    ///     replier_total: 0,
    /// };
    /// ```
    ///
    /// nor mutate one that already exists:
    ///
    /// ```compile_fail
    /// use silverhand_backend_core::conversation_messaging::messaging::ThreadListEntry;
    ///
    /// fn tamper(entry: &mut ThreadListEntry) {
    ///     entry.replier_total = 0;
    /// }
    /// ```
    #[allow(clippy::too_many_arguments)]
    pub fn committed(
        root_id: ThreadRootId,
        root_author: Option<MessageAuthor>,
        root_content: &str,
        root_created_at_ms: i64,
        root_seq: MessageSeq,
        reply_count: i64,
        last_reply_at_ms: i64,
        last_reply_message_id: MessageId,
        repliers: Vec<MessageAuthor>,
        replier_total: i64,
    ) -> Self {
        Self {
            root_id,
            root_author,
            root_preview: thread_root_preview(root_content),
            root_created_at_ms,
            root_seq,
            reply_count,
            last_reply_at_ms,
            last_reply_message_id,
            repliers,
            replier_total,
        }
    }

    /// The cluster's root identity — the single coordinate that names which message
    /// this entry is about. A [`ThreadReplyFact`] derives its own
    /// [`thread_root_id`](ThreadReplyFact::thread_root_id) from here, so the fact
    /// carries exactly one root.
    pub fn root_id(&self) -> &ThreadRootId {
        &self.root_id
    }

    pub fn root_author(&self) -> Option<&MessageAuthor> {
        self.root_author.as_ref()
    }

    pub fn root_preview(&self) -> &str {
        &self.root_preview
    }

    pub fn root_created_at_ms(&self) -> i64 {
        self.root_created_at_ms
    }

    pub fn root_seq(&self) -> MessageSeq {
        self.root_seq
    }

    pub fn reply_count(&self) -> i64 {
        self.reply_count
    }

    pub fn last_reply_at_ms(&self) -> i64 {
        self.last_reply_at_ms
    }

    pub fn last_reply_message_id(&self) -> &MessageId {
        &self.last_reply_message_id
    }

    pub fn repliers(&self) -> &[MessageAuthor] {
        &self.repliers
    }

    pub fn replier_total(&self) -> i64 {
        self.replier_total
    }
}

/// The published payload of one committed send: the message itself, and — only for
/// a reply — the cluster projections that ride the same frame.
///
/// Path-neutral by construction. A main-line send carries no projections, a reply
/// carries its own, and both can obtain a view only from the fact ([`MainCommittedSend::published`]
/// / [`ReplyCommittedSend::published`]): the fields are private and there is no
/// public constructor, so nobody outside can fabricate one or fake a main-line send
/// that carries reply projections. The acknowledgement projects from this same
/// value, which is why the published frame and the ACK cannot disagree.
#[derive(Clone, Copy, Debug)]
pub struct CommittedMessageView<'fact> {
    message: &'fact CreatedMessage,
    reply_projection: Option<&'fact ThreadReplyFact>,
}

impl<'fact> CommittedMessageView<'fact> {
    pub fn message(&self) -> &'fact CreatedMessage {
        self.message
    }

    /// The cluster projections carried on a reply's frame; absent on the main line.
    pub fn reply_projection(&self) -> Option<&'fact ThreadReplyFact> {
        self.reply_projection
    }
}

/// One committed main-line send and the plan that belongs to it.
///
/// A borrowed view, and the only way to hold a main-line plan: its fields are
/// private and it has no constructor, so a caller cannot assemble one, and the only
/// source is [`MessageCreatedFact::main_line`] — which answers `None` for a reply.
/// That is what makes "announce a reply as if it were a main-line send" something
/// nobody can write rather than something a runtime check has to catch.
#[derive(Clone, Copy, Debug)]
pub struct MainCommittedSend<'fact> {
    message: &'fact CreatedMessage,
    mentions: Option<&'fact post_commit::MentionIntent>,
    delivery: &'fact post_commit::MainDeliveryPlan,
}

impl<'fact> MainCommittedSend<'fact> {
    pub fn message(&self) -> &'fact CreatedMessage {
        self.message
    }

    /// What this send said about naming people. Path-independent: a person is named
    /// by the body, not by where the message landed.
    pub fn mentions(&self) -> Option<&'fact post_commit::MentionIntent> {
        self.mentions
    }

    pub fn delivery(&self) -> &'fact post_commit::MainDeliveryPlan {
        self.delivery
    }

    /// The published payload: the message alone — a main-line send carries no
    /// cluster projections.
    pub fn published(&self) -> CommittedMessageView<'fact> {
        CommittedMessageView {
            message: self.message,
            reply_projection: None,
        }
    }
}

/// The same shape for a reply, carrying the cluster's own plan. Distinct from
/// [`MainCommittedSend`] by type, so neither path's announcer accepts the other's.
#[derive(Clone, Copy, Debug)]
pub struct ReplyCommittedSend<'fact> {
    message: &'fact CreatedMessage,
    projection: &'fact ThreadReplyFact,
    mentions: Option<&'fact post_commit::MentionIntent>,
    audience: &'fact post_commit::ReplyAudience,
}

impl<'fact> ReplyCommittedSend<'fact> {
    pub fn message(&self) -> &'fact CreatedMessage {
        self.message
    }

    pub fn projection(&self) -> &'fact ThreadReplyFact {
        self.projection
    }

    /// The same path-independent fact the main line carries: naming someone does not
    /// depend on whether the message was a reply.
    pub fn mentions(&self) -> Option<&'fact post_commit::MentionIntent> {
        self.mentions
    }

    pub fn audience(&self) -> &'fact post_commit::ReplyAudience {
        self.audience
    }

    /// The published payload: the message together with the cluster projections that
    /// ride the same frame.
    pub fn published(&self) -> CommittedMessageView<'fact> {
        CommittedMessageView {
            message: self.message,
            reply_projection: Some(self.projection),
        }
    }
}

impl MessageCreatedFact {
    /// This send as a main-line one, or nothing if it was a reply.
    ///
    /// `None` is a question's answer, not an outcome: there is no announcer that
    /// takes a fact, so a caller holding `None` has nothing to call rather than
    /// something to skip.
    ///
    /// A view cannot be assembled by hand:
    ///
    /// ```compile_fail
    /// use silverhand_backend_core::conversation_messaging::messaging::MainCommittedSend;
    ///
    /// let view = MainCommittedSend {
    ///     message: todo!(),
    ///     mentions: todo!(),
    ///     delivery: todo!(),
    /// };
    /// ```
    ///
    /// Nor can a fact that already exists be given a different plan:
    ///
    /// ```compile_fail
    /// use silverhand_backend_core::conversation_messaging::messaging::{MessageCreatedFact, post_commit::PostCommitPlan};
    ///
    /// fn replace(fact: &mut MessageCreatedFact, plan: PostCommitPlan) {
    ///     fact.post_commit = plan;
    /// }
    /// ```
    pub fn main_line(&self) -> Option<MainCommittedSend<'_>> {
        match self.post_commit.delivery() {
            post_commit::DeliveryPlan::MainLine(delivery) => Some(MainCommittedSend {
                message: &self.message,
                mentions: self.post_commit.mentions(),
                delivery,
            }),
            post_commit::DeliveryPlan::ReplyCluster(_) => None,
        }
    }

    /// This send as a reply, or nothing if it was a main-line one.
    pub fn reply_cluster(&self) -> Option<ReplyCommittedSend<'_>> {
        match (self.post_commit.delivery(), &self.thread) {
            (post_commit::DeliveryPlan::ReplyCluster(audience), Some(projection)) => {
                Some(ReplyCommittedSend {
                    message: &self.message,
                    projection,
                    mentions: self.post_commit.mentions(),
                    audience,
                })
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::thread_root_preview;

    #[test]
    fn previews_obey_the_160_utf16_unit_wire_limit_without_splitting_scalars() {
        for (content, expected) in [
            ("a".repeat(159), "a".repeat(159)),
            ("a".repeat(160), "a".repeat(160)),
            ("a".repeat(161), "a".repeat(160)),
            (
                format!("{}世", "a".repeat(159)),
                format!("{}世", "a".repeat(159)),
            ),
            (format!("{}😀", "a".repeat(159)), "a".repeat(159)),
            (
                format!("{}😀", "a".repeat(158)),
                format!("{}😀", "a".repeat(158)),
            ),
        ] {
            assert_eq!(
                thread_root_preview(&content),
                expected,
                "input: {content:?}"
            );
        }
    }

    #[test]
    fn the_preview_never_trims_or_normalizes_leading_whitespace() {
        let content = "   spaced".to_owned();
        assert_eq!(thread_root_preview(&content), content);
    }
}
