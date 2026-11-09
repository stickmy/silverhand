//! Channel lifecycle mutation contracts.
//!
//! Preflight is only an auth-first/body-late optimization. Each write port owes
//! a fresh `BEGIN IMMEDIATE` authority read and returns an operation-specific
//! committed fact. Effects see only the bound fact and cannot compensate it.

use std::{collections::HashSet, error::Error, future::Future};

use crate::{
    conversation_messaging::messaging::{
        MessageId, MessageSeq,
        collaboration::channel_directory::{ChannelVisibility, DirectoryRoster},
        conversation_title::CanonicalConversationTitle,
    },
    foundation::identity::{AgentId, ConversationId, UserId, WorkspaceId},
};

use super::ChannelLifecycleRow;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GovernedChannelTarget {
    viewer: UserId,
    conversation: ConversationId,
    workspace: WorkspaceId,
}

impl GovernedChannelTarget {
    pub fn resolved(viewer: UserId, conversation: ConversationId, workspace: WorkspaceId) -> Self {
        Self {
            viewer,
            conversation,
            workspace,
        }
    }

    pub fn bind(
        self,
        viewer: &UserId,
        conversation: &ConversationId,
    ) -> Result<BoundGovernedChannelTarget, ChannelMutationIdentityMismatch> {
        if &self.viewer != viewer || &self.conversation != conversation {
            return Err(ChannelMutationIdentityMismatch);
        }
        Ok(BoundGovernedChannelTarget(self))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundGovernedChannelTarget(GovernedChannelTarget);

impl BoundGovernedChannelTarget {
    pub fn workspace(&self) -> &WorkspaceId {
        &self.0.workspace
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ChannelMutationAdmissionRefusal {
    #[error("channel not found")]
    NotFound,
    #[error("viewer cannot govern the channel workspace")]
    Forbidden,
}

pub trait ChannelMutationAdmissionPort {
    type Error: Error + Send + Sync + 'static;

    fn admit(
        &self,
        viewer: &UserId,
        conversation: &ConversationId,
    ) -> impl Future<
        Output = Result<
            Result<GovernedChannelTarget, ChannelMutationAdmissionRefusal>,
            Self::Error,
        >,
    > + Send;
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MutationCommit {
    viewer: UserId,
    workspace: WorkspaceId,
    row: ChannelLifecycleRow,
}

impl MutationCommit {
    fn new(
        viewer: UserId,
        workspace: WorkspaceId,
        row: ChannelLifecycleRow,
    ) -> Result<Self, ChannelMutationProjectionError> {
        if row.workspace() != Some(&workspace) {
            return Err(ChannelMutationProjectionError::WorkspaceCoordinate);
        }
        Ok(Self {
            viewer,
            workspace,
            row,
        })
    }

    fn bind(
        &self,
        viewer: &UserId,
        conversation: &ConversationId,
    ) -> Result<(), ChannelMutationIdentityMismatch> {
        if &self.viewer != viewer || self.row.conversation() != conversation {
            return Err(ChannelMutationIdentityMismatch);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommittedChannelArchive {
    commit: MutationCommit,
    archived: bool,
}

impl CommittedChannelArchive {
    pub fn committed(
        viewer: UserId,
        workspace: WorkspaceId,
        row: ChannelLifecycleRow,
        archived: bool,
    ) -> Result<Self, ChannelMutationProjectionError> {
        if row.archived_at().is_some() != archived {
            return Err(ChannelMutationProjectionError::ArchiveState);
        }
        Ok(Self {
            commit: MutationCommit::new(viewer, workspace, row)?,
            archived,
        })
    }

    pub fn bind(
        self,
        viewer: &UserId,
        conversation: &ConversationId,
        archived: bool,
    ) -> Result<BoundChannelArchive, ChannelMutationIdentityMismatch> {
        self.commit.bind(viewer, conversation)?;
        if self.archived != archived {
            return Err(ChannelMutationIdentityMismatch);
        }
        Ok(BoundChannelArchive(self))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundChannelArchive(CommittedChannelArchive);

impl BoundChannelArchive {
    pub fn viewer(&self) -> &UserId {
        &self.0.commit.viewer
    }
    pub fn workspace(&self) -> &WorkspaceId {
        &self.0.commit.workspace
    }
    pub fn row(&self) -> &ChannelLifecycleRow {
        &self.0.commit.row
    }
    pub const fn archived(&self) -> bool {
        self.0.archived
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ChannelArchiveRefusal {
    #[error("channel not found")]
    NotFound,
    #[error("viewer cannot govern the channel workspace")]
    Forbidden,
}

pub trait ChannelArchivePort {
    type Error: Error + Send + Sync + 'static;

    fn set_archived(
        &self,
        viewer: UserId,
        conversation: ConversationId,
        archived: bool,
    ) -> impl Future<
        Output = Result<Result<CommittedChannelArchive, ChannelArchiveRefusal>, Self::Error>,
    > + Send;
}

pub trait ChannelArchiveAftermath {
    /// Starts the best-effort runtime cleanup and returns immediately.
    ///
    /// The production authority resolves runs synchronously, then launches each
    /// host stop as fire-and-forget work whose failure is logged locally. A slow
    /// or offline host must never delay either read-face publication below.
    fn start_runtime_cleanup(&self, fact: &BoundChannelArchive);
    fn workspace_channels_changed(
        &self,
        fact: &BoundChannelArchive,
    ) -> impl Future<Output = Result<(), ChannelMutationEffectFailure>> + Send;
    fn unread_updated(
        &self,
        fact: &BoundChannelArchive,
    ) -> impl Future<Output = Result<(), ChannelMutationEffectFailure>> + Send;
}

pub async fn archive_channel<P: ChannelArchivePort>(
    port: &P,
    aftermath: &impl ChannelArchiveAftermath,
    viewer: UserId,
    conversation: ConversationId,
    archived: bool,
) -> Result<
    Result<ChannelMutationExecution<BoundChannelArchive>, ChannelArchiveRefusal>,
    ChannelMutationExecutionError<P::Error>,
> {
    let committed = match port
        .set_archived(viewer.clone(), conversation.clone(), archived)
        .await
        .map_err(ChannelMutationExecutionError::Persistence)?
    {
        Ok(value) => value,
        Err(refusal) => return Ok(Err(refusal)),
    };
    let committed = committed.bind(&viewer, &conversation, archived)?;
    let mut first_failure = None;
    if archived {
        aftermath.start_runtime_cleanup(&committed);
    }
    retain_first(
        &mut first_failure,
        aftermath.workspace_channels_changed(&committed).await.err(),
    );
    retain_first(
        &mut first_failure,
        aftermath.unread_updated(&committed).await.err(),
    );
    Ok(Ok(ChannelMutationExecution {
        committed,
        first_failure,
    }))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommittedChannelVisibility {
    commit: MutationCommit,
    visibility: ChannelVisibility,
    changed: bool,
}

impl CommittedChannelVisibility {
    pub fn changed(
        viewer: UserId,
        workspace: WorkspaceId,
        row: ChannelLifecycleRow,
        visibility: ChannelVisibility,
    ) -> Result<Self, ChannelMutationProjectionError> {
        Self::new(viewer, workspace, row, visibility, true)
    }

    pub fn unchanged(
        viewer: UserId,
        workspace: WorkspaceId,
        row: ChannelLifecycleRow,
        visibility: ChannelVisibility,
    ) -> Result<Self, ChannelMutationProjectionError> {
        Self::new(viewer, workspace, row, visibility, false)
    }

    fn new(
        viewer: UserId,
        workspace: WorkspaceId,
        row: ChannelLifecycleRow,
        visibility: ChannelVisibility,
        changed: bool,
    ) -> Result<Self, ChannelMutationProjectionError> {
        if row.roster() != DirectoryRoster::Open || row.visibility() != visibility {
            return Err(ChannelMutationProjectionError::VisibilityState);
        }
        Ok(Self {
            commit: MutationCommit::new(viewer, workspace, row)?,
            visibility,
            changed,
        })
    }

    pub fn bind(
        self,
        viewer: &UserId,
        conversation: &ConversationId,
        visibility: ChannelVisibility,
    ) -> Result<BoundChannelVisibility, ChannelMutationIdentityMismatch> {
        self.commit.bind(viewer, conversation)?;
        if self.visibility != visibility {
            return Err(ChannelMutationIdentityMismatch);
        }
        Ok(BoundChannelVisibility(self))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundChannelVisibility(CommittedChannelVisibility);

impl BoundChannelVisibility {
    pub fn viewer(&self) -> &UserId {
        &self.0.commit.viewer
    }
    pub fn workspace(&self) -> &WorkspaceId {
        &self.0.commit.workspace
    }
    pub fn row(&self) -> &ChannelLifecycleRow {
        &self.0.commit.row
    }
    pub const fn changed(&self) -> bool {
        self.0.changed
    }
    pub const fn visibility(&self) -> ChannelVisibility {
        self.0.visibility
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ChannelVisibilityRefusal {
    #[error("channel not found")]
    NotFound,
    #[error("viewer cannot govern the channel workspace")]
    Forbidden,
}

pub trait ChannelVisibilityPort {
    type Error: Error + Send + Sync + 'static;

    fn set_visibility(
        &self,
        viewer: UserId,
        conversation: ConversationId,
        visibility: ChannelVisibility,
    ) -> impl Future<
        Output = Result<Result<CommittedChannelVisibility, ChannelVisibilityRefusal>, Self::Error>,
    > + Send;
}

pub trait ChannelVisibilityAftermath {
    fn workspace_channels_changed(
        &self,
        fact: &BoundChannelVisibility,
    ) -> impl Future<Output = Result<(), ChannelMutationEffectFailure>> + Send;
}

pub async fn set_channel_visibility<P: ChannelVisibilityPort>(
    port: &P,
    aftermath: &impl ChannelVisibilityAftermath,
    viewer: UserId,
    conversation: ConversationId,
    visibility: ChannelVisibility,
) -> Result<
    Result<ChannelMutationExecution<BoundChannelVisibility>, ChannelVisibilityRefusal>,
    ChannelMutationExecutionError<P::Error>,
> {
    let committed = match port
        .set_visibility(viewer.clone(), conversation.clone(), visibility)
        .await
        .map_err(ChannelMutationExecutionError::Persistence)?
    {
        Ok(value) => value,
        Err(refusal) => return Ok(Err(refusal)),
    };
    let committed = committed.bind(&viewer, &conversation, visibility)?;
    let first_failure = if committed.changed() {
        aftermath.workspace_channels_changed(&committed).await.err()
    } else {
        None
    };
    Ok(Ok(ChannelMutationExecution {
        committed,
        first_failure,
    }))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommittedRenameNotice {
    conversation: ConversationId,
    old: CanonicalConversationTitle,
    new: CanonicalConversationTitle,
    message: MessageId,
    seq: MessageSeq,
    created_at: i64,
    content: String,
}

impl CommittedRenameNotice {
    pub fn for_titles(
        conversation: ConversationId,
        message: MessageId,
        seq: MessageSeq,
        created_at: i64,
        old: &CanonicalConversationTitle,
        new: &CanonicalConversationTitle,
    ) -> Self {
        Self {
            conversation,
            old: old.clone(),
            new: new.clone(),
            message,
            seq,
            created_at,
            content: format!(
                "This channel was renamed from “{}” to “{}”. A channel is addressed by its title, so the old one no longer reaches this channel — update any references in your MEMORY.md when you next edit it.",
                old.title(),
                new.title()
            ),
        }
    }

    pub fn conversation(&self) -> &ConversationId {
        &self.conversation
    }
    pub fn message(&self) -> &MessageId {
        &self.message
    }
    pub fn old(&self) -> &CanonicalConversationTitle {
        &self.old
    }
    pub fn new_title(&self) -> &CanonicalConversationTitle {
        &self.new
    }
    pub const fn seq(&self) -> MessageSeq {
        self.seq
    }
    pub const fn created_at(&self) -> i64 {
        self.created_at
    }
    pub fn content(&self) -> &str {
        &self.content
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommittedChannelRename {
    commit: MutationCommit,
    requested: CanonicalConversationTitle,
    change: Option<CommittedRenameChange>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CommittedRenameChange {
    old: CanonicalConversationTitle,
    notice: CommittedRenameNotice,
    recipients: Vec<RenameRecipient>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenameRecipient {
    conversation: ConversationId,
    agent: AgentId,
}

impl RenameRecipient {
    pub fn selected(conversation: ConversationId, agent: AgentId) -> Self {
        Self {
            conversation,
            agent,
        }
    }

    pub fn agent(&self) -> &AgentId {
        &self.agent
    }
}

impl CommittedChannelRename {
    pub fn unchanged(
        viewer: UserId,
        workspace: WorkspaceId,
        row: ChannelLifecycleRow,
        requested: CanonicalConversationTitle,
    ) -> Result<Self, ChannelMutationProjectionError> {
        if row.title() != requested.title() || row.title_key() != Some(requested.key()) {
            return Err(ChannelMutationProjectionError::RenameState);
        }
        Ok(Self {
            commit: MutationCommit::new(viewer, workspace, row)?,
            requested,
            change: None,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn changed(
        viewer: UserId,
        workspace: WorkspaceId,
        row: ChannelLifecycleRow,
        requested: CanonicalConversationTitle,
        old: CanonicalConversationTitle,
        notice: CommittedRenameNotice,
        mut recipients: Vec<RenameRecipient>,
    ) -> Result<Self, ChannelMutationProjectionError> {
        if old.title() == requested.title()
            || row.title() != requested.title()
            || row.title_key() != Some(requested.key())
            || notice.conversation() != row.conversation()
            || notice.old() != &old
            || notice.new_title() != &requested
            || recipients
                .iter()
                .any(|recipient| &recipient.conversation != row.conversation())
        {
            return Err(ChannelMutationProjectionError::RenameState);
        }
        let unique = recipients
            .iter()
            .map(|recipient| &recipient.agent)
            .collect::<HashSet<_>>()
            .len()
            == recipients.len();
        if !unique {
            return Err(ChannelMutationProjectionError::DuplicateRenameRecipient);
        }
        recipients.sort_unstable_by(|left, right| left.agent.as_str().cmp(right.agent.as_str()));
        Ok(Self {
            commit: MutationCommit::new(viewer, workspace, row)?,
            requested,
            change: Some(CommittedRenameChange {
                old,
                notice,
                recipients,
            }),
        })
    }

    pub fn bind(
        self,
        viewer: &UserId,
        conversation: &ConversationId,
        requested: &CanonicalConversationTitle,
    ) -> Result<BoundChannelRename, ChannelMutationIdentityMismatch> {
        self.commit.bind(viewer, conversation)?;
        if &self.requested != requested {
            return Err(ChannelMutationIdentityMismatch);
        }
        Ok(BoundChannelRename(self))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundChannelRename(CommittedChannelRename);

impl BoundChannelRename {
    pub fn viewer(&self) -> &UserId {
        &self.0.commit.viewer
    }
    pub fn workspace(&self) -> &WorkspaceId {
        &self.0.commit.workspace
    }
    pub fn row(&self) -> &ChannelLifecycleRow {
        &self.0.commit.row
    }
    pub fn requested(&self) -> &CanonicalConversationTitle {
        &self.0.requested
    }
    pub fn changed(&self) -> bool {
        self.0.change.is_some()
    }
    pub fn old(&self) -> Option<&CanonicalConversationTitle> {
        self.0.change.as_ref().map(|value| &value.old)
    }
    pub fn notice(&self) -> Option<&CommittedRenameNotice> {
        self.0.change.as_ref().map(|value| &value.notice)
    }
    pub fn recipient_rows(&self) -> impl Iterator<Item = &AgentId> {
        self.0
            .change
            .iter()
            .flat_map(|value| value.recipients.iter().map(RenameRecipient::agent))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ChannelRenameRefusal {
    #[error("channel not found")]
    NotFound,
    #[error("viewer cannot govern the channel workspace")]
    Forbidden,
    #[error("conversation has no workspace title address")]
    NotAddressable,
    #[error("conversation title is already taken")]
    TitleTaken,
}

pub trait ChannelRenamePort {
    type Error: Error + Send + Sync + 'static;

    fn rename(
        &self,
        viewer: UserId,
        conversation: ConversationId,
        requested: CanonicalConversationTitle,
    ) -> impl Future<
        Output = Result<Result<CommittedChannelRename, ChannelRenameRefusal>, Self::Error>,
    > + Send;
}

pub trait ChannelRenameAftermath {
    fn title_address_moved(&self, fact: &BoundChannelRename);
    fn deliver(
        &self,
        fact: &BoundChannelRename,
        recipient: &AgentId,
    ) -> impl Future<Output = Result<(), ChannelMutationEffectFailure>> + Send;
    fn publish_message(
        &self,
        fact: &BoundChannelRename,
    ) -> impl Future<Output = Result<(), ChannelMutationEffectFailure>> + Send;
    fn workspace_channels_changed(
        &self,
        fact: &BoundChannelRename,
    ) -> impl Future<Output = Result<(), ChannelMutationEffectFailure>> + Send;
}

pub async fn rename_channel<P: ChannelRenamePort>(
    port: &P,
    aftermath: &impl ChannelRenameAftermath,
    viewer: UserId,
    conversation: ConversationId,
    requested: CanonicalConversationTitle,
) -> Result<
    Result<ChannelMutationExecution<BoundChannelRename>, ChannelRenameRefusal>,
    ChannelMutationExecutionError<P::Error>,
> {
    let committed = match port
        .rename(viewer.clone(), conversation.clone(), requested.clone())
        .await
        .map_err(ChannelMutationExecutionError::Persistence)?
    {
        Ok(value) => value,
        Err(refusal) => return Ok(Err(refusal)),
    };
    let committed = committed.bind(&viewer, &conversation, &requested)?;
    let mut first_failure = None;
    if committed.changed() {
        aftermath.title_address_moved(&committed);
        for recipient in committed.recipient_rows() {
            retain_first(
                &mut first_failure,
                aftermath.deliver(&committed, recipient).await.err(),
            );
        }
        retain_first(
            &mut first_failure,
            aftermath.publish_message(&committed).await.err(),
        );
        retain_first(
            &mut first_failure,
            aftermath.workspace_channels_changed(&committed).await.err(),
        );
    }
    Ok(Ok(ChannelMutationExecution {
        committed,
        first_failure,
    }))
}

fn retain_first(
    slot: &mut Option<ChannelMutationEffectFailure>,
    candidate: Option<ChannelMutationEffectFailure>,
) {
    if slot.is_none() {
        *slot = candidate;
    }
}

#[derive(Debug)]
pub struct ChannelMutationExecution<T> {
    committed: T,
    first_failure: Option<ChannelMutationEffectFailure>,
}

impl<T> ChannelMutationExecution<T> {
    pub fn committed(&self) -> &T {
        &self.committed
    }
    pub fn first_failure(&self) -> Option<&ChannelMutationEffectFailure> {
        self.first_failure.as_ref()
    }
}

#[derive(Debug, thiserror::Error)]
#[error("channel lifecycle post-commit effect failed")]
pub struct ChannelMutationEffectFailure {
    #[source]
    source: Box<dyn Error + Send + Sync + 'static>,
}

impl ChannelMutationEffectFailure {
    pub fn from_source(source: impl Error + Send + Sync + 'static) -> Self {
        Self {
            source: Box::new(source),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ChannelMutationExecutionError<E: Error + 'static> {
    #[error("channel lifecycle persistence failed")]
    Persistence(#[source] E),
    #[error(transparent)]
    Identity(#[from] ChannelMutationIdentityMismatch),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("committed channel mutation does not belong to the authenticated request")]
pub struct ChannelMutationIdentityMismatch;

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ChannelMutationProjectionError {
    #[error("committed row does not belong to its workspace coordinate")]
    WorkspaceCoordinate,
    #[error("committed archive state differs from the requested state")]
    ArchiveState,
    #[error("visibility result is not an open workspace channel at the requested value")]
    VisibilityState,
    #[error("rename old/new/notice facts are inconsistent")]
    RenameState,
    #[error("rename recipients contain a duplicate agent identity")]
    DuplicateRenameRecipient,
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use crate::{
        conversation_messaging::messaging::collaboration::channel_directory::DirectoryRoster,
        foundation::persistence::PersistenceFailure,
    };

    use super::*;

    fn user(raw: &str) -> UserId {
        UserId::parse(raw).unwrap()
    }
    fn conversation(raw: &str) -> ConversationId {
        ConversationId::parse(raw).unwrap()
    }
    fn workspace(raw: &str) -> WorkspaceId {
        WorkspaceId::parse(raw).unwrap()
    }
    fn title(raw: &str) -> CanonicalConversationTitle {
        CanonicalConversationTitle::parse(raw).unwrap()
    }
    fn row(
        id: &str,
        title_raw: &str,
        visibility: ChannelVisibility,
        archived: bool,
    ) -> ChannelLifecycleRow {
        ChannelLifecycleRow::workspace_placed(
            conversation(id),
            workspace("w"),
            DirectoryRoster::Open,
            title(title_raw),
            None,
            archived.then_some(9),
            0,
            1,
            2,
            visibility,
        )
        .unwrap()
    }

    #[test]
    fn admission_and_each_commit_bind_every_request_coordinate() {
        let admission =
            GovernedChannelTarget::resolved(user("u"), conversation("c"), workspace("w"));
        assert!(
            admission
                .clone()
                .bind(&user("other"), &conversation("c"))
                .is_err()
        );
        assert!(
            admission
                .clone()
                .bind(&user("u"), &conversation("other"))
                .is_err()
        );
        assert!(admission.bind(&user("u"), &conversation("c")).is_ok());

        let archive = CommittedChannelArchive::committed(
            user("u"),
            workspace("w"),
            row("c", "General", ChannelVisibility::Private, true),
            true,
        )
        .unwrap();
        assert!(
            archive
                .clone()
                .bind(&user("u"), &conversation("c"), false)
                .is_err()
        );
        assert!(archive.bind(&user("u"), &conversation("c"), true).is_ok());

        let visibility = CommittedChannelVisibility::changed(
            user("u"),
            workspace("w"),
            row("c", "General", ChannelVisibility::Public, false),
            ChannelVisibility::Public,
        )
        .unwrap();
        assert!(
            visibility
                .clone()
                .bind(
                    &user("other"),
                    &conversation("c"),
                    ChannelVisibility::Public
                )
                .is_err()
        );
        assert!(
            visibility
                .clone()
                .bind(&user("u"), &conversation("c"), ChannelVisibility::Private)
                .is_err()
        );
        assert!(
            visibility
                .bind(&user("u"), &conversation("c"), ChannelVisibility::Public)
                .is_ok()
        );

        let rename = rename_commit(true);
        assert!(
            rename
                .clone()
                .bind(&user("u"), &conversation("c"), &title("Other"))
                .is_err()
        );
        assert!(
            rename
                .bind(&user("u"), &conversation("c"), &title("Roadmap"))
                .is_ok()
        );
    }

    #[derive(Clone)]
    struct RenamePort(CommittedChannelRename);
    impl ChannelRenamePort for RenamePort {
        type Error = PersistenceFailure;
        async fn rename(
            &self,
            _: UserId,
            _: ConversationId,
            _: CanonicalConversationTitle,
        ) -> Result<Result<CommittedChannelRename, ChannelRenameRefusal>, Self::Error> {
            Ok(Ok(self.0.clone()))
        }
    }

    #[derive(Clone, Debug, thiserror::Error)]
    #[error("{0}")]
    struct Boom(&'static str);

    #[derive(Default)]
    struct RenameEffects {
        calls: Arc<Mutex<Vec<String>>>,
    }
    impl ChannelRenameAftermath for RenameEffects {
        fn title_address_moved(&self, _: &BoundChannelRename) {
            self.calls.lock().unwrap().push("audit".into());
        }
        async fn deliver(
            &self,
            _: &BoundChannelRename,
            recipient: &AgentId,
        ) -> Result<(), ChannelMutationEffectFailure> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("deliver:{}", recipient.as_str()));
            Err(ChannelMutationEffectFailure::from_source(Boom(
                if recipient.as_str() == "a" {
                    "first"
                } else {
                    "later"
                },
            )))
        }
        async fn publish_message(
            &self,
            _: &BoundChannelRename,
        ) -> Result<(), ChannelMutationEffectFailure> {
            self.calls.lock().unwrap().push("message".into());
            Ok(())
        }
        async fn workspace_channels_changed(
            &self,
            _: &BoundChannelRename,
        ) -> Result<(), ChannelMutationEffectFailure> {
            self.calls.lock().unwrap().push("workspace".into());
            Ok(())
        }
    }

    fn rename_commit(changed: bool) -> CommittedChannelRename {
        let requested = title(if changed { "Roadmap" } else { "General" });
        let updated = row("c", requested.title(), ChannelVisibility::Private, false);
        if !changed {
            return CommittedChannelRename::unchanged(
                user("u"),
                workspace("w"),
                updated,
                requested,
            )
            .unwrap();
        }
        let old = title("General");
        let notice = CommittedRenameNotice::for_titles(
            conversation("c"),
            MessageId::parse("m").unwrap(),
            MessageSeq::try_from(1).unwrap(),
            3,
            &old,
            &requested,
        );
        CommittedChannelRename::changed(
            user("u"),
            workspace("w"),
            updated,
            requested,
            old,
            notice,
            vec![
                RenameRecipient::selected(conversation("c"), AgentId::parse("b").unwrap()),
                RenameRecipient::selected(conversation("c"), AgentId::parse("a").unwrap()),
            ],
        )
        .unwrap()
    }

    #[test]
    fn rename_change_seals_notice_titles_and_every_recipient_coordinate() {
        let requested = title("Roadmap");
        let old = title("General");
        let updated = row("c", requested.title(), ChannelVisibility::Private, false);

        let foreign_notice = CommittedRenameNotice::for_titles(
            conversation("c"),
            MessageId::parse("m").unwrap(),
            MessageSeq::try_from(1).unwrap(),
            3,
            &title("Other"),
            &requested,
        );
        assert_eq!(
            CommittedChannelRename::changed(
                user("u"),
                workspace("w"),
                updated.clone(),
                requested.clone(),
                old.clone(),
                foreign_notice,
                vec![],
            )
            .unwrap_err(),
            ChannelMutationProjectionError::RenameState
        );

        let notice = CommittedRenameNotice::for_titles(
            conversation("c"),
            MessageId::parse("m").unwrap(),
            MessageSeq::try_from(1).unwrap(),
            3,
            &old,
            &requested,
        );
        assert_eq!(
            CommittedChannelRename::changed(
                user("u"),
                workspace("w"),
                updated.clone(),
                requested.clone(),
                old.clone(),
                notice.clone(),
                vec![RenameRecipient::selected(
                    conversation("foreign"),
                    AgentId::parse("a").unwrap(),
                )],
            )
            .unwrap_err(),
            ChannelMutationProjectionError::RenameState
        );
        assert_eq!(
            CommittedChannelRename::changed(
                user("u"),
                workspace("w"),
                updated,
                requested,
                old,
                notice,
                vec![
                    RenameRecipient::selected(conversation("c"), AgentId::parse("a").unwrap(),),
                    RenameRecipient::selected(conversation("c"), AgentId::parse("a").unwrap(),),
                ],
            )
            .unwrap_err(),
            ChannelMutationProjectionError::DuplicateRenameRecipient
        );
    }

    #[tokio::test]
    async fn rename_changed_only_effects_are_total_and_preserve_the_first_cause() {
        let effects = RenameEffects::default();
        let result = rename_channel(
            &RenamePort(rename_commit(true)),
            &effects,
            user("u"),
            conversation("c"),
            title("Roadmap"),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(result.committed().changed());
        assert_eq!(
            effects.calls.lock().unwrap().as_slice(),
            ["audit", "deliver:a", "deliver:b", "message", "workspace"]
        );
        let source = std::error::Error::source(result.first_failure().unwrap()).unwrap();
        assert_eq!(source.to_string(), "first");

        let no_op_effects = RenameEffects::default();
        let result = rename_channel(
            &RenamePort(rename_commit(false)),
            &no_op_effects,
            user("u"),
            conversation("c"),
            title("General"),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!result.committed().changed());
        assert!(no_op_effects.calls.lock().unwrap().is_empty());
    }

    struct VisibilityEffects {
        calls: Arc<Mutex<Vec<&'static str>>>,
    }
    impl ChannelVisibilityAftermath for VisibilityEffects {
        async fn workspace_channels_changed(
            &self,
            _: &BoundChannelVisibility,
        ) -> Result<(), ChannelMutationEffectFailure> {
            self.calls.lock().unwrap().push("workspace");
            Err(ChannelMutationEffectFailure::from_source(Boom(
                "visibility-publication",
            )))
        }
    }

    #[derive(Clone)]
    struct VisibilityPort(CommittedChannelVisibility);
    impl ChannelVisibilityPort for VisibilityPort {
        type Error = PersistenceFailure;
        async fn set_visibility(
            &self,
            _: UserId,
            _: ConversationId,
            _: ChannelVisibility,
        ) -> Result<Result<CommittedChannelVisibility, ChannelVisibilityRefusal>, Self::Error>
        {
            Ok(Ok(self.0.clone()))
        }
    }

    #[tokio::test]
    async fn visibility_noop_is_silent_and_changed_has_one_product_effect() {
        for (changed, expected) in [(false, vec![]), (true, vec!["workspace"])] {
            let committed = if changed {
                CommittedChannelVisibility::changed(
                    user("u"),
                    workspace("w"),
                    row("c", "General", ChannelVisibility::Public, false),
                    ChannelVisibility::Public,
                )
                .unwrap()
            } else {
                CommittedChannelVisibility::unchanged(
                    user("u"),
                    workspace("w"),
                    row("c", "General", ChannelVisibility::Public, false),
                    ChannelVisibility::Public,
                )
                .unwrap()
            };
            let effects = VisibilityEffects {
                calls: Arc::new(Mutex::new(vec![])),
            };
            let result = set_channel_visibility(
                &VisibilityPort(committed),
                &effects,
                user("u"),
                conversation("c"),
                ChannelVisibility::Public,
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(*effects.calls.lock().unwrap(), expected);
            if changed {
                let source = std::error::Error::source(result.first_failure().unwrap()).unwrap();
                assert_eq!(source.to_string(), "visibility-publication");
            } else {
                assert!(result.first_failure().is_none());
            }
        }
    }

    #[derive(Clone)]
    struct ArchivePort(CommittedChannelArchive);
    impl ChannelArchivePort for ArchivePort {
        type Error = PersistenceFailure;

        async fn set_archived(
            &self,
            _: UserId,
            _: ConversationId,
            _: bool,
        ) -> Result<Result<CommittedChannelArchive, ChannelArchiveRefusal>, Self::Error> {
            Ok(Ok(self.0.clone()))
        }
    }

    #[derive(Default)]
    struct ArchiveEffects {
        calls: Arc<Mutex<Vec<&'static str>>>,
    }
    impl ChannelArchiveAftermath for ArchiveEffects {
        fn start_runtime_cleanup(&self, _: &BoundChannelArchive) {
            self.calls.lock().unwrap().push("runtime");
        }

        async fn workspace_channels_changed(
            &self,
            _: &BoundChannelArchive,
        ) -> Result<(), ChannelMutationEffectFailure> {
            self.calls.lock().unwrap().push("workspace");
            Err(ChannelMutationEffectFailure::from_source(Boom(
                "workspace-later",
            )))
        }

        async fn unread_updated(
            &self,
            _: &BoundChannelArchive,
        ) -> Result<(), ChannelMutationEffectFailure> {
            self.calls.lock().unwrap().push("unread");
            Err(ChannelMutationEffectFailure::from_source(Boom(
                "unread-later",
            )))
        }
    }

    #[tokio::test]
    async fn archive_and_unarchive_attempt_applicable_effects_and_preserve_first_failure() {
        let archived = CommittedChannelArchive::committed(
            user("u"),
            workspace("w"),
            row("c", "General", ChannelVisibility::Private, true),
            true,
        )
        .unwrap();
        let effects = ArchiveEffects::default();
        let result = archive_channel(
            &ArchivePort(archived),
            &effects,
            user("u"),
            conversation("c"),
            true,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            effects.calls.lock().unwrap().as_slice(),
            ["runtime", "workspace", "unread"]
        );
        let source = std::error::Error::source(result.first_failure().unwrap()).unwrap();
        assert_eq!(source.to_string(), "workspace-later");

        let unarchived = CommittedChannelArchive::committed(
            user("u"),
            workspace("w"),
            row("c", "General", ChannelVisibility::Private, false),
            false,
        )
        .unwrap();
        let effects = ArchiveEffects::default();
        let result = archive_channel(
            &ArchivePort(unarchived),
            &effects,
            user("u"),
            conversation("c"),
            false,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            effects.calls.lock().unwrap().as_slice(),
            ["workspace", "unread"]
        );
        let source = std::error::Error::source(result.first_failure().unwrap()).unwrap();
        assert_eq!(source.to_string(), "workspace-later");
    }
}
