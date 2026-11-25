//! Closed read contract for one conversation message window.
//!
//! The request chooses one stream and one mutually-exclusive anchor. Storage
//! resolves the stream from actual message rows, decodes every selected and
//! look-ahead candidate, and only then constructs the page. The page retains
//! viewer, conversation, requested stream, resolved stream, query and every row
//! coordinate until the HTTP consumer binds them.
//!
//! The complete viewer-private wire inventory for this operation is one bit:
//! `reactions[].viewer_reacted`. Public widening keeps all viewer-invariant
//! history (including reaction counts, linked tasks and reply sidecars) but seals
//! that bit to `false`. Follow state, read cursors, unread state and membership
//! rows are not part of this operation's success value at all.

use std::{collections::HashSet, error::Error, future::Future};

use crate::{
    conversation_messaging::messaging::{
        FromKind, MessageAuthor, MessageId, MessageSeq, PrincipalIdentity, PrincipalKind,
        ThreadRootId,
        attachments::{AttachmentLabel, AttachmentSnapshot},
        collaboration::reaction::ReactionAggregate,
        task::{TaskId, TaskNumber, TaskStatus, TaskTitle},
    },
    foundation::identity::{AgentId, ConversationId, UserId},
};

const MAX_JAVASCRIPT_INTEGER: i64 = 9_007_199_254_740_991;
const MAX_WINDOW_LIMIT: u16 = 500;
const MAX_REPLY_PREVIEWS: usize = 3;
const REPLY_PREVIEW_MAX_UTF16: usize = 160;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MessageWindowAnchor {
    Latest,
    Before(MessageSeq),
    After(MessageSeq),
    Around(MessageId),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MessageWindowQuery {
    limit: u16,
    anchor: MessageWindowAnchor,
}

impl MessageWindowQuery {
    fn new(limit: u16, anchor: MessageWindowAnchor) -> Result<Self, InvalidMessageWindowQuery> {
        if !(1..=MAX_WINDOW_LIMIT).contains(&limit) {
            return Err(InvalidMessageWindowQuery::Limit);
        }
        match &anchor {
            MessageWindowAnchor::Before(seq) | MessageWindowAnchor::After(seq) => {
                if !(0..=MAX_JAVASCRIPT_INTEGER).contains(&seq.get()) {
                    return Err(InvalidMessageWindowQuery::UnsafeCursor);
                }
            }
            MessageWindowAnchor::Latest | MessageWindowAnchor::Around(_) => {}
        }
        Ok(Self { limit, anchor })
    }

    pub const fn limit(&self) -> u16 {
        self.limit
    }

    pub const fn anchor(&self) -> &MessageWindowAnchor {
        &self.anchor
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum InvalidMessageWindowQuery {
    #[error("message-window limit must be between 1 and 500")]
    Limit,
    #[error("message-window cursor is not an exact nonnegative JavaScript integer")]
    UnsafeCursor,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MessageWindowStreamRequest {
    Mainline,
    Thread(MessageId),
}

/// One admitted operation selector. Server code cannot construct a stream and
/// cursor independently and accidentally send mismatched halves to storage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversationMessageWindowRequest {
    stream: MessageWindowStreamRequest,
    query: MessageWindowQuery,
}

impl ConversationMessageWindowRequest {
    pub fn new(
        stream: MessageWindowStreamRequest,
        limit: u16,
        anchor: MessageWindowAnchor,
    ) -> Result<Self, InvalidMessageWindowQuery> {
        Ok(Self {
            stream,
            query: MessageWindowQuery::new(limit, anchor)?,
        })
    }

    pub fn stream(&self) -> &MessageWindowStreamRequest {
        &self.stream
    }

    pub const fn query(&self) -> &MessageWindowQuery {
        &self.query
    }
}

/// The stream selected from actual persisted coordinates. A thread request may
/// name either its root or one reply; storage supplies that selected message's
/// own `conversation_id` and `thread_root_id`, and this constructor derives the
/// canonical root only after binding its id and conversation to the request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedMessageWindowStream {
    requested: MessageWindowStreamRequest,
    conversation: ConversationId,
    root: Option<ThreadRootId>,
}

impl ResolvedMessageWindowStream {
    pub fn mainline(conversation: ConversationId) -> Self {
        Self {
            requested: MessageWindowStreamRequest::Mainline,
            conversation,
            root: None,
        }
    }

    pub fn thread(
        requested_conversation: ConversationId,
        requested_message: MessageId,
        selected_message: MessageId,
        selected_conversation: ConversationId,
        selected_thread_root: Option<ThreadRootId>,
    ) -> Result<Self, MessageWindowCorruption> {
        if selected_message != requested_message || selected_conversation != requested_conversation
        {
            return Err(MessageWindowCorruption::StreamCoordinate);
        }
        let root = match selected_thread_root {
            Some(root) => root,
            None => ThreadRootId::parse(selected_message.as_str())
                .map_err(|_| MessageWindowCorruption::StreamCoordinate)?,
        };
        Ok(Self {
            requested: MessageWindowStreamRequest::Thread(requested_message),
            conversation: selected_conversation,
            root: Some(root),
        })
    }

    pub fn requested(&self) -> &MessageWindowStreamRequest {
        &self.requested
    }

    pub fn conversation(&self) -> &ConversationId {
        &self.conversation
    }

    pub fn root(&self) -> Option<&ThreadRootId> {
        self.root.as_ref()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MessageWindowAudience {
    Participating,
    PublicNonmember,
}

/// One selected row's actual author columns plus the shared resolved display
/// value. Keeping both halves is what lets the read consumer detect a resolved
/// author spliced from another message, including after the live FK was nulled.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowAuthorFact {
    from_kind: FromKind,
    from_agent: Option<AgentId>,
    from_user: Option<UserId>,
    snapshot_identity: Option<PrincipalIdentity>,
    snapshot_name: Option<String>,
    snapshot_display_name: Option<String>,
    author: Option<MessageAuthor>,
}

impl WindowAuthorFact {
    #[allow(clippy::too_many_arguments)]
    pub fn sealed(
        from_kind: FromKind,
        from_agent: Option<AgentId>,
        from_user: Option<UserId>,
        snapshot_identity: Option<PrincipalIdentity>,
        snapshot_name: Option<String>,
        snapshot_display_name: Option<String>,
        author: Option<MessageAuthor>,
    ) -> Result<Self, MessageWindowCorruption> {
        let fact = Self {
            from_kind,
            from_agent,
            from_user,
            snapshot_identity,
            snapshot_name,
            snapshot_display_name,
            author,
        };
        fact.validate()?;
        Ok(fact)
    }

    fn validate(&self) -> Result<(), MessageWindowCorruption> {
        if self.from_kind == FromKind::System {
            // System audit rows may retain the initiating actor's provenance
            // FK (for example session_cleared). That is not a display author:
            // both production writers and the shared author resolver preserve
            // this distinction. Keep the FK on the wire without inventing an
            // author or a send-time author snapshot for the system itself.
            return (self.snapshot_identity.is_none()
                && self.snapshot_name.is_none()
                && self.snapshot_display_name.is_none()
                && self.author.is_none())
            .then_some(())
            .ok_or(MessageWindowCorruption::AuthorProvenance);
        }
        let kind = match self.from_kind {
            FromKind::Agent => PrincipalKind::Agent,
            FromKind::User => PrincipalKind::Human,
            FromKind::System => unreachable!(),
        };
        let live_identity = match (&self.from_agent, &self.from_user, kind) {
            (Some(id), None, PrincipalKind::Agent) => Some(PrincipalIdentity::Agent(id.clone())),
            (None, Some(id), PrincipalKind::Human) => Some(PrincipalIdentity::Human(id.clone())),
            (None, None, _) => None,
            _ => return Err(MessageWindowCorruption::AuthorProvenance),
        };
        if self
            .snapshot_identity
            .as_ref()
            .is_some_and(|identity| identity.kind() != kind)
            || live_identity.as_ref().is_some_and(|live| {
                self.snapshot_identity
                    .as_ref()
                    .is_some_and(|snapshot| snapshot != live)
            })
        {
            return Err(MessageWindowCorruption::AuthorProvenance);
        }
        let author = self
            .author
            .as_ref()
            .ok_or(MessageWindowCorruption::AuthorProvenance)?;
        if author.kind() != kind {
            return Err(MessageWindowCorruption::AuthorProvenance);
        }
        match (live_identity, author) {
            (Some(live), MessageAuthor::Active(_))
                if author.stable_identity().as_ref() == Some(&live) =>
            {
                Ok(())
            }
            (Some(_), MessageAuthor::Unresolved(_)) => Ok(()),
            (None, MessageAuthor::Deleted(deleted)) => {
                let snapshot = self
                    .snapshot_identity
                    .as_ref()
                    .ok_or(MessageWindowCorruption::AuthorProvenance)?;
                let name = self
                    .snapshot_name
                    .as_deref()
                    .ok_or(MessageWindowCorruption::AuthorProvenance)?;
                let display = self.snapshot_display_name.as_deref().unwrap_or(name);
                (deleted.identity() == snapshot
                    && deleted.name() == name
                    && deleted.display_name() == display)
                    .then_some(())
                    .ok_or(MessageWindowCorruption::AuthorProvenance)
            }
            (None, MessageAuthor::Unresolved(_))
                if self.snapshot_identity.is_none() || self.snapshot_name.is_none() =>
            {
                Ok(())
            }
            _ => Err(MessageWindowCorruption::AuthorProvenance),
        }
    }

    pub const fn from_kind(&self) -> FromKind {
        self.from_kind
    }

    pub fn from_agent(&self) -> Option<&AgentId> {
        self.from_agent.as_ref()
    }

    pub fn from_user(&self) -> Option<&UserId> {
        self.from_user.as_ref()
    }

    pub fn author(&self) -> Option<&MessageAuthor> {
        self.author.as_ref()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MessageWindowAudienceFact {
    viewer: UserId,
    conversation: ConversationId,
    audience: MessageWindowAudience,
}

impl MessageWindowAudienceFact {
    pub fn admitted(
        viewer: UserId,
        conversation: ConversationId,
        participates: bool,
        public_widened: bool,
    ) -> Result<Self, MessageWindowCorruption> {
        let audience = if participates {
            MessageWindowAudience::Participating
        } else if public_widened {
            MessageWindowAudience::PublicNonmember
        } else {
            return Err(MessageWindowCorruption::Audience);
        };
        Ok(Self {
            viewer,
            conversation,
            audience,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MessageWindowHead {
    conversation: ConversationId,
    root: Option<ThreadRootId>,
    latest: MessageSeq,
}

impl MessageWindowHead {
    pub fn projected(
        conversation: ConversationId,
        root: Option<ThreadRootId>,
        latest: MessageSeq,
    ) -> Result<Self, MessageWindowCorruption> {
        exact_nonnegative("conversation_stream_heads.latest_seq", latest.get())?;
        Ok(Self {
            conversation,
            root,
            latest,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowBoundaryDirection {
    Before,
    After,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum WindowBoundaryState {
    Empty,
    At { pivot: MessageSeq, exists: bool },
}

/// One existence verdict tied to the actual stream and exact page edge it was
/// queried from. Empty pages have a distinct state whose answer is always false.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowBoundaryFact {
    conversation: ConversationId,
    root: Option<ThreadRootId>,
    direction: WindowBoundaryDirection,
    state: WindowBoundaryState,
}

impl WindowBoundaryFact {
    pub fn empty(
        conversation: ConversationId,
        root: Option<ThreadRootId>,
        direction: WindowBoundaryDirection,
    ) -> Self {
        Self {
            conversation,
            root,
            direction,
            state: WindowBoundaryState::Empty,
        }
    }

    pub fn at(
        conversation: ConversationId,
        root: Option<ThreadRootId>,
        direction: WindowBoundaryDirection,
        pivot: MessageSeq,
        exists: bool,
    ) -> Result<Self, MessageWindowCorruption> {
        exact_nonnegative("messages.window_boundary_seq", pivot.get())?;
        Ok(Self {
            conversation,
            root,
            direction,
            state: WindowBoundaryState::At { pivot, exists },
        })
    }

    fn exists(&self) -> bool {
        matches!(self.state, WindowBoundaryState::At { exists: true, .. })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowCount(i64);

impl WindowCount {
    pub fn new(value: i64) -> Result<Self, MessageWindowCorruption> {
        exact_nonnegative("count", value)?;
        Ok(Self(value))
    }

    pub const fn get(self) -> i64 {
        self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowTaskAssignee {
    principal: PrincipalIdentity,
    name: String,
}

impl WindowTaskAssignee {
    pub fn projected(principal: PrincipalIdentity, name: String) -> Self {
        Self { principal, name }
    }

    pub fn principal(&self) -> &PrincipalIdentity {
        &self.principal
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowLinkedTask {
    source_conversation: ConversationId,
    source_message: MessageId,
    id: TaskId,
    number: TaskNumber,
    title: TaskTitle,
    status: TaskStatus,
    assignee: Option<WindowTaskAssignee>,
}

impl WindowLinkedTask {
    pub fn projected(
        source_conversation: ConversationId,
        source_message: MessageId,
        id: TaskId,
        number: TaskNumber,
        title: TaskTitle,
        status: TaskStatus,
        assignee: Option<WindowTaskAssignee>,
    ) -> Self {
        Self {
            source_conversation,
            source_message,
            id,
            number,
            title,
            status,
            assignee,
        }
    }

    pub fn id(&self) -> &str {
        self.id.as_str()
    }

    pub const fn number(&self) -> TaskNumber {
        self.number
    }

    pub fn title(&self) -> &str {
        self.title.as_str()
    }

    pub const fn status(&self) -> TaskStatus {
        self.status
    }

    pub fn assignee(&self) -> Option<&WindowTaskAssignee> {
        self.assignee.as_ref()
    }
}

/// Metadata is decoded by the SQLite boundary because core deliberately has no
/// JSON parser dependency. The raw object and attachment snapshots travel as
/// one value so later consumers cannot replace only one half.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowMessageMetadata {
    json: Option<String>,
    attachments: Vec<AttachmentSnapshot>,
}

impl WindowMessageMetadata {
    pub fn decoded(json: Option<String>, attachments: Vec<AttachmentSnapshot>) -> Self {
        Self { json, attachments }
    }

    pub fn json(&self) -> Option<&str> {
        self.json.as_deref()
    }

    pub fn attachments(&self) -> &[AttachmentSnapshot] {
        &self.attachments
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowReaction {
    source_message: MessageId,
    viewer: WindowReactionViewer,
    aggregate: ReactionAggregate,
    first_created_at: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WindowReactionViewer {
    Participating(UserId),
    Neutral,
}

impl WindowReaction {
    pub fn projected(
        source_message: MessageId,
        viewer: WindowReactionViewer,
        aggregate: ReactionAggregate,
        first_created_at: i64,
    ) -> Result<Self, MessageWindowCorruption> {
        exact_nonnegative("message_reactions.first_created_at", first_created_at)?;
        Ok(Self {
            source_message,
            viewer,
            aggregate,
            first_created_at,
        })
    }

    pub fn aggregate(&self) -> &ReactionAggregate {
        &self.aggregate
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowMessageProjection {
    pub client_message_id: Option<String>,
    pub id: MessageId,
    pub conversation: ConversationId,
    pub author: WindowAuthorFact,
    pub content: String,
    pub metadata: WindowMessageMetadata,
    pub seq: MessageSeq,
    pub created_at_ms: i64,
    pub thread_root: Option<ThreadRootId>,
    pub linked_task: Option<WindowLinkedTask>,
    pub reactions: Vec<WindowReaction>,
    pub options: Option<super::message_options::MessageOptionsState>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectedWindowMessage(WindowMessageProjection);

impl SelectedWindowMessage {
    pub fn seal(projection: WindowMessageProjection) -> Result<Self, MessageWindowCorruption> {
        exact_nonnegative("messages.seq", projection.seq.get())?;
        exact_nonnegative("messages.created_at", projection.created_at_ms)?;
        if projection.linked_task.as_ref().is_some_and(|task| {
            task.source_conversation != projection.conversation
                || task.source_message != projection.id
        }) {
            return Err(MessageWindowCorruption::LinkedTask);
        }
        for attachment in projection.metadata.attachments() {
            exact_nonnegative("messages.metadata.attachments.size", attachment.size)?;
        }
        let mut emojis = HashSet::<String>::with_capacity(projection.reactions.len());
        let mut previous = None;
        for reaction in &projection.reactions {
            let emoji = reaction.aggregate().emoji().as_str();
            if !emojis.insert(emoji.to_owned()) {
                return Err(MessageWindowCorruption::DuplicateReaction);
            }
            if let Some((created_at, previous_emoji)) = previous
                && (reaction.first_created_at, emoji) <= (created_at, previous_emoji)
            {
                return Err(MessageWindowCorruption::ReactionOrder);
            }
            previous = Some((reaction.first_created_at, emoji));
        }
        Ok(Self(projection))
    }

    pub fn projection(&self) -> &WindowMessageProjection {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplyBadgeProjection {
    conversation: ConversationId,
    root: ThreadRootId,
    count: WindowCount,
    last_reply_at: i64,
    last_reply_seq: MessageSeq,
    repliers: Vec<WindowReplier>,
    replier_total: WindowCount,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowReplier {
    conversation: ConversationId,
    root: ThreadRootId,
    author: WindowResolvedAuthor,
    last_seq: MessageSeq,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct WindowResolvedAuthor {
    _source: WindowAuthorFact,
    author: MessageAuthor,
}

impl WindowResolvedAuthor {
    fn non_system(source: WindowAuthorFact) -> Result<Self, MessageWindowCorruption> {
        let author = source
            .author()
            .cloned()
            .ok_or(MessageWindowCorruption::ReplyProjection)?;
        Ok(Self {
            _source: source,
            author,
        })
    }
}

impl WindowReplier {
    pub fn projected(
        conversation: ConversationId,
        root: ThreadRootId,
        author: WindowAuthorFact,
        last_seq: MessageSeq,
    ) -> Result<Self, MessageWindowCorruption> {
        exact_nonnegative("messages.replier_last_seq", last_seq.get())?;
        let author = WindowResolvedAuthor::non_system(author)?;
        if author.author.stable_identity().is_none() {
            return Err(MessageWindowCorruption::ReplyProjection);
        }
        Ok(Self {
            conversation,
            root,
            author,
            last_seq,
        })
    }

    pub fn author(&self) -> &MessageAuthor {
        &self.author.author
    }

    pub const fn last_seq(&self) -> MessageSeq {
        self.last_seq
    }
}

impl ReplyBadgeProjection {
    pub fn sealed(
        conversation: ConversationId,
        root: ThreadRootId,
        count: WindowCount,
        last_reply_at: i64,
        last_reply_seq: MessageSeq,
        repliers: Vec<WindowReplier>,
        replier_total: WindowCount,
    ) -> Result<Self, MessageWindowCorruption> {
        exact_nonnegative("messages.last_reply_at", last_reply_at)?;
        exact_nonnegative("messages.last_reply_seq", last_reply_seq.get())?;
        if count.get() == 0
            || replier_total.get() > count.get()
            || repliers.len()
                != usize::try_from(replier_total.get().min(MAX_REPLY_PREVIEWS as i64))
                    .unwrap_or_default()
        {
            return Err(MessageWindowCorruption::ReplyProjection);
        }
        let mut identities = HashSet::<PrincipalIdentity>::with_capacity(repliers.len());
        for replier in &repliers {
            if replier.conversation != conversation
                || replier.root != root
                || replier.last_seq > last_reply_seq
            {
                return Err(MessageWindowCorruption::ReplyProjection);
            }
            let identity = replier
                .author()
                .stable_identity()
                .ok_or(MessageWindowCorruption::ReplyProjection)?;
            if !identities.insert(identity) {
                return Err(MessageWindowCorruption::ReplyProjection);
            }
        }
        if repliers
            .windows(2)
            .any(|pair| pair[0].last_seq <= pair[1].last_seq)
        {
            return Err(MessageWindowCorruption::ReplyProjection);
        }
        Ok(Self {
            conversation,
            root,
            count,
            last_reply_at,
            last_reply_seq,
            repliers,
            replier_total,
        })
    }

    pub const fn count(&self) -> WindowCount {
        self.count
    }

    pub const fn last_reply_at(&self) -> i64 {
        self.last_reply_at
    }

    pub fn repliers(&self) -> &[WindowReplier] {
        &self.repliers
    }

    pub const fn replier_total(&self) -> WindowCount {
        self.replier_total
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowReplyPreview {
    conversation: ConversationId,
    root: ThreadRootId,
    message: MessageId,
    seq: MessageSeq,
    created_at: i64,
    author: WindowResolvedAuthor,
    preview: String,
    attachment_count: WindowCount,
    first_attachment_label: Option<AttachmentLabel>,
}

impl WindowReplyPreview {
    #[allow(clippy::too_many_arguments)]
    pub fn sealed(
        conversation: ConversationId,
        root: ThreadRootId,
        message: MessageId,
        seq: MessageSeq,
        created_at: i64,
        author: WindowAuthorFact,
        content: &str,
        attachment_count: WindowCount,
        first_attachment_label: Option<AttachmentLabel>,
    ) -> Result<Self, MessageWindowCorruption> {
        exact_nonnegative("messages.reply_preview_seq", seq.get())?;
        exact_nonnegative("messages.reply_preview_created_at", created_at)?;
        let author = WindowResolvedAuthor::non_system(author)?;
        if (attachment_count.get() == 0) != first_attachment_label.is_none() {
            return Err(MessageWindowCorruption::ReplyProjection);
        }
        Ok(Self {
            conversation,
            root,
            message,
            seq,
            created_at,
            author,
            preview: reply_preview(content),
            attachment_count,
            first_attachment_label,
        })
    }

    pub fn message(&self) -> &MessageId {
        &self.message
    }

    pub const fn seq(&self) -> MessageSeq {
        self.seq
    }

    pub fn author(&self) -> &MessageAuthor {
        &self.author.author
    }

    pub fn preview(&self) -> &str {
        &self.preview
    }

    pub const fn attachment_count(&self) -> WindowCount {
        self.attachment_count
    }

    pub const fn first_attachment_label(&self) -> Option<AttachmentLabel> {
        self.first_attachment_label
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowRootReplies {
    conversation: ConversationId,
    root: ThreadRootId,
    count: WindowCount,
    summary: Option<ReplyBadgeProjection>,
    previews: Vec<WindowReplyPreview>,
}

impl WindowRootReplies {
    pub fn sealed(
        conversation: ConversationId,
        root: ThreadRootId,
        count: WindowCount,
        summary: Option<ReplyBadgeProjection>,
        previews: Vec<WindowReplyPreview>,
    ) -> Result<Self, MessageWindowCorruption> {
        if (count.get() == 0) != (summary.is_none() && previews.is_empty())
            || summary
                .as_ref()
                .is_some_and(|summary| summary.conversation != conversation || summary.root != root)
            || summary
                .as_ref()
                .is_some_and(|summary| summary.count() != count)
            || previews.is_empty() != (count.get() == 0)
            || previews.len()
                != usize::try_from(count.get().min(MAX_REPLY_PREVIEWS as i64)).unwrap_or_default()
            || previews
                .iter()
                .any(|preview| preview.conversation != conversation || preview.root != root)
            || previews.windows(2).any(|pair| pair[0].seq >= pair[1].seq)
            || summary
                .as_ref()
                .zip(previews.last())
                .is_some_and(|(summary, preview)| {
                    summary.last_reply_seq != preview.seq
                        || summary.last_reply_at != preview.created_at
                })
        {
            return Err(MessageWindowCorruption::ReplyProjection);
        }
        let mut messages = HashSet::with_capacity(previews.len());
        if previews
            .iter()
            .any(|preview| !messages.insert(preview.message.as_str()))
        {
            return Err(MessageWindowCorruption::ReplyProjection);
        }
        Ok(Self {
            conversation,
            root,
            count,
            summary,
            previews,
        })
    }

    pub fn root(&self) -> &ThreadRootId {
        &self.root
    }

    pub const fn count(&self) -> WindowCount {
        self.count
    }

    pub fn summary(&self) -> Option<&ReplyBadgeProjection> {
        self.summary.as_ref()
    }

    pub fn previews(&self) -> &[WindowReplyPreview] {
        &self.previews
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversationMessageWindow {
    audience: MessageWindowAudienceFact,
    request: ConversationMessageWindowRequest,
    stream: ResolvedMessageWindowStream,
    stream_head: MessageWindowHead,
    messages: Vec<SelectedWindowMessage>,
    roots: Vec<WindowRootReplies>,
    has_more_before: bool,
    has_more_after: bool,
}

impl ConversationMessageWindow {
    #[allow(clippy::too_many_arguments)]
    pub fn sealed(
        audience: MessageWindowAudienceFact,
        request: ConversationMessageWindowRequest,
        stream: ResolvedMessageWindowStream,
        stream_head: MessageWindowHead,
        messages: Vec<SelectedWindowMessage>,
        roots: Vec<WindowRootReplies>,
        before: WindowBoundaryFact,
        after: WindowBoundaryFact,
    ) -> Result<Self, MessageWindowCorruption> {
        let viewer = &audience.viewer;
        let conversation = &audience.conversation;
        if stream.conversation != *conversation
            || stream.requested() != request.stream()
            || stream_head.conversation != *conversation
            || stream_head.root.as_ref() != stream.root()
            || messages.len() > usize::from(request.query.limit)
        {
            return Err(MessageWindowCorruption::PageCoordinate);
        }
        let mut message_ids = HashSet::with_capacity(messages.len());
        for message in &messages {
            let projection = message.projection();
            if projection.conversation != *conversation
                || !stream_contains(&stream, projection)
                || !message_ids.insert(projection.id.as_str())
                || projection.reactions.iter().any(|reaction| {
                    reaction.source_message != projection.id
                        || !reaction_viewer_matches(audience.audience, viewer, reaction)
                })
            {
                return Err(MessageWindowCorruption::PageCoordinate);
            }
        }
        if messages
            .windows(2)
            .any(|pair| pair[0].projection().seq >= pair[1].projection().seq)
        {
            return Err(MessageWindowCorruption::PageOrder);
        }
        match &request.query.anchor {
            MessageWindowAnchor::Latest => {}
            MessageWindowAnchor::Before(seq) => {
                if messages
                    .iter()
                    .any(|message| message.projection().seq >= *seq)
                {
                    return Err(MessageWindowCorruption::OutsideAnchor);
                }
            }
            MessageWindowAnchor::After(seq) => {
                if messages
                    .iter()
                    .any(|message| message.projection().seq <= *seq)
                {
                    return Err(MessageWindowCorruption::OutsideAnchor);
                }
            }
            MessageWindowAnchor::Around(id) => {
                if !messages
                    .iter()
                    .any(|message| message.projection().id == *id)
                {
                    return Err(MessageWindowCorruption::OutsideAnchor);
                }
            }
        }

        let eligible_roots = messages
            .iter()
            .filter_map(|message| {
                let projection = message.projection();
                (projection.thread_root.is_none()
                    && projection.author.from_kind() != FromKind::System)
                    .then_some(projection.id.as_str())
            })
            .collect::<HashSet<_>>();
        let mut projected_roots = HashSet::with_capacity(roots.len());
        for root in &roots {
            if root.conversation != *conversation
                || !eligible_roots.contains(root.root.as_str())
                || !projected_roots.insert(root.root.as_str())
            {
                return Err(MessageWindowCorruption::RootSet);
            }
            let root_seq = messages
                .iter()
                .find(|message| message.projection().id.as_str() == root.root.as_str())
                .map(|message| message.projection().seq)
                .ok_or(MessageWindowCorruption::RootSet)?;
            if root.summary.as_ref().is_some_and(|summary| {
                summary.conversation != *conversation
                    || summary.root != root.root
                    || summary.last_reply_seq <= root_seq
            }) || root.previews.iter().any(|preview| preview.seq <= root_seq)
            {
                return Err(MessageWindowCorruption::ReplyProjection);
            }
        }
        if projected_roots != eligible_roots {
            return Err(MessageWindowCorruption::RootSet);
        }
        validate_boundary(
            &before,
            &stream,
            messages.first(),
            WindowBoundaryDirection::Before,
        )?;
        validate_boundary(
            &after,
            &stream,
            messages.last(),
            WindowBoundaryDirection::After,
        )?;
        let has_more_before = before.exists();
        let has_more_after = after.exists();
        if matches!(request.query.anchor, MessageWindowAnchor::Latest) && has_more_after {
            return Err(MessageWindowCorruption::PageCoordinate);
        }
        Ok(Self {
            audience,
            request,
            stream,
            stream_head,
            messages,
            roots,
            has_more_before,
            has_more_after,
        })
    }

    pub fn bind(
        self,
        requested_viewer: &UserId,
        requested_conversation: &ConversationId,
        requested: &ConversationMessageWindowRequest,
    ) -> Result<BoundConversationMessageWindow, MessageWindowIdentityMismatch> {
        if &self.audience.viewer != requested_viewer
            || &self.audience.conversation != requested_conversation
            || &self.request != requested
        {
            return Err(MessageWindowIdentityMismatch);
        }
        Ok(BoundConversationMessageWindow(self))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundConversationMessageWindow(ConversationMessageWindow);

impl BoundConversationMessageWindow {
    pub const fn audience(&self) -> MessageWindowAudience {
        self.0.audience.audience
    }

    pub fn stream(&self) -> &ResolvedMessageWindowStream {
        &self.0.stream
    }

    pub const fn stream_head(&self) -> MessageSeq {
        self.0.stream_head.latest
    }

    pub fn messages(&self) -> &[SelectedWindowMessage] {
        &self.0.messages
    }

    pub fn roots(&self) -> &[WindowRootReplies] {
        &self.0.roots
    }

    pub const fn has_more_before(&self) -> bool {
        self.0.has_more_before
    }

    pub const fn has_more_after(&self) -> bool {
        self.0.has_more_after
    }
}

fn validate_boundary(
    fact: &WindowBoundaryFact,
    stream: &ResolvedMessageWindowStream,
    edge: Option<&SelectedWindowMessage>,
    direction: WindowBoundaryDirection,
) -> Result<(), MessageWindowCorruption> {
    if fact.conversation != stream.conversation
        || fact.root.as_ref() != stream.root()
        || fact.direction != direction
    {
        return Err(MessageWindowCorruption::PageCoordinate);
    }
    match (&fact.state, edge) {
        (WindowBoundaryState::Empty, None) => Ok(()),
        (WindowBoundaryState::At { pivot, .. }, Some(message))
            if *pivot == message.projection().seq =>
        {
            Ok(())
        }
        _ => Err(MessageWindowCorruption::PageCoordinate),
    }
}

fn stream_contains(
    stream: &ResolvedMessageWindowStream,
    message: &WindowMessageProjection,
) -> bool {
    match stream.root() {
        None => message.thread_root.is_none(),
        Some(root) => {
            message.thread_root.as_ref() == Some(root)
                || (message.thread_root.is_none() && message.id.as_str() == root.as_str())
        }
    }
}

fn reaction_viewer_matches(
    audience: MessageWindowAudience,
    viewer: &UserId,
    reaction: &WindowReaction,
) -> bool {
    match (&reaction.viewer, audience) {
        (WindowReactionViewer::Participating(source), MessageWindowAudience::Participating) => {
            source == viewer
        }
        (WindowReactionViewer::Neutral, MessageWindowAudience::PublicNonmember) => {
            !reaction.aggregate.viewer_reacted()
        }
        _ => false,
    }
}

fn exact_nonnegative(field: &'static str, value: i64) -> Result<(), MessageWindowCorruption> {
    if (0..=MAX_JAVASCRIPT_INTEGER).contains(&value) {
        Ok(())
    } else {
        Err(MessageWindowCorruption::UnsafeNumber { field })
    }
}

fn reply_preview(content: &str) -> String {
    let mut compact = String::with_capacity(content.len());
    let mut pending_space = false;
    for scalar in content.chars() {
        if is_ecmascript_space(scalar) {
            if !compact.is_empty() {
                pending_space = true;
            }
        } else {
            if pending_space {
                compact.push(' ');
                pending_space = false;
            }
            compact.push(scalar);
        }
    }
    if compact.encode_utf16().count() <= REPLY_PREVIEW_MAX_UTF16 {
        return compact;
    }
    let mut units = 0_usize;
    let mut boundary = 0_usize;
    for (offset, scalar) in compact.char_indices() {
        let next = units + scalar.len_utf16();
        if next > REPLY_PREVIEW_MAX_UTF16 {
            break;
        }
        units = next;
        boundary = offset + scalar.len_utf8();
    }
    compact.truncate(boundary);
    compact
}

fn is_ecmascript_space(value: char) -> bool {
    matches!(
        value,
        '\u{0009}'
            | '\u{000A}'
            | '\u{000B}'
            | '\u{000C}'
            | '\u{000D}'
            | '\u{0020}'
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum MessageWindowCorruption {
    #[error("message-window field {field} is not an exact nonnegative JavaScript integer")]
    UnsafeNumber { field: &'static str },
    #[error("resolved message stream does not match its selected source row")]
    StreamCoordinate,
    #[error("message author does not match raw persisted provenance")]
    AuthorProvenance,
    #[error("message reactions repeat an emoji")]
    DuplicateReaction,
    #[error("message reactions are not in stable first-reaction order")]
    ReactionOrder,
    #[error("linked task projection is malformed")]
    LinkedTask,
    #[error("message-window access fact is neither participation nor public widening")]
    Audience,
    #[error("reply badge or preview projection is malformed")]
    ReplyProjection,
    #[error("message-window page coordinate, cardinality, audience, or head is inconsistent")]
    PageCoordinate,
    #[error("message-window rows are not in strict ascending sequence order")]
    PageOrder,
    #[error("message-window row is outside its requested anchor")]
    OutsideAnchor,
    #[error("message-window reply-root sidecars do not equal the eligible root set")]
    RootSet,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("message-window viewer, conversation, stream, or query differs from the request")]
pub struct MessageWindowIdentityMismatch;

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum MessageWindowRefusal {
    #[error("conversation not found")]
    NotFound,
    #[error("viewer may not read the conversation")]
    Forbidden,
    #[error("thread root message was not found in the conversation")]
    ThreadRootNotFound,
    #[error("around-message anchor was not found in the conversation or stream")]
    AnchorNotFound,
}

pub trait ConversationMessageWindowReadPort {
    type Error: Error + Send + Sync + 'static;

    /// Authorizes the viewer in the snapshot used for the returned window.
    fn read(
        &self,
        viewer: &UserId,
        conversation: &ConversationId,
        request: ConversationMessageWindowRequest,
    ) -> impl Future<
        Output = Result<Result<ConversationMessageWindow, MessageWindowRefusal>, Self::Error>,
    > + Send;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation_messaging::messaging::collaboration::reaction::{
        ReactionCount, ReactionEmoji,
    };

    fn conversation(id: &str) -> ConversationId {
        ConversationId::parse(id).unwrap()
    }

    fn user(id: &str) -> UserId {
        UserId::parse(id).unwrap()
    }

    fn message_id(id: &str) -> MessageId {
        MessageId::parse(id).unwrap()
    }

    fn thread_root(id: &str) -> ThreadRootId {
        ThreadRootId::parse(id).unwrap()
    }

    fn seq(value: i64) -> MessageSeq {
        MessageSeq::try_from(value).unwrap()
    }

    fn author(id: &str) -> MessageAuthor {
        MessageAuthor::active_human(user(id), id.to_owned(), id.to_owned(), None)
    }

    fn author_fact(id: &str) -> WindowAuthorFact {
        WindowAuthorFact::sealed(
            FromKind::User,
            None,
            Some(user(id)),
            Some(PrincipalIdentity::Human(user(id))),
            Some(id.to_owned()),
            Some(id.to_owned()),
            Some(author(id)),
        )
        .unwrap()
    }

    fn system_author_fact() -> WindowAuthorFact {
        WindowAuthorFact::sealed(FromKind::System, None, None, None, None, None, None).unwrap()
    }

    fn unresolved_author_fact() -> WindowAuthorFact {
        WindowAuthorFact::sealed(
            FromKind::User,
            None,
            None,
            None,
            None,
            None,
            Some(MessageAuthor::unresolved(PrincipalKind::Human)),
        )
        .unwrap()
    }

    fn deleted_author_fact(id: &str) -> WindowAuthorFact {
        WindowAuthorFact::sealed(
            FromKind::User,
            None,
            None,
            Some(PrincipalIdentity::Human(user(id))),
            Some(id.to_owned()),
            Some(id.to_owned()),
            Some(MessageAuthor::deleted(
                PrincipalIdentity::Human(user(id)),
                id.to_owned(),
                id.to_owned(),
            )),
        )
        .unwrap()
    }

    fn audience(
        viewer: &str,
        conversation_id: &str,
        kind: MessageWindowAudience,
    ) -> MessageWindowAudienceFact {
        MessageWindowAudienceFact::admitted(
            user(viewer),
            conversation(conversation_id),
            kind == MessageWindowAudience::Participating,
            kind == MessageWindowAudience::PublicNonmember,
        )
        .unwrap()
    }

    fn head(conversation_id: &str, root: Option<&str>, latest: i64) -> MessageWindowHead {
        MessageWindowHead::projected(
            conversation(conversation_id),
            root.map(thread_root),
            seq(latest),
        )
        .unwrap()
    }

    fn boundary(
        conversation_id: &str,
        root: Option<&str>,
        direction: WindowBoundaryDirection,
        pivot: Option<i64>,
        exists: bool,
    ) -> WindowBoundaryFact {
        match pivot {
            Some(pivot) => WindowBoundaryFact::at(
                conversation(conversation_id),
                root.map(thread_root),
                direction,
                seq(pivot),
                exists,
            )
            .unwrap(),
            None => {
                assert!(!exists);
                WindowBoundaryFact::empty(
                    conversation(conversation_id),
                    root.map(thread_root),
                    direction,
                )
            }
        }
    }

    fn selected(
        conversation_id: &str,
        id: &str,
        sequence: i64,
        root: Option<&str>,
        viewer_reacted: bool,
    ) -> SelectedWindowMessage {
        let reactions = if viewer_reacted {
            vec![
                WindowReaction::projected(
                    message_id(id),
                    WindowReactionViewer::Participating(user("viewer")),
                    ReactionAggregate::projected(
                        ReactionEmoji::parse("👍").unwrap(),
                        ReactionCount::new(1).unwrap(),
                        true,
                    ),
                    20,
                )
                .unwrap(),
            ]
        } else {
            Vec::new()
        };
        SelectedWindowMessage::seal(WindowMessageProjection {
            client_message_id: None,
            id: message_id(id),
            conversation: conversation(conversation_id),
            author: author_fact("author"),
            content: format!("message {id}"),
            metadata: WindowMessageMetadata::decoded(None, Vec::new()),
            seq: seq(sequence),
            created_at_ms: sequence * 10,
            thread_root: root.map(thread_root),
            linked_task: None,
            reactions,
            options: None,
        })
        .unwrap()
    }

    fn empty_root(conversation_id: &str, root: &str) -> WindowRootReplies {
        WindowRootReplies::sealed(
            conversation(conversation_id),
            thread_root(root),
            WindowCount::new(0).unwrap(),
            None,
            Vec::new(),
        )
        .unwrap()
    }

    #[test]
    fn query_and_thread_resolution_are_closed_over_actual_coordinates() {
        assert!(
            ConversationMessageWindowRequest::new(
                MessageWindowStreamRequest::Mainline,
                0,
                MessageWindowAnchor::Latest,
            )
            .is_err()
        );
        assert!(
            ConversationMessageWindowRequest::new(
                MessageWindowStreamRequest::Mainline,
                501,
                MessageWindowAnchor::Latest,
            )
            .is_err()
        );
        assert!(
            ConversationMessageWindowRequest::new(
                MessageWindowStreamRequest::Mainline,
                1,
                MessageWindowAnchor::Before(seq(MAX_JAVASCRIPT_INTEGER + 1)),
            )
            .is_err()
        );

        let resolved = ResolvedMessageWindowStream::thread(
            conversation("c"),
            message_id("reply"),
            message_id("reply"),
            conversation("c"),
            Some(thread_root("root")),
        )
        .unwrap();
        assert_eq!(resolved.root(), Some(&thread_root("root")));

        assert!(
            ResolvedMessageWindowStream::thread(
                conversation("c"),
                message_id("reply"),
                message_id("other"),
                conversation("c"),
                Some(thread_root("root")),
            )
            .is_err()
        );
        assert!(
            ResolvedMessageWindowStream::thread(
                conversation("c"),
                message_id("reply"),
                message_id("reply"),
                conversation("foreign"),
                Some(thread_root("root")),
            )
            .is_err()
        );

        let thread = ResolvedMessageWindowStream::thread(
            conversation("c"),
            message_id("root"),
            message_id("root"),
            conversation("c"),
            None,
        )
        .unwrap();
        assert_eq!(
            ConversationMessageWindow::sealed(
                audience("viewer", "c", MessageWindowAudience::Participating),
                ConversationMessageWindowRequest::new(
                    MessageWindowStreamRequest::Mainline,
                    1,
                    MessageWindowAnchor::Latest,
                )
                .unwrap(),
                thread,
                head("c", Some("root"), 0),
                Vec::new(),
                Vec::new(),
                boundary(
                    "c",
                    Some("root"),
                    WindowBoundaryDirection::Before,
                    None,
                    false
                ),
                boundary(
                    "c",
                    Some("root"),
                    WindowBoundaryDirection::After,
                    None,
                    false
                ),
            ),
            Err(MessageWindowCorruption::PageCoordinate)
        );
    }

    #[test]
    fn selected_rows_seal_author_and_reaction_authority() {
        assert_eq!(
            WindowAuthorFact::sealed(
                FromKind::System,
                None,
                None,
                None,
                None,
                None,
                Some(author("foreign")),
            ),
            Err(MessageWindowCorruption::AuthorProvenance)
        );
        assert_eq!(
            WindowAuthorFact::sealed(
                FromKind::User,
                None,
                None,
                Some(PrincipalIdentity::Human(user("deleted-a"))),
                Some("Deleted A".to_owned()),
                Some("Deleted A".to_owned()),
                Some(author("active-b")),
            ),
            Err(MessageWindowCorruption::AuthorProvenance)
        );
        assert_eq!(
            WindowAuthorFact::sealed(
                FromKind::User,
                None,
                None,
                Some(PrincipalIdentity::Human(user("deleted-a"))),
                Some("Deleted A".to_owned()),
                Some("Deleted A".to_owned()),
                Some(MessageAuthor::deleted(
                    PrincipalIdentity::Human(user("deleted-b")),
                    "Deleted B".to_owned(),
                    "Deleted B".to_owned(),
                )),
            ),
            Err(MessageWindowCorruption::AuthorProvenance)
        );
        assert!(
            WindowAuthorFact::sealed(
                FromKind::User,
                None,
                None,
                Some(PrincipalIdentity::Human(user("deleted-a"))),
                Some("Deleted A".to_owned()),
                Some("Deleted A".to_owned()),
                Some(MessageAuthor::deleted(
                    PrincipalIdentity::Human(user("deleted-a")),
                    "Deleted A".to_owned(),
                    "Deleted A".to_owned(),
                )),
            )
            .is_ok()
        );
        assert!(
            WindowAuthorFact::sealed(
                FromKind::User,
                None,
                None,
                None,
                None,
                None,
                Some(MessageAuthor::unresolved(PrincipalKind::Human)),
            )
            .is_ok()
        );

        let reaction = |emoji: &str, at| {
            WindowReaction::projected(
                message_id("m"),
                WindowReactionViewer::Participating(user("viewer")),
                ReactionAggregate::projected(
                    ReactionEmoji::parse(emoji).unwrap(),
                    ReactionCount::new(1).unwrap(),
                    false,
                ),
                at,
            )
            .unwrap()
        };
        let mut duplicate = selected("c", "m", 1, None, false).0;
        duplicate.reactions = vec![reaction("👍", 1), reaction("👍", 2)];
        assert_eq!(
            SelectedWindowMessage::seal(duplicate),
            Err(MessageWindowCorruption::DuplicateReaction)
        );
        let mut unordered = selected("c", "m", 1, None, false).0;
        unordered.reactions = vec![reaction("🎉", 2), reaction("👍", 1)];
        assert_eq!(
            SelectedWindowMessage::seal(unordered),
            Err(MessageWindowCorruption::ReactionOrder)
        );

        let mut foreign_task = selected("c", "m", 1, None, false).0;
        foreign_task.linked_task = Some(WindowLinkedTask::projected(
            conversation("c"),
            message_id("other"),
            TaskId::parse("task").unwrap(),
            TaskNumber::new(1).unwrap(),
            TaskTitle::parse("Task").unwrap(),
            TaskStatus::Todo,
            None,
        ));
        assert_eq!(
            SelectedWindowMessage::seal(foreign_task),
            Err(MessageWindowCorruption::LinkedTask)
        );
    }

    #[test]
    fn system_actor_provenance_does_not_become_a_display_author() {
        for (agent, human) in [
            (None, Some(user("initiator"))),
            (Some(AgentId::parse("initiator").unwrap()), None),
        ] {
            let fact = WindowAuthorFact::sealed(
                FromKind::System,
                agent.clone(),
                human.clone(),
                None,
                None,
                None,
                None,
            )
            .unwrap();
            assert_eq!(fact.from_agent(), agent.as_ref());
            assert_eq!(fact.from_user(), human.as_ref());
            assert!(fact.author().is_none());
            assert_eq!(
                WindowAuthorFact::sealed(
                    FromKind::System,
                    agent,
                    human,
                    None,
                    None,
                    None,
                    Some(author("foreign")),
                ),
                Err(MessageWindowCorruption::AuthorProvenance)
            );
        }
    }

    #[test]
    fn page_binds_request_and_neutralizes_the_complete_public_private_surface() {
        let viewer = user("viewer");
        let channel = conversation("c");
        let request = ConversationMessageWindowRequest::new(
            MessageWindowStreamRequest::Mainline,
            2,
            MessageWindowAnchor::Latest,
        )
        .unwrap();
        let page = ConversationMessageWindow::sealed(
            audience("viewer", "c", MessageWindowAudience::Participating),
            request.clone(),
            ResolvedMessageWindowStream::mainline(channel.clone()),
            head("c", None, 1),
            vec![selected("c", "root", 1, None, false)],
            vec![empty_root("c", "root")],
            boundary("c", None, WindowBoundaryDirection::Before, Some(1), false),
            boundary("c", None, WindowBoundaryDirection::After, Some(1), false),
        )
        .unwrap();
        assert!(page.clone().bind(&viewer, &channel, &request).is_ok());
        assert!(page.bind(&user("other"), &channel, &request).is_err());

        assert_eq!(
            ConversationMessageWindow::sealed(
                audience("viewer", "c", MessageWindowAudience::PublicNonmember),
                request.clone(),
                ResolvedMessageWindowStream::mainline(channel),
                head("c", None, 1),
                vec![selected("c", "root", 1, None, true)],
                vec![empty_root("c", "root")],
                boundary("c", None, WindowBoundaryDirection::Before, Some(1), false),
                boundary("c", None, WindowBoundaryDirection::After, Some(1), false),
            ),
            Err(MessageWindowCorruption::PageCoordinate)
        );

        let neutral = WindowReaction::projected(
            message_id("root"),
            WindowReactionViewer::Neutral,
            ReactionAggregate::projected(
                ReactionEmoji::parse("👍").unwrap(),
                ReactionCount::new(1).unwrap(),
                false,
            ),
            20,
        )
        .unwrap();
        let mut public_root = selected("c", "root", 1, None, false).0;
        public_root.reactions = vec![neutral];
        assert!(
            ConversationMessageWindow::sealed(
                audience("viewer", "c", MessageWindowAudience::PublicNonmember),
                request.clone(),
                ResolvedMessageWindowStream::mainline(conversation("c")),
                head("c", None, 1),
                vec![SelectedWindowMessage::seal(public_root).unwrap()],
                vec![empty_root("c", "root")],
                boundary("c", None, WindowBoundaryDirection::Before, Some(1), false),
                boundary("c", None, WindowBoundaryDirection::After, Some(1), false),
            )
            .is_ok()
        );

        let mut foreign_viewer = selected("c", "root", 1, None, false).0;
        foreign_viewer.reactions = vec![
            WindowReaction::projected(
                message_id("root"),
                WindowReactionViewer::Participating(user("other")),
                ReactionAggregate::projected(
                    ReactionEmoji::parse("👍").unwrap(),
                    ReactionCount::new(1).unwrap(),
                    false,
                ),
                20,
            )
            .unwrap(),
        ];
        assert_eq!(
            ConversationMessageWindow::sealed(
                audience("viewer", "c", MessageWindowAudience::Participating),
                request,
                ResolvedMessageWindowStream::mainline(conversation("c")),
                head("c", None, 1),
                vec![SelectedWindowMessage::seal(foreign_viewer).unwrap()],
                vec![empty_root("c", "root")],
                boundary("c", None, WindowBoundaryDirection::Before, Some(1), false),
                boundary("c", None, WindowBoundaryDirection::After, Some(1), false),
            ),
            Err(MessageWindowCorruption::PageCoordinate)
        );
    }

    #[test]
    fn page_requires_root_sidecars_and_around_anchor_but_allows_a_lagging_head() {
        let channel = conversation("c");
        let latest = ConversationMessageWindowRequest::new(
            MessageWindowStreamRequest::Mainline,
            3,
            MessageWindowAnchor::Latest,
        )
        .unwrap();
        assert_eq!(
            ConversationMessageWindow::sealed(
                audience("viewer", "c", MessageWindowAudience::Participating),
                latest.clone(),
                ResolvedMessageWindowStream::mainline(channel.clone()),
                head("c", None, 2),
                vec![selected("c", "root", 1, None, false)],
                Vec::new(),
                boundary("c", None, WindowBoundaryDirection::Before, Some(1), false),
                boundary("c", None, WindowBoundaryDirection::After, Some(1), false),
            ),
            Err(MessageWindowCorruption::RootSet)
        );

        let around = ConversationMessageWindowRequest::new(
            MessageWindowStreamRequest::Mainline,
            3,
            MessageWindowAnchor::Around(message_id("missing")),
        )
        .unwrap();
        assert_eq!(
            ConversationMessageWindow::sealed(
                audience("viewer", "c", MessageWindowAudience::Participating),
                around,
                ResolvedMessageWindowStream::mainline(channel.clone()),
                head("c", None, 2),
                vec![selected("c", "root", 1, None, false)],
                vec![empty_root("c", "root")],
                boundary("c", None, WindowBoundaryDirection::Before, Some(1), false),
                boundary("c", None, WindowBoundaryDirection::After, Some(1), true),
            ),
            Err(MessageWindowCorruption::OutsideAnchor)
        );

        let thread = ResolvedMessageWindowStream::thread(
            channel.clone(),
            message_id("root"),
            message_id("root"),
            channel.clone(),
            None,
        )
        .unwrap();
        let thread_request = ConversationMessageWindowRequest::new(
            MessageWindowStreamRequest::Thread(message_id("root")),
            3,
            MessageWindowAnchor::Latest,
        )
        .unwrap();
        assert!(
            ConversationMessageWindow::sealed(
                audience("viewer", "c", MessageWindowAudience::Participating),
                thread_request.clone(),
                thread.clone(),
                head("c", Some("root"), 0),
                vec![selected("c", "root", 1, None, false)],
                vec![empty_root("c", "root")],
                boundary(
                    "c",
                    Some("root"),
                    WindowBoundaryDirection::Before,
                    Some(1),
                    false
                ),
                boundary(
                    "c",
                    Some("root"),
                    WindowBoundaryDirection::After,
                    Some(1),
                    false
                ),
            )
            .is_ok()
        );
        assert!(
            ConversationMessageWindow::sealed(
                audience("viewer", "c", MessageWindowAudience::Participating),
                thread_request,
                thread,
                head("c", Some("root"), 0),
                vec![
                    selected("c", "root", 1, None, false),
                    selected("c", "reply", 2, Some("root"), false)
                ],
                vec![empty_root("c", "root")],
                boundary(
                    "c",
                    Some("root"),
                    WindowBoundaryDirection::Before,
                    Some(1),
                    false
                ),
                boundary(
                    "c",
                    Some("root"),
                    WindowBoundaryDirection::After,
                    Some(2),
                    false
                ),
            )
            .is_ok(),
            "an auxiliary head behind persisted replies must not hide the page"
        );
    }

    #[test]
    fn head_and_boundary_facts_bind_actual_stream_edges() {
        let latest = ConversationMessageWindowRequest::new(
            MessageWindowStreamRequest::Mainline,
            1,
            MessageWindowAnchor::Latest,
        )
        .unwrap();
        let window = |request: ConversationMessageWindowRequest,
                      stream_head: MessageWindowHead,
                      before: WindowBoundaryFact,
                      after: WindowBoundaryFact| {
            ConversationMessageWindow::sealed(
                audience("viewer", "c", MessageWindowAudience::Participating),
                request,
                ResolvedMessageWindowStream::mainline(conversation("c")),
                stream_head,
                vec![selected("c", "root", 1, None, false)],
                vec![empty_root("c", "root")],
                before,
                after,
            )
        };

        assert_eq!(
            window(
                latest.clone(),
                head("foreign", None, 10),
                boundary("c", None, WindowBoundaryDirection::Before, Some(1), false),
                boundary("c", None, WindowBoundaryDirection::After, Some(1), false),
            ),
            Err(MessageWindowCorruption::PageCoordinate)
        );
        assert_eq!(
            window(
                latest.clone(),
                head("c", Some("sibling-thread"), 10),
                boundary("c", None, WindowBoundaryDirection::Before, Some(1), false),
                boundary("c", None, WindowBoundaryDirection::After, Some(1), false),
            ),
            Err(MessageWindowCorruption::PageCoordinate)
        );
        assert_eq!(
            ConversationMessageWindow::sealed(
                audience("viewer", "c", MessageWindowAudience::Participating),
                latest.clone(),
                ResolvedMessageWindowStream::mainline(conversation("c")),
                head("c", None, 0),
                Vec::new(),
                Vec::new(),
                WindowBoundaryFact::at(
                    conversation("c"),
                    None,
                    WindowBoundaryDirection::Before,
                    seq(0),
                    true,
                )
                .unwrap(),
                boundary("c", None, WindowBoundaryDirection::After, None, false),
            ),
            Err(MessageWindowCorruption::PageCoordinate)
        );
        assert_eq!(
            window(
                latest.clone(),
                head("c", None, 1),
                boundary(
                    "foreign",
                    None,
                    WindowBoundaryDirection::Before,
                    Some(1),
                    true,
                ),
                boundary("c", None, WindowBoundaryDirection::After, Some(1), false),
            ),
            Err(MessageWindowCorruption::PageCoordinate)
        );
        let has_older = window(
            latest,
            head("c", None, 1),
            boundary("c", None, WindowBoundaryDirection::Before, Some(1), true),
            boundary("c", None, WindowBoundaryDirection::After, Some(1), false),
        )
        .unwrap();
        assert!(has_older.has_more_before);
        assert!(!has_older.has_more_after);

        let before = ConversationMessageWindowRequest::new(
            MessageWindowStreamRequest::Mainline,
            1,
            MessageWindowAnchor::Before(seq(10)),
        )
        .unwrap();
        let has_newer = window(
            before,
            head("c", None, 10),
            boundary("c", None, WindowBoundaryDirection::Before, Some(1), false),
            boundary("c", None, WindowBoundaryDirection::After, Some(1), true),
        )
        .unwrap();
        assert!(!has_newer.has_more_before);
        assert!(has_newer.has_more_after);
    }

    #[test]
    fn reply_sidecars_seal_distinct_recency_and_preview_algebra() {
        assert_eq!(
            WindowReplier::projected(
                conversation("c"),
                thread_root("root"),
                system_author_fact(),
                seq(2),
            ),
            Err(MessageWindowCorruption::ReplyProjection)
        );
        assert_eq!(
            WindowReplier::projected(
                conversation("c"),
                thread_root("root"),
                unresolved_author_fact(),
                seq(2),
            ),
            Err(MessageWindowCorruption::ReplyProjection)
        );
        assert!(
            WindowReplier::projected(
                conversation("c"),
                thread_root("root"),
                deleted_author_fact("deleted"),
                seq(2),
            )
            .is_ok()
        );

        let spliced_replier = WindowAuthorFact::sealed(
            FromKind::User,
            None,
            None,
            Some(PrincipalIdentity::Human(user("reply-author-a"))),
            Some("A".to_owned()),
            Some("A".to_owned()),
            Some(MessageAuthor::deleted(
                PrincipalIdentity::Human(user("reply-author-b")),
                "B".to_owned(),
                "B".to_owned(),
            )),
        );
        assert_eq!(
            spliced_replier,
            Err(MessageWindowCorruption::AuthorProvenance)
        );
        let spliced_preview = WindowAuthorFact::sealed(
            FromKind::User,
            None,
            None,
            Some(PrincipalIdentity::Human(user("preview-author-a"))),
            Some("A".to_owned()),
            Some("A".to_owned()),
            Some(MessageAuthor::deleted(
                PrincipalIdentity::Human(user("preview-author-b")),
                "B".to_owned(),
                "B".to_owned(),
            )),
        );
        assert_eq!(
            spliced_preview,
            Err(MessageWindowCorruption::AuthorProvenance)
        );

        let count = WindowCount::new(2).unwrap();
        let recent = WindowReplier::projected(
            conversation("c"),
            thread_root("root"),
            author_fact("recent"),
            seq(8),
        )
        .unwrap();
        let older = WindowReplier::projected(
            conversation("c"),
            thread_root("root"),
            author_fact("older"),
            seq(7),
        )
        .unwrap();
        let summary = ReplyBadgeProjection::sealed(
            conversation("c"),
            thread_root("root"),
            count,
            80,
            seq(8),
            vec![recent.clone(), older],
            WindowCount::new(2).unwrap(),
        )
        .unwrap();
        assert_eq!(summary.repliers()[0].last_seq().get(), 8);
        assert_eq!(
            ReplyBadgeProjection::sealed(
                conversation("c"),
                thread_root("root"),
                count,
                80,
                seq(8),
                vec![
                    recent.clone(),
                    WindowReplier::projected(
                        conversation("c"),
                        thread_root("root"),
                        author_fact("recent"),
                        seq(7),
                    )
                    .unwrap()
                ],
                WindowCount::new(2).unwrap(),
            ),
            Err(MessageWindowCorruption::ReplyProjection)
        );
        assert_eq!(
            ReplyBadgeProjection::sealed(
                conversation("c"),
                thread_root("root"),
                count,
                80,
                seq(8),
                vec![
                    WindowReplier::projected(
                        conversation("c"),
                        thread_root("root"),
                        author_fact("older"),
                        seq(7),
                    )
                    .unwrap(),
                    recent
                ],
                WindowCount::new(2).unwrap(),
            ),
            Err(MessageWindowCorruption::ReplyProjection)
        );

        assert_eq!(
            WindowReplyPreview::sealed(
                conversation("c"),
                thread_root("root"),
                message_id("reply"),
                seq(2),
                20,
                author_fact("author"),
                "reply",
                WindowCount::new(1).unwrap(),
                None,
            ),
            Err(MessageWindowCorruption::ReplyProjection)
        );
        let content = format!("{}😀", "a".repeat(159));
        let preview = WindowReplyPreview::sealed(
            conversation("c"),
            thread_root("root"),
            message_id("reply"),
            seq(2),
            20,
            author_fact("author"),
            &content,
            WindowCount::new(0).unwrap(),
            None,
        )
        .unwrap();
        assert_eq!(preview.preview(), "a".repeat(159));
        assert!(preview.preview().encode_utf16().count() <= REPLY_PREVIEW_MAX_UTF16);

        let foreign_replier = WindowReplier::projected(
            conversation("c"),
            thread_root("other-root"),
            author_fact("recent"),
            seq(2),
        )
        .unwrap();
        assert_eq!(
            ReplyBadgeProjection::sealed(
                conversation("c"),
                thread_root("root"),
                WindowCount::new(1).unwrap(),
                20,
                seq(2),
                vec![foreign_replier],
                WindowCount::new(1).unwrap(),
            ),
            Err(MessageWindowCorruption::ReplyProjection)
        );

        let summary = ReplyBadgeProjection::sealed(
            conversation("c"),
            thread_root("root"),
            WindowCount::new(1).unwrap(),
            20,
            seq(2),
            vec![
                WindowReplier::projected(
                    conversation("c"),
                    thread_root("root"),
                    author_fact("recent"),
                    seq(2),
                )
                .unwrap(),
            ],
            WindowCount::new(1).unwrap(),
        )
        .unwrap();
        let preview = WindowReplyPreview::sealed(
            conversation("c"),
            thread_root("root"),
            message_id("reply"),
            seq(2),
            20,
            author_fact("recent"),
            "reply",
            WindowCount::new(0).unwrap(),
            None,
        )
        .unwrap();
        let roots = WindowRootReplies::sealed(
            conversation("c"),
            thread_root("root"),
            WindowCount::new(1).unwrap(),
            Some(summary),
            vec![preview],
        )
        .unwrap();
        assert!(
            ConversationMessageWindow::sealed(
                audience("viewer", "c", MessageWindowAudience::Participating),
                ConversationMessageWindowRequest::new(
                    MessageWindowStreamRequest::Mainline,
                    1,
                    MessageWindowAnchor::Latest,
                )
                .unwrap(),
                ResolvedMessageWindowStream::mainline(conversation("c")),
                head("c", None, 1),
                vec![selected("c", "root", 1, None, false)],
                vec![roots],
                boundary("c", None, WindowBoundaryDirection::Before, Some(1), false),
                boundary("c", None, WindowBoundaryDirection::After, Some(1), false),
            )
            .is_ok()
        );
    }
}
