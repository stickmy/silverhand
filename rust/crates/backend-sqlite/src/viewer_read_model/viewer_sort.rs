//! The viewer-sort persistence adapter over `user_sort_entry` /
//! `user_list_sort_mode`.
//!
//! It is the write authority behind the sort/favorite mutation routes and the
//! generalized `ViewerSortState` read those routes echo. Each write primitive is
//! the byte-for-byte upsert its reference `SortRepo` statement performs; the
//! rules/scope derivation stay in the route + the core domain. The reorder is the
//! one multi-row primitive and runs as a single `BEGIN IMMEDIATE` transaction —
//! `order ⟹ manual` can never observe a torn state.

use silverhand_backend_core::foundation::identity::{ConversationId, WorkspaceId};
use silverhand_backend_core::{
    foundation::identity::UserId,
    foundation::persistence::PersistenceFailure,
    viewer_read_model::viewer_sort_state::{
        ChannelHome, ChannelHomeReadPort, ChannelHomeVerdict, ListGroupsRefusal,
        ResolvedListGroups, SortGroup, SortListKey, SortMode, SortMutation, SortMutationRefusal,
        ViewerSortEntryProjection, ViewerSortMutationWriter, ViewerSortStateProjection,
        decide_reorder, favorite_key,
    },
};
use sqlx::{Row, Sqlite, Transaction};
use std::collections::{BTreeMap, HashSet};
use thiserror::Error;

use crate::conversation_messaging::messaging::MessagingStore;
use crate::conversation_messaging::messaging::collaboration::channel_directory::resolve_active_channel_membership;

/// The write/read authority. A shared-pool clone of the one `MessagingStore`
/// pool; it opens no second connection authority and mints no ids (the sort
/// rows are keyed by `(user_id, list_key, entity_id)`), so it needs neither
/// entropy nor a domain clock beyond the wall-clock stamp the writes carry.
#[derive(Clone)]
pub struct SqliteViewerSort {
    pool: crate::foundation::database::SqliteDatabase,
    clock: SortClock,
}

/// The reserved-writer clock. Production reads the wall clock ONCE inside the write
/// transaction (after the lock is held and the fresh gates pass); a test can hold a
/// fixed value to prove the write + post-image are driven by that single sample.
#[derive(Clone, Copy)]
pub enum SortClock {
    System,
    Fixed(i64),
}

impl SortClock {
    fn now_ms(self) -> i64 {
        match self {
            Self::System => now_millis(),
            Self::Fixed(value) => value,
        }
    }
}

impl<E> MessagingStore<E> {
    /// The viewer-sort write/read authority, sharing this store's pool. Production
    /// uses the system reserved-writer clock.
    pub fn viewer_sort(&self) -> SqliteViewerSort {
        SqliteViewerSort {
            pool: self.pool().clone(),
            clock: SortClock::System,
        }
    }
}

#[derive(Debug, Error)]
pub enum ViewerSortStoreError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("stored sort mode is outside the closed set")]
    UnknownMode(String),
    #[error("stored channel home identity is malformed")]
    MalformedChannelHome,
    #[error("stored viewer-sort numeric is outside the JS-safe integer domain")]
    CorruptNumber,
}

impl From<ViewerSortStoreError> for PersistenceFailure {
    fn from(value: ViewerSortStoreError) -> Self {
        Self::from_source(value)
    }
}

/// The wall-clock millisecond stamp the write carries, mirroring the reference
/// `Date.now()`. Reading the clock is an adapter concern; there is no domain
/// value object because the column is a bare stored `INTEGER`.
fn now_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

