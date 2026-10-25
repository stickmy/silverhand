use std::collections::HashSet;

use silverhand_backend_core::{
    conversation_messaging::messaging::collaboration::channel_directory::{
        ActiveChannelDirectoryPort, ActiveWorkspaceChannelDirectory, ArchivedChannelRow,
        ArchivedSince, ArchivedWorkspaceChannelDirectory, ArchivedWorkspaceChannelDirectoryPort,
        ChannelDirectoryProjectionError, ChannelDirectoryRow, ChannelSortEntry, ChannelSortMode,
        ChannelSortState, ChannelStatus, ChannelVisibility, DirectoryRoster,
        DirectoryWorkspaceKind, GlobalArchivedChannelDirectory, GlobalArchivedChannelDirectoryPort,
        GlobalArchivedDirectoryRefusal, WorkspaceDirectoryRefusal,
    },
    foundation::identity::{AgentId, ConversationId, UserId, WorkspaceId},
    foundation::persistence::PersistenceFailure,
};
use sqlx::{QueryBuilder, Row, Sqlite, Transaction, sqlite::SqliteRow};
use thiserror::Error;

use crate::conversation_messaging::messaging::MessagingStore;

use super::super::activity::valid_title_key_spelling;

#[derive(Clone)]
pub struct SqliteChannelDirectoryRead {
    pool: crate::foundation::database::SqliteDatabase,
}

impl<E> MessagingStore<E> {
    pub fn channel_directory(&self) -> SqliteChannelDirectoryRead {
        SqliteChannelDirectoryRead {
            pool: self.pool.clone(),
        }
    }
}

#[derive(Debug, Error)]
pub enum ChannelDirectoryStoreError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("stored workspace identity is malformed")]
    WorkspaceIdentity,
    #[error("stored workspace kind/owner relation is malformed")]
    WorkspaceKind,
    #[error("stored workspace name is malformed")]
    WorkspaceName,
    #[error("stored conversation identity is malformed")]
    ConversationIdentity,
    #[error("stored conversation placement is malformed")]
    ConversationPlacement,
    #[error("stored conversation title is malformed")]
    ConversationTitle,
    #[error("stored conversation status is malformed")]
    ConversationStatus,
    #[error("stored conversation visibility is malformed")]
    ConversationVisibility,
    #[error("stored spawned-agent identity is malformed")]
    SpawnedAgentIdentity,
    #[error("stored channel sort state is malformed")]
    SortState,
    #[error("channel-directory projection is internally inconsistent")]
    Projection(#[from] ChannelDirectoryProjectionError),
}

impl From<ChannelDirectoryStoreError> for PersistenceFailure {
    fn from(value: ChannelDirectoryStoreError) -> Self {
        Self::from_source(value)
    }
}

struct WorkspaceFact {
    id: WorkspaceId,
    name: String,
    archived_at: Option<i64>,
    kind: DirectoryWorkspaceKind,
}

impl ActiveChannelDirectoryPort for SqliteChannelDirectoryRead {
    type Error = PersistenceFailure;
    async fn active(
        &self,
        viewer: &UserId,
        workspace: &WorkspaceId,
    ) -> Result<Result<ActiveWorkspaceChannelDirectory, WorkspaceDirectoryRefusal>, Self::Error>
    {
        self.workspace_active(viewer, workspace)
            .await
            .map_err(PersistenceFailure::from)
    }
}

impl ArchivedWorkspaceChannelDirectoryPort for SqliteChannelDirectoryRead {
    type Error = PersistenceFailure;
    async fn archived(
        &self,
        viewer: &UserId,
        workspace: &WorkspaceId,
    ) -> Result<Result<ArchivedWorkspaceChannelDirectory, WorkspaceDirectoryRefusal>, Self::Error>
    {
        self.workspace_archived(viewer, workspace)
            .await
            .map_err(PersistenceFailure::from)
    }
}

impl GlobalArchivedChannelDirectoryPort for SqliteChannelDirectoryRead {
    type Error = PersistenceFailure;
    async fn archived_global(
        &self,
        viewer: &UserId,
        workspace_filter: Option<&WorkspaceId>,
        since: Option<ArchivedSince>,
    ) -> Result<Result<GlobalArchivedChannelDirectory, GlobalArchivedDirectoryRefusal>, Self::Error>
    {
        self.global_archived(viewer, workspace_filter, since)
            .await
            .map_err(PersistenceFailure::from)
    }
}

