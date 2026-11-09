//! SQLite implementation of the workspace roster/capability component.

use std::collections::HashSet;

use silverhand_backend_core::{
    conversation_messaging::messaging::collaboration::membership::{
        CommittedMembershipNotice, MembershipAuditParticipant, member_removed_audit,
    },
    foundation::{
        identity::{AgentId, ConversationId, Principal, UserId, WorkspaceId},
        persistence::PersistenceFailure,
    },
    identity_access::access::{
        AgentCapabilitySet, Capability, CapabilityReadOutcome, CapabilityReplaceOutcome,
        CapabilityReplacePreflight, HumanRoleChange, HumanRoleChangeOutcome,
        OwnershipTransferOutcome, WorkspaceAgentRemovalOutcome, WorkspaceAgentRemovalPreflight,
        WorkspaceAgentRosterEntry, WorkspaceCapabilityPort, WorkspaceHumanRemovalOutcome,
        WorkspaceHumanRosterEntry, WorkspaceLeaveOutcome, WorkspaceMembershipChanged,
        WorkspaceMembershipPort, WorkspaceOwnershipTransferPort, WorkspaceRole, WorkspaceRoster,
        WorkspaceRosterRead, WorkspaceViewerMembershipChangeKind, WorkspaceViewerMembershipChanged,
        may_edit_agent_capabilities, role_meets,
    },
};
use sqlx::Row;

use super::{SqliteAccessStore, SqliteAccessStoreError};

#[cfg(test)]
#[derive(Clone)]
struct ReadSnapshotHook {
    workspace: String,
    reached: std::sync::Arc<tokio::sync::Notify>,
    resume: std::sync::Arc<tokio::sync::Notify>,
}

#[cfg(test)]
static READ_SNAPSHOT_HOOK: std::sync::Mutex<Option<ReadSnapshotHook>> = std::sync::Mutex::new(None);

#[cfg(test)]
async fn pause_after_read_authority(workspace: &WorkspaceId) {
    let hook = READ_SNAPSHOT_HOOK.lock().unwrap().clone();
    if let Some(hook) = hook.filter(|hook| hook.workspace == workspace.as_str()) {
        hook.reached.notify_one();
        hook.resume.notified().await;
    }
}

fn failure(error: sqlx::Error) -> PersistenceFailure {
    PersistenceFailure::from_source(SqliteAccessStoreError::Sql(error))
}

fn parse_role(token: &str) -> Result<WorkspaceRole, PersistenceFailure> {
    WorkspaceRole::parse(token).ok_or_else(|| {
        #[derive(Debug, thiserror::Error)]
        #[error("stored workspace role is outside the closed set")]
        struct CorruptRole;
        PersistenceFailure::from_source(CorruptRole)
    })
}

async fn regular_workspace<'e, E>(
    executor: E,
    workspace: &WorkspaceId,
) -> Result<bool, PersistenceFailure>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let row: Option<Option<String>> =
        sqlx::query_scalar("SELECT owner_agent_id FROM workspaces WHERE id = ?")
            .bind(workspace.as_str())
            .fetch_optional(executor)
            .await
            .map_err(failure)?;
    Ok(matches!(row, Some(None)))
}

async fn human_role<'e, E>(
    executor: E,
    workspace: &WorkspaceId,
    user: &UserId,
) -> Result<Option<WorkspaceRole>, PersistenceFailure>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let token: Option<String> = sqlx::query_scalar(
        "SELECT role FROM workspace_users WHERE workspace_id = ? AND user_id = ?",
    )
    .bind(workspace.as_str())
    .bind(user.as_str())
    .fetch_optional(executor)
    .await
    .map_err(failure)?;
    token.as_deref().map(parse_role).transpose()
}

fn human_actor(principal: &Principal) -> Option<&UserId> {
    match principal {
        Principal::Human { user_id } => Some(user_id),
        Principal::System | Principal::Agent { .. } => None,
    }
}

async fn human_entry<'e, E>(
    executor: E,
    workspace: &WorkspaceId,
    user: &UserId,
) -> Result<Option<WorkspaceHumanRosterEntry>, PersistenceFailure>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let row = sqlx::query("SELECT u.name,u.display_name,u.avatar_storage_key,u.created_at,u.updated_at,wu.joined_at,wu.role FROM workspace_users wu JOIN users u ON u.id=wu.user_id WHERE wu.workspace_id=? AND wu.user_id=?")
        .bind(workspace.as_str()).bind(user.as_str()).fetch_optional(executor).await.map_err(failure)?;
    row.map(|row| {
        Ok(WorkspaceHumanRosterEntry {
            id: user.clone(),
            name: row.try_get("name").map_err(failure)?,
            display_name: row.try_get("display_name").map_err(failure)?,
            avatar_storage_key: row.try_get("avatar_storage_key").map_err(failure)?,
            created_at: row.try_get("created_at").map_err(failure)?,
            updated_at: row.try_get("updated_at").map_err(failure)?,
            joined_at_ms: row.try_get("joined_at").map_err(failure)?,
            role: parse_role(&row.try_get::<String, _>("role").map_err(failure)?)?,
        })
    })
    .transpose()
}

async fn audit_human_departures(
    executor: &mut sqlx::SqliteConnection,
    principal: &Principal,
    workspace: &WorkspaceId,
    user: &UserId,
    now: i64,
) -> Result<Vec<CommittedMembershipNotice>, PersistenceFailure> {
    let actor = audit_actor(executor, principal).await?;
    let member = audit_member(executor, "human", user.as_str()).await?;
    let notice = member_removed_audit(actor, member);
    sqlx::query("UPDATE conversations SET latest_message_seq=latest_message_seq+1,updated_at=? WHERE workspace_id=? AND id IN (SELECT conversation_id FROM conversation_users WHERE user_id=?)")
        .bind(now).bind(workspace.as_str()).bind(user.as_str()).execute(&mut *executor).await.map_err(failure)?;
    insert_departure_notices(executor, workspace, "human", user.as_str(), &notice, now).await
}