impl ChannelHomeReadPort for SqliteViewerSort {
    async fn channel_home(
        &self,
        conversation_id: &str,
    ) -> Result<Option<ChannelHomeVerdict>, PersistenceFailure> {
        let Some(row) =
            sqlx::query("SELECT id, placement_kind, workspace_id FROM conversations WHERE id = ?")
                .bind(conversation_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(ViewerSortStoreError::from)?
        else {
            return Ok(None);
        };
        let id: String = row.try_get("id").map_err(ViewerSortStoreError::from)?;
        let placement: String = row
            .try_get("placement_kind")
            .map_err(ViewerSortStoreError::from)?;
        let workspace_id: Option<String> = row
            .try_get("workspace_id")
            .map_err(ViewerSortStoreError::from)?;
        match (placement.as_str(), workspace_id) {
            ("workspace", Some(workspace_id)) => {
                let conversation = ConversationId::parse(id)
                    .map_err(|_| ViewerSortStoreError::MalformedChannelHome)?;
                let workspace = WorkspaceId::parse(workspace_id)
                    .map_err(|_| ViewerSortStoreError::MalformedChannelHome)?;
                Ok(Some(ChannelHomeVerdict::WorkspacePlaced(ChannelHome {
                    conversation,
                    workspace,
                })))
            }
            ("direct_pair", None) => Ok(Some(ChannelHomeVerdict::DirectPair)),
            _ => Err(ViewerSortStoreError::MalformedChannelHome.into()),
        }
    }
}

/// The list's live membership resolved TX-BOUND under the write lock: each visible
/// entity id mapped to its group, plus the star-exclusion set. Runs the SAME
/// `resolveListGroups` SQL the pooled reads use but on the caller's writer
/// transaction, so the decision cannot go stale between resolve and write.
async fn resolve_list_groups_tx(
    tx: &mut Transaction<'_, Sqlite>,
    viewer: &UserId,
    list_key: &SortListKey,
) -> Result<Result<ResolvedListGroups, ListGroupsRefusal>, PersistenceFailure> {
    match list_key {
        SortListKey::Sidebar => {
            let ids = sidebar_ids_tx(tx, viewer).await?;
            Ok(Ok(ResolvedListGroups {
                groups: ids.into_iter().map(|id| (id, SortGroup::Sidebar)).collect(),
                starred: HashSet::new(),
            }))
        }

        SortListKey::Favorites => {
            // The favorites list IS the starred set — membership is the projected
            // conversation-favorites set, keyed by favorite_key.
            let rows = sqlx::query(SELECT_FAVORITE_KEYS_TX)
                .bind(viewer.as_str())
                .bind(viewer.as_str())
                .bind(viewer.as_str())
                .bind(viewer.as_str())
                .bind(viewer.as_str())
                .bind(viewer.as_str())
                .fetch_all(&mut **tx)
                .await
                .map_err(ViewerSortStoreError::from)?;
            let mut groups = BTreeMap::new();
            for row in &rows {
                let entity_id: String = row
                    .try_get("entity_id")
                    .map_err(ViewerSortStoreError::from)?;
                groups.insert(favorite_key(&entity_id), SortGroup::Favorites);
            }
            Ok(Ok(ResolvedListGroups {
                groups,
                starred: HashSet::new(),
            }))
        }
        SortListKey::DirectPairs => {
            // Star-only list: its members are the pairs the viewer is the human
            // endpoint of; the group value is never consulted.
            let rows = sqlx::query(SELECT_VIEWER_DIRECT_PAIR_IDS_TX)
                .bind(viewer.as_str())
                .bind(viewer.as_str())
                .fetch_all(&mut **tx)
                .await
                .map_err(ViewerSortStoreError::from)?;
            let mut groups = BTreeMap::new();
            for row in &rows {
                let id: String = row.try_get("id").map_err(ViewerSortStoreError::from)?;
                groups.insert(id, SortGroup::Channels);
            }
            Ok(Ok(ResolvedListGroups {
                groups,
                starred: HashSet::new(),
            }))
        }
        SortListKey::Channels(workspace) => {
            // An empty/invalid workspace suffix names no workspace — the TS
            // `workspaces.get('') → null → 404`, never an admission failure.
            let Ok(workspace) = WorkspaceId::parse(workspace.clone()) else {
                return Ok(Err(ListGroupsRefusal::NotFound));
            };
            match resolve_active_channel_membership(tx, viewer, &workspace)
                .await
                .map_err(PersistenceFailure::from_source)?
            {
                Ok(conversations) => {
                    let mut groups = BTreeMap::new();
                    for conversation in &conversations {
                        groups.insert(conversation.as_str().to_owned(), SortGroup::Channels);
                    }
                    // Live starred members: the stored starred entries for this list,
                    // filtered to the visible group (a stale star for a gone
                    // conversation must not enter the exclusion set).
                    let mut starred = HashSet::new();
                    let starred_rows = sqlx::query(SELECT_STARRED_ENTITY_IDS_TX)
                        .bind(viewer.as_str())
                        .bind(list_key.as_stored())
                        .fetch_all(&mut **tx)
                        .await
                        .map_err(ViewerSortStoreError::from)?;
                    for row in &starred_rows {
                        let id: String =
                            row.try_get("entity_id").map_err(ViewerSortStoreError::from)?;
                        if groups.contains_key(&id) {
                            starred.insert(id);
                        }
                    }
                    Ok(Ok(ResolvedListGroups { groups, starred }))
                }
                Err(silverhand_backend_core::conversation_messaging::messaging::collaboration::channel_directory::WorkspaceDirectoryRefusal::NotFound) => {
                    Ok(Err(ListGroupsRefusal::NotFound))
                }
                Err(silverhand_backend_core::conversation_messaging::messaging::collaboration::channel_directory::WorkspaceDirectoryRefusal::Forbidden) => {
                    Ok(Err(ListGroupsRefusal::Forbidden))
                }
            }
        }
    }
}

const SELECT_FAVORITE_KEYS_TX: &str = "\
    SELECT use.entity_id AS entity_id \
      FROM user_sort_entry use \
      JOIN conversations c ON c.id = use.entity_id \
      JOIN workspaces w ON w.id = c.workspace_id \
      JOIN workspace_users wu ON wu.workspace_id = w.id AND wu.user_id = ? \
     WHERE use.user_id = ? \
       AND use.list_key = 'channels:' || c.workspace_id \
       AND use.starred_at IS NOT NULL \
       AND c.archived_at IS NULL \
       AND w.archived_at IS NULL \
       AND ( \
         EXISTS (SELECT 1 FROM conversation_users cu \
                  WHERE cu.conversation_id = c.id AND cu.user_id = ?) \
         OR (c.placement_kind = 'workspace' AND c.roster_policy = 'open' \
             AND c.visibility = 'public') \
       ) \
    UNION ALL \
    SELECT use.entity_id AS entity_id \
      FROM user_sort_entry use \
      JOIN conversations c ON c.id = use.entity_id AND c.placement_kind = 'direct_pair' \
     WHERE use.user_id = ? \
       AND use.list_key = 'direct_pairs' \
       AND use.starred_at IS NOT NULL \
       AND ((c.endpoint_a_kind = 'human' AND c.endpoint_a_id = ?) \
         OR (c.endpoint_b_kind = 'human' AND c.endpoint_b_id = ?))";

const SELECT_VIEWER_DIRECT_PAIR_IDS_TX: &str = "\
    SELECT c.id AS id FROM conversations c \
     WHERE c.placement_kind = 'direct_pair' \
       AND ((c.endpoint_a_kind = 'human' AND c.endpoint_a_id = ?) \
         OR (c.endpoint_b_kind = 'human' AND c.endpoint_b_id = ?))";

const SELECT_STARRED_ENTITY_IDS_TX: &str = "\
    SELECT entity_id FROM user_sort_entry \
     WHERE user_id = ? AND list_key = ? AND starred_at IS NOT NULL";

/// Read the committed post-image tx-bound: the modes + the entries filtered to the
/// caller's live visible set, on the SAME transaction that wrote them — so the echo
/// is one authorized instant, never a new-mode/old-entries mix from two pool reads.
/// JS-safe integer gate for an OPTIONAL viewer-sort numeric at store->projection
/// (#48): a stored value JS can't represent exactly fails closed.
fn vs_js_safe(value: Option<i64>) -> Result<Option<i64>, ViewerSortStoreError> {
    match value {
        None => Ok(None),
        Some(v) => silverhand_backend_core::foundation::wire_number::JsSafeInteger::new(v)
            .map(|n| Some(n.get()))
            .map_err(|_| ViewerSortStoreError::CorruptNumber),
    }
}

async fn read_post_image_tx(
    tx: &mut Transaction<'_, Sqlite>,
    viewer: &UserId,
    list_key: &SortListKey,
    visible_entity_ids: &[String],
) -> Result<ViewerSortStateProjection, PersistenceFailure> {
    if matches!(list_key, SortListKey::Sidebar) {
        return sidebar_projection_tx(tx, viewer).await;
    }
    let stored_list_key = list_key.as_stored();
    let mode_rows = sqlx::query(
        "SELECT scope, mode FROM user_list_sort_mode WHERE user_id = ? AND list_key = ?",
    )
    .bind(viewer.as_str())
    .bind(&stored_list_key)
    .fetch_all(&mut **tx)
    .await
    .map_err(ViewerSortStoreError::from)?;
    let mut modes = BTreeMap::new();
    for row in &mode_rows {
        let scope: String = row.try_get("scope").map_err(ViewerSortStoreError::from)?;
        let mode_raw: String = row.try_get("mode").map_err(ViewerSortStoreError::from)?;
        let mode = SortMode::parse(&mode_raw).ok_or(ViewerSortStoreError::UnknownMode(mode_raw))?;
        modes.insert(scope, mode);
    }
    let visible: HashSet<&str> = visible_entity_ids.iter().map(String::as_str).collect();
    // Favorites projection: the entries are NOT the `list_key='favorites'`
    // rank rows. Each LIVE favorite gets an entry; `rank` comes from the favorites
    // list (null when never manually sorted), while `last_accessed_at`/`starred_at`
    // come from the conversation's HOME list (`channels:<ws>` / `direct_pairs`), where
    // the star lives.
    // A raw `user_sort_entry WHERE list_key='favorites'` read would drop favorites
    // that lack a rank row AND null out the home facts.
    if matches!(list_key, SortListKey::Favorites) {
        let ranks = sqlx::query(
            "SELECT entity_id, rank FROM user_sort_entry \
             WHERE user_id = ? AND list_key = 'favorites' AND rank IS NOT NULL",
        )
        .bind(viewer.as_str())
        .fetch_all(&mut **tx)
        .await
        .map_err(ViewerSortStoreError::from)?;
        let mut rank_by_id: BTreeMap<String, Option<i64>> = BTreeMap::new();
        for row in &ranks {
            let id: String = row
                .try_get("entity_id")
                .map_err(ViewerSortStoreError::from)?;
            rank_by_id.insert(id, row.try_get("rank").map_err(ViewerSortStoreError::from)?);
        }
        // The live favorites = the viewer's HOME-list starred conversations.
        let home_rows = sqlx::query(
            "SELECT use.entity_id AS entity_id, use.last_accessed_at AS last_accessed_at, \
                    use.starred_at AS starred_at \
             FROM user_sort_entry use JOIN conversations c ON c.id = use.entity_id \
             WHERE use.user_id = ? \
               AND (use.list_key = 'channels:' || c.workspace_id \
                 OR (use.list_key = 'direct_pairs' AND c.placement_kind = 'direct_pair')) \
               AND use.starred_at IS NOT NULL",
        )
        .bind(viewer.as_str())
        .fetch_all(&mut **tx)
        .await
        .map_err(ViewerSortStoreError::from)?;
        let mut entries = Vec::new();
        for row in &home_rows {
            let home_entity_id: String = row
                .try_get("entity_id")
                .map_err(ViewerSortStoreError::from)?;
            // The favorites list keys entries by `favorite_key` (matching the resolved
            // group keys + the wire), not the raw conversation id.
            let key = silverhand_backend_core::viewer_read_model::viewer_sort_state::favorite_key(
                &home_entity_id,
            );
            if !visible.contains(key.as_str()) {
                continue;
            }
            let rank = rank_by_id.get(&key).copied().flatten();
            entries.push(ViewerSortEntryProjection {
                rank: vs_js_safe(rank)?,
                last_accessed_at: vs_js_safe(
                    row.try_get("last_accessed_at")
                        .map_err(ViewerSortStoreError::from)?,
                )?,
                starred_at: vs_js_safe(
                    row.try_get("starred_at")
                        .map_err(ViewerSortStoreError::from)?,
                )?,
                entity_id: key,
            });
        }
        return Ok(ViewerSortStateProjection {
            list_key: list_key.clone(),
            modes,
            entries,
        });
    }
    let entry_rows = sqlx::query(
        "SELECT entity_id, rank, last_accessed_at, starred_at \
         FROM user_sort_entry WHERE user_id = ? AND list_key = ?",
    )
    .bind(viewer.as_str())
    .bind(&stored_list_key)
    .fetch_all(&mut **tx)
    .await
    .map_err(ViewerSortStoreError::from)?;
    let mut entries = Vec::new();
    for row in &entry_rows {
        let entity_id: String = row
            .try_get("entity_id")
            .map_err(ViewerSortStoreError::from)?;
        if !visible.contains(entity_id.as_str()) {
            continue;
        }
        entries.push(ViewerSortEntryProjection {
            entity_id,
            rank: vs_js_safe(row.try_get("rank").map_err(ViewerSortStoreError::from)?)?,
            last_accessed_at: vs_js_safe(
                row.try_get("last_accessed_at")
                    .map_err(ViewerSortStoreError::from)?,
            )?,
            starred_at: vs_js_safe(
                row.try_get("starred_at")
                    .map_err(ViewerSortStoreError::from)?,
            )?,
        });
    }
    Ok(ViewerSortStateProjection {
        list_key: list_key.clone(),
        modes,
        entries,
    })
}

impl ViewerSortMutationWriter for SqliteViewerSort {
    async fn apply_sort_mutation(
        &self,
        viewer: &UserId,
        list_key: &SortListKey,
        mutation: &SortMutation,
    ) -> Result<Result<ViewerSortStateProjection, SortMutationRefusal>, PersistenceFailure> {
        // ONE connection-bound writer transaction. The clock is sampled ONCE, after
        // the lock is held and the fresh gates pass, and drives this write + its
        // post-image. Every decision fact is re-resolved here, never a pre-lock
        // witness; a refusal rolls back with no write.

        if matches!(list_key, SortListKey::Sidebar)
            && !matches!(mutation, SortMutation::Reorder { .. })
        {
            return Ok(Err(SortMutationRefusal::ReorderOnly));
        }
        // `direct_pairs` is internal and star-only; the wire grammar never names it.
        if matches!(list_key, SortListKey::DirectPairs)
            && !matches!(mutation, SortMutation::SetStarred { .. })
        {
            return Ok(Err(SortMutationRefusal::ListNotFound));
        }
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(ViewerSortStoreError::from)?;

        let resolved = match resolve_list_groups_tx(&mut tx, viewer, list_key).await? {
            Ok(resolved) => resolved,
            Err(ListGroupsRefusal::NotFound) => {
                return Ok(Err(SortMutationRefusal::ListNotFound));
            }
            Err(ListGroupsRefusal::Forbidden) => {
                return Ok(Err(SortMutationRefusal::Forbidden));
            }
        };

        // The reserved-writer clock, sampled ONCE here — the lock is held and the
        // fresh gates above passed — driving this write and its post-image.
        let now = self.clock.now_ms();
        let stored_list_key = list_key.as_stored();

        match mutation {
            SortMutation::Touch { entity_id } => {
                if matches!(list_key, SortListKey::Favorites) {
                    return Ok(Err(SortMutationRefusal::FavoritesNotTouchable));
                }
                if !resolved.groups.contains_key(entity_id) {
                    return Ok(Err(SortMutationRefusal::UnknownEntity));
                }
                sqlx::query(
                    "INSERT INTO user_sort_entry (user_id, list_key, entity_id, rank, last_accessed_at) \
                     VALUES (?, ?, ?, NULL, ?) \
                     ON CONFLICT(user_id, list_key, entity_id) DO UPDATE SET \
                       last_accessed_at = excluded.last_accessed_at",
                )
                .bind(viewer.as_str())
                .bind(&stored_list_key)
                .bind(entity_id)
                .bind(now)
                .execute(&mut *tx)
                .await
                .map_err(ViewerSortStoreError::from)?;
            }
            SortMutation::SetMode { group, mode } => {
                // Group/list consistency under the lock.
                match list_key {
                    SortListKey::Sidebar
                    | SortListKey::Favorites
                    | SortListKey::Channels(_)
                    | SortListKey::DirectPairs => {
                        if group.is_some() {
                            return Ok(Err(SortMutationRefusal::GroupNotSupportedForList {
                                group: *group,
                            }));
                        }
                    }
                }
                let scope = list_key.mode_scope();
                sqlx::query(
                    "INSERT INTO user_list_sort_mode (user_id, list_key, scope, mode, updated_at) \
                     VALUES (?, ?, ?, ?, ?) \
                     ON CONFLICT(user_id, list_key, scope) DO UPDATE SET \
                       mode = excluded.mode, updated_at = excluded.updated_at",
                )
                .bind(viewer.as_str())
                .bind(&stored_list_key)
                .bind(scope)
                .bind(mode.as_str())
                .bind(now)
                .execute(&mut *tx)
                .await
                .map_err(ViewerSortStoreError::from)?;
            }
            SortMutation::Reorder { group, ordered_ids } => {
                // group-valid-for-list is a route-level (list-only) hint; the
                // authority here is the live group set + completeness under the lock.
                if !list_key.legal_groups().contains(group) {
                    return Ok(Err(SortMutationRefusal::GroupNotValidForList {
                        group: Some(*group),
                    }));
                }
                let live_group_ids: Vec<String> = resolved
                    .groups
                    .iter()
                    .filter(|(id, entity_group)| {
                        **entity_group == *group && !resolved.starred.contains(id.as_str())
                    })
                    .map(|(id, _)| id.clone())
                    .collect();
                use silverhand_backend_core::viewer_read_model::viewer_sort_state::ReorderVerdict;
                match decide_reorder(&live_group_ids, ordered_ids) {
                    ReorderVerdict::Accepted => {}
                    ReorderVerdict::IdNotInGroup { id } => {
                        if matches!(list_key, SortListKey::Sidebar) {
                            return Ok(Err(SortMutationRefusal::SidebarSetChanged));
                        }
                        return Ok(Err(SortMutationRefusal::IdNotInGroup { id }));
                    }
                    ReorderVerdict::NotExactMembers { expected_count } => {
                        if matches!(list_key, SortListKey::Sidebar) {
                            return Ok(Err(SortMutationRefusal::SidebarSetChanged));
                        }
                        return Ok(Err(SortMutationRefusal::NotExactMembers { expected_count }));
                    }
                }
                let scope = list_key.mode_scope();
                sqlx::query(
                    "INSERT INTO user_list_sort_mode (user_id, list_key, scope, mode, updated_at) \
                     VALUES (?, ?, ?, 'manual', ?) \
                     ON CONFLICT(user_id, list_key, scope) DO UPDATE SET \
                       mode = 'manual', updated_at = excluded.updated_at",
                )
                .bind(viewer.as_str())
                .bind(&stored_list_key)
                .bind(scope)
                .bind(now)
                .execute(&mut *tx)
                .await
                .map_err(ViewerSortStoreError::from)?;
                for (position, entity_id) in ordered_ids.iter().enumerate() {
                    sqlx::query(
                        "INSERT INTO user_sort_entry (user_id, list_key, entity_id, rank, last_accessed_at) \
                         VALUES (?, ?, ?, ?, NULL) \
                         ON CONFLICT(user_id, list_key, entity_id) DO UPDATE SET rank = excluded.rank",
                    )
                    .bind(viewer.as_str())
                    .bind(&stored_list_key)
                    .bind(entity_id)
                    .bind(i64::try_from(position).unwrap_or(i64::MAX))
                    .execute(&mut *tx)
                    .await
                    .map_err(ViewerSortStoreError::from)?;
                }
            }
            SortMutation::SetStarred { entity_id, starred } => {
                // The star target must be a live visible member of the channels list.
                if !resolved.groups.contains_key(entity_id) {
                    return Ok(Err(SortMutationRefusal::UnknownEntity));
                }
                if *starred {
                    sqlx::query(
                        "INSERT INTO user_sort_entry (user_id, list_key, entity_id, rank, last_accessed_at, starred_at) \
                         VALUES (?, ?, ?, NULL, NULL, ?) \
                         ON CONFLICT(user_id, list_key, entity_id) DO UPDATE SET starred_at = excluded.starred_at",
                    )
                    .bind(viewer.as_str())
                    .bind(&stored_list_key)
                    .bind(entity_id)
                    .bind(now)
                    .execute(&mut *tx)
                    .await
                    .map_err(ViewerSortStoreError::from)?;
                } else {
                    sqlx::query(
                        "UPDATE user_sort_entry SET starred_at = NULL \
                         WHERE user_id = ? AND list_key = ? AND entity_id = ?",
                    )
                    .bind(viewer.as_str())
                    .bind(&stored_list_key)
                    .bind(entity_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(ViewerSortStoreError::from)?;
                    // Unstar is a voluntary favorites removal → drop the favorites orphan.
                    sqlx::query(
                        "DELETE FROM user_sort_entry \
                         WHERE user_id = ? AND list_key = 'favorites' AND entity_id = ?",
                    )
                    .bind(viewer.as_str())
                    .bind(favorite_key(entity_id))
                    .execute(&mut *tx)
                    .await
                    .map_err(ViewerSortStoreError::from)?;
                }
            }
        }

        // The committed post-image, read on the SAME transaction against the SAME
        // live visible set the decision used.
        let visible: Vec<String> = resolved.groups.keys().cloned().collect();
        let projection = read_post_image_tx(&mut tx, viewer, list_key, &visible).await?;
        tx.commit().await.map_err(ViewerSortStoreError::from)?;
        Ok(Ok(projection))
    }
}

async fn sidebar_ids_tx(
    tx: &mut Transaction<'_, Sqlite>,
    viewer: &UserId,
) -> Result<Vec<String>, PersistenceFailure> {
    let rows = sqlx::query(
        "SELECT w.id
         FROM workspaces w
         WHERE w.archived_at IS NULL
           AND EXISTS (
               SELECT 1 FROM workspace_users wu
               WHERE wu.workspace_id = w.id AND wu.user_id = ?
           )
         ORDER BY (w.owner_agent_id IS NOT NULL), w.created_at, w.id",
    )
    .bind(viewer.as_str())
    .fetch_all(&mut **tx)
    .await
    .map_err(ViewerSortStoreError::from)?;

    let mut ids = vec![
        "section:starred".to_owned(),
        "section:direct_messages".to_owned(),
    ];
    for row in rows {
        ids.push(format!(
            "workspace:{}",
            row.try_get::<String, _>("id")
                .map_err(ViewerSortStoreError::from)?
        ));
    }
    Ok(ids)
}

async fn sidebar_projection_tx(
    tx: &mut Transaction<'_, Sqlite>,
    viewer: &UserId,
) -> Result<ViewerSortStateProjection, PersistenceFailure> {
    let ids = sidebar_ids_tx(tx, viewer).await?;
    let rows = sqlx::query(
        "SELECT entity_id,rank FROM user_sort_entry WHERE user_id=? AND list_key='sidebar'",
    )
    .bind(viewer.as_str())
    .fetch_all(&mut **tx)
    .await
    .map_err(ViewerSortStoreError::from)?;
    let mut ranks = BTreeMap::new();
    for row in rows {
        ranks.insert(
            row.try_get::<String, _>("entity_id")
                .map_err(ViewerSortStoreError::from)?,
            vs_js_safe(row.try_get("rank").map_err(ViewerSortStoreError::from)?)?,
        );
    }
    let mut entries: Vec<_> = ids
        .into_iter()
        .map(|entity_id| ViewerSortEntryProjection {
            rank: ranks.get(&entity_id).copied().flatten(),
            entity_id,
            last_accessed_at: None,
            starred_at: None,
        })
        .collect();
    // Stable sort preserves the explicit default order for unranked/tied rows.
    entries.sort_by_key(|e| (e.rank.is_none(), e.rank));
    Ok(ViewerSortStateProjection {
        list_key: SortListKey::Sidebar,
        modes: BTreeMap::from([("".to_owned(), SortMode::Manual)]),
        entries,
    })
}

impl silverhand_backend_core::viewer_read_model::viewer_sort_state::ViewerSidebarRead
    for SqliteViewerSort
{
    async fn sidebar_sort(
        &self,
        viewer: &UserId,
    ) -> Result<ViewerSortStateProjection, PersistenceFailure> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(ViewerSortStoreError::from)?;
        let result = sidebar_projection_tx(&mut tx, viewer).await?;
        tx.commit().await.map_err(ViewerSortStoreError::from)?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use silverhand_backend_core::viewer_read_model::viewer_sort_state::SortGroup;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use sqlx::{Connection, Executor, SqliteConnection};

    const CURRENT_SCHEMA: &str =
        include_str!("../../../../../rust/test-fixtures/migration-baseline/schema-current.sql");

    // A regular workspace `ws-1` (user-1 owner) with two OPEN public channels the
    // viewer can see. `conv-a`,`conv-b` are the live `channels:ws-1` group.
    const SEED_SQL: &str = "\
INSERT INTO users (id, name, display_name, created_at, updated_at) VALUES ('user-1','u','U',1,1); \
INSERT INTO workspaces (id, name, name_key, description, owner_agent_id, owner_user_id, archived_at, created_at, updated_at) \
  VALUES ('ws-1','W','w',NULL,NULL,'user-1',NULL,1,1); \
INSERT INTO workspace_users (workspace_id, user_id, joined_at, role) VALUES ('ws-1','user-1',1,'owner'); \
INSERT INTO conversations (id, workspace_id, placement_kind, roster_policy, title, title_key, status, visibility, created_at, updated_at) \
  VALUES ('conv-a','ws-1','workspace','open','A','a','idle','public',1,1); \
INSERT INTO conversations (id, workspace_id, placement_kind, roster_policy, title, title_key, status, visibility, created_at, updated_at) \
  VALUES ('conv-b','ws-1','workspace','open','B','b','idle','public',1,1);";

    async fn open_seeded() -> (tempfile::TempDir, SqliteViewerSort, sqlx::SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("viewer-sort.sqlite");
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .foreign_keys(true);
        let mut db = SqliteConnection::connect_with(&options).await.unwrap();
        db.execute(sqlx::raw_sql(CURRENT_SCHEMA)).await.unwrap();
        db.execute(sqlx::raw_sql(SEED_SQL)).await.unwrap();
        db.close().await.unwrap();
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&path)
                    .foreign_keys(true),
            )
            .await
            .unwrap();
        (
            dir,
            SqliteViewerSort {
                pool: pool.clone().into(),
                clock: SortClock::System,
            },
            pool,
        )
    }