impl SqliteChannelDirectoryRead {
    #[tracing::instrument(
        target = "silverhand::diagnostics",
        name = "db.channels.workspace_active",
        skip_all,
        fields(
            elapsed_ms = tracing::field::Empty,
            outcome = tracing::field::Empty,
            operation_finished = tracing::field::Empty,
        )
    )]
    async fn workspace_active(
        &self,
        viewer: &UserId,
        requested: &WorkspaceId,
    ) -> Result<
        Result<ActiveWorkspaceChannelDirectory, WorkspaceDirectoryRefusal>,
        ChannelDirectoryStoreError,
    > {
        let observation = silverhand_backend_observability::Operation::current();
        let result = async {
            let mut tx =
                crate::foundation::statement_metrics::begin(&self.pool, Some("BEGIN DEFERRED"))
                    .await?;
            let Some(workspace) = load_workspace(&mut tx, viewer, requested).await? else {
                tx.commit().await?;
                return Ok(Err(WorkspaceDirectoryRefusal::NotFound));
            };
            if !viewer_is_workspace_member(&mut tx, viewer, &workspace.id).await? {
                tx.commit().await?;
                return Ok(Err(WorkspaceDirectoryRefusal::Forbidden));
            }
            let all = load_workspace_rows(&mut tx, viewer, &workspace, false).await?;
            let rows = all
                .into_iter()
                .filter(|row| match &workspace.kind {
                    DirectoryWorkspaceKind::Regular => {
                        row.is_member() || row.visibility() == ChannelVisibility::Public
                    }
                    DirectoryWorkspaceKind::AgentHome(_) => row.is_member(),
                })
                .collect::<Vec<_>>();
            let sort = load_sort(&mut tx, viewer, &workspace.id, &rows).await?;
            let result = ActiveWorkspaceChannelDirectory::projected(
                viewer.clone(),
                workspace.id.clone(),
                workspace.kind,
                rows,
                sort,
            )?;
            tx.commit().await?;
            Ok(Ok(result))
        }
        .await;
        observation.finish(&result);
        result
    }

    #[tracing::instrument(
        target = "silverhand::diagnostics",
        name = "db.channels.workspace_archived",
        skip_all,
        fields(
            elapsed_ms = tracing::field::Empty,
            outcome = tracing::field::Empty,
            operation_finished = tracing::field::Empty,
        )
    )]
    async fn workspace_archived(
        &self,
        viewer: &UserId,
        requested: &WorkspaceId,
    ) -> Result<
        Result<ArchivedWorkspaceChannelDirectory, WorkspaceDirectoryRefusal>,
        ChannelDirectoryStoreError,
    > {
        let observation = silverhand_backend_observability::Operation::current();
        let result = async {
            let mut tx =
                crate::foundation::statement_metrics::begin(&self.pool, Some("BEGIN DEFERRED"))
                    .await?;
            let Some(workspace) = load_workspace(&mut tx, viewer, requested).await? else {
                tx.commit().await?;
                return Ok(Err(WorkspaceDirectoryRefusal::NotFound));
            };
            if !viewer_is_workspace_member(&mut tx, viewer, &workspace.id).await? {
                tx.commit().await?;
                return Ok(Err(WorkspaceDirectoryRefusal::Forbidden));
            }
            let rows = load_workspace_rows(&mut tx, viewer, &workspace, true)
                .await?
                .into_iter()
                .filter(ChannelDirectoryRow::is_member)
                .collect();
            let result = ArchivedWorkspaceChannelDirectory::projected(
                viewer.clone(),
                workspace.id.clone(),
                workspace.kind,
                rows,
            )?;
            tx.commit().await?;
            Ok(Ok(result))
        }
        .await;
        observation.finish(&result);
        result
    }

    #[tracing::instrument(
        target = "silverhand::diagnostics",
        name = "db.channels.global_archived",
        skip_all,
        fields(
            elapsed_ms = tracing::field::Empty,
            outcome = tracing::field::Empty,
            operation_finished = tracing::field::Empty,
        )
    )]
    async fn global_archived(
        &self,
        viewer: &UserId,
        workspace_filter: Option<&WorkspaceId>,
        since: Option<ArchivedSince>,
    ) -> Result<
        Result<GlobalArchivedChannelDirectory, GlobalArchivedDirectoryRefusal>,
        ChannelDirectoryStoreError,
    > {
        let observation = silverhand_backend_observability::Operation::current();
        let result = async {
            let mut tx =
                crate::foundation::statement_metrics::begin(&self.pool, Some("BEGIN DEFERRED"))
                    .await?;
            if let Some(requested) = workspace_filter {
                let Some(workspace) = load_workspace(&mut tx, viewer, requested).await? else {
                    tx.commit().await?;
                    return Ok(Err(GlobalArchivedDirectoryRefusal::WorkspaceNotFound));
                };
                if !viewer_is_workspace_member(&mut tx, viewer, &workspace.id).await? {
                    tx.commit().await?;
                    return Ok(Err(GlobalArchivedDirectoryRefusal::Forbidden));
                }
            }
            if since.is_some_and(ArchivedSince::is_beyond_persisted_range) {
                let empty = GlobalArchivedChannelDirectory::projected(
                    viewer.clone(),
                    workspace_filter.cloned(),
                    Vec::new(),
                )?;
                tx.commit().await?;
                return Ok(Ok(empty));
            }
            let rows = load_global_rows(
                &mut tx,
                viewer,
                workspace_filter,
                since.and_then(ArchivedSince::persisted_value),
            )
            .await?;
            let result = GlobalArchivedChannelDirectory::projected(
                viewer.clone(),
                workspace_filter.cloned(),
                rows,
            )?;
            tx.commit().await?;
            Ok(Ok(result))
        }
        .await;
        observation.finish(&result);
        result
    }
}

