use silverhand_backend_core::{
    conversation_messaging::messaging::collaboration::attention_projection::{
        AttentionModesRefusal, AttentionProjectionPort,
    },
    foundation::identity::{UserId, WorkspaceId},
};
use silverhand_backend_sqlite::conversation_messaging::messaging::MessagingStore;
use sqlx::Connection;
use std::error::Error;

use crate::current_schema;

async fn seed(path: &std::path::Path) {
    let mut db = current_schema::create_current_schema(path).await;
    for sql in [
        "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES('u','u-projection','U',1,1),('other','other-projection','Other',1,1)",
        "INSERT INTO workspaces(id,name,name_key,created_at,updated_at) VALUES('w','W','w-projection',1,1),('hidden','Hidden','hidden-projection',1,1)",
        "INSERT INTO workspace_users(workspace_id,user_id,joined_at) VALUES('w','u',1)",
        "INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,status,created_at,updated_at,latest_message_seq) VALUES('c-b','w','workspace','open','B','b-projection','open',2,2,1),('c-c','w','workspace','open','C','c-projection','open',1,1,0),('c-a','w','workspace','open','A','a-projection','open',1,1,1)",
        "INSERT INTO conversation_users(conversation_id,user_id,joined_at) VALUES('c-a','u',1),('c-b','u',1),('c-c','u',1)",
        "INSERT INTO channel_attention_preferences(user_id,conversation_id,mode,updated_at) VALUES('u','c-a','mentions',1),('u','c-c','mute',1)",
        "INSERT INTO workspace_attention_preferences(user_id,workspace_id,muted,updated_at) VALUES('u','w',1,1),('u','hidden',1,1)",
        "INSERT INTO messages(id,conversation_id,from_kind,from_user_id,content,seq,created_at,author_id,author_name,author_display_name) VALUES('m-a','c-a','user','other','mention',1,1,'other','other-projection','Other'),('m-b','c-b','user','other','plain',1,1,'other','other-projection','Other')",
        "INSERT INTO notifications(id,kind,user_id,workspace_id,conversation_id,message_id,payload,created_at) VALUES('n-1','mention_self','u','w','c-a','m-a','{}',1),('n-2','mention_self','u','w','c-a','m-a','{}',2)",
    ] {
        sqlx::query(sql).execute(&mut db).await.unwrap();
    }
    db.close().await.unwrap();
}

#[tokio::test]
async fn joined_archive_fact_preserves_loud_and_directed_badges() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("archive-badges.sqlite");
    seed(&path).await;
    let store = MessagingStore::open(&path)
        .await
        .unwrap()
        .attention_projection();
    let viewer = UserId::parse("u").unwrap();
    let mut db = sqlx::SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    for (sql, expected) in [
        (
            "DELETE FROM workspace_attention_preferences WHERE user_id='u' AND workspace_id='w'",
            2,
        ),
        ("UPDATE conversations SET archived_at=0 WHERE id='c-b'", 1),
        ("UPDATE conversations SET archived_at=0 WHERE id='c-a'", 0),
        (
            "UPDATE conversations SET archived_at=NULL WHERE id IN ('c-a','c-b')",
            2,
        ),
        (
            "DELETE FROM conversation_users WHERE conversation_id='c-b' AND user_id='u'",
            1,
        ),
        ("UPDATE notifications SET read_at=4 WHERE id='n-1'", 1),
        ("UPDATE notifications SET read_at=4 WHERE id='n-2'", 0),
    ] {
        sqlx::query(sql).execute(&mut db).await.unwrap();
        let badges = store.badges(&viewer).await.unwrap();
        assert_eq!(badges.total().get(), expected, "{sql}");
        assert_eq!(
            badges
                .by_workspace()
                .iter()
                .map(|row| row.count().get())
                .sum::<i64>(),
            expected,
            "{sql}"
        );
    }
    db.close().await.unwrap();
}

