//! Stored corruption and historical-data isolation for the SQLite cold-read adapter,
//! exercised against the checked-in executable current schema.

use std::error::Error;

use silverhand_backend_core::{
    foundation::identity::UserId, foundation::persistence::PersistenceFailure,
    identity_access::workspace_cold_read::WorkspaceColdReadUnitOfWork,
};
use silverhand_backend_sqlite::identity_access::workspace_cold_read::{
    SqliteWorkspaceColdRead, WorkspaceColdReadError,
};
use sqlx::Connection;

use crate::current_schema;

fn viewer() -> UserId {
    UserId::parse("user-1").unwrap()
}

/// Opens a fresh schema database with check constraints disabled, so a corruption
/// fixture can persist a value the write path could never produce.
async fn corrupt_schema(path: &std::path::Path) -> sqlx::SqliteConnection {
    let mut db = current_schema::create_current_schema(path).await;
    sqlx::query("PRAGMA ignore_check_constraints = ON")
        .execute(&mut db)
        .await
        .unwrap();
    db
}

async fn cold_read_failure(path: &std::path::Path) -> PersistenceFailure {
    SqliteWorkspaceColdRead::open(path)
        .await
        .unwrap()
        .load_workspace_cold_read(&viewer())
        .await
        .unwrap_err()
}

/// Downcasts the safe-500 failure to its typed corruption source. Every corruption
/// case must surface this way rather than as a product refusal.
fn corruption_source(failure: &PersistenceFailure) -> &WorkspaceColdReadError {
    Error::source(failure)
        .and_then(|source| source.downcast_ref::<WorkspaceColdReadError>())
        .expect("the cold-read failure must carry its typed source")
}

#[tokio::test]
async fn an_empty_committed_workspace_id_is_corruption_not_an_empty_wire_id() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("corrupt-id.sqlite");
    let mut db = corrupt_schema(&path).await;
    for statement in [
        "INSERT INTO users (id, name, display_name, created_at, updated_at) \
         VALUES ('user-1', 'viewer', 'Viewer', 1, 1)",
        "INSERT INTO workspaces (id, name, name_key, owner_agent_id, owner_user_id, created_at, \
         updated_at) VALUES ('', 'Blank Id', 'blank', NULL, 'user-1', 1, 1)",
        "INSERT INTO workspace_users (workspace_id, user_id, joined_at, role) \
         VALUES ('', 'user-1', 1, 'owner')",
    ] {
        sqlx::query(statement).execute(&mut db).await.unwrap();
    }
    db.close().await.unwrap();

    assert!(matches!(
        corruption_source(&cold_read_failure(&path).await),
        WorkspaceColdReadError::MalformedIdentity { field } if *field == "workspaces.id"
    ));
}

#[tokio::test]
async fn retired_sort_data_does_not_affect_workspace_identity() {
    for historical_row in [
        "INSERT INTO user_sort_entry (user_id, list_key, entity_id, rank) VALUES ('user-1', 'workspaces', '', 0)",
        "INSERT INTO user_list_sort_mode (user_id, list_key, scope, mode, updated_at) VALUES ('user-1', 'workspaces', '', 'manual', 1)",
        "INSERT INTO user_list_sort_mode (user_id, list_key, scope, mode, updated_at) VALUES ('user-1', 'workspaces', 'regular', 'chronological', 1)",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("corrupt-entity.sqlite");
        let mut db = corrupt_schema(&path).await;
        for statement in [
            "INSERT INTO users (id, name, display_name, created_at, updated_at) \
         VALUES ('user-1', 'viewer', 'Viewer', 1, 1)",
            "INSERT INTO workspaces (id, name, name_key, owner_agent_id, owner_user_id, created_at, \
         updated_at) VALUES ('ws-1', 'Regular', 'regular', NULL, 'user-1', 1, 1)",
            "INSERT INTO workspace_users (workspace_id, user_id, joined_at, role) \
         VALUES ('ws-1', 'user-1', 1, 'owner')",
            historical_row,
        ] {
            sqlx::query(statement).execute(&mut db).await.unwrap();
        }
        db.close().await.unwrap();

        let read = SqliteWorkspaceColdRead::open(&path)
            .await
            .unwrap()
            .load_workspace_cold_read(&UserId::parse("user-1").unwrap())
            .await
            .unwrap();
        assert_eq!(read.workspaces.len(), 1);
        assert_eq!(read.workspaces[0].id.as_str(), "ws-1");
    }
}