async fn load_workspace(
    tx: &mut Transaction<'_, Sqlite>,
    _: &UserId,
    requested: &WorkspaceId,
) -> Result<Option<WorkspaceFact>, ChannelDirectoryStoreError> {
    let row = sqlx::query("SELECT w.id,w.name,w.name_key,w.archived_at,w.owner_agent_id,a.id AS owner_id,a.name AS owner_name FROM workspaces w LEFT JOIN agents a ON a.id=w.owner_agent_id WHERE w.id=?")
        .bind(requested.as_str()).fetch_optional(&mut **tx).await?;
    row.map(|row| decode_workspace(&row, requested)).transpose()
}

fn decode_workspace(
    row: &SqliteRow,
    requested: &WorkspaceId,
) -> Result<WorkspaceFact, ChannelDirectoryStoreError> {
    let id = WorkspaceId::parse(row.try_get::<String, _>("id")?)
        .map_err(|_| ChannelDirectoryStoreError::WorkspaceIdentity)?;
    if &id != requested {
        return Err(ChannelDirectoryStoreError::WorkspaceIdentity);
    }
    let name: String = row.try_get("name")?;
    if name.is_empty() {
        return Err(ChannelDirectoryStoreError::WorkspaceName);
    }
    let name_key: Option<String> = row.try_get("name_key")?;
    let owner: Option<String> = row.try_get("owner_agent_id")?;
    let owner_id: Option<String> = row.try_get("owner_id")?;
    let owner_name: Option<String> = row.try_get("owner_name")?;
    let kind = match (owner, name_key, owner_id, owner_name) {
        (None, Some(key), None, None) if valid_title_key_spelling(&key) => {
            DirectoryWorkspaceKind::Regular
        }
        (Some(owner), None, Some(joined), Some(name)) if owner == joined => {
            DirectoryWorkspaceKind::agent_home(
                AgentId::parse(owner).map_err(|_| ChannelDirectoryStoreError::WorkspaceKind)?,
                name,
            )?
        }
        _ => return Err(ChannelDirectoryStoreError::WorkspaceKind),
    };
    Ok(WorkspaceFact {
        id,
        name,
        archived_at: row.try_get("archived_at")?,
        kind,
    })
}

async fn viewer_is_workspace_member(
    tx: &mut Transaction<'_, Sqlite>,
    viewer: &UserId,
    workspace: &WorkspaceId,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workspace_users wu JOIN users u ON u.id=wu.user_id WHERE wu.workspace_id=? AND wu.user_id=?)")
        .bind(workspace.as_str()).bind(viewer.as_str()).fetch_one(&mut **tx).await
}

