use silverhand_backend_core::{
    conversation_messaging::messaging::{
        MessageSeq, NotificationId,
        collaboration::activity::observe::{
            CommittedObservation, ObservationAftermath, ObservationEffectFailure, ObserveBatch,
            ObserveFailure, ObserveRefusal, ObserveSource, ObservedConversation, ObservedSeq,
            StreamPosition, observe,
        },
    },
    foundation::identity::{ConversationId, UserId},
};
use silverhand_backend_sqlite::conversation_messaging::messaging::{
    MessagingStore, collaboration::activity::observe::ObservationStoreError,
};
use sqlx::{Connection, Row};

use crate::current_schema;

struct NoEffects;

impl ObservationAftermath for NoEffects {
    async fn conversation_changed(
        &self,
        _: &CommittedObservation,
        _: &ObservedConversation,
    ) -> Result<(), ObservationEffectFailure> {
        Ok(())
    }

    async fn badge_changed(
        &self,
        _: &CommittedObservation,
        _: &silverhand_backend_core::foundation::identity::WorkspaceId,
    ) -> Result<(), ObservationEffectFailure> {
        Ok(())
    }

    async fn notifications_dismissed(
        &self,
        _: &CommittedObservation,
    ) -> Result<(), ObservationEffectFailure> {
        Ok(())
    }
}

async fn seed(path: &std::path::Path) {
    let mut db = current_schema::create_current_schema(path).await;
    for sql in [
        "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES \
         ('u','observe-u','Viewer',1,1),('other','observe-other','Other',1,1),\
         ('outsider','observe-outsider','Outsider',1,1)",
        "INSERT INTO workspaces(id,name,name_key,created_at,updated_at) VALUES \
         ('w','Observe','observe-w',1,1),('wrong-copy','Wrong','wrong-copy',1,1)",
        "INSERT INTO workspace_users(workspace_id,user_id,joined_at) VALUES('w','u',1)",
        "INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,\
         status,created_at,updated_at,latest_message_seq) VALUES \
         ('c','w','workspace','fixed','General','general-observe','open',1,1,3)",
        "INSERT INTO conversation_users(conversation_id,user_id,joined_at) VALUES('c','u',1)",
        "INSERT INTO messages(id,conversation_id,from_kind,from_user_id,content,seq,created_at) VALUES \
         ('m1','c','user','other','one',1,1),('root','c','user','other','root',2,2),\
         ('reply','c','user','other','reply',3,3)",
        "UPDATE messages SET thread_root_id='root' WHERE id='reply'",
        "INSERT INTO conversation_stream_heads(conversation_id,thread_root_id,latest_seq) VALUES \
         ('c',NULL,2),('c','root',3)",
        "INSERT INTO notifications(id,kind,user_id,workspace_id,conversation_id,message_id,payload,created_at) VALUES \
         ('n1','mention_self','u','w','c','m1','{}',1),\
         ('n2','mention_self','u','wrong-copy','c','reply','{}',2),\
         ('foreign','mention_self','other','w','c','m1','{}',3)",
        "INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,status,\
         created_at,updated_at,latest_message_seq,endpoint_a_kind,endpoint_a_id,endpoint_b_kind,endpoint_b_id) VALUES \
         ('pair',NULL,'direct_pair','fixed','',NULL,'open',2,2,1,'human','other','human','u')",
        "INSERT INTO messages(id,conversation_id,from_kind,from_user_id,content,seq,created_at) VALUES \
         ('pair-m','pair','user','other','pair',1,1)",
        "INSERT INTO conversation_stream_heads(conversation_id,thread_root_id,latest_seq) VALUES('pair',NULL,1)",
    ] {
        sqlx::query(sql).execute(&mut db).await.unwrap();
    }
    db.close().await.unwrap();
}

fn at(value: i64) -> StreamPosition {
    StreamPosition::AtSeq(ObservedSeq::Persisted(MessageSeq::try_from(value).unwrap()))
}

