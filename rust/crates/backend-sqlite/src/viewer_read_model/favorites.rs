//! The favorites read adapter over `user_sort_entry` + `conversations` +
//! `workspaces` + `workspace_users` + `agents`, with the favorites ordering state
//! and the canonical unread rollup.
//!
//! It reuses the ONE canonical `conversation_unread_counts` rollup (the same
//! predicate an in-tree conversation row uses) so a starred badge equals what the
//! in-tree row would show — one source, no drift. Membership is projected from the
//! join alone; the favorites `list_key` stores only its own rank/mode.

use silverhand_backend_core::{
    conversation_messaging::messaging::collaboration::channel_directory::DirectoryRoster,
    foundation::identity::{ConversationId, UserId, WorkspaceId},
    foundation::name_canonicalization::is_ecmascript_trim_whitespace,
    foundation::persistence::PersistenceFailure,
    viewer_read_model::{
        favorites::{
            DirectPairFavoriteRow, FavoriteRow, FavoriteVisibility, FavoritesReadPort,
            ListFavorites,
        },
        viewer_sort_state::{
            SortListKey, SortMode, ViewerSortEntryProjection, ViewerSortStateProjection,
            favorite_key,
        },
    },
};
use silverhand_backend_observability::Observe;
use sqlx::{Row, Sqlite, Transaction};
use std::collections::BTreeMap;
use thiserror::Error;

use crate::conversation_messaging::direct_pair_read::{
    DirectPairReadError, LivePrincipals, PairEvidence,
};
use crate::conversation_messaging::messaging::MessagingStore;
use crate::viewer_read_model::unread_read::conversation_unread_counts;

/// The favorites read authority — a shared-pool clone of the one messaging store
/// pool. It opens no second connection authority.
#[derive(Clone)]
pub struct SqliteFavoritesRead {
    pool: crate::foundation::database::SqliteDatabase,
}

impl<E> MessagingStore<E> {
    /// The favorites read authority, sharing this store's pool.
    pub fn favorites(&self) -> SqliteFavoritesRead {
        SqliteFavoritesRead {
            pool: self.pool().clone(),
        }
    }
}

#[derive(Debug, Error)]
pub enum FavoritesReadError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("stored favorite identity {field} is malformed")]
    MalformedIdentity { field: &'static str },
    #[error("stored favorite roster policy is outside the closed set")]
    UnknownRoster(String),
    #[error("stored favorite visibility is outside the closed set")]
    UnknownVisibility(String),
    #[error("stored favorites sort mode is outside the closed set")]
    UnknownMode(String),
    #[error("stored direct-pair favorite is malformed")]
    DirectPair(#[from] DirectPairReadError),
}

impl From<FavoritesReadError> for PersistenceFailure {
    fn from(value: FavoritesReadError) -> Self {
        Self::from_source(value)
    }
}

const SELECT_CONVERSATION_FAVORITES: &str = "\
    SELECT use.entity_id AS entity_id, use.starred_at AS starred_at, \
           use.last_accessed_at AS last_accessed_at, c.created_at AS created_at, \
           c.roster_policy AS roster_policy, c.title AS title, c.visibility AS visibility, \
           c.workspace_id AS workspace_id, w.name AS workspace_name, \
           w.owner_agent_id AS owner_agent_id, a.display_name AS agent_display_name, \
           a.name AS agent_name \
      FROM user_sort_entry use \
      JOIN conversations c ON c.id = use.entity_id \
      JOIN workspaces w ON w.id = c.workspace_id \
      JOIN workspace_users wu ON wu.workspace_id = w.id AND wu.user_id = ? \
      LEFT JOIN agents a ON a.id = w.owner_agent_id \
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
       )";

const SELECT_DIRECT_PAIR_FAVORITES: &str = "\
    SELECT c.id, c.endpoint_a_kind, c.endpoint_a_id, c.endpoint_a_deleted_at, \
           c.endpoint_b_kind, c.endpoint_b_id, c.endpoint_b_deleted_at, c.created_at, \
           use.starred_at AS starred_at, use.last_accessed_at AS last_accessed_at \
      FROM user_sort_entry use \
      JOIN conversations c ON c.id = use.entity_id AND c.placement_kind = 'direct_pair' \
     WHERE use.user_id = ? \
       AND use.list_key = 'direct_pairs' \
       AND use.starred_at IS NOT NULL \
       AND ((c.endpoint_a_kind = 'human' AND c.endpoint_a_id = ?) \
         OR (c.endpoint_b_kind = 'human' AND c.endpoint_b_id = ?))";