const ROW_COLUMNS_BEFORE_VIEWER: &str = "c.id,c.workspace_id,c.placement_kind,c.roster_policy,c.title,c.title_key,c.status,c.spawned_by_agent_id,c.closed_at,c.close_summary,c.archived_at,c.latest_message_seq,c.created_at,c.updated_at,c.visibility,c.endpoint_a_kind,c.endpoint_a_id,c.endpoint_a_deleted_at,c.endpoint_b_kind,c.endpoint_b_id,c.endpoint_b_deleted_at,CASE WHEN c.spawned_by_agent_id IS NULL THEN 1 ELSE EXISTS(SELECT 1 FROM agents spawned WHERE spawned.id=c.spawned_by_agent_id) END AS spawned_agent_valid,EXISTS(SELECT 1 FROM conversation_users cu WHERE cu.conversation_id=c.id AND cu.user_id=";
const ROW_COLUMNS_AFTER_VIEWER: &str = ") AS is_member";

async fn load_workspace_rows(
    tx: &mut Transaction<'_, Sqlite>,
    viewer: &UserId,
    workspace: &WorkspaceFact,
    archived: bool,
) -> Result<Vec<ChannelDirectoryRow>, ChannelDirectoryStoreError> {
    let mut query = QueryBuilder::<Sqlite>::new("SELECT ");
    query
        .push(ROW_COLUMNS_BEFORE_VIEWER)
        .push_bind(viewer.as_str())
        .push(ROW_COLUMNS_AFTER_VIEWER)
        .push(" FROM conversations c WHERE c.workspace_id=")
        .push_bind(workspace.id.as_str());
    match &workspace.kind {
        DirectoryWorkspaceKind::Regular => {
            query.push(" AND c.roster_policy='open'");
        }
        DirectoryWorkspaceKind::AgentHome(_) => {
            query.push(" AND c.roster_policy='fixed'");
        }
    }
    if archived {
        query.push(" AND c.archived_at IS NOT NULL");
    } else {
        query.push(" AND c.archived_at IS NULL");
    }
    let rows = query.build().fetch_all(&mut **tx).await?;
    rows.iter()
        .map(|row| decode_channel(row, viewer, &workspace.id))
        .collect()
}

/// The `channels:<ws>` list's live visible membership, resolved tx-bound so a
/// caller (the viewer-sort mutation writer) can re-derive it UNDER its own write
/// lock instead of trusting a stale pre-lock witness. It reuses the SAME
/// `load_workspace` / `viewer_is_workspace_member` / `load_workspace_rows` SQL the
/// pooled `active` read uses and applies the identical visibility filter, so the
/// two never diverge. Returns the visible conversation ids (the sort group) or the
/// existence/visibility refusal.
pub(crate) async fn resolve_active_channel_membership(
    tx: &mut Transaction<'_, Sqlite>,
    viewer: &UserId,
    workspace: &WorkspaceId,
) -> Result<Result<Vec<ConversationId>, WorkspaceDirectoryRefusal>, ChannelDirectoryStoreError> {
    let Some(workspace_fact) = load_workspace(tx, viewer, workspace).await? else {
        return Ok(Err(WorkspaceDirectoryRefusal::NotFound));
    };
    if !viewer_is_workspace_member(tx, viewer, &workspace_fact.id).await? {
        return Ok(Err(WorkspaceDirectoryRefusal::Forbidden));
    }
    let rows = load_workspace_rows(tx, viewer, &workspace_fact, false).await?;
    let visible = rows
        .into_iter()
        .filter(|row| match &workspace_fact.kind {
            DirectoryWorkspaceKind::Regular => {
                row.is_member() || row.visibility() == ChannelVisibility::Public
            }
            DirectoryWorkspaceKind::AgentHome(_) => row.is_member(),
        })
        .map(|row| row.conversation().clone())
        .collect();
    Ok(Ok(visible))
}

