//! Closed human-facing collaboration search contract.
//!
//! This operation is deliberately separate from internal-agent message search:
//! the human route has three result arms, a human-participation boundary and a
//! different published query algebra.
//!
//! The persisted TS reader under-specifies equal-key order for both named arms
//! and for messages tied on its `(created_at, seq)` keys. Rust deliberately adds
//! stable identity ties as a cutover refinement. Cross-language fixtures must
//! therefore sign the admitted/result set without presenting ambiguous equal-key
//! rows; the added tie order is a Rust determinism contract, not TS byte parity.

use std::{collections::HashSet, error::Error, future::Future};

use crate::{
    conversation_messaging::messaging::{
        MessageId, PrincipalIdentity,
        collaboration::{
            conversation_message_window::SelectedWindowMessage,
            conversation_ref::ConversationRosterPolicy,
        },
        conversation_title::CanonicalConversationTitle,
    },
    foundation::identity::{AgentId, ConversationId, UserId, WorkspaceId},
};

mod identities;
pub use identities::*;

const MAX_JAVASCRIPT_INTEGER: i64 = 9_007_199_254_740_991;
const I64_UPPER_EXCLUSIVE_AS_F64: f64 = 9_223_372_036_854_775_808.0;
const MAX_QUERY_UTF16: usize = 200;
const MAX_SEARCH_LIMIT: u16 = 500;
const MAX_WORKSPACE_RESULTS: usize = 20;
const MAX_CONVERSATION_RESULTS: usize = 30;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HumanSearchText(String);