#[tokio::test]
async fn badge_and_modes_share_the_directed_claim_and_stable_channel_order() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("projection.sqlite");
    seed(&path).await;
    let store = MessagingStore::open(&path)
        .await
        .unwrap()
        .attention_projection();
    let viewer = UserId::parse("u").unwrap();
    let badges = store.badges(&viewer).await.unwrap();
    assert_eq!(badges.total().get(), 1);
    assert_eq!(badges.by_workspace().len(), 1);
    assert_eq!(badges.by_workspace()[0].count().get(), 1);
    assert_eq!(
        badges
            .muted_workspaces()
            .iter()
            .map(WorkspaceId::as_str)
            .collect::<Vec<_>>(),
        ["w"]
    );

    let modes = store
        .modes(&viewer, &WorkspaceId::parse("w").unwrap())
        .await
        .unwrap()
        .unwrap();
    assert!(modes.muted());
    assert_eq!(
        modes
            .channels()
            .iter()
            .map(|row| (
                row.conversation().as_str(),
                row.mode().as_str(),
                row.effective_mode(modes.muted()).as_str(),
                row.directed_unread_count().get(),
            ))
            .collect::<Vec<_>>(),
        [
            ("c-a", "mentions", "mute", 1),
            ("c-c", "mute", "mute", 0),
            ("c-b", "all", "mute", 0),
        ]
    );
}

#[tokio::test]
async fn modes_hides_missing_and_nonmember_workspaces() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("projection.sqlite");
    seed(&path).await;
    let store = MessagingStore::open(&path)
        .await
        .unwrap()
        .attention_projection();
    let viewer = UserId::parse("u").unwrap();
    for workspace in ["missing", "hidden"] {
        assert!(matches!(
            store
                .modes(&viewer, &WorkspaceId::parse(workspace).unwrap())
                .await
                .unwrap(),
            Err(AttentionModesRefusal::Forbidden)
        ));
    }
}

#[tokio::test]
async fn cursor_and_liveness_move_badge_and_mode_explanation_together() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("projection.sqlite");
    seed(&path).await;
    let store = MessagingStore::open(&path)
        .await
        .unwrap()
        .attention_projection();
    let viewer = UserId::parse("u").unwrap();
    let workspace = WorkspaceId::parse("w").unwrap();
    let mut db = sqlx::SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();

    sqlx::query("INSERT INTO conversation_reads(user_id,conversation_id,thread_root_id,last_read_seq,read_at) VALUES('u','c-a',NULL,1,2)")
        .execute(&mut db)
        .await
        .unwrap();
    assert_eq!(store.badges(&viewer).await.unwrap().total().get(), 0);
    let modes = store.modes(&viewer, &workspace).await.unwrap().unwrap();
    assert_eq!(
        modes
            .channels()
            .iter()
            .find(|row| row.conversation().as_str() == "c-a")
            .unwrap()
            .directed_unread_count()
            .get(),
        0
    );
    let unread_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM notifications WHERE conversation_id='c-a' AND read_at IS NULL",
    )
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(
        unread_rows, 2,
        "cursor, not notification mutation, cleared the claim"
    );

    sqlx::query("DELETE FROM conversation_reads")
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::query("UPDATE conversations SET archived_at=3 WHERE id='c-a'")
        .execute(&mut db)
        .await
        .unwrap();
    assert_eq!(store.badges(&viewer).await.unwrap().total().get(), 0);
    let modes = store.modes(&viewer, &workspace).await.unwrap().unwrap();
    assert_eq!(
        modes
            .channels()
            .iter()
            .map(|row| row.conversation().as_str())
            .collect::<Vec<_>>(),
        ["c-c", "c-b"]
    );
}

#[tokio::test]
async fn strict_decode_rejects_corrupt_mode_and_mute_with_source_chain() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("projection.sqlite");
    seed(&path).await;
    let store = MessagingStore::open(&path)
        .await
        .unwrap()
        .attention_projection();
    let viewer = UserId::parse("u").unwrap();
    let workspace = WorkspaceId::parse("w").unwrap();
    let mut db = sqlx::SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    sqlx::query("PRAGMA ignore_check_constraints=ON")
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::query("UPDATE channel_attention_preferences SET mode='loud' WHERE user_id='u' AND conversation_id='c-a'")
        .execute(&mut db)
        .await
        .unwrap();
    let error = store.modes(&viewer, &workspace).await.unwrap_err();
    assert!(error.source().is_some());
    assert!(format!("{error:?}").contains("MalformedMode"));

    sqlx::query("UPDATE channel_attention_preferences SET mode='mentions'")
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE workspace_attention_preferences SET muted=7 WHERE user_id='u' AND workspace_id='w'",
    )
    .execute(&mut db)
    .await
    .unwrap();
    let error = store.modes(&viewer, &workspace).await.unwrap_err();
    assert!(error.source().is_some());
    assert!(format!("{error:?}").contains("MalformedMute"));
}
