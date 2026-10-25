//! SQLite adapter for the human user-identity context's `users` authority.
//!
//! [`SqliteUserStore`] offers the three `users`-authority ports this vertical
//! needs: the self-profile read behind `GET /api/me` ([`UserProfileRead`]), the
//! email→user existence read signup consults ([`UserEmailLookup`]), and the
//! `display_name` write behind `PATCH /api/me` ([`UserProfileWriteUnitOfWork`]).
//!
//! The write mirrors the product's `updateProfile`: it re-reads the row under one
//! `BEGIN IMMEDIATE` lock and writes ONLY when the requested display differs from
//! the stored one — a no-op patch returns the existing row WITHOUT bumping
//! `updated_at`. The adapter decides that skip under its own lock and reports the
//! committed row plus the display it read BEFORE writing; the core seal
//! (`seal_profile_write_fact`) derives whether the display actually moved. The
//! clock stays in the application: `now_ms` is the value a real write stamps into
//! `updated_at`.

use std::{path::Path, time::Duration};

use silverhand_backend_core::{
    conversation_messaging::messaging::AvatarRevision,
    foundation::identity::UserId,
    foundation::persistence::PersistenceFailure,
    identity_access::avatar::{
        AvatarBlobPointer, AvatarContentType, AvatarPointerWrite, AvatarPointerWriteUnitOfWork,
        AvatarStorageRef, AvatarStorageRefRead,
    },
    identity_access::avatar_design::AvatarDesign,
    identity_access::user_identity::{
        CompleteEmbeddedProfileSetup, DisplayName, EmbeddedProfileSetup, EmbeddedProfileSetupStore,
        ParticipantHandle, RelatedUserDirectoryRead, UserDirectoryRead, UserEmailLookup,
        UserProfileRead, UserProfileWrite, UserProfileWriteUnitOfWork, UserRecord,
    },
};
use sqlx::{Row, SqlitePool};
use thiserror::Error;

const ROOT_WRITE_BEGIN: &str = "BEGIN IMMEDIATE";
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

const SELECT_PROFILE: &str = "\
SELECT id, name, display_name, email, avatar_storage_key, created_at, updated_at FROM users WHERE id = ?";
const SELECT_EMBEDDED_PROFILE: &str = "
SELECT id, name, display_name, email, avatar_storage_key, created_at, updated_at,
       embedded_profile_pending
FROM users WHERE id = ?";
const COMPLETE_EMBEDDED_PROFILE: &str = "
UPDATE users SET name = ?, display_name = ?, embedded_profile_pending = 0, updated_at = ?
WHERE id = ?
RETURNING id, name, display_name, email, avatar_storage_key, created_at, updated_at";
const SELECT_ID_BY_EMAIL: &str = "SELECT id FROM users WHERE email = ? COLLATE NOCASE";
const UPDATE_DISPLAY_NAME: &str = "\
UPDATE users SET display_name = ?, updated_at = ? \
 WHERE id = ? \
 RETURNING id, name, display_name, email, avatar_storage_key, created_at, updated_at";
const LIST_PROFILES: &str = "\
SELECT id, name, display_name, email, avatar_storage_key, created_at, updated_at \
 FROM users ORDER BY created_at ASC";
const SELECT_AVATAR_REF: &str =
    "SELECT avatar_storage_key, avatar_content_type FROM users WHERE id = ?";
const SET_AVATAR: &str = "\
UPDATE users SET avatar_storage_key = ?, avatar_content_type = ?, avatar_design_id = ?, updated_at = ? \
 WHERE id = ? \
 RETURNING id, name, display_name, email, avatar_storage_key, created_at, updated_at";
const CLEAR_AVATAR: &str = "\
UPDATE users SET avatar_storage_key = NULL, avatar_content_type = NULL, avatar_design_id = NULL, updated_at = ? \
 WHERE id = ? \
 RETURNING id, name, display_name, email, avatar_storage_key, created_at, updated_at";

/// A pooled connection to an existing product database offering the
/// user-identity reads and the self-profile write. Holds no clock; `now_ms` is
/// passed into the write by the application, which owns the time source.
#[derive(Clone)]
pub struct SqliteUserStore {
    pool: SqlitePool,
}

