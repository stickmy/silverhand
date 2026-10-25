//! One-snapshot SQLite adapter for channel lifecycle reads.

mod mutation;
pub use mutation::{ChannelLifecycleMutationStoreError, SqliteChannelLifecycleMutation};

use silverhand_backend_core::{
    conversation_messaging::messaging::{
        DirectPairParticipants, PrincipalIdentity,
        collaboration::{
            channel_directory::{ChannelStatus, ChannelVisibility, DirectoryRoster},
            channel_lifecycle::{
                ChannelDetailFact, ChannelDetailReadPort, ChannelDetailRefusal,
                ChannelLifecyclePlacement, ChannelLifecycleProjectionError, ChannelLifecycleRow,
            },
        },
        conversation_title::CanonicalConversationTitle,
    },
    foundation::identity::{AgentId, ConversationId, UserId, WorkspaceId},
    foundation::persistence::PersistenceFailure,
    identity_access::access::WorkspaceRole,
};
use sqlx::{Row, Sqlite, Transaction, sqlite::SqliteRow};
use thiserror::Error;

use crate::conversation_messaging::messaging::MessagingStore;

#[derive(Clone)]
pub struct SqliteChannelLifecycleRead {
    pool: crate::foundation::database::SqliteDatabase,
}

impl<E> MessagingStore<E> {
    pub fn channel_lifecycle(&self) -> SqliteChannelLifecycleRead {
        SqliteChannelLifecycleRead {
            pool: self.pool.clone(),
        }
    }
}

#[derive(Debug, Error)]
pub enum ChannelLifecycleStoreError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("stored column {field} does not contain a usable identity")]
    Identity { field: &'static str },
    #[error("stored conversation placement is malformed")]
    Placement,
    #[error("stored workspace title/key pair is not canonical")]
    Title,
    #[error("stored conversation status is outside the closed set")]
    Status,
    #[error("stored conversation visibility is outside the closed set")]
    Visibility,
    #[error("stored workspace role is outside the closed set")]
    WorkspaceRole,
    #[error("stored spawned-agent reference is malformed")]
    SpawnedAgent,
    #[error("channel lifecycle projection is inconsistent")]
    Projection(#[from] ChannelLifecycleProjectionError),
}

impl From<ChannelLifecycleStoreError> for PersistenceFailure {
    fn from(value: ChannelLifecycleStoreError) -> Self {
        Self::from_source(value)
    }
}

impl ChannelDetailReadPort for SqliteChannelLifecycleRead {
    type Error = PersistenceFailure;

    async fn detail(
        &self,
        viewer: &UserId,
        conversation: &ConversationId,
    ) -> Result<Result<ChannelDetailFact, ChannelDetailRefusal>, Self::Error> {
        self.detail_inner(viewer, conversation)
            .await
            .map_err(PersistenceFailure::from)
    }
}

impl SqliteChannelLifecycleRead {
    async fn detail_inner(
        &self,
        viewer: &UserId,
        requested: &ConversationId,
    ) -> Result<Result<ChannelDetailFact, ChannelDetailRefusal>, ChannelLifecycleStoreError> {
        let mut tx = self.pool.begin_with("BEGIN DEFERRED").await?;
        let Some(row) = load_channel_row(&mut tx, requested).await? else {
            tx.commit().await?;
            return Ok(Err(ChannelDetailRefusal::NotFound));
        };

        // Decode the complete human-membership candidate set before admission.
        // A malformed later row cannot disappear behind a nonmember/public filter.
        let human_members = load_human_members(&mut tx, requested).await?;
        let stored_conversation_member = human_members.iter().any(|member| member == viewer);

        let (workspace_role, admitted, is_member) = match row.placement() {
            ChannelLifecyclePlacement::Workspace(workspace) => {
                let workspace_exists: bool =
                    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workspaces w WHERE w.id=?)")
                        .bind(workspace.workspace().as_str())
                        .fetch_one(&mut *tx)
                        .await?;
                let role = if workspace_exists {
                    load_workspace_role(&mut tx, viewer, workspace.workspace()).await?
                } else {
                    None
                };
                let is_member = role.is_some() && stored_conversation_member;
                let public_widen = role.is_some()
                    && workspace.roster() == DirectoryRoster::Open
                    && row.visibility() == ChannelVisibility::Public
                    && row.archived_at().is_none();
                (role, is_member || public_widen, is_member)
            }
            ChannelLifecyclePlacement::DirectPair(participants) => {
                let identity = PrincipalIdentity::Human(viewer.clone());
                (
                    None,
                    participants.both().contains(&&identity),
                    stored_conversation_member,
                )
            }
        };
        if !admitted {
            tx.commit().await?;
            return Ok(Err(ChannelDetailRefusal::Forbidden));
        }
        let fact = ChannelDetailFact::projected(
            viewer.clone(),
            row,
            is_member,
            workspace_role,
            human_members.len() as u64,
        )?;
        tx.commit().await?;
        Ok(Ok(fact))
    }
}