// Production ObservationPort, not a standalone SQLite upgrade experiment. The
// competing writer owns the lock before observe starts and releases it only after
// observe has had a chance to reach its write boundary.
#[tokio::test]
async fn observation_waits_for_competing_writer_and_validates_fresh_head() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("observe-contention.sqlite");
    seed(&path).await;
    let store = MessagingStore::open(&path).await.unwrap().observation();
    let mut writer = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new().filename(&path),
    )
    .await
    .unwrap();
    let mut tx = writer.begin_with("BEGIN IMMEDIATE").await.unwrap();
    sqlx::query("UPDATE conversation_stream_heads SET latest_seq=22 WHERE conversation_id='c' AND thread_root_id IS NULL")
        .execute(&mut *tx).await.unwrap();
    let reading = tokio::spawn(async move {
        observe(
            &store,
            &NoEffects,
            UserId::parse("u").unwrap(),
            ObserveBatch::new(vec![ObserveSource::Stream {
                conversation: ConversationId::parse("c").unwrap(),
                thread_root: None,
                position: StreamPosition::StreamHead,
            }])
            .unwrap(),
        )
        .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    tx.commit().await.unwrap();
    let outcome = reading.await.unwrap();
    assert!(
        outcome.is_ok(),
        "production observe contention: {outcome:?}"
    );
    assert!(outcome.unwrap().is_ok());
    let cursor: i64 = sqlx::query_scalar("SELECT last_read_seq FROM conversation_reads WHERE conversation_id='c' AND user_id='u' AND thread_root_id IS NULL")
        .fetch_one(&mut writer).await.unwrap();
    assert_eq!(
        cursor, 22,
        "validation must see the writer's committed head"
    );
    writer.close().await.unwrap();
}

#[tokio::test]
async fn observation_revalidates_membership_after_waiting_for_writer() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("observe-membership.sqlite");
    seed(&path).await;
    let store = MessagingStore::open(&path).await.unwrap().observation();
    let mut writer = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new().filename(&path),
    )
    .await
    .unwrap();
    let mut tx = writer.begin_with("BEGIN IMMEDIATE").await.unwrap();
    sqlx::query("DELETE FROM conversation_users WHERE conversation_id='c' AND user_id='u'")
        .execute(&mut *tx)
        .await
        .unwrap();
    let reading = tokio::spawn(async move {
        observe(
            &store,
            &NoEffects,
            UserId::parse("u").unwrap(),
            ObserveBatch::new(vec![ObserveSource::Stream {
                conversation: ConversationId::parse("c").unwrap(),
                thread_root: None,
                position: StreamPosition::StreamHead,
            }])
            .unwrap(),
        )
        .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    tx.commit().await.unwrap();
    assert!(matches!(
        reading.await.unwrap().unwrap(),
        Err(ObserveRefusal::Forbidden)
    ));
    let rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM conversation_reads WHERE conversation_id='c' AND user_id='u'",
    )
    .fetch_one(&mut writer)
    .await
    .unwrap();
    assert_eq!(rows, 0);
    let unread: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM notifications WHERE id='n1' AND read_at IS NULL")
            .fetch_one(&mut writer)
            .await
            .unwrap();
    assert_eq!(unread, 1);
    writer.close().await.unwrap();
}

#[tokio::test]
async fn observation_real_busy_timeout_remains_an_error_without_partial_writes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("observe-timeout.sqlite");
    seed(&path).await;
    let store = MessagingStore::open(&path).await.unwrap().observation();
    let mut writer = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new().filename(&path),
    )
    .await
    .unwrap();
    let tx = writer.begin_with("BEGIN IMMEDIATE").await.unwrap();
    let outcome = observe(
        &store,
        &NoEffects,
        UserId::parse("u").unwrap(),
        ObserveBatch::new(vec![ObserveSource::Stream {
            conversation: ConversationId::parse("c").unwrap(),
            thread_root: None,
            position: StreamPosition::StreamHead,
        }])
        .unwrap(),
    )
    .await;
    assert!(matches!(
        outcome,
        Err(ObserveFailure::Driver(ObservationStoreError::Sql(_)))
    ));
    tx.rollback().await.unwrap();
    let rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM conversation_reads WHERE conversation_id='c' AND user_id='u'",
    )
    .fetch_one(&mut writer)
    .await
    .unwrap();
    assert_eq!(rows, 0);
    writer.close().await.unwrap();
}

#[tokio::test]
async fn one_batch_is_atomic_monotone_and_returns_only_actual_committed_facts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("observe.sqlite");
    seed(&path).await;
    let store = MessagingStore::open(&path).await.unwrap().observation();
    let viewer = UserId::parse("u").unwrap();

    let batch = ObserveBatch::new(vec![
        ObserveSource::Stream {
            conversation: ConversationId::parse("c").unwrap(),
            thread_root: None,
            position: at(1),
        },
        ObserveSource::Notification(NotificationId::parse("n2").unwrap()),
        ObserveSource::Notification(NotificationId::parse("foreign").unwrap()),
    ])
    .unwrap();
    let done = observe(&store, &NoEffects, viewer.clone(), batch.clone())
        .await
        .unwrap()
        .unwrap();
    let committed = done.committed();
    assert!(committed.changed_anything());
    assert!(matches!(
        committed.conversations().next().unwrap(),
        ObservedConversation::Workspace { conversation, workspace }
            if conversation.as_str()=="c" && workspace.as_str()=="w"
    ));
    assert_eq!(
        committed
            .dismissed_notifications()
            .iter()
            .map(NotificationId::as_str)
            .collect::<Vec<_>>(),
        ["n1", "n2"]
    );
    // The copied notifications.workspace_id is not an attribution authority;
    // the changed workspace is already covered by the conversation fact.
    assert_eq!(committed.directed_badge_workspaces().count(), 0);

    let second = observe(&store, &NoEffects, viewer, batch)
        .await
        .unwrap()
        .unwrap();
    assert!(!second.committed().changed_anything());

    let mut db = sqlx::SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    let cursor: i64 = sqlx::query_scalar(
        "SELECT last_read_seq FROM conversation_reads WHERE user_id='u' AND conversation_id='c' AND thread_root_id IS NULL",
    )
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(cursor, 1);
    let rows = sqlx::query("SELECT id,read_at FROM notifications ORDER BY id")
        .fetch_all(&mut db)
        .await
        .unwrap();
    assert_eq!(
        rows.iter()
            .map(|row| (
                row.get::<String, _>("id"),
                row.get::<Option<i64>, _>("read_at").is_some()
            ))
            .collect::<Vec<_>>(),
        [
            ("foreign".into(), false),
            ("n1".into(), true),
            ("n2".into(), true)
        ]
    );
}

