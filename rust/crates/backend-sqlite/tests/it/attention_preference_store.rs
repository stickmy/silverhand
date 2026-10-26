use crate::current_schema;
use silverhand_backend_core::{
    conversation_messaging::messaging::{
        EnvironmentUnavailable, SendClock, collaboration::attention_preference::*,
    },
    foundation::identity::{ConversationId, UserId, WorkspaceId},
};
use silverhand_backend_sqlite::conversation_messaging::messaging::MessagingStore;
use sqlx::Connection;
use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

async fn seeded(path: &std::path::Path) {
    let mut db = current_schema::create_current_schema(path).await;
    for sql in [
        "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES('u','u','U',1,1),('outsider','outsider','Out',1,1)",
        "INSERT INTO hosts(id,name,kind,created_at,user_id) VALUES('h','H','local',1,'u')",
        "INSERT INTO agents(id,name,runtime,host_id,display_name,created_by_user_id,created_at,updated_at,materialization_status,session_scope) VALUES('a','a','codex','h','A','u',1,1,'active','agent')",
        "INSERT INTO workspaces(id,name,name_key,created_at,owner_user_id,updated_at) VALUES('w','W','w',1,'u',1)",
        "INSERT INTO workspaces(id,name,name_key,owner_agent_id,created_at,owner_user_id,updated_at) VALUES('home','Home',NULL,'a',1,'u',1)",
        "INSERT INTO workspace_users(workspace_id,user_id,joined_at) VALUES('w','u',1)",
        "INSERT INTO workspace_users(workspace_id,user_id,joined_at) VALUES('home','u',1)",
        "INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,status,created_at,updated_at) VALUES('c','w','workspace','open','C','c','open',1,1)",
        "INSERT INTO conversation_users(conversation_id,user_id,joined_at) VALUES('c','u',1)",
        "INSERT INTO conversations(id,placement_kind,roster_policy,endpoint_a_kind,endpoint_a_id,endpoint_b_kind,endpoint_b_id,title,status,created_at,updated_at) VALUES('pair','direct_pair','fixed','human','outsider','human','u','','open',1,1)",
    ] {
        sqlx::query(sql).execute(&mut db).await.unwrap();
    }
    db.close().await.unwrap();
}

#[tokio::test]
async fn three_operation_specific_faces_round_trip() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("a.sqlite");
    seeded(&path).await;
    let store = MessagingStore::open(&path)
        .await
        .unwrap()
        .attention_preferences();
    let u = UserId::parse("u").unwrap();
    let w = WorkspaceId::parse("w").unwrap();
    let c = ConversationId::parse("c").unwrap();
    assert!(
        !store
            .get_workspace(&u, w.clone())
            .await
            .unwrap()
            .unwrap()
            .muted()
    );
    let target = store.admit_channel(&u, &c).await.unwrap().unwrap();
    let channel = store
        .commit_channel(target, ChannelAttentionMode::Mentions)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(channel.workspace(), &w);
    assert_eq!(channel.conversation(), &c);
    assert_eq!(channel.mode(), ChannelAttentionMode::Mentions);
    let target = store.admit_workspace_write(&u, &w).await.unwrap().unwrap();
    let workspace = store.commit_workspace(target, true).await.unwrap().unwrap();
    assert!(workspace.muted());
    assert!(store.get_workspace(&u, w).await.unwrap().unwrap().muted());
}

#[tokio::test]
async fn admission_refusal_order_and_operation_specific_reasons_are_exact() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("a.sqlite");
    seeded(&path).await;
    let store = MessagingStore::open(&path)
        .await
        .unwrap()
        .attention_preferences();
    let u = UserId::parse("u").unwrap();
    assert!(matches!(
        store
            .admit_channel(&u, &ConversationId::parse("pair").unwrap())
            .await
            .unwrap(),
        Err(ChannelAttentionAdmissionRefusal::ChannelNotFound)
    ));
    assert!(matches!(
        store
            .admit_workspace_write(&u, &WorkspaceId::parse("missing").unwrap())
            .await
            .unwrap(),
        Err(WorkspaceAttentionAdmissionRefusal::Forbidden)
    ));
    let home = WorkspaceId::parse("home").unwrap();
    assert!(matches!(
        store.get_workspace(&u, home.clone()).await.unwrap(),
        Err(WorkspaceAttentionReadRefusal::WorkspaceNotFound)
    ));
    assert!(matches!(
        store.admit_workspace_write(&u, &home).await.unwrap(),
        Err(WorkspaceAttentionAdmissionRefusal::WorkspaceNotFound)
    ));
}

#[tokio::test]
async fn writer_recheck_catches_revocation_between_preflight_and_commit() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("a.sqlite");
    seeded(&path).await;
    let store = MessagingStore::open(&path)
        .await
        .unwrap()
        .attention_preferences();
    let u = UserId::parse("u").unwrap();
    let c = ConversationId::parse("c").unwrap();
    let w = WorkspaceId::parse("w").unwrap();
    let channel = store.admit_channel(&u, &c).await.unwrap().unwrap();
    let workspace = store.admit_workspace_write(&u, &w).await.unwrap().unwrap();
    let mut db = sqlx::SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    sqlx::query("DELETE FROM conversation_users WHERE conversation_id='c' AND user_id='u'")
        .execute(&mut db)
        .await
        .unwrap();
    assert!(matches!(
        store
            .commit_channel(channel, ChannelAttentionMode::All)
            .await
            .unwrap(),
        Err(ChannelAttentionWriteRefusal::Forbidden)
    ));
    sqlx::query("DELETE FROM workspace_users WHERE workspace_id='w' AND user_id='u'")
        .execute(&mut db)
        .await
        .unwrap();
    assert!(matches!(
        store.commit_workspace(workspace, true).await.unwrap(),
        Err(WorkspaceAttentionWriteRefusal::Forbidden)
    ));
}

#[derive(Clone)]
struct FailingClock(Arc<AtomicUsize>);
impl SendClock for FailingClock {
    fn now_unix_ms(&self) -> Result<i64, EnvironmentUnavailable> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(EnvironmentUnavailable::from_source(io::Error::other(
            "clock",
        )))
    }
}

#[tokio::test]
async fn preflight_avoids_the_clock_and_a_commit_clock_failure_writes_nothing() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("a.sqlite");
    seeded(&path).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let store = MessagingStore::open_with(&path, FailingClock(calls.clone()))
        .await
        .unwrap()
        .attention_preferences();
    let u = UserId::parse("u").unwrap();
    assert!(matches!(
        store
            .admit_channel(&u, &ConversationId::parse("missing").unwrap())
            .await
            .unwrap(),
        Err(ChannelAttentionAdmissionRefusal::ChannelNotFound)
    ));
    assert!(matches!(
        store
            .admit_workspace_write(&u, &WorkspaceId::parse("missing").unwrap())
            .await
            .unwrap(),
        Err(WorkspaceAttentionAdmissionRefusal::Forbidden)
    ));
    assert!(matches!(
        store
            .admit_workspace_write(&u, &WorkspaceId::parse("home").unwrap())
            .await
            .unwrap(),
        Err(WorkspaceAttentionAdmissionRefusal::WorkspaceNotFound)
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let target = store
        .admit_channel(&u, &ConversationId::parse("c").unwrap())
        .await
        .unwrap()
        .unwrap();
    let error = store
        .commit_channel(target, ChannelAttentionMode::All)
        .await
        .unwrap_err();
    assert!(std::error::Error::source(&error).is_some());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let mut db = sqlx::SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM channel_attention_preferences")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(count, 0);
}
