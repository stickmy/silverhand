//! One writer-reserved SQLite unit for human channel creation.
//!
//! The first deferred snapshot is only a cheap refusal path. Every mutable fact
//! is read again after `BEGIN IMMEDIATE`; no value from preflight authorizes a
//! write. The insert, exact founding roster, strict RETURNING/readback decode and
//! committed projection either all commit or all roll back.

use silverhand_backend_core::{
    conversation_messaging::messaging::{
        SendClock, SendEntropy,
        collaboration::channel_directory::{
            ChannelCreationProjectionError, ChannelDirectoryProjectionError, ChannelDirectoryRow,
            ChannelStatus, ChannelVisibility, CommittedHumanChannelCreation, DirectoryRoster,
            HumanChannelCreatePort, HumanChannelCreateRefusal, HumanChannelCreateRequest,
        },
        conversation_title::CanonicalConversationTitle,
        environment::EnvironmentUnavailable,
        mint_conversation_id,
    },
    foundation::identity::{AgentId, ConversationId, UserId, WorkspaceId},
    foundation::persistence::PersistenceFailure,
    identity_access::access::{WorkspaceRole, role_governs_workspace},
};
use sqlx::{Row, Sqlite, Transaction, sqlite::SqliteRow};
use thiserror::Error;

use crate::conversation_messaging::messaging::MessagingStore;

use super::super::activity::valid_title_key_spelling;

#[derive(Clone)]
pub struct SqliteChannelDirectoryCreate<E> {
    pool: crate::foundation::database::SqliteDatabase,
    environment: E,
}

impl<E: Clone> MessagingStore<E> {
    pub fn channel_directory_create(&self) -> SqliteChannelDirectoryCreate<E> {
        SqliteChannelDirectoryCreate {
            pool: self.pool.clone(),
            environment: self.environment.clone(),
        }
    }
}

