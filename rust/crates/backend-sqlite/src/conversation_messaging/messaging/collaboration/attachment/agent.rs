//! SQLite internal-agent attachment operation adapters.
//!
//! Every operation selects its own authority and result in one snapshot. Writes
//! reserve SQLite's writer before the first authority read, then reconstruct the
//! capability fact from persisted participation, placement, and grant rows. The
//! request command is never reflected back as an authority witness.

use silverhand_backend_core::{
    conversation_messaging::messaging::{
        AttachmentId, PrincipalIdentity,
        collaboration::{
            attachment::{
                AgentAttachmentContentUpdateCommand, AgentAttachmentContentUpdateRefusal,
                AgentAttachmentContentUpdateUnitOfWork, AgentAttachmentContentUpdatedFact,
                AgentAttachmentCreateCommand, AgentAttachmentCreateRefusal,
                AgentAttachmentCreateUnitOfWork, AgentAttachmentCreatedFact,
                AgentAttachmentDownloadFact, AgentAttachmentDownloadReadPort,
                AgentAttachmentLocateFact, AgentAttachmentLocateReadPort,
                AgentAttachmentReadRefusal, AgentAttachmentRuntime, AgentAttachmentUpdateAdmission,
                AgentAttachmentUpdateAdmissionPort, AgentAttachmentUpdateAdmissionRefusal,
                AgentAttachmentUploadAdmission, AgentAttachmentUploadAdmissionPort,
                AgentAttachmentUploadRefusal, AttachmentRecord,
            },
            channel_lifecycle::ChannelLifecyclePlacement,
        },
    },
    foundation::identity::{AgentId, ConversationId, WorkspaceId},
    foundation::persistence::PersistenceFailure,
    foundation::wire_number::JsSafeInteger,
    identity_access::access::Capability,
};
use sqlx::{Row, Sqlite, Transaction};
use thiserror::Error;

use crate::conversation_messaging::messaging::{
    MessagingStore,
    collaboration::{
        attachment::{
            read::{AttachmentReadStoreError, load_attachment},
            write::{insert_attachment_row, swap_attachment_content_row},
        },
        channel_lifecycle::{ChannelLifecycleStoreError, load_channel_row},
    },
};

#[derive(Clone)]
pub struct SqliteAgentAttachment {
    pool: crate::foundation::database::SqliteDatabase,
}

impl<E> MessagingStore<E> {
    pub fn agent_attachments(&self) -> SqliteAgentAttachment {
        SqliteAgentAttachment {
            pool: self.pool.clone(),
        }
    }
}

