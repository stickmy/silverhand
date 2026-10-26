use silverhand_backend_core::{
    conversation_messaging::messaging::{
        PrincipalIdentity,
        collaboration::conversation_threads::{
            ConversationThreadsReadPort, ThreadListFilter, ThreadListQuery, ThreadReadCorruption,
        },
    },
    foundation::identity::{ConversationId, UserId},
};
use silverhand_backend_sqlite::conversation_messaging::messaging::{
    MessagingStore, collaboration::conversation_threads::ConversationThreadsStoreError,
};
use sqlx::{Connection, Executor};

use crate::current_schema;

async fn seed(path: &std::path::Path) {
    let mut db = current_schema::create_current_schema(path).await;
    for statement in [
        "INSERT INTO hosts(id,name,kind,created_at) VALUES('h','host','local',1)",
        "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES \
         ('viewer','viewer','Viewer',1,1),('root-user','root-user','Root User',1,1),\
         ('reply-a','reply-a','Reply A',1,1),('reply-b','reply-b','Reply B',1,1),\
         ('reply-c','reply-c','Reply C',1,1),('reply-d','reply-d','Reply D',1,1),\
         ('outsider','outsider','Outsider',1,1)",
        "INSERT INTO agents(id,name,runtime,host_id,created_by_user_id,created_at,updated_at,display_name) \
         VALUES('root-agent','root-agent','codex','h','viewer',1,1,'Root Agent')",
        "INSERT INTO workspaces(id,name,name_key,created_at,updated_at) VALUES \
         ('w','Workspace','workspace',1,1)",
        "INSERT INTO workspace_users(workspace_id,user_id,joined_at,role) VALUES \
         ('w','viewer',1,'member'),('w','outsider',1,'member')",
        "INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,status,visibility,created_at,updated_at) \
         VALUES('c','w','workspace','open','Channel','channel','idle','private',1,1),\
         ('public','w','workspace','open','Public','public','idle','public',1,1)",
        "INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,endpoint_a_kind,endpoint_a_id,endpoint_b_kind,endpoint_b_id,title,title_key,status,visibility,created_at,updated_at) VALUES \
         ('pair',NULL,'direct_pair','fixed','agent','root-agent','human','viewer','',NULL,'idle','private',1,1)",
        "INSERT INTO conversation_users(conversation_id,user_id,joined_at) VALUES('c','viewer',1)",
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
         ('root-1','c','human','viewer',1,'manual'),('root-3','c','human','viewer',1,'manual')",
        "INSERT INTO conversation_reads(user_id,conversation_id,thread_root_id,last_read_seq,read_at) \
         VALUES('viewer','c','root-1',2,1)",
    ] {
        db.execute(statement).await.unwrap();
    }
    db.close().await.unwrap();
}

async fn read(
    path: &std::path::Path,
    viewer: &str,
    query: ThreadListQuery,
) -> Result<
    Result<
        silverhand_backend_core::conversation_messaging::messaging::collaboration::conversation_threads::ConversationThreadsPage,
        silverhand_backend_core::conversation_messaging::messaging::collaboration::conversation_threads::ConversationThreadsRefusal,
    >,
    silverhand_backend_core::foundation::persistence::PersistenceFailure,
>{
    read_conversation(path, viewer, "c", query).await
}

async fn read_conversation(
    path: &std::path::Path,
    viewer: &str,
    conversation: &str,
    query: ThreadListQuery,
) -> Result<
    Result<
        silverhand_backend_core::conversation_messaging::messaging::collaboration::conversation_threads::ConversationThreadsPage,
        silverhand_backend_core::conversation_messaging::messaging::collaboration::conversation_threads::ConversationThreadsRefusal,
    >,
    silverhand_backend_core::foundation::persistence::PersistenceFailure,
>{
    MessagingStore::open(path)
        .await
        .unwrap()
        .conversation_threads()
        .read(
            &UserId::parse(viewer).unwrap(),
            &ConversationId::parse(conversation).unwrap(),
            query,
        )
        .await
}

fn source(
    error: &silverhand_backend_core::foundation::persistence::PersistenceFailure,
) -> &ConversationThreadsStoreError {
    std::error::Error::source(error)
        .and_then(|source| source.downcast_ref())
        .expect("thread-list failure preserves its named store source")
}