async fn audit_agent_departures(
    executor: &mut sqlx::SqliteConnection,
    principal: &Principal,
    workspace: &WorkspaceId,
    agent: &AgentId,
    now: i64,
) -> Result<Vec<CommittedMembershipNotice>, PersistenceFailure> {
    let actor = audit_actor(executor, principal).await?;
    let member = audit_member(executor, "agent", agent.as_str()).await?;
    let notice = member_removed_audit(actor, member);
    sqlx::query("UPDATE conversations SET latest_message_seq=latest_message_seq+1,updated_at=? WHERE workspace_id=? AND id IN (SELECT conversation_id FROM conversation_agents WHERE agent_id=?)")
        .bind(now).bind(workspace.as_str()).bind(agent.as_str()).execute(&mut *executor).await.map_err(failure)?;
    insert_departure_notices(executor, workspace, "agent", agent.as_str(), &notice, now).await
}

async fn audit_actor(
    executor: &mut sqlx::SqliteConnection,
    principal: &Principal,
) -> Result<MembershipAuditParticipant, PersistenceFailure> {
    let (kind, id, agent) = match principal {
        Principal::System => return Ok(MembershipAuditParticipant::system()),
        Principal::Human { user_id } => ("user", user_id.as_str(), false),
        Principal::Agent { agent_id } => ("agent", agent_id.as_str(), true),
    };
    let row = if agent {
        sqlx::query("SELECT name,display_name FROM agents WHERE id=?")
            .bind(id)
            .fetch_optional(&mut *executor)
            .await
            .map_err(failure)?
    } else {
        sqlx::query("SELECT name,display_name FROM users WHERE id=?")
            .bind(id)
            .fetch_optional(&mut *executor)
            .await
            .map_err(failure)?
    };
    match row {
        Some(row) => Ok(MembershipAuditParticipant {
            kind,
            id: id.to_owned(),
            name: row.try_get("name").map_err(failure)?,
            display_name: row.try_get("display_name").map_err(failure)?,
        }),
        None => Ok(MembershipAuditParticipant {
            kind,
            id: id.to_owned(),
            name: id.to_owned(),
            display_name: None,
        }),
    }
}

async fn audit_member(
    executor: &mut sqlx::SqliteConnection,
    kind: &'static str,
    id: &str,
) -> Result<MembershipAuditParticipant, PersistenceFailure> {
    let row = if kind == "agent" {
        sqlx::query("SELECT name,display_name FROM agents WHERE id=?")
            .bind(id)
            .fetch_one(&mut *executor)
            .await
            .map_err(failure)?
    } else {
        sqlx::query("SELECT name,display_name FROM users WHERE id=?")
            .bind(id)
            .fetch_one(&mut *executor)
            .await
            .map_err(failure)?
    };
    Ok(MembershipAuditParticipant {
        kind,
        id: id.to_owned(),
        name: row.try_get("name").map_err(failure)?,
        display_name: row.try_get("display_name").map_err(failure)?,
    })
}

async fn insert_departure_notices(
    executor: &mut sqlx::SqliteConnection,
    workspace: &WorkspaceId,
    member_kind: &str,
    member_id: &str,
    notice: &silverhand_backend_core::conversation_messaging::messaging::collaboration::membership::MembershipAudit,
    now: i64,
) -> Result<Vec<CommittedMembershipNotice>, PersistenceFailure> {
    let query = if member_kind == "agent" {
        "INSERT INTO messages(id,conversation_id,from_kind,content,metadata,seq,created_at) SELECT lower(hex(randomblob(16))),c.id,'system',?,json_object('subtype',?,'actor_kind',?,'actor_id',?,'actor_name',?,'actor_display_name',?,'member_kind',?,'member_id',?,'member_name',?,'member_display_name',?),c.latest_message_seq,? FROM conversations c JOIN conversation_agents cm ON cm.conversation_id=c.id AND cm.agent_id=? WHERE c.workspace_id=? RETURNING id,conversation_id,content,metadata,seq,created_at"
    } else {
        "INSERT INTO messages(id,conversation_id,from_kind,content,metadata,seq,created_at) SELECT lower(hex(randomblob(16))),c.id,'system',?,json_object('subtype',?,'actor_kind',?,'actor_id',?,'actor_name',?,'actor_display_name',?,'member_kind',?,'member_id',?,'member_name',?,'member_display_name',?),c.latest_message_seq,? FROM conversations c JOIN conversation_users cm ON cm.conversation_id=c.id AND cm.user_id=? WHERE c.workspace_id=? RETURNING id,conversation_id,content,metadata,seq,created_at"
    };
    let rows = sqlx::query(query)
        .bind(&notice.content)
        .bind(notice.subtype)
        .bind(notice.actor.kind)
        .bind(&notice.actor.id)
        .bind(&notice.actor.name)
        .bind(&notice.actor.display_name)
        .bind(notice.member.kind)
        .bind(&notice.member.id)
        .bind(&notice.member.name)
        .bind(&notice.member.display_name)
        .bind(now)
        .bind(member_id)
        .bind(workspace.as_str())
        .fetch_all(&mut *executor)
        .await
        .map_err(failure)?;
    rows.into_iter()
        .map(|row| {
            let message = row.try_get::<String, _>("id").map_err(failure)?;
            let conversation = row
                .try_get::<String, _>("conversation_id")
                .map_err(failure)?;
            Ok(CommittedMembershipNotice {
                conversation_id: ConversationId::parse(conversation)
                    .map_err(PersistenceFailure::from_source)?,
                workspace_id: workspace.clone(),
                message_id:
                    silverhand_backend_core::conversation_messaging::messaging::MessageId::parse(
                        message,
                    )
                    .map_err(PersistenceFailure::from_source)?,
                content: row.try_get("content").map_err(failure)?,
                metadata: row.try_get("metadata").map_err(failure)?,
                seq: row.try_get("seq").map_err(failure)?,
                created_at: row.try_get("created_at").map_err(failure)?,
            })
        })
        .collect()
}

