//! SQLx statement-count proof through the public thread follower state boundary.
use silverhand_backend_core::{
    conversation_messaging::messaging::{
        ThreadRootId, collaboration::thread_follow::ThreadFollowPort,
    },
    foundation::identity::{ConversationId, UserId},
};
use silverhand_backend_sqlite::conversation_messaging::messaging::MessagingStore;
use sqlx::{Connection, Executor, SqliteConnection, sqlite::SqliteConnectOptions};
#[path = "support/statement_log.rs"]
mod statement_log;
use statement_log::StatementLog;

#[tokio::test]
async fn follower_cardinality_does_not_grow_state_read_statements() {
    let log = StatementLog::default();
    tracing::subscriber::set_global_default(log.clone()).unwrap();
    const SCHEMA: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../rust/test-fixtures/migration-baseline/schema-current.sql"
    ));
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("thread-follow-set-authority.sqlite");
    let options = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true)
        .foreign_keys(true);
    let mut db = SqliteConnection::connect_with(&options).await.unwrap();
    db.execute(sqlx::raw_sql(SCHEMA)).await.unwrap();
    db.execute(sqlx::raw_sql(
            "INSERT INTO users(id,name,display_name,created_at,updated_at) \
             VALUES('local','local-thread-follow-set','Local',1,1); \
             INSERT INTO hosts(id,name,kind,created_at) VALUES('host','Host','local',1); \
             INSERT INTO workspaces(id,name,name_key,created_at,updated_at) \
             VALUES('w','Workspace','thread-follow-set',1,1); \
             INSERT INTO workspace_users(workspace_id,user_id,joined_at) VALUES('w','local',1); \
             INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,visibility,created_at,updated_at) \
             VALUES('c','w','workspace','open','General','thread-follow-set','private',1,1);",
        ))
        .await
        .unwrap();
    db.execute("INSERT INTO conversation_users(conversation_id,user_id,joined_at) VALUES('c','local',1); INSERT INTO messages(id,conversation_id,from_kind,from_user_id,content,seq,created_at) VALUES('root','c','user','local','root',1,1)").await.unwrap();
    for ordinal in 0..32_i64 {
        let id = format!("agent-{ordinal:02}");
        let name = format!("thread-follow-set-{ordinal:02}");
        sqlx::query(
            "INSERT INTO agents(id,name,runtime,host_id,created_at,updated_at,created_by_user_id) \
                 VALUES(?,?,'claude','host',1,1,'local')",
        )
        .bind(&id)
        .bind(name)
        .execute(&mut db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO conversation_agents(conversation_id,agent_id,joined_at) VALUES('c',?,1)",
        )
        .bind(&id)
        .execute(&mut db)
        .await
        .unwrap();
        sqlx::query("INSERT INTO thread_followers(thread_root_id,conversation_id,principal_kind,principal_id,followed_at,source) VALUES('root','c','agent',?,?,'manual')")
                .bind(&id).bind(ordinal).execute(&mut db).await.unwrap();
    }
    db.close().await.unwrap();

    let store = MessagingStore::open(&path).await.unwrap();
    let port = store.thread_follow();
    let viewer = UserId::parse("local").unwrap();
    let conversation = ConversationId::parse("c").unwrap();
    let root = ThreadRootId::parse("root").unwrap();
    port.state(&viewer, &conversation, &root)
        .await
        .unwrap()
        .unwrap();
    let before = log.len();
    let many = port
        .state(&viewer, &conversation, &root)
        .await
        .unwrap()
        .unwrap()
        .bind(&viewer, &conversation, &root)
        .unwrap();
    let many_reads = log.reads_since(before);
    let mut db = SqliteConnection::connect_with(&options).await.unwrap();
    sqlx::query("DELETE FROM thread_followers WHERE principal_id != 'agent-00'")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    let before = log.len();
    let one = port
        .state(&viewer, &conversation, &root)
        .await
        .unwrap()
        .unwrap()
        .bind(&viewer, &conversation, &root)
        .unwrap();
    let one_reads = log.reads_since(before);
    assert_eq!(many.agents().len(), 32);
    assert_eq!(one.agents().len(), 1);
    assert!(
        !one_reads.is_empty(),
        "SQLx capture must observe actual SELECT statements"
    );
    assert_eq!(
        one_reads.len(),
        many_reads.len(),
        "one: {one_reads:?}; many: {many_reads:?}"
    );
}
