use std::{cmp::Ordering, collections::HashSet, error::Error, future::Future};

use crate::foundation::identity::{AgentId, ConversationId, UserId, WorkspaceId};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChannelVisibility {
    Private,
    Public,
}
impl ChannelVisibility {
    pub fn parse(raw: &str) -> Result<Self, UnknownChannelVisibility> {
        match raw {
            "private" => Ok(Self::Private),
            "public" => Ok(Self::Public),
            _ => Err(UnknownChannelVisibility),
        }
    }
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Public => "public",
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("stored channel visibility is outside the published closed set")]
pub struct UnknownChannelVisibility;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DirectoryRoster {
    Open,
    Fixed,
}
impl DirectoryRoster {
    pub fn parse(raw: &str) -> Result<Self, UnknownDirectoryRoster> {
        match raw {
            "open" => Ok(Self::Open),
            "fixed" => Ok(Self::Fixed),
            _ => Err(UnknownDirectoryRoster),
        }
    }
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Fixed => "fixed",
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("stored channel roster is outside the published closed set")]
pub struct UnknownDirectoryRoster;

/// Agent-home is an ownership relation. The owner name is retained only so the
/// projector can reproduce the published Onboarding/owner-DM/other order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DirectoryWorkspaceKind {
    Regular,
    AgentHome(AgentHomeWorkspace),
}
impl DirectoryWorkspaceKind {
    pub fn agent_home(
        owner: AgentId,
        owner_name: String,
    ) -> Result<Self, ChannelDirectoryProjectionError> {
        if owner_name.is_empty() {
            return Err(ChannelDirectoryProjectionError::EmptyOwnerName);
        }
        Ok(Self::AgentHome(AgentHomeWorkspace { owner, owner_name }))
    }

    pub fn agent_home_fact(&self) -> Option<&AgentHomeWorkspace> {
        match self {
            Self::Regular => None,
            Self::AgentHome(value) => Some(value),
        }
    }
}

/// The payload is intentionally opaque: an empty owner name cannot bypass
/// `DirectoryWorkspaceKind::agent_home` through the public enum arm.
///
/// ```compile_fail
/// use silverhand_backend_core::{foundation::identity::AgentId, conversation_messaging::messaging::collaboration::channel_directory::{AgentHomeWorkspace, DirectoryWorkspaceKind}};
/// let forged = DirectoryWorkspaceKind::AgentHome(AgentHomeWorkspace {
///     owner: AgentId::parse("a").unwrap(), owner_name: String::new()
/// });
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentHomeWorkspace {
    owner: AgentId,
    owner_name: String,
}