    fn user(id: &str) -> UserId {
        UserId::parse(id).unwrap()
    }
    fn channels(ws: &str) -> SortListKey {
        SortListKey::Channels(ws.to_owned())
    }

    #[tokio::test]
    async fn sidebar_default_order_places_agent_spaces_after_regular_workspaces() {
        use silverhand_backend_core::viewer_read_model::viewer_sort_state::ViewerSidebarRead;
        let (_dir, store, pool) = open_seeded().await;
        for statement in [
            "INSERT INTO hosts(id,name,kind,created_at) VALUES('host-1','Host','local',0)",
            "INSERT INTO agents(id,name,display_name,runtime,host_id,created_by_user_id,created_at,updated_at) VALUES('agent-1','agent','Agent','claude','host-1','user-1',0,0)",
            "INSERT INTO workspaces(id,name,name_key,owner_agent_id,created_at,updated_at) VALUES('space-1','Space',NULL,'agent-1',0,0)",
            "INSERT INTO workspaces(id,name,name_key,owner_user_id,created_at,updated_at) VALUES('ws-0','Earlier','earlier','user-1',0,0)",
            "INSERT INTO workspace_users(workspace_id,user_id,joined_at,role) VALUES('space-1','user-1',0,'member'),('ws-0','user-1',0,'owner')",
        ] {
            sqlx::query(statement).execute(&pool).await.unwrap();
        }

        let state = store
            .sidebar_sort(&UserId::parse("user-1").unwrap())
            .await
            .unwrap();
        assert_eq!(
            state
                .entries
                .iter()
                .map(|e| e.entity_id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "section:starred",
                "section:direct_messages",
                "workspace:ws-0",
                "workspace:ws-1",
                "workspace:space-1"
            ]
        );
        assert_eq!(state.modes.get(""), Some(&SortMode::Manual));
    }

