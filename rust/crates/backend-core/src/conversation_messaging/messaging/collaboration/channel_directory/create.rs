//! Human channel creation: one workspace-keyed durable UoW and commit-only effects.
//!
//! Title admission is complete before the persistence port opens: the request
//! carries the core-owned canonical display/key pair. Mutable membership and
//! governance facts remain writer authorities. The application layer owns the
//! order: commit, bind the returned fact to the retained request values, then
//! attempt both post-commit obligations without ever compensating the durable
//! channel.
//!
//! The former public address trait must not return: it let any adapter invent a
//! title/key pair while claiming the pair was opaque.
//!
//! ```compile_fail
//! use silverhand_backend_core::conversation_messaging::messaging::collaboration::channel_directory::CommittedChannelAddress;
//! ```
//! A title/key pair likewise cannot be rebuilt from parts at this boundary:
//!
//! ```compile_fail
//! use silverhand_backend_core::conversation_messaging::messaging::conversation_title::CanonicalConversationTitle;
//! let _forged = CanonicalConversationTitle {
//!     title: "General".to_owned(),
//!     key: String::new(),
//! };
//! ```

use std::{collections::HashSet, error::Error, future::Future};

use crate::{
    conversation_messaging::messaging::conversation_title::CanonicalConversationTitle,
    foundation::identity::{UserId, WorkspaceId},
};

use super::{ChannelDirectoryRow, ChannelVisibility, DirectoryRoster};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HumanChannelCreateRequest {
    title: CanonicalConversationTitle,
    member_agent_ids: Vec<String>,
    member_user_ids: Option<Vec<String>>,
    visibility: ChannelVisibility,
}

impl HumanChannelCreateRequest {
    pub fn new(
        title: CanonicalConversationTitle,
        member_agent_ids: Vec<String>,
        member_user_ids: Option<Vec<String>>,
        visibility: Option<ChannelVisibility>,
    ) -> Self {
        Self {
            title,
            member_agent_ids: dedupe(member_agent_ids),
            member_user_ids: member_user_ids.map(dedupe),
            visibility: visibility.unwrap_or(ChannelVisibility::Private),
        }
    }

    pub fn title(&self) -> &CanonicalConversationTitle {
        &self.title
    }
    pub fn member_agent_ids(&self) -> &[String] {
        &self.member_agent_ids
    }
    pub fn member_user_ids(&self) -> Option<&[String]> {
        self.member_user_ids.as_deref()
    }
    pub const fn visibility(&self) -> ChannelVisibility {
        self.visibility
    }
}

