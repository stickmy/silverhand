//! SQLite conversation-membership unit of work.

use silverhand_backend_core::{
    conversation_messaging::messaging::MessageSeq,
    conversation_messaging::messaging::collaboration::membership::{
        AmbiguousMemberCandidate, ChannelMembershipOutcome, ChannelMembershipRefusal,
        CommittedMembershipNotice, ConversationAgentRemovalPreflight, ConversationMember,
        ConversationMemberCandidate, ConversationMemberMutation, ConversationMembershipChangeKind,
        ConversationMembershipChanged, ConversationMembershipPort, ConversationRoster,
        ConversationRosterRead, ManageConversationMembersCapability, ManageMembersDenied,
        MemberRemovability, MembershipAudit, MembershipAuditParticipant, channel_exit_refusal,
        member_added_audit, member_removed_audit,
    },
    foundation::{
        identity::{AgentId, ConversationId, Principal, UserId, WorkspaceId},
        persistence::PersistenceFailure,
    },
    identity_access::access::{Capability, DEFAULT_AGENT_CAPABILITIES, WorkspaceRole},
};
use sqlx::{Row, Sqlite, SqliteConnection, Transaction};

use silverhand_backend_core::conversation_messaging::messaging::collaboration::channel_lifecycle::ChannelDetailFact;

use super::super::{MessagingStore, MessagingStoreError, bump_stream_head_on};

fn failure(error: sqlx::Error) -> PersistenceFailure {
    PersistenceFailure::from_source(MessagingStoreError::Sql(error))
}

#[derive(Clone)]
struct Facts {
    id: ConversationId,
    workspace: Option<WorkspaceId>,
    placement: String,
    roster: String,
    visibility: String,
    archived_at: Option<i64>,
}

async fn facts<'e, E>(executor: E, id: &ConversationId) -> Result<Option<Facts>, PersistenceFailure>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let row = sqlx::query("SELECT workspace_id,placement_kind,roster_policy,visibility,title,archived_at FROM conversations WHERE id=?")
        .bind(id.as_str()).fetch_optional(executor).await.map_err(failure)?;
    row.map(|row| {
        let workspace = row
            .try_get::<Option<String>, _>("workspace_id")
            .map_err(failure)?
            .map(WorkspaceId::parse)
            .transpose()
            .map_err(|_| {
                PersistenceFailure::from_source(MessagingStoreError::MalformedIdentity {
                    field: "conversations.workspace_id",
                })
            })?;
        Ok(Facts {
            id: id.clone(),
            workspace,
            placement: row.try_get("placement_kind").map_err(failure)?,
            roster: row.try_get("roster_policy").map_err(failure)?,
            visibility: row.try_get("visibility").map_err(failure)?,
            archived_at: row.try_get("archived_at").map_err(failure)?,
        })
    })
    .transpose()
}

async fn role(
    executor: &mut SqliteConnection,
    workspace: &WorkspaceId,
    user: &UserId,
) -> Result<Option<WorkspaceRole>, PersistenceFailure> {
    let token: Option<String> =
        sqlx::query_scalar("SELECT role FROM workspace_users WHERE workspace_id=? AND user_id=?")
            .bind(workspace.as_str())
            .bind(user.as_str())
            .fetch_optional(&mut *executor)
            .await
            .map_err(failure)?;
    token
        .map(|token| {
            WorkspaceRole::parse(&token).ok_or_else(|| {
                PersistenceFailure::from_source(MessagingStoreError::MalformedIdentity {
                    field: "workspace_users.role",
                })
            })
        })
        .transpose()
}

async fn participates(
    executor: &mut SqliteConnection,
    principal: &Principal,
    item: &Facts,
) -> Result<bool, PersistenceFailure> {
    match principal {
        Principal::System => Ok(true),
        Principal::Agent { agent_id } => sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM conversation_agents WHERE conversation_id=? AND agent_id=?)").bind(item.id.as_str()).bind(agent_id.as_str()).fetch_one(&mut *executor).await.map_err(failure),
        Principal::Human { user_id } => {
            if item.placement == "direct_pair" {
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM conversations WHERE id=? AND ((endpoint_a_kind='human' AND endpoint_a_id=?) OR (endpoint_b_kind='human' AND endpoint_b_id=?)))").bind(item.id.as_str()).bind(user_id.as_str()).bind(user_id.as_str()).fetch_one(&mut *executor).await.map_err(failure)
            } else {
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM conversation_users cu JOIN workspace_users wu ON wu.user_id=cu.user_id JOIN conversations c ON c.id=cu.conversation_id AND c.workspace_id=wu.workspace_id WHERE cu.conversation_id=? AND cu.user_id=?)")
                    .bind(item.id.as_str())
                    .bind(user_id.as_str())
                    .fetch_one(&mut *executor)
                    .await
                    .map_err(failure)
            }
        }
    }
}

async fn roster(
    executor: &mut SqliteConnection,
    id: &ConversationId,
) -> Result<ConversationRoster, PersistenceFailure> {
    let agent_rows = sqlx::query("SELECT a.id,a.name,a.display_name,a.runtime FROM conversation_agents ca JOIN agents a ON a.id=ca.agent_id WHERE ca.conversation_id=? ORDER BY ca.joined_at")
        .bind(id.as_str()).fetch_all(&mut *executor).await.map_err(failure)?;
    let mut members = Vec::with_capacity(agent_rows.len());
    for row in agent_rows {
        let raw: String = row.try_get("id").map_err(failure)?;
        let id = AgentId::parse(raw).map_err(|_| {
            PersistenceFailure::from_source(MessagingStoreError::MalformedIdentity {
                field: "agents.id",
            })
        })?;
        members.push(ConversationMember::Agent {
            id,
            name: row.try_get("name").map_err(failure)?,
            display_name: row.try_get("display_name").map_err(failure)?,
            runtime: row.try_get("runtime").map_err(failure)?,
        });
    }
    let human_rows = sqlx::query("SELECT u.id,u.name,u.display_name,u.avatar_storage_key,u.updated_at FROM conversation_users cu JOIN users u ON u.id=cu.user_id WHERE cu.conversation_id=? ORDER BY cu.joined_at")
        .bind(id.as_str()).fetch_all(&mut *executor).await.map_err(failure)?;
    for row in human_rows {
        let raw: String = row.try_get("id").map_err(failure)?;
        let id = UserId::parse(raw).map_err(|_| {
            PersistenceFailure::from_source(MessagingStoreError::MalformedIdentity {
                field: "users.id",
            })
        })?;
        members.push(ConversationMember::Human {
            id,
            name: row.try_get("name").map_err(failure)?,
            display_name: row.try_get("display_name").map_err(failure)?,
            avatar_storage_key: row.try_get("avatar_storage_key").map_err(failure)?,
            updated_at: row.try_get("updated_at").map_err(failure)?,
        });
    }
    Ok(ConversationRoster {
        conversation_id: id.clone(),
        members,
    })
}