pub(crate) async fn load_channel_row(
    tx: &mut Transaction<'_, Sqlite>,
    requested: &ConversationId,
) -> Result<Option<ChannelLifecycleRow>, ChannelLifecycleStoreError> {
    let raw = sqlx::query(
        "SELECT c.id,c.workspace_id,c.placement_kind,c.roster_policy,c.title,c.title_key,\
         c.status,c.spawned_by_agent_id,c.closed_at,c.close_summary,c.archived_at,\
         c.latest_message_seq,c.created_at,c.updated_at,c.visibility,c.endpoint_a_kind,c.endpoint_a_id,\
         c.endpoint_a_deleted_at,c.endpoint_b_kind,c.endpoint_b_id,c.endpoint_b_deleted_at,\
         CASE WHEN c.spawned_by_agent_id IS NULL THEN 1 ELSE EXISTS(SELECT 1 FROM agents a WHERE a.id=c.spawned_by_agent_id) END AS spawned_agent_valid \
         FROM conversations c WHERE c.id=?",
    )
    .bind(requested.as_str())
    .fetch_optional(&mut **tx)
    .await?;
    raw.map(|raw| decode_row(&raw, requested)).transpose()
}

fn decode_row(
    raw: &SqliteRow,
    requested: &ConversationId,
) -> Result<ChannelLifecycleRow, ChannelLifecycleStoreError> {
    let conversation = ConversationId::parse(raw.try_get::<String, _>("id")?).map_err(|_| {
        ChannelLifecycleStoreError::Identity {
            field: "conversations.id",
        }
    })?;
    if &conversation != requested {
        return Err(ChannelLifecycleStoreError::Identity {
            field: "conversations.id",
        });
    }
    let roster = DirectoryRoster::parse(&raw.try_get::<String, _>("roster_policy")?)
        .map_err(|_| ChannelLifecycleStoreError::Placement)?;
    let status = ChannelStatus::parse(&raw.try_get::<String, _>("status")?)
        .map_err(|_| ChannelLifecycleStoreError::Status)?;
    let visibility = ChannelVisibility::parse(&raw.try_get::<String, _>("visibility")?)
        .map_err(|_| ChannelLifecycleStoreError::Visibility)?;
    let spawned_by_agent = raw
        .try_get::<Option<String>, _>("spawned_by_agent_id")?
        .map(AgentId::parse)
        .transpose()
        .map_err(|_| ChannelLifecycleStoreError::SpawnedAgent)?;
    if raw.try_get::<i64, _>("spawned_agent_valid")? != 1 {
        return Err(ChannelLifecycleStoreError::SpawnedAgent);
    }
    let workspace: Option<String> = raw.try_get("workspace_id")?;
    let title: String = raw.try_get("title")?;
    let title_key: Option<String> = raw.try_get("title_key")?;
    let a_kind: Option<String> = raw.try_get("endpoint_a_kind")?;
    let a_id: Option<String> = raw.try_get("endpoint_a_id")?;
    let a_deleted: Option<i64> = raw.try_get("endpoint_a_deleted_at")?;
    let b_kind: Option<String> = raw.try_get("endpoint_b_kind")?;
    let b_id: Option<String> = raw.try_get("endpoint_b_id")?;
    let b_deleted: Option<i64> = raw.try_get("endpoint_b_deleted_at")?;

    let common = |row: ChannelLifecycleRow| Ok(row);
    match raw.try_get::<String, _>("placement_kind")?.as_str() {
        "workspace" => {
            if a_kind.is_some()
                || a_id.is_some()
                || a_deleted.is_some()
                || b_kind.is_some()
                || b_id.is_some()
                || b_deleted.is_some()
            {
                return Err(ChannelLifecycleStoreError::Placement);
            }
            let workspace =
                WorkspaceId::parse(workspace.ok_or(ChannelLifecycleStoreError::Placement)?)
                    .map_err(|_| ChannelLifecycleStoreError::Identity {
                        field: "conversations.workspace_id",
                    })?;
            let canonical = CanonicalConversationTitle::parse(&title)
                .map_err(|_| ChannelLifecycleStoreError::Title)?;
            if canonical.title() != title || title_key.as_deref() != Some(canonical.key()) {
                return Err(ChannelLifecycleStoreError::Title);
            }
            common(ChannelLifecycleRow::workspace_placed(
                conversation,
                workspace,
                roster,
                canonical,
                status,
                spawned_by_agent,
                raw.try_get("closed_at")?,
                raw.try_get("close_summary")?,
                raw.try_get("archived_at")?,
                raw.try_get("latest_message_seq")?,
                raw.try_get("created_at")?,
                raw.try_get("updated_at")?,
                visibility,
            )?)
        }
        "direct_pair" => {
            if workspace.is_some() || title_key.is_some() || roster != DirectoryRoster::Fixed {
                return Err(ChannelLifecycleStoreError::Placement);
            }
            let first = endpoint(a_kind, a_id, "conversations.endpoint_a_id")?;
            let second = endpoint(b_kind, b_id, "conversations.endpoint_b_id")?;
            let participants = DirectPairParticipants::between(first.clone(), second.clone())
                .map_err(|_| ChannelLifecycleStoreError::Placement)?;
            if participants.first() != &first || participants.second() != &second {
                return Err(ChannelLifecycleStoreError::Placement);
            }
            common(ChannelLifecycleRow::direct_pair(
                conversation,
                participants,
                title,
                status,
                spawned_by_agent,
                raw.try_get("closed_at")?,
                raw.try_get("close_summary")?,
                raw.try_get("archived_at")?,
                raw.try_get("latest_message_seq")?,
                raw.try_get("created_at")?,
                raw.try_get("updated_at")?,
                visibility,
            )?)
        }
        _ => Err(ChannelLifecycleStoreError::Placement),
    }
}

