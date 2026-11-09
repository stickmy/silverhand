use silverhand_backend_core::{
    conversation_messaging::messaging::collaboration::channel_lifecycle::{
        ChannelDetailReadPort, ChannelDetailRefusal,
    },
    foundation::identity::{ConversationId, UserId},
};
use silverhand_backend_sqlite::conversation_messaging::messaging::MessagingStore;
use silverhand_backend_sqlite::conversation_messaging::messaging::collaboration::channel_lifecycle::ChannelLifecycleStoreError;
use sqlx::{Connection, Executor, SqliteConnection};

use crate::current_schema;

async fn seed(path: &std::path::Path) {
    let mut db = current_schema::create_current_schema(path).await;
    db.execute("INSERT INTO hosts(id,name,kind,created_at) VALUES('h','host','local',1)")
        .await
        .unwrap();
    db.execute(
        "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES \
         ('member','life-member','Member',1,1),('browser','life-browser','Browser',1,1),\
         ('other','life-other','Other',1,1)",
    )
    .await
    .unwrap();
    db.execute(seed_agent!(
        id = "agent",
        name = "life-agent",
        runtime = "codex",
        host_id = "h",
        created_by_user_id = "member",
        display_name = "Agent",
    ))
    .await
    .unwrap();
    db.execute(
        "INSERT INTO workspaces(id,name,name_key,created_at,updated_at) \
         VALUES('w','Workspace','lifecycle-workspace',1,1)",
    )
    .await
    .unwrap();
    db.execute(
        "INSERT INTO workspace_users(workspace_id,user_id,joined_at,role) VALUES \
         ('w','member',1,'member'),('w','browser',1,'admin')",
    )
    .await
    .unwrap();
    db.execute(concat!(
        seed_channel!(id = "private", workspace_id = "w", title = "Private"),
        "; ",
        seed_channel!(
            id = "public",
            workspace_id = "w",
            title = "Public",
            visibility = "public",
        ),
        "; ",
        seed_channel!(
            id = "archived",
            workspace_id = "w",
            title = "Archived",
            visibility = "public",
        ),
        "; ",
        seed_channel!(
            id = "fixed",
            workspace_id = "w",
            roster_policy = "fixed",
            title = "Fixed",
        ),
    ))
    .await
    .unwrap();
    db.execute("UPDATE conversations SET archived_at=9 WHERE id='archived'")
        .await
        .unwrap();
    db.execute(
        "INSERT INTO conversation_users(conversation_id,user_id,joined_at) VALUES \
         ('private','member',1),('fixed','member',1),('archived','member',1)",
    )
    .await
    .unwrap();
    db.execute(concat!(
        seed_direct_pair!(
            id = "pair",
            endpoint_a_kind = "agent",
            endpoint_a_id = "agent",
            endpoint_b_kind = "human",
            endpoint_b_id = "member",
        ),
        "; ",
        seed_direct_pair!(
            id = "foreign-pair",
            endpoint_a_kind = "agent",
            endpoint_a_id = "agent",
            endpoint_b_kind = "human",
            endpoint_b_id = "other",
        ),
    ))
    .await
    .unwrap();
    db.close().await.unwrap();
}

async fn detail(
    path: &std::path::Path,
    viewer: &str,
    conversation: &str,
) -> Result<
    Result<
        silverhand_backend_core::conversation_messaging::messaging::collaboration::channel_lifecycle::ChannelDetailFact,
        ChannelDetailRefusal,
    >,
    silverhand_backend_core::foundation::persistence::PersistenceFailure,
>{
    MessagingStore::open(path)
        .await
        .unwrap()
        .channel_lifecycle()
        .detail(
            &UserId::parse(viewer).unwrap(),
            &ConversationId::parse(conversation).unwrap(),
        )
        .await
}

fn storage_source(
    error: &silverhand_backend_core::foundation::persistence::PersistenceFailure,
) -> &ChannelLifecycleStoreError {
    std::error::Error::source(error)
        .and_then(|source| source.downcast_ref())
        .expect("channel lifecycle failures preserve their named store source")
}

#[tokio::test]
async fn human_endpoint_reads_agent_human_pair_without_becoming_a_member() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("channel-lifecycle.sqlite");
    seed(&path).await;

    let pair = detail(&path, "member", "pair")
        .await
        .unwrap()
        .unwrap()
        .bind(
            &UserId::parse("member").unwrap(),
            &ConversationId::parse("pair").unwrap(),
        )
        .unwrap();
    assert!(pair.row().workspace().is_none());
    assert_eq!(pair.row().title(), "");
    assert!(!pair.is_member());
}