impl HumanSearchText {
    pub fn parse(raw: &str) -> Result<Self, InvalidHumanSearchQuery> {
        let value = raw.trim_matches(is_ecmascript_trim_whitespace).to_owned();
        if value.encode_utf16().count() > MAX_QUERY_UTF16 {
            return Err(InvalidHumanSearchQuery::Text);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HumanSearchMillisKind {
    Persisted(i64),
    BeyondPersistedRange,
}

/// An admitted optional millisecond boundary. The representation is private so
/// callers cannot bypass finite/integer/nonnegative classification.
///
/// ```compile_fail
/// use silverhand_backend_core::conversation_messaging::messaging::collaboration::human_search::HumanSearchMillis;
///
/// let _forged = HumanSearchMillis::Persisted(-1);
/// ```
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HumanSearchMillis(HumanSearchMillisKind);

impl HumanSearchMillis {
    pub fn from_ecmascript_number(value: f64) -> Result<Self, InvalidHumanSearchQuery> {
        if !value.is_finite() || value.fract() != 0.0 || value < 0.0 {
            return Err(InvalidHumanSearchQuery::Millis);
        }
        if value >= I64_UPPER_EXCLUSIVE_AS_F64 {
            return Ok(Self(HumanSearchMillisKind::BeyondPersistedRange));
        }
        Ok(Self(HumanSearchMillisKind::Persisted(value as i64)))
    }

    pub const fn persisted_value(self) -> Option<i64> {
        match self.0 {
            HumanSearchMillisKind::Persisted(value) => Some(value),
            HumanSearchMillisKind::BeyondPersistedRange => None,
        }
    }

    pub const fn is_beyond_persisted_range(self) -> bool {
        matches!(self.0, HumanSearchMillisKind::BeyondPersistedRange)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HumanSearchOrder {
    RecencyDesc,
    SequenceAsc,
}

impl HumanSearchOrder {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RecencyDesc => "recency_desc",
            Self::SequenceAsc => "seq_asc",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HumanSearchStream {
    All,
    Mainline,
    Thread,
}

impl HumanSearchStream {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Mainline => "mainline",
            Self::Thread => "thread",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HumanSearchQuery {
    text: HumanSearchText,
    stream: HumanSearchStream,
    thread_root: Option<MessageId>,
    workspace: Option<WorkspaceId>,
    conversation: Option<ConversationId>,
    sender: Option<PrincipalIdentity>,
    from: Option<HumanSearchMillis>,
    to: Option<HumanSearchMillis>,
    order: HumanSearchOrder,
    limit: u16,
    include_identities: bool,
}

impl HumanSearchQuery {
    #[allow(clippy::too_many_arguments)]
    pub fn admitted(
        text: HumanSearchText,
        stream: HumanSearchStream,
        thread_root: Option<MessageId>,
        workspace: Option<WorkspaceId>,
        conversation: Option<ConversationId>,
        sender: Option<PrincipalIdentity>,
        from: Option<HumanSearchMillis>,
        to: Option<HumanSearchMillis>,
        order: HumanSearchOrder,
        limit: u16,
    ) -> Result<Self, InvalidHumanSearchQuery> {
        if !(1..=MAX_SEARCH_LIMIT).contains(&limit) {
            return Err(InvalidHumanSearchQuery::Limit);
        }
        if !matches!(stream, HumanSearchStream::All) && conversation.is_none() {
            return Err(InvalidHumanSearchQuery::StreamWithoutConversation);
        }
        if matches!(stream, HumanSearchStream::Thread) && thread_root.is_none() {
            return Err(InvalidHumanSearchQuery::ThreadWithoutRoot);
        }
        if order == HumanSearchOrder::SequenceAsc && conversation.is_none() {
            return Err(InvalidHumanSearchQuery::SequenceWithoutConversation);
        }
        Ok(Self {
            text,
            stream,
            thread_root,
            workspace,
            conversation,
            sender,
            from,
            to,
            order,
            limit,
            include_identities: false,
        })
    }

    pub fn with_identities(mut self, include: bool) -> Self {
        self.include_identities = include;
        self
    }

    pub fn include_identities(&self) -> bool {
        self.include_identities
    }

    pub fn text(&self) -> &HumanSearchText {
        &self.text
    }

    pub fn stream(&self) -> &HumanSearchStream {
        &self.stream
    }

    pub fn thread_root(&self) -> Option<&MessageId> {
        self.thread_root.as_ref()
    }

    pub fn workspace(&self) -> Option<&WorkspaceId> {
        self.workspace.as_ref()
    }

    pub fn conversation(&self) -> Option<&ConversationId> {
        self.conversation.as_ref()
    }

    pub fn sender(&self) -> Option<&PrincipalIdentity> {
        self.sender.as_ref()
    }

    pub const fn from(&self) -> Option<HumanSearchMillis> {
        self.from
    }

    pub const fn to(&self) -> Option<HumanSearchMillis> {
        self.to
    }

    pub const fn order(&self) -> HumanSearchOrder {
        self.order
    }

    pub const fn limit(&self) -> u16 {
        self.limit
    }

    pub fn has_search_intent(&self) -> bool {
        !self.text.is_empty()
            || self.workspace.is_some()
            || self.conversation.is_some()
            || self.sender.is_some()
            || self.from.is_some()
            || self.to.is_some()
    }

    pub fn includes_named_entities(&self) -> bool {
        !self.text.is_empty() && self.sender.is_none() && self.conversation.is_none()
    }

    fn with_thread_root(&self, root: MessageId) -> Self {
        let mut effective = self.clone();
        effective.thread_root = Some(root);
        effective
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum InvalidHumanSearchQuery {
    #[error("human search text is outside the admitted UTF-16 bound")]
    Text,
    #[error("human search millisecond filter is not a nonnegative ECMAScript integer")]
    Millis,
    #[error("human search limit must be between 1 and 500")]
    Limit,
    #[error("human search stream scope requires one conversation")]
    StreamWithoutConversation,
    #[error("human search thread scope requires one root message")]
    ThreadWithoutRoot,
    #[error("human search sequence order requires one conversation")]
    SequenceWithoutConversation,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum HumanSearchResolutionKind {
    Direct,
    ThreadUnavailable {
        viewer: UserId,
        conversation: ConversationId,
    },
    ThreadResolved {
        viewer: UserId,
        conversation: ConversationId,
    },
}

/// The actual selected anchor row and, for a reply, its actually loaded root
/// row. A missing or cross-conversation root is the product 404 arm, not an
/// adapter-selected effective query.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectedHumanSearchThreadAnchor {
    message: MessageId,
    conversation: ConversationId,
    effective_root: MessageId,
}

impl SelectedHumanSearchThreadAnchor {
    pub fn from_rows(
        message: MessageId,
        conversation: ConversationId,
        stored_thread_root: Option<crate::conversation_messaging::messaging::ThreadRootId>,
        loaded_root: Option<(
            MessageId,
            ConversationId,
            Option<crate::conversation_messaging::messaging::ThreadRootId>,
        )>,
    ) -> Result<Self, HumanSearchRefusal> {
        let effective_root = match (stored_thread_root, loaded_root) {
            (None, None) => message.clone(),
            (Some(stored), Some((root, root_conversation, None)))
                if stored.as_str() == root.as_str() && root_conversation == conversation =>
            {
                root
            }
            _ => return Err(HumanSearchRefusal::ThreadRootNotFound),
        };
        Ok(Self {
            message,
            conversation,
            effective_root,
        })
    }
}

/// Requested and published query as one fact. Thread search has a closed
/// unavailable arm and a visible arm that derives its root from an actual row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedHumanSearchQuery {
    requested: HumanSearchQuery,
    effective: HumanSearchQuery,
    kind: HumanSearchResolutionKind,
}

impl ResolvedHumanSearchQuery {
    pub fn direct(requested: HumanSearchQuery) -> Result<Self, HumanSearchCorruption> {
        if matches!(requested.stream, HumanSearchStream::Thread) {
            return Err(HumanSearchCorruption::ThreadResolution);
        }
        Ok(Self {
            effective: requested.clone(),
            requested,
            kind: HumanSearchResolutionKind::Direct,
        })
    }

    pub fn thread_unavailable(
        viewer: UserId,
        requested: HumanSearchQuery,
    ) -> Result<Self, HumanSearchCorruption> {
        let conversation = requested
            .conversation
            .clone()
            .ok_or(HumanSearchCorruption::ThreadResolution)?;
        if !matches!(requested.stream, HumanSearchStream::Thread) {
            return Err(HumanSearchCorruption::ThreadResolution);
        }
        Ok(Self {
            effective: requested.clone(),
            requested,
            kind: HumanSearchResolutionKind::ThreadUnavailable {
                viewer,
                conversation,
            },
        })
    }

    pub fn thread_resolved(
        viewer: UserId,
        requested: HumanSearchQuery,
        anchor: SelectedHumanSearchThreadAnchor,
    ) -> Result<Self, HumanSearchCorruption> {
        let requested_conversation = requested
            .conversation
            .as_ref()
            .ok_or(HumanSearchCorruption::ThreadResolution)?;
        if !matches!(requested.stream, HumanSearchStream::Thread) {
            return Err(HumanSearchCorruption::ThreadResolution);
        }
        let requested_message = requested
            .thread_root
            .as_ref()
            .ok_or(HumanSearchCorruption::ThreadResolution)?;
        if requested_message != &anchor.message || requested_conversation != &anchor.conversation {
            return Err(HumanSearchCorruption::ThreadResolution);
        }
        Ok(Self {
            effective: requested.with_thread_root(anchor.effective_root),
            requested,
            kind: HumanSearchResolutionKind::ThreadResolved {
                viewer,
                conversation: anchor.conversation,
            },
        })
    }

    pub fn requested(&self) -> &HumanSearchQuery {
        &self.requested
    }

    pub fn effective(&self) -> &HumanSearchQuery {
        &self.effective
    }

    fn is_unavailable(&self) -> bool {
        matches!(
            self.kind,
            HumanSearchResolutionKind::ThreadUnavailable { .. }
        )
    }

    fn source(&self) -> Option<(&UserId, &ConversationId)> {
        match &self.kind {
            HumanSearchResolutionKind::Direct => None,
            HumanSearchResolutionKind::ThreadUnavailable {
                viewer,
                conversation,
            }
            | HumanSearchResolutionKind::ThreadResolved {
                viewer,
                conversation,
            } => Some((viewer, conversation)),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HumanSearchWorkspaceRef {
    id: WorkspaceId,
    name: String,
    owner_agent: Option<AgentId>,
}

impl HumanSearchWorkspaceRef {
    pub fn projected(
        id: WorkspaceId,
        name: String,
        owner_agent: Option<AgentId>,
    ) -> Result<Self, HumanSearchCorruption> {
        if name.is_empty() {
            return Err(HumanSearchCorruption::Workspace);
        }
        Ok(Self {
            id,
            name,
            owner_agent,
        })
    }

    pub fn id(&self) -> &WorkspaceId {
        &self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn owner_agent(&self) -> Option<&AgentId> {
        self.owner_agent.as_ref()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectedHumanSearchWorkspace {
    source_viewer: UserId,
    workspace: HumanSearchWorkspaceRef,
    description: Option<String>,
    archived_at: Option<i64>,
    created_at: i64,
    updated_at: i64,
}

impl SelectedHumanSearchWorkspace {
    pub fn projected(
        source_viewer: UserId,
        workspace: HumanSearchWorkspaceRef,
        description: Option<String>,
        archived_at: Option<i64>,
        created_at: i64,
        updated_at: i64,
    ) -> Result<Self, HumanSearchCorruption> {
        exact_number(created_at)?;
        exact_number(updated_at)?;
        if let Some(value) = archived_at {
            exact_number(value)?;
        }
        Ok(Self {
            source_viewer,
            workspace,
            description,
            archived_at,
            created_at,
            updated_at,
        })
    }

    pub fn workspace(&self) -> &HumanSearchWorkspaceRef {
        &self.workspace
    }

    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    pub const fn archived_at(&self) -> Option<i64> {
        self.archived_at
    }

    pub const fn created_at(&self) -> i64 {
        self.created_at
    }

    pub const fn updated_at(&self) -> i64 {
        self.updated_at
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HumanSearchConversationPlacement {
    Workspace {
        title: CanonicalConversationTitle,
        workspace: HumanSearchWorkspaceRef,
    },
    DirectPair,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HumanSearchConversationPlacementKind {
    Workspace,
    DirectPair,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HumanSearchConversationRef {
    id: ConversationId,
    roster: ConversationRosterPolicy,
    created_at: i64,
    placement: HumanSearchConversationPlacement,
}

impl HumanSearchConversationRef {
    #[allow(clippy::too_many_arguments)]
    pub fn from_selected_row(
        id: ConversationId,
        placement_kind: HumanSearchConversationPlacementKind,
        roster: ConversationRosterPolicy,
        stored_title: String,
        stored_workspace: Option<WorkspaceId>,
        joined_workspace: Option<HumanSearchWorkspaceRef>,
        created_at: i64,
    ) -> Result<Self, HumanSearchCorruption> {
        exact_number(created_at)?;
        let placement = match placement_kind {
            HumanSearchConversationPlacementKind::Workspace => {
                let workspace = joined_workspace.ok_or(HumanSearchCorruption::Conversation)?;
                if stored_workspace.as_ref() != Some(workspace.id()) {
                    return Err(HumanSearchCorruption::Conversation);
                }
                let title = CanonicalConversationTitle::parse(&stored_title)
                    .map_err(|_| HumanSearchCorruption::Conversation)?;
                HumanSearchConversationPlacement::Workspace { title, workspace }
            }
            HumanSearchConversationPlacementKind::DirectPair => {
                if roster != ConversationRosterPolicy::Fixed
                    || !stored_title.is_empty()
                    || stored_workspace.is_some()
                    || joined_workspace.is_some()
                {
                    return Err(HumanSearchCorruption::Conversation);
                }
                HumanSearchConversationPlacement::DirectPair
            }
        };
        Ok(Self {
            id,
            roster,
            created_at,
            placement,
        })
    }

    pub fn id(&self) -> &ConversationId {
        &self.id
    }

    pub fn channel_id(&self) -> &ConversationId {
        &self.id
    }

    pub fn title(&self) -> &str {
        match &self.placement {
            HumanSearchConversationPlacement::Workspace { title, .. } => title.title(),
            HumanSearchConversationPlacement::DirectPair => "",
        }
    }

    pub const fn roster(&self) -> ConversationRosterPolicy {
        self.roster
    }

    pub const fn created_at(&self) -> i64 {
        self.created_at
    }

    pub const fn placement(&self) -> &HumanSearchConversationPlacement {
        &self.placement
    }

    fn workspace_id(&self) -> Option<&WorkspaceId> {
        match &self.placement {
            HumanSearchConversationPlacement::Workspace { workspace, .. } => Some(workspace.id()),
            HumanSearchConversationPlacement::DirectPair => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectedHumanSearchConversation {
    source_viewer: UserId,
    conversation: HumanSearchConversationRef,
    updated_at: i64,
    archived_at: Option<i64>,
}

impl SelectedHumanSearchConversation {
    pub fn projected(
        source_viewer: UserId,
        conversation: HumanSearchConversationRef,
        updated_at: i64,
        archived_at: Option<i64>,
    ) -> Result<Self, HumanSearchCorruption> {
        exact_number(updated_at)?;
        if let Some(value) = archived_at {
            exact_number(value)?;
        }
        Ok(Self {
            source_viewer,
            conversation,
            updated_at,
            archived_at,
        })
    }

    pub fn archived_at(&self) -> Option<i64> {
        self.archived_at
    }

    pub fn conversation(&self) -> &HumanSearchConversationRef {
        &self.conversation
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectedHumanSearchMessage {
    source_viewer: UserId,
    conversation: HumanSearchConversationRef,
    message: SelectedWindowMessage,
}

impl SelectedHumanSearchMessage {
    pub fn projected(
        source_viewer: UserId,
        conversation: HumanSearchConversationRef,
        message: SelectedWindowMessage,
    ) -> Result<Self, HumanSearchCorruption> {
        let projection = message.projection();
        if projection.conversation != *conversation.id()
            || !projection.reactions.is_empty()
            || projection.linked_task.is_some()
        {
            return Err(HumanSearchCorruption::Message);
        }
        Ok(Self {
            source_viewer,
            conversation,
            message,
        })
    }

    pub fn conversation(&self) -> &HumanSearchConversationRef {
        &self.conversation
    }

    pub fn message(&self) -> &SelectedWindowMessage {
        &self.message
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HumanSearchResult {
    Workspace(SelectedHumanSearchWorkspace),
    Conversation(SelectedHumanSearchConversation),
    Message(Box<SelectedHumanSearchMessage>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HumanSearchMessageTotal(i64);

impl HumanSearchMessageTotal {
    fn new(value: i64) -> Result<Self, HumanSearchCorruption> {
        exact_number(value)?;
        Ok(Self(value))
    }

    pub const fn get(self) -> i64 {
        self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum HumanSearchMessageCountKind {
    Skipped,
    Counted {
        source_viewer: UserId,
        query: HumanSearchQuery,
        total: HumanSearchMessageTotal,
    },
}

/// Whether the message COUNT was deliberately skipped by an early-empty arm or
/// evaluated against one exact viewer/query predicate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HumanSearchMessageCountFact(HumanSearchMessageCountKind);

impl HumanSearchMessageCountFact {
    pub fn skipped() -> Self {
        Self(HumanSearchMessageCountKind::Skipped)
    }

    pub fn counted(
        source_viewer: UserId,
        query: HumanSearchQuery,
        total: i64,
    ) -> Result<Self, HumanSearchCorruption> {
        Ok(Self(HumanSearchMessageCountKind::Counted {
            source_viewer,
            query,
            total: HumanSearchMessageTotal::new(total)?,
        }))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HumanSearchPage {
    source_viewer: UserId,
    query: ResolvedHumanSearchQuery,
    results: Vec<HumanSearchResult>,
    total: HumanSearchMessageTotal,
    truncated: bool,
    identities: Option<IdentitySearchGroups>,
}

impl HumanSearchPage {
    pub fn sealed(
        source_viewer: UserId,
        query: ResolvedHumanSearchQuery,
        workspaces: Vec<SelectedHumanSearchWorkspace>,
        conversations: Vec<SelectedHumanSearchConversation>,
        messages: Vec<SelectedHumanSearchMessage>,
        count: HumanSearchMessageCountFact,
    ) -> Result<Self, HumanSearchCorruption> {
        if query.source().is_some_and(|(viewer, conversation)| {
            viewer != &source_viewer || Some(conversation) != query.requested().conversation()
        }) {
            return Err(HumanSearchCorruption::Identity);
        }
        let effective = query.effective();
        let skipped = query.is_unavailable() || !effective.has_search_intent();
        let total = match (skipped, count.0) {
            (true, HumanSearchMessageCountKind::Skipped) => HumanSearchMessageTotal(0),
            (
                false,
                HumanSearchMessageCountKind::Counted {
                    source_viewer: count_viewer,
                    query: count_query,
                    total,
                },
            ) if count_viewer == source_viewer && count_query == *effective => total,
            _ => return Err(HumanSearchCorruption::Identity),
        };
        if skipped && (!workspaces.is_empty() || !conversations.is_empty() || !messages.is_empty())
        {
            return Err(if query.is_unavailable() {
                HumanSearchCorruption::ThreadResolution
            } else {
                HumanSearchCorruption::ResultSet
            });
        }
        if !effective.includes_named_entities()
            && (!workspaces.is_empty() || !conversations.is_empty())
        {
            return Err(HumanSearchCorruption::ResultSet);
        }
        if workspaces.len() > MAX_WORKSPACE_RESULTS.min(usize::from(effective.limit()))
            || conversations.len() > MAX_CONVERSATION_RESULTS.min(usize::from(effective.limit()))
            || messages.len()
                != usize::try_from(total.get().min(i64::from(effective.limit())))
                    .map_err(|_| HumanSearchCorruption::ResultSet)?
        {
            return Err(HumanSearchCorruption::ResultSet);
        }
        validate_workspaces(&source_viewer, effective, &workspaces)?;
        validate_conversations(&source_viewer, effective, &conversations)?;
        validate_messages(&source_viewer, effective, &messages)?;

        let mut results =
            Vec::with_capacity(workspaces.len() + conversations.len() + messages.len());
        results.extend(workspaces.into_iter().map(HumanSearchResult::Workspace));
        results.extend(
            conversations
                .into_iter()
                .map(HumanSearchResult::Conversation),
        );
        results.extend(
            messages
                .into_iter()
                .map(|message| HumanSearchResult::Message(Box::new(message))),
        );
        results.truncate(usize::from(effective.limit()));
        let truncated = total.get() > i64::from(effective.limit());
        Ok(Self {
            source_viewer,
            query,
            results,
            total,
            truncated,
            identities: None,
        })
    }

    pub fn with_identities(
        mut self,
        identities: IdentitySearchGroups,
    ) -> Result<Self, HumanSearchCorruption> {
        if !identities.matches(&self.source_viewer, self.query.effective()) {
            return Err(HumanSearchCorruption::Identity);
        }
        self.identities = Some(identities);
        Ok(self)
    }

    pub fn bind(
        self,
        requested_viewer: &UserId,
        requested_query: &HumanSearchQuery,
    ) -> Result<BoundHumanSearchPage, HumanSearchIdentityMismatch> {
        if &self.source_viewer != requested_viewer
            || self.query.requested() != requested_query
            || requested_query.include_identities() != self.identities.is_some()
        {
            return Err(HumanSearchIdentityMismatch);
        }
        Ok(BoundHumanSearchPage(self))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundHumanSearchPage(HumanSearchPage);

impl BoundHumanSearchPage {
    pub fn identities(&self) -> Option<&IdentitySearchGroups> {
        self.0.identities.as_ref()
    }

    pub fn query(&self) -> &HumanSearchQuery {
        self.0.query.effective()
    }

    pub fn results(&self) -> &[HumanSearchResult] {
        &self.0.results
    }

    pub const fn total(&self) -> HumanSearchMessageTotal {
        self.0.total
    }

    pub const fn truncated(&self) -> bool {
        self.0.truncated
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum HumanSearchCorruption {
    #[error("human-search thread resolution is not source-bound")]
    ThreadResolution,
    #[error("human-search workspace projection is malformed")]
    Workspace,
    #[error("human-search conversation projection is malformed")]
    Conversation,
    #[error("human-search message projection is malformed")]
    Message,
    #[error("human-search selected timestamp/count is not an exact nonnegative JS integer")]
    Number,
    #[error("human-search result identity differs from its authority source")]
    Identity,
    #[error("human-search selected rows do not form the requested result set")]
    ResultSet,
    #[error("human-search selected rows are not in the canonical total order")]
    Order,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("human-search response differs from the requested viewer/query")]
pub struct HumanSearchIdentityMismatch;

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum HumanSearchRefusal {
    #[error("thread root message not found in this conversation")]
    ThreadRootNotFound,
}

pub trait HumanSearchReadPort {
    type Error: Error + Send + Sync + 'static;

    fn search(
        &self,
        viewer: &UserId,
        query: HumanSearchQuery,
    ) -> impl Future<Output = Result<Result<HumanSearchPage, HumanSearchRefusal>, Self::Error>> + Send;
}

fn validate_workspaces(
    viewer: &UserId,
    query: &HumanSearchQuery,
    rows: &[SelectedHumanSearchWorkspace],
) -> Result<(), HumanSearchCorruption> {
    let mut ids = HashSet::with_capacity(rows.len());
    for row in rows {
        if &row.source_viewer != viewer
            || !ids.insert(row.workspace.id().as_str())
            || query
                .workspace()
                .is_some_and(|workspace| workspace != row.workspace.id())
            || !timestamp_in_query(query, row.created_at)
        {
            return Err(HumanSearchCorruption::ResultSet);
        }
    }
    if rows.windows(2).any(|pair| {
        pair[0].updated_at < pair[1].updated_at
            || (pair[0].updated_at == pair[1].updated_at
                && pair[0].workspace.id().as_str() >= pair[1].workspace.id().as_str())
    }) {
        return Err(HumanSearchCorruption::Order);
    }
    Ok(())
}

fn validate_conversations(
    viewer: &UserId,
    query: &HumanSearchQuery,
    rows: &[SelectedHumanSearchConversation],
) -> Result<(), HumanSearchCorruption> {
    let mut ids = HashSet::with_capacity(rows.len());
    for row in rows {
        if &row.source_viewer != viewer
            || !ids.insert(row.conversation.id().as_str())
            || !conversation_in_query(query, &row.conversation)
        {
            return Err(HumanSearchCorruption::ResultSet);
        }
    }
    if rows.windows(2).any(|pair| {
        pair[0].updated_at < pair[1].updated_at
            || (pair[0].updated_at == pair[1].updated_at
                && pair[0].conversation.id().as_str() >= pair[1].conversation.id().as_str())
    }) {
        return Err(HumanSearchCorruption::Order);
    }
    Ok(())
}

fn validate_messages(
    viewer: &UserId,
    query: &HumanSearchQuery,
    rows: &[SelectedHumanSearchMessage],
) -> Result<(), HumanSearchCorruption> {
    let mut ids = HashSet::with_capacity(rows.len());
    for row in rows {
        let message = row.message.projection();
        if &row.source_viewer != viewer
            || !ids.insert(message.id.as_str())
            || !conversation_in_query(query, &row.conversation)
            || !timestamp_in_query(query, message.created_at_ms)
            || !stream_contains(query, message)
            || query.sender().is_some_and(|sender| {
                message
                    .author
                    .author()
                    .and_then(|author| author.stable_identity())
                    != Some(sender.clone())
            })
        {
            return Err(HumanSearchCorruption::ResultSet);
        }
    }
    if rows
        .windows(2)
        .any(|pair| !message_precedes(query.order(), &pair[0], &pair[1]))
    {
        return Err(HumanSearchCorruption::Order);
    }
    Ok(())
}

fn conversation_in_query(
    query: &HumanSearchQuery,
    conversation: &HumanSearchConversationRef,
) -> bool {
    query
        .conversation()
        .is_none_or(|selected| selected == conversation.id())
        && query
            .workspace()
            .is_none_or(|selected| Some(selected) == conversation.workspace_id())
        && timestamp_in_query(query, conversation.created_at())
}

fn timestamp_in_query(query: &HumanSearchQuery, value: i64) -> bool {
    let after_from = match query.from() {
        None => true,
        Some(HumanSearchMillis(HumanSearchMillisKind::Persisted(from))) => value >= from,
        Some(HumanSearchMillis(HumanSearchMillisKind::BeyondPersistedRange)) => false,
    };
    let before_to = match query.to() {
        None | Some(HumanSearchMillis(HumanSearchMillisKind::BeyondPersistedRange)) => true,
        Some(HumanSearchMillis(HumanSearchMillisKind::Persisted(to))) => value <= to,
    };
    after_from && before_to
}

fn stream_contains(
    query: &HumanSearchQuery,
    message: &crate::conversation_messaging::messaging::collaboration::conversation_message_window::WindowMessageProjection,
) -> bool {
    match query.stream() {
        HumanSearchStream::All => true,
        HumanSearchStream::Mainline => message.thread_root.is_none(),
        HumanSearchStream::Thread => message
            .thread_root
            .as_ref()
            .zip(query.thread_root())
            .is_some_and(|(actual, requested)| actual.as_str() == requested.as_str()),
    }
}

fn message_precedes(
    order: HumanSearchOrder,
    left: &SelectedHumanSearchMessage,
    right: &SelectedHumanSearchMessage,
) -> bool {
    let left_message = left.message.projection();
    let right_message = right.message.projection();
    match order {
        HumanSearchOrder::SequenceAsc => left_message.seq < right_message.seq,
        HumanSearchOrder::RecencyDesc => {
            left_message.created_at_ms > right_message.created_at_ms
                || (left_message.created_at_ms == right_message.created_at_ms
                    && (left_message.seq > right_message.seq
                        || (left_message.seq == right_message.seq
                            && (left_message.conversation.as_str(), left_message.id.as_str())
                                < (
                                    right_message.conversation.as_str(),
                                    right_message.id.as_str(),
                                ))))
        }
    }
}

fn exact_number(value: i64) -> Result<(), HumanSearchCorruption> {
    (0..=MAX_JAVASCRIPT_INTEGER)
        .contains(&value)
        .then_some(())
        .ok_or(HumanSearchCorruption::Number)
}

// ECMA-262 WhiteSpace + LineTerminator. SearchQueryInputSchema calls
// `String.prototype.trim`, so the core value closes over that exact set.
fn is_ecmascript_trim_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{0009}' | '\u{000B}' | '\u{000C}' | '\u{0020}' | '\u{00A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
                | '\u{000A}'
                | '\u{000D}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation_messaging::messaging::{
        FromKind, MessageAuthor, MessageSeq, ThreadRootId,
        collaboration::conversation_message_window::{
            WindowAuthorFact, WindowMessageMetadata, WindowMessageProjection,
        },
    };

    fn user(value: &str) -> UserId {
        UserId::parse(value).unwrap()
    }

    fn conversation(value: &str) -> ConversationId {
        ConversationId::parse(value).unwrap()
    }

    fn workspace(value: &str) -> WorkspaceId {
        WorkspaceId::parse(value).unwrap()
    }

    fn message(value: &str) -> MessageId {
        MessageId::parse(value).unwrap()
    }

    fn text(value: &str) -> HumanSearchText {
        HumanSearchText::parse(value).unwrap()
    }

    fn query(
        stream: HumanSearchStream,
        conversation: Option<ConversationId>,
        sender: Option<PrincipalIdentity>,
        limit: u16,
    ) -> HumanSearchQuery {
        let thread_root = matches!(stream, HumanSearchStream::Thread).then(|| message("reply"));
        HumanSearchQuery::admitted(
            text("  needle  "),
            stream,
            thread_root,
            None,
            conversation,
            sender,
            None,
            None,
            HumanSearchOrder::RecencyDesc,
            limit,
        )
        .unwrap()
    }

    fn workspace_ref(value: &str) -> HumanSearchWorkspaceRef {
        HumanSearchWorkspaceRef::projected(workspace(value), "Workspace".to_owned(), None).unwrap()
    }

    fn conversation_ref(value: &str) -> HumanSearchConversationRef {
        HumanSearchConversationRef::from_selected_row(
            conversation(value),
            HumanSearchConversationPlacementKind::Workspace,
            ConversationRosterPolicy::Open,
            "General".to_owned(),
            Some(workspace("w")),
            Some(workspace_ref("w")),
            1,
        )
        .unwrap()
    }

    fn system_message(
        conversation_id: &str,
        id: &str,
        seq: i64,
        created: i64,
    ) -> SelectedWindowMessage {
        SelectedWindowMessage::seal(WindowMessageProjection {
            client_message_id: None,
            id: message(id),
            conversation: conversation(conversation_id),
            author: WindowAuthorFact::sealed(FromKind::System, None, None, None, None, None, None)
                .unwrap(),
            content: "needle".to_owned(),
            metadata: WindowMessageMetadata::decoded(None, Vec::new()),
            seq: MessageSeq::try_from(seq).unwrap(),
            created_at_ms: created,
            thread_root: None,
            linked_task: None,
            reactions: Vec::new(),
        })
        .unwrap()
    }

    fn selected_message(
        viewer: &str,
        conversation_id: &str,
        id: &str,
        seq: i64,
        created: i64,
    ) -> SelectedHumanSearchMessage {
        SelectedHumanSearchMessage::projected(
            user(viewer),
            conversation_ref(conversation_id),
            system_message(conversation_id, id, seq, created),
        )
        .unwrap()
    }

    #[test]
    fn admitted_query_is_one_closed_filter_algebra() {
        assert_eq!(text("\u{FEFF} hello \u{3000}").as_str(), "hello");
        assert!(HumanSearchText::parse(&"🚀".repeat(101)).is_err());
        assert_eq!(
            HumanSearchMillis::from_ecmascript_number(1e20).unwrap(),
            HumanSearchMillis(HumanSearchMillisKind::BeyondPersistedRange)
        );
        assert!(HumanSearchMillis::from_ecmascript_number(-1.0).is_err());

        let missing_conversation = HumanSearchQuery::admitted(
            text("x"),
            HumanSearchStream::Mainline,
            None,
            None,
            None,
            None,
            None,
            None,
            HumanSearchOrder::RecencyDesc,
            80,
        );
        assert_eq!(
            missing_conversation,
            Err(InvalidHumanSearchQuery::StreamWithoutConversation)
        );
        let invalid_order = HumanSearchQuery::admitted(
            text("x"),
            HumanSearchStream::All,
            None,
            None,
            None,
            None,
            None,
            None,
            HumanSearchOrder::SequenceAsc,
            80,
        );
        assert_eq!(
            invalid_order,
            Err(InvalidHumanSearchQuery::SequenceWithoutConversation)
        );

        let ignored_but_published_root = HumanSearchQuery::admitted(
            text("x"),
            HumanSearchStream::All,
            Some(message("still-published")),
            None,
            None,
            None,
            None,
            None,
            HumanSearchOrder::RecencyDesc,
            80,
        )
        .unwrap();
        assert_eq!(
            ignored_but_published_root.thread_root().unwrap().as_str(),
            "still-published"
        );
    }

    #[test]
    fn selected_conversation_consumes_the_actual_placement_row() {
        assert_eq!(
            HumanSearchConversationRef::from_selected_row(
                conversation("c"),
                HumanSearchConversationPlacementKind::Workspace,
                ConversationRosterPolicy::Open,
                "General".to_owned(),
                Some(workspace("a")),
                Some(workspace_ref("b")),
                1,
            ),
            Err(HumanSearchCorruption::Conversation)
        );
        assert!(
            HumanSearchConversationRef::from_selected_row(
                conversation("c"),
                HumanSearchConversationPlacementKind::Workspace,
                ConversationRosterPolicy::Open,
                "General".to_owned(),
                Some(workspace("a")),
                Some(workspace_ref("a")),
                1,
            )
            .is_ok()
        );

        for (roster, title, stored_workspace, joined_workspace) in [
            (ConversationRosterPolicy::Open, "", None, None),
            (ConversationRosterPolicy::Fixed, "not-empty", None, None),
            (
                ConversationRosterPolicy::Fixed,
                "",
                Some(workspace("w")),
                Some(workspace_ref("w")),
            ),
        ] {
            assert_eq!(
                HumanSearchConversationRef::from_selected_row(
                    conversation("pair"),
                    HumanSearchConversationPlacementKind::DirectPair,
                    roster,
                    title.to_owned(),
                    stored_workspace,
                    joined_workspace,
                    1,
                ),
                Err(HumanSearchCorruption::Conversation)
            );
        }
        assert!(
            HumanSearchConversationRef::from_selected_row(
                conversation("pair"),
                HumanSearchConversationPlacementKind::DirectPair,
                ConversationRosterPolicy::Fixed,
                String::new(),
                None,
                None,
                1,
            )
            .is_ok()
        );
    }

    #[test]
    fn thread_outcome_keeps_requested_query_and_only_actual_rows_normalize_the_root() {
        let requested = query(HumanSearchStream::Thread, Some(conversation("c")), None, 10);
        let unavailable =
            ResolvedHumanSearchQuery::thread_unavailable(user("viewer"), requested.clone())
                .unwrap();
        assert_eq!(unavailable.requested(), unavailable.effective());
        let empty = HumanSearchPage::sealed(
            user("viewer"),
            unavailable,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            HumanSearchMessageCountFact::skipped(),
        )
        .unwrap()
        .bind(&user("viewer"), &requested)
        .unwrap();
        assert_eq!(empty.query().thread_root().unwrap().as_str(), "reply");

        let resolved = ResolvedHumanSearchQuery::thread_resolved(
            user("viewer"),
            requested.clone(),
            SelectedHumanSearchThreadAnchor::from_rows(
                message("reply"),
                conversation("c"),
                Some(ThreadRootId::parse("root").unwrap()),
                Some((message("root"), conversation("c"), None)),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(resolved.effective().thread_root().unwrap().as_str(), "root");
        assert!(
            ResolvedHumanSearchQuery::thread_resolved(
                user("viewer"),
                requested,
                SelectedHumanSearchThreadAnchor::from_rows(
                    message("foreign"),
                    conversation("c"),
                    None,
                    None,
                )
                .unwrap(),
            )
            .is_err()
        );

        assert_eq!(
            SelectedHumanSearchThreadAnchor::from_rows(
                message("reply"),
                conversation("c"),
                Some(ThreadRootId::parse("root").unwrap()),
                Some((message("root"), conversation("foreign"), None)),
            ),
            Err(HumanSearchRefusal::ThreadRootNotFound)
        );
        assert_eq!(
            SelectedHumanSearchThreadAnchor::from_rows(
                message("reply"),
                conversation("c"),
                Some(ThreadRootId::parse("root").unwrap()),
                Some((
                    message("root"),
                    conversation("c"),
                    Some(ThreadRootId::parse("older-root").unwrap()),
                )),
            ),
            Err(HumanSearchRefusal::ThreadRootNotFound)
        );
    }

    #[test]
    fn three_arms_are_ordered_then_capped_while_total_remains_message_only() {
        let requested = query(HumanSearchStream::All, None, None, 2);
        let resolution = ResolvedHumanSearchQuery::direct(requested.clone()).unwrap();
        let workspace_row = SelectedHumanSearchWorkspace::projected(
            user("viewer"),
            workspace_ref("w"),
            Some("needle".to_owned()),
            None,
            1,
            9,
        )
        .unwrap();
        let conversation_row = SelectedHumanSearchConversation::projected(
            user("viewer"),
            conversation_ref("c"),
            8,
            None,
        )
        .unwrap();
        let messages = vec![
            selected_message("viewer", "c", "m2", 2, 7),
            selected_message("viewer", "c", "m1", 1, 6),
        ];
        let page = HumanSearchPage::sealed(
            user("viewer"),
            resolution,
            vec![workspace_row],
            vec![conversation_row],
            messages,
            HumanSearchMessageCountFact::counted(user("viewer"), requested.clone(), 5).unwrap(),
        )
        .unwrap()
        .bind(&user("viewer"), &requested)
        .unwrap();
        assert!(matches!(page.results()[0], HumanSearchResult::Workspace(_)));
        assert!(matches!(
            page.results()[1],
            HumanSearchResult::Conversation(_)
        ));
        assert_eq!(page.total().get(), 5);
        assert!(page.truncated());
    }

    #[test]
    fn viewer_query_deleted_sender_and_result_order_are_all_sealed() {
        let sender = PrincipalIdentity::Human(user("author"));
        let requested = query(HumanSearchStream::All, None, Some(sender.clone()), 2);
        let resolution = ResolvedHumanSearchQuery::direct(requested.clone()).unwrap();
        let deleted =
            MessageAuthor::deleted(sender.clone(), "author".to_owned(), "Author".to_owned());
        let author = WindowAuthorFact::sealed(
            FromKind::User,
            None,
            None,
            Some(sender),
            Some("author".to_owned()),
            Some("Author".to_owned()),
            Some(deleted),
        )
        .unwrap();
        let make = |viewer: &str, id: &str, seq: i64, created: i64| {
            let message = SelectedWindowMessage::seal(WindowMessageProjection {
                client_message_id: None,
                id: message(id),
                conversation: conversation("c"),
                author: author.clone(),
                content: "needle".to_owned(),
                metadata: WindowMessageMetadata::decoded(None, Vec::new()),
                seq: MessageSeq::try_from(seq).unwrap(),
                created_at_ms: created,
                thread_root: None,
                linked_task: None,
                reactions: Vec::new(),
            })
            .unwrap();
            SelectedHumanSearchMessage::projected(user(viewer), conversation_ref("c"), message)
                .unwrap()
        };

        assert_eq!(
            HumanSearchPage::sealed(
                user("viewer"),
                resolution.clone(),
                Vec::new(),
                Vec::new(),
                vec![make("other", "m2", 2, 2), make("other", "m1", 1, 1)],
                HumanSearchMessageCountFact::counted(user("viewer"), requested.clone(), 2,)
                    .unwrap(),
            ),
            Err(HumanSearchCorruption::ResultSet)
        );
        assert_eq!(
            HumanSearchPage::sealed(
                user("viewer"),
                resolution.clone(),
                Vec::new(),
                Vec::new(),
                vec![make("viewer", "m1", 1, 1), make("viewer", "m2", 2, 2)],
                HumanSearchMessageCountFact::counted(user("viewer"), requested.clone(), 2,)
                    .unwrap(),
            ),
            Err(HumanSearchCorruption::Order)
        );
        assert_eq!(
            HumanSearchPage::sealed(
                user("viewer"),
                resolution.clone(),
                Vec::new(),
                Vec::new(),
                vec![make("viewer", "m2", 2, 2), make("viewer", "m1", 1, 1)],
                HumanSearchMessageCountFact::counted(user("other"), requested.clone(), 2,).unwrap(),
            ),
            Err(HumanSearchCorruption::Identity)
        );
        let foreign_count_query = query(
            HumanSearchStream::All,
            None,
            Some(PrincipalIdentity::Human(user("someone-else"))),
            2,
        );
        assert_eq!(
            HumanSearchPage::sealed(
                user("viewer"),
                resolution.clone(),
                Vec::new(),
                Vec::new(),
                vec![make("viewer", "m2", 2, 2), make("viewer", "m1", 1, 1)],
                HumanSearchMessageCountFact::counted(user("viewer"), foreign_count_query, 2,)
                    .unwrap(),
            ),
            Err(HumanSearchCorruption::Identity)
        );
        let page = HumanSearchPage::sealed(
            user("viewer"),
            resolution,
            Vec::new(),
            Vec::new(),
            vec![make("viewer", "m2", 2, 2), make("viewer", "m1", 1, 1)],
            HumanSearchMessageCountFact::counted(user("viewer"), requested.clone(), 2).unwrap(),
        )
        .unwrap();
        assert!(page.clone().bind(&user("other"), &requested).is_err());
        let different = query(
            HumanSearchStream::All,
            None,
            Some(PrincipalIdentity::Human(user("author"))),
            1,
        );
        assert!(page.bind(&user("viewer"), &different).is_err());
    }
}
