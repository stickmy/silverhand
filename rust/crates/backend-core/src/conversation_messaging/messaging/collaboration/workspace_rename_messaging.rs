//! Narrow messaging prerequisites consumed by workspace rename.
//!
//! This module does not own workspace mutation, runtime delivery, publication or
//! viewer events. It owns only the two messaging facts that #2193 consumes:
//! which workspace channels have an open roster, and one committed
//! system rename message in each selected conversation.

use std::{error::Error, future::Future};

use crate::{
    conversation_messaging::messaging::{MessageId, MessageSeq, markdown::inline_code},
    foundation::identity::{AgentId, ConversationId, WorkspaceId},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceRenameChannel {
    workspace: WorkspaceId,
    conversation: ConversationId,
}

impl WorkspaceRenameChannel {
    pub fn selected(workspace: WorkspaceId, conversation: ConversationId) -> Self {
        Self {
            workspace,
            conversation,
        }
    }

    pub fn conversation(&self) -> &ConversationId {
        &self.conversation
    }
}

/// All open-roster channels selected for one workspace rename.
///
/// Archive state is intentionally absent: the existing product announces into
/// archived channels too. Agent-home/fixed-roster conversations are not members
/// of this population.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceRenameChannels {
    workspace: WorkspaceId,
    channels: Vec<WorkspaceRenameChannel>,
}

impl WorkspaceRenameChannels {
    pub fn selected(
        workspace: WorkspaceId,
        mut channels: Vec<WorkspaceRenameChannel>,
    ) -> Result<Self, WorkspaceRenameChannelProjectionError> {
        if channels.iter().any(|row| row.workspace != workspace) {
            return Err(WorkspaceRenameChannelProjectionError::ForeignWorkspace);
        }
        channels.sort_unstable_by(|left, right| {
            left.conversation.as_str().cmp(right.conversation.as_str())
        });
        if channels
            .windows(2)
            .any(|pair| pair[0].conversation == pair[1].conversation)
        {
            return Err(WorkspaceRenameChannelProjectionError::DuplicateConversation);
        }
        Ok(Self {
            workspace,
            channels,
        })
    }

