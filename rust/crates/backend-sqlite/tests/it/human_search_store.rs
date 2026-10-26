use silverhand_backend_core::{
    conversation_messaging::messaging::{
        MessageId, PrincipalIdentity,
        collaboration::human_search::{
            HumanSearchOrder, HumanSearchQuery, HumanSearchReadPort, HumanSearchResult,
            HumanSearchStream, HumanSearchText,
        },
    },
    foundation::identity::{ConversationId, UserId},
};
use silverhand_backend_sqlite::conversation_messaging::messaging::MessagingStore;
use silverhand_backend_sqlite::conversation_messaging::messaging::collaboration::human_search::HumanSearchStoreError;
use sqlx::{Connection, Executor, Row};

use crate::current_schema;

async fn seed(path: &std::path::Path) {
    let mut db = current_schema::create_current_schema(path).await;
    for statement in [
        "INSERT INTO hosts(id,name,kind,created_at) VALUES('h','host','local',1)",
        "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES \
         ('viewer','viewer','Viewer',1,1),('outsider','outsider','Outsider',1,1),\
         ('author','author','Author',1,1)",
        "INSERT INTO agents(id,name,runtime,host_id,created_by_user_id,created_at,updated_at,\
         display_name) VALUES('agent','agent','codex','h','viewer',1,1,'Agent')",
        "INSERT INTO workspaces(id,name,name_key,description,created_at,updated_at) VALUES \
         ('w','Needle Space','needle space','needle workspace',1,30)",
        "INSERT INTO workspace_users(workspace_id,user_id,joined_at,role) VALUES \
         ('w','viewer',1,'member')",
        "INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,\
         status,visibility,created_at,updated_at) VALUES \
         ('c','w','workspace','open','Needle Channel','needle channel','idle','private',1,20)",
        "INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,endpoint_a_kind,\
         endpoint_a_id,endpoint_b_kind,endpoint_b_id,title,title_key,status,visibility,created_at,\
         updated_at) VALUES ('pair',NULL,'direct_pair','fixed','agent','agent','human','viewer','',\
         NULL,'idle','private',1,10)",
        "INSERT INTO conversation_users(conversation_id,user_id,joined_at) VALUES('c','viewer',1)",
        "INSERT INTO messages(id,conversation_id,from_kind,from_user_id,content,metadata,seq,\
         created_at,author_id,author_name,author_display_name) VALUES \
         ('root','c','user','author','needle root',NULL,1,10,'author','author','Author'),\
         ('older','c','system',NULL,'older root',NULL,2,11,NULL,NULL,NULL)",
        "INSERT INTO messages(id,conversation_id,from_kind,from_user_id,content,metadata,seq,\
         created_at,thread_root_id,author_id,author_name,author_display_name) VALUES \
         ('reply','c','user','author','needle reply',NULL,3,12,'root','author','author','Author')",
        "INSERT INTO messages(id,conversation_id,from_kind,from_agent_id,content,metadata,seq,\
         created_at,author_id,author_name,author_display_name) VALUES \
         ('pair-message','pair','agent','agent','needle pair',NULL,1,13,'agent','agent','Agent')",
    ] {
        db.execute(statement).await.unwrap();
    }
    // Stable snapshot identity remains search-authoritative after hard delete.
    db.execute("DELETE FROM users WHERE id='author'")
        .await
        .unwrap();
    db.close().await.unwrap();
}

fn query(
    stream: HumanSearchStream,
    conversation: Option<&str>,
    thread_root: Option<&str>,
    sender: Option<PrincipalIdentity>,
    limit: u16,
) -> HumanSearchQuery {
    HumanSearchQuery::admitted(
        HumanSearchText::parse("needle").unwrap(),
        stream,
        thread_root.map(|id| MessageId::parse(id).unwrap()),
        None,
        conversation.map(|id| ConversationId::parse(id).unwrap()),
        sender,
        None,
        None,
        HumanSearchOrder::RecencyDesc,
        limit,
    )
    .unwrap()
}

async fn read(
    path: &std::path::Path,
    query: HumanSearchQuery,
) -> Result<
    Result<
        silverhand_backend_core::conversation_messaging::messaging::collaboration::human_search::HumanSearchPage,
        silverhand_backend_core::conversation_messaging::messaging::collaboration::human_search::HumanSearchRefusal,
    >,
    silverhand_backend_core::foundation::persistence::PersistenceFailure,
>{
    read_as(path, "viewer", query).await
}

async fn read_as(
    path: &std::path::Path,
    viewer: &str,
    query: HumanSearchQuery,
) -> Result<
    Result<
        silverhand_backend_core::conversation_messaging::messaging::collaboration::human_search::HumanSearchPage,
        silverhand_backend_core::conversation_messaging::messaging::collaboration::human_search::HumanSearchRefusal,
    >,
    silverhand_backend_core::foundation::persistence::PersistenceFailure,
>{
    MessagingStore::open(path)
        .await
        .unwrap()
        .human_search()
        .search(&UserId::parse(viewer).unwrap(), query)
        .await
}

async fn drop_table_triggers(db: &mut sqlx::SqliteConnection, table: &str) {
    let rows = sqlx::query("SELECT name FROM sqlite_master WHERE type='trigger' AND tbl_name=?")
        .bind(table)
        .fetch_all(&mut *db)
        .await
        .unwrap();
    for row in rows {
        let name: String = row.try_get("name").unwrap();
        let statement = format!("DROP TRIGGER \"{}\"", name.replace('"', "\"\""));
        // Trigger names come from this test-owned schema snapshot, not input.
        sqlx::query(sqlx::AssertSqlSafe(statement))
            .execute(&mut *db)
            .await
            .unwrap();
    }
}