fn decode_channel(
    row: &SqliteRow,
    viewer: &UserId,
    workspace: &WorkspaceId,
) -> Result<ChannelDirectoryRow, ChannelDirectoryStoreError> {
    let id = ConversationId::parse(row.try_get::<String, _>("id")?)
        .map_err(|_| ChannelDirectoryStoreError::ConversationIdentity)?;
    let stored_workspace = WorkspaceId::parse(row.try_get::<String, _>("workspace_id")?)
        .map_err(|_| ChannelDirectoryStoreError::ConversationIdentity)?;
    if &stored_workspace != workspace {
        return Err(ChannelDirectoryStoreError::ConversationIdentity);
    }
    let placement: String = row.try_get("placement_kind")?;
    let title_key: Option<String> = row.try_get("title_key")?;
    let endpoints: [Option<String>; 6] = [
        row.try_get("endpoint_a_kind")?,
        row.try_get("endpoint_a_id")?,
        row.try_get::<Option<i64>, _>("endpoint_a_deleted_at")?
            .map(|v| v.to_string()),
        row.try_get("endpoint_b_kind")?,
        row.try_get("endpoint_b_id")?,
        row.try_get::<Option<i64>, _>("endpoint_b_deleted_at")?
            .map(|v| v.to_string()),
    ];
    if placement != "workspace"
        || !title_key.as_deref().is_some_and(valid_title_key_spelling)
        || endpoints.iter().any(Option::is_some)
    {
        return Err(ChannelDirectoryStoreError::ConversationPlacement);
    }
    let title: String = row.try_get("title")?;
    if title.is_empty() {
        return Err(ChannelDirectoryStoreError::ConversationTitle);
    }
    let roster = DirectoryRoster::parse(&row.try_get::<String, _>("roster_policy")?)
        .map_err(|_| ChannelDirectoryStoreError::ConversationPlacement)?;
    let status = ChannelStatus::parse(&row.try_get::<String, _>("status")?)
        .map_err(|_| ChannelDirectoryStoreError::ConversationStatus)?;
    let visibility = ChannelVisibility::parse(&row.try_get::<String, _>("visibility")?)
        .map_err(|_| ChannelDirectoryStoreError::ConversationVisibility)?;
    let spawned_by_agent = row
        .try_get::<Option<String>, _>("spawned_by_agent_id")?
        .map(AgentId::parse)
        .transpose()
        .map_err(|_| ChannelDirectoryStoreError::SpawnedAgentIdentity)?;
    if row.try_get::<i64, _>("spawned_agent_valid")? != 1 {
        return Err(ChannelDirectoryStoreError::SpawnedAgentIdentity);
    }
    ChannelDirectoryRow::projected(
        viewer.clone(),
        stored_workspace,
        id,
        roster,
        title,
        status,
        spawned_by_agent,
        row.try_get("closed_at")?,
        row.try_get("close_summary")?,
        row.try_get("archived_at")?,
        row.try_get("latest_message_seq")?,
        row.try_get("created_at")?,
        row.try_get("updated_at")?,
        visibility,
        row.try_get::<i64, _>("is_member")? == 1,
    )
    .map_err(Into::into)
}

async fn load_sort(
    tx: &mut Transaction<'_, Sqlite>,
    viewer: &UserId,
    workspace: &WorkspaceId,
    live: &[ChannelDirectoryRow],
) -> Result<ChannelSortState, ChannelDirectoryStoreError> {
    let list_key = format!("channels:{}", workspace.as_str());
    let mode_rows=sqlx::query("SELECT user_id,list_key,scope,mode FROM user_list_sort_mode WHERE user_id=? AND list_key=?").bind(viewer.as_str()).bind(&list_key).fetch_all(&mut **tx).await?;
    let mut mode = None;
    for row in mode_rows {
        if row.try_get::<String, _>("user_id")? != viewer.as_str()
            || row.try_get::<String, _>("list_key")? != list_key
            || !row.try_get::<String, _>("scope")?.is_empty()
            || mode.is_some()
        {
            return Err(ChannelDirectoryStoreError::SortState);
        }
        mode = Some(
            ChannelSortMode::parse(&row.try_get::<String, _>("mode")?)
                .map_err(|_| ChannelDirectoryStoreError::SortState)?,
        );
    }
    let entry_rows=sqlx::query("SELECT user_id,list_key,entity_id,rank,last_accessed_at,starred_at FROM user_sort_entry WHERE user_id=? AND list_key=?").bind(viewer.as_str()).bind(&list_key).fetch_all(&mut **tx).await?;
    let visible: HashSet<_> = live.iter().map(|row| row.conversation().as_str()).collect();
    let mut entries = Vec::new();
    for row in entry_rows {
        if row.try_get::<String, _>("user_id")? != viewer.as_str()
            || row.try_get::<String, _>("list_key")? != list_key
        {
            return Err(ChannelDirectoryStoreError::SortState);
        }
        let entity = ConversationId::parse(row.try_get::<String, _>("entity_id")?)
            .map_err(|_| ChannelDirectoryStoreError::SortState)?;
        let entry = ChannelSortEntry::projected(
            viewer.clone(),
            workspace.clone(),
            entity,
            row.try_get("rank")?,
            row.try_get("last_accessed_at")?,
            row.try_get("starred_at")?,
        );
        if visible.contains(entry.conversation().as_str()) {
            entries.push(entry);
        }
    }
    ChannelSortState::projected(viewer.clone(), workspace.clone(), mode, entries)
        .map_err(Into::into)
}

