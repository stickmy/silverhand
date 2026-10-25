//! SQLite adapter for the regular-workspace METADATA write behind
//! `PATCH /api/workspaces/:id` (rename + description) and
//! `PATCH /api/workspaces/:id/archive`.
//!
//! Like the enrollment store, this is a **pure consumer** of an existing product
//! database: it opens a pool against a schema that already exists and migrates or
//! seeds nothing. It decides nothing on its own either — the governance floor is
//! [`governance_floor_verdict`](silverhand_backend_core::identity_access::access::governance_floor_verdict)
//! over the facts it re-reads UNDER the write lock, and the name-change gate and
//! collision are decided against the domain's own `CanonicalWorkspaceName` fold,
//! so this crate never re-implements the role axis or the name canonicalizer.
//!
//! The whole write is one `BEGIN IMMEDIATE` transaction, so the single writer is
//! reserved before the authoritative row and the actor's role are re-read: a
//! pre-lock boundary verdict can go stale (a demotion may commit in the seam), and
//! the write re-derives authority under its own lock. Existence (and the
//! agent-home fold into the same `NoSuchWorkspace`), the owner/admin floor, the
//! name collision against the regular namespace, and the `updated_at`/`archived_at`
//! writes all resolve under that one lock, and the committed face is projected from
//! the write's own `RETURNING` row — never a post-commit re-read.
//!
//! The clock stays in the application: `now_ms` is passed into each method (the
//! value the always-bumped `updated_at`, and an archive's `archived_at`, take),
//! exactly as the enrollment join time is. Every driver error and every committed
//! row the domain refuses reaches the port's caller as the source of a
//! `PersistenceFailure`; nothing is logged here.

use std::{path::Path, time::Duration};

use silverhand_backend_core::{
    foundation::identity::{AgentId, Principal, UserId, WorkspaceId},
    foundation::persistence::PersistenceFailure,
    identity_access::access::{
        WorkspaceGovernanceFacts, WorkspaceGovernanceFloor, WorkspaceRole, governance_floor_verdict,
    },
    identity_access::workspace_face::WorkspaceFace,
    identity_access::workspace_metadata_write::{
        PriorWorkspaceAddress, WorkspaceArchiveWrite, WorkspaceMetadataEdit,
        WorkspaceMetadataWrite, WorkspaceMetadataWriteUnitOfWork,
    },
};
use sqlx::{Row, Sqlite, SqlitePool, Transaction};
use thiserror::Error;

const ROOT_WRITE_BEGIN: &str = "BEGIN IMMEDIATE";
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

// The authoritative row re-read under the write lock, joined to the actor's role
// row (human only) in the SAME statement, so existence, the home flag, the
// current name/key the change gate compares against, and the caller's governance
// role are one consistent read. The archive path derives no `changed` flag, so the
// pre-write archived state is not read here.
const SELECT_WORKSPACE_FOR_WRITE_HUMAN: &str = "\
SELECT w.owner_agent_id, w.name, w.name_key, u.role \
  FROM workspaces w \
  LEFT JOIN workspace_users u ON u.workspace_id = w.id AND u.user_id = ? \
 WHERE w.id = ?";
// A non-human principal holds no role row; its floor is decided without one.
const SELECT_WORKSPACE_FOR_WRITE_NONHUMAN: &str =
    "SELECT owner_agent_id, name, name_key FROM workspaces WHERE id = ?";

// Collision against the REGULAR namespace only, by the pinned fold key, excluding
// the workspace being renamed — the same partial unique index
// `idx_workspaces_name_key ... WHERE owner_agent_id IS NULL` the create path
// collides against. A row here is the surface's `409`.
const SELECT_NAME_KEY_HOLDER: &str = "\
SELECT 1 FROM workspaces \
 WHERE name_key = ? AND owner_agent_id IS NULL AND id <> ? \
 LIMIT 1";

