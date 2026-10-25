//! SQLite adapter for
//! [`WorkspaceHumanAudienceReadPort`](silverhand_backend_core::identity_access::access::WorkspaceHumanAudienceReadPort):
//! the human members of a workspace, read for the post-commit
//! `metadata_changed` fan-out.
//!
//! Human membership is the `workspace_users` roster (agents live in the separate
//! `workspace_members` table and are not part of this viewer-scoped audience). The
//! read is a flat projection of that roster's `user_id` for one workspace; a
//! workspace with no human members — or one that no longer exists — is an empty
//! set, which is the true "owed to nobody" answer, never a refusal. Ids come back
//! ordered by `user_id` so the fan-out adapter's own tests observe a deterministic
//! sequence, though no reader depends on the order.

use std::{path::Path, time::Duration};

use silverhand_backend_core::{
    foundation::identity::{UserId, WorkspaceId},
    foundation::persistence::PersistenceFailure,
    identity_access::access::WorkspaceHumanAudienceReadPort,
};
use sqlx::Row;
use thiserror::Error;

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

const SELECT_HUMAN_MEMBERS: &str = "\
SELECT wu.user_id \
  FROM workspace_users wu \
 WHERE wu.workspace_id = ? \
 ORDER BY wu.user_id";

#[derive(Clone)]
pub struct SqliteWorkspaceAudienceRead {
    pool: crate::foundation::database::SqliteDatabase,
}

/// Read failures surfaced only as the source of a [`PersistenceFailure`]. A
/// malformed stored `user_id` is corruption of a committed roster row, never a
/// product refusal.
#[derive(Debug, Error)]
pub enum WorkspaceAudienceReadError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("stored column {field} does not name a usable identity")]
    MalformedIdentity { field: &'static str },
}

impl SqliteWorkspaceAudienceRead {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, WorkspaceAudienceReadError> {
        let pool = crate::foundation::database::store_pool(path.as_ref(), BUSY_TIMEOUT).await?;
        Ok(Self { pool })
    }

    /// Construct over an already-open pool, so a composition root that shares one
    /// pool across adapters need not reopen the file.
    pub fn from_pool(pool: impl Into<crate::foundation::database::SqliteDatabase>) -> Self {
        let pool = pool.into();
        Self { pool }
    }
}

impl WorkspaceHumanAudienceReadPort for SqliteWorkspaceAudienceRead {
    async fn human_member_ids(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Vec<UserId>, PersistenceFailure> {
        let rows = sqlx::query(SELECT_HUMAN_MEMBERS)
            .bind(workspace_id.as_str())
            .fetch_all(&self.pool)
            .await
            .map_err(|error| {
                PersistenceFailure::from_source(WorkspaceAudienceReadError::Sql(error))
            })?;
        rows.into_iter()
            .map(|row| {
                let raw: String = row.try_get("user_id").map_err(|error| {
                    PersistenceFailure::from_source(WorkspaceAudienceReadError::Sql(error))
                })?;
                UserId::parse(raw).map_err(|_| {
                    PersistenceFailure::from_source(WorkspaceAudienceReadError::MalformedIdentity {
                        field: "workspace_users.user_id",
                    })
                })
            })
            .collect()
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
INSERT INTO users (id, name, display_name, created_at, updated_at) \
  VALUES ('user-1', 'one', 'One', 1, 1); \
INSERT INTO users (id, name, display_name, created_at, updated_at) \
  VALUES ('user-2', 'two', 'Two', 2, 2); \
INSERT INTO users (id, name, display_name, created_at, updated_at) \
  VALUES ('user-3', 'three', 'Three', 3, 3); \
INSERT INTO hosts (id, name, kind, created_at) VALUES ('host-1', 'Host', 'local', 1); \
INSERT INTO agents (id, name, display_name, runtime, host_id, created_by_user_id, created_at, updated_at) \
  VALUES ('agent-1', 'alice', 'Alice', 'claude', 'host-1', 'user-1', 1, 1); \
INSERT INTO workspaces (id, name, name_key, description, owner_agent_id, owner_user_id, archived_at, created_at, updated_at) \
  VALUES ('ws-1', 'Regular', 'regular', 'desc', NULL, 'user-1', NULL, 1, 300); \
INSERT INTO workspaces (id, name, name_key, description, owner_agent_id, owner_user_id, archived_at, created_at, updated_at) \
  VALUES ('ws-empty', 'Empty', 'empty', NULL, NULL, 'user-1', NULL, 1, 400); \
INSERT INTO workspace_users (workspace_id, user_id, joined_at, role) VALUES ('ws-1', 'user-2', 5, 'member'); \
INSERT INTO workspace_users (workspace_id, user_id, joined_at, role) VALUES ('ws-1', 'user-1', 1, 'owner'); \
INSERT INTO workspace_users (workspace_id, user_id, joined_at, role) VALUES ('ws-1', 'user-3', 9, 'admin'); \
INSERT INTO workspace_members (workspace_id, agent_id, joined_at) VALUES ('ws-1', 'agent-1', 1);";

    async fn open_seeded() -> (tempfile::TempDir, SqliteWorkspaceAudienceRead) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("workspace-audience.sqlite");
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .foreign_keys(true);
        let mut db = SqliteConnection::connect_with(&options).await.unwrap();
        db.execute(sqlx::raw_sql(CURRENT_SCHEMA)).await.unwrap();
        db.execute(sqlx::raw_sql(SEED_SQL)).await.unwrap();
        db.close().await.unwrap();
        let adapter = SqliteWorkspaceAudienceRead::open(&path).await.unwrap();
        (directory, adapter)
    }

    #[tokio::test]
    async fn enumerates_only_the_human_members() {
        let (_dir, adapter) = open_seeded().await;
        let mut members = adapter
            .human_member_ids(&WorkspaceId::parse("ws-1").unwrap())
            .await
            .unwrap();
        // The agent in `workspace_members` is NOT in this audience; only the three
        // `workspace_users` rows. Audience ordering is not a consumer contract.
        members.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        assert_eq!(
            members,
            vec![
                UserId::parse("user-1").unwrap(),
                UserId::parse("user-2").unwrap(),
                UserId::parse("user-3").unwrap(),
            ]
        );
    }

    #[tokio::test]
    async fn a_workspace_with_no_human_members_is_an_empty_audience() {
        let (_dir, adapter) = open_seeded().await;
        let members = adapter
            .human_member_ids(&WorkspaceId::parse("ws-empty").unwrap())
            .await
            .unwrap();
        assert!(members.is_empty());
    }

    #[tokio::test]
    async fn an_absent_workspace_is_an_empty_audience_not_an_error() {
        let (_dir, adapter) = open_seeded().await;
        let members = adapter
            .human_member_ids(&WorkspaceId::parse("ws-missing").unwrap())
            .await
            .unwrap();
        assert!(members.is_empty());
    }
}