async fn load_global_rows(
    tx: &mut Transaction<'_, Sqlite>,
    viewer: &UserId,
    workspace_filter: Option<&WorkspaceId>,
    since: Option<i64>,
) -> Result<Vec<ArchivedChannelRow>, ChannelDirectoryStoreError> {
    let mut sql = QueryBuilder::<Sqlite>::new("SELECT ");
    sql.push(ROW_COLUMNS_BEFORE_VIEWER)
        .push_bind(viewer.as_str())
        .push(ROW_COLUMNS_AFTER_VIEWER)
        .push(",w.name AS workspace_name,w.name_key AS workspace_name_key,w.archived_at AS workspace_archived_at,w.owner_agent_id,a.id AS owner_id,a.name AS owner_name FROM conversations c LEFT JOIN workspaces w ON w.id=c.workspace_id LEFT JOIN agents a ON a.id=w.owner_agent_id WHERE c.placement_kind='workspace' AND c.archived_at IS NOT NULL AND EXISTS(SELECT 1 FROM conversation_users member WHERE member.conversation_id=c.id AND member.user_id=")
        .push_bind(viewer.as_str())
        .push(")");
    if let Some(workspace) = workspace_filter {
        sql.push(" AND c.workspace_id=")
            .push_bind(workspace.as_str());
    }
    if let Some(since) = since {
        sql.push(" AND c.archived_at>=").push_bind(since);
    }
    let rows = sql.build().fetch_all(&mut **tx).await?;
    rows.iter()
        .map(|row| {
            let workspace_raw: Option<String> = row.try_get("workspace_id")?;
            let workspace = WorkspaceId::parse(
                workspace_raw.ok_or(ChannelDirectoryStoreError::WorkspaceIdentity)?,
            )
            .map_err(|_| ChannelDirectoryStoreError::WorkspaceIdentity)?;
            let workspace_fact = decode_global_workspace(row, workspace.clone())?;
            let channel = decode_channel(row, viewer, &workspace)?;
            ArchivedChannelRow::projected(
                viewer.clone(),
                workspace,
                workspace_fact.kind,
                workspace_fact.name,
                workspace_fact.archived_at,
                channel,
            )
            .map_err(Into::into)
        })
        .collect()
}

fn decode_global_workspace(
    row: &SqliteRow,
    id: WorkspaceId,
) -> Result<WorkspaceFact, ChannelDirectoryStoreError> {
    let name: Option<String> = row.try_get("workspace_name")?;
    let name = name.ok_or(ChannelDirectoryStoreError::WorkspaceName)?;
    if name.is_empty() {
        return Err(ChannelDirectoryStoreError::WorkspaceName);
    }
    let name_key: Option<String> = row.try_get("workspace_name_key")?;
    let owner: Option<String> = row.try_get("owner_agent_id")?;
    let owner_id: Option<String> = row.try_get("owner_id")?;
    let owner_name: Option<String> = row.try_get("owner_name")?;
    let kind = match (owner, name_key, owner_id, owner_name) {
        (None, Some(key), None, None) if valid_title_key_spelling(&key) => {
            DirectoryWorkspaceKind::Regular
        }
        (Some(owner), None, Some(joined), Some(name)) if owner == joined => {
            DirectoryWorkspaceKind::agent_home(
                AgentId::parse(owner).map_err(|_| ChannelDirectoryStoreError::WorkspaceKind)?,
                name,
            )?
        }
        _ => return Err(ChannelDirectoryStoreError::WorkspaceKind),
    };
    Ok(WorkspaceFact {
        id,
        name,
        archived_at: row.try_get("workspace_archived_at")?,
        kind,
    })
}