async fn actor_role(
    executor: &mut SqliteConnection,
    principal: &Principal,
    workspace: &WorkspaceId,
) -> Result<Option<WorkspaceRole>, PersistenceFailure> {
    match principal {
        Principal::System => Ok(Some(WorkspaceRole::Owner)),
        Principal::Human { user_id } => role(executor, workspace, user_id).await,
        Principal::Agent { .. } => Ok(None),
    }
}

async fn audit(
    executor: &mut SqliteConnection,
    item: &Facts,
    audit: &MembershipAudit,
    now: i64,
) -> Result<CommittedMembershipNotice, PersistenceFailure> {
    let id = &item.id;
    let workspace = item.workspace.clone().ok_or_else(|| {
        failure(sqlx::Error::Protocol(
            "membership notice lacks workspace placement".into(),
        ))
    })?;
    sqlx::query(
        "UPDATE conversations SET latest_message_seq=latest_message_seq+1,updated_at=? WHERE id=?",
    )
    .bind(now)
    .bind(id.as_str())
    .execute(&mut *executor)
    .await
    .map_err(failure)?;
    let row = sqlx::query("INSERT INTO messages(id,conversation_id,from_kind,content,metadata,seq,created_at) SELECT lower(hex(randomblob(16))),id,'system',?,json_object('subtype',?,'actor_kind',?,'actor_id',?,'actor_name',?,'actor_display_name',?,'member_kind',?,'member_id',?,'member_name',?,'member_display_name',?),latest_message_seq,? FROM conversations WHERE id=? RETURNING id,content,metadata,seq,created_at")
        .bind(&audit.content)
        .bind(audit.subtype)
        .bind(audit.actor.kind)
        .bind(&audit.actor.id)
        .bind(&audit.actor.name)
        .bind(&audit.actor.display_name)
        .bind(audit.member.kind)
        .bind(&audit.member.id)
        .bind(&audit.member.name)
        .bind(&audit.member.display_name)
        .bind(now)
        .bind(id.as_str())
        .fetch_one(&mut *executor)
        .await
        .map_err(failure)?;
    let seq: i64 = row.try_get("seq").map_err(failure)?;
    let seq_value = MessageSeq::try_from(seq).map_err(PersistenceFailure::from_source)?;
    bump_stream_head_on(executor, id, None, seq_value)
        .await
        .map_err(failure)?;
    let message: String = row.try_get("id").map_err(failure)?;
    Ok(CommittedMembershipNotice {
        conversation_id: id.clone(),
        workspace_id: workspace,
        message_id: silverhand_backend_core::conversation_messaging::messaging::MessageId::parse(
            message,
        )
        .map_err(PersistenceFailure::from_source)?,
        content: row.try_get("content").map_err(failure)?,
        metadata: row.try_get("metadata").map_err(failure)?,
        seq,
        created_at: row.try_get("created_at").map_err(failure)?,
    })
}

fn changed(
    item: &Facts,
    kind: ConversationMembershipChangeKind,
    notice: Option<CommittedMembershipNotice>,
    workspace_enrolled_human: Option<UserId>,
) -> Result<ConversationMembershipChanged, PersistenceFailure> {
    let workspace_id = item.workspace.clone().ok_or_else(|| {
        failure(sqlx::Error::Protocol(
            "workspace membership change lacks workspace placement".into(),
        ))
    })?;
    Ok(ConversationMembershipChanged {
        conversation_id: item.id.clone(),
        workspace_id,
        kind,
        notice,
        workspace_enrolled_human,
    })
}