impl AgentHomeWorkspace {
    pub fn owner(&self) -> &AgentId {
        &self.owner
    }
    pub fn owner_name(&self) -> &str {
        &self.owner_name
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChannelDirectoryRow {
    source_viewer: UserId,
    workspace: WorkspaceId,
    conversation: ConversationId,
    roster: DirectoryRoster,
    title: String,
    spawned_by_agent: Option<AgentId>,
    archived_at: Option<i64>,
    latest_message_seq: i64,
    created_at: i64,
    updated_at: i64,
    visibility: ChannelVisibility,
    is_member: bool,
}
impl ChannelDirectoryRow {
    #[allow(clippy::too_many_arguments)]
    pub fn projected(
        source_viewer: UserId,
        workspace: WorkspaceId,
        conversation: ConversationId,
        roster: DirectoryRoster,
        title: String,
        spawned_by_agent: Option<AgentId>,
        archived_at: Option<i64>,
        latest_message_seq: i64,
        created_at: i64,
        updated_at: i64,
        visibility: ChannelVisibility,
        is_member: bool,
    ) -> Result<Self, ChannelDirectoryProjectionError> {
        if title.is_empty() {
            return Err(ChannelDirectoryProjectionError::EmptyTitle);
        }
        if roster == DirectoryRoster::Fixed && visibility == ChannelVisibility::Public {
            return Err(ChannelDirectoryProjectionError::PublicFixedRoster);
        }
        if latest_message_seq < 0 {
            return Err(ChannelDirectoryProjectionError::NegativeLatestMessageSeq);
        }
        Ok(Self {
            source_viewer,
            workspace,
            conversation,
            roster,
            title,
            spawned_by_agent,
            archived_at,
            latest_message_seq,
            created_at,
            updated_at,
            visibility,
            is_member,
        })
    }
    pub fn conversation(&self) -> &ConversationId {
        &self.conversation
    }
    pub(super) fn source_viewer(&self) -> &UserId {
        &self.source_viewer
    }
    pub(super) fn workspace(&self) -> &WorkspaceId {
        &self.workspace
    }
    pub const fn roster(&self) -> DirectoryRoster {
        self.roster
    }
    pub fn title(&self) -> &str {
        &self.title
    }
    pub fn spawned_by_agent(&self) -> Option<&AgentId> {
        self.spawned_by_agent.as_ref()
    }
    pub const fn archived_at(&self) -> Option<i64> {
        self.archived_at
    }
    pub const fn latest_message_seq(&self) -> i64 {
        self.latest_message_seq
    }
    pub const fn created_at(&self) -> i64 {
        self.created_at
    }
    pub const fn updated_at(&self) -> i64 {
        self.updated_at
    }
    pub const fn visibility(&self) -> ChannelVisibility {
        self.visibility
    }
    pub const fn is_member(&self) -> bool {
        self.is_member
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChannelSortEntry {
    source_viewer: UserId,
    workspace: WorkspaceId,
    conversation: ConversationId,
    rank: Option<i64>,
    last_accessed_at: Option<i64>,
    starred_at: Option<i64>,
}
impl ChannelSortEntry {
    pub fn projected(
        source_viewer: UserId,
        workspace: WorkspaceId,
        conversation: ConversationId,
        rank: Option<i64>,
        last_accessed_at: Option<i64>,
        starred_at: Option<i64>,
    ) -> Self {
        Self {
            source_viewer,
            workspace,
            conversation,
            rank,
            last_accessed_at,
            starred_at,
        }
    }
    pub fn conversation(&self) -> &ConversationId {
        &self.conversation
    }
    pub const fn rank(&self) -> Option<i64> {
        self.rank
    }
    pub const fn last_accessed_at(&self) -> Option<i64> {
        self.last_accessed_at
    }
    pub const fn starred_at(&self) -> Option<i64> {
        self.starred_at
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChannelSortMode {
    Recent,
    Manual,
}
impl ChannelSortMode {
    pub fn parse(raw: &str) -> Result<Self, UnknownChannelSortMode> {
        match raw {
            "recent" => Ok(Self::Recent),
            "manual" => Ok(Self::Manual),
            _ => Err(UnknownChannelSortMode),
        }
    }
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Recent => "recent",
            Self::Manual => "manual",
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("stored channel sort mode is outside the published closed set")]
pub struct UnknownChannelSortMode;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChannelSortState {
    source_viewer: UserId,
    workspace: WorkspaceId,
    mode: Option<ChannelSortMode>,
    entries: Vec<ChannelSortEntry>,
}
impl ChannelSortState {
    pub fn projected(
        source_viewer: UserId,
        workspace: WorkspaceId,
        mode: Option<ChannelSortMode>,
        mut entries: Vec<ChannelSortEntry>,
    ) -> Result<Self, ChannelDirectoryProjectionError> {
        let mut ids = HashSet::new();
        for entry in &entries {
            if entry.source_viewer != source_viewer || entry.workspace != workspace {
                return Err(ChannelDirectoryProjectionError::ForeignSortCoordinate);
            }
            if !ids.insert(entry.conversation.clone()) {
                return Err(ChannelDirectoryProjectionError::DuplicateSortEntry);
            }
        }
        entries.sort_by(|a, b| a.conversation.as_str().cmp(b.conversation.as_str()));
        Ok(Self {
            source_viewer,
            workspace,
            mode,
            entries,
        })
    }
    pub const fn mode(&self) -> Option<ChannelSortMode> {
        self.mode
    }
    pub fn entries(&self) -> &[ChannelSortEntry] {
        &self.entries
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActiveWorkspaceChannelDirectory {
    source_viewer: UserId,
    workspace: WorkspaceId,
    kind: DirectoryWorkspaceKind,
    rows: Vec<ChannelDirectoryRow>,
    sort: ChannelSortState,
}
impl ActiveWorkspaceChannelDirectory {
    pub fn projected(
        source_viewer: UserId,
        workspace: WorkspaceId,
        kind: DirectoryWorkspaceKind,
        mut rows: Vec<ChannelDirectoryRow>,
        sort: ChannelSortState,
    ) -> Result<Self, ChannelDirectoryProjectionError> {
        validate_rows(&source_viewer, &workspace, &kind, &rows, false)?;
        if sort.source_viewer != source_viewer || sort.workspace != workspace {
            return Err(ChannelDirectoryProjectionError::ForeignSortCoordinate);
        }
        let row_ids: HashSet<_> = rows.iter().map(|row| &row.conversation).collect();
        if sort
            .entries
            .iter()
            .any(|entry| !row_ids.contains(&entry.conversation))
        {
            return Err(ChannelDirectoryProjectionError::SortEntryOutsideDirectory);
        }
        sort_active(&kind, &mut rows);
        Ok(Self {
            source_viewer,
            workspace,
            kind,
            rows,
            sort,
        })
    }
    pub fn bind(
        self,
        viewer: &UserId,
        workspace: &WorkspaceId,
    ) -> Result<BoundActiveWorkspaceChannelDirectory, ChannelDirectoryProjectionError> {
        if &self.source_viewer != viewer || &self.workspace != workspace {
            return Err(ChannelDirectoryProjectionError::ForeignDirectoryCoordinate);
        }
        Ok(BoundActiveWorkspaceChannelDirectory(self))
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundActiveWorkspaceChannelDirectory(ActiveWorkspaceChannelDirectory);
impl BoundActiveWorkspaceChannelDirectory {
    pub fn workspace(&self) -> &WorkspaceId {
        &self.0.workspace
    }
    pub fn kind(&self) -> &DirectoryWorkspaceKind {
        &self.0.kind
    }
    pub fn rows(&self) -> &[ChannelDirectoryRow] {
        &self.0.rows
    }
    pub fn sort(&self) -> &ChannelSortState {
        &self.0.sort
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArchivedWorkspaceChannelDirectory {
    source_viewer: UserId,
    workspace: WorkspaceId,
    kind: DirectoryWorkspaceKind,
    rows: Vec<ChannelDirectoryRow>,
}
impl ArchivedWorkspaceChannelDirectory {
    pub fn projected(
        source_viewer: UserId,
        workspace: WorkspaceId,
        kind: DirectoryWorkspaceKind,
        mut rows: Vec<ChannelDirectoryRow>,
    ) -> Result<Self, ChannelDirectoryProjectionError> {
        validate_rows(&source_viewer, &workspace, &kind, &rows, true)?;
        sort_archived(&mut rows);
        Ok(Self {
            source_viewer,
            workspace,
            kind,
            rows,
        })
    }
    pub fn bind(
        self,
        viewer: &UserId,
        workspace: &WorkspaceId,
    ) -> Result<BoundArchivedWorkspaceChannelDirectory, ChannelDirectoryProjectionError> {
        if &self.source_viewer != viewer || &self.workspace != workspace {
            return Err(ChannelDirectoryProjectionError::ForeignDirectoryCoordinate);
        }
        Ok(BoundArchivedWorkspaceChannelDirectory(self))
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundArchivedWorkspaceChannelDirectory(ArchivedWorkspaceChannelDirectory);
impl BoundArchivedWorkspaceChannelDirectory {
    pub fn workspace(&self) -> &WorkspaceId {
        &self.0.workspace
    }
    pub fn kind(&self) -> &DirectoryWorkspaceKind {
        &self.0.kind
    }
    pub fn rows(&self) -> &[ChannelDirectoryRow] {
        &self.0.rows
    }
}

fn validate_rows(
    viewer: &UserId,
    workspace: &WorkspaceId,
    kind: &DirectoryWorkspaceKind,
    rows: &[ChannelDirectoryRow],
    archived: bool,
) -> Result<(), ChannelDirectoryProjectionError> {
    let mut ids = HashSet::new();
    for row in rows {
        if &row.source_viewer != viewer || &row.workspace != workspace {
            return Err(ChannelDirectoryProjectionError::ForeignDirectoryRow);
        }
        if !ids.insert(row.conversation.clone()) {
            return Err(ChannelDirectoryProjectionError::DuplicateConversation);
        }
        if archived != row.archived_at.is_some() {
            return Err(if archived {
                ChannelDirectoryProjectionError::ActiveRowInArchivedDirectory
            } else {
                ChannelDirectoryProjectionError::ArchivedRowInActiveDirectory
            });
        }
        match (kind, row.roster) {
            (DirectoryWorkspaceKind::Regular, DirectoryRoster::Open)
            | (DirectoryWorkspaceKind::AgentHome(_), DirectoryRoster::Fixed) => {}
            _ => return Err(ChannelDirectoryProjectionError::WorkspaceKindRosterMismatch),
        }
        if archived && !row.is_member {
            return Err(ChannelDirectoryProjectionError::ArchivedNonMember);
        }
        if !archived && !row.is_member && row.visibility != ChannelVisibility::Public {
            return Err(ChannelDirectoryProjectionError::PrivateNonMember);
        }
        if matches!(kind, DirectoryWorkspaceKind::AgentHome(_)) && !row.is_member {
            return Err(ChannelDirectoryProjectionError::AgentHomeNonMember);
        }
    }
    Ok(())
}

fn sort_active(kind: &DirectoryWorkspaceKind, rows: &mut [ChannelDirectoryRow]) {
    rows.sort_by(|a, b| match kind {
        DirectoryWorkspaceKind::Regular => active_tail_order(a, b),
        DirectoryWorkspaceKind::AgentHome(home) => agent_home_rank(a.title(), home.owner_name())
            .cmp(&agent_home_rank(b.title(), home.owner_name()))
            .then_with(|| active_tail_order(a, b)),
    });
}
fn active_tail_order(a: &ChannelDirectoryRow, b: &ChannelDirectoryRow) -> Ordering {
    a.created_at
        .cmp(&b.created_at)
        .then_with(|| a.conversation.as_str().cmp(b.conversation.as_str()))
}
fn agent_home_rank(title: &str, owner_name: &str) -> u8 {
    if title == "Onboarding" {
        0
    } else if title == format!("dm-{owner_name}") {
        1
    } else {
        2
    }
}
fn sort_archived(rows: &mut [ChannelDirectoryRow]) {
    rows.sort_by(|a, b| {
        b.archived_at
            .cmp(&a.archived_at)
            .then_with(|| a.created_at.cmp(&b.created_at))
            .then_with(|| a.conversation.as_str().cmp(b.conversation.as_str()))
    });
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArchivedChannelRow {
    source_viewer: UserId,
    workspace: WorkspaceId,
    workspace_kind: DirectoryWorkspaceKind,
    workspace_name: String,
    workspace_archived_at: Option<i64>,
    channel: ChannelDirectoryRow,
}
impl ArchivedChannelRow {
    pub fn projected(
        source_viewer: UserId,
        workspace: WorkspaceId,
        workspace_kind: DirectoryWorkspaceKind,
        workspace_name: String,
        workspace_archived_at: Option<i64>,
        channel: ChannelDirectoryRow,
    ) -> Result<Self, ChannelDirectoryProjectionError> {
        if workspace_name.is_empty() {
            return Err(ChannelDirectoryProjectionError::EmptyWorkspaceName);
        }
        if channel.source_viewer != source_viewer {
            return Err(ChannelDirectoryProjectionError::ForeignGlobalViewer);
        }
        if channel.workspace != workspace {
            return Err(ChannelDirectoryProjectionError::ForeignGlobalWorkspace);
        }
        if channel.archived_at.is_none() {
            return Err(ChannelDirectoryProjectionError::ActiveRowInGlobalArchive);
        }
        if !channel.is_member {
            return Err(ChannelDirectoryProjectionError::GlobalArchivedNonMember);
        }
        match (&workspace_kind, channel.roster) {
            (DirectoryWorkspaceKind::Regular, DirectoryRoster::Open)
            | (DirectoryWorkspaceKind::AgentHome(_), DirectoryRoster::Fixed) => {}
            _ => return Err(ChannelDirectoryProjectionError::GlobalWorkspaceKindRosterMismatch),
        }
        Ok(Self {
            source_viewer,
            workspace,
            workspace_kind,
            workspace_name,
            workspace_archived_at,
            channel,
        })
    }
    pub fn workspace(&self) -> &WorkspaceId {
        &self.workspace
    }
    pub fn workspace_kind(&self) -> &DirectoryWorkspaceKind {
        &self.workspace_kind
    }
    pub fn workspace_name(&self) -> &str {
        &self.workspace_name
    }
    pub const fn workspace_archived_at(&self) -> Option<i64> {
        self.workspace_archived_at
    }
    pub fn channel(&self) -> &ChannelDirectoryRow {
        &self.channel
    }
}

/// Wire classification is opaque so negative and misclassified values cannot
/// enter persistence by directly constructing an enum variant.
///
/// ```compile_fail
/// use silverhand_backend_core::conversation_messaging::messaging::collaboration::channel_directory::ArchivedSince;
/// let forged = ArchivedSince::ExactSafe(-1);
/// ```
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArchivedSince(ArchivedSinceValue);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ArchivedSinceValue {
    ExactSafe(i64),
    RoundedPersisted(i64),
    BeyondPersistedRange,
}
impl ArchivedSince {
    pub const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

    pub fn from_ecmascript_number(value: f64) -> Result<Self, InvalidArchivedSince> {
        if !value.is_finite() || value < 0.0 || value.fract() != 0.0 {
            return Err(InvalidArchivedSince);
        }
        if value >= 9_223_372_036_854_775_808.0 {
            return Ok(Self(ArchivedSinceValue::BeyondPersistedRange));
        }
        let persisted = value as i64;
        if persisted <= Self::MAX_SAFE_INTEGER {
            Ok(Self(ArchivedSinceValue::ExactSafe(persisted)))
        } else {
            Ok(Self(ArchivedSinceValue::RoundedPersisted(persisted)))
        }
    }

    pub const fn persisted_value(self) -> Option<i64> {
        match self.0 {
            ArchivedSinceValue::ExactSafe(value) | ArchivedSinceValue::RoundedPersisted(value) => {
                Some(value)
            }
            ArchivedSinceValue::BeyondPersistedRange => None,
        }
    }

    pub const fn is_beyond_persisted_range(self) -> bool {
        matches!(self.0, ArchivedSinceValue::BeyondPersistedRange)
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("archived since must be a nonnegative integer")]
pub struct InvalidArchivedSince;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GlobalArchivedChannelDirectory {
    source_viewer: UserId,
    workspace_filter: Option<WorkspaceId>,
    rows: Vec<ArchivedChannelRow>,
}
impl GlobalArchivedChannelDirectory {
    pub fn projected(
        source_viewer: UserId,
        workspace_filter: Option<WorkspaceId>,
        mut rows: Vec<ArchivedChannelRow>,
    ) -> Result<Self, ChannelDirectoryProjectionError> {
        let mut ids = HashSet::new();
        for row in &rows {
            if row.source_viewer != source_viewer {
                return Err(ChannelDirectoryProjectionError::ForeignGlobalViewer);
            }
            if workspace_filter
                .as_ref()
                .is_some_and(|workspace| workspace != &row.workspace)
            {
                return Err(ChannelDirectoryProjectionError::ForeignGlobalWorkspace);
            }
            if !ids.insert(row.channel.conversation.clone()) {
                return Err(ChannelDirectoryProjectionError::DuplicateConversation);
            }
        }
        rows.sort_by(|a, b| {
            b.channel
                .archived_at
                .cmp(&a.channel.archived_at)
                .then_with(|| a.channel.created_at.cmp(&b.channel.created_at))
                .then_with(|| {
                    a.channel
                        .conversation
                        .as_str()
                        .cmp(b.channel.conversation.as_str())
                })
        });
        Ok(Self {
            source_viewer,
            workspace_filter,
            rows,
        })
    }
    pub fn bind(
        self,
        viewer: &UserId,
        workspace_filter: Option<&WorkspaceId>,
    ) -> Result<BoundGlobalArchivedChannelDirectory, ChannelDirectoryProjectionError> {
        if &self.source_viewer != viewer || self.workspace_filter.as_ref() != workspace_filter {
            return Err(ChannelDirectoryProjectionError::ForeignDirectoryCoordinate);
        }
        Ok(BoundGlobalArchivedChannelDirectory(self))
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundGlobalArchivedChannelDirectory(GlobalArchivedChannelDirectory);
impl BoundGlobalArchivedChannelDirectory {
    pub fn rows(&self) -> &[ArchivedChannelRow] {
        &self.0.rows
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ChannelDirectoryProjectionError {
    #[error("stored channel title is empty")]
    EmptyTitle,
    #[error("stored agent-home owner name is empty")]
    EmptyOwnerName,
    #[error("a fixed-roster channel cannot be public")]
    PublicFixedRoster,
    #[error("stored latest message sequence is negative")]
    NegativeLatestMessageSeq,
    #[error("directory aggregate differs from the requested viewer or workspace")]
    ForeignDirectoryCoordinate,
    #[error("directory contains a row from another viewer or workspace")]
    ForeignDirectoryRow,
    #[error("sort state belongs to another viewer or workspace")]
    ForeignSortCoordinate,
    #[error("directory contains duplicate conversation identity")]
    DuplicateConversation,
    #[error("sort state contains duplicate conversation identity")]
    DuplicateSortEntry,
    #[error("workspace kind and channel roster disagree")]
    WorkspaceKindRosterMismatch,
    #[error("archived directory contains an active row")]
    ActiveRowInArchivedDirectory,
    #[error("active directory contains an archived row")]
    ArchivedRowInActiveDirectory,
    #[error("archived directory exposed a non-member row")]
    ArchivedNonMember,
    #[error("active directory exposed a private non-member row")]
    PrivateNonMember,
    #[error("agent-home directory exposed a non-member row")]
    AgentHomeNonMember,
    #[error("sort state contains an entity outside the active directory")]
    SortEntryOutsideDirectory,
    #[error("global archived row has an empty workspace name")]
    EmptyWorkspaceName,
    #[error("global archived row belongs to another viewer")]
    ForeignGlobalViewer,
    #[error("global archived row belongs to another workspace")]
    ForeignGlobalWorkspace,
    #[error("global archived directory contains an active row")]
    ActiveRowInGlobalArchive,
    #[error("global archived directory exposed a non-member row")]
    GlobalArchivedNonMember,
    #[error("global archived workspace kind and channel roster disagree")]
    GlobalWorkspaceKindRosterMismatch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum WorkspaceDirectoryRefusal {
    #[error("workspace not found")]
    NotFound,
    #[error("viewer cannot read workspace")]
    Forbidden,
}

/// Active and archived facts are deliberately different success types.
///
/// ```compile_fail
/// use silverhand_backend_core::conversation_messaging::messaging::collaboration::channel_directory::{ActiveWorkspaceChannelDirectory, ArchivedWorkspaceChannelDirectory};
/// fn archived(_: ArchivedWorkspaceChannelDirectory) {}
/// fn cannot_splice(active: ActiveWorkspaceChannelDirectory) { archived(active); }
/// ```
pub trait ActiveChannelDirectoryPort {
    type Error: Error + Send + Sync + 'static;
    fn active(
        &self,
        viewer: &UserId,
        workspace: &WorkspaceId,
    ) -> impl Future<
        Output = Result<
            Result<ActiveWorkspaceChannelDirectory, WorkspaceDirectoryRefusal>,
            Self::Error,
        >,
    > + Send;
}
pub trait ArchivedWorkspaceChannelDirectoryPort {
    type Error: Error + Send + Sync + 'static;
    fn archived(
        &self,
        viewer: &UserId,
        workspace: &WorkspaceId,
    ) -> impl Future<
        Output = Result<
            Result<ArchivedWorkspaceChannelDirectory, WorkspaceDirectoryRefusal>,
            Self::Error,
        >,
    > + Send;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum GlobalArchivedDirectoryRefusal {
    #[error("workspace not found")]
    WorkspaceNotFound,
    #[error("viewer cannot read workspace")]
    Forbidden,
}
pub trait GlobalArchivedChannelDirectoryPort {
    type Error: Error + Send + Sync + 'static;
    fn archived_global(
        &self,
        viewer: &UserId,
        workspace_filter: Option<&WorkspaceId>,
        since: Option<ArchivedSince>,
    ) -> impl Future<
        Output = Result<
            Result<GlobalArchivedChannelDirectory, GlobalArchivedDirectoryRefusal>,
            Self::Error,
        >,
    > + Send;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(clippy::too_many_arguments)]
    fn row(
        viewer: &str,
        workspace: &str,
        id: &str,
        roster: DirectoryRoster,
        archived: Option<i64>,
        created: i64,
        member: bool,
        visibility: ChannelVisibility,
    ) -> ChannelDirectoryRow {
        ChannelDirectoryRow::projected(
            UserId::parse(viewer).unwrap(),
            WorkspaceId::parse(workspace).unwrap(),
            ConversationId::parse(id).unwrap(),
            roster,
            id.to_owned(),
            None,
            archived,
            0,
            created,
            created,
            visibility,
            member,
        )
        .unwrap()
    }
    fn sort(viewer: &str, workspace: &str, ids: &[&str]) -> ChannelSortState {
        ChannelSortState::projected(
            UserId::parse(viewer).unwrap(),
            WorkspaceId::parse(workspace).unwrap(),
            None,
            ids.iter()
                .map(|id| {
                    ChannelSortEntry::projected(
                        UserId::parse(viewer).unwrap(),
                        WorkspaceId::parse(workspace).unwrap(),
                        ConversationId::parse(*id).unwrap(),
                        None,
                        None,
                        None,
                    )
                })
                .collect(),
        )
        .unwrap()
    }
    #[test]
    fn only_active_regular_public_rows_may_widen() {
        let u = UserId::parse("u").unwrap();
        let w = WorkspaceId::parse("w").unwrap();
        assert!(
            ActiveWorkspaceChannelDirectory::projected(
                u.clone(),
                w.clone(),
                DirectoryWorkspaceKind::Regular,
                vec![row(
                    "u",
                    "w",
                    "p",
                    DirectoryRoster::Open,
                    None,
                    1,
                    false,
                    ChannelVisibility::Public
                )],
                sort("u", "w", &[])
            )
            .is_ok()
        );
        assert!(
            ArchivedWorkspaceChannelDirectory::projected(
                u.clone(),
                w.clone(),
                DirectoryWorkspaceKind::Regular,
                vec![row(
                    "u",
                    "w",
                    "p",
                    DirectoryRoster::Open,
                    Some(2),
                    1,
                    false,
                    ChannelVisibility::Public
                )]
            )
            .is_err()
        );
        assert!(
            ActiveWorkspaceChannelDirectory::projected(
                u,
                w,
                DirectoryWorkspaceKind::agent_home(AgentId::parse("a").unwrap(), "owner".into())
                    .unwrap(),
                vec![row(
                    "u",
                    "w",
                    "f",
                    DirectoryRoster::Fixed,
                    None,
                    1,
                    false,
                    ChannelVisibility::Private
                )],
                sort("u", "w", &[])
            )
            .is_err()
        );
    }
    #[test]
    fn every_face_projects_total_order() {
        let u = UserId::parse("u").unwrap();
        let w = WorkspaceId::parse("w").unwrap();
        let active = ActiveWorkspaceChannelDirectory::projected(
            u.clone(),
            w.clone(),
            DirectoryWorkspaceKind::Regular,
            vec![
                row(
                    "u",
                    "w",
                    "b",
                    DirectoryRoster::Open,
                    None,
                    1,
                    true,
                    ChannelVisibility::Private,
                ),
                row(
                    "u",
                    "w",
                    "a",
                    DirectoryRoster::Open,
                    None,
                    1,
                    true,
                    ChannelVisibility::Private,
                ),
            ],
            sort("u", "w", &[]),
        )
        .unwrap()
        .bind(&u, &w)
        .unwrap();
        assert_eq!(
            active
                .rows()
                .iter()
                .map(|r| r.conversation().as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        let archived = ArchivedWorkspaceChannelDirectory::projected(
            u.clone(),
            w.clone(),
            DirectoryWorkspaceKind::Regular,
            vec![
                row(
                    "u",
                    "w",
                    "old",
                    DirectoryRoster::Open,
                    Some(2),
                    1,
                    true,
                    ChannelVisibility::Private,
                ),
                row(
                    "u",
                    "w",
                    "new",
                    DirectoryRoster::Open,
                    Some(3),
                    1,
                    true,
                    ChannelVisibility::Private,
                ),
            ],
        )
        .unwrap()
        .bind(&u, &w)
        .unwrap();
        assert_eq!(
            archived
                .rows()
                .iter()
                .map(|r| r.conversation().as_str())
                .collect::<Vec<_>>(),
            vec!["new", "old"]
        );
    }
    #[test]
    fn agent_home_order_is_derived_from_owner_fact() {
        let u = UserId::parse("u").unwrap();
        let w = WorkspaceId::parse("w").unwrap();
        let kind = DirectoryWorkspaceKind::agent_home(AgentId::parse("a").unwrap(), "alice".into())
            .unwrap();
        let home = kind.agent_home_fact().unwrap();
        assert_eq!(home.owner().as_str(), "a");
        assert_eq!(home.owner_name(), "alice");
        let active = ActiveWorkspaceChannelDirectory::projected(
            u.clone(),
            w.clone(),
            kind,
            vec![
                row(
                    "u",
                    "w",
                    "other",
                    DirectoryRoster::Fixed,
                    None,
                    1,
                    true,
                    ChannelVisibility::Private,
                ),
                ChannelDirectoryRow {
                    title: "dm-alice".into(),
                    ..row(
                        "u",
                        "w",
                        "dm",
                        DirectoryRoster::Fixed,
                        None,
                        1,
                        true,
                        ChannelVisibility::Private,
                    )
                },
                ChannelDirectoryRow {
                    title: "Onboarding".into(),
                    ..row(
                        "u",
                        "w",
                        "onboarding",
                        DirectoryRoster::Fixed,
                        None,
                        1,
                        true,
                        ChannelVisibility::Private,
                    )
                },
            ],
            sort("u", "w", &[]),
        )
        .unwrap()
        .bind(&u, &w)
        .unwrap();
        assert_eq!(
            active
                .rows()
                .iter()
                .map(|r| r.conversation().as_str())
                .collect::<Vec<_>>(),
            vec!["onboarding", "dm", "other"]
        );
    }
    #[test]
    fn global_archive_seals_workspace_kind_and_names_corruption() {
        let u = UserId::parse("u").unwrap();
        let w = WorkspaceId::parse("w").unwrap();
        let open = row(
            "u",
            "w",
            "open",
            DirectoryRoster::Open,
            Some(3),
            1,
            true,
            ChannelVisibility::Private,
        );
        assert_eq!(
            ArchivedChannelRow::projected(
                u.clone(),
                w.clone(),
                DirectoryWorkspaceKind::agent_home(AgentId::parse("a").unwrap(), "a".into())
                    .unwrap(),
                "W".into(),
                None,
                open.clone()
            ),
            Err(ChannelDirectoryProjectionError::GlobalWorkspaceKindRosterMismatch)
        );
        let fixed = row(
            "u",
            "w",
            "fixed",
            DirectoryRoster::Fixed,
            Some(3),
            1,
            true,
            ChannelVisibility::Private,
        );
        assert_eq!(
            ArchivedChannelRow::projected(
                u.clone(),
                w.clone(),
                DirectoryWorkspaceKind::Regular,
                "W".into(),
                None,
                fixed
            ),
            Err(ChannelDirectoryProjectionError::GlobalWorkspaceKindRosterMismatch)
        );
        assert_eq!(
            ArchivedChannelRow::projected(
                u,
                w,
                DirectoryWorkspaceKind::Regular,
                "".into(),
                None,
                open
            ),
            Err(ChannelDirectoryProjectionError::EmptyWorkspaceName)
        );
    }
    #[test]
    fn archived_since_distinguishes_boundaries() {
        assert_eq!(
            ArchivedSince::from_ecmascript_number(0.0)
                .unwrap()
                .persisted_value(),
            Some(0)
        );
        assert_eq!(
            ArchivedSince::from_ecmascript_number(9_007_199_254_740_991.0)
                .unwrap()
                .persisted_value(),
            Some(9_007_199_254_740_991)
        );
        assert_eq!(
            ArchivedSince::from_ecmascript_number(9_007_199_254_740_992.0)
                .unwrap()
                .persisted_value(),
            Some(9_007_199_254_740_992)
        );
        assert!(ArchivedSince::from_ecmascript_number(-1.0).is_err());
        assert!(
            ArchivedSince::from_ecmascript_number(9_223_372_036_854_775_808.0)
                .unwrap()
                .is_beyond_persisted_range()
        );
    }
}
