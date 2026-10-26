use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use serde::Deserialize;
use serde_json::Value;
use silverhand_backend_core::foundation::identity::UserId;
use silverhand_backend_server::conversation_messaging::messaging::collaboration::conversation_threads;
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
        "INSERT INTO hosts(id,name,kind,created_at) VALUES('h','host','local',1)",
        "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES \
         ('local','local','Local',1,1),('root-user','root-user','Root User',1,1),\
         ('reply-a','reply-a','Reply A',1,1),('reply-b','reply-b','Reply B',1,1),\
         ('reply-c','reply-c','Reply C',1,1),('reply-d','reply-d','Reply D',1,1)",
        "INSERT INTO agents(id,name,runtime,host_id,created_by_user_id,created_at,updated_at,display_name) \
         VALUES('root-agent','root-agent','codex','h','local',1,1,'Root Agent')",
        "INSERT INTO workspaces(id,name,name_key,created_at,updated_at) VALUES \
         ('threads-w','Threads Workspace','threads-workspace',1,1)",
        "INSERT OR IGNORE INTO workspace_users(workspace_id,user_id,joined_at,role) VALUES \
         ('threads-w','local',1,'member')",
        "INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,status,visibility,created_at,updated_at) \
         VALUES('c','threads-w','workspace','open','Threads','threads','idle','private',1,1)",
        "INSERT OR IGNORE INTO conversation_users(conversation_id,user_id,joined_at) VALUES('c','local',1)",
        "INSERT INTO messages(id,conversation_id,from_kind,from_user_id,content,seq,created_at,author_id,author_name,author_display_name) VALUES \
         ('root-1','c','user','root-user','root one',1,10,'root-user','root-user','Root User')",
        "INSERT INTO messages(id,conversation_id,from_kind,from_user_id,content,seq,created_at,thread_root_id,author_id,author_name,author_display_name) VALUES \
         ('r1-a','c','user','reply-a','reply',2,20,'root-1','reply-a','reply-a','Reply A')",
        "INSERT INTO messages(id,conversation_id,from_kind,content,seq,created_at) VALUES \
         ('root-2','c','system','system root',3,11)",
        "INSERT INTO messages(id,conversation_id,from_kind,from_user_id,content,seq,created_at,thread_root_id,author_id,author_name,author_display_name) VALUES \
         ('r2-b','c','user','reply-b','reply',4,20,'root-2','reply-b','reply-b','Reply B')",
        "INSERT INTO messages(id,conversation_id,from_kind,from_agent_id,content,seq,created_at,author_id,author_name,author_display_name) VALUES \
         ('root-3','c','agent','root-agent','root three',5,12,'root-agent','root-agent','Root Agent')",
        "INSERT INTO messages(id,conversation_id,from_kind,from_user_id,content,seq,created_at,thread_root_id,author_id,author_name,author_display_name) VALUES \
         ('r3-c','c','user','reply-c','reply',6,21,'root-3','reply-c','reply-c','Reply C'),\
         ('r3-d','c','user','reply-d','reply',7,22,'root-3','reply-d','reply-d','Reply D'),\
         ('r3-a','c','user','reply-a','reply',8,23,'root-3','reply-a','reply-a','Reply A'),\
         ('r3-b','c','user','reply-b','reply',9,30,'root-3','reply-b','reply-b','Reply B')",
        "INSERT INTO thread_followers(thread_root_id,conversation_id,principal_kind,principal_id,followed_at,source) VALUES \
         ('root-1','c','human','local',1,'manual'),('root-3','c','human','local',1,'manual')",
        "INSERT INTO conversation_reads(user_id,conversation_id,thread_root_id,last_read_seq,read_at) \
         VALUES('local','c','root-1',2,1)",
    ] {
        db.execute(statement).await.unwrap();
    }
    db.close().await.unwrap();
}

#[tokio::test]
async fn production_sqlite_router_consumes_the_typescript_wire_fixture() {
    let fixture: Vec<FixtureEntry> = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../rust/test-fixtures/fixtures/conversation-threads-wire.json"
    )))
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("conversation-threads-router.sqlite");
    seed(&path).await;
    let store = MessagingStore::open(&path).await.unwrap();
    let app = conversation_threads::router(store.conversation_threads());

    for entry in fixture {
        let mut request = Request::builder()
            .uri(format!("/api/conversations/c/threads{}", entry.query))
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
