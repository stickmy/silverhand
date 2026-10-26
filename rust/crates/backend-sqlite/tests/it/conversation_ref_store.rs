use silverhand_backend_core::{
    conversation_messaging::messaging::collaboration::conversation_ref::{
        ConversationRefReadPort, ConversationRefRefusal,
    },
    foundation::identity::{ConversationId, UserId},
};
use silverhand_backend_sqlite::conversation_messaging::messaging::MessagingStore;
use sqlx::{Connection, Executor, SqliteConnection};

use crate::current_schema;

async fn seed(path: &std::path::Path) {
    let mut db = current_schema::create_current_schema(path).await;
    db.execute(
        "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES \
         ('u','ref-u','User',1,1),('other','ref-other','Other',1,1),\
         ('teammate','ref-teammate','Teammate',1,1)",
    )
    .await
    .unwrap();
    db.execute(
        "INSERT INTO workspaces(id,name,name_key,created_at,updated_at) VALUES \
         ('w','Workspace','conversation-ref-store',1,1)",
    )
    .await
    .unwrap();
    db.execute("INSERT INTO workspace_users(workspace_id,user_id,joined_at) VALUES('w','u',1)")
        .await
        .unwrap();
    db.execute(
        "INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,\
         status,visibility,created_at,updated_at) VALUES \
         ('member','w','workspace','open','Member','member-ref','idle','private',1,1),\
         ('public','w','workspace','open','Public','public-ref','idle','public',1,1),\
         ('archived','w','workspace','open','Archived','archived-ref','idle','public',1,1),\
         ('fixed','w','workspace','fixed','Fixed','fixed-ref','idle','private',1,1)",
    )
    .await
    .unwrap();
    db.execute("UPDATE conversations SET archived_at=9 WHERE id='archived'")
        .await
        .unwrap();
    db.execute(
        "INSERT INTO conversation_users(conversation_id,user_id,joined_at) VALUES('member','u',1)",
    )
    .await
    .unwrap();
    db.execute(
        "INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,endpoint_a_kind,\
         endpoint_a_id,endpoint_b_kind,endpoint_b_id,title,title_key,status,visibility,created_at,updated_at) \
         VALUES('pair',NULL,'direct_pair','fixed','human','other','human','u','',NULL,'idle','private',1,1),\
         ('foreign-pair',NULL,'direct_pair','fixed','human','other','human','teammate','',NULL,'idle','private',1,1)",
    )
    .await
    .unwrap();
    db.close().await.unwrap();
}

#[tokio::test]
async fn strict_placement_status_and_visibility_decode_fail_before_projection() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("conversation-ref-corrupt.sqlite");
    seed(&path).await;
    let read = MessagingStore::open(&path)
        .await
        .unwrap()
        .conversation_refs();
    let viewer = UserId::parse("u").unwrap();
    let mut db = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    db.execute("PRAGMA ignore_check_constraints=ON")
        .await
        .unwrap();
    for sql in [
        "DROP TRIGGER trg_conversations_endpoints_immutable",
        "DROP TRIGGER trg_conversations_endpoints_birth",
        "DROP TRIGGER trg_conversations_endpoints_born_alive",
        "DROP TRIGGER conversations_placement_immutable",
    ] {
        db.execute(sql).await.unwrap();
    }

    for (sql, id) in [
        (
            "UPDATE conversations SET endpoint_a_kind='human',endpoint_a_id='u' WHERE id='member'",
            "member",
        ),
        (
            "UPDATE conversations SET roster_policy='open' WHERE id='pair'",
            "pair",
        ),
        (
            "UPDATE conversations SET status='unknown' WHERE id='public'",
            "public",
        ),
        (
            "UPDATE conversations SET visibility='unknown' WHERE id='fixed'",
            "fixed",
        ),
    ] {
        db.execute(sql).await.unwrap();
        let error = read
            .resolve(&viewer, &ConversationId::parse(id).unwrap())
            .await
            .unwrap_err();
        assert!(std::error::Error::source(&error).is_some(), "{id}");
    }
}

#[tokio::test]
async fn missing_workspace_is_forbidden_by_acl_before_the_workspace_face_can_404() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("conversation-ref-orphan.sqlite");
    let mut db = current_schema::create_current_schema(&path).await;
    db.execute("PRAGMA foreign_keys=OFF").await.unwrap();
    db.execute("INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES('u','orphan-u','U',1,1)")
        .await.unwrap();
    db.execute("INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,status,visibility,created_at,updated_at) VALUES('orphan','gone','workspace','open','Orphan','orphan-ref','idle','public',1,1)")
        .await.unwrap();
    db.execute(
        "INSERT INTO conversation_users(conversation_id,user_id,joined_at) VALUES('orphan','u',1)",
    )
    .await
    .unwrap();
    db.close().await.unwrap();

    let result = MessagingStore::open(&path)
        .await
        .unwrap()
        .conversation_refs()
        .resolve(
            &UserId::parse("u").unwrap(),
            &ConversationId::parse("orphan").unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(result, Err(ConversationRefRefusal::Forbidden));
}
