use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use serde::Deserialize;
use serde_json::Value;
use silverhand_backend_core::foundation::identity::UserId;
use silverhand_backend_server::conversation_messaging::messaging::collaboration::conversation_message_window;
use silverhand_backend_sqlite::conversation_messaging::messaging::MessagingStore;
use sqlx::{Connection, Executor, SqliteConnection, sqlite::SqliteConnectOptions};
use tower::ServiceExt;

#[derive(Debug, Deserialize)]
struct FixtureEntry {
    name: String,
    query: String,
    status: u16,
    body: Value,
}

async fn seed(path: &std::path::Path) {
    const SCHEMA: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../rust/test-fixtures/migration-baseline/schema-current.sql"
    ));
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .foreign_keys(true);
    let mut db = SqliteConnection::connect_with(&options).await.unwrap();
    db.execute(sqlx::raw_sql(SCHEMA)).await.unwrap();
    for statement in [
        "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES \
         ('local','local-test','Local Test User',1,1),('reply-a','reply-a','Reply A',1,1),\
         ('reply-b','reply-b','Reply B',1,1),('outsider','outsider','Outsider',1,1)",
        "INSERT INTO workspaces(id,name,name_key,created_at,updated_at) VALUES \
         ('window-w','Window Workspace','window-workspace',1,1)",
        "INSERT INTO workspace_users(workspace_id,user_id,joined_at,role) VALUES \
         ('window-w','local',1,'member')",
        "INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,\
         status,visibility,latest_message_seq,created_at,updated_at) VALUES \
         ('c','window-w','workspace','open','Window','window','idle','public',5,1,1)",
        "INSERT INTO conversation_users(conversation_id,user_id,joined_at) VALUES('c','local',1)",
        "INSERT INTO messages(id,conversation_id,from_kind,from_user_id,content,seq,created_at,\
         author_id,author_name,author_display_name) VALUES \
         ('root-1','c','user','local','root one',1,10,'local','local','Local User'),\
         ('root-2','c','user','local','root two',2,20,'local','local','Local User')",
        "INSERT INTO messages(id,conversation_id,from_kind,from_user_id,content,metadata,seq,\
         created_at,thread_root_id,author_id,author_name,author_display_name) VALUES \
         ('reply-1','c','user','reply-a',' first   reply ',NULL,3,30,'root-1','reply-a','reply-a','Reply A'),\
         ('reply-system','c','system',NULL,'system reply',NULL,4,40,'root-1',NULL,NULL,NULL),\
         ('reply-2','c','user','reply-b','second reply',\
          '{\"attachments\":[{\"id\":\"att-2\",\"filename\":\"note.txt\",\"size\":4,\"content_type\":\"text/plain\",\"label\":\"File #2\"}]}',\
          5,50,'root-1','reply-b','reply-b','Reply B')",
        "INSERT INTO conversation_stream_heads(conversation_id,thread_root_id,latest_seq) VALUES \
         ('c',NULL,2),('c','root-1',5)",
        "INSERT INTO message_reactions(id,message_id,reactor_kind,reactor_id,emoji,created_at) VALUES \
         ('reaction-1','root-2','user','local','👍',3),\
         ('reaction-2','root-2','user','outsider','👍',4)",
        "INSERT INTO tasks(id,workspace_id,task_number,conversation_id,title,status,creator_kind,\
         creator_user_id,source_message_id,created_at,board_key) VALUES \
         ('task-1','window-w',1,'c','Linked task','todo','user','local','root-2',1,'ws:window-w')",
    ] {
        db.execute(statement).await.unwrap();
    }
    db.close().await.unwrap();
}

