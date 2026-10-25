//! The workspace-delete authority uses one `BEGIN IMMEDIATE` transaction:
//! read the subject, decide the verdict, capture the teardown plan and cascade
//! the row under the same lock. Refusals never produce a teardown plan.
//!
//! The delete row uses the SAME `owner_agent_id IS NULL` guard the reference
//! `workspaces.delete` uses, so an agent-home can never be removed even if the
//! verdict were somehow wrong. The captured plan (conversations to terminalize,
//! seats to stop, blobs to sweep, viewer members to notify) is read in the SAME
//! transaction as the DELETE, just before the cascade erases it, and handed back
//! inside the `Committed(plan)` verdict for the route's post-commit effects.

use silverhand_backend_core::{
    foundation::blob_store::{BlobStorageKey, StorageKind, StoredBlob},
    foundation::identity::{AgentId, ConversationId, UserId, WorkspaceId},
    foundation::persistence::PersistenceFailure,
    identity_access::{
        access::WorkspaceRole,
        workspace_deletion::{
            WorkspaceDeletionOutcome, WorkspaceDeletionSubject, WorkspaceDeletionVerdict,
            WorkspaceDeletionWrite, WorkspaceRuntimeSeat, WorkspaceTeardownPlan,
            decide_workspace_deletion,
        },
    },
};
use sqlx::{Row, Sqlite, Transaction};
use thiserror::Error;

use crate::conversation_messaging::messaging::MessagingStore;

/// The delete write authority — a shared-pool clone of the ONE messaging store
/// pool (the single writer). It opens no second connection authority.
#[derive(Clone)]
pub struct SqliteWorkspaceDeletion {
    pool: crate::foundation::database::SqliteDatabase,
}

impl<E> MessagingStore<E> {
    /// The workspace-delete write authority, sharing this store's pool.
    pub fn workspace_deletion(&self) -> SqliteWorkspaceDeletion {
        SqliteWorkspaceDeletion {
            pool: self.pool().clone(),
        }
    }
}

impl SqliteWorkspaceDeletion {
    /// Construct directly over a pool — for composition tests / integration
    /// harnesses that already hold the shared pool.
    pub fn from_pool(pool: impl Into<crate::foundation::database::SqliteDatabase>) -> Self {
        let pool = pool.into();
        Self { pool }
    }
}

#[derive(Debug, Error)]
pub enum WorkspaceDeletionStoreError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("stored workspace_users.role is outside the closed set: {0}")]
    UnknownRole(String),
    #[error("stored teardown identity {field} is malformed")]
    MalformedIdentity { field: &'static str },
    #[error("stored attachment storage kind is outside the closed set: {0}")]
    UnknownStorageKind(String),
    #[error("stored attachment storage key is malformed")]
    MalformedStorageKey,
}

impl From<WorkspaceDeletionStoreError> for PersistenceFailure {
    fn from(value: WorkspaceDeletionStoreError) -> Self {
        Self::from_source(value)
    }
}

const ROOT_WRITE_BEGIN: &str = "BEGIN IMMEDIATE";

/// The workspace kind + the requester's role, read under the write lock.
const SELECT_WORKSPACE_FOR_DELETE: &str = "\
    SELECT w.owner_agent_id AS owner_agent_id, u.role AS role \
      FROM workspaces w \
      LEFT JOIN workspace_users u ON u.workspace_id = w.id AND u.user_id = ? \
     WHERE w.id = ?";

/// The workspace's conversation-scoped runtime seats, captured before cascade.
/// Only `session_scope = 'conversation'` agents (ConversationSlot) — AgentSlot
/// seats are not workspace-scoped and are excluded, matching the reference.
const SELECT_WORKSPACE_RUNTIME_SEATS: &str = "\
    SELECT ca.conversation_id AS conversation_id, ca.agent_id AS agent_id \
      FROM conversation_agents ca \
      JOIN conversations c ON c.id = ca.conversation_id \
      JOIN agents a ON a.id = ca.agent_id \
     WHERE c.workspace_id = ? AND a.session_scope = 'conversation'";