impl WorkspaceOwnershipTransferPort for SqliteAccessStore {
    async fn transfer_ownership(
        &self,
        owner: &UserId,
        workspace: &WorkspaceId,
        target: &UserId,
    ) -> Result<OwnershipTransferOutcome, PersistenceFailure> {
        let mut tx = self
            .pool
            .begin_with(super::ROOT_WRITE_BEGIN)
            .await
            .map_err(failure)?;
        if !regular_workspace(&mut *tx, workspace).await? {
            return Ok(OwnershipTransferOutcome::WorkspaceNotFound);
        }
        match human_role(&mut *tx, workspace, owner).await? {
            Some(WorkspaceRole::Owner) => {}
            None => return Ok(OwnershipTransferOutcome::WorkspaceNotFound),
            Some(_) => return Ok(OwnershipTransferOutcome::Forbidden),
        }
        if target == owner || human_role(&mut *tx, workspace, target).await?.is_none() {
            return Ok(OwnershipTransferOutcome::MemberNotFound);
        }
        for (role, user) in [("admin", owner), ("owner", target)] {
            sqlx::query("UPDATE workspace_users SET role=? WHERE workspace_id=? AND user_id=?")
                .bind(role)
                .bind(workspace.as_str())
                .bind(user.as_str())
                .execute(&mut *tx)
                .await
                .map_err(failure)?;
        }
        sqlx::query("UPDATE workspaces SET owner_user_id=? WHERE id=?")
            .bind(target.as_str())
            .bind(workspace.as_str())
            .execute(&mut *tx)
            .await
            .map_err(failure)?;
        tx.commit().await.map_err(failure)?;
        let changed = |user: &UserId, role| WorkspaceViewerMembershipChanged {
            workspace_id: workspace.clone(),
            user_id: user.clone(),
            kind: WorkspaceViewerMembershipChangeKind::RoleChanged,
            role: Some(role),
        };
        Ok(OwnershipTransferOutcome::Transferred {
            previous_owner: changed(owner, WorkspaceRole::Admin),
            new_owner: changed(target, WorkspaceRole::Owner),
        })
    }
}

impl WorkspaceMembershipPort for SqliteAccessStore {
    async fn list_workspace_members(
        &self,
        principal: &Principal,
        workspace: &WorkspaceId,
    ) -> Result<WorkspaceRosterRead, PersistenceFailure> {
        let mut tx = self.pool.begin().await.map_err(failure)?;
        if !regular_workspace(&mut *tx, workspace).await? {
            return Ok(WorkspaceRosterRead::WorkspaceNotFound);
        }
        let viewer_role = match human_actor(principal) {
            Some(user) => human_role(&mut *tx, workspace, user).await?,
            None if matches!(principal, Principal::System) => Some(WorkspaceRole::Owner),
            None => None,
        };
        if viewer_role.is_none() {
            return Ok(WorkspaceRosterRead::Forbidden);
        }
        #[cfg(test)]
        pause_after_read_authority(workspace).await;
        let rows = sqlx::query("SELECT a.id,a.name,a.display_name,a.runtime,a.model,a.reasoning_effort,a.fast_mode,a.host_id,h.display_label host_display_label,h.name host_name,a.description,a.session_scope,a.created_by_user_id,a.avatar_storage_key,a.materialization_status,a.materialization_failed_code,a.materialization_failed_at,a.deletion_started_at,a.created_at,a.updated_at FROM workspace_members wm JOIN agents a ON a.id=wm.agent_id LEFT JOIN hosts h ON h.id=a.host_id WHERE wm.workspace_id=? ORDER BY wm.joined_at")
            .bind(workspace.as_str()).fetch_all(&mut *tx).await.map_err(failure)?;
        let mut agents = Vec::with_capacity(rows.len());
        for row in rows {
            let raw_id: String = row.try_get("id").map_err(failure)?;
            let raw_creator: String = row.try_get("created_by_user_id").map_err(failure)?;
            let id = AgentId::parse(raw_id).map_err(|_| {
                PersistenceFailure::from_source(SqliteAccessStoreError::Sql(sqlx::Error::Protocol(
                    "invalid stored agent id".into(),
                )))
            })?;
            let created_by_user_id = UserId::parse(raw_creator).map_err(|_| {
                PersistenceFailure::from_source(SqliteAccessStoreError::Sql(sqlx::Error::Protocol(
                    "invalid stored creator id".into(),
                )))
            })?;
            agents.push(WorkspaceAgentRosterEntry {
                id,
                name: row.try_get("name").map_err(failure)?,
                display_name: row.try_get("display_name").map_err(failure)?,
                runtime: row.try_get("runtime").map_err(failure)?,
                model: row.try_get("model").map_err(failure)?,
                reasoning_effort: row.try_get("reasoning_effort").map_err(failure)?,
                fast_mode: row
                    .try_get::<Option<i64>, _>("fast_mode")
                    .map_err(failure)?
                    .map(|value| value != 0),
                host_id: row.try_get("host_id").map_err(failure)?,
                host_display_label: row.try_get("host_display_label").map_err(failure)?,
                host_name: row.try_get("host_name").map_err(failure)?,
                description: row.try_get("description").map_err(failure)?,
                session_scope: row.try_get("session_scope").map_err(failure)?,
                created_by_user_id,
                avatar_storage_key: row.try_get("avatar_storage_key").map_err(failure)?,
                materialization_status: row.try_get("materialization_status").map_err(failure)?,
                materialization_failed_code: row
                    .try_get("materialization_failed_code")
                    .map_err(failure)?,
                materialization_failed_at: row
                    .try_get("materialization_failed_at")
                    .map_err(failure)?,
                deletion_started_at: row.try_get("deletion_started_at").map_err(failure)?,
                created_at: row.try_get("created_at").map_err(failure)?,
                updated_at: row.try_get("updated_at").map_err(failure)?,
            });
        }
        let human_rows: Vec<String> = sqlx::query_scalar(
            "SELECT user_id FROM workspace_users WHERE workspace_id=? ORDER BY joined_at",
        )
        .bind(workspace.as_str())
        .fetch_all(&mut *tx)
        .await
        .map_err(failure)?;
        let mut humans = Vec::with_capacity(human_rows.len());
        for raw in human_rows {
            let id = UserId::parse(raw).map_err(|_| {
                PersistenceFailure::from_source(SqliteAccessStoreError::Sql(sqlx::Error::Protocol(
                    "invalid stored user id".into(),
                )))
            })?;
            if let Some(entry) = human_entry(&mut *tx, workspace, &id).await? {
                humans.push(entry);
            }
        }
        Ok(WorkspaceRosterRead::Visible(WorkspaceRoster {
            workspace_id: workspace.clone(),
            agents,
            humans,
            viewer_role,
        }))
    }