fn source(
    error: &silverhand_backend_core::foundation::persistence::PersistenceFailure,
) -> &HumanSearchStoreError {
    std::error::Error::source(error)
        .and_then(|source| source.downcast_ref())
        .expect("human-search failure preserves its named store source")
}

#[tokio::test]
async fn named_message_count_direct_pair_and_deleted_sender_share_one_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("human-search.sqlite");
    seed(&path).await;
    let viewer = UserId::parse("viewer").unwrap();

    let all_query = query(HumanSearchStream::All, None, None, None, 10);
    let all = read(&path, all_query.clone())
        .await
        .unwrap()
        .unwrap()
        .bind(&viewer, &all_query)
        .unwrap();
    assert_eq!(all.total().get(), 3);
    assert!(!all.truncated());
    assert!(matches!(all.results()[0], HumanSearchResult::Workspace(_)));
    assert!(matches!(
        all.results()[1],
        HumanSearchResult::Conversation(_)
    ));
    assert_eq!(
        all.results()
            .iter()
            .filter(|result| matches!(result, HumanSearchResult::Message(_)))
            .count(),
        3
    );

    let pair_query = query(HumanSearchStream::All, Some("pair"), None, None, 10);
    let pair = read(&path, pair_query.clone())
        .await
        .unwrap()
        .unwrap()
        .bind(&viewer, &pair_query)
        .unwrap();
    assert_eq!(pair.total().get(), 1);
    let HumanSearchResult::Message(message) = &pair.results()[0] else {
        panic!("direct-pair search returns its message arm")
    };
    assert_eq!(message.conversation().id().as_str(), "pair");

    let deleted_query = query(
        HumanSearchStream::All,
        Some("c"),
        None,
        Some(PrincipalIdentity::Human(UserId::parse("author").unwrap())),
        10,
    );
    let deleted = read(&path, deleted_query.clone())
        .await
        .unwrap()
        .unwrap()
        .bind(&viewer, &deleted_query)
        .unwrap();
    assert_eq!(deleted.total().get(), 2);
}

#[tokio::test]
async fn actual_direct_pair_placement_corruption_is_not_washed_into_a_valid_ref() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("human-search-placement.sqlite");
    seed(&path).await;
    let mut db = sqlx::SqliteConnection::connect(path.to_str().unwrap())
        .await
        .unwrap();
    drop_table_triggers(&mut db, "conversations").await;
    db.execute("PRAGMA ignore_check_constraints=ON")
        .await
        .unwrap();
    db.execute("UPDATE conversations SET roster_policy='open' WHERE id='pair'")
        .await
        .unwrap();
    db.close().await.unwrap();

    let pair_query = query(HumanSearchStream::All, Some("pair"), None, None, 10);
    let failure = read(&path, pair_query.clone()).await.unwrap_err();
    assert!(matches!(
        source(&failure),
        HumanSearchStoreError::Projection(
            silverhand_backend_core::conversation_messaging::messaging::collaboration::human_search::HumanSearchCorruption::Conversation
        )
    ));

    let mut db = sqlx::SqliteConnection::connect(path.to_str().unwrap())
        .await
        .unwrap();
    db.execute("PRAGMA ignore_check_constraints=ON")
        .await
        .unwrap();
    db.execute("UPDATE conversations SET roster_policy='fixed' WHERE id='pair'")
        .await
        .unwrap();
    db.close().await.unwrap();
    assert!(read(&path, pair_query).await.unwrap().is_ok());
}

#[tokio::test]
async fn loaded_thread_root_must_be_an_actual_root_and_repair_restores_search() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("human-search-thread.sqlite");
    seed(&path).await;
    let mut db = sqlx::SqliteConnection::connect(path.to_str().unwrap())
        .await
        .unwrap();
    drop_table_triggers(&mut db, "messages").await;
    db.execute("UPDATE messages SET thread_root_id='older' WHERE id='root'")
        .await
        .unwrap();
    db.close().await.unwrap();

    let thread_query = query(
        HumanSearchStream::Thread,
        Some("c"),
        Some("reply"),
        None,
        10,
    );
    assert!(matches!(
        read(&path, thread_query.clone()).await.unwrap(),
        Err(silverhand_backend_core::conversation_messaging::messaging::collaboration::human_search::HumanSearchRefusal::ThreadRootNotFound)
    ));

    let mut db = sqlx::SqliteConnection::connect(path.to_str().unwrap())
        .await
        .unwrap();
    db.execute("UPDATE messages SET thread_root_id=NULL WHERE id='root'")
        .await
        .unwrap();
    db.close().await.unwrap();
    let page = read(&path, thread_query.clone())
        .await
        .unwrap()
        .unwrap()
        .bind(&UserId::parse("viewer").unwrap(), &thread_query)
        .unwrap();
    assert_eq!(page.query().thread_root().unwrap().as_str(), "root");

    // Requested and effective query are byte-equal when the selected anchor is
    // already a root. The adapter-local outcome, not equality, distinguishes a
    // visible active search from the anti-enumeration empty arm.
    let root_query = query(HumanSearchStream::Thread, Some("c"), Some("root"), None, 10);
    let visible = read(&path, root_query.clone())
        .await
        .unwrap()
        .unwrap()
        .bind(&UserId::parse("viewer").unwrap(), &root_query)
        .unwrap();
    assert_eq!(visible.total().get(), 1);
    let invisible = read_as(&path, "outsider", root_query.clone())
        .await
        .unwrap()
        .unwrap()
        .bind(&UserId::parse("outsider").unwrap(), &root_query)
        .unwrap();
    assert_eq!(invisible.total().get(), 0);
    assert!(invisible.results().is_empty());
}