/// Every attachment stored blob in the workspace's conversations, captured before
/// cascade so the storage keys survive for the post-commit sweep.
const SELECT_WORKSPACE_ATTACHMENT_BLOBS: &str = "\
    SELECT att.storage AS storage, att.storage_key AS storage_key \
      FROM attachments att \
      JOIN conversations c ON c.id = att.channel_id \
     WHERE c.workspace_id = ?";

/// The workspace's human members (for the `viewer_workspaces_changed` fan-out).
const SELECT_WORKSPACE_VIEWERS: &str = "SELECT user_id FROM workspace_users WHERE workspace_id = ?";
const SELECT_WORKSPACE_CONVERSATION_IDS: &str =
    "SELECT id FROM conversations WHERE workspace_id = ?";

/// The row delete, guarded by `owner_agent_id IS NULL` — a regular workspace
/// only. FK cascade removes its conversations, members, and capabilities.
const DELETE_WORKSPACE: &str = "DELETE FROM workspaces WHERE id = ? AND owner_agent_id IS NULL";

impl WorkspaceDeletionWrite for SqliteWorkspaceDeletion {
    async fn delete_workspace(
        &self,
        requester: &UserId,
        workspace_id: &WorkspaceId,
    ) -> Result<WorkspaceDeletionOutcome, PersistenceFailure> {
        // ONE `BEGIN IMMEDIATE` tx is the sole authority. Read the subject
        // under the lock (never a stale pre-lock witness), decide the full verdict, and
        // ONLY on `Deletable` capture the FULL plan (ordered BEFORE the DELETE so the
        // cascade has not yet erased the rows) then cascade — all in this one tx. Each
        // distinct product result maps to a DISTINCT outcome: a concurrent winner that
        // removed the row before this lock is read as `Missing`→`IdempotentNoop`; an
        // owner change as `Forbidden`; a kind change as `NotDeletableKind`. The
        // conditional DELETE removing no row (owner_agent_id race) collapses to the same
        // idempotent no-op — the workspace is gone, so the caller's intent already holds.
        let mut tx = self
            .pool
            .begin_with(ROOT_WRITE_BEGIN)
            .await
            .map_err(WorkspaceDeletionStoreError::from)?;
        let subject = read_subject(&mut tx, requester, workspace_id).await?;
        let outcome = match decide_workspace_deletion(&subject) {
            WorkspaceDeletionVerdict::IdempotentNoop => WorkspaceDeletionOutcome::IdempotentNoop,
            WorkspaceDeletionVerdict::NotDeletableKind => {
                WorkspaceDeletionOutcome::NotDeletableKind
            }
            WorkspaceDeletionVerdict::Forbidden => WorkspaceDeletionOutcome::Forbidden,
            WorkspaceDeletionVerdict::Deletable => {
                // Capture the plan FIRST (rows still present), then cascade — same tx.
                let plan = capture_plan(&mut tx, workspace_id).await?;
                let deleted = sqlx::query(DELETE_WORKSPACE)
                    .bind(workspace_id.as_str())
                    .execute(&mut *tx)
                    .await
                    .map_err(WorkspaceDeletionStoreError::from)?
                    .rows_affected()
                    == 1;
                if deleted {
                    WorkspaceDeletionOutcome::Committed(plan)
                } else {
                    // The `owner_agent_id IS NULL` guard matched no row under the lock
                    // (an agent-home guard, or a concurrent winner): idempotent no-op.
                    WorkspaceDeletionOutcome::IdempotentNoop
                }
            }
        };
        tx.commit()
            .await
            .map_err(WorkspaceDeletionStoreError::from)?;
        Ok(outcome)
    }
}

async fn read_subject(
    tx: &mut Transaction<'_, Sqlite>,
    requester: &UserId,
    workspace_id: &WorkspaceId,
) -> Result<WorkspaceDeletionSubject, PersistenceFailure> {
    let Some(row) = sqlx::query(SELECT_WORKSPACE_FOR_DELETE)
        .bind(requester.as_str())
        .bind(workspace_id.as_str())
        .fetch_optional(&mut **tx)
        .await
        .map_err(WorkspaceDeletionStoreError::from)?
    else {
        return Ok(WorkspaceDeletionSubject::Missing);
    };
    let owner_agent_id: Option<String> = row
        .try_get("owner_agent_id")
        .map_err(WorkspaceDeletionStoreError::from)?;
    if owner_agent_id.is_some() {
        return Ok(WorkspaceDeletionSubject::AgentHome);
    }
    let role_token: Option<String> = row
        .try_get("role")
        .map_err(WorkspaceDeletionStoreError::from)?;
    let actor_role = match role_token {
        None => None,
        Some(token) => Some(
            WorkspaceRole::parse(&token).ok_or(WorkspaceDeletionStoreError::UnknownRole(token))?,
        ),
    };
    Ok(WorkspaceDeletionSubject::Regular { actor_role })
}

