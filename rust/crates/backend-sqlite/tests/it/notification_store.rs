use crate::current_schema;
use silverhand_backend_core::{
    conversation_messaging::messaging::{
        NotificationId,
        collaboration::notification::{
            NotificationClearPort, NotificationClearSelector, NotificationListQuery,
            NotificationReadPort,
        },
    },
    foundation::identity::UserId,
};
use silverhand_backend_sqlite::conversation_messaging::messaging::{
    MessagingStore, collaboration::NotificationStoreError,
};
use sqlx::Connection;

async fn seeded(path: &std::path::Path) {
    let mut db = current_schema::create_current_schema(path).await;
    for sql in [
        "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES('u','u','U',1,1),('other','o','O',1,1)",
        "INSERT INTO workspaces(id,name,name_key,created_at,owner_user_id,updated_at) VALUES('w1','W1','w1',1,'u',1),('w2','W2','w2',1,'u',1)",
        "INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,status,created_at,updated_at) VALUES('c1','w1','workspace','open','C1','c1','open',1,1),('c2','w2','workspace','open','C2','c2','open',1,1)",
        "INSERT INTO workspace_users(workspace_id,user_id,joined_at) VALUES('w1','u',1),('w2','u',1)",
        "INSERT INTO conversation_users(conversation_id,user_id,joined_at) VALUES('c1','u',1),('c2','u',1)",
        "INSERT INTO channel_attention_preferences(user_id,conversation_id,mode,updated_at) VALUES('u','c1','mute',1)",
        "INSERT INTO messages(id,conversation_id,from_kind,from_user_id,content,seq,created_at) VALUES('m1','c1','user','other','one',1,1),('m2','c2','user','other','two',1,2)",
        "INSERT INTO notifications(id,kind,user_id,workspace_id,conversation_id,message_id,payload,created_at) VALUES('n1','mention_self','u','w2','c1','m1','{}',10),('n2','mention_self','u',NULL,'c2','m2','{}',10),('n3','agent_start_failed','u','w1',NULL,NULL,'{}',10),('n4','agent_start_failed','u','w2',NULL,NULL,'{}',10),('foreign','mention_self','other','w1','c1','m1','{}',20)",
    ] {
        sqlx::query(sql).execute(&mut db).await.unwrap();
    }
    db.close().await.unwrap();
}

async fn unread_ids(port: &impl NotificationReadPort, viewer: &UserId) -> Vec<String> {
    port.list(
        viewer,
        NotificationListQuery {
            unread_only: true,
            limit: 100,
        },
    )
    .await
    .expect("unread list succeeds")
    .into_iter()
    .map(|listed| listed.notification.id.as_str().to_owned())
    .collect()
}

#[tokio::test]
async fn viewer_read_and_atomic_clear_return_the_marked_count() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("n.sqlite");
    seeded(&path).await;
    let store = MessagingStore::open(&path).await.unwrap();
    let n = store.notifications();
    let viewer = UserId::parse("u").unwrap();
    let list = n
        .list(
            &viewer,
            NotificationListQuery {
                unread_only: true,
                limit: 100,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        list.iter()
            .map(|n| n.notification.id.as_str())
            .collect::<Vec<_>>(),
        ["n4", "n3", "n2", "n1"]
    );
    let summary = n.unread_summary(&viewer).await.unwrap();
    assert_eq!(summary.total, 4);
    assert_eq!(
        summary
            .by_workspace
            .iter()
            .map(|(id, count)| (id.as_str(), *count))
            .collect::<Vec<_>>(),
        vec![("w1", 1), ("w2", 3)]
    );
    assert_eq!(
        summary
            .latest_by_workspace
            .iter()
            .map(|(id, anchor)| (
                id.as_str(),
                anchor.conversation.as_str(),
                anchor.message.as_str(),
                anchor.thread_root.as_ref().map(|id| id.as_str())
            ))
            .collect::<Vec<_>>(),
        vec![("w2", "c2", "m2", None)]
    );
    let by_id = n
        .clear(
            viewer.clone(),
            NotificationClearSelector::Id(NotificationId::parse("n1").unwrap()),
        )
        .await
        .unwrap();
    assert_eq!(by_id.marked(), 1);
    assert_eq!(unread_ids(&n, &viewer).await, ["n4", "n3", "n2"]);
    let by_workspace = n
        .clear(
            viewer.clone(),
            NotificationClearSelector::Workspace(
                silverhand_backend_core::foundation::identity::WorkspaceId::parse("w1").unwrap(),
            ),
        )
        .await
        .unwrap();
    assert_eq!(by_workspace.marked(), 1);
    assert_eq!(unread_ids(&n, &viewer).await, ["n4", "n2"]);
    let by_conversation = n
        .clear(
            viewer.clone(),
            NotificationClearSelector::Conversation(
                silverhand_backend_core::foundation::identity::ConversationId::parse("c2").unwrap(),
            ),
        )
        .await
        .unwrap();
    assert_eq!(by_conversation.marked(), 1);
    assert_eq!(unread_ids(&n, &viewer).await, ["n4"]);
    let committed = n
        .clear(viewer.clone(), NotificationClearSelector::All)
        .await
        .unwrap();
    assert_eq!(committed.marked(), 1);
    assert!(
        n.list(
            &viewer,
            NotificationListQuery {
                unread_only: true,
                limit: 100
            }
        )
        .await
        .unwrap()
        .is_empty()
    );
    let mut db = sqlx::SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    let foreign: Option<i64> =
        sqlx::query_scalar("SELECT read_at FROM notifications WHERE id='foreign'")
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(foreign, None);
}