#[tokio::test]
async fn every_source_is_resolved_before_writes_and_refusal_order_is_stable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("observe-refusal.sqlite");
    seed(&path).await;
    let store = MessagingStore::open(&path).await.unwrap().observation();
    let viewer = UserId::parse("u").unwrap();
    let future = observe(
        &store,
        &NoEffects,
        viewer.clone(),
        ObserveBatch::new(vec![
            ObserveSource::Notification(NotificationId::parse("n1").unwrap()),
            ObserveSource::Stream {
                conversation: ConversationId::parse("c").unwrap(),
                thread_root: None,
                position: at(3),
            },
        ])
        .unwrap(),
    )
    .await
    .unwrap();
    assert!(matches!(future, Err(ObserveRefusal::PositionFuture)));

    let reply_as_root = observe(
        &store,
        &NoEffects,
        viewer,
        ObserveBatch::new(vec![ObserveSource::Stream {
            conversation: ConversationId::parse("c").unwrap(),
            thread_root: Some(
                silverhand_backend_core::conversation_messaging::messaging::ThreadRootId::parse(
                    "reply",
                )
                .unwrap(),
            ),
            position: StreamPosition::StreamHead,
        }])
        .unwrap(),
    )
    .await
    .unwrap();
    assert!(matches!(
        reply_as_root,
        Err(ObserveRefusal::ThreadRootIsReply)
    ));

    let forbidden = observe(
        &store,
        &NoEffects,
        UserId::parse("outsider").unwrap(),
        ObserveBatch::new(vec![ObserveSource::Stream {
            conversation: ConversationId::parse("pair").unwrap(),
            thread_root: None,
            position: StreamPosition::StreamHead,
        }])
        .unwrap(),
    )
    .await
    .unwrap();
    assert!(matches!(forbidden, Err(ObserveRefusal::Forbidden)));

    let mut db = sqlx::SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    let read: Option<i64> = sqlx::query_scalar("SELECT read_at FROM notifications WHERE id='n1'")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(read, None);

    sqlx::query("UPDATE notifications SET kind='mention_self',id='' WHERE id='n1'")
        .execute(&mut db)
        .await
        .unwrap();
    let error = observe(
        &store,
        &NoEffects,
        UserId::parse("u").unwrap(),
        ObserveBatch::new(vec![ObserveSource::Stream {
            conversation: ConversationId::parse("c").unwrap(),
            thread_root: None,
            position: at(1),
        }])
        .unwrap(),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        ObserveFailure::Driver(ObservationStoreError::MalformedIdentity(_))
    ));
    let read: Option<i64> = sqlx::query_scalar("SELECT read_at FROM notifications WHERE id=''")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(
        read, None,
        "post-update decode failure must roll back the batch"
    );
}

#[tokio::test]
async fn direct_pair_uses_endpoint_authority_and_corruption_rolls_back() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("observe-pair.sqlite");
    seed(&path).await;
    let store = MessagingStore::open(&path).await.unwrap().observation();
    let viewer = UserId::parse("u").unwrap();
    let pair = observe(
        &store,
        &NoEffects,
        viewer.clone(),
        ObserveBatch::new(vec![ObserveSource::Stream {
            conversation: ConversationId::parse("pair").unwrap(),
            thread_root: None,
            position: StreamPosition::StreamHead,
        }])
        .unwrap(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(matches!(
        pair.committed().conversations().next().unwrap(),
        ObservedConversation::DirectPair { conversation } if conversation.as_str()=="pair"
    ));

    let mut db = sqlx::SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    sqlx::query("PRAGMA ignore_check_constraints=ON")
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::query("UPDATE notifications SET kind='unknown' WHERE id='n1'")
        .execute(&mut db)
        .await
        .unwrap();
    let error = observe(
        &store,
        &NoEffects,
        viewer,
        ObserveBatch::new(vec![ObserveSource::Notification(
            NotificationId::parse("n1").unwrap(),
        )])
        .unwrap(),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        ObserveFailure::Driver(ObservationStoreError::MalformedNotificationKind)
    ));
    let read: Option<i64> = sqlx::query_scalar("SELECT read_at FROM notifications WHERE id='n1'")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(read, None);
}