/// Startup/driver failures surfaced only to the composition root that opens the
/// store; the port methods hand the driver error over as a `PersistenceFailure`.
#[derive(Debug, Error)]
pub enum SqliteUserStoreError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
}

/// A committed identity column that decodes to a value the domain refuses (the
/// empty string). The product schema's ids are non-empty by construction, so this
/// is a persistence-integrity fault, never coerced to a sentinel.
#[derive(Debug, Error)]
#[error("users.id holds an unusable identifier")]
struct CorruptStoredUserId;

/// A committed `avatar_content_type` that is not one of the admitted image types.
/// The sole production writer validates against the allowlist before storing, so a
/// stored value outside it is a persistence-integrity fault, not a served type.
#[derive(Debug, Error)]
#[error("users.avatar_content_type holds an unsupported media type")]
struct CorruptStoredAvatarContentType;

/// A write that failed in the driver or on a row that cannot be decoded into the
/// domain. Both reach the caller as the source of one `PersistenceFailure`.
#[derive(Debug, Error)]
enum ProfileWriteFailure {
    #[error(transparent)]
    Sql(#[from] sqlx::Error),
    #[error(transparent)]
    Integrity(#[from] CorruptStoredUserId),
    #[error(transparent)]
    Design(#[from] super::avatar_design::DesignInsertFailure),
}

impl SqliteUserStore {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, SqliteUserStoreError> {
        let pool = crate::foundation::database::store_pool(path.as_ref(), BUSY_TIMEOUT).await?;
        Ok(Self { pool })
    }

    /// Construct over an already-open pool so a composition root that shares one
    /// pool across adapters need not reopen the file.
    pub fn from_pool(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub(crate) fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    async fn write_display_name(
        &self,
        user_id: &UserId,
        display_name: &DisplayName,
        now_ms: i64,
    ) -> Result<UserProfileWrite, ProfileWriteFailure> {
        let mut tx = self.pool.begin_with(ROOT_WRITE_BEGIN).await?;

        // The before-image, read under the write lock: the whole current row, so a
        // no-op can be answered without a second read and a real write can report
        // the display it is about to replace.
        let current = sqlx::query(SELECT_PROFILE)
            .bind(user_id.as_str())
            .fetch_optional(&mut *tx)
            .await?;
        let Some(current) = current else {
            tx.commit().await?;
            return Ok(UserProfileWrite::NoSuchUser);
        };
        let prior = decode_user(&current)?;

        // A no-op resolves to no write: the row keeps its `updated_at`, and the
        // committed display already equals the requested one. The core seal reads
        // `prior == committed` as "the display did not move".
        if prior.display_name == display_name.as_str() {
            tx.commit().await?;
            let prior_display_name = prior.display_name.clone();
            return Ok(UserProfileWrite::Committed {
                user: prior,
                prior_display_name,
            });
        }

        let prior_display_name = prior.display_name;
        let committed = sqlx::query(UPDATE_DISPLAY_NAME)
            .bind(display_name.as_str())
            .bind(now_ms)
            .bind(user_id.as_str())
            .fetch_one(&mut *tx)
            .await?;
        let user = decode_user(&committed)?;
        tx.commit().await?;
        Ok(UserProfileWrite::Committed {
            user,
            prior_display_name,
        })
    }

    async fn write_avatar_pointer(
        &self,
        user_id: &UserId,
        pointer: &AvatarBlobPointer,
        content_type: AvatarContentType,
        design: Option<&AvatarDesign>,
        now_ms: i64,
    ) -> Result<AvatarPointerWrite, ProfileWriteFailure> {
        let mut tx = self.pool.begin_with(ROOT_WRITE_BEGIN).await?;
        // The before-image under the write lock: its storage key is the previous
        // blob the orchestration cleans up after the pointer moves.
        let Some(current) = sqlx::query(SELECT_PROFILE)
            .bind(user_id.as_str())
            .fetch_optional(&mut *tx)
            .await?
        else {
            tx.commit().await?;
            return Ok(AvatarPointerWrite::NoSuchUser);
        };
        let prior_key = decode_prior_avatar_key(&current)?;
        let design_id = super::avatar_design::insert_design(&mut tx, design, now_ms).await?;

        let committed = sqlx::query(SET_AVATAR)
            .bind(pointer.as_str())
            .bind(content_type.as_str())
            .bind(design_id)
            .bind(now_ms)
            .bind(user_id.as_str())
            .fetch_one(&mut *tx)
            .await?;
        let user = decode_user(&committed)?;
        tx.commit().await?;
        Ok(AvatarPointerWrite::Committed { user, prior_key })
    }

    async fn clear_avatar_pointer(
        &self,
        user_id: &UserId,
        now_ms: i64,
    ) -> Result<AvatarPointerWrite, ProfileWriteFailure> {
        let mut tx = self.pool.begin_with(ROOT_WRITE_BEGIN).await?;
        let Some(current) = sqlx::query(SELECT_PROFILE)
            .bind(user_id.as_str())
            .fetch_optional(&mut *tx)
            .await?
        else {
            tx.commit().await?;
            return Ok(AvatarPointerWrite::NoSuchUser);
        };
        let prior_key = decode_prior_avatar_key(&current)?;

        // Already clear: skip the write so `updated_at` is not bumped — the no-op
        // the product's `clearAvatar` returns without publishing. The core seal
        // reads `prior_key == None` as "nothing was cleared".
        if prior_key.is_none() {
            let user = decode_user(&current)?;
            tx.commit().await?;
            return Ok(AvatarPointerWrite::Committed {
                user,
                prior_key: None,
            });
        }

        let committed = sqlx::query(CLEAR_AVATAR)
            .bind(now_ms)
            .bind(user_id.as_str())
            .fetch_one(&mut *tx)
            .await?;
        let user = decode_user(&committed)?;
        tx.commit().await?;
        Ok(AvatarPointerWrite::Committed { user, prior_key })
    }
}

/// The previous avatar blob pointer from a `users` before-image, or `None` when
/// the row carried no avatar (absent or empty key — the same fold `decode_user`
/// uses for the revision).
fn decode_prior_avatar_key(
    row: &sqlx::sqlite::SqliteRow,
) -> Result<Option<AvatarBlobPointer>, CorruptStoredUserId> {
    let key: Option<String> = row
        .try_get("avatar_storage_key")
        .map_err(|_| CorruptStoredUserId)?;
    match key.as_deref() {
        None | Some("") => Ok(None),
        Some(key) => Ok(Some(
            AvatarBlobPointer::parse(key).map_err(|_| CorruptStoredUserId)?,
        )),
    }
}

/// Decode a `users` row into the projected [`UserRecord`]. Every identity
/// coordinate is parsed through the domain's fallible parser, so a malformed
/// committed id is corruption, never a product answer.
fn decode_user(row: &sqlx::sqlite::SqliteRow) -> Result<UserRecord, CorruptStoredUserId> {
    let id = UserId::parse(
        row.try_get::<String, _>("id")
            .map_err(|_| CorruptStoredUserId)?,
    )
    .map_err(|_| CorruptStoredUserId)?;
    let updated_at: i64 = row.try_get("updated_at").map_err(|_| CorruptStoredUserId)?;
    // The avatar URL is cache-busted by `updated_at`; an absent or empty storage
    // key means "no avatar", exactly as the messaging author projection folds it.
    let avatar_storage_key: Option<String> = row
        .try_get("avatar_storage_key")
        .map_err(|_| CorruptStoredUserId)?;
    let avatar = match avatar_storage_key.as_deref() {
        None | Some("") => None,
        Some(_) => Some(AvatarRevision::at(updated_at)),
    };
    Ok(UserRecord {
        id,
        handle: row.try_get("name").map_err(|_| CorruptStoredUserId)?,
        display_name: row
            .try_get("display_name")
            .map_err(|_| CorruptStoredUserId)?,
        email: row.try_get("email").map_err(|_| CorruptStoredUserId)?,
        avatar,
        created_at: row.try_get("created_at").map_err(|_| CorruptStoredUserId)?,
        updated_at,
    })
}

impl UserProfileRead for SqliteUserStore {
    async fn get(&self, user_id: &UserId) -> Result<Option<UserRecord>, PersistenceFailure> {
        let row = sqlx::query(SELECT_PROFILE)
            .bind(user_id.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(PersistenceFailure::from_source)?;
        match row {
            Some(row) => Ok(Some(
                decode_user(&row).map_err(PersistenceFailure::from_source)?,
            )),
            None => Ok(None),
        }
    }
}

impl UserEmailLookup for SqliteUserStore {
    async fn find_by_email(&self, email: &str) -> Result<Option<UserId>, PersistenceFailure> {
        let row = sqlx::query(SELECT_ID_BY_EMAIL)
            .bind(email)
            .fetch_optional(&self.pool)
            .await
            .map_err(PersistenceFailure::from_source)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let raw: String = row.try_get("id").map_err(PersistenceFailure::from_source)?;
        let user_id =
            UserId::parse(raw).map_err(|_| PersistenceFailure::from_source(CorruptStoredUserId))?;
        Ok(Some(user_id))
    }
}

impl UserProfileWriteUnitOfWork for SqliteUserStore {
    async fn update_display_name(
        &self,
        user_id: &UserId,
        display_name: &DisplayName,
        now_ms: i64,
    ) -> Result<UserProfileWrite, PersistenceFailure> {
        self.write_display_name(user_id, display_name, now_ms)
            .await
            .map_err(PersistenceFailure::from_source)
    }
}

// IN deduplicates memberships before projecting users; unrelated profiles never leave SQLite.
const LIST_RELATED_PROFILES: &str =
    "SELECT id, name, display_name, email, avatar_storage_key, created_at, updated_at
 FROM users WHERE id IN (
 SELECT wu.user_id FROM workspaces w JOIN workspace_users wu ON wu.workspace_id = w.id
 WHERE w.owner_user_id = ? AND w.owner_agent_id IS NULL)
 ORDER BY created_at ASC, id ASC";

impl RelatedUserDirectoryRead for SqliteUserStore {
    async fn list_related(&self, viewer: &UserId) -> Result<Vec<UserRecord>, PersistenceFailure> {
        let rows = sqlx::query(LIST_RELATED_PROFILES)
            .bind(viewer.as_str())
            .fetch_all(&self.pool)
            .await
            .map_err(PersistenceFailure::from_source)?;
        rows.iter()
            .map(|row| decode_user(row).map_err(PersistenceFailure::from_source))
            .collect()
    }
}

impl UserDirectoryRead for SqliteUserStore {
    async fn list(&self) -> Result<Vec<UserRecord>, PersistenceFailure> {
        let rows = sqlx::query(LIST_PROFILES)
            .fetch_all(&self.pool)
            .await
            .map_err(PersistenceFailure::from_source)?;
        rows.iter()
            .map(|row| decode_user(row).map_err(PersistenceFailure::from_source))
            .collect()
    }
}

impl AvatarStorageRefRead for SqliteUserStore {
    async fn get_avatar_ref(
        &self,
        user_id: &UserId,
    ) -> Result<Option<AvatarStorageRef>, PersistenceFailure> {
        let Some(row) = sqlx::query(SELECT_AVATAR_REF)
            .bind(user_id.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(PersistenceFailure::from_source)?
        else {
            return Ok(None);
        };
        let key: Option<String> = row
            .try_get("avatar_storage_key")
            .map_err(PersistenceFailure::from_source)?;
        let content_type: Option<String> = row
            .try_get("avatar_content_type")
            .map_err(PersistenceFailure::from_source)?;
        // The product's `getAvatarStorageRef` returns null unless BOTH the key
        // and the content type are present (the CHECK couples them, but a defensive
        // read still guards each). An empty key is "no avatar", as elsewhere.
        let (Some(key), Some(content_type)) = (key.filter(|k| !k.is_empty()), content_type) else {
            return Ok(None);
        };
        let pointer = AvatarBlobPointer::parse(key).map_err(PersistenceFailure::from_source)?;
        // The only production writer stores an admitted (allowlist) content type,
        // so a stored value that does not parse is a persistence-integrity fault,
        // surfaced as a safe-500 rather than a silent 404 or a raw served type.
        let content_type = AvatarContentType::parse(&content_type)
            .ok_or_else(|| PersistenceFailure::from_source(CorruptStoredAvatarContentType))?;
        Ok(Some(AvatarStorageRef {
            pointer,
            content_type,
        }))
    }
}

impl AvatarPointerWriteUnitOfWork for SqliteUserStore {
    async fn set_avatar(
        &self,
        user_id: &UserId,
        pointer: &AvatarBlobPointer,
        content_type: AvatarContentType,
        design: Option<&AvatarDesign>,
        now_ms: i64,
    ) -> Result<AvatarPointerWrite, PersistenceFailure> {
        self.write_avatar_pointer(user_id, pointer, content_type, design, now_ms)
            .await
            .map_err(PersistenceFailure::from_source)
    }

    async fn clear_avatar(
        &self,
        user_id: &UserId,
        now_ms: i64,
    ) -> Result<AvatarPointerWrite, PersistenceFailure> {
        self.clear_avatar_pointer(user_id, now_ms)
            .await
            .map_err(PersistenceFailure::from_source)
    }
}

impl EmbeddedProfileSetupStore for SqliteUserStore {
    async fn read_setup(
        &self,
        user_id: &UserId,
    ) -> Result<Option<EmbeddedProfileSetup>, PersistenceFailure> {
        let row = sqlx::query(SELECT_EMBEDDED_PROFILE)
            .bind(user_id.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(PersistenceFailure::from_source)?;
        row.map(|row| {
            Ok(EmbeddedProfileSetup {
                required: row
                    .try_get::<bool, _>("embedded_profile_pending")
                    .map_err(PersistenceFailure::from_source)?,
                user: decode_user(&row).map_err(PersistenceFailure::from_source)?,
            })
        })
        .transpose()
    }

    async fn complete_setup(
        &self,
        user_id: &UserId,
        handle: &ParticipantHandle,
        display_name: &DisplayName,
        now_ms: i64,
    ) -> Result<CompleteEmbeddedProfileSetup, PersistenceFailure> {
        self.complete_embedded_profile(user_id, handle, display_name, now_ms)
            .await
            .map_err(PersistenceFailure::from_source)
    }
}

impl SqliteUserStore {
    async fn complete_embedded_profile(
        &self,
        user_id: &UserId,
        handle: &ParticipantHandle,
        display_name: &DisplayName,
        now_ms: i64,
    ) -> Result<CompleteEmbeddedProfileSetup, ProfileWriteFailure> {
        use silverhand_backend_core::identity_access::user_identity::CompleteEmbeddedProfileSetup as Outcome;
        let mut tx = self.pool.begin_with(ROOT_WRITE_BEGIN).await?;
        let current = sqlx::query("SELECT name, embedded_profile_pending FROM users WHERE id = ?")
            .bind(user_id.as_str())
            .fetch_optional(&mut *tx)
            .await?;
        let Some(current) = current else {
            return Ok(Outcome::NoSuchUser);
        };
        if !current.try_get::<bool, _>("embedded_profile_pending")? {
            return Ok(Outcome::AlreadyCompleted);
        }
        let current_handle: String = current.try_get("name")?;
        if current_handle != handle.as_str()
            && super::handle_namespace::handle_taken(&mut *tx, handle.as_str()).await?
        {
            return Ok(Outcome::HandleTaken);
        }

        let row = sqlx::query(COMPLETE_EMBEDDED_PROFILE)
            .bind(handle.as_str())
            .bind(display_name.as_str())
            .bind(now_ms)
            .bind(user_id.as_str())
            .fetch_one(&mut *tx)
            .await?;
        let user = decode_user(&row)?;
        tx.commit().await?;
        Ok(Outcome::Completed(user))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqliteConnectOptions;
    use sqlx::{Connection, Executor, SqliteConnection};

    const CURRENT_SCHEMA: &str =
        include_str!("../../../../../rust/test-fixtures/migration-baseline/schema-current.sql");

    const SEED_SQL: &str = "\
INSERT INTO users (id, name, display_name, email, created_at, updated_at) \
  VALUES ('user-1', 'alice', 'Alice', 'alice@example.com', 10, 100); \
INSERT INTO users (id, name, display_name, email, created_at, updated_at) \
  VALUES ('user-2', 'bob', 'Bob', NULL, 10, 100); \
INSERT INTO users (id, name, display_name, email, avatar_storage_key, avatar_content_type, created_at, updated_at) \
  VALUES ('user-3', 'carol', 'Carol', 'carol@example.com', 'avatars/carol.png', 'image/png', 10, 250);";

    async fn open_seeded() -> (tempfile::TempDir, SqliteUserStore) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("user.sqlite");
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .foreign_keys(true);
        let mut db = SqliteConnection::connect_with(&options).await.unwrap();
        db.execute(sqlx::raw_sql(CURRENT_SCHEMA)).await.unwrap();
        db.execute(sqlx::raw_sql(SEED_SQL)).await.unwrap();
        db.close().await.unwrap();
        let store = SqliteUserStore::open(&path).await.unwrap();
        (directory, store)
    }

    fn uid(raw: &str) -> UserId {
        UserId::parse(raw).unwrap()
    }

    #[tokio::test]
    async fn related_directory_uses_owner_and_union_including_archived_but_not_agent_homes() {
        let (_dir, store) = open_seeded().await;
        sqlx::raw_sql("INSERT INTO workspaces(id,name,name_key,owner_user_id,archived_at,created_at,updated_at) VALUES
          ('a','A','a','user-1',NULL,1,1), ('b','B','b','user-1',10,1,1),
          ('other','Other','other','user-2',NULL,1,1);
          INSERT INTO workspace_users(workspace_id,user_id,joined_at) VALUES
          ('a','user-1',1),('a','user-2',1),('b','user-2',1),('b','user-3',1),
          ('other','user-1',1);")
          .execute(&store.pool).await.unwrap();
        let ids = |rows: Vec<UserRecord>| {
            rows.into_iter()
                .map(|u| u.id.as_str().to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            ids(store.list_related(&uid("user-1")).await.unwrap()),
            ["user-1", "user-2", "user-3"]
        );
        assert_eq!(
            ids(store.list_related(&uid("user-2")).await.unwrap()),
            ["user-1"]
        );
        assert!(store.list_related(&uid("user-3")).await.unwrap().is_empty());

        sqlx::query("DELETE FROM workspace_users WHERE workspace_id='a' AND user_id='user-2'")
            .execute(&store.pool)
            .await
            .unwrap();
        assert_eq!(store.list_related(&uid("user-1")).await.unwrap().len(), 3);
        sqlx::query("DELETE FROM workspaces WHERE id='b'")
            .execute(&store.pool)
            .await
            .unwrap();
        assert_eq!(
            ids(store.list_related(&uid("user-1")).await.unwrap()),
            ["user-1"]
        );

        sqlx::raw_sql("INSERT INTO hosts(id,name,kind,created_at) VALUES('h','H','local',1);
          INSERT INTO agents(id,name,display_name,runtime,host_id,created_by_user_id,created_at,updated_at)
          VALUES('agent','agent','Agent','claude','h','user-1',1,1);
          INSERT INTO workspaces(id,name,name_key,owner_user_id,owner_agent_id,created_at,updated_at)
          VALUES('home','Home',NULL,'user-1','agent',1,1);
          INSERT INTO workspace_users(workspace_id,user_id,joined_at) VALUES('home','user-3',1);")
          .execute(&store.pool).await.unwrap();
        assert_eq!(
            ids(store.list_related(&uid("user-1")).await.unwrap()),
            ["user-1"]
        );
        // Read the persisted owner pointer on every request; no renderer or process cache.
        sqlx::query("UPDATE workspaces SET owner_user_id='user-2' WHERE id='a'")
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(store.list_related(&uid("user-1")).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn get_projects_the_profile_row() {
        let (_dir, store) = open_seeded().await;
        let record = store.get(&uid("user-1")).await.unwrap().expect("a row");
        assert_eq!(record.id.as_str(), "user-1");
        assert_eq!(record.handle, "alice");
        assert_eq!(record.display_name, "Alice");
        assert_eq!(record.email.as_deref(), Some("alice@example.com"));
        assert_eq!(
            record.avatar, None,
            "no stored avatar key projects no revision"
        );
        assert_eq!(record.created_at, 10);
        assert_eq!(record.updated_at, 100);
    }

    #[tokio::test]
    async fn get_folds_a_stored_avatar_key_into_a_cache_busted_revision() {
        let (_dir, store) = open_seeded().await;
        let record = store.get(&uid("user-3")).await.unwrap().expect("a row");
        // A non-empty storage key yields a revision cache-busted by updated_at.
        assert_eq!(record.avatar, Some(AvatarRevision::at(250)));
        assert_eq!(record.avatar.unwrap().cache_buster(), 250);
    }

    #[tokio::test]
    async fn get_a_profile_only_user_has_a_null_email() {
        let (_dir, store) = open_seeded().await;
        let record = store.get(&uid("user-2")).await.unwrap().unwrap();
        assert_eq!(record.email, None);
    }

    #[tokio::test]
    async fn get_an_unknown_user_is_none() {
        let (_dir, store) = open_seeded().await;
        assert!(store.get(&uid("ghost")).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn find_by_email_is_case_insensitive_and_none_for_unknown() {
        let (_dir, store) = open_seeded().await;
        assert_eq!(
            store
                .find_by_email("ALICE@example.com")
                .await
                .unwrap()
                .unwrap()
                .as_str(),
            "user-1"
        );
        assert!(
            store
                .find_by_email("nobody@example.com")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_real_change_bumps_updated_at_and_witnesses_the_before_image() {
        let (_dir, store) = open_seeded().await;
        let outcome = store
            .update_display_name(
                &uid("user-1"),
                &DisplayName::parse("Alice Cooper").unwrap(),
                777,
            )
            .await
            .unwrap();
        let UserProfileWrite::Committed {
            user,
            prior_display_name,
        } = outcome
        else {
            panic!("expected Committed");
        };
        assert_eq!(user.display_name, "Alice Cooper");
        assert_eq!(user.updated_at, 777);
        assert_eq!(prior_display_name, "Alice");
        // The row on disk carries the new display and bumped updated_at.
        let reread = store.get(&uid("user-1")).await.unwrap().unwrap();
        assert_eq!(reread.display_name, "Alice Cooper");
        assert_eq!(reread.updated_at, 777);
    }

    #[tokio::test]
    async fn a_no_op_skips_the_write_and_leaves_updated_at_untouched() {
        let (_dir, store) = open_seeded().await;
        let outcome = store
            .update_display_name(&uid("user-1"), &DisplayName::parse("Alice").unwrap(), 999)
            .await
            .unwrap();
        let UserProfileWrite::Committed {
            user,
            prior_display_name,
        } = outcome
        else {
            panic!("expected Committed");
        };
        // No write happened: updated_at is still the seeded 100, and the before-image
        // equals the committed display (the core seal reads that as "no move").
        assert_eq!(user.updated_at, 100);
        assert_eq!(prior_display_name, "Alice");
        assert_eq!(
            store.get(&uid("user-1")).await.unwrap().unwrap().updated_at,
            100
        );
    }

    #[tokio::test]
    async fn an_unknown_user_is_no_such_user() {
        let (_dir, store) = open_seeded().await;
        let outcome = store
            .update_display_name(&uid("ghost"), &DisplayName::parse("X").unwrap(), 100)
            .await
            .unwrap();
        assert!(matches!(outcome, UserProfileWrite::NoSuchUser));
    }

    #[tokio::test]
    async fn list_returns_every_user_ordered_by_created_at() {
        let (_dir, store) = open_seeded().await;
        sqlx::query("UPDATE users SET created_at = CASE id WHEN 'user-3' THEN 1 WHEN 'user-1' THEN 2 ELSE 3 END")
            .execute(&store.pool)
            .await
            .unwrap();
        let records = store.list().await.unwrap();
        // Creation order deliberately differs from both insertion and identifier order.
        let ids: Vec<_> = records.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["user-3", "user-1", "user-2"]);
        // The directory read folds avatar keys exactly as the self-profile read.
        assert_eq!(records[0].avatar, Some(AvatarRevision::at(250)));
    }

    #[tokio::test]
    async fn avatar_ref_is_none_without_a_stored_avatar() {
        let (_dir, store) = open_seeded().await;
        assert!(
            store
                .get_avatar_ref(&uid("user-1"))
                .await
                .unwrap()
                .is_none()
        );
        assert!(store.get_avatar_ref(&uid("ghost")).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn avatar_ref_projects_the_stored_pointer_and_typed_content_type() {
        let (_dir, store) = open_seeded().await;
        let stored = store
            .get_avatar_ref(&uid("user-3"))
            .await
            .unwrap()
            .expect("a ref");
        assert_eq!(stored.pointer.as_str(), "avatars/carol.png");
        assert_eq!(stored.content_type, AvatarContentType::Png);
    }

    #[tokio::test]
    async fn set_avatar_records_the_pointer_and_witnesses_the_prior_key() {
        let (_dir, store) = open_seeded().await;
        let pointer = AvatarBlobPointer::parse("avatars/alice.webp".to_string()).unwrap();
        let outcome = store
            .set_avatar(&uid("user-1"), &pointer, AvatarContentType::Webp, None, 500)
            .await
            .unwrap();
        let AvatarPointerWrite::Committed { user, prior_key } = outcome else {
            panic!("expected Committed");
        };
        assert_eq!(user.updated_at, 500);
        assert_eq!(user.avatar, Some(AvatarRevision::at(500)));
        assert_eq!(prior_key, None, "user-1 had no prior avatar");
        // The stored ref now resolves to the new pointer and declared type.
        let stored = store.get_avatar_ref(&uid("user-1")).await.unwrap().unwrap();
        assert_eq!(stored.pointer.as_str(), "avatars/alice.webp");
        assert_eq!(stored.content_type, AvatarContentType::Webp);
    }

    #[tokio::test]
    async fn set_avatar_over_an_existing_one_reports_the_replaced_key() {
        let (_dir, store) = open_seeded().await;
        let pointer = AvatarBlobPointer::parse("avatars/carol-2.jpg".to_string()).unwrap();
        let outcome = store
            .set_avatar(&uid("user-3"), &pointer, AvatarContentType::Jpeg, None, 600)
            .await
            .unwrap();
        let AvatarPointerWrite::Committed { prior_key, .. } = outcome else {
            panic!("expected Committed");
        };
        assert_eq!(
            prior_key.as_ref().map(AvatarBlobPointer::as_str),
            Some("avatars/carol.png"),
            "the replaced blob is reported for cleanup"
        );
    }

    #[tokio::test]
    async fn set_avatar_on_an_unknown_user_is_no_such_user() {
        let (_dir, store) = open_seeded().await;
        let pointer = AvatarBlobPointer::parse("avatars/ghost.png".to_string()).unwrap();
        let outcome = store
            .set_avatar(&uid("ghost"), &pointer, AvatarContentType::Png, None, 100)
            .await
            .unwrap();
        assert!(matches!(outcome, AvatarPointerWrite::NoSuchUser));
    }

    #[tokio::test]
    async fn clear_avatar_removes_the_pointer_and_reports_the_prior_key() {
        let (_dir, store) = open_seeded().await;
        let outcome = store.clear_avatar(&uid("user-3"), 700).await.unwrap();
        let AvatarPointerWrite::Committed { user, prior_key } = outcome else {
            panic!("expected Committed");
        };
        assert_eq!(user.updated_at, 700);
        assert_eq!(user.avatar, None);
        assert_eq!(
            prior_key.as_ref().map(AvatarBlobPointer::as_str),
            Some("avatars/carol.png")
        );
        assert!(
            store
                .get_avatar_ref(&uid("user-3"))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn clear_avatar_when_already_clear_is_a_no_op_without_a_bump() {
        let (_dir, store) = open_seeded().await;
        let outcome = store.clear_avatar(&uid("user-1"), 999).await.unwrap();
        let AvatarPointerWrite::Committed { user, prior_key } = outcome else {
            panic!("expected Committed");
        };
        // No write: updated_at stays the seeded 100, and no prior key was reported,
        // which the core seal reads as "nothing to clear, do not announce".
        assert_eq!(user.updated_at, 100);
        assert_eq!(prior_key, None);
    }

    #[tokio::test]
    async fn clear_avatar_on_an_unknown_user_is_no_such_user() {
        let (_dir, store) = open_seeded().await;
        let outcome = store.clear_avatar(&uid("ghost"), 100).await.unwrap();
        assert!(matches!(outcome, AvatarPointerWrite::NoSuchUser));
    }
}