async fn actor_snapshot(
    executor: &mut SqliteConnection,
    principal: &Principal,
) -> Result<MembershipAuditParticipant, PersistenceFailure> {
    let (kind, id, table) = match principal {
        Principal::System => return Ok(MembershipAuditParticipant::system()),
        Principal::Human { user_id } => ("user", user_id.as_str(), "users"),
        Principal::Agent { agent_id } => ("agent", agent_id.as_str(), "agents"),
    };
    let row = if table == "users" {
        sqlx::query("SELECT name,display_name FROM users WHERE id=?")
            .bind(id)
            .fetch_optional(&mut *executor)
            .await
            .map_err(failure)?
    } else {
        sqlx::query("SELECT name,display_name FROM agents WHERE id=?")
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

async fn member_snapshot(
    executor: &mut SqliteConnection,
    kind: &'static str,
    id: &str,
) -> Result<MembershipAuditParticipant, PersistenceFailure> {
    let row = if kind == "human" {
        sqlx::query("SELECT name,display_name FROM users WHERE id=?")
            .bind(id)
            .fetch_one(&mut *executor)
            .await
            .map_err(failure)?
    } else {
        sqlx::query("SELECT name,display_name FROM agents WHERE id=?")
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

async fn membership_audit(
    executor: &mut SqliteConnection,
    principal: &Principal,
    member_kind: &'static str,
    member_id: &str,
    added: bool,
) -> Result<MembershipAudit, PersistenceFailure> {
    let actor = actor_snapshot(executor, principal).await?;
    let member = member_snapshot(executor, member_kind, member_id).await?;
    Ok(if added {
        member_added_audit(actor, member)
    } else {
        member_removed_audit(actor, member)
    })
}

async fn seed_human_entry(
    executor: &mut SqliteConnection,
    id: &ConversationId,
    user: &UserId,
    now: i64,
) -> Result<(), PersistenceFailure> {
    sqlx::query("INSERT OR IGNORE INTO channel_attention_preferences(user_id,conversation_id,mode,updated_at) VALUES(?,?,'mentions',?)").bind(user.as_str()).bind(id.as_str()).bind(now).execute(&mut *executor).await.map_err(failure)?;
    sqlx::query("INSERT INTO conversation_reads(user_id,conversation_id,thread_root_id,last_read_seq,read_at) SELECT ?,id,NULL,latest_message_seq,? FROM conversations WHERE id=? ON CONFLICT(user_id,conversation_id) WHERE thread_root_id IS NULL DO UPDATE SET last_read_seq=max(last_read_seq,excluded.last_read_seq),read_at=excluded.read_at")
        .bind(user.as_str()).bind(now).bind(id.as_str()).execute(&mut *executor).await.map_err(failure)?;
    sqlx::query("INSERT INTO conversation_reads(user_id,conversation_id,thread_root_id,last_read_seq,read_at) SELECT ?,?,m.id,COALESCE(max(r.seq),m.seq),? FROM messages m LEFT JOIN messages r ON r.thread_root_id=m.id WHERE m.conversation_id=? AND m.thread_root_id IS NULL GROUP BY m.id HAVING count(r.id)>0 ON CONFLICT(user_id,conversation_id,thread_root_id) WHERE thread_root_id IS NOT NULL DO UPDATE SET last_read_seq=max(last_read_seq,excluded.last_read_seq),read_at=excluded.read_at")
        .bind(user.as_str()).bind(id.as_str()).bind(now).bind(id.as_str()).execute(&mut *executor).await.map_err(failure)?;
    Ok(())
}

async fn channel_state(
    tx: &mut Transaction<'_, Sqlite>,
    item: &Facts,
    viewer: &UserId,
) -> Result<ChannelDetailFact, PersistenceFailure> {
    let is_member: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM conversation_users WHERE conversation_id=? AND user_id=?)",
    )
    .bind(item.id.as_str())
    .bind(viewer.as_str())
    .fetch_one(&mut **tx)
    .await
    .map_err(failure)?;
    let workspace_owner = match &item.workspace {
        Some(workspace) => role(&mut *tx, workspace, viewer).await?,
        None => None,
    };
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM conversation_users WHERE conversation_id=?")
            .bind(item.id.as_str())
            .fetch_one(&mut **tx)
            .await
            .map_err(failure)?;
    let row = super::channel_lifecycle::load_channel_row(tx, &item.id)
        .await
        .map_err(PersistenceFailure::from)?
        .ok_or_else(|| failure(sqlx::Error::RowNotFound))?;
    ChannelDetailFact::projected(
        viewer.clone(),
        row,
        is_member,
        workspace_owner,
        count as u64,
    )
    .map_err(PersistenceFailure::from_source)
}

async fn resolve_candidate(
    executor: &mut SqliteConnection,
    workspace: &WorkspaceId,
    principal: &Principal,
    candidate: ConversationMemberCandidate,
) -> Result<Result<ConversationMemberCandidate, Vec<AmbiguousMemberCandidate>>, PersistenceFailure>
{
    match candidate {
        direct
        @ (ConversationMemberCandidate::Agent(_) | ConversationMemberCandidate::Human(_)) => {
            Ok(Ok(direct))
        }
        ConversationMemberCandidate::Handle(handle) => {
            let rows = sqlx::query(
                "SELECT kind,id,name,display_name,workspace_member,created_by_user_id FROM (\
                 SELECT 'agent' kind,a.id,a.name,a.display_name,\
                   EXISTS(SELECT 1 FROM workspace_members wm WHERE wm.workspace_id=? AND wm.agent_id=a.id) workspace_member,\
                   a.created_by_user_id created_by_user_id FROM agents a WHERE a.name=?\
                 UNION ALL \
                 SELECT 'human',u.id,u.name,u.display_name,\
                   EXISTS(SELECT 1 FROM workspace_users wu WHERE wu.workspace_id=? AND wu.user_id=u.id),\
                   NULL FROM users u WHERE u.name=?)",
            )
            .bind(workspace.as_str())
            .bind(handle.as_str())
            .bind(workspace.as_str())
            .bind(handle.as_str())
            .fetch_all(&mut *executor)
            .await
            .map_err(failure)?;
            if rows.is_empty() {
                return Ok(Err(Vec::new()));
            }

            let (actor_user, actor_is_admin) = match principal {
                Principal::Human { user_id } => (
                    Some(user_id),
                    role(executor, workspace, user_id)
                        .await?
                        .is_some_and(|role| role.meets_floor(WorkspaceRole::Admin)),
                ),
                Principal::System | Principal::Agent { .. } => (None, false),
            };
            let mut all = Vec::with_capacity(rows.len());
            let mut narrowed = Vec::with_capacity(rows.len());
            for row in rows {
                let kind: String = row.try_get("kind").map_err(failure)?;
                let id: String = row.try_get("id").map_err(failure)?;
                let workspace_member: bool = row.try_get("workspace_member").map_err(failure)?;
                let created_by: Option<String> =
                    row.try_get("created_by_user_id").map_err(failure)?;
                let display_name = row.try_get("display_name").map_err(failure)?;
                let handle = format!("@{}", row.try_get::<String, _>("name").map_err(failure)?);
                let candidate = if kind == "agent" {
                    AmbiguousMemberCandidate::Agent {
                        id: AgentId::parse(id).map_err(PersistenceFailure::from_source)?,
                        display_name,
                        handle,
                    }
                } else {
                    AmbiguousMemberCandidate::Human {
                        id: UserId::parse(id).map_err(PersistenceFailure::from_source)?,
                        display_name,
                        handle,
                    }
                };
                let may_attempt_enrollment = actor_user.is_some()
                    && if matches!(candidate, AmbiguousMemberCandidate::Agent { .. }) {
                        actor_is_admin || created_by.as_deref() == actor_user.map(UserId::as_str)
                    } else {
                        actor_is_admin
                    };
                if workspace_member || may_attempt_enrollment {
                    narrowed.push(candidate.clone());
                }
                all.push(candidate);
            }
            let considered = if narrowed.is_empty() { all } else { narrowed };
            if considered.len() != 1 {
                return Ok(Err(considered));
            }
            let candidate = &considered[0];
            Ok(Ok(match candidate {
                AmbiguousMemberCandidate::Agent { id, .. } => {
                    ConversationMemberCandidate::Agent(id.clone())
                }
                AmbiguousMemberCandidate::Human { id, .. } => {
                    ConversationMemberCandidate::Human(id.clone())
                }
            }))
        }
    }
}

impl<E: Clone + Send + Sync + 'static> ConversationMembershipPort for MessagingStore<E> {
    async fn list_conversation_members(
        &self,
        principal: &Principal,
        conversation: &ConversationId,
    ) -> Result<ConversationRosterRead, PersistenceFailure> {
        let mut tx = self.pool.begin().await.map_err(failure)?;
        let Some(item) = facts(&mut *tx, conversation).await? else {
            return Ok(ConversationRosterRead::ConversationNotFound);
        };
        if !participates(&mut tx, principal, &item).await? {
            return Ok(ConversationRosterRead::Forbidden);
        }
        let roster = roster(&mut tx, conversation).await?;
        let removability = removability(&mut tx, principal, &item, &roster).await?;
        Ok(ConversationRosterRead::Visible {
            roster,
            removability,
        })
    }

    async fn add_conversation_member(
        &self,
        principal: &Principal,
        conversation: &ConversationId,
        candidate: ConversationMemberCandidate,
        now: i64,
    ) -> Result<ConversationMemberMutation, PersistenceFailure> {
        let mut tx = self
            .pool
            .begin_with(super::super::ROOT_WRITE_BEGIN)
            .await
            .map_err(failure)?;
        let Some(item) = facts(&mut *tx, conversation).await? else {
            return Ok(ConversationMemberMutation::ConversationNotFound);
        };
        if item.roster != "open" || item.placement != "workspace" {
            return Ok(ConversationMemberMutation::Unsupported);
        }
        if !participates(&mut tx, principal, &item).await? {
            return Ok(ConversationMemberMutation::Forbidden);
        }
        let Some(workspace) = &item.workspace else {
            return Ok(ConversationMemberMutation::Unsupported);
        };
        let candidate = match resolve_candidate(&mut tx, workspace, principal, candidate).await? {
            Ok(candidate) => candidate,
            Err(candidates) if candidates.is_empty() => {
                return Ok(ConversationMemberMutation::CandidateNotFound);
            }
            Err(candidates) => {
                return Ok(ConversationMemberMutation::HandleAmbiguous { candidates });
            }
        };
        let role = actor_role(&mut tx, principal, workspace).await?;
        let committed = match candidate {
            ConversationMemberCandidate::Agent(agent) => {
                let creator: Option<String> =
                    sqlx::query_scalar("SELECT created_by_user_id FROM agents WHERE id=?")
                        .bind(agent.as_str())
                        .fetch_optional(&mut *tx)
                        .await
                        .map_err(failure)?;
                let Some(creator) = creator else {
                    return Ok(ConversationMemberMutation::CandidateNotFound);
                };
                let actor_owns = matches!(principal, Principal::Human { user_id } if user_id.as_str()==creator)
                    || matches!(principal, Principal::System);
                if matches!(principal, Principal::Human { .. })
                    && !role.is_some_and(|r| r.meets_floor(WorkspaceRole::Admin))
                    && !actor_owns
                {
                    return Ok(ConversationMemberMutation::CandidateNotOwned);
                }
                let workspace_member: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workspace_members WHERE workspace_id=? AND agent_id=?)").bind(workspace.as_str()).bind(agent.as_str()).fetch_one(&mut *tx).await.map_err(failure)?;
                if !workspace_member {
                    if !actor_owns {
                        return Ok(if matches!(principal, Principal::Human { .. }) {
                            ConversationMemberMutation::CandidateNotOwned
                        } else {
                            ConversationMemberMutation::CandidateNotWorkspaceMember
                        });
                    }
                    sqlx::query("INSERT INTO workspace_members(workspace_id,agent_id,joined_at) VALUES(?,?,?)").bind(workspace.as_str()).bind(agent.as_str()).bind(now).execute(&mut *tx).await.map_err(failure)?;
                    for capability in DEFAULT_AGENT_CAPABILITIES {
                        sqlx::query("INSERT INTO workspace_agent_capabilities(workspace_id,agent_id,capability) VALUES(?,?,?)").bind(workspace.as_str()).bind(agent.as_str()).bind(capability.as_str()).execute(&mut *tx).await.map_err(failure)?;
                    }
                }
                let inserted = sqlx::query("INSERT OR IGNORE INTO conversation_agents(conversation_id,agent_id,joined_at) VALUES(?,?,?)").bind(conversation.as_str()).bind(agent.as_str()).bind(now).execute(&mut *tx).await.map_err(failure)?.rows_affected() > 0;
                if inserted {
                    let notice =
                        membership_audit(&mut tx, principal, "agent", agent.as_str(), true).await?;
                    let message = audit(&mut tx, &item, &notice, now).await?;
                    Some(Box::new(changed(
                        &item,
                        ConversationMembershipChangeKind::MemberAdded,
                        Some(message),
                        None,
                    )?))
                } else {
                    None
                }
            }
            ConversationMemberCandidate::Human(user) => {
                let may_enroll = matches!(principal, Principal::System)
                    || role.is_some_and(|r| r.meets_floor(WorkspaceRole::Admin));
                if matches!(principal, Principal::Human { .. }) && !may_enroll {
                    return Ok(ConversationMemberMutation::Forbidden);
                }
                let exists: bool =
                    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE id=?)")
                        .bind(user.as_str())
                        .fetch_one(&mut *tx)
                        .await
                        .map_err(failure)?;
                if !exists {
                    return Ok(ConversationMemberMutation::CandidateNotFound);
                }
                let workspace_member: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workspace_users WHERE workspace_id=? AND user_id=?)").bind(workspace.as_str()).bind(user.as_str()).fetch_one(&mut *tx).await.map_err(failure)?;
                if !workspace_member && !may_enroll {
                    return Ok(ConversationMemberMutation::CandidateNotWorkspaceMember);
                }
                let workspace_enrolled = !workspace_member
                    && sqlx::query("INSERT INTO workspace_users(workspace_id,user_id,joined_at,role) VALUES(?,?,?,'member')").bind(workspace.as_str()).bind(user.as_str()).bind(now).execute(&mut *tx).await.map_err(failure)?.rows_affected() > 0;
                let inserted = sqlx::query("INSERT OR IGNORE INTO conversation_users(conversation_id,user_id,joined_at) VALUES(?,?,?)").bind(conversation.as_str()).bind(user.as_str()).bind(now).execute(&mut *tx).await.map_err(failure)?.rows_affected() > 0;
                if inserted {
                    let notice =
                        membership_audit(&mut tx, principal, "human", user.as_str(), true).await?;
                    let message = audit(&mut tx, &item, &notice, now).await?;
                    seed_human_entry(&mut tx, conversation, &user, now).await?;
                    Some(Box::new(changed(
                        &item,
                        ConversationMembershipChangeKind::MemberAdded,
                        Some(message),
                        workspace_enrolled.then_some(user),
                    )?))
                } else {
                    None
                }
            }
            ConversationMemberCandidate::Handle(_) => unreachable!("handle was resolved"),
        };
        let roster = roster(&mut tx, conversation).await?;
        tx.commit().await.map_err(failure)?;
        Ok(ConversationMemberMutation::Changed { roster, committed })
    }

    async fn join_public_channel(
        &self,
        viewer: &UserId,
        conversation: &ConversationId,
        now: i64,
    ) -> Result<ChannelMembershipOutcome, PersistenceFailure> {
        let mut tx = self
            .pool
            .begin_with(super::super::ROOT_WRITE_BEGIN)
            .await
            .map_err(failure)?;
        let Some(item) = facts(&mut *tx, conversation).await? else {
            return Ok(ChannelMembershipOutcome::Refused(
                ChannelMembershipRefusal::ChannelNotFound,
            ));
        };
        if item.roster != "open"
            || item.visibility != "public"
            || item.placement != "workspace"
            || item.archived_at.is_some()
        {
            return Ok(ChannelMembershipOutcome::Refused(
                ChannelMembershipRefusal::Forbidden,
            ));
        }
        let Some(workspace) = &item.workspace else {
            return Ok(ChannelMembershipOutcome::Refused(
                ChannelMembershipRefusal::Forbidden,
            ));
        };
        if role(&mut tx, workspace, viewer).await?.is_none() {
            return Ok(ChannelMembershipOutcome::Refused(
                ChannelMembershipRefusal::Forbidden,
            ));
        }
        let inserted = sqlx::query("INSERT OR IGNORE INTO conversation_users(conversation_id,user_id,joined_at) VALUES(?,?,?)").bind(conversation.as_str()).bind(viewer.as_str()).bind(now).execute(&mut *tx).await.map_err(failure)?.rows_affected()>0;
        if inserted {
            seed_human_entry(&mut tx, conversation, viewer, now).await?;
        }
        let state = channel_state(&mut tx, &item, viewer).await?;
        tx.commit().await.map_err(failure)?;
        let committed = inserted
            .then(|| {
                changed(
                    &item,
                    ConversationMembershipChangeKind::SelfJoined,
                    None,
                    None,
                )
            })
            .transpose()?
            .map(Box::new);
        Ok(ChannelMembershipOutcome::Changed {
            channel: Box::new(state),
            committed,
        })
    }

    async fn leave_channel(
        &self,
        viewer: &UserId,
        conversation: &ConversationId,
    ) -> Result<ChannelMembershipOutcome, PersistenceFailure> {
        let mut tx = self
            .pool
            .begin_with(super::super::ROOT_WRITE_BEGIN)
            .await
            .map_err(failure)?;
        let Some(item) = facts(&mut *tx, conversation).await? else {
            return Ok(ChannelMembershipOutcome::Refused(
                ChannelMembershipRefusal::ChannelNotFound,
            ));
        };
        if item.roster != "open" {
            return Ok(ChannelMembershipOutcome::Refused(
                ChannelMembershipRefusal::ChannelNotFound,
            ));
        }
        let before = channel_state(&mut tx, &item, viewer).await?;
        let before = before
            .bind(viewer, conversation)
            .map_err(PersistenceFailure::from_source)?;
        if !before.is_member() {
            return Ok(ChannelMembershipOutcome::Refused(
                ChannelMembershipRefusal::NotMember,
            ));
        }
        if let Some(reason) = before.leave().blocked_reason() {
            let refusal = match reason {
                silverhand_backend_core::conversation_messaging::messaging::collaboration::channel_lifecycle::ChannelLeaveBlockedReason::ChannelArchived => ChannelMembershipRefusal::ChannelArchived,
                silverhand_backend_core::conversation_messaging::messaging::collaboration::channel_lifecycle::ChannelLeaveBlockedReason::ChannelOwner => ChannelMembershipRefusal::ChannelOwner,
                _ => ChannelMembershipRefusal::LastHumanMember,
            };
            return Ok(ChannelMembershipOutcome::Refused(refusal));
        }
        let removed =
            sqlx::query("DELETE FROM conversation_users WHERE conversation_id=? AND user_id=?")
                .bind(conversation.as_str())
                .bind(viewer.as_str())
                .execute(&mut *tx)
                .await
                .map_err(failure)?
                .rows_affected()
                > 0;
        let state = channel_state(&mut tx, &item, viewer).await?;
        tx.commit().await.map_err(failure)?;
        let committed = removed
            .then(|| {
                changed(
                    &item,
                    ConversationMembershipChangeKind::SelfLeft,
                    None,
                    None,
                )
            })
            .transpose()?
            .map(Box::new);
        Ok(ChannelMembershipOutcome::Changed {
            channel: Box::new(state),
            committed,
        })
    }

    async fn preflight_remove_conversation_agent(
        &self,
        viewer: &UserId,
        conversation: &ConversationId,
        agent: &AgentId,
    ) -> Result<ConversationAgentRemovalPreflight, PersistenceFailure> {
        let mut connection = self.pool.acquire().await.map_err(failure)?;
        let Some(item) = facts(&mut *connection, conversation).await? else {
            return Ok(ConversationAgentRemovalPreflight::ConversationNotFound);
        };
        if item.roster != "open" {
            return Ok(ConversationAgentRemovalPreflight::Unsupported);
        }
        if !participates(
            &mut connection,
            &Principal::Human {
                user_id: viewer.clone(),
            },
            &item,
        )
        .await?
        {
            return Ok(ConversationAgentRemovalPreflight::Forbidden);
        }
        let Some(workspace) = &item.workspace else {
            return Ok(ConversationAgentRemovalPreflight::Unsupported);
        };
        let actor_role = role(&mut connection, workspace, viewer).await?;
        let row = sqlx::query("SELECT created_by_user_id,session_scope FROM agents WHERE id=?")
            .bind(agent.as_str())
            .fetch_optional(&mut *connection)
            .await
            .map_err(failure)?;
        let Some(row) = row else {
            return Ok(
                if actor_role.is_some_and(|r| r.meets_floor(WorkspaceRole::Admin)) {
                    ConversationAgentRemovalPreflight::CandidateNotFound
                } else {
                    ConversationAgentRemovalPreflight::Forbidden
                },
            );
        };
        let creator: String = row.try_get("created_by_user_id").map_err(failure)?;
        if !actor_role.is_some_and(|r| r.meets_floor(WorkspaceRole::Admin))
            && creator != viewer.as_str()
        {
            return Ok(ConversationAgentRemovalPreflight::Forbidden);
        }
        let member: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM conversation_agents WHERE conversation_id=? AND agent_id=?)")
            .bind(conversation.as_str())
            .bind(agent.as_str())
            .fetch_one(&mut *connection)
            .await
            .map_err(failure)?;
        let scope: String = row.try_get("session_scope").map_err(failure)?;
        Ok(ConversationAgentRemovalPreflight::Ready {
            stop_runtime: member && scope == "conversation",
        })
    }

    async fn remove_conversation_agent(
        &self,
        viewer: &UserId,
        conversation: &ConversationId,
        agent: &AgentId,
    ) -> Result<ConversationMemberMutation, PersistenceFailure> {
        let mut tx = self
            .pool
            .begin_with(super::super::ROOT_WRITE_BEGIN)
            .await
            .map_err(failure)?;
        let Some(item) = facts(&mut *tx, conversation).await? else {
            return Ok(ConversationMemberMutation::ConversationNotFound);
        };
        if item.roster != "open" {
            return Ok(ConversationMemberMutation::Unsupported);
        }
        if !participates(
            &mut tx,
            &Principal::Human {
                user_id: viewer.clone(),
            },
            &item,
        )
        .await?
        {
            return Ok(ConversationMemberMutation::Forbidden);
        }
        let Some(workspace) = &item.workspace else {
            return Ok(ConversationMemberMutation::Unsupported);
        };
        let role = role(&mut tx, workspace, viewer).await?;
        let creator: Option<String> =
            sqlx::query_scalar("SELECT created_by_user_id FROM agents WHERE id=?")
                .bind(agent.as_str())
                .fetch_optional(&mut *tx)
                .await
                .map_err(failure)?;
        let Some(creator) = creator else {
            return Ok(
                if role.is_some_and(|r| r.meets_floor(WorkspaceRole::Admin)) {
                    ConversationMemberMutation::CandidateNotFound
                } else {
                    ConversationMemberMutation::Forbidden
                },
            );
        };
        if !role.is_some_and(|r| r.meets_floor(WorkspaceRole::Admin)) && creator != viewer.as_str()
        {
            return Ok(ConversationMemberMutation::Forbidden);
        }
        let removed =
            sqlx::query("DELETE FROM conversation_agents WHERE conversation_id=? AND agent_id=?")
                .bind(conversation.as_str())
                .bind(agent.as_str())
                .execute(&mut *tx)
                .await
                .map_err(failure)?
                .rows_affected()
                > 0;
        let committed = if removed {
            let now: i64 = sqlx::query_scalar("SELECT CAST(unixepoch('subsec')*1000 AS INTEGER)")
                .fetch_one(&mut *tx)
                .await
                .map_err(failure)?;
            let principal = Principal::Human {
                user_id: viewer.clone(),
            };
            let notice =
                membership_audit(&mut tx, &principal, "agent", agent.as_str(), false).await?;
            let message = audit(&mut tx, &item, &notice, now).await?;
            Some(Box::new(changed(
                &item,
                ConversationMembershipChangeKind::MemberRemoved,
                Some(message),
                None,
            )?))
        } else {
            None
        };
        let roster = roster(&mut tx, conversation).await?;
        tx.commit().await.map_err(failure)?;
        Ok(ConversationMemberMutation::Changed { roster, committed })
    }

    async fn remove_conversation_human(
        &self,
        viewer: &UserId,
        conversation: &ConversationId,
        user: &UserId,
    ) -> Result<ConversationMemberMutation, PersistenceFailure> {
        let mut tx = self
            .pool
            .begin_with(super::super::ROOT_WRITE_BEGIN)
            .await
            .map_err(failure)?;
        let Some(item) = facts(&mut *tx, conversation).await? else {
            return Ok(ConversationMemberMutation::ConversationNotFound);
        };
        if !participates(
            &mut tx,
            &Principal::Human {
                user_id: viewer.clone(),
            },
            &item,
        )
        .await?
        {
            return Ok(ConversationMemberMutation::Forbidden);
        }
        if item.roster != "open" {
            return Ok(ConversationMemberMutation::Unsupported);
        }
        if viewer == user {
            return Ok(ConversationMemberMutation::SelfTarget);
        }
        let Some(workspace) = &item.workspace else {
            return Ok(ConversationMemberMutation::Unsupported);
        };
        let actor_role = role(&mut tx, workspace, viewer).await?;
        if !actor_role.is_some_and(|r| r.meets_floor(WorkspaceRole::Admin)) {
            return Ok(ConversationMemberMutation::Forbidden);
        }
        let target_role = role(&mut tx, workspace, user).await?;
        if matches!((target_role, actor_role), (Some(target), Some(actor)) if target.outranks(actor))
        {
            return Ok(ConversationMemberMutation::TargetOutranksActor);
        }
        let edge: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM conversation_users WHERE conversation_id=? AND user_id=?)",
        )
        .bind(conversation.as_str())
        .bind(user.as_str())
        .fetch_one(&mut *tx)
        .await
        .map_err(failure)?;
        if !edge {
            let roster = roster(&mut tx, conversation).await?;
            return Ok(ConversationMemberMutation::Changed {
                roster,
                committed: None,
            });
        }
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM conversation_users WHERE conversation_id=?")
                .bind(conversation.as_str())
                .fetch_one(&mut *tx)
                .await
                .map_err(failure)?;
        if let Some(refusal) = channel_exit_refusal(
            item.archived_at.is_some(),
            item.visibility == "private",
            target_role == Some(WorkspaceRole::Owner),
            count,
        ) {
            return Ok(match refusal {
                ChannelMembershipRefusal::ChannelArchived => {
                    ConversationMemberMutation::ChannelArchived
                }
                ChannelMembershipRefusal::ChannelOwner => ConversationMemberMutation::ChannelOwner,
                _ => ConversationMemberMutation::LastHumanMember,
            });
        }
        sqlx::query("DELETE FROM conversation_users WHERE conversation_id=? AND user_id=?")
            .bind(conversation.as_str())
            .bind(user.as_str())
            .execute(&mut *tx)
            .await
            .map_err(failure)?;
        let now: i64 = sqlx::query_scalar("SELECT CAST(unixepoch('subsec')*1000 AS INTEGER)")
            .fetch_one(&mut *tx)
            .await
            .map_err(failure)?;
        let principal = Principal::Human {
            user_id: viewer.clone(),
        };
        let notice = membership_audit(&mut tx, &principal, "human", user.as_str(), false).await?;
        let message = audit(&mut tx, &item, &notice, now).await?;
        let committed = Some(Box::new(changed(
            &item,
            ConversationMembershipChangeKind::MemberRemoved,
            Some(message),
            None,
        )?));
        let roster = roster(&mut tx, conversation).await?;
        tx.commit().await.map_err(failure)?;
        Ok(ConversationMemberMutation::Changed { roster, committed })
    }
}