// The three metadata SET branches, each bumping `updated_at`, mirroring the
// product's `updateMutableMetadata`. All return the committed face columns AND the
// committed `name_key` (the witness the core seal binds against the fold of the
// requested name) so the write projects the fact it produced without a post-commit
// re-read. A regular workspace has no DM channels, so those face fields are
// `None`/empty here.
const UPDATE_NAME_AND_DESCRIPTION: &str = "\
UPDATE workspaces SET name = ?, name_key = ?, description = ?, updated_at = ? \
 WHERE id = ? \
 RETURNING id, name, name_key, description, owner_agent_id, owner_user_id, archived_at, created_at, updated_at";
const UPDATE_NAME: &str = "\
UPDATE workspaces SET name = ?, name_key = ?, updated_at = ? \
 WHERE id = ? \
 RETURNING id, name, name_key, description, owner_agent_id, owner_user_id, archived_at, created_at, updated_at";
const UPDATE_DESCRIPTION: &str = "\
UPDATE workspaces SET description = ?, updated_at = ? \
 WHERE id = ? \
 RETURNING id, name, name_key, description, owner_agent_id, owner_user_id, archived_at, created_at, updated_at";
const UPDATE_ARCHIVED_AT: &str = "\
UPDATE workspaces SET archived_at = ?, updated_at = ? \
 WHERE id = ? \
 RETURNING id, name, description, owner_agent_id, owner_user_id, archived_at, created_at, updated_at";

/// The write-locked read of a human actor: `(owner_agent_id, name, name_key,
/// role)` — the role column is the LEFT JOIN's, absent for a non-member.
type LockedWorkspaceRowHuman = (Option<String>, String, Option<String>, Option<String>);
/// The write-locked read of a non-human actor: the same row without a role, since
/// only a human holds a `workspace_users` role.
type LockedWorkspaceRowNonHuman = (Option<String>, String, Option<String>);

/// A pooled connection to an existing product database, offering the
/// regular-workspace metadata/archive write. Holds no clock; `now_ms` is passed
/// in by the application, which owns the time source.
#[derive(Clone)]
pub struct SqliteWorkspaceMetadataWrite {
    pool: SqlitePool,
}

/// Startup and driver failures surfaced only to the composition root that opens
/// the store. The unit-of-work methods hand the driver error over as the source
/// of a [`PersistenceFailure`] instead.
#[derive(Debug, Error)]
pub enum SqliteWorkspaceMetadataWriteError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
}

/// A committed row whose identifier column decodes to a value the domain refuses
/// (today only the empty string). The product schema's ids are non-empty by
/// construction, so such a row is a persistence-integrity fault surfaced as the
/// source of a [`PersistenceFailure`], never coerced to a sentinel.
#[derive(Debug, Error)]
#[error("workspaces.{column} holds an unusable identifier")]
struct CorruptStoredIdentifier {
    column: &'static str,
}

/// A committed `workspace_users.role` outside the schema's `CHECK` set. Committed
/// rows cannot hold one, so this is a persistence-integrity fault, never coerced
/// to a default.
#[derive(Debug, Error)]
#[error("workspace_users.role holds an unrecognized value: {0}")]
struct CorruptRoleToken(String);

