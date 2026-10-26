//! Workspace ownership transfer against the checked-in current schema: the role
//! edges, the `owner_user_id` pointer, and the previous owner's authority after.

use silverhand_backend_core::foundation::identity::{Principal, UserId, WorkspaceId};
use silverhand_backend_core::identity_access::access::{
    HumanRoleChangeOutcome, OwnershipTransferOutcome, WorkspaceAccessPolicy, WorkspaceGovernance,
    WorkspaceMembershipPort, WorkspaceOwnershipTransferPort, WorkspaceRole,
};
use silverhand_backend_core::identity_access::workspace_deletion::{
    WorkspaceDeletionOutcome, WorkspaceDeletionWrite,
};
use silverhand_backend_sqlite::identity_access::access::SqliteAccessStore;
use silverhand_backend_sqlite::identity_access::workspace_deletion::SqliteWorkspaceDeletion;
use sqlx::Connection;
use tempfile::TempDir;

use crate::current_schema;

async fn seeded() -> (TempDir, std::path::PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("transfer.db");
    let mut connection = current_schema::create_current_schema(&path).await;
    sqlx::raw_sql(
        "INSERT INTO users (id,name,display_name,created_at,updated_at) VALUES \
           ('owner','owner','Owner',1,1),('admin','admin','Admin',1,1), \
           ('member','member','Member',1,1),('stranger','stranger','Stranger',1,1); \
         INSERT INTO workspaces (id,name,name_key,owner_user_id,created_at,updated_at) \
           VALUES ('ws','WS','ws','owner',1,1); \
         INSERT INTO workspace_users (workspace_id,user_id,joined_at,role) VALUES \
           ('ws','owner',1,'owner'),('ws','admin',1,'admin'),('ws','member',1,'member');",
    )
    .execute(&mut connection)
    .await
    .unwrap();
    connection.close().await.unwrap();
    (directory, path)
}

fn uid(raw: &str) -> UserId {
    UserId::parse(raw).unwrap()
}

fn ws() -> WorkspaceId {
    WorkspaceId::parse("ws").unwrap()
}

async fn open(path: &std::path::Path) -> sqlx::SqliteConnection {
    sqlx::SqliteConnection::connect(&format!("sqlite://{}", path.display()))
        .await
        .unwrap()
}

async fn roles(path: &std::path::Path) -> Vec<(String, String)> {
    let mut connection = open(path).await;
    sqlx::query_as(
        "SELECT user_id, role FROM workspace_users WHERE workspace_id='ws' ORDER BY user_id",
    )
    .fetch_all(&mut connection)
    .await
    .unwrap()
}

async fn owner_pointer(path: &std::path::Path) -> Option<String> {
    let mut connection = open(path).await;
    sqlx::query_scalar("SELECT owner_user_id FROM workspaces WHERE id='ws'")
        .fetch_one(&mut connection)
        .await
        .unwrap()
}

#[tokio::test]
async fn transfer_swaps_owner_and_demotes_previous_owner_to_admin() {
    let (_dir, path) = seeded().await;
    let store = SqliteAccessStore::open(&path).await.unwrap();
    let outcome = store
        .transfer_ownership(&uid("owner"), &ws(), &uid("member"))
        .await
        .unwrap();
    let OwnershipTransferOutcome::Transferred {
        previous_owner,
        new_owner,
    } = outcome
    else {
        panic!("expected Transferred, got {outcome:?}");
    };
    assert_eq!(previous_owner.role, Some(WorkspaceRole::Admin));
    assert_eq!(new_owner.role, Some(WorkspaceRole::Owner));
    assert_eq!(
        roles(&path).await,
        [
            ("admin".into(), "admin".into()),
            ("member".into(), "owner".into()),
            ("owner".into(), "admin".into()),
        ]
    );
    assert_eq!(owner_pointer(&path).await.as_deref(), Some("member"));

    // The previous owner keeps admin governance but has lost every owner-only power.
    let principal = Principal::Human {
        user_id: uid("owner"),
    };
    assert_eq!(
        store
            .authorize_member_governance(&principal, &ws())
            .await
            .unwrap(),
        WorkspaceGovernance::Admin
    );
    assert_eq!(
        store
            .change_human_role(&principal, &ws(), &uid("admin"), WorkspaceRole::Member)
            .await
            .unwrap(),
        HumanRoleChangeOutcome::Forbidden
    );
    assert_eq!(
        store
            .transfer_ownership(&uid("owner"), &ws(), &uid("admin"))
            .await
            .unwrap(),
        OwnershipTransferOutcome::Forbidden
    );
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&path)
                .foreign_keys(true),
        )
        .await
        .unwrap();
    let deletion = SqliteWorkspaceDeletion::from_pool(pool);
    assert_eq!(
        deletion
            .delete_workspace(&uid("owner"), &ws())
            .await
            .unwrap(),
        WorkspaceDeletionOutcome::Forbidden
    );
}

#[tokio::test]
async fn only_the_owner_may_transfer_and_only_to_another_member() {
    let (_dir, path) = seeded().await;
    let store = SqliteAccessStore::open(&path).await.unwrap();
    for (actor, target, expected) in [
        ("admin", "member", OwnershipTransferOutcome::Forbidden),
        ("member", "admin", OwnershipTransferOutcome::Forbidden),
        (
            "stranger",
            "member",
            OwnershipTransferOutcome::WorkspaceNotFound,
        ),
        (
            "owner",
            "stranger",
            OwnershipTransferOutcome::MemberNotFound,
        ),
        ("owner", "owner", OwnershipTransferOutcome::MemberNotFound),
    ] {
        assert_eq!(
            store
                .transfer_ownership(&uid(actor), &ws(), &uid(target))
                .await
                .unwrap(),
            expected,
            "{actor} -> {target}"
        );
    }
    assert_eq!(owner_pointer(&path).await.as_deref(), Some("owner"));
    assert_eq!(
        roles(&path).await,
        [
            ("admin".into(), "admin".into()),
            ("member".into(), "member".into()),
            ("owner".into(), "owner".into()),
        ]
    );
}