const SELECT_FAVORITES_RANKS: &str = "\
    SELECT entity_id, rank FROM user_sort_entry \
     WHERE user_id = ? AND list_key = 'favorites' AND rank IS NOT NULL";

const SELECT_FAVORITES_MODES: &str = "\
    SELECT scope, mode FROM user_list_sort_mode \
     WHERE user_id = ? AND list_key = 'favorites'";

/// A decoded favorites row before its unread count is joined in.
struct RawFavorite {
    entity_id: ConversationId,
    starred_at: i64,
    last_accessed_at: Option<i64>,
    created_at: i64,
    roster_policy: DirectoryRoster,
    title: String,
    visibility: FavoriteVisibility,
    workspace_id: WorkspaceId,
    parent_title: String,
}

impl FavoritesReadPort for SqliteFavoritesRead {
    #[tracing::instrument(
        target = "silverhand::diagnostics",
        name = "db.favorites",
        skip_all,
        fields(
            elapsed_ms = tracing::field::Empty,
            outcome = tracing::field::Empty,
            operation_finished = tracing::field::Empty,
        )
    )]
    async fn list_favorites(&self, viewer: &UserId) -> Result<ListFavorites, PersistenceFailure> {
        let observation = silverhand_backend_observability::Operation::current();
        let result = async {
            // One snapshot: membership + ranks + modes + the unread rollup read as one
            // consistent instant, so the badge cannot drift from the projected set.
            let mut tx =
                crate::foundation::statement_metrics::begin(&self.pool, Some("BEGIN DEFERRED"))
                    .await
                    .map_err(FavoritesReadError::from)?;

            let rows = sqlx::query(SELECT_CONVERSATION_FAVORITES)
                .bind(viewer.as_str())
                .bind(viewer.as_str())
                .bind(viewer.as_str())
                .fetch_all(&mut *tx)
                .observe(silverhand_backend_observability::operation_span!(
                    "db.favorites.conversations"
                ))
                .await
                .map_err(FavoritesReadError::from)?;
            let mut raw = Vec::with_capacity(rows.len());
            for row in &rows {
                raw.push(decode_favorite(row)?);
            }

            let pair_rows = sqlx::query(SELECT_DIRECT_PAIR_FAVORITES)
                .bind(viewer.as_str())
                .bind(viewer.as_str())
                .bind(viewer.as_str())
                .fetch_all(&mut *tx)
                .observe(silverhand_backend_observability::operation_span!(
                    "db.favorites.direct_pairs"
                ))
                .await
                .map_err(FavoritesReadError::from)?;
            let mut pairs = Vec::with_capacity(pair_rows.len());
            for row in &pair_rows {
                let starred_at = fav_js_safe(
                    row.try_get("starred_at")
                        .map_err(FavoritesReadError::from)?,
                    "starred_at",
                )?;
                let last_accessed_at = match row
                    .try_get::<Option<i64>, _>("last_accessed_at")
                    .map_err(FavoritesReadError::from)?
                {
                    Some(v) => Some(fav_js_safe(v, "last_accessed_at")?),
                    None => None,
                };
                let pair = PairEvidence::decode(row).map_err(FavoritesReadError::from)?;
                pairs.push((pair, starred_at, last_accessed_at));
            }
            let evidence: Vec<PairEvidence> = pairs.iter().map(|(p, ..)| p.clone()).collect();
            let principals = LivePrincipals::load(&mut tx, &evidence)
                .await
                .map_err(FavoritesReadError::from)?;

            let rank_rows = sqlx::query(SELECT_FAVORITES_RANKS)
                .bind(viewer.as_str())
                .fetch_all(&mut *tx)
                .observe(silverhand_backend_observability::operation_span!(
                    "db.favorites.ranks"
                ))
                .await
                .map_err(FavoritesReadError::from)?;
            let mut rank_by_key: BTreeMap<String, i64> = BTreeMap::new();
            for row in &rank_rows {
                let key: String = row.try_get("entity_id").map_err(FavoritesReadError::from)?;
                let rank: i64 = fav_js_safe(
                    row.try_get("rank").map_err(FavoritesReadError::from)?,
                    "rank",
                )?;
                rank_by_key.insert(key, rank);
            }

            let mode_rows = sqlx::query(SELECT_FAVORITES_MODES)
                .bind(viewer.as_str())
                .fetch_all(&mut *tx)
                .observe(silverhand_backend_observability::operation_span!(
                    "db.favorites.modes"
                ))
                .await
                .map_err(FavoritesReadError::from)?;
            let mut modes: BTreeMap<String, SortMode> = BTreeMap::new();
            for row in &mode_rows {
                let scope: String = row.try_get("scope").map_err(FavoritesReadError::from)?;
                let mode_raw: String = row.try_get("mode").map_err(FavoritesReadError::from)?;
                let mode =
                    SortMode::parse(&mode_raw).ok_or(FavoritesReadError::UnknownMode(mode_raw))?;
                modes.insert(scope, mode);
            }

            let conversation_ids: Vec<ConversationId> = raw
                .iter()
                .map(|row| row.entity_id.clone())
                .chain(evidence.iter().map(|pair| pair.conversation().clone()))
                .collect();
            let unread = unread_counts(&mut tx, viewer, &conversation_ids).await?;

            tx.commit().await.map_err(FavoritesReadError::from)?;

            let mut items = Vec::with_capacity(raw.len());
            let mut entries = Vec::with_capacity(raw.len());
            for row in raw {
                let key = favorite_key(row.entity_id.as_str());
                // #10: the unread rollup's raw i64 crosses the wire as a JS number — gate
                // it through JsSafeInteger (fail-closed) like every other outbound numeric.
                let unread_count = fav_js_safe(
                    unread.get(&row.entity_id).copied().unwrap_or(0),
                    "unread_count",
                )?;
                entries.push(ViewerSortEntryProjection {
                    entity_id: key.clone(),
                    rank: rank_by_key.get(&key).copied(),
                    last_accessed_at: row.last_accessed_at,
                    starred_at: Some(row.starred_at),
                });
                items.push(FavoriteRow {
                    roster_policy: row.roster_policy,
                    favorite_key: key,
                    entity_id: row.entity_id,
                    workspace_id: row.workspace_id,
                    title: row.title,
                    parent_title: row.parent_title,
                    starred_at: row.starred_at,
                    created_at: row.created_at,
                    unread_count,
                    visibility: row.visibility,
                });
            }
            let mut direct_pairs = Vec::with_capacity(pairs.len());
            for (pair, starred_at, last_accessed_at) in pairs {
                let counterpart = pair
                    .counterpart(viewer)
                    .and_then(|endpoint| principals.counterpart(&endpoint))
                    .map_err(FavoritesReadError::from)?;
                let entity_id = pair.conversation().clone();
                let key = favorite_key(entity_id.as_str());
                let created_at = fav_js_safe(pair.created_at_ms(), "created_at")?;
                let unread_count =
                    fav_js_safe(unread.get(&entity_id).copied().unwrap_or(0), "unread_count")?;
                entries.push(ViewerSortEntryProjection {
                    entity_id: key.clone(),
                    rank: rank_by_key.get(&key).copied(),
                    last_accessed_at,
                    starred_at: Some(starred_at),
                });
                direct_pairs.push(DirectPairFavoriteRow {
                    favorite_key: key,
                    entity_id,
                    counterpart,
                    starred_at,
                    created_at,
                    unread_count,
                });
            }
            direct_pairs.sort_by(|a, b| {
                b.starred_at
                    .cmp(&a.starred_at)
                    .then_with(|| cmp_utf16(&a.favorite_key, &b.favorite_key))
            });
            // Deterministic fallback order: most-recently-starred first, then a favorite_key
            // tiebreak by UTF-16 CODE UNITS (#10) — matching TS `<`/`>` on strings, NOT
            // Rust's default UTF-8 byte `String::cmp` (an opaque non-ASCII conversation id
            // can order differently between the two, e.g. U+10000 vs U+E000). The renderer
            // composes the authoritative order from viewer_sort.
            items.sort_by(|a, b| {
                b.starred_at
                    .cmp(&a.starred_at)
                    .then_with(|| cmp_utf16(&a.favorite_key, &b.favorite_key))
            });

            Ok(ListFavorites {
                items,
                direct_pairs,
                viewer_sort: ViewerSortStateProjection {
                    list_key: SortListKey::Favorites,
                    modes: modes.into_iter().collect(),
                    entries,
                },
            })
        }
        .await;
        observation.finish(&result);
        result
    }
}