async fn capture_plan(
    tx: &mut Transaction<'_, Sqlite>,
    workspace_id: &WorkspaceId,
) -> Result<WorkspaceTeardownPlan, PersistenceFailure> {
    // Conversation-scoped seats (ConversationSlot only), captured before cascade
    // erases `conversation_agents`.
    let seat_rows = sqlx::query(SELECT_WORKSPACE_RUNTIME_SEATS)
        .bind(workspace_id.as_str())
        .fetch_all(&mut **tx)
        .await
        .map_err(WorkspaceDeletionStoreError::from)?;
    let mut runtime_seats = Vec::with_capacity(seat_rows.len());
    for row in &seat_rows {
        let conversation: String = row
            .try_get("conversation_id")
            .map_err(WorkspaceDeletionStoreError::from)?;
        let agent: String = row
            .try_get("agent_id")
            .map_err(WorkspaceDeletionStoreError::from)?;
        runtime_seats.push(WorkspaceRuntimeSeat {
            conversation: ConversationId::parse(conversation).map_err(|_| {
                WorkspaceDeletionStoreError::MalformedIdentity {
                    field: "conversation_agents.conversation_id",
                }
            })?,
            agent: AgentId::parse(agent).map_err(|_| {
                WorkspaceDeletionStoreError::MalformedIdentity {
                    field: "conversation_agents.agent_id",
                }
            })?,
        });
    }

    // Attachment stored blobs, captured before cascade erases the rows so their
    // storage keys survive for the post-commit sweep.
    let blob_rows = sqlx::query(SELECT_WORKSPACE_ATTACHMENT_BLOBS)
        .bind(workspace_id.as_str())
        .fetch_all(&mut **tx)
        .await
        .map_err(WorkspaceDeletionStoreError::from)?;
    let mut blobs = Vec::with_capacity(blob_rows.len());
    for row in &blob_rows {
        let storage_raw: String = row
            .try_get("storage")
            .map_err(WorkspaceDeletionStoreError::from)?;
        let key_raw: String = row
            .try_get("storage_key")
            .map_err(WorkspaceDeletionStoreError::from)?;
        let storage = StorageKind::parse(&storage_raw)
            .map_err(|_| WorkspaceDeletionStoreError::UnknownStorageKind(storage_raw))?;
        let key = BlobStorageKey::parse(key_raw)
            .map_err(|_| WorkspaceDeletionStoreError::MalformedStorageKey)?;
        blobs.push(StoredBlob::new(storage, key));
    }

    let viewer_rows = sqlx::query(SELECT_WORKSPACE_VIEWERS)
        .bind(workspace_id.as_str())
        .fetch_all(&mut **tx)
        .await
        .map_err(WorkspaceDeletionStoreError::from)?;
    let mut viewer_members = Vec::with_capacity(viewer_rows.len());
    for row in &viewer_rows {
        let id: String = row
            .try_get("user_id")
            .map_err(WorkspaceDeletionStoreError::from)?;
        viewer_members.push(UserId::parse(id).map_err(|_| {
            WorkspaceDeletionStoreError::MalformedIdentity {
                field: "workspace_users.user_id",
            }
        })?);
    }
    // EVERY conversation id in the workspace, captured
    // INDEPENDENTLY of the runtime seats: the `conversation_closed` terminalization
    // must cover a conversation whose roster is already gone but whose run map still
    // has a live ConversationSlot run — which a seat-derived set (built from current
    // roster rows) would miss.
    let conversation_rows = sqlx::query(SELECT_WORKSPACE_CONVERSATION_IDS)
        .bind(workspace_id.as_str())
        .fetch_all(&mut **tx)
        .await
        .map_err(WorkspaceDeletionStoreError::from)?;
    let mut conversation_ids = Vec::with_capacity(conversation_rows.len());
    for row in &conversation_rows {
        let id: String = row
            .try_get("id")
            .map_err(WorkspaceDeletionStoreError::from)?;
        conversation_ids.push(ConversationId::parse(id).map_err(|_| {
            WorkspaceDeletionStoreError::MalformedIdentity {
                field: "conversations.id",
            }
        })?);
    }

    Ok(WorkspaceTeardownPlan {
        runtime_seats,
        blobs,
        conversation_ids,
        viewer_members,
        workspace: workspace_id.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqliteConnectOptions;
    use sqlx::sqlite::SqlitePoolOptions;
    use sqlx::{Connection, Executor, SqliteConnection};

    const CURRENT_SCHEMA: &str =
        include_str!("../../../../../rust/test-fixtures/migration-baseline/schema-current.sql");

    // A regular workspace `ws-1` owned by user-1 (member user-2), holding two
    // conversations. `conv-a` has a ConversationSlot agent (`agent-conv`,
    // session_scope='conversation') and an attachment blob; `conv-b` has an
    // AgentSlot agent (`agent-slot`, session_scope='agent') that must NOT be a
    // captured seat. An agent-home `ws-home` (owner agent) is the not-deletable kind.
    const SEED_SQL: &str = "\
INSERT INTO users (id, name, display_name, created_at, updated_at) VALUES ('user-1','owner','Owner',1,1); \
INSERT INTO users (id, name, display_name, created_at, updated_at) VALUES ('user-2','member','Member',1,1); \
INSERT INTO hosts (id, name, kind, created_at) VALUES ('host-1','Host','local',1); \
INSERT INTO agents (id, name, display_name, runtime, host_id, created_by_user_id, created_at, updated_at, session_scope) \
  VALUES ('agent-conv','conv','Conv','claude','host-1','user-1',1,1,'conversation'); \
INSERT INTO agents (id, name, display_name, runtime, host_id, created_by_user_id, created_at, updated_at, session_scope) \
  VALUES ('agent-slot','slot','Slot','claude','host-1','user-1',1,1,'agent'); \
INSERT INTO agents (id, name, display_name, runtime, host_id, created_by_user_id, created_at, updated_at, session_scope) \
  VALUES ('agent-home-owner','home','Home','claude','host-1','user-1',1,1,'agent'); \
INSERT INTO workspaces (id, name, name_key, description, owner_agent_id, owner_user_id, archived_at, created_at, updated_at) \
  VALUES ('ws-1','Original','original',NULL,NULL,'user-1',NULL,10,100); \
INSERT INTO workspaces (id, name, name_key, description, owner_agent_id, owner_user_id, archived_at, created_at, updated_at) \
  VALUES ('ws-home','Agent Home',NULL,NULL,'agent-home-owner',NULL,NULL,10,100); \
INSERT INTO workspace_users (workspace_id, user_id, joined_at, role) VALUES ('ws-1','user-1',1,'owner'); \
INSERT INTO workspace_users (workspace_id, user_id, joined_at, role) VALUES ('ws-1','user-2',1,'member'); \
INSERT INTO conversations (id, workspace_id, placement_kind, roster_policy, title, title_key, status, visibility, created_at, updated_at) \
  VALUES ('conv-a','ws-1','workspace','open','A','a','idle','public',1,1); \
INSERT INTO conversations (id, workspace_id, placement_kind, roster_policy, title, title_key, status, visibility, created_at, updated_at) \
  VALUES ('conv-b','ws-1','workspace','open','B','b','idle','public',1,1); \
INSERT INTO conversations (id, workspace_id, placement_kind, roster_policy, title, title_key, status, visibility, created_at, updated_at) \
  VALUES ('conv-c','ws-1','workspace','open','C','c','idle','public',1,1); \
INSERT INTO conversation_agents (conversation_id, agent_id, joined_at) VALUES ('conv-a','agent-conv',1); \
INSERT INTO conversation_agents (conversation_id, agent_id, joined_at) VALUES ('conv-b','agent-slot',1); \
INSERT INTO attachments (id, channel_id, filename, size, content_type, storage, storage_key, created_by_kind, created_by_id, created_at) \
  VALUES ('att-1','conv-a','f.txt',4,'text/plain','local','attachments/blob-1.bin','user','user-1',1);";

    async fn open_seeded() -> (tempfile::TempDir, SqliteWorkspaceDeletion) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("workspace-deletion.sqlite");
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .foreign_keys(true);
        let mut db = SqliteConnection::connect_with(&options).await.unwrap();
        db.execute(sqlx::raw_sql(CURRENT_SCHEMA)).await.unwrap();
        db.execute(sqlx::raw_sql(SEED_SQL)).await.unwrap();
        db.close().await.unwrap();
        let pool = SqlitePoolOptions::new()
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&path)
                    .foreign_keys(true),
            )
            .await
            .unwrap();
        (directory, SqliteWorkspaceDeletion::from_pool(pool))
    }

    fn user(id: &str) -> UserId {
        UserId::parse(id).unwrap()
    }
    fn ws(id: &str) -> WorkspaceId {
        WorkspaceId::parse(id).unwrap()
    }

    #[tokio::test]
    async fn owner_delete_captures_seats_blobs_and_cascades_in_one_tx() {
        let (_dir, adapter) = open_seeded().await;
        // ONE call is the authority: it captures the plan in the cascade tx and
        // returns Committed(plan).
        let WorkspaceDeletionOutcome::Committed(plan) = adapter
            .delete_workspace(&user("user-1"), &ws("ws-1"))
            .await
            .unwrap()
        else {
            panic!("a deletable owner delete must be Committed(plan)");
        };
        // ConversationSlot seat captured; AgentSlot excluded.
        assert_eq!(
            plan.runtime_seats.len(),
            1,
            "exactly the ConversationSlot seat"
        );
        assert_eq!(plan.runtime_seats[0].conversation.as_str(), "conv-a");
        assert_eq!(plan.runtime_seats[0].agent.as_str(), "agent-conv");
        // conversation_ids captures EVERY workspace conversation independently of the
        // seats — incl conv-c which has NO conversation_agents row (no seat).
        let mut convs: Vec<&str> = plan
            .conversation_ids
            .iter()
            .map(ConversationId::as_str)
            .collect();
        convs.sort();
        assert_eq!(convs, vec!["conv-a", "conv-b", "conv-c"]);
        // Attachment blob captured with its exact stored key.
        assert_eq!(plan.blobs.len(), 1);
        assert_eq!(plan.blobs[0].key().as_str(), "attachments/blob-1.bin");
        assert_eq!(plan.blobs[0].storage(), StorageKind::Local);
        // Both viewer members captured for the removed fan-out.
        let mut viewers: Vec<&str> = plan.viewer_members.iter().map(UserId::as_str).collect();
        viewers.sort_unstable();
        assert_eq!(viewers, vec!["user-1", "user-2"]);
        // Row is gone and the cascade removed its membership.
        let workspace_rows: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM workspaces WHERE id = 'ws-1'")
                .fetch_one(&adapter.pool)
                .await
                .unwrap();
        assert_eq!(workspace_rows, 0, "row deleted");
        let membership_rows: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM conversation_agents WHERE conversation_id IN ('conv-a','conv-b')",
        )
        .fetch_one(&adapter.pool)
        .await
        .unwrap();
        assert_eq!(membership_rows, 0, "cascade erased conversation_agents");
    }

    #[tokio::test]
    async fn missing_id_is_an_idempotent_noop() {
        let (_dir, adapter) = open_seeded().await;
        let outcome = adapter
            .delete_workspace(&user("user-1"), &ws("ws-absent"))
            .await
            .unwrap();
        assert_eq!(outcome, WorkspaceDeletionOutcome::IdempotentNoop);
    }

    #[tokio::test]
    async fn agent_home_is_not_deletable_kind() {
        let (_dir, adapter) = open_seeded().await;
        let outcome = adapter
            .delete_workspace(&user("user-1"), &ws("ws-home"))
            .await
            .unwrap();
        assert_eq!(outcome, WorkspaceDeletionOutcome::NotDeletableKind);
        // The agent-home row survives.
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM workspaces WHERE id = 'ws-home'")
            .fetch_one(&adapter.pool)
            .await
            .unwrap();
        assert_eq!(rows, 1);
    }

    #[tokio::test]
    async fn a_non_owner_member_is_forbidden_and_the_row_survives() {
        for role in ["member", "admin"] {
            let (_dir, adapter) = open_seeded().await;
            sqlx::query("UPDATE workspace_users SET role = ? WHERE workspace_id = 'ws-1' AND user_id = 'user-2'")
            .bind(role)
            .execute(&adapter.pool)
            .await
            .unwrap();
            let outcome = adapter
                .delete_workspace(&user("user-2"), &ws("ws-1"))
                .await
                .unwrap();
            assert_eq!(outcome, WorkspaceDeletionOutcome::Forbidden);
            let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM workspaces WHERE id = 'ws-1'")
                .fetch_one(&adapter.pool)
                .await
                .unwrap();
            assert_eq!(rows, 1, "forbidden delete leaves the row");
        }
    }

    #[tokio::test]
    async fn an_owner_change_before_the_lock_is_forbidden_not_a_noop() {
        // An owner change is a DISTINCT product result (403 Forbidden), NOT
        // folded into the idempotent 204. Under the single fresh lock the subject is a
        // regular workspace the requester no longer owns → Forbidden; the row survives.
        let (_dir, adapter) = open_seeded().await;
        sqlx::query("UPDATE workspaces SET owner_user_id='user-2' WHERE id='ws-1'")
            .execute(&adapter.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE workspace_users SET role='member' WHERE workspace_id='ws-1' AND user_id='user-1'")
            .execute(&adapter.pool)
            .await
            .unwrap();
        let outcome = adapter
            .delete_workspace(&user("user-1"), &ws("ws-1"))
            .await
            .unwrap();
        assert_eq!(
            outcome,
            WorkspaceDeletionOutcome::Forbidden,
            "a no-longer-owned workspace is Forbidden (403), not a silent 204 no-op"
        );
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM workspaces WHERE id='ws-1'")
            .fetch_one(&adapter.pool)
            .await
            .unwrap();
        assert_eq!(rows, 1, "forbidden cascades nothing");
    }

    #[tokio::test]
    async fn a_kind_change_before_the_lock_is_not_deletable_kind_not_a_noop() {
        // A kind change (regular → agent-home) is a DISTINCT product result
        // (409 NotDeletableKind), NOT the idempotent 204.
        let (_dir, adapter) = open_seeded().await;
        // The schema CHECK ties (owner_agent_id IS NULL) == (name_key IS NOT NULL): an
        // agent-home has an owner agent AND a null name_key. Use agent-slot (owns no
        // other workspace — owner_agent_id is UNIQUE) and flip both columns together.
        sqlx::query(
            "UPDATE workspaces SET owner_agent_id='agent-slot', name_key=NULL WHERE id='ws-1'",
        )
        .execute(&adapter.pool)
        .await
        .unwrap();
        let outcome = adapter
            .delete_workspace(&user("user-1"), &ws("ws-1"))
            .await
            .unwrap();
        assert_eq!(outcome, WorkspaceDeletionOutcome::NotDeletableKind);
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM workspaces WHERE id='ws-1'")
            .fetch_one(&adapter.pool)
            .await
            .unwrap();
        assert_eq!(rows, 1, "kind-changed workspace is not cascaded");
    }

    #[tokio::test]
    async fn retry_after_committed_delete_is_an_idempotent_noop() {
        // A sequential retry finds the committed deletion and does not produce
        // a second destructive authority. This is not a concurrency test.
        let (_dir, adapter) = open_seeded().await;
        assert!(
            matches!(
                adapter
                    .delete_workspace(&user("user-1"), &ws("ws-1"))
                    .await
                    .unwrap(),
                WorkspaceDeletionOutcome::Committed(_)
            ),
            "the winner cascades and carries the plan"
        );
        assert_eq!(
            adapter
                .delete_workspace(&user("user-1"), &ws("ws-1"))
                .await
                .unwrap(),
            WorkspaceDeletionOutcome::IdempotentNoop,
            "the loser finds the row gone → idempotent no-op"
        );
    }
}