#[tokio::test]
async fn malformed_selected_identity_or_scope_rolls_back_the_entire_clear() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("corrupt-notifications.sqlite");
    seeded(&path).await;
    let mut db = sqlx::SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    sqlx::query("UPDATE notifications SET id='' WHERE id='n4'")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();

    let store = MessagingStore::open(&path).await.unwrap();
    let notifications = store.notifications();
    let viewer = UserId::parse("u").unwrap();
    let failure = notifications
        .clear(viewer.clone(), NotificationClearSelector::All)
        .await
        .expect_err("an empty stored identity must refuse the clear");
    assert!(
        matches!(
            std::error::Error::source(&failure)
                .and_then(|source| source.downcast_ref::<NotificationStoreError>()),
            Some(NotificationStoreError::MalformedIdentity)
        ),
        "the refusal must be the identity fault, not the scope fault this test checks next"
    );

    let mut db = sqlx::SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    let read_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM notifications WHERE user_id='u' AND read_at IS NOT NULL",
    )
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(read_rows, 0, "decode failure must precede every write");
    sqlx::query("PRAGMA foreign_keys=OFF")
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::query("UPDATE notifications SET id='n4', workspace_id='' WHERE id=''")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();

    assert!(
        notifications
            .clear(viewer.clone(), NotificationClearSelector::All)
            .await
            .is_err()
    );
    let mut db = sqlx::SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    let read_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM notifications WHERE user_id='u' AND read_at IS NOT NULL",
    )
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(
        read_rows, 0,
        "scope corruption must also leave every row unread"
    );
    sqlx::query("UPDATE notifications SET workspace_id='w2' WHERE id='n4'")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    assert_eq!(
        notifications
            .clear(viewer, NotificationClearSelector::All)
            .await
            .unwrap()
            .marked(),
        4
    );
}

#[tokio::test]
async fn list_resolves_the_anchor_sender_through_the_shared_author_authority() {
    use silverhand_backend_core::conversation_messaging::messaging::{
        ActiveAuthor, MessageAuthor, PrincipalKind,
    };
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("sender.sqlite");
    let mut db = current_schema::create_current_schema(&path).await;
    for sql in [
        "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES('u','u','U',1,1),('live','live','Live Renamed',1,1)",
        "INSERT INTO workspaces(id,name,name_key,created_at,owner_user_id,updated_at) VALUES('w1','W1','w1',1,'u',1)",
        "INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,status,created_at,updated_at) VALUES('c1','w1','workspace','open','C1','c1','open',1,1)",
        "INSERT INTO messages(id,conversation_id,from_kind,from_user_id,content,seq,created_at,author_id,author_name,author_display_name) VALUES \
         ('m-live','c1','user','live','hi',1,1,'live','live','Live Old'), \
         ('m-gone','c1','user',NULL,'hi',2,2,'gone','gone','Gone'), \
         ('m-lost','c1','user',NULL,'hi',3,3,NULL,NULL,NULL), \
         ('m-sys','c1','system',NULL,'hi',4,4,NULL,NULL,NULL)",
        "INSERT INTO notifications(id,kind,user_id,workspace_id,conversation_id,message_id,payload,created_at) VALUES \
         ('n-live','mention_self','u','w1','c1','m-live','{}',7), \
         ('n-ext','mention_external','u','w1','c1','m-live','{}',6), \
         ('n-gone','mention_self','u','w1','c1','m-gone','{}',5), \
         ('n-lost','mention_self','u','w1','c1','m-lost','{}',4), \
         ('n-sys','mention_self','u','w1','c1','m-sys','{}',3), \
         ('n-anchorless','mention_self','u','w1','c1',NULL,'{}',2), \
         ('n-failed','agent_start_failed','u','w1',NULL,NULL,'{}',1)",
    ] {
        sqlx::query(sql).execute(&mut db).await.unwrap();
    }
    db.close().await.unwrap();
    let store = MessagingStore::open(&path).await.unwrap();
    let list = store
        .notifications()
        .list(
            &UserId::parse("u").unwrap(),
            NotificationListQuery {
                unread_only: false,
                limit: 100,
            },
        )
        .await
        .unwrap();
    let senders = list
        .iter()
        .map(|listed| (listed.notification.id.as_str(), listed.sender.clone()))
        .collect::<Vec<_>>();
    let live_name = |sender: &Option<MessageAuthor>| match sender {
        Some(MessageAuthor::Active(ActiveAuthor::Human(human))) => {
            Some(human.display_name().to_owned())
        }
        _ => None,
    };
    assert_eq!(senders[0].0, "n-live");
    assert_eq!(live_name(&senders[0].1).as_deref(), Some("Live Renamed"));
    assert_eq!(senders[1].0, "n-ext");
    assert_eq!(live_name(&senders[1].1).as_deref(), Some("Live Renamed"));
    assert_eq!(senders[2].0, "n-gone");
    assert!(matches!(senders[2].1, Some(MessageAuthor::Deleted(_))));
    assert_eq!(
        senders[3..],
        [
            (
                "n-lost",
                Some(MessageAuthor::unresolved(PrincipalKind::Human))
            ),
            ("n-sys", None),
            ("n-anchorless", None),
            ("n-failed", None),
        ]
    );
}