async fn unread_counts(
    tx: &mut Transaction<'_, Sqlite>,
    viewer: &UserId,
    conversations: &[ConversationId],
) -> Result<std::collections::HashMap<ConversationId, i64>, PersistenceFailure> {
    conversation_unread_counts(tx, viewer, conversations)
        .await
        .map_err(PersistenceFailure::from_source)
}

/// JS-safe integer gate at store->projection (#48): a stored numeric JS can't
/// represent exactly is a read error, not a truncated wire number.
fn fav_js_safe(value: i64, field: &'static str) -> Result<i64, FavoritesReadError> {
    silverhand_backend_core::foundation::wire_number::JsSafeInteger::new(value)
        .map(|v| v.get())
        .map_err(|_| FavoritesReadError::MalformedIdentity { field })
}

/// Compare two strings by their UTF-16 CODE UNITS, matching TS `<`/`>`
/// on strings — NOT Rust's default UTF-8 byte order. The two agree on ASCII and the
/// BMP but diverge on astral scalars: e.g. U+10000 is one code point but the surrogate
/// pair `0xD800 0xDC00` sorts BELOW U+E000 in UTF-16, while its UTF-8 bytes sort ABOVE.
fn cmp_utf16(a: &str, b: &str) -> std::cmp::Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

fn decode_favorite(row: &sqlx::sqlite::SqliteRow) -> Result<RawFavorite, FavoritesReadError> {
    let entity_id_raw: String = row.try_get("entity_id")?;
    let entity_id = ConversationId::parse(entity_id_raw)
        .map_err(|_| FavoritesReadError::MalformedIdentity { field: "entity_id" })?;
    let workspace_id_raw: String = row.try_get("workspace_id")?;
    let workspace_id = WorkspaceId::parse(workspace_id_raw).map_err(|_| {
        FavoritesReadError::MalformedIdentity {
            field: "workspace_id",
        }
    })?;
    let roster_raw: String = row.try_get("roster_policy")?;
    let roster_policy = DirectoryRoster::parse(&roster_raw)
        .map_err(|_| FavoritesReadError::UnknownRoster(roster_raw))?;
    let visibility_raw: String = row.try_get("visibility")?;
    let visibility = FavoriteVisibility::parse(&visibility_raw)
        .ok_or(FavoritesReadError::UnknownVisibility(visibility_raw))?;
    let title: String = row.try_get("title")?;
    let starred_at: i64 = fav_js_safe(row.try_get("starred_at")?, "starred_at")?;
    let last_accessed_at: Option<i64> = match row.try_get::<Option<i64>, _>("last_accessed_at")? {
        Some(v) => Some(fav_js_safe(v, "last_accessed_at")?),
        None => None,
    };
    let created_at: i64 = fav_js_safe(row.try_get("created_at")?, "created_at")?;
    let workspace_name: String = row.try_get("workspace_name")?;
    let owner_agent_id: Option<String> = row.try_get("owner_agent_id")?;
    let agent_display_name: Option<String> = row.try_get("agent_display_name")?;
    let agent_name: Option<String> = row.try_get("agent_name")?;
    // Parent label: for an agent home, the bound agent's display name, falling
    // back to its handle, then the workspace name; for a regular workspace, the
    // workspace name. The TS uses `display_name.trim()` ONLY as the emptiness TEST
    // and returns the ORIGINAL (untrimmed) `display_name` when that test passes —
    // so we keep `display_name` verbatim, testing emptiness against the shared
    // ECMAScript trim-whitespace authority (NOT Rust `str::trim()`, whose
    // Unicode-White_Space set differs — e.g. it strips U+0085 NEL that ECMAScript
    // keeps, which would flip a `"\u{0085}x"`-style name to the handle fallback).
    let parent_title = if owner_agent_id.is_some() {
        match agent_display_name {
            Some(display)
                if !display
                    .trim_matches(is_ecmascript_trim_whitespace)
                    .is_empty() =>
            {
                display
            }
            _ => agent_name.unwrap_or(workspace_name),
        }
    } else {
        workspace_name
    };
    Ok(RawFavorite {
        entity_id,
        starred_at,
        last_accessed_at,
        created_at,
        roster_policy,
        title,
        visibility,
        workspace_id,
        parent_title,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use silverhand_backend_core::viewer_read_model::favorites::FavoritesReadPort;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use sqlx::{Connection, Executor, SqliteConnection};

    const CURRENT_SCHEMA: &str =
        include_str!("../../../../../rust/test-fixtures/migration-baseline/schema-current.sql");

    // A viewer (`user-1`) who has starred a channel `chan` that lives in an AGENT-HOME
    // workspace `home` owned by `agent-a`. `parent_title` therefore derives from the
    // agent's display_name (emptiness-tested by ECMAScript trim) with the handle as
    // fallback. The agent's display_name is injected per-test (bound, not formatted).
    const SEED_PRE: &str = "INSERT INTO users (id, name, display_name, created_at, updated_at) VALUES ('user-1','viewer','Viewer',1,1); \
         INSERT INTO hosts (id, name, kind, created_at) VALUES ('host-1','Host','local',1);";

    const SEED_POST: &str = "INSERT INTO workspaces (id, name, name_key, owner_agent_id, created_at, updated_at) VALUES ('home',' ',NULL,'agent-a',1,1); \
         INSERT INTO workspace_users (workspace_id, user_id, joined_at, role) VALUES ('home','user-1',1,'member'); \
         INSERT INTO conversations (id, workspace_id, placement_kind, roster_policy, title, title_key, status, visibility, created_at, updated_at) \
           VALUES ('chan','home','workspace','open','Chan','chan','idle','private',1,1); \
         INSERT INTO conversation_users (conversation_id, user_id, joined_at) VALUES ('chan','user-1',1); \
         INSERT INTO user_sort_entry (user_id, list_key, entity_id, rank, last_accessed_at, starred_at) \
           VALUES ('user-1','channels:home','chan',NULL,NULL,10);";

    async fn parent_title_for(display_name: &str) -> String {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("favorites.sqlite");
        let mut db = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&path)
                .create_if_missing(true)
                .foreign_keys(true),
        )
        .await
        .unwrap();
        db.execute(sqlx::raw_sql(CURRENT_SCHEMA)).await.unwrap();
        db.execute(sqlx::raw_sql(SEED_PRE)).await.unwrap();
        // The owning agent, with its display_name BOUND (never formatted into SQL), so the
        // trim discriminator is exercised on the real stored value.
        sqlx::query(
            "INSERT INTO agents (id, name, display_name, runtime, host_id, created_by_user_id, created_at, updated_at, session_scope) \
             VALUES ('agent-a','alice',?,'claude','host-1','user-1',1,1,'conversation')",
        )
        .bind(display_name)
        .execute(&mut db)
        .await
        .unwrap();
        db.execute(sqlx::raw_sql(SEED_POST)).await.unwrap();
        db.close().await.unwrap();
        let pool = SqlitePoolOptions::new()
            .max_connections(2)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&path)
                    .foreign_keys(true),
            )
            .await
            .unwrap();
        let read = SqliteFavoritesRead { pool: pool.into() };
        let listed = read
            .list_favorites(&UserId::parse("user-1").unwrap())
            .await
            .unwrap();
        assert_eq!(
            listed.items.len(),
            1,
            "the starred agent-home channel is favorited"
        );
        listed.items[0].parent_title.clone()
    }

    #[tokio::test]
    async fn parent_title_uses_ecmascript_trim_not_rust_trim_for_emptiness() {
        // U+FEFF is ECMAScript trim-whitespace but not Rust trim-whitespace:
        // the stored BOM-only name must fall back to the agent handle `alice`.
        assert_eq!(parent_title_for("\u{FEFF}").await, "alice");

        // U+0085 (NEL) is Rust Unicode-White_Space but NOT ECMAScript trim-whitespace:
        // JS `"\u{0085}".trim()` is NON-empty → JS KEEPS the display_name. Rust
        // `str::trim()` would wrongly judge it empty and fall back to `alice`. With the
        // shared ECMAScript authority, Rust must keep the ORIGINAL display_name `"\u{0085}"`,
        // matching TS byte-for-byte. This is the migration discriminator flagged.
        assert_eq!(parent_title_for("\u{0085}").await, "\u{0085}");

        // A U+0085-adorned real name: JS keeps the whole original (untrimmed) value.
        assert_eq!(parent_title_for("\u{0085}Ada").await, "\u{0085}Ada");

        // A genuinely-empty (after ECMAScript trim) name with a leading BOM + spaces
        // falls back to the handle in both.
        assert_eq!(parent_title_for("\u{FEFF} \u{00A0}").await, "alice");

        // A normal display_name is returned verbatim.
        assert_eq!(parent_title_for("Alice").await, "Alice");
    }

    #[tokio::test]
    async fn a_starred_direct_pair_is_listed_only_for_its_starring_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("favorites.sqlite");
        let mut db = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&path)
                .create_if_missing(true)
                .foreign_keys(true),
        )
        .await
        .unwrap();
        db.execute(sqlx::raw_sql(CURRENT_SCHEMA)).await.unwrap();
        db.execute(sqlx::raw_sql(SEED_PRE)).await.unwrap();
        db.execute(sqlx::raw_sql(
            "INSERT INTO agents (id, name, display_name, runtime, host_id, created_by_user_id, created_at, updated_at, session_scope) \
               VALUES ('agent-a','alice','Alice','claude','host-1','user-1',1,1,'agent'); \
             INSERT INTO users (id, name, display_name, created_at, updated_at) VALUES ('user-2','other','Other',1,1); \
             INSERT INTO conversations (id,placement_kind,roster_policy,title,status,visibility,created_at,updated_at,endpoint_a_kind,endpoint_a_id,endpoint_b_kind,endpoint_b_id) \
               VALUES ('dm-1','direct_pair','fixed','','idle','private',7,7,'agent','agent-a','human','user-1'); \
             INSERT INTO user_sort_entry (user_id, list_key, entity_id, rank, last_accessed_at, starred_at) \
               VALUES ('user-1','direct_pairs','dm-1',NULL,NULL,20), ('user-2','direct_pairs','dm-1',NULL,NULL,20); \
             INSERT INTO user_sort_entry (user_id, list_key, entity_id, rank) VALUES ('user-1','favorites','conversation:dm-1',3);",
        ))
        .await
        .unwrap();
        db.close().await.unwrap();
        let read = SqliteFavoritesRead {
            pool: SqlitePoolOptions::new()
                .connect_with(SqliteConnectOptions::new().filename(&path))
                .await
                .unwrap()
                .into(),
        };

        let listed = read
            .list_favorites(&UserId::parse("user-1").unwrap())
            .await
            .unwrap();
        assert!(listed.items.is_empty());
        let [pair] = listed.direct_pairs.as_slice() else {
            panic!("one direct-pair favorite: {:?}", listed.direct_pairs);
        };
        assert_eq!(pair.favorite_key, "conversation:dm-1");
        assert_eq!(pair.entity_id.as_str(), "dm-1");
        assert_eq!(pair.created_at, 7);
        assert!(matches!(
            &pair.counterpart,
            silverhand_backend_core::conversation_messaging::direct_pair_read::DirectPairCounterpart::Active(_)
        ));
        assert_eq!(
            listed.viewer_sort.entries,
            vec![ViewerSortEntryProjection {
                entity_id: "conversation:dm-1".to_owned(),
                rank: Some(3),
                last_accessed_at: None,
                starred_at: Some(20),
            }]
        );

        // A star row from a non-endpoint never surfaces.
        let other = read
            .list_favorites(&UserId::parse("user-2").unwrap())
            .await
            .unwrap();
        assert!(other.direct_pairs.is_empty());
        assert!(other.viewer_sort.entries.is_empty());
    }

    #[test]
    fn utf16_tiebreak_orders_astral_below_private_use_matching_js() {
        use std::cmp::Ordering;
        // U+10000 (astral) is `0xD800 0xDC00` in UTF-16 — BELOW U+E000
        // (`0xE000`). JS `<` / `>` on strings compares code units, so U+10000 < U+E000;
        // Rust's default UTF-8 byte `str::cmp` would put U+10000 (4 bytes f0 90 80 80)
        // ABOVE U+E000 (3 bytes ee 80 80). Our tiebreak matches JS.
        let astral = "\u{10000}";
        let private_use = "\u{E000}";
        assert_eq!(cmp_utf16(astral, private_use), Ordering::Less);
        // (Sanity: the naive byte comparison would disagree.)
        assert_eq!(astral.cmp(private_use), Ordering::Greater);
        // ASCII / BMP agree with byte order.
        assert_eq!(cmp_utf16("a", "b"), Ordering::Less);
        assert_eq!(cmp_utf16("chan-1", "chan-1"), Ordering::Equal);
    }

    #[test]
    fn unread_count_is_js_safe_gated() {
        // #10: MAX_SAFE_INTEGER is accepted; MAX_SAFE_INTEGER+1 fails closed.
        assert!(fav_js_safe(9_007_199_254_740_991, "unread_count").is_ok());
        assert!(fav_js_safe(9_007_199_254_740_992, "unread_count").is_err());
    }
}