#[tokio::test]
async fn production_sqlite_router_consumes_the_typescript_wire_fixture() {
    let fixture: Vec<FixtureEntry> = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../rust/test-fixtures/fixtures/conversation-message-window-wire.json"
    )))
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("conversation-message-window-router.sqlite");
    seed(&path).await;
    let store = MessagingStore::open(&path).await.unwrap();
    let app = conversation_message_window::router(store.conversation_message_window());

    for entry in fixture {
        if entry.name.starts_with("public_") {
            let mut db = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
                .await
                .unwrap();
            db.execute(
                "DELETE FROM conversation_users WHERE conversation_id='c' AND user_id='local'",
            )
            .await
            .unwrap();
            db.close().await.unwrap();
        }
        let mut request = Request::builder()
            .uri(format!("/api/channels/c/messages{}", entry.query))
            .body(Body::empty())
            .unwrap();
        request
            .extensions_mut()
            .insert(UserId::parse("local").unwrap());
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status().as_u16(), entry.status, "{}", entry.name);
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body, entry.body, "{}", entry.name);
    }
}

#[tokio::test]
async fn lagging_or_missing_auxiliary_heads_do_not_hide_persisted_messages() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lagging-head.sqlite");
    seed(&path).await;
    let mut db = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    db.execute("INSERT INTO messages(id,conversation_id,from_kind,content,metadata,seq,created_at) VALUES('wake','c','system','wake','{\"subtype\":\"schedule_fire\"}',6,60)")
        .await
        .unwrap();
    db.execute("UPDATE conversations SET latest_message_seq=6 WHERE id='c'")
        .await
        .unwrap();
    let store = MessagingStore::open(&path).await.unwrap();
    let app = conversation_message_window::router(store.conversation_message_window());
    for mutation in [
        "UPDATE conversation_stream_heads SET latest_seq=0 WHERE conversation_id='c'",
        "DELETE FROM conversation_stream_heads WHERE conversation_id='c'",
    ] {
        db.execute(mutation).await.unwrap();
        for (query, expected_last) in [("", "wake"), ("?thread_root_id=root-1", "reply-2")] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!("/api/channels/c/messages{query}"))
                        .extension(UserId::parse("local").unwrap())
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), 200);
            let body: Value =
                serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                    .unwrap();
            assert_eq!(
                body["messages"].as_array().unwrap().last().unwrap()["id"],
                expected_last
            );
            assert_eq!(
                body["stream_head_seq"], 0,
                "reading does not silently repair the head"
            );
        }
    }
    // Availability relaxation must not change the private-channel permission gate.
    db.execute("UPDATE conversations SET visibility='private' WHERE id='c'")
        .await
        .unwrap();
    db.close().await.unwrap();
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/channels/c/messages")
                .extension(UserId::parse("outsider").unwrap())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 403);
}

#[tokio::test]
async fn system_audit_actor_is_provenance_not_a_display_author() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("system-audit-window.sqlite");
    seed(&path).await;
    let mut db = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    // Both TS insertSystemMessage and Rust session-clear persist the initiating
    // user's FK on a system audit message, but project its author as null.
    db.execute("INSERT INTO messages(id,conversation_id,from_kind,from_user_id,content,metadata,seq,created_at) VALUES ('audit','c','system','local','session cleared','{\"subtype\":\"session_cleared\",\"actor_user_id\":\"local\"}',6,60); UPDATE conversations SET latest_message_seq=6 WHERE id='c'; UPDATE conversation_stream_heads SET latest_seq=6 WHERE conversation_id='c' AND thread_root_id IS NULL; UPDATE messages SET from_user_id='local' WHERE id='reply-system'").await.unwrap();
    db.close().await.unwrap();
    let store = MessagingStore::open(&path).await.unwrap();
    let app = conversation_message_window::router(store.conversation_message_window());
    for (query, id) in [("", "audit"), ("?thread_root_id=root-1", "reply-system")] {
        let request = Request::builder()
            .uri(format!("/api/channels/c/messages{query}"))
            .extension(UserId::parse("local").unwrap())
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status().as_u16(), 200);
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        let row = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == id)
            .unwrap();
        assert_eq!(row["from_kind"], "system");
        assert_eq!(row["from_user_id"], "local");
        assert!(row["author"].is_null());
    }
}
