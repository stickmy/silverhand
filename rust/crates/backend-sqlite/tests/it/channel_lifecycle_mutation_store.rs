use std::{
    error::Error,
    sync::{
        Arc,
        atomic::{AtomicI64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use silverhand_backend_core::{
    conversation_messaging::messaging::{
        SendClock, SendEntropy,
        collaboration::{
            channel_directory::ChannelVisibility,
            channel_lifecycle::{
                ChannelArchivePort, ChannelArchiveRefusal, ChannelMutationAdmissionPort,
                ChannelMutationAdmissionRefusal, ChannelRenamePort, ChannelRenameRefusal,
                ChannelVisibilityPort, ChannelVisibilityRefusal,
            },
        },
        conversation_title::CanonicalConversationTitle,
        environment::EnvironmentUnavailable,
    },
    foundation::identity::{ConversationId, UserId},
    foundation::persistence::PersistenceFailure,
};
use silverhand_backend_sqlite::conversation_messaging::messaging::{
    MessagingStore, collaboration::channel_lifecycle::ChannelLifecycleMutationStoreError,
};
use sqlx::{Connection, Executor, Row, SqliteConnection};

use crate::current_schema;

#[derive(Clone)]
struct FixedEnvironment {
    clock_calls: Arc<AtomicUsize>,
    entropy_calls: Arc<AtomicUsize>,
    next_time: Arc<AtomicI64>,
}

impl FixedEnvironment {
    fn new() -> Self {
        Self {
            clock_calls: Arc::new(AtomicUsize::new(0)),
            entropy_calls: Arc::new(AtomicUsize::new(0)),
            next_time: Arc::new(AtomicI64::new(77)),
        }
    }
}

impl SendClock for FixedEnvironment {
    fn now_unix_ms(&self) -> Result<i64, EnvironmentUnavailable> {
        self.clock_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.next_time.fetch_add(1, Ordering::SeqCst))
    }
}

impl SendEntropy for FixedEnvironment {
    fn fill(&self, bytes: &mut [u8]) -> Result<(), EnvironmentUnavailable> {
        let call = self.entropy_calls.fetch_add(1, Ordering::SeqCst);
        bytes.fill(u8::try_from(call + 1).unwrap());
        Ok(())
    }
}

fn user(raw: &str) -> UserId {
    UserId::parse(raw).unwrap()
}

fn conversation(raw: &str) -> ConversationId {
    ConversationId::parse(raw).unwrap()
}

fn title(raw: &str) -> CanonicalConversationTitle {
    CanonicalConversationTitle::parse(raw).unwrap()
}

async fn seed(path: &std::path::Path) {
    let mut db = current_schema::create_current_schema(path).await;
    for sql in [
        "INSERT INTO hosts(id,name,kind,created_at) VALUES('h','Lifecycle Host','local',1)",
        "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES \
         ('admin','lifecycle-admin','Admin',1,1),('owner','lifecycle-owner','Owner',1,1),\
         ('member','lifecycle-member','Member',1,1)",
        "INSERT INTO agents(id,name,runtime,host_id,created_by_user_id,created_at,updated_at,display_name) VALUES \
         ('a','lifecycle-agent-a','codex','h','admin',1,1,'A'),\
         ('b','lifecycle-agent-b','codex','h','admin',1,1,'B')",
        "INSERT INTO workspaces(id,name,name_key,created_at,updated_at) VALUES \
         ('w','Lifecycle Workspace','lifecycle-workspace',1,1)",
        "INSERT INTO workspace_users(workspace_id,user_id,joined_at,role) VALUES \
         ('w','admin',1,'admin'),('w','owner',1,'owner'),('w','member',1,'member')",
        "INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,status,visibility,created_at,updated_at) VALUES \
         ('c','w','workspace','open','General','general','idle','private',1,1),\
         ('fixed','w','workspace','fixed','Fixed','fixed','idle','private',1,1),\
         ('taken','w','workspace','open','Roadmap','roadmap','idle','private',1,1)",
        "INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,endpoint_a_kind,endpoint_a_id,endpoint_b_kind,endpoint_b_id,title,title_key,status,visibility,created_at,updated_at) VALUES \
         ('pair',NULL,'direct_pair','fixed','human','admin','human','member','',NULL,'idle','private',1,1)",
        "INSERT INTO conversation_agents(conversation_id,agent_id,joined_at) VALUES \
         ('c','b',2),('c','a',1)",
    ] {
        db.execute(sql).await.unwrap();
    }
    db.close().await.unwrap();
}

fn mutation_source(error: &PersistenceFailure) -> &ChannelLifecycleMutationStoreError {
    Error::source(error)
        .and_then(|source| source.downcast_ref())
        .expect("mutation failures preserve their named SQLite source")
}

fn error_chain(error: &(dyn Error + 'static)) -> String {
    let mut values = vec![error.to_string()];
    let mut current = error.source();
    while let Some(source) = current {
        values.push(source.to_string());
        current = source.source();
    }
    values.join(": ")
}

#[tokio::test]
async fn admission_and_writer_refusals_keep_not_found_forbidden_and_host_order() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir
        .path()
        .join("channel-lifecycle-mutation-admission.sqlite");
    seed(&path).await;
    let environment = FixedEnvironment::new();
    let port = MessagingStore::open_with(&path, environment.clone())
        .await
        .unwrap()
        .channel_lifecycle_mutation();

    let admitted = port
        .admit(&user("admin"), &conversation("c"))
        .await
        .unwrap()
        .unwrap();
    assert!(admitted.bind(&user("admin"), &conversation("c")).is_ok());
    assert_eq!(
        port.admit(&user("admin"), &conversation("missing"))
            .await
            .unwrap(),
        Err(ChannelMutationAdmissionRefusal::NotFound)
    );
    assert_eq!(
        port.admit(&user("member"), &conversation("c"))
            .await
            .unwrap(),
        Err(ChannelMutationAdmissionRefusal::Forbidden)
    );
    assert_eq!(
        port.admit(&user("admin"), &conversation("pair"))
            .await
            .unwrap(),
        Err(ChannelMutationAdmissionRefusal::Forbidden)
    );
    assert_eq!(
        port.set_archived(user("admin"), conversation("missing"), true)
            .await
            .unwrap(),
        Err(ChannelArchiveRefusal::NotFound)
    );
    assert_eq!(
        port.set_visibility(user("member"), conversation("c"), ChannelVisibility::Public,)
            .await
            .unwrap(),
        Err(ChannelVisibilityRefusal::Forbidden)
    );
    assert_eq!(
        port.rename(user("admin"), conversation("pair"), title("Other"))
            .await
            .unwrap(),
        Err(ChannelRenameRefusal::Forbidden)
    );
    assert_eq!(environment.clock_calls.load(Ordering::SeqCst), 0);
    assert_eq!(environment.entropy_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn every_operation_reserves_the_writer_before_its_authority_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("channel-lifecycle-mutation-writer.sqlite");
    seed(&path).await;
    let environment = FixedEnvironment::new();
    let port = MessagingStore::open_with(&path, environment.clone())
        .await
        .unwrap()
        .channel_lifecycle_mutation();
    let mut writer = SqliteConnection::connect(path.to_str().unwrap())
        .await
        .unwrap();

    let archive = {
        writer.execute("BEGIN IMMEDIATE").await.unwrap();
        let port = port.clone();
        let pending = tokio::spawn(async move {
            port.set_archived(user("admin"), conversation("c"), true)
                .await
        });
        tokio::time::sleep(Duration::from_millis(75)).await;
        assert!(!pending.is_finished());
        writer
            .execute("UPDATE workspace_users SET role='member' WHERE workspace_id='w' AND user_id='admin'")
            .await
            .unwrap();
        writer.execute("COMMIT").await.unwrap();
        pending.await.unwrap().unwrap()
    };
    assert_eq!(archive, Err(ChannelArchiveRefusal::Forbidden));

    writer
        .execute(
            "UPDATE workspace_users SET role='admin' WHERE workspace_id='w' AND user_id='admin'",
        )
        .await
        .unwrap();
    let visibility = {
        writer.execute("BEGIN IMMEDIATE").await.unwrap();
        let port = port.clone();
        let pending = tokio::spawn(async move {
            port.set_visibility(user("admin"), conversation("c"), ChannelVisibility::Private)
                .await
        });
        tokio::time::sleep(Duration::from_millis(75)).await;
        assert!(!pending.is_finished());
        writer
            .execute("UPDATE workspace_users SET role='member' WHERE workspace_id='w' AND user_id='admin'")
            .await
            .unwrap();
        writer.execute("COMMIT").await.unwrap();
        pending.await.unwrap().unwrap()
    };
    assert_eq!(visibility, Err(ChannelVisibilityRefusal::Forbidden));

    writer
        .execute(
            "UPDATE workspace_users SET role='admin' WHERE workspace_id='w' AND user_id='admin'",
        )
        .await
        .unwrap();
    let rename = {
        writer.execute("BEGIN IMMEDIATE").await.unwrap();
        let port = port.clone();
        let pending = tokio::spawn(async move {
            port.rename(user("admin"), conversation("c"), title("General"))
                .await
        });
        tokio::time::sleep(Duration::from_millis(75)).await;
        assert!(!pending.is_finished());
        writer
            .execute("UPDATE workspace_users SET role='member' WHERE workspace_id='w' AND user_id='admin'")
            .await
            .unwrap();
        writer.execute("COMMIT").await.unwrap();
        pending.await.unwrap().unwrap()
    };
    assert_eq!(rename, Err(ChannelRenameRefusal::Forbidden));
    assert_eq!(environment.clock_calls.load(Ordering::SeqCst), 0);
    assert_eq!(environment.entropy_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn archive_writes_every_request_and_strict_readback_rolls_back_drift() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("channel-lifecycle-archive.sqlite");
    seed(&path).await;
    let environment = FixedEnvironment::new();
    let port = MessagingStore::open_with(&path, environment.clone())
        .await
        .unwrap()
        .channel_lifecycle_mutation();

    for expected_updated in [77_i64, 78] {
        let fact = port
            .set_archived(user("owner"), conversation("c"), true)
            .await
            .unwrap()
            .unwrap()
            .bind(&user("owner"), &conversation("c"), true)
            .unwrap();
        assert!(fact.archived());
        let mut db = SqliteConnection::connect(path.to_str().unwrap())
            .await
            .unwrap();
        let row = sqlx::query("SELECT archived_at,updated_at FROM conversations WHERE id='c'")
            .fetch_one(&mut db)
            .await
            .unwrap();
        assert_eq!(row.get::<i64, _>("archived_at"), expected_updated);
        assert_eq!(row.get::<i64, _>("updated_at"), expected_updated);
        db.close().await.unwrap();
    }

    let mut db = SqliteConnection::connect(path.to_str().unwrap())
        .await
        .unwrap();
    db.execute(
        "CREATE TRIGGER corrupt_archive AFTER UPDATE OF archived_at ON conversations \
         WHEN NEW.id='c' BEGIN UPDATE conversations SET title_key='wrong' WHERE id=NEW.id; END",
    )
    .await
    .unwrap();
    let error = port
        .set_archived(user("admin"), conversation("c"), false)
        .await
        .unwrap_err();
    assert!(error_chain(&error).contains("stored workspace title/key pair is not canonical"));
    assert!(matches!(
        mutation_source(&error),
        ChannelLifecycleMutationStoreError::Row(_)
    ));
    let row =
        sqlx::query("SELECT archived_at,title_key,updated_at FROM conversations WHERE id='c'")
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(row.get::<i64, _>("archived_at"), 78);
    assert_eq!(row.get::<String, _>("title_key"), "general");
    assert_eq!(row.get::<i64, _>("updated_at"), 78);
    db.execute("DROP TRIGGER corrupt_archive").await.unwrap();
    assert!(
        port.set_archived(user("admin"), conversation("c"), false)
            .await
            .unwrap()
            .is_ok()
    );
}

#[tokio::test]
async fn visibility_seals_noop_changed_and_fixed_roster_refusal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("channel-lifecycle-visibility.sqlite");
    seed(&path).await;
    let environment = FixedEnvironment::new();
    let port = MessagingStore::open_with(&path, environment.clone())
        .await
        .unwrap()
        .channel_lifecycle_mutation();

    let no_op = port
        .set_visibility(user("admin"), conversation("c"), ChannelVisibility::Private)
        .await
        .unwrap()
        .unwrap()
        .bind(
            &user("admin"),
            &conversation("c"),
            ChannelVisibility::Private,
        )
        .unwrap();
    assert!(!no_op.changed());
    assert_eq!(environment.clock_calls.load(Ordering::SeqCst), 0);

    let changed = port
        .set_visibility(user("admin"), conversation("c"), ChannelVisibility::Public)
        .await
        .unwrap()
        .unwrap()
        .bind(
            &user("admin"),
            &conversation("c"),
            ChannelVisibility::Public,
        )
        .unwrap();
    assert!(changed.changed());
    assert_eq!(changed.visibility(), ChannelVisibility::Public);
    assert_eq!(environment.clock_calls.load(Ordering::SeqCst), 1);

    assert_eq!(
        port.set_visibility(
            user("admin"),
            conversation("fixed"),
            ChannelVisibility::Private,
        )
        .await
        .unwrap(),
        Err(ChannelVisibilityRefusal::NotFound)
    );
}

#[tokio::test]
async fn rename_commits_exact_notice_recipients_and_rolls_back_corrupt_readback() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("channel-lifecycle-rename.sqlite");
    seed(&path).await;
    let environment = FixedEnvironment::new();
    let port = MessagingStore::open_with(&path, environment.clone())
        .await
        .unwrap()
        .channel_lifecycle_mutation();

    let no_op = port
        .rename(user("admin"), conversation("c"), title("General"))
        .await
        .unwrap()
        .unwrap()
        .bind(&user("admin"), &conversation("c"), &title("General"))
        .unwrap();
    assert!(!no_op.changed());
    assert_eq!(environment.clock_calls.load(Ordering::SeqCst), 0);
    assert_eq!(environment.entropy_calls.load(Ordering::SeqCst), 0);

    assert_eq!(
        port.rename(user("admin"), conversation("c"), title("Roadmap"))
            .await
            .unwrap(),
        Err(ChannelRenameRefusal::TitleTaken)
    );
    assert_eq!(environment.clock_calls.load(Ordering::SeqCst), 0);

    let renamed = port
        .rename(user("admin"), conversation("c"), title("general"))
        .await
        .unwrap()
        .unwrap()
        .bind(&user("admin"), &conversation("c"), &title("general"))
        .unwrap();
    assert!(renamed.changed());
    assert_eq!(renamed.old().unwrap().title(), "General");
    assert_eq!(
        renamed
            .recipient_rows()
            .map(|value| value.as_str())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
    let notice = renamed.notice().unwrap();
    assert_eq!(notice.seq().get(), 1);
    assert_eq!(notice.created_at(), 77);

    let mut db = SqliteConnection::connect(path.to_str().unwrap())
        .await
        .unwrap();
    let message =
        sqlx::query("SELECT from_kind,content,metadata,seq,created_at FROM messages WHERE id=?")
            .bind(notice.message().as_str())
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(message.get::<String, _>("from_kind"), "system");
    assert_eq!(message.get::<String, _>("content"), notice.content());
    assert_eq!(message.get::<i64, _>("seq"), 1);
    assert_eq!(message.get::<i64, _>("created_at"), 77);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT latest_seq FROM conversation_stream_heads \
             WHERE conversation_id='c' AND thread_root_id IS NULL",
        )
        .fetch_one(&mut db)
        .await
        .unwrap(),
        notice.seq().get(),
        "the committed rename notice advances the main read-model head"
    );
    let metadata: serde_json::Value =
        serde_json::from_str(&message.get::<String, _>("metadata")).unwrap();
    assert_eq!(
        metadata,
        serde_json::json!({
            "subtype":"rename",
            "old_name":"General",
            "new_name":"general",
            "scope":"conversation"
        })
    );

    db.execute(
        "CREATE TRIGGER corrupt_rename_notice AFTER INSERT ON messages \
         BEGIN UPDATE messages SET content='corrupt' WHERE id=NEW.id; END",
    )
    .await
    .unwrap();
    let error = port
        .rename(user("admin"), conversation("c"), title("Delivery"))
        .await
        .unwrap_err();
    assert!(error_chain(&error).contains("messages:rename_notice"));
    let row =
        sqlx::query("SELECT title,title_key,latest_message_seq FROM conversations WHERE id='c'")
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(row.get::<String, _>("title"), "general");
    assert_eq!(row.get::<String, _>("title_key"), "general");
    assert_eq!(row.get::<i64, _>("latest_message_seq"), 1);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT latest_seq FROM conversation_stream_heads \
             WHERE conversation_id='c' AND thread_root_id IS NULL",
        )
        .fetch_one(&mut db)
        .await
        .unwrap(),
        1,
        "a rolled-back rename notice leaves the prior stream head unchanged"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM messages WHERE conversation_id='c'")
            .fetch_one(&mut db)
            .await
            .unwrap(),
        1
    );
    db.execute("DROP TRIGGER corrupt_rename_notice")
        .await
        .unwrap();
    assert!(
        port.rename(user("admin"), conversation("c"), title("Delivery"))
            .await
            .unwrap()
            .is_ok()
    );
}