fn endpoint(
    kind: Option<String>,
    id: Option<String>,
    field: &'static str,
) -> Result<PrincipalIdentity, ChannelLifecycleStoreError> {
    let id = id.ok_or(ChannelLifecycleStoreError::Placement)?;
    match kind.as_deref() {
        Some("human") => UserId::parse(id)
            .map(PrincipalIdentity::Human)
            .map_err(|_| ChannelLifecycleStoreError::Identity { field }),
        Some("agent") => AgentId::parse(id)
            .map(PrincipalIdentity::Agent)
            .map_err(|_| ChannelLifecycleStoreError::Identity { field }),
        _ => Err(ChannelLifecycleStoreError::Placement),
    }
}

pub(crate) async fn load_human_members(
    tx: &mut Transaction<'_, Sqlite>,
    requested: &ConversationId,
) -> Result<Vec<UserId>, ChannelLifecycleStoreError> {
    let rows = sqlx::query(
        "SELECT conversation_id,user_id,joined_at FROM conversation_users WHERE conversation_id=?",
    )
    .bind(requested.as_str())
    .fetch_all(&mut **tx)
    .await?;
    let mut members = Vec::with_capacity(rows.len());
    for row in rows {
        let source =
            ConversationId::parse(row.try_get::<String, _>("conversation_id")?).map_err(|_| {
                ChannelLifecycleStoreError::Identity {
                    field: "conversation_users.conversation_id",
                }
            })?;
        if &source != requested {
            return Err(ChannelLifecycleStoreError::Identity {
                field: "conversation_users.conversation_id",
            });
        }
        let member = UserId::parse(row.try_get::<String, _>("user_id")?).map_err(|_| {
            ChannelLifecycleStoreError::Identity {
                field: "conversation_users.user_id",
            }
        })?;
        let _: i64 = row.try_get("joined_at")?;
        members.push(member);
    }
    Ok(members)
}

pub(crate) async fn load_workspace_role(
    tx: &mut Transaction<'_, Sqlite>,
    viewer: &UserId,
    workspace: &WorkspaceId,
) -> Result<Option<WorkspaceRole>, ChannelLifecycleStoreError> {
    Ok(load_workspace_role_fact(tx, viewer, workspace)
        .await?
        .map(|fact| fact.role))
}

pub(crate) struct WorkspaceRoleFact {
    pub(crate) viewer: UserId,
    role: WorkspaceRole,
}

pub(crate) async fn load_workspace_role_fact(
    tx: &mut Transaction<'_, Sqlite>,
    viewer: &UserId,
    workspace: &WorkspaceId,
) -> Result<Option<WorkspaceRoleFact>, ChannelLifecycleStoreError> {
    let row = sqlx::query(
        "SELECT workspace_id,user_id,role FROM workspace_users WHERE workspace_id=? AND user_id=?",
    )
    .bind(workspace.as_str())
    .bind(viewer.as_str())
    .fetch_optional(&mut **tx)
    .await?;
    row.map(|row| {
        let stored_workspace = WorkspaceId::parse(row.try_get::<String, _>("workspace_id")?)
            .map_err(|_| ChannelLifecycleStoreError::Identity {
                field: "workspace_users.workspace_id",
            })?;
        let stored_viewer = UserId::parse(row.try_get::<String, _>("user_id")?).map_err(|_| {
            ChannelLifecycleStoreError::Identity {
                field: "workspace_users.user_id",
            }
        })?;
        if &stored_workspace != workspace || &stored_viewer != viewer {
            return Err(ChannelLifecycleStoreError::Identity {
                field: "workspace_users.coordinate",
            });
        }
        let role = WorkspaceRole::parse(&row.try_get::<String, _>("role")?)
            .ok_or(ChannelLifecycleStoreError::WorkspaceRole)?;
        Ok(WorkspaceRoleFact {
            viewer: stored_viewer,
            role,
        })
    })
    .transpose()
}