#[derive(Debug, Error)]
pub enum AgentAttachmentStoreError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("attachment row decode failed")]
    Read(#[from] AttachmentReadStoreError),
    #[error("attachment conversation authority decode failed")]
    Channel(#[from] ChannelLifecycleStoreError),
    #[error("stored column {field} does not contain a usable identity")]
    Identity { field: &'static str },
    #[error("stored workspace placement is malformed")]
    WorkspacePlacement,
    #[error("stored agent attachment authority coordinates disagree")]
    Coordinate,
    #[error("attachment row points at a missing workspace")]
    MissingWorkspace,
    #[error("direct-pair liveness row is malformed")]
    DirectPairLiveness,
    #[error("replacement bytes were validated against another locked content type")]
    LockedContentTypeChanged,
    #[error("replacement blob storage differs from the committed attachment storage")]
    ReplacementStorageChanged,
    #[error("replacement blob key is not fresh")]
    ReplacementKeyReused,
    #[error("attachment content revision reached the exact JSON integer ceiling")]
    RevisionExhausted,
    #[error("the selected attachment row changed inside the writer-reserved unit")]
    PointerChanged,
    #[error("an operation fact could not seal the selected persistence rows")]
    Fact,
}

impl From<AgentAttachmentStoreError> for PersistenceFailure {
    fn from(value: AgentAttachmentStoreError) -> Self {
        Self::from_source(value)
    }
}

impl AgentAttachmentUploadAdmissionPort for SqliteAgentAttachment {
    type Error = PersistenceFailure;

    async fn admit_upload(
        &self,
        agent: &AgentId,
        conversation: &ConversationId,
    ) -> Result<Result<AgentAttachmentUploadAdmission, AgentAttachmentUploadRefusal>, Self::Error>
    {
        self.admit_upload_inner(agent, conversation)
            .await
            .map_err(PersistenceFailure::from)
    }
}

impl AgentAttachmentCreateUnitOfWork for SqliteAgentAttachment {
    type Error = PersistenceFailure;

    async fn create_for_agent(
        &self,
        command: AgentAttachmentCreateCommand,
    ) -> Result<Result<AgentAttachmentCreatedFact, AgentAttachmentCreateRefusal>, Self::Error> {
        self.create_inner(command)
            .await
            .map_err(PersistenceFailure::from)
    }
}

impl AgentAttachmentUpdateAdmissionPort for SqliteAgentAttachment {
    type Error = PersistenceFailure;

    async fn admit_update(
        &self,
        runtime: &AgentAttachmentRuntime,
        attachment: &AttachmentId,
    ) -> Result<
        Result<AgentAttachmentUpdateAdmission, AgentAttachmentUpdateAdmissionRefusal>,
        Self::Error,
    > {
        self.admit_update_inner(runtime, attachment)
            .await
            .map_err(PersistenceFailure::from)
    }
}

impl AgentAttachmentContentUpdateUnitOfWork for SqliteAgentAttachment {
    type Error = PersistenceFailure;

    async fn update_content_for_agent(
        &self,
        command: AgentAttachmentContentUpdateCommand,
    ) -> Result<
        Result<AgentAttachmentContentUpdatedFact, AgentAttachmentContentUpdateRefusal>,
        Self::Error,
    > {
        self.update_inner(command)
            .await
            .map_err(PersistenceFailure::from)
    }
}

impl AgentAttachmentLocateReadPort for SqliteAgentAttachment {
    type Error = PersistenceFailure;

    async fn locate_for_agent(
        &self,
        agent: &AgentId,
        attachment: &AttachmentId,
    ) -> Result<Result<AgentAttachmentLocateFact, AgentAttachmentReadRefusal>, Self::Error> {
        self.locate_inner(agent, attachment)
            .await
            .map_err(PersistenceFailure::from)
    }
}

impl AgentAttachmentDownloadReadPort for SqliteAgentAttachment {
    type Error = PersistenceFailure;

    async fn download_for_agent(
        &self,
        agent: &AgentId,
        attachment: &AttachmentId,
    ) -> Result<Result<AgentAttachmentDownloadFact, AgentAttachmentReadRefusal>, Self::Error> {
        self.download_inner(agent, attachment)
            .await
            .map_err(PersistenceFailure::from)
    }
}

impl SqliteAgentAttachment {
    async fn begin_read(&self) -> Result<Transaction<'_, Sqlite>, AgentAttachmentStoreError> {
        Ok(self.pool.begin_with("BEGIN DEFERRED").await?)
    }

    async fn begin_write(&self) -> Result<Transaction<'_, Sqlite>, AgentAttachmentStoreError> {
        Ok(self.pool.begin_with("BEGIN IMMEDIATE").await?)
    }

    async fn admit_upload_inner(
        &self,
        agent: &AgentId,
        conversation: &ConversationId,
    ) -> Result<
        Result<AgentAttachmentUploadAdmission, AgentAttachmentUploadRefusal>,
        AgentAttachmentStoreError,
    > {
        let mut tx = self.begin_read().await?;
        let result = match select_authority(
            &mut tx,
            agent,
            conversation,
            Capability::UploadAttachments,
            true,
        )
        .await?
        {
            Ok(authority) => Ok(authority.upload_fact()?),
            Err(AuthorityRefusal::NotFound) => Err(AgentAttachmentUploadRefusal::NotFound),
            Err(AuthorityRefusal::CapabilityDenied) => {
                Err(AgentAttachmentUploadRefusal::CapabilityDenied)
            }
        };
        tx.commit().await?;
        Ok(result)
    }

    async fn create_inner(
        &self,
        command: AgentAttachmentCreateCommand,
    ) -> Result<
        Result<AgentAttachmentCreatedFact, AgentAttachmentCreateRefusal>,
        AgentAttachmentStoreError,
    > {
        let mut tx = self.begin_write().await?;
        let authority = match select_authority(
            &mut tx,
            command.agent(),
            command.conversation(),
            Capability::UploadAttachments,
            true,
        )
        .await?
        {
            Ok(authority) => authority,
            Err(AuthorityRefusal::NotFound) => {
                tx.commit().await?;
                return Ok(Err(AgentAttachmentCreateRefusal::NotFound));
            }
            Err(AuthorityRefusal::CapabilityDenied) => {
                tx.commit().await?;
                return Ok(Err(AgentAttachmentCreateRefusal::CapabilityDenied));
            }
        };
        if insert_attachment_row(&mut tx, command.record()).await? != 1 {
            return Err(AgentAttachmentStoreError::PointerChanged);
        }
        let committed = load_attachment(&mut tx, command.record().id())
            .await?
            .ok_or(AgentAttachmentStoreError::PointerChanged)?;
        let fact =
            AgentAttachmentCreatedFact::committed(&command, authority.upload_fact()?, committed)
                .map_err(|_| AgentAttachmentStoreError::Fact)?;
        tx.commit().await?;
        Ok(Ok(fact))
    }

    async fn admit_update_inner(
        &self,
        runtime: &AgentAttachmentRuntime,
        requested: &AttachmentId,
    ) -> Result<
        Result<AgentAttachmentUpdateAdmission, AgentAttachmentUpdateAdmissionRefusal>,
        AgentAttachmentStoreError,
    > {
        let mut tx = self.begin_read().await?;
        let Some(record) = load_attachment(&mut tx, requested).await? else {
            tx.commit().await?;
            return Ok(Err(AgentAttachmentUpdateAdmissionRefusal::NotFound));
        };
        if runtime
            .source()
            .is_some_and(|source| source != record.channel())
        {
            tx.commit().await?;
            return Ok(Err(AgentAttachmentUpdateAdmissionRefusal::NotFound));
        }
        let authority = match select_authority(
            &mut tx,
            runtime.agent(),
            record.channel(),
            Capability::UploadAttachments,
            true,
        )
        .await?
        {
            Ok(authority) => authority,
            Err(AuthorityRefusal::NotFound) => {
                tx.commit().await?;
                return Ok(Err(AgentAttachmentUpdateAdmissionRefusal::NotFound));
            }
            Err(AuthorityRefusal::CapabilityDenied) => {
                tx.commit().await?;
                return Ok(Err(AgentAttachmentUpdateAdmissionRefusal::CapabilityDenied));
            }
        };
        let result =
            AgentAttachmentUpdateAdmission::selected(runtime, authority.upload_fact()?, record)
                .map_err(|_| AgentAttachmentStoreError::Fact)?;
        tx.commit().await?;
        Ok(result)
    }

    async fn update_inner(
        &self,
        command: AgentAttachmentContentUpdateCommand,
    ) -> Result<
        Result<AgentAttachmentContentUpdatedFact, AgentAttachmentContentUpdateRefusal>,
        AgentAttachmentStoreError,
    > {
        let mut tx = self.begin_write().await?;
        let Some(before) = load_attachment(&mut tx, command.requested()).await? else {
            tx.commit().await?;
            return Ok(Err(AgentAttachmentContentUpdateRefusal::NotFound));
        };
        if command
            .runtime()
            .source()
            .is_some_and(|source| source != before.channel())
        {
            tx.commit().await?;
            return Ok(Err(AgentAttachmentContentUpdateRefusal::NotFound));
        }
        let authority = match select_authority(
            &mut tx,
            command.agent(),
            before.channel(),
            Capability::UploadAttachments,
            true,
        )
        .await?
        {
            Ok(authority) => authority,
            Err(AuthorityRefusal::NotFound) => {
                tx.commit().await?;
                return Ok(Err(AgentAttachmentContentUpdateRefusal::NotFound));
            }
            Err(AuthorityRefusal::CapabilityDenied) => {
                tx.commit().await?;
                return Ok(Err(AgentAttachmentContentUpdateRefusal::CapabilityDenied));
            }
        };
        let selected = match AgentAttachmentUpdateAdmission::selected(
            command.runtime(),
            authority.upload_fact()?,
            before.clone(),
        )
        .map_err(|_| AgentAttachmentStoreError::Fact)?
        {
            Ok(selected) => selected,
            Err(AgentAttachmentUpdateAdmissionRefusal::NotFound) => {
                tx.commit().await?;
                return Ok(Err(AgentAttachmentContentUpdateRefusal::NotFound));
            }
            Err(AgentAttachmentUpdateAdmissionRefusal::CapabilityDenied) => {
                tx.commit().await?;
                return Ok(Err(AgentAttachmentContentUpdateRefusal::CapabilityDenied));
            }
            Err(AgentAttachmentUpdateAdmissionRefusal::NotCreator) => {
                tx.commit().await?;
                return Ok(Err(AgentAttachmentContentUpdateRefusal::NotCreator));
            }
        };
        if before.content_type() != command.expected_content_type() {
            return Err(AgentAttachmentStoreError::LockedContentTypeChanged);
        }
        if before.blob().storage() != command.replacement_blob().storage() {
            return Err(AgentAttachmentStoreError::ReplacementStorageChanged);
        }
        if before.blob().key() == command.replacement_blob().key() {
            return Err(AgentAttachmentStoreError::ReplacementKeyReused);
        }
        let next_revision = before
            .content_revision()
            .get()
            .checked_add(1)
            .and_then(|value| JsSafeInteger::new(value).ok())
            .ok_or(AgentAttachmentStoreError::RevisionExhausted)?;
        if swap_attachment_content_row(
            &mut tx,
            &before,
            command.replacement_blob(),
            command.replacement_size(),
            next_revision,
        )
        .await?
            != 1
        {
            return Err(AgentAttachmentStoreError::PointerChanged);
        }
        let after = load_attachment(&mut tx, command.requested())
            .await?
            .ok_or(AgentAttachmentStoreError::PointerChanged)?;
        let fact = AgentAttachmentContentUpdatedFact::committed(&command, selected, before, after)
            .map_err(|_| AgentAttachmentStoreError::Fact)?;
        tx.commit().await?;
        Ok(Ok(fact))
    }

    async fn locate_inner(
        &self,
        agent: &AgentId,
        requested: &AttachmentId,
    ) -> Result<
        Result<AgentAttachmentLocateFact, AgentAttachmentReadRefusal>,
        AgentAttachmentStoreError,
    > {
        let mut tx = self.begin_read().await?;
        let Some(record) = load_attachment(&mut tx, requested).await? else {
            tx.commit().await?;
            return Ok(Err(AgentAttachmentReadRefusal::NotFound));
        };
        let authority = match select_authority(
            &mut tx,
            agent,
            record.channel(),
            Capability::ReadAttachments,
            false,
        )
        .await?
        {
            Ok(authority) => authority,
            Err(AuthorityRefusal::NotFound) => {
                tx.commit().await?;
                return Ok(Err(AgentAttachmentReadRefusal::NotFound));
            }
            Err(AuthorityRefusal::CapabilityDenied) => {
                tx.commit().await?;
                return Ok(Err(AgentAttachmentReadRefusal::CapabilityDenied));
            }
        };
        let fact = authority.locate_fact(record)?;
        tx.commit().await?;
        Ok(Ok(fact))
    }

    async fn download_inner(
        &self,
        agent: &AgentId,
        requested: &AttachmentId,
    ) -> Result<
        Result<AgentAttachmentDownloadFact, AgentAttachmentReadRefusal>,
        AgentAttachmentStoreError,
    > {
        let mut tx = self.begin_read().await?;
        let Some(record) = load_attachment(&mut tx, requested).await? else {
            tx.commit().await?;
            return Ok(Err(AgentAttachmentReadRefusal::NotFound));
        };
        let authority = match select_authority(
            &mut tx,
            agent,
            record.channel(),
            Capability::ReadAttachments,
            false,
        )
        .await?
        {
            Ok(authority) => authority,
            Err(AuthorityRefusal::NotFound) => {
                tx.commit().await?;
                return Ok(Err(AgentAttachmentReadRefusal::NotFound));
            }
            Err(AuthorityRefusal::CapabilityDenied) => {
                tx.commit().await?;
                return Ok(Err(AgentAttachmentReadRefusal::CapabilityDenied));
            }
        };
        let fact = authority.download_fact(record)?;
        tx.commit().await?;
        Ok(Ok(fact))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AuthorityRefusal {
    NotFound,
    CapabilityDenied,
}

#[derive(Clone, Debug)]
enum SelectedAuthority {
    Workspace {
        conversation: ConversationId,
        participant: AgentId,
        workspace_member: AgentId,
        capability_holder: AgentId,
    },
    AgentHome {
        conversation: ConversationId,
        participant: AgentId,
        owner: AgentId,
    },
    DirectPair {
        conversation: ConversationId,
        endpoint: AgentId,
    },
}

impl SelectedAuthority {
    fn upload_fact(&self) -> Result<AgentAttachmentUploadAdmission, AgentAttachmentStoreError> {
        match self {
            Self::Workspace {
                conversation,
                participant,
                workspace_member,
                capability_holder,
            } => AgentAttachmentUploadAdmission::workspace(
                conversation.clone(),
                participant.clone(),
                workspace_member.clone(),
                capability_holder.clone(),
            ),
            Self::AgentHome {
                conversation,
                participant,
                owner,
            } => AgentAttachmentUploadAdmission::agent_home(
                conversation.clone(),
                participant.clone(),
                owner.clone(),
            ),
            Self::DirectPair {
                conversation,
                endpoint,
            } => Ok(AgentAttachmentUploadAdmission::direct_pair(
                conversation.clone(),
                endpoint.clone(),
            )),
        }
        .map_err(|_| AgentAttachmentStoreError::Fact)
    }

    fn locate_fact(
        self,
        record: AttachmentRecord,
    ) -> Result<AgentAttachmentLocateFact, AgentAttachmentStoreError> {
        match self {
            Self::Workspace {
                participant,
                workspace_member,
                capability_holder,
                ..
            } => AgentAttachmentLocateFact::workspace(
                record,
                participant,
                workspace_member,
                capability_holder,
            ),
            Self::AgentHome {
                participant, owner, ..
            } => AgentAttachmentLocateFact::agent_home(record, participant, owner),
            Self::DirectPair { endpoint, .. } => {
                AgentAttachmentLocateFact::direct_pair(record, endpoint)
            }
        }
        .map_err(|_| AgentAttachmentStoreError::Fact)
    }

    fn download_fact(
        self,
        record: AttachmentRecord,
    ) -> Result<AgentAttachmentDownloadFact, AgentAttachmentStoreError> {
        match self {
            Self::Workspace {
                participant,
                workspace_member,
                capability_holder,
                ..
            } => AgentAttachmentDownloadFact::workspace(
                record,
                participant,
                workspace_member,
                capability_holder,
            ),
            Self::AgentHome {
                participant, owner, ..
            } => AgentAttachmentDownloadFact::agent_home(record, participant, owner),
            Self::DirectPair { endpoint, .. } => {
                AgentAttachmentDownloadFact::direct_pair(record, endpoint)
            }
        }
        .map_err(|_| AgentAttachmentStoreError::Fact)
    }
}

async fn select_authority(
    tx: &mut Transaction<'_, Sqlite>,
    agent: &AgentId,
    requested: &ConversationId,
    capability: Capability,
    require_write_liveness: bool,
) -> Result<Result<SelectedAuthority, AuthorityRefusal>, AgentAttachmentStoreError> {
    let Some(channel) = load_channel_row(tx, requested).await? else {
        return Ok(Err(AuthorityRefusal::NotFound));
    };
    match channel.placement() {
        ChannelLifecyclePlacement::Workspace(workspace) => {
            let Some(participant) = select_conversation_agent(tx, agent, requested).await? else {
                return Ok(Err(AuthorityRefusal::NotFound));
            };
            let placement = select_workspace(tx, workspace.workspace()).await?;
            match placement {
                WorkspacePlacement::AgentHome(owner) => {
                    if &owner != agent {
                        return Ok(Err(AuthorityRefusal::CapabilityDenied));
                    }
                    Ok(Ok(SelectedAuthority::AgentHome {
                        conversation: channel.conversation().clone(),
                        participant,
                        owner,
                    }))
                }
                WorkspacePlacement::Regular => {
                    let Some(workspace_member) =
                        select_workspace_member(tx, agent, workspace.workspace()).await?
                    else {
                        return Ok(Err(AuthorityRefusal::CapabilityDenied));
                    };
                    let Some(capability_holder) =
                        select_capability_holder(tx, agent, workspace.workspace(), capability)
                            .await?
                    else {
                        return Ok(Err(AuthorityRefusal::CapabilityDenied));
                    };
                    Ok(Ok(SelectedAuthority::Workspace {
                        conversation: channel.conversation().clone(),
                        participant,
                        workspace_member,
                        capability_holder,
                    }))
                }
            }
        }
        ChannelLifecyclePlacement::DirectPair(participants) => {
            let endpoint = participants
                .both()
                .iter()
                .find_map(|identity| match identity {
                    PrincipalIdentity::Agent(selected) if selected == agent => {
                        Some(selected.clone())
                    }
                    _ => None,
                });
            let Some(endpoint) = endpoint else {
                return Ok(Err(AuthorityRefusal::NotFound));
            };
            if require_write_liveness && !direct_pair_is_live(tx, requested).await? {
                return Ok(Err(AuthorityRefusal::NotFound));
            }
            Ok(Ok(SelectedAuthority::DirectPair {
                conversation: channel.conversation().clone(),
                endpoint,
            }))
        }
    }
}

async fn select_conversation_agent(
    tx: &mut Transaction<'_, Sqlite>,
    agent: &AgentId,
    conversation: &ConversationId,
) -> Result<Option<AgentId>, AgentAttachmentStoreError> {
    let row = sqlx::query(
        "SELECT conversation_id,agent_id FROM conversation_agents \
         WHERE conversation_id=? AND agent_id=?",
    )
    .bind(conversation.as_str())
    .bind(agent.as_str())
    .fetch_optional(&mut **tx)
    .await?;
    row.map(|row| {
        let source = parse_conversation(
            row.try_get("conversation_id")?,
            "conversation_agents.conversation_id",
        )?;
        let selected = parse_agent(row.try_get("agent_id")?, "conversation_agents.agent_id")?;
        if &source != conversation || &selected != agent {
            return Err(AgentAttachmentStoreError::Coordinate);
        }
        Ok(selected)
    })
    .transpose()
}

enum WorkspacePlacement {
    Regular,
    AgentHome(AgentId),
}

async fn select_workspace(
    tx: &mut Transaction<'_, Sqlite>,
    workspace: &WorkspaceId,
) -> Result<WorkspacePlacement, AgentAttachmentStoreError> {
    let row = sqlx::query("SELECT id,name_key,owner_agent_id FROM workspaces WHERE id=?")
        .bind(workspace.as_str())
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(AgentAttachmentStoreError::MissingWorkspace)?;
    let selected = WorkspaceId::parse(row.try_get::<String, _>("id")?).map_err(|_| {
        AgentAttachmentStoreError::Identity {
            field: "workspaces.id",
        }
    })?;
    if &selected != workspace {
        return Err(AgentAttachmentStoreError::Coordinate);
    }
    let name_key: Option<String> = row.try_get("name_key")?;
    let owner = row
        .try_get::<Option<String>, _>("owner_agent_id")?
        .map(|raw| parse_agent(raw, "workspaces.owner_agent_id"))
        .transpose()?;
    match (name_key, owner) {
        (Some(key), None) if !key.is_empty() => Ok(WorkspacePlacement::Regular),
        (None, Some(owner)) => Ok(WorkspacePlacement::AgentHome(owner)),
        _ => Err(AgentAttachmentStoreError::WorkspacePlacement),
    }
}

async fn select_workspace_member(
    tx: &mut Transaction<'_, Sqlite>,
    agent: &AgentId,
    workspace: &WorkspaceId,
) -> Result<Option<AgentId>, AgentAttachmentStoreError> {
    let row = sqlx::query(
        "SELECT workspace_id,agent_id FROM workspace_members WHERE workspace_id=? AND agent_id=?",
    )
    .bind(workspace.as_str())
    .bind(agent.as_str())
    .fetch_optional(&mut **tx)
    .await?;
    row.map(|row| {
        let selected_workspace = WorkspaceId::parse(row.try_get::<String, _>("workspace_id")?)
            .map_err(|_| AgentAttachmentStoreError::Identity {
                field: "workspace_members.workspace_id",
            })?;
        let selected = parse_agent(row.try_get("agent_id")?, "workspace_members.agent_id")?;
        if &selected_workspace != workspace || &selected != agent {
            return Err(AgentAttachmentStoreError::Coordinate);
        }
        Ok(selected)
    })
    .transpose()
}

async fn select_capability_holder(
    tx: &mut Transaction<'_, Sqlite>,
    agent: &AgentId,
    workspace: &WorkspaceId,
    capability: Capability,
) -> Result<Option<AgentId>, AgentAttachmentStoreError> {
    let row = sqlx::query(
        "SELECT workspace_id,agent_id,capability FROM workspace_agent_capabilities \
         WHERE workspace_id=? AND agent_id=? AND capability=?",
    )
    .bind(workspace.as_str())
    .bind(agent.as_str())
    .bind(capability.as_str())
    .fetch_optional(&mut **tx)
    .await?;
    row.map(|row| {
        let selected_workspace = WorkspaceId::parse(row.try_get::<String, _>("workspace_id")?)
            .map_err(|_| AgentAttachmentStoreError::Identity {
                field: "workspace_agent_capabilities.workspace_id",
            })?;
        let selected = parse_agent(
            row.try_get("agent_id")?,
            "workspace_agent_capabilities.agent_id",
        )?;
        let selected_capability: String = row.try_get("capability")?;
        if &selected_workspace != workspace
            || &selected != agent
            || selected_capability != capability.as_str()
        {
            return Err(AgentAttachmentStoreError::Coordinate);
        }
        Ok(selected)
    })
    .transpose()
}

async fn direct_pair_is_live(
    tx: &mut Transaction<'_, Sqlite>,
    requested: &ConversationId,
) -> Result<bool, AgentAttachmentStoreError> {
    let row = sqlx::query(
        r#"SELECT id,placement_kind,endpoint_a_deleted_at,endpoint_b_deleted_at,
        CASE endpoint_a_kind
          WHEN 'human' THEN EXISTS(SELECT 1 FROM users WHERE id=endpoint_a_id)
          WHEN 'agent' THEN EXISTS(SELECT 1 FROM agents WHERE id=endpoint_a_id)
          ELSE -1 END AS endpoint_a_exists,
        CASE endpoint_b_kind
          WHEN 'human' THEN EXISTS(SELECT 1 FROM users WHERE id=endpoint_b_id)
          WHEN 'agent' THEN EXISTS(SELECT 1 FROM agents WHERE id=endpoint_b_id)
          ELSE -1 END AS endpoint_b_exists
        FROM conversations WHERE id=?"#,
    )
    .bind(requested.as_str())
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(AgentAttachmentStoreError::DirectPairLiveness)?;
    let source = parse_conversation(row.try_get("id")?, "conversations.id")?;
    if &source != requested || row.try_get::<String, _>("placement_kind")? != "direct_pair" {
        return Err(AgentAttachmentStoreError::DirectPairLiveness);
    }
    let a_exists: i64 = row.try_get("endpoint_a_exists")?;
    let b_exists: i64 = row.try_get("endpoint_b_exists")?;
    if ![0, 1].contains(&a_exists) || ![0, 1].contains(&b_exists) {
        return Err(AgentAttachmentStoreError::DirectPairLiveness);
    }
    Ok(row
        .try_get::<Option<i64>, _>("endpoint_a_deleted_at")?
        .is_none()
        && row
            .try_get::<Option<i64>, _>("endpoint_b_deleted_at")?
            .is_none()
        && a_exists == 1
        && b_exists == 1)
}

fn parse_agent(raw: String, field: &'static str) -> Result<AgentId, AgentAttachmentStoreError> {
    AgentId::parse(raw).map_err(|_| AgentAttachmentStoreError::Identity { field })
}

fn parse_conversation(
    raw: String,
    field: &'static str,
) -> Result<ConversationId, AgentAttachmentStoreError> {
    ConversationId::parse(raw).map_err(|_| AgentAttachmentStoreError::Identity { field })
}