    #[tokio::test]
    async fn sidebar_order_is_complete_private_and_manual_only() {
        use silverhand_backend_core::viewer_read_model::viewer_sort_state::ViewerSidebarRead;
        let (_dir, store, pool) = open_seeded().await;
        let viewer = UserId::parse("user-1").unwrap();
        let ids = |state: ViewerSortStateProjection| {
            state
                .entries
                .into_iter()
                .map(|e| e.entity_id)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            ids(store.sidebar_sort(&viewer).await.unwrap()),
            vec![
                "section:starred",
                "section:direct_messages",
                "workspace:ws-1"
            ]
        );

        let ordered = vec![
            "workspace:ws-1".to_owned(),
            "section:direct_messages".to_owned(),
            "section:starred".to_owned(),
        ];
        let mutation = SortMutation::Reorder {
            group: SortGroup::Sidebar,
            ordered_ids: ordered.clone(),
        };
        assert_eq!(
            ids(store
                .apply_sort_mutation(&viewer, &SortListKey::Sidebar, &mutation)
                .await
                .unwrap()
                .unwrap()),
            ordered
        );
        assert_eq!(ids(store.sidebar_sort(&viewer).await.unwrap()), ordered);

        for ordered_ids in [
            vec!["workspace:ws-1".to_owned()],
            vec!["section:starred".to_owned(); 3],
            vec![
                "section:starred".to_owned(),
                "section:direct_messages".to_owned(),
                "workspace:invisible".to_owned(),
            ],
        ] {
            assert_eq!(
                store
                    .apply_sort_mutation(
                        &viewer,
                        &SortListKey::Sidebar,
                        &SortMutation::Reorder {
                            group: SortGroup::Sidebar,
                            ordered_ids
                        }
                    )
                    .await
                    .unwrap(),
                Err(SortMutationRefusal::SidebarSetChanged)
            );
        }
        for mutation in [
            SortMutation::Touch {
                entity_id: "section:starred".to_owned(),
            },
            SortMutation::SetMode {
                group: None,
                mode: SortMode::Recent,
            },
        ] {
            assert_eq!(
                store
                    .apply_sort_mutation(&viewer, &SortListKey::Sidebar, &mutation)
                    .await
                    .unwrap(),
                Err(SortMutationRefusal::ReorderOnly)
            );
        }

        // A newly visible workspace invalidates the old full-set drag and follows
        // ranked branches. Archiving it removes it from the authorized read.
        sqlx::query("INSERT INTO workspaces(id,name,name_key,owner_user_id,created_at,updated_at) VALUES('ws-2','Second','second','user-1',2,2)")
            .execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO workspace_users(workspace_id,user_id,joined_at,role) VALUES('ws-2','user-1',2,'owner')")
            .execute(&pool).await.unwrap();
        assert_eq!(
            store
                .apply_sort_mutation(&viewer, &SortListKey::Sidebar, &mutation)
                .await
                .unwrap(),
            Err(SortMutationRefusal::SidebarSetChanged)
        );
        assert_eq!(
            ids(store.sidebar_sort(&viewer).await.unwrap())
                .last()
                .unwrap(),
            "workspace:ws-2"
        );
        sqlx::query("UPDATE workspaces SET archived_at=3 WHERE id='ws-2'")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(ids(store.sidebar_sort(&viewer).await.unwrap()), ordered);

        sqlx::query("DELETE FROM workspace_users WHERE user_id='user-1' AND workspace_id='ws-1'")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            ids(store.sidebar_sort(&viewer).await.unwrap()),
            vec!["section:direct_messages", "section:starred"]
        );
        assert_eq!(
            store
                .apply_sort_mutation(&viewer, &SortListKey::Sidebar, &mutation)
                .await
                .unwrap(),
            Err(SortMutationRefusal::SidebarSetChanged)
        );
    }

    #[tokio::test]
    async fn touch_then_reorder_echoes_persisted_dense_ranks_and_manual_mode() {
        let (_dir, sort, pool) = open_seeded().await;
        // Touch conv-a: reaches the live group, stamps last_accessed_at, echoes state.
        let committed = sort
            .apply_sort_mutation(
                &user("user-1"),
                &channels("ws-1"),
                &SortMutation::Touch {
                    entity_id: "conv-a".to_owned(),
                },
            )
            .await
            .unwrap();
        let projection = committed.expect("touch of a live member succeeds");
        assert!(
            projection
                .entries
                .iter()
                .any(|e| e.entity_id == "conv-a" && e.last_accessed_at.is_some())
        );

        // Reorder the whole live channels group [conv-b, conv-a]; the writer
        // re-derives the live set UNDER the lock and rewrites dense ranks + mode.
        let committed = sort
            .apply_sort_mutation(
                &user("user-1"),
                &channels("ws-1"),
                &SortMutation::Reorder {
                    group: SortGroup::Channels,
                    ordered_ids: vec!["conv-b".to_owned(), "conv-a".to_owned()],
                },
            )
            .await
            .unwrap();
        let projection = committed.expect("exact-live-set reorder succeeds");
        // The committed post-image (one snapshot) shows dense ranks 0/1 and the
        // list-wide scope flipped to manual.
        let rank = |id: &str| {
            projection
                .entries
                .iter()
                .find(|e| e.entity_id == id)
                .and_then(|e| e.rank)
        };
        assert_eq!(rank("conv-b"), Some(0));
        assert_eq!(rank("conv-a"), Some(1));
        assert_eq!(projection.modes.get(""), Some(&SortMode::Manual));

        // The committed rows match the echoed post-image (no reread drift).
        let stored: Vec<(String, Option<i64>)> = sqlx::query_as(
            "SELECT entity_id, rank FROM user_sort_entry WHERE list_key='channels:ws-1' ORDER BY entity_id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            stored,
            vec![
                ("conv-a".to_owned(), Some(1)),
                ("conv-b".to_owned(), Some(0))
            ]
        );
    }

    #[tokio::test]
    async fn a_reorder_that_is_not_the_exact_live_set_refuses_and_writes_nothing() {
        let (_dir, sort, pool) = open_seeded().await;
        // Only conv-a — not the exact live members (conv-a,conv-b) — must refuse.
        let committed = sort
            .apply_sort_mutation(
                &user("user-1"),
                &channels("ws-1"),
                &SortMutation::Reorder {
                    group: SortGroup::Channels,
                    ordered_ids: vec!["conv-a".to_owned()],
                },
            )
            .await
            .unwrap();
        assert_eq!(
            committed,
            Err(SortMutationRefusal::NotExactMembers { expected_count: 2 })
        );
        // No mode flip, no rank rows — the refusal rolled back with zero write.
        let mode_rows: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM user_list_sort_mode WHERE list_key='channels:ws-1'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(mode_rows, 0, "refusal wrote no mode row");
        let rank_rows: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM user_sort_entry WHERE list_key='channels:ws-1' AND rank IS NOT NULL",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(rank_rows, 0, "refusal wrote no rank row");
    }

    #[tokio::test]
    async fn an_empty_channels_workspace_is_list_not_found_never_a_write() {
        let (_dir, sort, pool) = open_seeded().await;
        // `channels:` (empty ws) — admitted by the grammar, resolves to list 404.
        let committed = sort
            .apply_sort_mutation(
                &user("user-1"),
                &channels(""),
                &SortMutation::Touch {
                    entity_id: "conv-a".to_owned(),
                },
            )
            .await
            .unwrap();
        assert_eq!(committed, Err(SortMutationRefusal::ListNotFound));
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM user_sort_entry")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 0);
    }

    #[tokio::test]
    async fn touch_of_a_non_member_is_unknown_entity_and_writes_nothing() {
        let (_dir, sort, pool) = open_seeded().await;
        let committed = sort
            .apply_sort_mutation(
                &user("user-1"),
                &channels("ws-1"),
                &SortMutation::Touch {
                    entity_id: "conv-ghost".to_owned(),
                },
            )
            .await
            .unwrap();
        assert_eq!(committed, Err(SortMutationRefusal::UnknownEntity));
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM user_sort_entry")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 0);
    }

    #[tokio::test]
    async fn favorites_does_not_support_touch() {
        let (_dir, sort, _pool) = open_seeded().await;
        let committed = sort
            .apply_sort_mutation(
                &user("user-1"),
                &SortListKey::Favorites,
                &SortMutation::Touch {
                    entity_id: "conversation:conv-a".to_owned(),
                },
            )
            .await
            .unwrap();
        assert_eq!(committed, Err(SortMutationRefusal::FavoritesNotTouchable));
    }

    #[tokio::test]
    async fn the_configured_clock_value_is_persisted_and_echoed() {
        // The committed row and post-image carry the configured clock value.
        // A fixed clock does not prove when or how often it was sampled.
        let (_dir, mut sort, pool) = open_seeded().await;
        sort.clock = SortClock::Fixed(1_234_567);
        let committed = sort
            .apply_sort_mutation(
                &user("user-1"),
                &channels("ws-1"),
                &SortMutation::Touch {
                    entity_id: "conv-a".to_owned(),
                },
            )
            .await
            .unwrap()
            .expect("touch succeeds");
        // The echoed post-image carries the fixed sample.
        let echoed = committed
            .entries
            .iter()
            .find(|e| e.entity_id == "conv-a")
            .and_then(|e| e.last_accessed_at);
        assert_eq!(echoed, Some(1_234_567));
        // And the committed row carries the same value.
        let stored: Option<i64> = sqlx::query_scalar(
            "SELECT last_accessed_at FROM user_sort_entry WHERE list_key='channels:ws-1' AND entity_id='conv-a'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(stored, Some(1_234_567));
    }

    #[tokio::test]
    async fn unstar_clears_the_home_stamp_and_removes_the_favorites_rank() {
        let (_dir, sort, pool) = open_seeded().await;
        // Star conv-a, then seed a favorites rank orphan, then unstar: the unstar
        // clears starred_at AND drops the favorites orphan.
        sort.apply_sort_mutation(
            &user("user-1"),
            &channels("ws-1"),
            &SortMutation::SetStarred {
                entity_id: "conv-a".to_owned(),
                starred: true,
            },
        )
        .await
        .unwrap()
        .unwrap();
        sqlx::query(
            "INSERT INTO user_sort_entry (user_id,list_key,entity_id,rank,last_accessed_at) \
             VALUES ('user-1','favorites','conversation:conv-a',5,NULL)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sort.apply_sort_mutation(
            &user("user-1"),
            &channels("ws-1"),
            &SortMutation::SetStarred {
                entity_id: "conv-a".to_owned(),
                starred: false,
            },
        )
        .await
        .unwrap()
        .unwrap();
        let starred_at: Option<i64> = sqlx::query_scalar(
            "SELECT starred_at FROM user_sort_entry WHERE list_key='channels:ws-1' AND entity_id='conv-a'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(starred_at, None, "unstar cleared starred_at");
        let favorites_rows: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM user_sort_entry WHERE list_key='favorites' AND entity_id='conversation:conv-a'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            favorites_rows, 0,
            "unstar dropped the favorites rank orphan"
        );
    }

    #[tokio::test]
    async fn favorites_post_image_is_entry_per_live_favorite_with_null_rank_and_preserved_home_facts()
     {
        // Star conv-a on its channels list (stamps home starred_at +
        // last_accessed), then a favorites set-mode returns the FAVORITES post-image —
        // which must carry an entry for conv-a with rank=null (never manually sorted)
        // and the HOME starred_at/last_accessed preserved, NOT the null-valued
        // `list_key='favorites'` rank row.
        let (_dir, writer, _pool) = open_seeded().await;
        writer
            .apply_sort_mutation(
                &user("user-1"),
                &channels("ws-1"),
                &SortMutation::Touch {
                    entity_id: "conv-a".to_owned(),
                },
            )
            .await
            .unwrap()
            .unwrap();
        writer
            .apply_sort_mutation(
                &user("user-1"),
                &channels("ws-1"),
                &SortMutation::SetStarred {
                    entity_id: "conv-a".to_owned(),
                    starred: true,
                },
            )
            .await
            .unwrap()
            .unwrap();
        // The favorites post-image (via a favorites list-wide set-mode).
        let projection = writer
            .apply_sort_mutation(
                &user("user-1"),
                &SortListKey::Favorites,
                &SortMutation::SetMode {
                    group: None,
                    mode: SortMode::Recent,
                },
            )
            .await
            .unwrap()
            .unwrap();
        let entry = projection
            .entries
            .iter()
            .find(|e| e.entity_id == "conversation:conv-a")
            .expect("conv-a is a live favorite in the favorites post-image");
        assert_eq!(
            entry.rank, None,
            "never manually sorted → rank null, entry still present"
        );
        assert!(entry.starred_at.is_some(), "home starred_at preserved");
        assert!(
            entry.last_accessed_at.is_some(),
            "home last_accessed_at preserved"
        );
    }

    #[tokio::test]
    async fn a_direct_pair_stars_in_its_own_home_list_and_joins_the_favorites_order() {
        let (_dir, sort, pool) = open_seeded().await;
        for statement in [
            "INSERT INTO users (id,name,display_name,created_at,updated_at) VALUES ('user-2','v','V',1,1)",
            "INSERT INTO conversations (id,placement_kind,roster_policy,title,status,visibility,created_at,updated_at,endpoint_a_kind,endpoint_a_id,endpoint_b_kind,endpoint_b_id) \
             VALUES ('dm-1','direct_pair','fixed','','idle','private',1,1,'human','user-1','human','user-2')",
        ] {
            sqlx::query(statement).execute(&pool).await.unwrap();
        }
        let star = |starred| SortMutation::SetStarred {
            entity_id: "dm-1".to_owned(),
            starred,
        };
        // Only an endpoint can star the pair.
        assert_eq!(
            sort.apply_sort_mutation(&user("user-3"), &SortListKey::DirectPairs, &star(true))
                .await
                .unwrap(),
            Err(SortMutationRefusal::UnknownEntity)
        );
        // The internal list admits nothing but stars.
        assert_eq!(
            sort.apply_sort_mutation(
                &user("user-1"),
                &SortListKey::DirectPairs,
                &SortMutation::Touch {
                    entity_id: "dm-1".to_owned()
                },
            )
            .await
            .unwrap(),
            Err(SortMutationRefusal::ListNotFound)
        );
        sort.apply_sort_mutation(&user("user-1"), &SortListKey::DirectPairs, &star(true))
            .await
            .unwrap()
            .unwrap();
        sort.apply_sort_mutation(
            &user("user-1"),
            &channels("ws-1"),
            &SortMutation::SetStarred {
                entity_id: "conv-a".to_owned(),
                starred: true,
            },
        )
        .await
        .unwrap()
        .unwrap();

        // The pair and the channel are one favorites group.
        let ordered = sort
            .apply_sort_mutation(
                &user("user-1"),
                &SortListKey::Favorites,
                &SortMutation::Reorder {
                    group: SortGroup::Favorites,
                    ordered_ids: vec![
                        "conversation:dm-1".to_owned(),
                        "conversation:conv-a".to_owned(),
                    ],
                },
            )
            .await
            .unwrap()
            .unwrap();
        let mut ranks: Vec<_> = ordered
            .entries
            .iter()
            .map(|e| (e.entity_id.as_str(), e.rank, e.starred_at.is_some()))
            .collect();
        ranks.sort();
        assert_eq!(
            ranks,
            vec![
                ("conversation:conv-a", Some(1), true),
                ("conversation:dm-1", Some(0), true),
            ]
        );
        // The other endpoint's favorites are untouched.
        let theirs = sort
            .apply_sort_mutation(
                &user("user-2"),
                &SortListKey::Favorites,
                &SortMutation::SetMode {
                    group: None,
                    mode: SortMode::Recent,
                },
            )
            .await
            .unwrap()
            .unwrap();
        assert!(theirs.entries.is_empty());

        // Unstar leaves the favorites group and drops its rank.
        sort.apply_sort_mutation(&user("user-1"), &SortListKey::DirectPairs, &star(false))
            .await
            .unwrap()
            .unwrap();
        let ranked: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM user_sort_entry WHERE list_key='favorites' AND entity_id='conversation:dm-1'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(ranked, 0);
    }
}