    async fn change_human_role(
        &self,
        principal: &Principal,
        workspace: &WorkspaceId,
        user: &UserId,
        role: WorkspaceRole,
    ) -> Result<HumanRoleChangeOutcome, PersistenceFailure> {
        let mut tx = self
            .pool
            .begin_with(super::ROOT_WRITE_BEGIN)
            .await
            .map_err(failure)?;
        let regular: Option<Option<String>> =
            sqlx::query_scalar("SELECT owner_agent_id FROM workspaces WHERE id=?")
                .bind(workspace.as_str())
                .fetch_optional(&mut *tx)
                .await
                .map_err(failure)?;
        if !matches!(regular, Some(None)) {
            return Ok(HumanRoleChangeOutcome::WorkspaceNotFound);
        }
        let actor_role = match human_actor(principal) {
            Some(actor) => human_role(&mut *tx, workspace, actor).await?,
            None if matches!(principal, Principal::System) => Some(WorkspaceRole::Owner),
            None => None,
        };
        if !role_meets(actor_role, WorkspaceRole::Owner) {
            return Ok(HumanRoleChangeOutcome::Forbidden);
        }
        if role == WorkspaceRole::Owner {
            return Ok(HumanRoleChangeOutcome::OwnerRoleChangeUnsupported);
        }
        let target_role = human_role(&mut *tx, workspace, user).await?;
        match target_role {
            None => return Ok(HumanRoleChangeOutcome::MemberNotFound),
            Some(WorkspaceRole::Owner) => {
                return Ok(HumanRoleChangeOutcome::OwnerRoleChangeUnsupported);
            }
            _ => {}
        }
        sqlx::query("UPDATE workspace_users SET role=? WHERE workspace_id=? AND user_id=? AND role!='owner'").bind(role.as_str()).bind(workspace.as_str()).bind(user.as_str()).execute(&mut *tx).await.map_err(failure)?;
        let Some(member) = human_entry(&mut *tx, workspace, user).await? else {
            return Ok(HumanRoleChangeOutcome::MemberNotFound);
        };
        tx.commit().await.map_err(failure)?;
        Ok(HumanRoleChangeOutcome::Updated(Box::new(HumanRoleChange {
            workspace_id: workspace.clone(),
            committed: (target_role != Some(role)).then(|| WorkspaceViewerMembershipChanged {
                workspace_id: workspace.clone(),
                user_id: user.clone(),
                kind: WorkspaceViewerMembershipChangeKind::RoleChanged,
                role: Some(role),
            }),
            member,
        })))
    }