/// A write that failed in the driver or on a row that cannot be decoded into the
/// domain. Both reach the caller as the source of one `PersistenceFailure`; the
/// split lets an integrity fault name its exact column rather than masquerade as a
/// driver error.
#[derive(Debug, Error)]
enum MetadataWriteFailure {
    #[error(transparent)]
    Sql(#[from] sqlx::Error),
    #[error(transparent)]
    Integrity(#[from] CorruptStoredIdentifier),
    #[error(transparent)]
    Role(#[from] CorruptRoleToken),
}

/// The authoritative row re-read under the write lock: the current display name
/// and fold key (the change gate compares against these), and the caller's
/// governance facts. An agent-home or missing row never reaches this struct — both
/// fold into the `None` the callers turn into `NoSuchWorkspace`.
struct LockedWorkspace {
    current_name: String,
    current_name_key: Option<String>,
    facts: WorkspaceGovernanceFacts,
}

impl SqliteWorkspaceMetadataWrite {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, SqliteWorkspaceMetadataWriteError> {
        let pool = crate::foundation::database::store_pool(path.as_ref(), BUSY_TIMEOUT).await?;
        Ok(Self { pool })
    }

    /// Construct over an already-open pool, so a composition root that shares one
    /// pool across adapters need not reopen the file.
    pub fn from_pool(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Re-read the authoritative row and the actor's role under the write lock. A
    /// missing row, or an agent-home row, both fold into `None` — the same
    /// `NoSuchWorkspace` the caller cannot tell apart.
    async fn locked_workspace(
        tx: &mut Transaction<'_, Sqlite>,
        actor: &Principal,
        workspace_id: &WorkspaceId,
    ) -> Result<Option<LockedWorkspace>, MetadataWriteFailure> {
        let (owner_agent_id, name, name_key, role_token) = match actor {
            Principal::Human { user_id } => {
                let row: Option<LockedWorkspaceRowHuman> =
                    sqlx::query_as(SELECT_WORKSPACE_FOR_WRITE_HUMAN)
                        .bind(user_id.as_str())
                        .bind(workspace_id.as_str())
                        .fetch_optional(&mut **tx)
                        .await?;
                match row {
                    Some((owner_agent_id, name, name_key, role_token)) => {
                        (owner_agent_id, name, name_key, role_token)
                    }
                    None => return Ok(None),
                }
            }
            Principal::System | Principal::Agent { .. } => {
                let row: Option<LockedWorkspaceRowNonHuman> =
                    sqlx::query_as(SELECT_WORKSPACE_FOR_WRITE_NONHUMAN)
                        .bind(workspace_id.as_str())
                        .fetch_optional(&mut **tx)
                        .await?;
                match row {
                    Some((owner_agent_id, name, name_key)) => {
                        (owner_agent_id, name, name_key, None)
                    }
                    None => return Ok(None),
                }
            }
        };

        // A row with a home agent is that agent's private container, which has no
        // roster to govern — the same "no such workspace" as a missing row.
        if owner_agent_id.is_some() {
            return Ok(None);
        }

        let actor_role = match role_token {
            Some(token) => Some(WorkspaceRole::parse(&token).ok_or(CorruptRoleToken(token))?),
            None => None,
        };
        Ok(Some(LockedWorkspace {
            current_name: name,
            current_name_key: name_key,
            facts: WorkspaceGovernanceFacts::Regular { actor_role },
        }))
    }

    async fn write_metadata(
        &self,
        actor: Principal,
        workspace_id: &WorkspaceId,
        edit: &WorkspaceMetadataEdit,
        now_ms: i64,
    ) -> Result<WorkspaceMetadataWrite, MetadataWriteFailure> {
        let mut tx = self.pool.begin_with(ROOT_WRITE_BEGIN).await?;

        let Some(locked) = Self::locked_workspace(&mut tx, &actor, workspace_id).await? else {
            return Ok(WorkspaceMetadataWrite::NoSuchWorkspace);
        };

        // Rename/description requires the ADMIN floor.
        if governance_floor_verdict(&actor, locked.facts, WorkspaceRole::Admin)
            != WorkspaceGovernanceFloor::Governs
        {
            return Ok(WorkspaceMetadataWrite::Forbidden);
        }

        // Collision against the regular namespace, by the requested fold key,
        // excluding this workspace — the surface's `409`. Only a name-bearing edit
        // can collide.
        if let Some(name) = edit.requested_name() {
            let taken: Option<i64> = sqlx::query_scalar(SELECT_NAME_KEY_HOLDER)
                .bind(name.key())
                .bind(workspace_id.as_str())
                .fetch_optional(&mut *tx)
                .await?;
            if taken.is_some() {
                return Ok(WorkspaceMetadataWrite::NameTaken);
            }
        }

        // The write's before-image WITNESS, read under this same lock: the display
        // and fold key the workspace addressed by BEFORE the write. The core seal
        // derives the name-change gate from this against the committed face — this
        // adapter decides nothing. A regular workspace always carries a `name_key`;
        // a NULL here is a corrupt row, never a silent empty key.
        let prior = PriorWorkspaceAddress {
            name: locked.current_name.clone(),
            key: locked
                .current_name_key
                .clone()
                .ok_or(CorruptStoredIdentifier { column: "name_key" })?,
        };

        let row = match edit {
            WorkspaceMetadataEdit::NameAndDescription { name, description } => {
                sqlx::query(UPDATE_NAME_AND_DESCRIPTION)
                    .bind(name.name())
                    .bind(name.key())
                    .bind(description.as_deref())
                    .bind(now_ms)
                    .bind(workspace_id.as_str())
                    .fetch_one(&mut *tx)
                    .await?
            }
            WorkspaceMetadataEdit::Name(name) => {
                sqlx::query(UPDATE_NAME)
                    .bind(name.name())
                    .bind(name.key())
                    .bind(now_ms)
                    .bind(workspace_id.as_str())
                    .fetch_one(&mut *tx)
                    .await?
            }
            WorkspaceMetadataEdit::Description(description) => {
                sqlx::query(UPDATE_DESCRIPTION)
                    .bind(description.as_deref())
                    .bind(now_ms)
                    .bind(workspace_id.as_str())
                    .fetch_one(&mut *tx)
                    .await?
            }
        };
        let face = decode_face(&row)?;
        // The committed fold key from the write's own `RETURNING` row — the second
        // witness the seal binds against the fold of the requested name. A regular
        // workspace's `name_key` is NOT NULL, so a NULL here is a corrupt committed
        // row, not a product answer.
        let committed_name_key = row
            .try_get::<Option<String>, _>("name_key")?
            .ok_or(CorruptStoredIdentifier { column: "name_key" })?;

        tx.commit().await?;
        Ok(WorkspaceMetadataWrite::Committed {
            face,
            prior,
            committed_name_key,
        })
    }

    async fn write_archived(
        &self,
        actor: Principal,
        workspace_id: &WorkspaceId,
        archived: bool,
        now_ms: i64,
    ) -> Result<WorkspaceArchiveWrite, MetadataWriteFailure> {
        let mut tx = self.pool.begin_with(ROOT_WRITE_BEGIN).await?;

        let Some(locked) = Self::locked_workspace(&mut tx, &actor, workspace_id).await? else {
            return Ok(WorkspaceArchiveWrite::NoSuchWorkspace);
        };

        // Archive requires the OWNER floor — stricter than rename's admin.
        if governance_floor_verdict(&actor, locked.facts, WorkspaceRole::Owner)
            != WorkspaceGovernanceFloor::Governs
        {
            return Ok(WorkspaceArchiveWrite::Forbidden);
        }

        // `archived_at` is the write instant when archiving, NULL when un-archiving;
        // `updated_at` always bumps, matching the product's `setArchived`. The write
        // commits regardless of whether the archived state actually moved — archive
        // owes no fan-out, so no `changed` flag is derived or carried.
        let archived_at = archived.then_some(now_ms);
        let row = sqlx::query(UPDATE_ARCHIVED_AT)
            .bind(archived_at)
            .bind(now_ms)
            .bind(workspace_id.as_str())
            .fetch_one(&mut *tx)
            .await?;
        let face = decode_face(&row)?;

        tx.commit().await?;
        Ok(WorkspaceArchiveWrite::Committed { face })
    }
}

/// Decode the committed regular-workspace face from a write's `RETURNING` row.
/// A regular workspace has no DM channels, so those face fields are `None`/empty;
/// every identity coordinate is parsed through the domain's fallible parser, so a
/// malformed committed id is corruption, never a product answer.
fn decode_face(row: &sqlx::sqlite::SqliteRow) -> Result<WorkspaceFace, MetadataWriteFailure> {
    let id = WorkspaceId::parse(row.try_get::<String, _>("id")?)
        .map_err(|_| CorruptStoredIdentifier { column: "id" })?;
    let owner_agent_id = match row.try_get::<Option<String>, _>("owner_agent_id")? {
        Some(raw) => Some(AgentId::parse(raw).map_err(|_| CorruptStoredIdentifier {
            column: "owner_agent_id",
        })?),
        None => None,
    };
    let owner_user_id = match row.try_get::<Option<String>, _>("owner_user_id")? {
        Some(raw) => Some(UserId::parse(raw).map_err(|_| CorruptStoredIdentifier {
            column: "owner_user_id",
        })?),
        None => None,
    };
    Ok(WorkspaceFace {
        id,
        name: row.try_get("name")?,
        description: row.try_get("description")?,
        owner_agent_id,
        owner_user_id,
        dm_channel_id: None,
        dm_channel_ids: Vec::new(),
        archived_at: row.try_get("archived_at")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

impl WorkspaceMetadataWriteUnitOfWork for SqliteWorkspaceMetadataWrite {
    async fn update_metadata(
        &self,
        actor: Principal,
        workspace_id: &WorkspaceId,
        edit: &WorkspaceMetadataEdit,
        now_ms: i64,
    ) -> Result<WorkspaceMetadataWrite, PersistenceFailure> {
        self.write_metadata(actor, workspace_id, edit, now_ms)
            .await
            .map_err(PersistenceFailure::from_source)
    }

    async fn set_archived(
        &self,
        actor: Principal,
        workspace_id: &WorkspaceId,
        archived: bool,
        now_ms: i64,
    ) -> Result<WorkspaceArchiveWrite, PersistenceFailure> {
        self.write_archived(actor, workspace_id, archived, now_ms)
            .await
            .map_err(PersistenceFailure::from_source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use silverhand_backend_core::identity_access::workspace_name::CanonicalWorkspaceName;
    use sqlx::sqlite::SqliteConnectOptions;
    use sqlx::{Connection, Executor, SqliteConnection};

    const CURRENT_SCHEMA: &str =
        include_str!("../../../../../rust/test-fixtures/migration-baseline/schema-current.sql");

    // A regular workspace `ws-1` (owner user-1, admin user-2, member user-3), a
    // second regular workspace `ws-2` holding the fold key `taken` for the
    // collision case, and an agent-home `ws-home` that must read as NoSuchWorkspace.
    const SEED_SQL: &str = "\
INSERT INTO users (id, name, display_name, created_at, updated_at) \
  VALUES ('user-1', 'owner', 'Owner', 1, 1); \
INSERT INTO users (id, name, display_name, created_at, updated_at) \
  VALUES ('user-2', 'admin', 'Admin', 1, 1); \
INSERT INTO users (id, name, display_name, created_at, updated_at) \
  VALUES ('user-3', 'member', 'Member', 1, 1); \
INSERT INTO users (id, name, display_name, created_at, updated_at) \
  VALUES ('user-9', 'stranger', 'Stranger', 1, 1); \
INSERT INTO hosts (id, name, kind, created_at) VALUES ('host-1', 'Host', 'local', 1); \
INSERT INTO agents (id, name, display_name, runtime, host_id, created_by_user_id, created_at, updated_at) \
  VALUES ('agent-1', 'alice', 'Alice', 'claude', 'host-1', 'user-1', 1, 1); \
INSERT INTO workspaces (id, name, name_key, description, owner_agent_id, owner_user_id, archived_at, created_at, updated_at) \
  VALUES ('ws-1', 'Original', 'original', 'first', NULL, 'user-1', NULL, 10, 100); \
INSERT INTO workspaces (id, name, name_key, description, owner_agent_id, owner_user_id, archived_at, created_at, updated_at) \
  VALUES ('ws-2', 'Taken', 'taken', NULL, NULL, 'user-1', NULL, 10, 100); \
INSERT INTO workspaces (id, name, name_key, description, owner_agent_id, owner_user_id, archived_at, created_at, updated_at) \
  VALUES ('ws-home', 'Agent Home', NULL, NULL, 'agent-1', NULL, NULL, 10, 100); \
INSERT INTO workspace_users (workspace_id, user_id, joined_at, role) VALUES ('ws-1', 'user-1', 1, 'owner'); \
INSERT INTO workspace_users (workspace_id, user_id, joined_at, role) VALUES ('ws-1', 'user-2', 1, 'admin'); \
INSERT INTO workspace_users (workspace_id, user_id, joined_at, role) VALUES ('ws-1', 'user-3', 1, 'member');";

    async fn open_seeded() -> (tempfile::TempDir, SqliteWorkspaceMetadataWrite) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("workspace-metadata-write.sqlite");
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .foreign_keys(true);
        let mut db = SqliteConnection::connect_with(&options).await.unwrap();
        db.execute(sqlx::raw_sql(CURRENT_SCHEMA)).await.unwrap();
        db.execute(sqlx::raw_sql(SEED_SQL)).await.unwrap();
        db.close().await.unwrap();
        let adapter = SqliteWorkspaceMetadataWrite::open(&path).await.unwrap();
        (directory, adapter)
    }

    fn ws(id: &str) -> WorkspaceId {
        WorkspaceId::parse(id).unwrap()
    }

    fn human(id: &str) -> Principal {
        Principal::Human {
            user_id: UserId::parse(id).unwrap(),
        }
    }

    fn name(raw: &str) -> CanonicalWorkspaceName {
        CanonicalWorkspaceName::parse(raw).unwrap()
    }

    async fn stored_name_key(
        adapter: &SqliteWorkspaceMetadataWrite,
        id: &str,
    ) -> (String, Option<String>) {
        sqlx::query_as::<_, (String, Option<String>)>(
            "SELECT name, name_key FROM workspaces WHERE id = ?",
        )
        .bind(id)
        .fetch_one(&adapter.pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn a_rename_commits_the_pair_bumps_updated_at_and_witnesses_the_before_image() {
        let (_dir, adapter) = open_seeded().await;
        let edit = WorkspaceMetadataEdit::Name(name("Design Team"));
        let outcome = adapter
            .update_metadata(human("user-1"), &ws("ws-1"), &edit, 777)
            .await
            .unwrap();
        let WorkspaceMetadataWrite::Committed {
            face,
            prior,
            committed_name_key,
        } = outcome
        else {
            panic!("expected Committed, got {outcome:?}");
        };
        assert_eq!(face.name, "Design Team");
        assert_eq!(face.updated_at, 777);
        assert_eq!(face.owner_agent_id, None);
        assert_eq!(face.dm_channel_ids, Vec::<_>::new());
        // The adapter reports only what it read/committed; the core derives the gate.
        assert_eq!(prior.name, "Original");
        assert_eq!(prior.key, "original");
        assert_eq!(committed_name_key, name("Design Team").key());
        assert_eq!(
            stored_name_key(&adapter, "ws-1").await,
            (
                "Design Team".to_owned(),
                Some(name("Design Team").key().to_owned())
            )
        );
    }

    #[tokio::test]
    async fn a_description_only_edit_commits_and_leaves_the_address_untouched() {
        let (_dir, adapter) = open_seeded().await;
        let edit = WorkspaceMetadataEdit::Description(Some("second".to_owned()));
        let outcome = adapter
            .update_metadata(human("user-1"), &ws("ws-1"), &edit, 500)
            .await
            .unwrap();
        let WorkspaceMetadataWrite::Committed {
            face,
            prior,
            committed_name_key,
        } = outcome
        else {
            panic!("expected Committed");
        };
        assert_eq!(face.description.as_deref(), Some("second"));
        assert_eq!(face.name, "Original");
        assert_eq!(face.updated_at, 500);
        // A description-only edit re-addresses nothing: the committed key equals the
        // before-image key, which is how the core seal proves no rename happened.
        assert_eq!(prior.name, "Original");
        assert_eq!(prior.key, "original");
        assert_eq!(committed_name_key, "original");
    }

    #[tokio::test]
    async fn a_rename_to_the_same_display_commits_and_witnesses_an_unchanged_address() {
        let (_dir, adapter) = open_seeded().await;
        // NFC/trim folds to the same display "Original"; the write still commits and
        // bumps updated_at. The before-image display equals the committed display, so
        // the core derives no gate.
        let edit = WorkspaceMetadataEdit::Name(name("  Original  "));
        let outcome = adapter
            .update_metadata(human("user-1"), &ws("ws-1"), &edit, 600)
            .await
            .unwrap();
        let WorkspaceMetadataWrite::Committed {
            face,
            prior,
            committed_name_key,
        } = outcome
        else {
            panic!("expected Committed");
        };
        assert_eq!(face.name, "Original");
        assert_eq!(face.updated_at, 600);
        assert_eq!(prior.name, "Original");
        assert_eq!(committed_name_key, name("Original").key());
    }

    #[tokio::test]
    async fn a_name_held_by_another_regular_workspace_is_name_taken() {
        let (_dir, adapter) = open_seeded().await;
        let edit = WorkspaceMetadataEdit::Name(name("Taken"));
        let outcome = adapter
            .update_metadata(human("user-1"), &ws("ws-1"), &edit, 700)
            .await
            .unwrap();
        assert_eq!(outcome, WorkspaceMetadataWrite::NameTaken);
        // Nothing was written: the row keeps its original address.
        assert_eq!(stored_name_key(&adapter, "ws-1").await.0, "Original");
    }

    #[tokio::test]
    async fn an_admin_may_rename_but_not_archive() {
        let (_dir, adapter) = open_seeded().await;
        let rename = adapter
            .update_metadata(
                human("user-2"),
                &ws("ws-1"),
                &WorkspaceMetadataEdit::Name(name("Renamed")),
                800,
            )
            .await
            .unwrap();
        assert!(matches!(rename, WorkspaceMetadataWrite::Committed { .. }));

        let archive = adapter
            .set_archived(human("user-2"), &ws("ws-1"), true, 810)
            .await
            .unwrap();
        assert_eq!(archive, WorkspaceArchiveWrite::Forbidden);
    }

    #[tokio::test]
    async fn a_plain_member_is_forbidden_from_renaming() {
        let (_dir, adapter) = open_seeded().await;
        let outcome = adapter
            .update_metadata(
                human("user-3"),
                &ws("ws-1"),
                &WorkspaceMetadataEdit::Name(name("Nope")),
                900,
            )
            .await
            .unwrap();
        assert_eq!(outcome, WorkspaceMetadataWrite::Forbidden);
    }

    #[tokio::test]
    async fn a_non_member_is_forbidden() {
        let (_dir, adapter) = open_seeded().await;
        let outcome = adapter
            .update_metadata(
                human("user-9"),
                &ws("ws-1"),
                &WorkspaceMetadataEdit::Name(name("Nope")),
                900,
            )
            .await
            .unwrap();
        assert_eq!(outcome, WorkspaceMetadataWrite::Forbidden);
    }

    #[tokio::test]
    async fn a_missing_or_home_workspace_is_no_such_workspace() {
        let (_dir, adapter) = open_seeded().await;
        for id in ["ws-missing", "ws-home"] {
            let outcome = adapter
                .update_metadata(
                    human("user-1"),
                    &ws(id),
                    &WorkspaceMetadataEdit::Name(name("X")),
                    100,
                )
                .await
                .unwrap();
            assert_eq!(outcome, WorkspaceMetadataWrite::NoSuchWorkspace, "{id}");
            let archive = adapter
                .set_archived(human("user-1"), &ws(id), true, 100)
                .await
                .unwrap();
            assert_eq!(archive, WorkspaceArchiveWrite::NoSuchWorkspace, "{id}");
        }
    }

    #[tokio::test]
    async fn archiving_sets_the_instant_bumps_updated_at_and_is_idempotent() {
        let (_dir, adapter) = open_seeded().await;
        let first = adapter
            .set_archived(human("user-1"), &ws("ws-1"), true, 1234)
            .await
            .unwrap();
        let WorkspaceArchiveWrite::Committed { face } = first else {
            panic!("expected Committed");
        };
        assert_eq!(face.archived_at, Some(1234));
        assert_eq!(face.updated_at, 1234);

        // Archiving again commits and re-stamps the instant (no flip is tracked).
        let again = adapter
            .set_archived(human("user-1"), &ws("ws-1"), true, 1300)
            .await
            .unwrap();
        let WorkspaceArchiveWrite::Committed { face } = again else {
            panic!("expected Committed");
        };
        assert_eq!(face.archived_at, Some(1300));

        // Un-archiving clears archived_at and bumps updated_at.
        let cleared = adapter
            .set_archived(human("user-1"), &ws("ws-1"), false, 1400)
            .await
            .unwrap();
        let WorkspaceArchiveWrite::Committed { face } = cleared else {
            panic!("expected Committed");
        };
        assert_eq!(face.archived_at, None);
        assert_eq!(face.updated_at, 1400);
    }
}