#[derive(Debug, Error)]
pub enum ChannelDirectoryCreateStoreError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("host clock or entropy failed")]
    Environment(#[from] EnvironmentUnavailable),
    #[error("created conversation row is malformed at {field}")]
    Corrupt { field: &'static str },
    #[error("created channel projection is internally inconsistent")]
    Projection(#[from] ChannelDirectoryProjectionError),
    #[error("committed channel creation is internally inconsistent")]
    CommitProjection(#[from] ChannelCreationProjectionError),
}

impl From<ChannelDirectoryCreateStoreError> for PersistenceFailure {
    fn from(value: ChannelDirectoryCreateStoreError) -> Self {
        Self::from_source(value)
    }
}

impl<E> HumanChannelCreatePort for SqliteChannelDirectoryCreate<E>
where
    E: SendClock + SendEntropy + Clone + Send + Sync + 'static,
{
    type Error = PersistenceFailure;

    async fn create_human_channel(
        &self,
        viewer: UserId,
        workspace: WorkspaceId,
        request: HumanChannelCreateRequest,
    ) -> Result<Result<CommittedHumanChannelCreation, HumanChannelCreateRefusal>, Self::Error> {
        self.create_inner(viewer, workspace, request)
            .await
            .map_err(PersistenceFailure::from)
    }
}

#[derive(Clone)]
enum WorkspaceKind {
    Regular,
    AgentHome(AgentId),
}

impl<E> SqliteChannelDirectoryCreate<E>
where
    E: SendClock + SendEntropy + Clone + Send + Sync + 'static,
{
    async fn create_inner(
        &self,
        viewer: UserId,
        workspace: WorkspaceId,
        request: HumanChannelCreateRequest,
    ) -> Result<
        Result<CommittedHumanChannelCreation, HumanChannelCreateRefusal>,
        ChannelDirectoryCreateStoreError,
    > {
        // Preflight deliberately owns no authority. It lets ordinary refusals
        // complete without joining the SQLite writer queue.
        let mut preflight = self.pool.begin_with("BEGIN DEFERRED").await?;
        if let Err(refusal) = admit(&mut preflight, &viewer, &workspace, &request).await? {
            preflight.commit().await?;
            return Ok(Err(refusal));
        }
        preflight.commit().await?;

        // Host work happens before the reservation. Its answers carry no product
        // authority; the writer snapshot below re-derives every admission fact.
        let now = self.environment.now_unix_ms()?;
        let conversation = mint_conversation_id(&self.environment)?;

        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let kind = match admit(&mut tx, &viewer, &workspace, &request).await? {
            Ok(kind) => kind,
            Err(refusal) => {
                tx.commit().await?;
                return Ok(Err(refusal));
            }
        };

        let roster = match kind {
            WorkspaceKind::Regular => DirectoryRoster::Open,
            WorkspaceKind::AgentHome(_) => DirectoryRoster::Fixed,
        };
        let returning = sqlx::query(
            "INSERT INTO conversations(\
               id,workspace_id,placement_kind,roster_policy,title,title_key,status,\
               spawned_by_agent_id,closed_at,close_summary,pinned_at,created_at,updated_at,\
               archived_at,latest_message_seq,visibility\
             ) VALUES(?,?,'workspace',?,?,?,'idle',NULL,NULL,NULL,NULL,?,?,NULL,0,?) \
             RETURNING id,workspace_id,placement_kind,roster_policy,title,title_key,status,\
                       spawned_by_agent_id,closed_at,close_summary,pinned_at,created_at,updated_at,\
                       archived_at,latest_message_seq,visibility,endpoint_a_kind,endpoint_a_id,\
                       endpoint_b_kind,endpoint_b_id,endpoint_a_deleted_at,endpoint_b_deleted_at",
        )
        .bind(conversation.as_str())
        .bind(workspace.as_str())
        .bind(roster.as_str())
        .bind(request.title().title())
        .bind(request.title().key())
        .bind(now)
        .bind(now)
        .bind(request.visibility().as_str())
        .fetch_one(&mut *tx)
        .await?;
        let returning = decode_created_row(&returning, &viewer, &workspace, request.title())?;

        match &kind {
            WorkspaceKind::Regular => {
                for agent in request.member_agent_ids() {
                    sqlx::query(
                        "INSERT INTO conversation_agents(conversation_id,agent_id,joined_at) \
                         VALUES(?,?,?)",
                    )
                    .bind(conversation.as_str())
                    .bind(agent)
                    .bind(now)
                    .execute(&mut *tx)
                    .await?;
                }
                insert_user(&mut tx, &conversation, &viewer, now).await?;
                if let Some(users) = request.member_user_ids() {
                    for raw in users {
                        if raw != viewer.as_str() {
                            insert_user_raw(&mut tx, &conversation, raw, now).await?;
                        }
                    }
                }
            }
            WorkspaceKind::AgentHome(owner) => {
                sqlx::query(
                    "INSERT INTO conversation_agents(conversation_id,agent_id,joined_at) \
                     VALUES(?,?,?)",
                )
                .bind(conversation.as_str())
                .bind(owner.as_str())
                .bind(now)
                .execute(&mut *tx)
                .await?;
                insert_user(&mut tx, &conversation, &viewer, now).await?;
            }
        }

        // A product trigger must not be able to change the row after RETURNING
        // and before commit unnoticed. Decode the durable image too and demand
        // exact agreement with the winning RETURNING fact.
        let stored = load_created_row(&mut tx, &conversation).await?;
        let stored = decode_created_row(&stored, &viewer, &workspace, request.title())?;
        if stored != returning {
            return Err(ChannelDirectoryCreateStoreError::Corrupt {
                field: "conversations:returning_readback",
            });
        }
        assert_exact_roster(&mut tx, &conversation, &kind, &viewer, &request, now).await?;

        let committed = match kind {
            WorkspaceKind::Regular => CommittedHumanChannelCreation::regular(
                viewer,
                workspace,
                request.title().clone(),
                stored,
            )?,
            WorkspaceKind::AgentHome(_) => CommittedHumanChannelCreation::agent_home(
                viewer,
                workspace,
                request.title().clone(),
                stored,
            )?,
        };
        tx.commit().await?;
        Ok(Ok(committed))
    }
}

async fn admit(
    tx: &mut Transaction<'_, Sqlite>,
    viewer: &UserId,
    workspace: &WorkspaceId,
    request: &HumanChannelCreateRequest,
) -> Result<Result<WorkspaceKind, HumanChannelCreateRefusal>, ChannelDirectoryCreateStoreError> {
    let workspace_row = sqlx::query("SELECT id,name_key,owner_agent_id FROM workspaces WHERE id=?")
        .bind(workspace.as_str())
        .fetch_optional(&mut **tx)
        .await?;
    let Some(row) = workspace_row else {
        return Ok(Err(HumanChannelCreateRefusal::WorkspaceNotFound));
    };
    let stored_workspace: String = row.try_get("id")?;
    let stored_workspace =
        WorkspaceId::parse(stored_workspace).map_err(|_| corrupt("workspaces.id"))?;
    if &stored_workspace != workspace {
        return Err(corrupt("workspaces.id:request"));
    }
    let owner: Option<String> = row.try_get("owner_agent_id")?;
    let name_key: Option<String> = row.try_get("name_key")?;
    let kind = match (owner, name_key) {
        (None, Some(key)) if valid_title_key_spelling(&key) => WorkspaceKind::Regular,
        (Some(owner), None) => WorkspaceKind::AgentHome(
            AgentId::parse(owner).map_err(|_| corrupt("workspaces.owner_agent_id"))?,
        ),
        _ => return Err(corrupt("workspaces.owner_agent_id:name_key")),
    };

    let role = sqlx::query_scalar::<_, String>(
        "SELECT role FROM workspace_users WHERE workspace_id=? AND user_id=?",
    )
    .bind(workspace.as_str())
    .bind(viewer.as_str())
    .fetch_optional(&mut **tx)
    .await?;
    let role = role
        .as_deref()
        .map(|token| WorkspaceRole::parse(token).ok_or_else(|| corrupt("workspace_users.role")))
        .transpose()?;
    if !role_governs_workspace(role) {
        return Ok(Err(HumanChannelCreateRefusal::Forbidden));
    }

    let kind = if let WorkspaceKind::AgentHome(owner) = kind {
        if request.visibility() == ChannelVisibility::Public {
            return Ok(Err(HumanChannelCreateRefusal::AgentHomePublic));
        }
        if !request.member_agent_ids().is_empty()
            || request
                .member_user_ids()
                .is_some_and(|users| !users.is_empty())
        {
            return Ok(Err(HumanChannelCreateRefusal::AgentHomeExplicitMembers));
        }
        let exists = sqlx::query_scalar::<_, i64>("SELECT 1 FROM agents WHERE id=?")
            .bind(owner.as_str())
            .fetch_optional(&mut **tx)
            .await?
            .is_some();
        if !exists {
            return Ok(Err(HumanChannelCreateRefusal::AgentHomeOwnerNotFound));
        }
        WorkspaceKind::AgentHome(owner)
    } else {
        for agent in request.member_agent_ids() {
            let exists = sqlx::query_scalar::<_, i64>("SELECT 1 FROM agents WHERE id=?")
                .bind(agent)
                .fetch_optional(&mut **tx)
                .await?
                .is_some();
            if !exists {
                return Ok(Err(HumanChannelCreateRefusal::AgentNotFound(agent.clone())));
            }
            let member = sqlx::query_scalar::<_, i64>(
                "SELECT 1 FROM workspace_members WHERE workspace_id=? AND agent_id=?",
            )
            .bind(workspace.as_str())
            .bind(agent)
            .fetch_optional(&mut **tx)
            .await?
            .is_some();
            if !member {
                return Ok(Err(HumanChannelCreateRefusal::AgentNotWorkspaceMember(
                    agent.clone(),
                )));
            }
        }
        if let Some(users) = request.member_user_ids() {
            for user in users {
                let member = sqlx::query_scalar::<_, i64>(
                    "SELECT 1 FROM workspace_users WHERE workspace_id=? AND user_id=?",
                )
                .bind(workspace.as_str())
                .bind(user)
                .fetch_optional(&mut **tx)
                .await?
                .is_some();
                if !member {
                    return Ok(Err(HumanChannelCreateRefusal::UserNotWorkspaceMember(
                        user.clone(),
                    )));
                }
            }
        }
        WorkspaceKind::Regular
    };

    let title_taken = sqlx::query_scalar::<_, i64>(
        "SELECT 1 FROM conversations \
         WHERE workspace_id=? AND placement_kind='workspace' AND title_key=?",
    )
    .bind(workspace.as_str())
    .bind(request.title().key())
    .fetch_optional(&mut **tx)
    .await?
    .is_some();
    if title_taken {
        return Ok(Err(HumanChannelCreateRefusal::TitleTaken));
    }
    Ok(Ok(kind))
}

async fn insert_user(
    tx: &mut Transaction<'_, Sqlite>,
    conversation: &ConversationId,
    user: &UserId,
    now: i64,
) -> Result<(), sqlx::Error> {
    insert_user_raw(tx, conversation, user.as_str(), now).await
}

async fn insert_user_raw(
    tx: &mut Transaction<'_, Sqlite>,
    conversation: &ConversationId,
    user: &str,
    now: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO conversation_users(conversation_id,user_id,joined_at) VALUES(?,?,?)")
        .bind(conversation.as_str())
        .bind(user)
        .bind(now)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn load_created_row(
    tx: &mut Transaction<'_, Sqlite>,
    conversation: &ConversationId,
) -> Result<SqliteRow, sqlx::Error> {
    sqlx::query(
        "SELECT id,workspace_id,placement_kind,roster_policy,title,title_key,status,\
                spawned_by_agent_id,closed_at,close_summary,pinned_at,created_at,updated_at,\
                archived_at,latest_message_seq,visibility,endpoint_a_kind,endpoint_a_id,\
                endpoint_b_kind,endpoint_b_id,endpoint_a_deleted_at,endpoint_b_deleted_at \
         FROM conversations WHERE id=?",
    )
    .bind(conversation.as_str())
    .fetch_one(&mut **tx)
    .await
}

fn decode_created_row(
    row: &SqliteRow,
    viewer: &UserId,
    requested_workspace: &WorkspaceId,
    requested_title: &CanonicalConversationTitle,
) -> Result<ChannelDirectoryRow, ChannelDirectoryCreateStoreError> {
    let id: String = row.try_get("id")?;
    let workspace: String = row.try_get("workspace_id")?;
    let placement: String = row.try_get("placement_kind")?;
    let roster: String = row.try_get("roster_policy")?;
    let title: String = row.try_get("title")?;
    let key: Option<String> = row.try_get("title_key")?;
    let status: String = row.try_get("status")?;
    let visibility: String = row.try_get("visibility")?;
    let spawned: Option<String> = row.try_get("spawned_by_agent_id")?;
    let closed_at: Option<i64> = row.try_get("closed_at")?;
    let close_summary: Option<String> = row.try_get("close_summary")?;
    let pinned_at: Option<i64> = row.try_get("pinned_at")?;
    let archived_at: Option<i64> = row.try_get("archived_at")?;
    let latest_message_seq: i64 = row.try_get("latest_message_seq")?;
    let created_at: i64 = row.try_get("created_at")?;
    let updated_at: i64 = row.try_get("updated_at")?;

    let conversation = ConversationId::parse(id).map_err(|_| corrupt("conversations.id"))?;
    let workspace =
        WorkspaceId::parse(workspace).map_err(|_| corrupt("conversations.workspace_id"))?;
    if &workspace != requested_workspace {
        return Err(corrupt("conversations.workspace_id:request"));
    }
    if placement != "workspace"
        || optional_text(row, "endpoint_a_kind")?.is_some()
        || optional_text(row, "endpoint_a_id")?.is_some()
        || optional_text(row, "endpoint_b_kind")?.is_some()
        || optional_text(row, "endpoint_b_id")?.is_some()
        || optional_i64(row, "endpoint_a_deleted_at")?.is_some()
        || optional_i64(row, "endpoint_b_deleted_at")?.is_some()
    {
        return Err(corrupt("conversations.placement"));
    }
    let roster =
        DirectoryRoster::parse(&roster).map_err(|_| corrupt("conversations.roster_policy"))?;
    let status = ChannelStatus::parse(&status).map_err(|_| corrupt("conversations.status"))?;
    let visibility =
        ChannelVisibility::parse(&visibility).map_err(|_| corrupt("conversations.visibility"))?;
    if status != ChannelStatus::Idle
        || spawned.is_some()
        || closed_at.is_some()
        || close_summary.is_some()
        || pinned_at.is_some()
        || archived_at.is_some()
        || latest_message_seq != 0
    {
        return Err(corrupt("conversations.freshness"));
    }
    let decoded =
        CanonicalConversationTitle::parse(&title).map_err(|_| corrupt("conversations.title"))?;
    if decoded.title() != title {
        return Err(corrupt("conversations.title:not_canonical"));
    }
    if key.as_deref() != Some(decoded.key()) || &decoded != requested_title {
        return Err(corrupt("conversations.title_key"));
    }
    ChannelDirectoryRow::projected(
        viewer.clone(),
        workspace,
        conversation,
        roster,
        title,
        status,
        None,
        None,
        None,
        None,
        latest_message_seq,
        created_at,
        updated_at,
        visibility,
        true,
    )
    .map_err(ChannelDirectoryCreateStoreError::Projection)
}

fn optional_text(row: &SqliteRow, field: &'static str) -> Result<Option<String>, sqlx::Error> {
    row.try_get(field)
}

fn optional_i64(row: &SqliteRow, field: &'static str) -> Result<Option<i64>, sqlx::Error> {
    row.try_get(field)
}

fn corrupt(field: &'static str) -> ChannelDirectoryCreateStoreError {
    ChannelDirectoryCreateStoreError::Corrupt { field }
}

async fn assert_exact_roster(
    tx: &mut Transaction<'_, Sqlite>,
    conversation: &ConversationId,
    kind: &WorkspaceKind,
    viewer: &UserId,
    request: &HumanChannelCreateRequest,
    now: i64,
) -> Result<(), ChannelDirectoryCreateStoreError> {
    let agent_rows = sqlx::query(
        "SELECT conversation_id,agent_id,joined_at FROM conversation_agents \
         WHERE conversation_id=? ORDER BY agent_id",
    )
    .bind(conversation.as_str())
    .fetch_all(&mut **tx)
    .await?;
    let mut agents = Vec::with_capacity(agent_rows.len());
    for row in agent_rows {
        let source = ConversationId::parse(row.try_get::<String, _>("conversation_id")?)
            .map_err(|_| corrupt("conversation_agents.conversation_id"))?;
        if &source != conversation {
            return Err(corrupt("conversation_agents.conversation_id:created"));
        }
        let member = AgentId::parse(row.try_get::<String, _>("agent_id")?)
            .map_err(|_| corrupt("conversation_agents.agent_id"))?;
        if row.try_get::<i64, _>("joined_at")? != now {
            return Err(corrupt("conversation_agents.joined_at"));
        }
        agents.push(member.as_str().to_owned());
    }
    let mut expected_agents = match kind {
        WorkspaceKind::Regular => request.member_agent_ids().to_vec(),
        WorkspaceKind::AgentHome(owner) => vec![owner.as_str().to_owned()],
    };
    expected_agents.sort();
    if agents != expected_agents {
        return Err(corrupt("conversation_agents:founding_set"));
    }

    let user_rows = sqlx::query(
        "SELECT conversation_id,user_id,joined_at FROM conversation_users \
         WHERE conversation_id=? ORDER BY user_id",
    )
    .bind(conversation.as_str())
    .fetch_all(&mut **tx)
    .await?;
    let mut users = Vec::with_capacity(user_rows.len());
    for row in user_rows {
        let source = ConversationId::parse(row.try_get::<String, _>("conversation_id")?)
            .map_err(|_| corrupt("conversation_users.conversation_id"))?;
        if &source != conversation {
            return Err(corrupt("conversation_users.conversation_id:created"));
        }
        let member = UserId::parse(row.try_get::<String, _>("user_id")?)
            .map_err(|_| corrupt("conversation_users.user_id"))?;
        if row.try_get::<i64, _>("joined_at")? != now {
            return Err(corrupt("conversation_users.joined_at"));
        }
        users.push(member.as_str().to_owned());
    }
    let mut expected_users = vec![viewer.as_str().to_owned()];
    if matches!(kind, WorkspaceKind::Regular)
        && let Some(requested) = request.member_user_ids()
    {
        for user in requested {
            if !expected_users.contains(user) {
                expected_users.push(user.clone());
            }
        }
    }
    expected_users.sort();
    if users != expected_users {
        return Err(corrupt("conversation_users:founding_set"));
    }
    Ok(())
}