    async fn preflight_remove_workspace_agent(
        &self,
        principal: &Principal,
        workspace: &WorkspaceId,
        agent: &AgentId,
    ) -> Result<WorkspaceAgentRemovalPreflight, PersistenceFailure> {
        if !regular_workspace(&self.pool, workspace).await? {
            return Ok(WorkspaceAgentRemovalPreflight::NoSuchWorkspaceOrMember);
        }
        let actor = human_actor(principal);
        let role = match actor {
            Some(actor) => human_role(&self.pool, workspace, actor).await?,
            None if matches!(principal, Principal::System) => Some(WorkspaceRole::Owner),
            None => None,
        };
        if !role_meets(role, WorkspaceRole::Member) {
            return Ok(WorkspaceAgentRemovalPreflight::Forbidden);
        }
        let row = sqlx::query("SELECT created_by_user_id,session_scope FROM agents WHERE id=?")
            .bind(agent.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(failure)?;
        let Some(row) = row else {
            return Ok(if role_meets(role, WorkspaceRole::Admin) {
                WorkspaceAgentRemovalPreflight::Ready {
                    runtime_stops: Vec::new(),
                }
            } else {
                WorkspaceAgentRemovalPreflight::AgentNotOwned
            });
        };
        let creator: String = row.try_get("created_by_user_id").map_err(failure)?;
        if !role_meets(role, WorkspaceRole::Admin)
            && creator != actor.map(UserId::as_str).unwrap_or("")
        {
            return Ok(WorkspaceAgentRemovalPreflight::AgentNotOwned);
        }
        let scope: String = row.try_get("session_scope").map_err(failure)?;
        let raw_stops: Vec<String> = if scope == "conversation" {
            sqlx::query_scalar("SELECT c.id FROM conversations c JOIN conversation_agents ca ON ca.conversation_id=c.id WHERE c.workspace_id=? AND ca.agent_id=? ORDER BY c.id")
                .bind(workspace.as_str())
                .bind(agent.as_str())
                .fetch_all(&self.pool)
                .await
                .map_err(failure)?
        } else {
            Vec::new()
        };
        let runtime_stops = raw_stops
            .into_iter()
            .map(|raw| ConversationId::parse(raw).map_err(PersistenceFailure::from_source))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(WorkspaceAgentRemovalPreflight::Ready { runtime_stops })
    }

    async fn remove_workspace_agent(
        &self,
        principal: &Principal,
        workspace: &WorkspaceId,
        agent: &AgentId,
    ) -> Result<WorkspaceAgentRemovalOutcome, PersistenceFailure> {
        let mut tx = self
            .pool
            .begin_with(super::ROOT_WRITE_BEGIN)
            .await
            .map_err(failure)?;
        let regular: Option<Option<String>> =
            sqlx::query_scalar("SELECT owner_agent_id FROM workspaces WHERE id=?")
                .bind(workspace.as_str())
                .fetch_optional(&mut *tx)
                .await
                .map_err(failure)?;
        if !matches!(regular, Some(None)) {
            return Ok(WorkspaceAgentRemovalOutcome::NoSuchWorkspaceOrMember);
        }
        let actor = human_actor(principal);
        let role = match actor {
            Some(actor) => human_role(&mut *tx, workspace, actor).await?,
            None if matches!(principal, Principal::System) => Some(WorkspaceRole::Owner),
            None => None,
        };
        if !role_meets(role, WorkspaceRole::Member) {
            return Ok(WorkspaceAgentRemovalOutcome::Forbidden);
        }
        let creator: Option<String> =
            sqlx::query_scalar("SELECT created_by_user_id FROM agents WHERE id=?")
                .bind(agent.as_str())
                .fetch_optional(&mut *tx)
                .await
                .map_err(failure)?;
        if !role_meets(role, WorkspaceRole::Admin)
            && creator.as_deref() != actor.map(UserId::as_str)
        {
            return Ok(WorkspaceAgentRemovalOutcome::AgentNotOwned);
        }
        if creator.is_none() {
            sqlx::query("DELETE FROM workspace_members WHERE workspace_id=? AND agent_id=?")
                .bind(workspace.as_str())
                .bind(agent.as_str())
                .execute(&mut *tx)
                .await
                .map_err(failure)?;
            tx.commit().await.map_err(failure)?;
            return Ok(WorkspaceAgentRemovalOutcome::Removed(
                WorkspaceMembershipChanged {
                    workspace_id: workspace.clone(),
                    notices: Vec::new(),
                    viewer: None,
                },
            ));
        }
        let now: i64 = sqlx::query_scalar("SELECT CAST(unixepoch('subsec') * 1000 AS INTEGER)")
            .fetch_one(&mut *tx)
            .await
            .map_err(failure)?;
        let notices = audit_agent_departures(&mut tx, principal, workspace, agent, now).await?;
        sqlx::query("DELETE FROM conversation_agents WHERE agent_id=? AND conversation_id IN (SELECT id FROM conversations WHERE workspace_id=?)").bind(agent.as_str()).bind(workspace.as_str()).execute(&mut *tx).await.map_err(failure)?;
        sqlx::query("DELETE FROM workspace_members WHERE workspace_id=? AND agent_id=?")
            .bind(workspace.as_str())
            .bind(agent.as_str())
            .execute(&mut *tx)
            .await
            .map_err(failure)?;
        tx.commit().await.map_err(failure)?;
        Ok(WorkspaceAgentRemovalOutcome::Removed(
            WorkspaceMembershipChanged {
                workspace_id: workspace.clone(),
                notices,
                viewer: None,
            },
        ))
    }

    async fn remove_workspace_human(
        &self,
        principal: &Principal,
        workspace: &WorkspaceId,
        user: &UserId,
    ) -> Result<WorkspaceHumanRemovalOutcome, PersistenceFailure> {
        let mut tx = self
            .pool
            .begin_with(super::ROOT_WRITE_BEGIN)
            .await
            .map_err(failure)?;
        let regular: Option<Option<String>> =
            sqlx::query_scalar("SELECT owner_agent_id FROM workspaces WHERE id=?")
                .bind(workspace.as_str())
                .fetch_optional(&mut *tx)
                .await
                .map_err(failure)?;
        if !matches!(regular, Some(None)) {
            return Ok(WorkspaceHumanRemovalOutcome::WorkspaceNotFound);
        }
        let actor_role = match human_actor(principal) {
            Some(actor) => human_role(&mut *tx, workspace, actor).await?,
            None if matches!(principal, Principal::System) => Some(WorkspaceRole::Owner),
            None => None,
        };
        if !role_meets(actor_role, WorkspaceRole::Owner) {
            return Ok(WorkspaceHumanRemovalOutcome::Forbidden);
        }
        match human_role(&mut *tx, workspace, user).await? {
            None => return Ok(WorkspaceHumanRemovalOutcome::MemberNotFound),
            Some(WorkspaceRole::Owner) => {
                return Ok(WorkspaceHumanRemovalOutcome::OwnerRemovalUnsupported);
            }
            _ => {}
        }
        let now: i64 = sqlx::query_scalar("SELECT CAST(unixepoch('subsec') * 1000 AS INTEGER)")
            .fetch_one(&mut *tx)
            .await
            .map_err(failure)?;
        let notices = audit_human_departures(&mut tx, principal, workspace, user, now).await?;
        sqlx::query("DELETE FROM conversation_users WHERE user_id=? AND conversation_id IN (SELECT id FROM conversations WHERE workspace_id=?)").bind(user.as_str()).bind(workspace.as_str()).execute(&mut *tx).await.map_err(failure)?;
        sqlx::query("DELETE FROM workspace_users WHERE workspace_id=? AND user_id=?")
            .bind(workspace.as_str())
            .bind(user.as_str())
            .execute(&mut *tx)
            .await
            .map_err(failure)?;
        tx.commit().await.map_err(failure)?;
        Ok(WorkspaceHumanRemovalOutcome::Removed(
            WorkspaceMembershipChanged {
                workspace_id: workspace.clone(),
                notices,
                viewer: Some(WorkspaceViewerMembershipChanged {
                    workspace_id: workspace.clone(),
                    user_id: user.clone(),
                    kind: WorkspaceViewerMembershipChangeKind::Removed,
                    role: None,
                }),
            },
        ))
    }

    async fn leave_workspace(
        &self,
        viewer: &UserId,
        workspace: &WorkspaceId,
    ) -> Result<WorkspaceLeaveOutcome, PersistenceFailure> {
        let mut tx = self
            .pool
            .begin_with(super::ROOT_WRITE_BEGIN)
            .await
            .map_err(failure)?;
        let regular: Option<Option<String>> =
            sqlx::query_scalar("SELECT owner_agent_id FROM workspaces WHERE id=?")
                .bind(workspace.as_str())
                .fetch_optional(&mut *tx)
                .await
                .map_err(failure)?;
        if !matches!(regular, Some(None)) {
            return Ok(WorkspaceLeaveOutcome::WorkspaceNotFound);
        }
        match human_role(&mut *tx, workspace, viewer).await? {
            None => return Ok(WorkspaceLeaveOutcome::NotMember),
            Some(WorkspaceRole::Owner) => {
                return Ok(WorkspaceLeaveOutcome::OwnerRemovalUnsupported);
            }
            _ => {}
        }
        let now: i64 = sqlx::query_scalar("SELECT CAST(unixepoch('subsec') * 1000 AS INTEGER)")
            .fetch_one(&mut *tx)
            .await
            .map_err(failure)?;
        let notices = audit_human_departures(
            &mut tx,
            &Principal::Human {
                user_id: viewer.clone(),
            },
            workspace,
            viewer,
            now,
        )
        .await?;
        sqlx::query("DELETE FROM conversation_users WHERE user_id=? AND conversation_id IN (SELECT id FROM conversations WHERE workspace_id=?)").bind(viewer.as_str()).bind(workspace.as_str()).execute(&mut *tx).await.map_err(failure)?;
        sqlx::query("DELETE FROM workspace_users WHERE workspace_id=? AND user_id=?")
            .bind(workspace.as_str())
            .bind(viewer.as_str())
            .execute(&mut *tx)
            .await
            .map_err(failure)?;
        tx.commit().await.map_err(failure)?;
        Ok(WorkspaceLeaveOutcome::Left(WorkspaceMembershipChanged {
            workspace_id: workspace.clone(),
            notices,
            viewer: Some(WorkspaceViewerMembershipChanged {
                workspace_id: workspace.clone(),
                user_id: viewer.clone(),
                kind: WorkspaceViewerMembershipChangeKind::Removed,
                role: None,
            }),
        }))
    }
}

impl WorkspaceCapabilityPort for SqliteAccessStore {
    async fn read_capabilities(
        &self,
        principal: &Principal,
        workspace: &WorkspaceId,
        agent: &AgentId,
    ) -> Result<CapabilityReadOutcome, PersistenceFailure> {
        let mut tx = self.pool.begin().await.map_err(failure)?;
        if !regular_workspace(&mut *tx, workspace).await? {
            return Ok(CapabilityReadOutcome::WorkspaceNotFound);
        }
        let allowed = match human_actor(principal) {
            Some(user) => human_role(&mut *tx, workspace, user).await?.is_some(),
            None => matches!(principal, Principal::System),
        };
        if !allowed {
            return Ok(CapabilityReadOutcome::Forbidden);
        }
        #[cfg(test)]
        pause_after_read_authority(workspace).await;
        let member: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM workspace_members WHERE workspace_id=? AND agent_id=?)",
        )
        .bind(workspace.as_str())
        .bind(agent.as_str())
        .fetch_one(&mut *tx)
        .await
        .map_err(failure)?;
        if !member {
            return Ok(CapabilityReadOutcome::AgentNotMember);
        }
        let tokens: Vec<String> = sqlx::query_scalar("SELECT capability FROM workspace_agent_capabilities WHERE workspace_id=? AND agent_id=? ORDER BY capability ASC").bind(workspace.as_str()).bind(agent.as_str()).fetch_all(&mut *tx).await.map_err(failure)?;
        let capabilities = tokens
            .into_iter()
            .map(|token| {
                Capability::parse(&token).ok_or_else(|| {
                    PersistenceFailure::from_source(SqliteAccessStoreError::Sql(
                        sqlx::Error::Protocol("invalid stored capability".into()),
                    ))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(CapabilityReadOutcome::Found(AgentCapabilitySet {
            workspace_id: workspace.clone(),
            agent_id: agent.clone(),
            capabilities,
        }))
    }

    async fn preflight_replace_capabilities(
        &self,
        principal: &Principal,
        workspace: &WorkspaceId,
        agent: &AgentId,
    ) -> Result<CapabilityReplacePreflight, PersistenceFailure> {
        if !regular_workspace(&self.pool, workspace).await? {
            return Ok(CapabilityReplacePreflight::WorkspaceNotFound);
        }
        let actor = human_actor(principal);
        let role = match actor {
            Some(user) => human_role(&self.pool, workspace, user).await?,
            None if matches!(principal, Principal::System) => Some(WorkspaceRole::Owner),
            None => None,
        };
        let creator: Option<String> =
            sqlx::query_scalar("SELECT created_by_user_id FROM agents WHERE id=?")
                .bind(agent.as_str())
                .fetch_optional(&self.pool)
                .await
                .map_err(failure)?;
        if !may_edit_agent_capabilities(
            role,
            role.is_some(),
            creator.as_deref() == actor.map(UserId::as_str),
        ) {
            return Ok(CapabilityReplacePreflight::Forbidden);
        }
        let member: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM workspace_members WHERE workspace_id=? AND agent_id=?)",
        )
        .bind(workspace.as_str())
        .bind(agent.as_str())
        .fetch_one(&self.pool)
        .await
        .map_err(failure)?;
        Ok(if member {
            CapabilityReplacePreflight::Authorized
        } else {
            CapabilityReplacePreflight::AgentNotMember
        })
    }

    async fn replace_capabilities(
        &self,
        principal: &Principal,
        workspace: &WorkspaceId,
        agent: &AgentId,
        capabilities: &[Capability],
    ) -> Result<CapabilityReplaceOutcome, PersistenceFailure> {
        let mut tx = self
            .pool
            .begin_with(super::ROOT_WRITE_BEGIN)
            .await
            .map_err(failure)?;
        let regular: Option<Option<String>> =
            sqlx::query_scalar("SELECT owner_agent_id FROM workspaces WHERE id=?")
                .bind(workspace.as_str())
                .fetch_optional(&mut *tx)
                .await
                .map_err(failure)?;
        if !matches!(regular, Some(None)) {
            return Ok(CapabilityReplaceOutcome::WorkspaceNotFound);
        }
        let actor = human_actor(principal);
        let role = match actor {
            Some(user) => human_role(&mut *tx, workspace, user).await?,
            None if matches!(principal, Principal::System) => Some(WorkspaceRole::Owner),
            None => None,
        };
        let creator: Option<String> =
            sqlx::query_scalar("SELECT created_by_user_id FROM agents WHERE id=?")
                .bind(agent.as_str())
                .fetch_optional(&mut *tx)
                .await
                .map_err(failure)?;
        if !may_edit_agent_capabilities(
            role,
            role.is_some(),
            creator.as_deref() == actor.map(UserId::as_str),
        ) {
            return Ok(CapabilityReplaceOutcome::Forbidden);
        }
        let member: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM workspace_members WHERE workspace_id=? AND agent_id=?)",
        )
        .bind(workspace.as_str())
        .bind(agent.as_str())
        .fetch_one(&mut *tx)
        .await
        .map_err(failure)?;
        if !member {
            return Ok(CapabilityReplaceOutcome::AgentNotMember);
        }
        sqlx::query("DELETE FROM workspace_agent_capabilities WHERE workspace_id=? AND agent_id=?")
            .bind(workspace.as_str())
            .bind(agent.as_str())
            .execute(&mut *tx)
            .await
            .map_err(failure)?;
        let mut unique = HashSet::new();
        for capability in capabilities
            .iter()
            .copied()
            .filter(|capability| unique.insert(*capability))
        {
            sqlx::query("INSERT INTO workspace_agent_capabilities(workspace_id,agent_id,capability) VALUES(?,?,?)").bind(workspace.as_str()).bind(agent.as_str()).bind(capability.as_str()).execute(&mut *tx).await.map_err(failure)?;
        }
        let tokens: Vec<String> = sqlx::query_scalar("SELECT capability FROM workspace_agent_capabilities WHERE workspace_id=? AND agent_id=? ORDER BY capability ASC").bind(workspace.as_str()).bind(agent.as_str()).fetch_all(&mut *tx).await.map_err(failure)?;
        let stored = tokens
            .into_iter()
            .map(|token| {
                Capability::parse(&token).expect("writer only persisted closed capabilities")
            })
            .collect();
        tx.commit().await.map_err(failure)?;
        Ok(CapabilityReplaceOutcome::Replaced(AgentCapabilitySet {
            workspace_id: workspace.clone(),
            agent_id: agent.clone(),
            capabilities: stored,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::{Connection, Executor, SqliteConnection, sqlite::SqliteConnectOptions};

    async fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("access-membership.sqlite");
        let mut db = crate::current_schema::create_current_schema(&path).await;
        db.execute("INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES('owner','owner','Owner',1,1),('creator','creator','Creator',1,1),('other','other','Other',1,1)").await.unwrap();
        db.execute("INSERT INTO hosts(id,name,kind,created_at) VALUES('host','host','local',1)")
            .await
            .unwrap();
        db.execute(seed_agent!(
            id = "agent",
            runtime = "codex",
            host_id = "host",
            created_by_user_id = "creator",
            display_name = "Agent",
        ))
        .await
        .unwrap();
        db.execute("INSERT INTO workspaces(id,name,name_key,created_at,updated_at,owner_user_id) VALUES('workspace','Workspace','workspace',1,1,'owner')").await.unwrap();
        db.execute("INSERT INTO workspace_users(workspace_id,user_id,joined_at,role) VALUES('workspace','owner',1,'owner'),('workspace','creator',2,'member'),('workspace','other',3,'member')").await.unwrap();
        db.execute("INSERT INTO workspace_members(workspace_id,agent_id,joined_at) VALUES('workspace','agent',2)").await.unwrap();
        db.execute("INSERT INTO workspace_agent_capabilities(workspace_id,agent_id,capability) VALUES('workspace','agent','read_messages')").await.unwrap();
        db.execute(seed_channel!(
            id = "channel",
            workspace_id = "workspace",
            title = "Channel",
            visibility = "public",
        ))
        .await
        .unwrap();
        db.execute("INSERT INTO conversation_users(conversation_id,user_id,joined_at) VALUES('channel','creator',1)").await.unwrap();
        db.execute("INSERT INTO conversation_agents(conversation_id,agent_id,joined_at) VALUES('channel','agent',1)").await.unwrap();
        db.close().await.unwrap();
        (dir, path)
    }

    fn workspace() -> WorkspaceId {
        WorkspaceId::parse("workspace").unwrap()
    }
    fn agent() -> AgentId {
        AgentId::parse("agent").unwrap()
    }
    fn human(id: &str) -> Principal {
        Principal::Human {
            user_id: UserId::parse(id).unwrap(),
        }
    }

    fn install_read_hook(
        workspace: &str,
    ) -> (
        std::sync::Arc<tokio::sync::Notify>,
        std::sync::Arc<tokio::sync::Notify>,
    ) {
        let reached = std::sync::Arc::new(tokio::sync::Notify::new());
        let resume = std::sync::Arc::new(tokio::sync::Notify::new());
        *READ_SNAPSHOT_HOOK.lock().unwrap() = Some(ReadSnapshotHook {
            workspace: workspace.to_owned(),
            reached: reached.clone(),
            resume: resume.clone(),
        });
        (reached, resume)
    }

    fn clear_read_hook() {
        *READ_SNAPSHOT_HOOK.lock().unwrap() = None;
    }

    #[tokio::test]
    async fn creator_can_replace_the_set_but_an_unrelated_member_cannot_mutate_it() {
        let (_dir, path) = fixture().await;
        let store = SqliteAccessStore::open(&path).await.unwrap();
        assert_eq!(
            store
                .preflight_replace_capabilities(&human("other"), &workspace(), &agent())
                .await
                .unwrap(),
            CapabilityReplacePreflight::Forbidden
        );
        assert_eq!(
            store
                .preflight_replace_capabilities(&human("creator"), &workspace(), &agent())
                .await
                .unwrap(),
            CapabilityReplacePreflight::Authorized
        );
        let changed = store
            .replace_capabilities(
                &human("creator"),
                &workspace(),
                &agent(),
                &[Capability::ReadTasks, Capability::ReadTasks],
            )
            .await
            .unwrap();
        assert!(
            matches!(changed, CapabilityReplaceOutcome::Replaced(AgentCapabilitySet { capabilities, .. }) if capabilities == vec![Capability::ReadTasks])
        );
        assert_eq!(
            store
                .replace_capabilities(&human("other"), &workspace(), &agent(), &[])
                .await
                .unwrap(),
            CapabilityReplaceOutcome::Forbidden
        );
        let read = store
            .read_capabilities(&human("owner"), &workspace(), &agent())
            .await
            .unwrap();
        assert!(
            matches!(read, CapabilityReadOutcome::Found(AgentCapabilitySet { capabilities, .. }) if capabilities == vec![Capability::ReadTasks])
        );
        assert_eq!(
            store
                .preflight_remove_workspace_agent(&human("creator"), &workspace(), &agent())
                .await
                .unwrap(),
            WorkspaceAgentRemovalPreflight::Ready {
                runtime_stops: vec![ConversationId::parse("channel").unwrap()]
            }
        );
    }

    #[tokio::test]
    async fn capability_get_and_put_return_canonical_lexical_order_after_deduplication() {
        let (_dir, path) = fixture().await;
        let store = SqliteAccessStore::open(&path).await.unwrap();
        let expected = vec![
            Capability::CreateTasks,
            Capability::ReadMessages,
            Capability::SendMessages,
        ];
        let replaced = store
            .replace_capabilities(
                &human("creator"),
                &workspace(),
                &agent(),
                &[
                    Capability::SendMessages,
                    Capability::ReadMessages,
                    Capability::CreateTasks,
                    Capability::SendMessages,
                ],
            )
            .await
            .unwrap();
        assert!(matches!(
            replaced,
            CapabilityReplaceOutcome::Replaced(AgentCapabilitySet { capabilities, .. })
                if capabilities == expected
        ));
        let read = store
            .read_capabilities(&human("owner"), &workspace(), &agent())
            .await
            .unwrap();
        assert!(matches!(
            read,
            CapabilityReadOutcome::Found(AgentCapabilitySet { capabilities, .. })
                if capabilities == expected
        ));
    }

    #[tokio::test]
    async fn roster_and_capability_reads_remain_on_their_authorized_snapshot() {
        let (_dir, path) = fixture().await;
        let mut writer = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&path)
                .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal),
        )
        .await
        .unwrap();
        writer.execute("INSERT INTO workspaces(id,name,name_key,created_at,updated_at,owner_user_id) VALUES('snapshot-race','Snapshot','snapshot',1,1,'owner')").await.unwrap();
        writer.execute("INSERT INTO workspace_users(workspace_id,user_id,joined_at,role) VALUES('snapshot-race','owner',1,'owner'),('snapshot-race','creator',2,'member')").await.unwrap();
        writer.execute("INSERT INTO workspace_members(workspace_id,agent_id,joined_at) VALUES('snapshot-race','agent',1)").await.unwrap();
        writer.execute("INSERT INTO workspace_agent_capabilities(workspace_id,agent_id,capability) VALUES('snapshot-race','agent','read_messages')").await.unwrap();
        let store = SqliteAccessStore::open(&path).await.unwrap();
        let snapshot = WorkspaceId::parse("snapshot-race").unwrap();