#[tokio::test]
async fn total_order_cursor_repliers_and_followed_unread_share_the_product_authorities() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("conversation-threads.sqlite");
    seed(&path).await;
    let viewer = UserId::parse("viewer").unwrap();
    let conversation = ConversationId::parse("c").unwrap();

    let first_query = ThreadListQuery::new(2, None, ThreadListFilter::All).unwrap();
    let first = read(&path, "viewer", first_query)
        .await
        .unwrap()
        .unwrap()
        .bind(&viewer, &conversation, first_query)
        .unwrap();
    assert_eq!(
        first
            .threads()
            .iter()
            .map(|thread| thread.entry().root_id().as_str())
            .collect::<Vec<_>>(),
        ["root-3", "root-2"]
    );
    let root_three = &first.threads()[0];
    assert!(matches!(
        root_three.entry().root_author().unwrap().stable_identity(),
        Some(PrincipalIdentity::Agent(_))
    ));
    assert_eq!(root_three.entry().reply_count(), 4);
    assert_eq!(root_three.entry().replier_total(), 4);
    assert_eq!(
        root_three
            .entry()
            .repliers()
            .iter()
            .map(|author| author.stable_identity().unwrap().as_str().to_owned())
            .collect::<Vec<_>>(),
        ["reply-b", "reply-a", "reply-d"]
    );
    assert!(first.threads()[1].entry().root_author().is_none());

    let cursor = first.next_cursor().unwrap();
    assert_eq!(cursor.last_reply_at(), 20);
    assert_eq!(cursor.root_seq().get(), 3);
    let second_query = ThreadListQuery::new(2, Some(cursor), ThreadListFilter::All).unwrap();
    let second = read(&path, "viewer", second_query)
        .await
        .unwrap()
        .unwrap()
        .bind(&viewer, &conversation, second_query)
        .unwrap();
    assert_eq!(second.threads()[0].entry().root_id().as_str(), "root-1");
    assert!(second.next_cursor().is_none());

    let filtered_query = ThreadListQuery::new(30, None, ThreadListFilter::FollowedUnread).unwrap();
    let filtered = read(&path, "viewer", filtered_query)
        .await
        .unwrap()
        .unwrap()
        .bind(&viewer, &conversation, filtered_query)
        .unwrap();
    assert_eq!(filtered.threads().len(), 1);
    assert_eq!(filtered.threads()[0].entry().root_id().as_str(), "root-3");
    assert!(filtered.threads()[0].viewer_following());

    assert!(matches!(
        read(
            &path,
            "outsider",
            ThreadListQuery::new(30, None, ThreadListFilter::All).unwrap(),
        )
        .await
        .unwrap(),
        Err(silverhand_backend_core::conversation_messaging::messaging::collaboration::conversation_threads::ConversationThreadsRefusal::Forbidden)
    ));

    let empty_query = ThreadListQuery::new(30, None, ThreadListFilter::All).unwrap();
    assert!(
        read_conversation(&path, "viewer", "pair", empty_query)
            .await
            .unwrap()
            .is_ok()
    );
    assert!(matches!(
        read_conversation(&path, "outsider", "public", empty_query)
            .await
            .unwrap(),
        Err(silverhand_backend_core::conversation_messaging::messaging::collaboration::conversation_threads::ConversationThreadsRefusal::Forbidden)
    ));
    assert!(matches!(
        read_conversation(&path, "viewer", "missing", empty_query)
            .await
            .unwrap(),
        Err(silverhand_backend_core::conversation_messaging::messaging::collaboration::conversation_threads::ConversationThreadsRefusal::NotFound)
    ));
}

#[tokio::test]
async fn lookahead_and_replier_rows_are_strictly_decoded_before_page_projection() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("conversation-threads-corrupt.sqlite");
    seed(&path).await;
    let mut db = sqlx::SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    db.execute("PRAGMA ignore_check_constraints=ON")
        .await
        .unwrap();
    let query = ThreadListQuery::new(1, None, ThreadListFilter::All).unwrap();

    // root-2 is the look-ahead row when limit=1; it must still be decoded.
    db.execute("UPDATE messages SET created_at=-1 WHERE id='root-2'")
        .await
        .unwrap();
    let error = read(&path, "viewer", query).await.unwrap_err();
    assert!(matches!(
        source(&error),
        ConversationThreadsStoreError::Projection(ThreadReadCorruption::UnsafeNumber {
            field: "messages.root_created_at"
        })
    ));
    db.execute("UPDATE messages SET created_at=11 WHERE id='root-2'")
        .await
        .unwrap();

    // r2-b belongs to the look-ahead root too; author resolution must not
    // start only after the visible page has been sliced.
    db.execute("UPDATE messages SET from_kind='future' WHERE id='r2-b'")
        .await
        .unwrap();
    let error = read(&path, "viewer", query).await.unwrap_err();
    assert!(matches!(
        source(&error),
        ConversationThreadsStoreError::MalformedAuthorKind
    ));
    db.execute("UPDATE messages SET from_kind='user' WHERE id='r2-b'")
        .await
        .unwrap();
    assert!(read(&path, "viewer", query).await.unwrap().is_ok());
}