    pub fn bind(
        self,
        requested: &WorkspaceId,
    ) -> Result<BoundWorkspaceRenameChannels, WorkspaceRenameChannelIdentityMismatch> {
        if &self.workspace != requested
            || self.channels.iter().any(|row| &row.workspace != requested)
        {
            return Err(WorkspaceRenameChannelIdentityMismatch);
        }
        Ok(BoundWorkspaceRenameChannels(self))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundWorkspaceRenameChannels(WorkspaceRenameChannels);

impl BoundWorkspaceRenameChannels {
    pub fn channels(&self) -> &[WorkspaceRenameChannel] {
        &self.0.channels
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum WorkspaceRenameChannelProjectionError {
    #[error("workspace rename channel came from another workspace")]
    ForeignWorkspace,
    #[error("workspace rename channel population contains a duplicate conversation")]
    DuplicateConversation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("workspace rename channel population differs from the requested workspace")]
pub struct WorkspaceRenameChannelIdentityMismatch;

pub trait WorkspaceRenameChannelsReadPort {
    type Error: Error + Send + Sync + 'static;

    fn list_open_roster(
        &self,
        workspace: &WorkspaceId,
    ) -> impl Future<Output = Result<WorkspaceRenameChannels, Self::Error>> + Send;
}

pub async fn read_workspace_rename_channels<P: WorkspaceRenameChannelsReadPort>(
    port: &P,
    workspace: WorkspaceId,
) -> Result<BoundWorkspaceRenameChannels, WorkspaceRenameChannelReadError<P::Error>> {
    let selected = port
        .list_open_roster(&workspace)
        .await
        .map_err(WorkspaceRenameChannelReadError::Persistence)?;
    selected.bind(&workspace).map_err(Into::into)
}

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceRenameChannelReadError<E: Error + 'static> {
    #[error("workspace rename channel read failed")]
    Persistence(#[source] E),
    #[error(transparent)]
    Identity(#[from] WorkspaceRenameChannelIdentityMismatch),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceRenameMessageDraft {
    workspace: WorkspaceId,
    conversation: ConversationId,
    old_name: String,
    new_name: String,
    content: String,
}

impl WorkspaceRenameMessageDraft {
    /// Constructs the deterministic system body and its structured rename
    /// metadata inputs. Name canonicality remains owned by #2193; this seam
    /// neither reparses nor refolds workspace names.
    pub fn for_names(
        workspace: WorkspaceId,
        conversation: ConversationId,
        old_name: impl Into<String>,
        new_name: impl Into<String>,
    ) -> Self {
        let old_name = old_name.into();
        let new_name = new_name.into();
        let content = format!(
            "This workspace was renamed from {} to {}. Update any references in your MEMORY.md when you next edit it.",
            inline_code(&old_name),
            inline_code(&new_name)
        );
        Self {
            workspace,
            conversation,
            old_name,
            new_name,
            content,
        }
    }

    pub fn workspace(&self) -> &WorkspaceId {
        &self.workspace
    }
    pub fn conversation(&self) -> &ConversationId {
        &self.conversation
    }
    pub fn old_name(&self) -> &str {
        &self.old_name
    }
    pub fn new_name(&self) -> &str {
        &self.new_name
    }
    pub fn content(&self) -> &str {
        &self.content
    }
    pub const fn subtype(&self) -> &'static str {
        "rename"
    }
    pub const fn scope(&self) -> &'static str {
        "workspace"
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkspaceRenameMessageCreatedAt(i64);

impl WorkspaceRenameMessageCreatedAt {
    const MAX_JAVASCRIPT_INTEGER: i64 = 9_007_199_254_740_991;

    pub fn new(value: i64) -> Result<Self, InvalidWorkspaceRenameMessageCreatedAt> {
        (0..=Self::MAX_JAVASCRIPT_INTEGER)
            .contains(&value)
            .then_some(Self(value))
            .ok_or(InvalidWorkspaceRenameMessageCreatedAt)
    }

    pub const fn get(self) -> i64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("workspace rename message created_at is outside its wire domain")]
pub struct InvalidWorkspaceRenameMessageCreatedAt;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommittedWorkspaceRenameMessage {
    draft: WorkspaceRenameMessageDraft,
    message: MessageId,
    seq: MessageSeq,
    created_at: WorkspaceRenameMessageCreatedAt,
}

impl CommittedWorkspaceRenameMessage {
    pub fn committed(
        draft: WorkspaceRenameMessageDraft,
        message: MessageId,
        seq: MessageSeq,
        created_at: WorkspaceRenameMessageCreatedAt,
    ) -> Self {
        Self {
            draft,
            message,
            seq,
            created_at,
        }
    }

    pub fn bind(
        self,
        requested: &WorkspaceRenameMessageDraft,
    ) -> Result<BoundWorkspaceRenameMessage, WorkspaceRenameMessageIdentityMismatch> {
        if &self.draft != requested {
            return Err(WorkspaceRenameMessageIdentityMismatch);
        }
        Ok(BoundWorkspaceRenameMessage(self))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundWorkspaceRenameMessage(CommittedWorkspaceRenameMessage);

impl BoundWorkspaceRenameMessage {
    pub fn draft(&self) -> &WorkspaceRenameMessageDraft {
        &self.0.draft
    }
    pub fn message(&self) -> &MessageId {
        &self.0.message
    }
    pub const fn seq(&self) -> MessageSeq {
        self.0.seq
    }
    pub const fn created_at(&self) -> WorkspaceRenameMessageCreatedAt {
        self.0.created_at
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("committed workspace rename message differs from its requested draft")]
pub struct WorkspaceRenameMessageIdentityMismatch;

pub trait WorkspaceRenameMessageAuthorPort {
    type Error: Error + Send + Sync + 'static;

    fn author(
        &self,
        draft: WorkspaceRenameMessageDraft,
    ) -> impl Future<Output = Result<CommittedWorkspaceRenameMessage, Self::Error>> + Send;
}

pub async fn author_workspace_rename_message<P: WorkspaceRenameMessageAuthorPort>(
    port: &P,
    draft: WorkspaceRenameMessageDraft,
) -> Result<BoundWorkspaceRenameMessage, WorkspaceRenameMessageAuthorError<P::Error>> {
    let committed = port
        .author(draft.clone())
        .await
        .map_err(WorkspaceRenameMessageAuthorError::Persistence)?;
    committed.bind(&draft).map_err(Into::into)
}

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceRenameMessageAuthorError<E: Error + 'static> {
    #[error("workspace rename message persistence failed")]
    Persistence(#[source] E),
    #[error(transparent)]
    Identity(#[from] WorkspaceRenameMessageIdentityMismatch),
}

/// The agents currently in a conversation, for the rename delivery leg.
///
/// UNGATED on purpose: it lists every agent joined to the conversation, mirroring
/// the TypeScript `conversations.listAgents(channel.id)`. The rename notice is an
/// in-channel system message, so every member's runtime is owed the push — there
/// is no viewer ACL to apply here, and adding one would silently drop agents from
/// the fan-out.
pub trait ConversationAgentsReadPort {
    type Error: Error + Send + Sync + 'static;

    fn agent_ids(
        &self,
        conversation: &ConversationId,
    ) -> impl Future<Output = Result<Vec<AgentId>, Self::Error>> + Send;
}

/// Push a committed rename message onto ONE agent's runtime — the dual of the
/// TypeScript `runtimeHost.deliverMessage(conversation, agent, message.id)`.
///
/// The conversation is NOT a separate parameter: it is derived from the sealed
/// message's own [`draft().conversation()`](WorkspaceRenameMessageDraft::conversation),
/// so a message committed for conversation c1 can only ever be delivered against
/// c1. A caller cannot splice c1's committed message onto c2's runtime, because
/// there is no second coordinate to disagree with the sealed one.
pub trait WorkspaceRenameDeliveryPort {
    type Error: Error + Send + Sync + 'static;

    fn deliver(
        &self,
        agent: &AgentId,
        message: &BoundWorkspaceRenameMessage,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// Publish a committed rename message to its conversation's live subscribers — the
/// dual of the TypeScript `events.publish(channel.id, {kind:'message', payload})`.
/// The adapter owns the wire projection; this port names only the obligation.
///
/// As with [`WorkspaceRenameDeliveryPort`], the target scope is derived from the
/// sealed message's own [`draft().conversation()`](WorkspaceRenameMessageDraft::conversation)
/// — never a caller-supplied coordinate — so the frame's scope and the
/// `conversation_id` inside its body are one and the same sealed identity and can
/// never be spliced apart.
pub trait WorkspaceRenamePublicationPort {
    type Error: Error + Send + Sync + 'static;

    fn publish(
        &self,
        message: &BoundWorkspaceRenameMessage,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// A post-commit rename fan-out leg could not be completed. Opaque and
/// source-preserving like [`crate::identity_access::workspace_metadata_write::WorkspaceMetadataAnnounceFailure`],
/// and for the same reason: the rename is already durable, so a failure here is a
/// LOST effect to be reported (the concrete cause stays available as `source` for
/// the boundary that logs it), never a corrupted fact and never a request refusal.
#[derive(Debug, thiserror::Error)]
#[error("a post-commit workspace rename fan-out leg could not be completed")]
pub struct WorkspaceRenameFanoutFailure {
    #[source]
    source: Box<dyn Error + Send + Sync + 'static>,
}

impl WorkspaceRenameFanoutFailure {
    pub fn from_source<E>(source: E) -> Self
    where
        E: Error + Send + Sync + 'static,
    {
        Self {
            source: Box::new(source),
        }
    }
}

/// Fan a committed workspace rename out to every open channel: author one system
/// message per channel, push it to each of that channel's agents, and publish it
/// to the channel's live subscribers. The dual of the TypeScript
/// `PATCH /api/workspaces/:id` per-channel loop, driven off the sealed
/// [`WorkspaceNameChange`](crate::identity_access::workspace_metadata_write::WorkspaceNameChange)
/// the write hands onward — NOT a second decision about whether a rename happened.
///
/// BEST-EFFORT after commit, with a layered, un-fusable failure contract so each
/// leg is independently observable (a no-op that skipped one would redden a gate):
///
///  - Reading the channel population is a whole-fan-out precondition: with no list
///    there is nothing to iterate, so a read failure aborts.
///  - AUTHOR is each channel's precondition: without a committed message there is
///    no coordinate to deliver or publish, so an author failure skips THAT
///    channel's downstream — but does not abort the other channels.
///  - DELIVERY isolates per agent: one agent's failure never aborts the remaining
///    agents, and the channel's publication is still attempted afterward.
///  - PUBLICATION is always attempted once author succeeded, after delivery.
///
/// The FIRST failing leg's cause is retained (later causes are not carried — they
/// are subsumed into the single aggregate `Err`); the fan-out is PARTIAL, so
/// channels and agents that succeeded stay done. The aggregate is returned as `Err`
/// only after every channel has been fully attempted, so one dead leg cannot
/// silently skip the rest. The caller records it and still returns the committed
/// `200`; nothing here downgrades the durable rename to an error.
#[allow(clippy::too_many_arguments)]
pub async fn fan_out_workspace_rename<Ch, Au, Ag, De, Pu>(
    channels: &Ch,
    author: &Au,
    conversation_agents: &Ag,
    delivery: &De,
    publication: &Pu,
    workspace: WorkspaceId,
    old_name: impl Into<String>,
    new_name: impl Into<String>,
) -> Result<(), WorkspaceRenameFanoutFailure>
where
    Ch: WorkspaceRenameChannelsReadPort,
    Au: WorkspaceRenameMessageAuthorPort,
    Ag: ConversationAgentsReadPort,
    De: WorkspaceRenameDeliveryPort,
    Pu: WorkspaceRenamePublicationPort,
{
    let old_name = old_name.into();
    let new_name = new_name.into();
    let bound = read_workspace_rename_channels(channels, workspace.clone())
        .await
        .map_err(WorkspaceRenameFanoutFailure::from_source)?;

    let mut first_failure: Option<WorkspaceRenameFanoutFailure> = None;
    let mut retain = |failure: WorkspaceRenameFanoutFailure| {
        if first_failure.is_none() {
            first_failure = Some(failure);
        }
    };

    for channel in bound.channels() {
        let conversation = channel.conversation().clone();
        let draft = WorkspaceRenameMessageDraft::for_names(
            workspace.clone(),
            conversation.clone(),
            old_name.clone(),
            new_name.clone(),
        );
        // AUTHOR — the per-channel precondition. On failure this channel has no
        // coordinate, so skip its delivery and publication (never fake them).
        let committed = match author_workspace_rename_message(author, draft).await {
            Ok(committed) => committed,
            Err(cause) => {
                retain(WorkspaceRenameFanoutFailure::from_source(cause));
                continue;
            }
        };
        // DELIVERY — every agent attempted; a per-agent failure is retained but
        // does not abort the rest, and publication still runs afterward. Each push
        // targets the committed message's OWN conversation (the port derives it from
        // the sealed message), so the agents read for `conversation` and the message
        // delivered to them are one and the same channel.
        match conversation_agents.agent_ids(&conversation).await {
            Ok(agents) => {
                for agent in &agents {
                    if let Err(cause) = delivery.deliver(agent, &committed).await {
                        retain(WorkspaceRenameFanoutFailure::from_source(cause));
                    }
                }
            }
            Err(cause) => retain(WorkspaceRenameFanoutFailure::from_source(cause)),
        }
        // PUBLICATION — always attempted once the message is committed. Targets the
        // committed message's own conversation (derived by the port), never a
        // caller-supplied scope.
        if let Err(cause) = publication.publish(&committed).await {
            retain(WorkspaceRenameFanoutFailure::from_source(cause));
        }
    }

    match first_failure {
        Some(failure) => Err(failure),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        convert::Infallible,
        sync::{Arc, Mutex},
    };

    use super::*;

    fn workspace(value: &str) -> WorkspaceId {
        WorkspaceId::parse(value).unwrap()
    }

    fn conversation(value: &str) -> ConversationId {
        ConversationId::parse(value).unwrap()
    }

    #[test]
    fn channel_population_is_workspace_bound_unique_and_projector_ordered() {
        let rows = WorkspaceRenameChannels::selected(
            workspace("w"),
            vec![
                WorkspaceRenameChannel::selected(workspace("w"), conversation("z")),
                WorkspaceRenameChannel::selected(workspace("w"), conversation("a")),
            ],
        )
        .unwrap()
        .bind(&workspace("w"))
        .unwrap();
        assert_eq!(
            rows.channels()
                .iter()
                .map(|row| row.conversation().as_str())
                .collect::<Vec<_>>(),
            ["a", "z"]
        );
        assert!(
            WorkspaceRenameChannels::selected(
                workspace("w"),
                vec![WorkspaceRenameChannel::selected(
                    workspace("other"),
                    conversation("a")
                )]
            )
            .is_err()
        );
        assert!(
            WorkspaceRenameChannels::selected(
                workspace("w"),
                vec![
                    WorkspaceRenameChannel::selected(workspace("w"), conversation("a")),
                    WorkspaceRenameChannel::selected(workspace("w"), conversation("a")),
                ]
            )
            .is_err()
        );
    }

    #[derive(Clone)]
    struct SelectedChannels {
        workspace: &'static str,
    }

    impl WorkspaceRenameChannelsReadPort for SelectedChannels {
        type Error = Infallible;

        async fn list_open_roster(
            &self,
            _: &WorkspaceId,
        ) -> Result<WorkspaceRenameChannels, Self::Error> {
            Ok(WorkspaceRenameChannels::selected(
                workspace(self.workspace),
                vec![WorkspaceRenameChannel::selected(
                    workspace(self.workspace),
                    conversation("c"),
                )],
            )
            .unwrap())
        }
    }

    #[tokio::test]
    async fn read_use_case_binds_the_returned_workspace_before_consumption() {
        assert!(
            read_workspace_rename_channels(
                &SelectedChannels { workspace: "other" },
                workspace("w")
            )
            .await
            .is_err()
        );
        let bound =
            read_workspace_rename_channels(&SelectedChannels { workspace: "w" }, workspace("w"))
                .await
                .unwrap();
        assert_eq!(bound.channels()[0].conversation().as_str(), "c");
    }

    #[derive(Clone)]
    struct EchoPort {
        committed: WorkspaceRenameMessageDraft,
    }

    impl WorkspaceRenameMessageAuthorPort for EchoPort {
        type Error = Infallible;

        async fn author(
            &self,
            _: WorkspaceRenameMessageDraft,
        ) -> Result<CommittedWorkspaceRenameMessage, Self::Error> {
            Ok(CommittedWorkspaceRenameMessage::committed(
                self.committed.clone(),
                MessageId::parse("message").unwrap(),
                MessageSeq::try_from(2).unwrap(),
                WorkspaceRenameMessageCreatedAt::new(3).unwrap(),
            ))
        }
    }

    #[tokio::test]
    async fn deterministic_draft_and_committed_fact_bind_before_consumption() {
        let draft = WorkspaceRenameMessageDraft::for_names(
            workspace("w"),
            conversation("c"),
            "Old`Name",
            "New\nName",
        );
        assert_eq!(draft.subtype(), "rename");
        assert_eq!(draft.scope(), "workspace");
        assert_eq!(
            draft.content(),
            "This workspace was renamed from `` Old`Name `` to `New Name`. Update any references in your MEMORY.md when you next edit it."
        );
        let bound = author_workspace_rename_message(
            &EchoPort {
                committed: draft.clone(),
            },
            draft.clone(),
        )
        .await
        .unwrap();
        assert_eq!(bound.draft(), &draft);
        assert_eq!(bound.message().as_str(), "message");

        let foreign = WorkspaceRenameMessageDraft::for_names(
            workspace("other"),
            conversation("c"),
            "Old`Name",
            "New\nName",
        );
        assert!(
            author_workspace_rename_message(&EchoPort { committed: foreign }, draft)
                .await
                .is_err()
        );
    }

    #[test]
    fn created_at_is_wire_safe() {
        assert!(WorkspaceRenameMessageCreatedAt::new(-1).is_err());
        assert_eq!(WorkspaceRenameMessageCreatedAt::new(0).unwrap().get(), 0);
        assert_eq!(
            WorkspaceRenameMessageCreatedAt::new(9_007_199_254_740_991)
                .unwrap()
                .get(),
            9_007_199_254_740_991
        );
        assert!(WorkspaceRenameMessageCreatedAt::new(9_007_199_254_740_992).is_err());
    }

    #[derive(Debug, thiserror::Error)]
    #[error("stub rename fan-out leg failed")]
    struct StubLegFailure;

    /// Two channels for one workspace, so the per-channel contract (skip THIS
    /// channel's downstream on author failure, keep going for the rest) is
    /// observable. `selected` sorts by conversation id, so iteration is c1 then c2.
    #[derive(Clone)]
    struct FixedChannels;

    impl WorkspaceRenameChannelsReadPort for FixedChannels {
        type Error = Infallible;

        async fn list_open_roster(
            &self,
            _: &WorkspaceId,
        ) -> Result<WorkspaceRenameChannels, Self::Error> {
            Ok(WorkspaceRenameChannels::selected(
                workspace("w"),
                vec![
                    WorkspaceRenameChannel::selected(workspace("w"), conversation("c1")),
                    WorkspaceRenameChannel::selected(workspace("w"), conversation("c2")),
                ],
            )
            .unwrap())
        }
    }

    /// Records the conversation of every SUCCESSFUL author, and can fail one named
    /// conversation so the author-precondition falsifier has a channel to trip on.
    #[derive(Clone)]
    struct RecordingAuthor {
        authored: Arc<Mutex<Vec<String>>>,
        fail_on: Option<&'static str>,
    }

    impl WorkspaceRenameMessageAuthorPort for RecordingAuthor {
        type Error = StubLegFailure;

        async fn author(
            &self,
            draft: WorkspaceRenameMessageDraft,
        ) -> Result<CommittedWorkspaceRenameMessage, Self::Error> {
            if self.fail_on == Some(draft.conversation().as_str()) {
                return Err(StubLegFailure);
            }
            self.authored
                .lock()
                .unwrap()
                .push(draft.conversation().as_str().to_owned());
            Ok(CommittedWorkspaceRenameMessage::committed(
                draft,
                MessageId::parse("message").unwrap(),
                MessageSeq::try_from(2).unwrap(),
                WorkspaceRenameMessageCreatedAt::new(3).unwrap(),
            ))
        }
    }

    #[derive(Clone)]
    struct TwoAgents;

    impl ConversationAgentsReadPort for TwoAgents {
        type Error = StubLegFailure;

        async fn agent_ids(&self, _: &ConversationId) -> Result<Vec<AgentId>, Self::Error> {
            Ok(vec![
                AgentId::parse("agent-a").unwrap(),
                AgentId::parse("agent-b").unwrap(),
            ])
        }
    }

    /// Records EVERY delivery attempt (before deciding whether it fails), so the
    /// per-agent isolation falsifier can prove the remaining agents were still
    /// attempted after one failed.
    #[derive(Clone)]
    struct RecordingDelivery {
        attempted: Arc<Mutex<Vec<(String, String)>>>,
        fail_on_agent: Option<&'static str>,
    }

    impl WorkspaceRenameDeliveryPort for RecordingDelivery {
        type Error = StubLegFailure;

        async fn deliver(
            &self,
            agent: &AgentId,
            message: &BoundWorkspaceRenameMessage,
        ) -> Result<(), Self::Error> {
            // Record the target from the SEALED message, not a caller coordinate:
            // this is the only conversation the port can act on.
            let conversation = message.draft().conversation();
            self.attempted
                .lock()
                .unwrap()
                .push((conversation.as_str().to_owned(), agent.as_str().to_owned()));
            if self.fail_on_agent == Some(agent.as_str()) {
                return Err(StubLegFailure);
            }
            Ok(())
        }
    }

    /// Records EVERY publication attempt (before deciding whether it fails), so the
    /// falsifiers can prove publication ran after a delivery failure, and that a
    /// channel whose author failed was never published.
    #[derive(Clone)]
    struct RecordingPublication {
        attempted: Arc<Mutex<Vec<String>>>,
        fail_on: Option<&'static str>,
    }

    impl WorkspaceRenamePublicationPort for RecordingPublication {
        type Error = StubLegFailure;

        async fn publish(&self, message: &BoundWorkspaceRenameMessage) -> Result<(), Self::Error> {
            // Target derived from the sealed message; there is no caller scope to
            // splice against.
            let conversation = message.draft().conversation();
            self.attempted
                .lock()
                .unwrap()
                .push(conversation.as_str().to_owned());
            if self.fail_on == Some(conversation.as_str()) {
                return Err(StubLegFailure);
            }
            Ok(())
        }
    }

    struct FanoutProbe {
        author: RecordingAuthor,
        delivery: RecordingDelivery,
        publication: RecordingPublication,
    }

    impl FanoutProbe {
        fn new(
            fail_author_on: Option<&'static str>,
            fail_delivery_agent: Option<&'static str>,
            fail_publication_on: Option<&'static str>,
        ) -> Self {
            Self {
                author: RecordingAuthor {
                    authored: Arc::new(Mutex::new(Vec::new())),
                    fail_on: fail_author_on,
                },
                delivery: RecordingDelivery {
                    attempted: Arc::new(Mutex::new(Vec::new())),
                    fail_on_agent: fail_delivery_agent,
                },
                publication: RecordingPublication {
                    attempted: Arc::new(Mutex::new(Vec::new())),
                    fail_on: fail_publication_on,
                },
            }
        }

        async fn run(&self) -> Result<(), WorkspaceRenameFanoutFailure> {
            fan_out_workspace_rename(
                &FixedChannels,
                &self.author,
                &TwoAgents,
                &self.delivery,
                &self.publication,
                workspace("w"),
                "Old",
                "New",
            )
            .await
        }

        fn authored(&self) -> Vec<String> {
            self.author.authored.lock().unwrap().clone()
        }
        fn delivered(&self) -> Vec<(String, String)> {
            self.delivery.attempted.lock().unwrap().clone()
        }
        fn published(&self) -> Vec<String> {
            self.publication.attempted.lock().unwrap().clone()
        }
    }

    /// SUCCESS ARM: every leg fires for every channel. This is the
    /// baseline the three falsifiers each perturb by exactly one leg — proving no
    /// leg is a no-op that a single stub could fake away.
    #[tokio::test]
    async fn success_authors_delivers_and_publishes_every_channel() {
        let probe = FanoutProbe::new(None, None, None);
        probe.run().await.unwrap();
        assert_eq!(probe.authored(), ["c1", "c2"]);
        assert_eq!(
            probe.delivered(),
            [
                ("c1".to_owned(), "agent-a".to_owned()),
                ("c1".to_owned(), "agent-b".to_owned()),
                ("c2".to_owned(), "agent-a".to_owned()),
                ("c2".to_owned(), "agent-b".to_owned()),
            ]
        );
        assert_eq!(probe.published(), ["c1", "c2"]);
    }

    /// FALSIFIER ①: an author failure is a per-channel precondition. c1's author
    /// fails, so NOTHING downstream fires for c1 — no fake delivery, no fake
    /// publication — while c2 proceeds untouched. Aggregate is `Err`.
    #[tokio::test]
    async fn author_failure_skips_only_that_channels_delivery_and_publication() {
        let probe = FanoutProbe::new(Some("c1"), None, None);
        let outcome = probe.run().await;
        assert!(outcome.is_err());
        // c1 never authored, so it must appear in neither downstream leg.
        assert_eq!(probe.authored(), ["c2"]);
        assert_eq!(
            probe.delivered(),
            [
                ("c2".to_owned(), "agent-a".to_owned()),
                ("c2".to_owned(), "agent-b".to_owned()),
            ]
        );
        assert_eq!(probe.published(), ["c2"]);
    }

    /// FALSIFIER ②: one agent's delivery failure never aborts the rest. agent-a
    /// fails in both channels, yet agent-b is still attempted in each, and each
    /// channel's publication is still attempted. Aggregate is `Err`.
    #[tokio::test]
    async fn one_delivery_failure_isolates_to_its_agent_and_publication_still_runs() {
        let probe = FanoutProbe::new(None, Some("agent-a"), None);
        let outcome = probe.run().await;
        assert!(outcome.is_err());
        // Every agent in every channel was attempted despite agent-a failing.
        assert_eq!(
            probe.delivered(),
            [
                ("c1".to_owned(), "agent-a".to_owned()),
                ("c1".to_owned(), "agent-b".to_owned()),
                ("c2".to_owned(), "agent-a".to_owned()),
                ("c2".to_owned(), "agent-b".to_owned()),
            ]
        );
        // Publication ran for both channels: a delivery failure does not abort it.
        assert_eq!(probe.published(), ["c1", "c2"]);
    }

    /// A failed publication does not prevent either channel's other effects.
    #[tokio::test]
    async fn publication_failure_preserves_all_author_and_delivery_attempts() {
        let probe = FanoutProbe::new(None, None, Some("c1"));
        let outcome = probe.run().await;
        assert!(outcome.is_err());
        assert_eq!(probe.authored(), ["c1", "c2"]);
        assert_eq!(
            probe.delivered(),
            [
                ("c1".to_owned(), "agent-a".to_owned()),
                ("c1".to_owned(), "agent-b".to_owned()),
                ("c2".to_owned(), "agent-a".to_owned()),
                ("c2".to_owned(), "agent-b".to_owned()),
            ]
        );
        // Both publications were attempted; only c1's failed.
        assert_eq!(probe.published(), ["c1", "c2"]);
    }
}