        let (reached, resume) = install_read_hook(snapshot.as_str());
        let roster_store = store.clone();
        let roster_workspace = snapshot.clone();
        let roster_read = tokio::spawn(async move {
            roster_store
                .list_workspace_members(&human("owner"), &roster_workspace)
                .await
                .unwrap()
        });
        reached.notified().await;
        writer.execute("DELETE FROM workspace_users WHERE workspace_id='snapshot-race' AND user_id='owner'").await.unwrap();
        resume.notify_one();
        let roster = roster_read.await.unwrap();
        clear_read_hook();
        assert!(matches!(
            roster,
            WorkspaceRosterRead::Visible(WorkspaceRoster { viewer_role: Some(WorkspaceRole::Owner), humans, .. })
                if humans.iter().any(|row| row.id.as_str() == "owner")
        ));

        writer.execute("INSERT INTO workspace_users(workspace_id,user_id,joined_at,role) VALUES('snapshot-race','owner',1,'owner')").await.unwrap();
        let (reached, resume) = install_read_hook(snapshot.as_str());
        let capability_store = store.clone();
        let capability_workspace = snapshot.clone();
        let capability_read = tokio::spawn(async move {
            capability_store
                .read_capabilities(&human("owner"), &capability_workspace, &agent())
                .await
                .unwrap()
        });
        reached.notified().await;
        writer.execute("DELETE FROM workspace_users WHERE workspace_id='snapshot-race' AND user_id='owner'").await.unwrap();
        writer.execute("DELETE FROM workspace_members WHERE workspace_id='snapshot-race' AND agent_id='agent'").await.unwrap();
        resume.notify_one();
        let capabilities = capability_read.await.unwrap();
        clear_read_hook();
        assert!(matches!(
            capabilities,
            CapabilityReadOutcome::Found(AgentCapabilitySet { capabilities, .. })
                if capabilities == vec![Capability::ReadMessages]
        ));
    }

    #[tokio::test]
    async fn workspace_leave_cascades_only_the_human_edges_and_writes_an_audit() {
        let (_dir, path) = fixture().await;
        let store = SqliteAccessStore::open(&path).await.unwrap();
        let WorkspaceLeaveOutcome::Left(committed) = store
            .leave_workspace(&UserId::parse("creator").unwrap(), &workspace())
            .await
            .unwrap()
        else {
            panic!("workspace leave was refused");
        };
        assert_eq!(committed.notices.len(), 1);
        assert_eq!(committed.notices[0].conversation_id.as_str(), "channel");
        assert_eq!(
            committed.viewer.as_ref().map(|fact| fact.kind),
            Some(WorkspaceViewerMembershipChangeKind::Removed)
        );
        let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(path))
            .await
            .unwrap();
        let workspace_human: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM workspace_users WHERE workspace_id='workspace' AND user_id='creator'").fetch_one(&mut db).await.unwrap();
        let channel_human: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM conversation_users WHERE conversation_id='channel' AND user_id='creator'").fetch_one(&mut db).await.unwrap();
        let workspace_agent: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM workspace_members WHERE workspace_id='workspace' AND agent_id='agent'").fetch_one(&mut db).await.unwrap();
        let audit: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE conversation_id='channel' AND json_extract(metadata,'$.subtype')='conversation_member_removed'").fetch_one(&mut db).await.unwrap();
        assert_eq!(
            (workspace_human, channel_human, workspace_agent, audit),
            (0, 0, 1, 1)
        );
        let notice = sqlx::query("SELECT content,json_extract(metadata,'$.actor_kind') actor_kind,json_extract(metadata,'$.member_kind') member_kind,json_extract(metadata,'$.member_id') member_id FROM messages WHERE conversation_id='channel' AND json_extract(metadata,'$.subtype')='conversation_member_removed'").fetch_one(&mut db).await.unwrap();
        assert_eq!(
            notice.get::<String, _>("content"),
            "<silverhand://human/creator> left this conversation."
        );
        assert_eq!(notice.get::<String, _>("actor_kind"), "user");
        assert_eq!(notice.get::<String, _>("member_kind"), "human");
        assert_eq!(notice.get::<String, _>("member_id"), "creator");
    }

    #[tokio::test]
    async fn workspace_agent_remove_recomputes_authoritative_conversation_set_after_preflight() {
        let (_dir, path) = fixture().await;
        let store = SqliteAccessStore::open(&path).await.unwrap();
        assert_eq!(
            store
                .preflight_remove_workspace_agent(&human("creator"), &workspace(), &agent())
                .await
                .unwrap(),
            WorkspaceAgentRemovalPreflight::Ready {
                runtime_stops: vec![ConversationId::parse("channel").unwrap()]
            }
        );

        let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
            .await
            .unwrap();
        db.execute(seed_channel!(
            id = "late-channel",
            workspace_id = "workspace",
            title = "Late",
            title_key = "late",
            visibility = "public",
        ))
        .await
        .unwrap();
        db.execute("INSERT INTO conversation_agents(conversation_id,agent_id,joined_at) VALUES('late-channel','agent',2)").await.unwrap();
        db.close().await.unwrap();

        let WorkspaceAgentRemovalOutcome::Removed(committed) = store
            .remove_workspace_agent(&human("creator"), &workspace(), &agent())
            .await
            .unwrap()
        else {
            panic!("workspace agent removal was refused");
        };
        let mut conversations = committed
            .notices
            .iter()
            .map(|notice| notice.conversation_id.as_str())
            .collect::<Vec<_>>();
        conversations.sort_unstable();
        assert_eq!(conversations, vec!["channel", "late-channel"]);
        let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(path))
            .await
            .unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM conversation_agents WHERE agent_id='agent'"
            )
            .fetch_one(&mut db)
            .await
            .unwrap(),
            0
        );
    }
}
