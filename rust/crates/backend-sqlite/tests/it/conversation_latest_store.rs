use silverhand_backend_core::{
    conversation_messaging::messaging::collaboration::conversation_latest::ConversationLatestReadPort,
    foundation::identity::UserId,
};
use silverhand_backend_sqlite::conversation_messaging::messaging::MessagingStore;
use sqlx::{Connection, Executor};

use crate::current_schema;

async fn seed(path: &std::path::Path) {
    let mut db = current_schema::create_current_schema(path).await;
    db.execute(
        "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES \
         ('viewer','latest-viewer','Viewer',1,1),('peer','latest-peer','Peer',1,1),\
         ('outsider','latest-outsider','Outsider',1,1); \
         INSERT INTO workspaces(id,name,name_key,created_at,updated_at) VALUES \
         ('ws','Workspace','latest-ws',1,1); \
         INSERT INTO workspace_users(workspace_id,user_id,joined_at) VALUES \
         ('ws','viewer',1),('ws','peer',1),('ws','outsider',1); \
         INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,\
         status,visibility,created_at,updated_at) VALUES \
         ('chan','ws','workspace','open','Chan','chan','idle','public',1,1),\
         ('quiet','ws','workspace','open','Quiet','quiet','idle','public',1,1),\
         ('other','ws','workspace','open','Other','other','idle','public',1,1); \
         INSERT INTO conversation_users(conversation_id,user_id,joined_at) VALUES \
         ('chan','viewer',1),('chan','peer',1),('quiet','viewer',1),('other','outsider',1); \
         INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,endpoint_a_kind,\
         endpoint_a_id,endpoint_b_kind,endpoint_b_id,title,title_key,status,visibility,created_at,updated_at) \
         VALUES('pair',NULL,'direct_pair','fixed','human','peer','human','viewer','',NULL,'idle','private',1,1); \
         INSERT INTO messages(id,conversation_id,from_kind,from_user_id,content,metadata,seq,created_at,thread_root_id) VALUES \
         ('root','chan','user','peer','root',NULL,1,10,NULL),\
         ('reply','chan','user','peer','  a\n reply  ',NULL,2,20,'root'),\
         ('notice','chan','system',NULL,'renamed',NULL,3,30,NULL),\
         ('quiet-notice','quiet','system',NULL,'joined',NULL,1,5,NULL),\
         ('secret','other','user','outsider','secret',NULL,1,40,NULL),\
         ('dm','pair','user','viewer','',\
          '{\"attachments\":[{\"id\":\"att-1\",\"filename\":\"a.png\",\"size\":1,\"content_type\":\"image/png\"}]}',1,15,NULL);",
    )
    .await
    .unwrap();
    db.close().await.unwrap();
}

#[tokio::test]
async fn latest_is_the_newest_participant_visible_human_or_agent_message() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("conversation-latest.sqlite");
    seed(&path).await;
    let read = MessagingStore::open(&path)
        .await
        .unwrap()
        .conversation_latest();

    let items = read
        .list_latest(&UserId::parse("viewer").unwrap())
        .await
        .unwrap();
    let summary: Vec<_> = items
        .iter()
        .map(|item| {
            (
                item.conversation.as_str(),
                item.message.as_str(),
                item.thread_root.as_ref().map(|root| root.as_str()),
                item.preview.as_ref().map(|preview| preview.as_str()),
                item.attachment_count,
            )
        })
        .collect();
    // System messages never count (`notice`, `quiet` has none); a thread reply does;
    // a conversation the viewer does not participate in is absent; newest first.
    assert_eq!(
        summary,
        vec![
            ("chan", "reply", Some("root"), Some("a reply"), 0),
            ("pair", "dm", None, None, 1),
        ]
    );
}