#[tokio::test]
async fn refusal_order_keeps_archived_public_foreign_pair_and_missing_distinct() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("channel-lifecycle-refusal.sqlite");
    seed(&path).await;

    for (viewer, id, expected) in [
        ("browser", "private", ChannelDetailRefusal::Forbidden),
        ("browser", "archived", ChannelDetailRefusal::Forbidden),
        ("member", "foreign-pair", ChannelDetailRefusal::Forbidden),
        ("member", "missing", ChannelDetailRefusal::NotFound),
    ] {
        assert_eq!(
            detail(&path, viewer, id).await.unwrap(),
            Err(expected),
            "{id}"
        );
    }
}

#[tokio::test]
async fn every_selected_membership_decodes_before_public_widen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir
        .path()
        .join("channel-lifecycle-member-corruption.sqlite");
    seed(&path).await;
    let mut db = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    db.execute("PRAGMA foreign_keys=OFF").await.unwrap();
    db.execute(
        "INSERT INTO conversation_users(conversation_id,user_id,joined_at) VALUES('public','',1)",
    )
    .await
    .unwrap();

    let error = detail(&path, "browser", "public").await.unwrap_err();
    assert!(matches!(
        storage_source(&error),
        ChannelLifecycleStoreError::Identity {
            field: "conversation_users.user_id"
        }
    ));
    db.execute("DELETE FROM conversation_users WHERE conversation_id='public' AND user_id=''")
        .await
        .unwrap();
    assert!(detail(&path, "browser", "public").await.unwrap().is_ok());
}

#[tokio::test]
async fn placement_and_latest_sequence_corruption_fail_before_projection() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("channel-lifecycle-placement.sqlite");
    seed(&path).await;
    let mut db = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    db.execute("PRAGMA ignore_check_constraints=ON")
        .await
        .unwrap();
    db.execute("DROP TRIGGER trg_conversations_endpoints_immutable")
        .await
        .unwrap();

    db.execute(
        "UPDATE conversations SET endpoint_a_kind='human',endpoint_a_id='member' WHERE id='public'",
    )
    .await
    .unwrap();
    let placement = detail(&path, "browser", "public").await.unwrap_err();
    assert!(matches!(
        storage_source(&placement),
        ChannelLifecycleStoreError::Placement
    ));
    db.execute(
        "UPDATE conversations SET endpoint_a_kind=NULL,endpoint_a_id=NULL WHERE id='public'",
    )
    .await
    .unwrap();
    assert!(detail(&path, "browser", "public").await.unwrap().is_ok());

    db.execute("UPDATE conversations SET latest_message_seq=-1 WHERE id='public'")
        .await
        .unwrap();
    let sequence = detail(&path, "browser", "public").await.unwrap_err();
    assert!(matches!(
        storage_source(&sequence),
        ChannelLifecycleStoreError::Projection(
            silverhand_backend_core::conversation_messaging::messaging::collaboration::channel_lifecycle::ChannelLifecycleProjectionError::NegativeLatestMessageSeq
        )
    ));
    db.execute("UPDATE conversations SET latest_message_seq=0 WHERE id='public'")
        .await
        .unwrap();
    assert!(detail(&path, "browser", "public").await.unwrap().is_ok());
}

#[tokio::test]
async fn noncanonical_title_and_unknown_role_are_named_storage_failures() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("channel-lifecycle-corruption.sqlite");
    seed(&path).await;
    let mut db = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    db.execute("PRAGMA ignore_check_constraints=ON")
        .await
        .unwrap();

    db.execute("UPDATE conversations SET title='  Public  ' WHERE id='public'")
        .await
        .unwrap();
    let title_error = detail(&path, "browser", "public").await.unwrap_err();
    assert!(matches!(
        storage_source(&title_error),
        ChannelLifecycleStoreError::Title
    ));
    db.execute("UPDATE conversations SET title='Public' WHERE id='public'")
        .await
        .unwrap();
    assert!(detail(&path, "browser", "public").await.unwrap().is_ok());

    db.execute(
        "UPDATE workspace_users SET role='governor' WHERE workspace_id='w' AND user_id='browser'",
    )
    .await
    .unwrap();
    let role_error = detail(&path, "browser", "public").await.unwrap_err();
    assert!(matches!(
        storage_source(&role_error),
        ChannelLifecycleStoreError::WorkspaceRole
    ));
    db.execute(
        "UPDATE workspace_users SET role='admin' WHERE workspace_id='w' AND user_id='browser'",
    )
    .await
    .unwrap();
    assert!(detail(&path, "browser", "public").await.unwrap().is_ok());
}
