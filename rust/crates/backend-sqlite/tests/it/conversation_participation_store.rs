use silverhand_backend_core::foundation::identity::{ConversationId, UserId};
use silverhand_backend_sqlite::conversation_messaging::messaging::{
    MessagingStore, collaboration::conversation_participation::ConversationParticipationRefusal,
};
use sqlx::{Connection, Executor, SqliteConnection};

use crate::current_schema;

async fn seed(path: &std::path::Path) {
    let mut db = current_schema::create_current_schema(path).await;
    db.execute(
        "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES \
         ('viewer','participation-viewer','Viewer',1,1),\
         ('outsider','participation-outsider','Outsider',1,1),\
         ('peer','participation-peer','Peer',1,1)",
    )
    .await
    .unwrap();
    db.execute(
        "INSERT INTO workspaces(id,name,name_key,created_at,updated_at) VALUES \
         ('workspace','Workspace','participation-workspace',1,1)",
    )
    .await
    .unwrap();
    db.execute(
        "INSERT INTO workspace_users(workspace_id,user_id,joined_at) VALUES \
         ('workspace','viewer',1),('workspace','outsider',1)",
    )
    .await
    .unwrap();
    db.execute(concat!(
        seed_channel!(
            id = "workspace-private",
            workspace_id = "workspace",
            title = "Private",
            title_key = "private",
        ),
        "; ",
        seed_channel!(
            id = "workspace-public",
            workspace_id = "workspace",
            title = "Public",
            title_key = "public",
            visibility = "public",
        ),
    ))
    .await
    .unwrap();
    db.execute(
        "INSERT INTO conversation_users(conversation_id,user_id,joined_at) VALUES \
         ('workspace-private','viewer',1),('workspace-public','viewer',1)",
    )
    .await
    .unwrap();
    db.execute(seed_direct_pair!(
        id = "pair",
        endpoint_a_kind = "human",
        endpoint_a_id = "peer",
        endpoint_b_kind = "human",
        endpoint_b_id = "viewer",
    ))
    .await
    .unwrap();
    db.close().await.unwrap();
}

#[tokio::test]
async fn strict_participation_has_no_public_widen_and_returns_actual_source_coordinates() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("conversation-participation.sqlite");
    seed(&path).await;
    let read = MessagingStore::open(&path)
        .await
        .unwrap()
        .conversation_participation();
    let viewer = UserId::parse("viewer").unwrap();
    let outsider = UserId::parse("outsider").unwrap();

    for id in ["workspace-private", "workspace-public", "pair"] {
        let conversation = ConversationId::parse(id).unwrap();
        let source = read.read(&viewer, &conversation).await.unwrap().unwrap();
        assert_eq!(source.viewer(), &viewer, "{id}");
        assert_eq!(source.conversation(), &conversation, "{id}");
        assert!(source.clone().bind(&viewer, &conversation).is_ok(), "{id}");
        assert!(
            source.clone().bind(&outsider, &conversation).is_err(),
            "{id}"
        );
        assert!(
            source
                .bind(&viewer, &ConversationId::parse("other").unwrap())
                .is_err(),
            "{id}"
        );
    }

    for (viewer, id, refusal) in [
        (
            "outsider",
            "workspace-private",
            ConversationParticipationRefusal::Forbidden,
        ),
        (
            "outsider",
            "workspace-public",
            ConversationParticipationRefusal::Forbidden,
        ),
        (
            "outsider",
            "pair",
            ConversationParticipationRefusal::Forbidden,
        ),
        (
            "viewer",
            "missing",
            ConversationParticipationRefusal::NotFound,
        ),
    ] {
        assert_eq!(
            read.read(
                &UserId::parse(viewer).unwrap(),
                &ConversationId::parse(id).unwrap(),
            )
            .await
            .unwrap(),
            Err(refusal),
            "{viewer}/{id}"
        );
    }
}

#[tokio::test]
async fn orphan_workspace_and_malformed_selected_row_fail_before_success() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("conversation-participation-corrupt.sqlite");
    seed(&path).await;
    let read = MessagingStore::open(&path)
        .await
        .unwrap()
        .conversation_participation();
    let viewer = UserId::parse("viewer").unwrap();
    let conversation = ConversationId::parse("workspace-private").unwrap();
    let mut db = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    db.execute("PRAGMA foreign_keys=OFF").await.unwrap();
    db.execute("DELETE FROM workspaces WHERE id='workspace'")
        .await
        .unwrap();
    assert_eq!(
        read.read(&viewer, &conversation).await.unwrap(),
        Err(ConversationParticipationRefusal::Forbidden)
    );

    db.execute(
        "INSERT INTO workspaces(id,name,name_key,created_at,updated_at) VALUES \
         ('workspace','Workspace','participation-workspace',1,1)",
    )
    .await
    .unwrap();
    db.execute("PRAGMA ignore_check_constraints=ON")
        .await
        .unwrap();
    db.execute("UPDATE conversations SET visibility='smuggled' WHERE id='workspace-private'")
        .await
        .unwrap();
    assert!(read.read(&viewer, &conversation).await.is_err());

    db.execute("UPDATE conversations SET visibility='private' WHERE id='workspace-private'")
        .await
        .unwrap();
    assert!(read.read(&viewer, &conversation).await.unwrap().is_ok());
}