fn dedupe(values: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    values
        .into_iter()
        .filter(|value| seen.insert(value.clone()))
        .collect()
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum HumanChannelCreateRefusal {
    #[error("workspace not found")]
    WorkspaceNotFound,
    #[error("viewer cannot govern workspace")]
    Forbidden,
    #[error("agent-home DM channels cannot be public")]
    AgentHomePublic,
    #[error("agent-home channels use only the home owner and current user")]
    AgentHomeExplicitMembers,
    #[error("agent-home owner not found")]
    AgentHomeOwnerNotFound,
    #[error("requested agent does not exist: {0}")]
    AgentNotFound(String),
    #[error("requested agent is not a workspace member: {0}")]
    AgentNotWorkspaceMember(String),
    #[error("requested user is not a workspace member: {0}")]
    UserNotWorkspaceMember(String),
    #[error("conversation title is already taken")]
    TitleTaken,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CreatedChannelKind {
    Regular,
    AgentHome,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommittedHumanChannelCreation {
    source_viewer: UserId,
    source_workspace: WorkspaceId,
    kind: CreatedChannelKind,
    address: CanonicalConversationTitle,
    channel: ChannelDirectoryRow,
}

impl CommittedHumanChannelCreation {
    pub fn regular(
        source_viewer: UserId,
        source_workspace: WorkspaceId,
        address: CanonicalConversationTitle,
        channel: ChannelDirectoryRow,
    ) -> Result<Self, ChannelCreationProjectionError> {
        Self::projected(
            source_viewer,
            source_workspace,
            CreatedChannelKind::Regular,
            address,
            channel,
        )
    }

    pub fn agent_home(
        source_viewer: UserId,
        source_workspace: WorkspaceId,
        address: CanonicalConversationTitle,
        channel: ChannelDirectoryRow,
    ) -> Result<Self, ChannelCreationProjectionError> {
        Self::projected(
            source_viewer,
            source_workspace,
            CreatedChannelKind::AgentHome,
            address,
            channel,
        )
    }

    fn projected(
        source_viewer: UserId,
        source_workspace: WorkspaceId,
        kind: CreatedChannelKind,
        address: CanonicalConversationTitle,
        channel: ChannelDirectoryRow,
    ) -> Result<Self, ChannelCreationProjectionError> {
        if channel.source_viewer() != &source_viewer || channel.workspace() != &source_workspace {
            return Err(ChannelCreationProjectionError::ForeignChannelCoordinate);
        }
        if channel.title() != address.title() {
            return Err(ChannelCreationProjectionError::AddressRowMismatch);
        }
        if channel.archived_at().is_some()
            || !channel.is_member()
            || channel.spawned_by_agent().is_some()
        {
            return Err(ChannelCreationProjectionError::NotFreshHumanCreation);
        }
        match (kind, channel.roster(), channel.visibility()) {
            (CreatedChannelKind::Regular, DirectoryRoster::Open, _)
            | (CreatedChannelKind::AgentHome, DirectoryRoster::Fixed, ChannelVisibility::Private) =>
                {}
            _ => return Err(ChannelCreationProjectionError::KindRosterVisibilityMismatch),
        }
        Ok(Self {
            source_viewer,
            source_workspace,
            kind,
            address,
            channel,
        })
    }

    pub fn bind(
        self,
        requested_viewer: &UserId,
        requested_workspace: &WorkspaceId,
        requested_title: &CanonicalConversationTitle,
        requested_visibility: ChannelVisibility,
    ) -> Result<BoundHumanChannelCreation, ChannelCreationProjectionError> {
        if &self.source_viewer != requested_viewer
            || &self.source_workspace != requested_workspace
            || self.channel.source_viewer() != requested_viewer
            || self.channel.workspace() != requested_workspace
        {
            return Err(ChannelCreationProjectionError::ForeignCommitCoordinate);
        }
        if &self.address != requested_title {
            return Err(ChannelCreationProjectionError::RequestedAddressMismatch);
        }
        if self.channel.visibility() != requested_visibility {
            return Err(ChannelCreationProjectionError::RequestedVisibilityMismatch);
        }
        Ok(BoundHumanChannelCreation(self))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundHumanChannelCreation(CommittedHumanChannelCreation);

impl BoundHumanChannelCreation {
    pub fn viewer(&self) -> &UserId {
        &self.0.source_viewer
    }
    pub fn workspace(&self) -> &WorkspaceId {
        &self.0.source_workspace
    }
    pub const fn kind(&self) -> CreatedChannelKind {
        self.0.kind
    }
    pub fn channel(&self) -> &ChannelDirectoryRow {
        &self.0.channel
    }
    pub fn address(&self) -> &CanonicalConversationTitle {
        &self.0.address
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ChannelCreationProjectionError {
    #[error("created channel carries a foreign viewer or workspace coordinate")]
    ForeignChannelCoordinate,
    #[error("created channel address and stored display title disagree")]
    AddressRowMismatch,
    #[error("created channel is not a fresh human-created row")]
    NotFreshHumanCreation,
    #[error("created channel kind, roster and visibility disagree")]
    KindRosterVisibilityMismatch,
    #[error("committed channel creation does not belong to the request")]
    ForeignCommitCoordinate,
    #[error("committed channel address does not equal the admitted request address")]
    RequestedAddressMismatch,
    #[error("committed channel visibility does not equal the admitted request visibility")]
    RequestedVisibilityMismatch,
}

pub trait HumanChannelCreatePort {
    type Error: Error + Send + Sync + 'static;

    fn create_human_channel(
        &self,
        viewer: UserId,
        workspace: WorkspaceId,
        request: HumanChannelCreateRequest,
    ) -> impl Future<
        Output = Result<
            Result<CommittedHumanChannelCreation, HumanChannelCreateRefusal>,
            Self::Error,
        >,
    > + Send;
}

pub trait HumanChannelCreateAftermath {
    fn workspace_channels_changed(
        &self,
        fact: &BoundHumanChannelCreation,
    ) -> impl Future<Output = Result<(), ChannelCreationPublicationFailure>> + Send;

    /// Synchronous, infallible audit mirroring the production structured warn.
    /// It is attempted after the fallible workspace publication even when that
    /// publication fails.
    fn title_address_taken(&self, fact: &BoundHumanChannelCreation);
}

#[derive(Debug, thiserror::Error)]
#[error("the committed channel creation could not be published")]
pub struct ChannelCreationPublicationFailure {
    #[source]
    source: Box<dyn Error + Send + Sync + 'static>,
}

impl ChannelCreationPublicationFailure {
    pub fn from_source(source: impl Error + Send + Sync + 'static) -> Self {
        Self {
            source: Box::new(source),
        }
    }
}

#[derive(Debug)]
pub struct HumanChannelCreateExecution {
    committed: BoundHumanChannelCreation,
    publication_failure: Option<ChannelCreationPublicationFailure>,
}

impl HumanChannelCreateExecution {
    pub fn committed(&self) -> &BoundHumanChannelCreation {
        &self.committed
    }
    pub fn publication_failure(&self) -> Option<&ChannelCreationPublicationFailure> {
        self.publication_failure.as_ref()
    }
}

pub async fn create_human_channel<P: HumanChannelCreatePort>(
    port: &P,
    aftermath: &impl HumanChannelCreateAftermath,
    viewer: UserId,
    workspace: WorkspaceId,
    request: HumanChannelCreateRequest,
) -> Result<
    Result<HumanChannelCreateExecution, HumanChannelCreateRefusal>,
    HumanChannelCreateExecutionError<P::Error>,
> {
    let requested_title = request.title.clone();
    let requested_visibility = request.visibility;
    let committed = match port
        .create_human_channel(viewer.clone(), workspace.clone(), request)
        .await
        .map_err(HumanChannelCreateExecutionError::Persistence)?
    {
        Ok(committed) => committed,
        Err(refusal) => return Ok(Err(refusal)),
    };
    let committed = committed.bind(&viewer, &workspace, &requested_title, requested_visibility)?;

    let publication_failure = aftermath.workspace_channels_changed(&committed).await.err();
    aftermath.title_address_taken(&committed);

    Ok(Ok(HumanChannelCreateExecution {
        committed,
        publication_failure,
    }))
}

#[derive(Debug, thiserror::Error)]
pub enum HumanChannelCreateExecutionError<E: Error + 'static> {
    #[error("channel creation persistence failed")]
    Persistence(#[source] E),
    #[error(transparent)]
    Identity(#[from] ChannelCreationProjectionError),
}

#[cfg(test)]
mod tests {
    use std::{
        error::Error,
        sync::{Arc, Mutex},
    };

    use crate::foundation::identity::ConversationId;

    use super::*;

    fn user(raw: &str) -> UserId {
        UserId::parse(raw).unwrap()
    }
    fn workspace(raw: &str) -> WorkspaceId {
        WorkspaceId::parse(raw).unwrap()
    }
    fn title(raw: &str) -> CanonicalConversationTitle {
        CanonicalConversationTitle::parse(raw).unwrap()
    }
    fn request(raw_title: &str, visibility: ChannelVisibility) -> HumanChannelCreateRequest {
        HumanChannelCreateRequest::new(title(raw_title), vec![], None, Some(visibility))
    }
    fn row(
        viewer: &str,
        workspace_id: &str,
        raw_title: &str,
        roster: DirectoryRoster,
        visibility: ChannelVisibility,
        archived_at: Option<i64>,
    ) -> ChannelDirectoryRow {
        ChannelDirectoryRow::projected(
            user(viewer),
            workspace(workspace_id),
            ConversationId::parse("created").unwrap(),
            roster,
            raw_title.into(),
            None,
            archived_at,
            0,
            10,
            10,
            visibility,
            true,
        )
        .unwrap()
    }
    fn regular_commit(
        raw_title: &str,
        visibility: ChannelVisibility,
    ) -> CommittedHumanChannelCreation {
        CommittedHumanChannelCreation::regular(
            user("u"),
            workspace("w"),
            title(raw_title),
            row("u", "w", raw_title, DirectoryRoster::Open, visibility, None),
        )
        .unwrap()
    }

    #[test]
    fn request_dedupes_each_explicit_member_axis_without_inventing_a_snapshot() {
        let request = HumanChannelCreateRequest::new(
            title("General"),
            vec!["a".into(), "a".into(), "b".into()],
            Some(vec!["u".into(), "u".into()]),
            None,
        );
        assert_eq!(request.member_agent_ids(), ["a", "b"]);
        assert_eq!(request.member_user_ids().unwrap(), ["u"]);
        assert_eq!(request.visibility(), ChannelVisibility::Private);

        assert_eq!(request.title().title(), "General");
        assert_eq!(request.title().key(), "general");

        let omitted = HumanChannelCreateRequest::new(title("General"), vec![], None, None);
        assert!(omitted.member_user_ids().is_none());
    }

    #[test]
    fn committed_creation_rejects_a_foreign_row_coordinate() {
        assert_eq!(
            CommittedHumanChannelCreation::regular(
                user("u"),
                workspace("w"),
                title("General"),
                row(
                    "u",
                    "other",
                    "General",
                    DirectoryRoster::Open,
                    ChannelVisibility::Private,
                    None,
                ),
            )
            .unwrap_err(),
            ChannelCreationProjectionError::ForeignChannelCoordinate
        );
    }

    #[test]
    fn committed_creation_rejects_a_kind_roster_mismatch() {
        assert_eq!(
            CommittedHumanChannelCreation::agent_home(
                user("u"),
                workspace("w"),
                title("General"),
                row(
                    "u",
                    "w",
                    "General",
                    DirectoryRoster::Open,
                    ChannelVisibility::Private,
                    None,
                ),
            )
            .unwrap_err(),
            ChannelCreationProjectionError::KindRosterVisibilityMismatch
        );
    }

    #[test]
    fn committed_creation_rejects_an_address_row_mismatch() {
        assert_eq!(
            CommittedHumanChannelCreation::regular(
                user("u"),
                workspace("w"),
                title("General"),
                row(
                    "u",
                    "w",
                    "Other",
                    DirectoryRoster::Open,
                    ChannelVisibility::Private,
                    None,
                ),
            )
            .unwrap_err(),
            ChannelCreationProjectionError::AddressRowMismatch
        );
    }

    #[test]
    fn committed_creation_rejects_a_non_fresh_row() {
        assert_eq!(
            CommittedHumanChannelCreation::regular(
                user("u"),
                workspace("w"),
                title("General"),
                row(
                    "u",
                    "w",
                    "General",
                    DirectoryRoster::Open,
                    ChannelVisibility::Private,
                    Some(1),
                ),
            )
            .unwrap_err(),
            ChannelCreationProjectionError::NotFreshHumanCreation
        );
    }

    #[test]
    fn committed_creation_binds_both_request_coordinates() {
        let committed = regular_commit("General", ChannelVisibility::Private);
        assert_eq!(
            committed.clone().bind(
                &user("other"),
                &workspace("w"),
                &title("General"),
                ChannelVisibility::Private,
            ),
            Err(ChannelCreationProjectionError::ForeignCommitCoordinate)
        );
        assert_eq!(
            committed.bind(
                &user("u"),
                &workspace("other"),
                &title("General"),
                ChannelVisibility::Private,
            ),
            Err(ChannelCreationProjectionError::ForeignCommitCoordinate)
        );
    }

    #[derive(Debug, thiserror::Error)]
    #[error("store failed")]
    struct StoreFailure;

    struct Store(CommittedHumanChannelCreation);
    impl HumanChannelCreatePort for Store {
        type Error = StoreFailure;
        async fn create_human_channel(
            &self,
            _: UserId,
            _: WorkspaceId,
            _: HumanChannelCreateRequest,
        ) -> Result<Result<CommittedHumanChannelCreation, HumanChannelCreateRefusal>, Self::Error>
        {
            Ok(Ok(self.0.clone()))
        }
    }

    struct RecordingStore {
        committed: CommittedHumanChannelCreation,
        admitted: Arc<Mutex<Vec<(String, String, ChannelVisibility)>>>,
    }
    impl HumanChannelCreatePort for RecordingStore {
        type Error = StoreFailure;
        async fn create_human_channel(
            &self,
            _: UserId,
            _: WorkspaceId,
            request: HumanChannelCreateRequest,
        ) -> Result<Result<CommittedHumanChannelCreation, HumanChannelCreateRefusal>, Self::Error>
        {
            self.admitted.lock().unwrap().push((
                request.title().title().to_owned(),
                request.title().key().to_owned(),
                request.visibility(),
            ));
            Ok(Ok(self.committed.clone()))
        }
    }

    #[derive(Debug, thiserror::Error)]
    #[error("{0}")]
    struct EffectFailure(&'static str);

    struct Effects {
        calls: Arc<Mutex<Vec<&'static str>>>,
        publication_fails: bool,
    }
    impl HumanChannelCreateAftermath for Effects {
        async fn workspace_channels_changed(
            &self,
            _: &BoundHumanChannelCreation,
        ) -> Result<(), ChannelCreationPublicationFailure> {
            self.calls.lock().unwrap().push("workspace");
            if self.publication_fails {
                return Err(ChannelCreationPublicationFailure::from_source(
                    EffectFailure("publication"),
                ));
            }
            Ok(())
        }
        fn title_address_taken(&self, _: &BoundHumanChannelCreation) {
            self.calls.lock().unwrap().push("title-audit");
        }
    }

    async fn execute(
        committed: CommittedHumanChannelCreation,
        request: HumanChannelCreateRequest,
        effects: &Effects,
    ) -> Result<
        Result<HumanChannelCreateExecution, HumanChannelCreateRefusal>,
        HumanChannelCreateExecutionError<StoreFailure>,
    > {
        create_human_channel(
            &Store(committed),
            effects,
            user("u"),
            workspace("w"),
            request,
        )
        .await
    }

    #[tokio::test]
    async fn the_same_admitted_title_pair_reaches_the_port_and_commit_bind() {
        let admitted = Arc::new(Mutex::new(vec![]));
        let effects = Effects {
            calls: Arc::new(Mutex::new(vec![])),
            publication_fails: false,
        };
        let result = create_human_channel(
            &RecordingStore {
                committed: regular_commit("Straße", ChannelVisibility::Private),
                admitted: admitted.clone(),
            },
            &effects,
            user("u"),
            workspace("w"),
            request("  Straße  ", ChannelVisibility::Private),
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(
            *admitted.lock().unwrap(),
            [(
                "Straße".into(),
                "strasse".into(),
                ChannelVisibility::Private
            )]
        );
        assert_eq!(result.committed().address().title(), "Straße");
        assert_eq!(result.committed().address().key(), "strasse");
    }

    #[tokio::test]
    async fn a_wrong_committed_title_is_rejected_before_any_effect() {
        let calls = Arc::new(Mutex::new(vec![]));
        let effects = Effects {
            calls: calls.clone(),
            publication_fails: false,
        };
        let error = execute(
            regular_commit("Other", ChannelVisibility::Private),
            request("General", ChannelVisibility::Private),
            &effects,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            HumanChannelCreateExecutionError::Identity(
                ChannelCreationProjectionError::RequestedAddressMismatch
            )
        ));
        assert!(calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_wrong_committed_visibility_is_rejected_before_any_effect() {
        let calls = Arc::new(Mutex::new(vec![]));
        let effects = Effects {
            calls: calls.clone(),
            publication_fails: false,
        };
        let error = execute(
            regular_commit("General", ChannelVisibility::Public),
            request("General", ChannelVisibility::Private),
            &effects,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            HumanChannelCreateExecutionError::Identity(
                ChannelCreationProjectionError::RequestedVisibilityMismatch
            )
        ));
        assert!(calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn publication_failure_does_not_skip_the_infallible_title_audit() {
        let calls = Arc::new(Mutex::new(vec![]));
        let effects = Effects {
            calls: calls.clone(),
            publication_fails: true,
        };
        let result = execute(
            regular_commit("General", ChannelVisibility::Private),
            request("General", ChannelVisibility::Private),
            &effects,
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(*calls.lock().unwrap(), ["workspace", "title-audit"]);
        let failure = result.publication_failure().unwrap();
        assert_eq!(failure.source().unwrap().to_string(), "publication");
        assert_eq!(
            result.committed().channel().conversation().as_str(),
            "created"
        );
        assert_eq!(result.committed().address().key(), "general");
    }
}