async fn removability(
    executor: &mut SqliteConnection,
    principal: &Principal,
    item: &Facts,
    roster: &ConversationRoster,
) -> Result<Vec<MemberRemovability>, PersistenceFailure> {
    let mut out = Vec::with_capacity(roster.members.len());
    let Some(workspace) = &item.workspace else {
        return Ok(roster
            .members
            .iter()
            .map(|m| MemberRemovability {
                member_is_agent: matches!(m, ConversationMember::Agent { .. }),
                id: m.id().to_owned(),
                can_remove: false,
            })
            .collect());
    };
    let actor_role = actor_role(executor, principal, workspace).await?;
    let actor_user = match principal {
        Principal::Human { user_id } => Some(user_id),
        _ => None,
    };
    let human_count = roster
        .members
        .iter()
        .filter(|m| matches!(m, ConversationMember::Human { .. }))
        .count() as i64;
    for member in &roster.members {
        let can_remove = match member {
            ConversationMember::Agent { id, .. } => {
                let creator: Option<String> =
                    sqlx::query_scalar("SELECT created_by_user_id FROM agents WHERE id=?")
                        .bind(id.as_str())
                        .fetch_optional(&mut *executor)
                        .await
                        .map_err(failure)?;
                item.roster == "open"
                    && (actor_role.is_some_and(|r| r.meets_floor(WorkspaceRole::Admin))
                        || creator.as_deref() == actor_user.map(UserId::as_str))
            }
            ConversationMember::Human { id, .. } => {
                if actor_user == Some(id)
                    || !actor_role.is_some_and(|r| r.meets_floor(WorkspaceRole::Admin))
                {
                    false
                } else {
                    let target_role = role(executor, workspace, id).await?;
                    !matches!((target_role, actor_role), (Some(target), Some(actor)) if target.outranks(actor))
                        && channel_exit_refusal(
                            item.archived_at.is_some(),
                            item.visibility == "private",
                            target_role == Some(WorkspaceRole::Owner),
                            human_count,
                        )
                        .is_none()
                }
            }
        };
        out.push(MemberRemovability {
            member_is_agent: matches!(member, ConversationMember::Agent { .. }),
            id: member.id().to_owned(),
            can_remove,
        });
    }
    Ok(out)
}