#[tokio::test]
async fn corrupt_selected_rename_recipient_rolls_back_the_whole_unit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("channel-lifecycle-rename-recipient.sqlite");
    seed(&path).await;
    let port = MessagingStore::open_with(&path, FixedEnvironment::new())
        .await
        .unwrap()
        .channel_lifecycle_mutation();
    let mut db = SqliteConnection::connect(path.to_str().unwrap())
        .await
        .unwrap();
    db.execute("PRAGMA foreign_keys=OFF").await.unwrap();
    db.execute(
        "INSERT INTO conversation_agents(conversation_id,agent_id,joined_at) VALUES('c','',3)",
    )
    .await
    .unwrap();

    let error = port
        .rename(user("admin"), conversation("c"), title("Delivery"))
        .await
        .unwrap_err();
    assert!(error_chain(&error).contains("conversation_agents.agent_id"));
    let row =
        sqlx::query("SELECT title,title_key,latest_message_seq FROM conversations WHERE id='c'")
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(row.get::<String, _>("title"), "General");
    assert_eq!(row.get::<String, _>("title_key"), "general");
    assert_eq!(row.get::<i64, _>("latest_message_seq"), 0);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM messages WHERE conversation_id='c'")
            .fetch_one(&mut db)
            .await
            .unwrap(),
        0
    );
    db.execute("DELETE FROM conversation_agents WHERE conversation_id='c' AND agent_id='' ")
        .await
        .unwrap();
    assert!(
        port.rename(user("admin"), conversation("c"), title("Delivery"))
            .await
            .unwrap()
            .is_ok()
    );
}