impl<E: Clone + Send + Sync + 'static> ManageConversationMembersCapability for MessagingStore<E> {
    async fn may_manage_conversation_members(
        &self,
        agent: &AgentId,
        conversation: &ConversationId,
    ) -> Result<Result<(), ManageMembersDenied>, PersistenceFailure> {
        let granted: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM conversations c WHERE c.id=? AND (\
             (c.placement_kind='direct_pair' AND ((c.endpoint_a_kind='agent' AND c.endpoint_a_id=?) OR (c.endpoint_b_kind='agent' AND c.endpoint_b_id=?))) OR\
             (c.placement_kind='workspace' AND EXISTS(SELECT 1 FROM workspace_agent_capabilities cap WHERE cap.workspace_id=c.workspace_id AND cap.agent_id=? AND cap.capability=?))))",
        )
        .bind(conversation.as_str())
        .bind(agent.as_str())
        .bind(agent.as_str())
        .bind(agent.as_str())
        .bind(Capability::ManageConversationMembers.as_str())
        .fetch_one(&self.pool)
        .await
        .map_err(failure)?;
        Ok(if granted {
            Ok(())
        } else {
            Err(ManageMembersDenied)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::{Connection, Executor, SqliteConnection, sqlite::SqliteConnectOptions};

    async fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("membership.sqlite");
        let mut connection = crate::current_schema::create_current_schema(&path).await;
        connection.execute("INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES('owner','owner','Owner',1,1),('member','member','Member',1,1)").await.unwrap();
        connection
            .execute("INSERT INTO hosts(id,name,kind,created_at) VALUES('host','host','local',1)")
            .await
            .unwrap();
        connection
            .execute(seed_agent!(
                id = "agent",
                runtime = "codex",
                host_id = "host",
                created_by_user_id = "member",
                display_name = "Agent",
            ))
            .await
            .unwrap();
        connection.execute("INSERT INTO workspaces(id,name,name_key,created_at,updated_at,owner_user_id) VALUES('workspace','Workspace','workspace',1,1,'owner')").await.unwrap();
        connection.execute("INSERT INTO workspace_users(workspace_id,user_id,joined_at,role) VALUES('workspace','owner',1,'owner'),('workspace','member',2,'member')").await.unwrap();
        connection
            .execute(concat!(
                seed_channel!(
                    id = "public",
                    workspace_id = "workspace",
                    title = "Public",
                    visibility = "public",
                ),
                "; ",
                seed_channel!(
                    id = "private",
                    workspace_id = "workspace",
                    title = "Private",
                ),
            ))
            .await
            .unwrap();
        connection.execute("INSERT INTO conversation_users(conversation_id,user_id,joined_at) VALUES('public','owner',1),('private','member',1)").await.unwrap();
        connection.close().await.unwrap();
        (dir, path)
    }

    fn conversation(id: &str) -> ConversationId {
        ConversationId::parse(id).unwrap()
    }
    fn user(id: &str) -> UserId {
        UserId::parse(id).unwrap()
    }

    fn agent(id: &str) -> AgentId {
        AgentId::parse(id).unwrap()
    }

    #[tokio::test]
    async fn public_join_commits_edge_attention_and_read_baseline_together() {
        let (_dir, path) = fixture().await;
        let store = MessagingStore::open(&path).await.unwrap();
        let result = store
            .join_public_channel(&user("member"), &conversation("public"), 99)
            .await
            .unwrap();
        let ChannelMembershipOutcome::Changed {
            channel: detail,
            committed,
        } = result
        else {
            panic!("public join was refused");
        };
        assert_eq!(
            committed.as_ref().map(|fact| fact.kind),
            Some(ConversationMembershipChangeKind::SelfJoined)
        );
        assert!(
            (*detail)
                .bind(&user("member"), &conversation("public"))
                .unwrap()
                .is_member()
        );
        let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(path))
            .await
            .unwrap();
        for (table, query) in [
            (
                "conversation_users",
                "SELECT COUNT(*) FROM conversation_users WHERE user_id='member' AND conversation_id='public'",
            ),
            (
                "channel_attention_preferences",
                "SELECT COUNT(*) FROM channel_attention_preferences WHERE user_id='member' AND conversation_id='public'",
            ),
            (
                "conversation_reads",
                "SELECT COUNT(*) FROM conversation_reads WHERE user_id='member' AND conversation_id='public'",
            ),
        ] {
            let count: i64 = sqlx::query_scalar(query).fetch_one(&mut db).await.unwrap();
            assert_eq!(count, 1, "missing atomic join side in {table}");
        }
    }

    #[tokio::test]
    async fn private_last_human_leave_is_refused_without_touching_the_edge() {
        let (_dir, path) = fixture().await;
        let store = MessagingStore::open(&path).await.unwrap();
        let result = store
            .leave_channel(&user("member"), &conversation("private"))
            .await
            .unwrap();
        assert_eq!(
            result,
            ChannelMembershipOutcome::Refused(ChannelMembershipRefusal::LastHumanMember)
        );
        let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(path))
            .await
            .unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM conversation_users WHERE user_id='member' AND conversation_id='private'").fetch_one(&mut db).await.unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn own_agent_add_atomically_enrolls_seeds_and_places() {
        let (_dir, path) = fixture().await;
        let store = MessagingStore::open(&path).await.unwrap();
        store
            .join_public_channel(&user("member"), &conversation("public"), 99)
            .await
            .unwrap();
        let result = store
            .add_conversation_member(
                &Principal::Human {
                    user_id: user("member"),
                },
                &conversation("public"),
                ConversationMemberCandidate::Agent(AgentId::parse("agent").unwrap()),
                100,
            )
            .await
            .unwrap();
        assert!(matches!(
            result,
            ConversationMemberMutation::Changed {
                committed: Some(_),
                ..
            }
        ));
        assert_eq!(
            store
                .preflight_remove_conversation_agent(
                    &user("member"),
                    &conversation("public"),
                    &AgentId::parse("agent").unwrap(),
                )
                .await
                .unwrap(),
            ConversationAgentRemovalPreflight::Ready { stop_runtime: true }
        );
        let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(path))
            .await
            .unwrap();
        let workspace: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM workspace_members WHERE workspace_id='workspace' AND agent_id='agent'").fetch_one(&mut db).await.unwrap();
        let channel: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM conversation_agents WHERE conversation_id='public' AND agent_id='agent'").fetch_one(&mut db).await.unwrap();
        let capabilities: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM workspace_agent_capabilities WHERE workspace_id='workspace' AND agent_id='agent'").fetch_one(&mut db).await.unwrap();
        let audit: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE conversation_id='public' AND json_extract(metadata,'$.subtype')='conversation_member_added'").fetch_one(&mut db).await.unwrap();
        assert_eq!(
            (workspace, channel, capabilities, audit),
            (1, 1, DEFAULT_AGENT_CAPABILITIES.len() as i64, 1)
        );
        let notice = sqlx::query("SELECT content,json_extract(metadata,'$.actor_kind') actor_kind,json_extract(metadata,'$.member_kind') member_kind,json_extract(metadata,'$.member_id') member_id FROM messages WHERE conversation_id='public' AND json_extract(metadata,'$.subtype')='conversation_member_added'").fetch_one(&mut db).await.unwrap();
        assert_eq!(
            notice.get::<String, _>("content"),
            "<silverhand://human/member> added <silverhand://agent/agent> to this conversation."
        );
        assert_eq!(notice.get::<String, _>("actor_kind"), "user");
        assert_eq!(notice.get::<String, _>("member_kind"), "agent");
        assert_eq!(notice.get::<String, _>("member_id"), "agent");
    }

    #[tokio::test]
    async fn public_visibility_does_not_grant_roster_mutation() {
        let (_dir, path) = fixture().await;
        let store = MessagingStore::open(&path).await.unwrap();
        let result = store
            .add_conversation_member(
                &Principal::Human {
                    user_id: user("member"),
                },
                &conversation("public"),
                ConversationMemberCandidate::Agent(AgentId::parse("agent").unwrap()),
                100,
            )
            .await
            .unwrap();
        assert_eq!(result, ConversationMemberMutation::Forbidden);
    }

    #[tokio::test]
    async fn handle_resolution_is_caller_aware_and_preserves_ambiguity() {
        let (_dir, path) = fixture().await;
        let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
            .await
            .unwrap();
        // Preserve the resolver's compatibility contract for a database that
        // predates the current cross-table namespace trigger.
        db.execute("DROP TRIGGER agents_name_unique_against_users_insert")
            .await
            .unwrap();
        db.execute("INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES('same-human','same','Same Human',1,1)").await.unwrap();
        db.execute(seed_agent!(
            id = "same-agent",
            name = "same",
            runtime = "codex",
            host_id = "host",
            created_by_user_id = "member",
            display_name = "Same Agent",
            session_scope = "agent",
        ))
        .await
        .unwrap();
        db.execute("INSERT INTO workspace_members(workspace_id,agent_id,joined_at) VALUES('workspace','same-agent',1)").await.unwrap();
        db.execute("INSERT INTO conversation_users(conversation_id,user_id,joined_at) VALUES('public','member',2)").await.unwrap();
        db.close().await.unwrap();
        let store = MessagingStore::open(&path).await.unwrap();
        let handle = || {
            ConversationMemberCandidate::Handle(
                silverhand_backend_core::conversation_messaging::messaging::collaboration::membership::NormalizedMemberHandle::try_from("same".to_owned()).unwrap(),
            )
        };
        let owner_result = store
            .add_conversation_member(
                &Principal::Human {
                    user_id: user("owner"),
                },
                &conversation("public"),
                handle(),
                100,
            )
            .await
            .unwrap();
        assert!(matches!(
            owner_result,
            ConversationMemberMutation::HandleAmbiguous { candidates }
                if candidates.len() == 2
                    && candidates.iter().any(|row| matches!(row, AmbiguousMemberCandidate::Agent { .. }))
                    && candidates.iter().any(|row| matches!(row, AmbiguousMemberCandidate::Human { .. }))
        ));

        let member_result = store
            .add_conversation_member(
                &Principal::Human {
                    user_id: user("member"),
                },
                &conversation("public"),
                handle(),
                101,
            )
            .await
            .unwrap();
        assert!(matches!(
            member_result,
            ConversationMemberMutation::Changed { roster, .. }
                if roster.members.iter().any(|row| matches!(row, ConversationMember::Agent { id, .. } if id.as_str() == "same-agent"))
        ));
    }

    #[tokio::test]
    async fn agent_caller_can_place_existing_workspace_human_and_agent_but_cannot_enroll_human() {
        let (_dir, path) = fixture().await;
        let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
            .await
            .unwrap();
        db.execute("INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES('outside','outside','Outside',1,1)").await.unwrap();
        db.execute(seed_agent!(
            id = "peer",
            runtime = "codex",
            host_id = "host",
            created_by_user_id = "owner",
            display_name = "Peer",
            session_scope = "agent",
        ))
        .await
        .unwrap();
        db.execute("INSERT INTO workspace_members(workspace_id,agent_id,joined_at) VALUES('workspace','agent',1),('workspace','peer',2)").await.unwrap();
        db.execute("INSERT INTO conversation_agents(conversation_id,agent_id,joined_at) VALUES('public','agent',1)").await.unwrap();
        db.close().await.unwrap();
        let store = MessagingStore::open(&path).await.unwrap();
        let principal = Principal::Agent {
            agent_id: agent("agent"),
        };

        for candidate in [
            ConversationMemberCandidate::Human(user("member")),
            ConversationMemberCandidate::Agent(agent("peer")),
        ] {
            assert!(matches!(
                store
                    .add_conversation_member(&principal, &conversation("public"), candidate, 100)
                    .await
                    .unwrap(),
                ConversationMemberMutation::Changed {
                    committed: Some(_),
                    ..
                }
            ));
        }
        assert_eq!(
            store
                .add_conversation_member(
                    &principal,
                    &conversation("public"),
                    ConversationMemberCandidate::Human(user("outside")),
                    101,
                )
                .await
                .unwrap(),
            ConversationMemberMutation::CandidateNotWorkspaceMember
        );
        let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(path))
            .await
            .unwrap();
        let counts: (i64, i64, i64, i64) = (
            sqlx::query_scalar("SELECT COUNT(*) FROM workspace_users WHERE workspace_id='workspace' AND user_id='outside'").fetch_one(&mut db).await.unwrap(),
            sqlx::query_scalar("SELECT COUNT(*) FROM conversation_users WHERE conversation_id='public' AND user_id='outside'").fetch_one(&mut db).await.unwrap(),
            sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE conversation_id='public' AND json_extract(metadata,'$.member_id')='outside'").fetch_one(&mut db).await.unwrap(),
            sqlx::query_scalar("SELECT COUNT(*) FROM channel_attention_preferences WHERE conversation_id='public' AND user_id='outside'").fetch_one(&mut db).await.unwrap(),
        );
        assert_eq!(counts, (0, 0, 0, 0));
    }

    #[tokio::test]
    async fn agent_remove_classifies_fixed_roster_as_unsupported() {
        let (_dir, path) = fixture().await;
        let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
            .await
            .unwrap();
        db.execute(seed_direct_pair!(
            id = "fixed",
            endpoint_a_kind = "agent",
            endpoint_a_id = "agent",
            endpoint_b_kind = "human",
            endpoint_b_id = "member",
        ))
        .await
        .unwrap();
        db.close().await.unwrap();
        let store = MessagingStore::open(&path).await.unwrap();

        assert_eq!(
            store
                .preflight_remove_conversation_agent(
                    &user("owner"),
                    &conversation("fixed"),
                    &agent("agent"),
                )
                .await
                .unwrap(),
            ConversationAgentRemovalPreflight::Unsupported
        );
        assert_eq!(
            store
                .remove_conversation_agent(&user("owner"), &conversation("fixed"), &agent("agent"),)
                .await
                .unwrap(),
            ConversationMemberMutation::Unsupported
        );
    }
}
