//! The actual onboarding/live/sealed ports must wait before reading mutable witnesses.
use silverhand_backend_core::{
    application_orchestration::agent_creation::OnboardingInboxEnsure,
    conversation_messaging::messaging::{
        MessageId,
        post_commit::{InboxEnsureOutcome, LiveMessageInboxEnsure, SealedMessageInboxEnsure},
    },
    foundation::identity::{AgentId, ConversationId},
};
use silverhand_backend_sqlite::conversation_messaging::messaging::MessagingStore;
use sqlx::{
    Connection, Executor, SqliteConnection,
    sqlite::{SqliteConnectOptions, SqliteJournalMode},
};
use std::time::Duration;

#[tokio::test]
async fn all_inbox_ports_wait_then_validate_fresh_witnesses_and_remain_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let options = SqliteConnectOptions::new()
        .filename(dir.path().join("inbox.sqlite"))
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);
    let mut db = SqliteConnection::connect_with(&options).await.unwrap();
    db.execute("CREATE TABLE conversations(id TEXT PRIMARY KEY,placement_kind TEXT,endpoint_a_kind TEXT,endpoint_a_id TEXT,endpoint_b_kind TEXT,endpoint_b_id TEXT,archived_at INTEGER);
      CREATE TABLE conversation_agents(conversation_id TEXT,agent_id TEXT);
      CREATE TABLE messages(id TEXT PRIMARY KEY,conversation_id TEXT,from_kind TEXT,metadata TEXT);
      CREATE TABLE conversation_inboxes(conversation_id TEXT,agent_id TEXT,message_id TEXT,delivered_at INTEGER,delivery_kind TEXT,created_at INTEGER,UNIQUE(conversation_id,agent_id,message_id));
      INSERT INTO conversations VALUES('c','workspace',NULL,NULL,NULL,NULL,NULL);
      INSERT INTO conversation_agents VALUES('c','a');
      INSERT INTO messages VALUES('m','c','system','{\"subtype\":\"onboarding\"}');").await.unwrap();
    let store = MessagingStore::open(dir.path().join("inbox.sqlite"))
        .await
        .unwrap();
    let c = ConversationId::parse("c").unwrap();
    let a = AgentId::parse("a").unwrap();
    let m = MessageId::parse("m").unwrap();
    for kind in 0..3 {
        for revoke in [false, true] {
            db.execute("DELETE FROM conversation_inboxes; DELETE FROM conversation_agents; INSERT INTO conversation_agents VALUES('c','a');").await.unwrap();
            let mut held = db.begin_with("BEGIN IMMEDIATE").await.unwrap();
            if revoke {
                sqlx::query("DELETE FROM conversation_agents")
                    .execute(&mut *held)
                    .await
                    .unwrap();
            }
            let attempt = async {
                match kind {
                    0 => store
                        .onboarding_inbox_ensure()
                        .ensure(&c, &a, &m)
                        .await
                        .map(|_| ()),
                    1 => store
                        .committed_message_inbox_ensure()
                        .ensure_for_live_recipient(&c, &a, &m)
                        .await
                        .map(|_| ()),
                    _ => store
                        .committed_message_inbox_ensure()
                        .ensure_for_sealed_recipient(&c, &a, &m)
                        .await
                        .map(|_| ()),
                }
            };
            let release = async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                held.commit().await.unwrap();
            };
            let (result, ()) = tokio::join!(attempt, release);
            let allowed = !revoke || kind == 2;
            assert_eq!(
                result.is_ok(),
                allowed,
                "port={kind}, revoke={revoke}: {result:?}"
            );
            let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM conversation_inboxes")
                .fetch_one(&mut db)
                .await
                .unwrap();
            assert_eq!(count, i64::from(allowed));
            if allowed {
                store
                    .committed_message_inbox_ensure()
                    .ensure_for_sealed_recipient(&c, &a, &m)
                    .await
                    .unwrap();
                let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM conversation_inboxes")
                    .fetch_one(&mut db)
                    .await
                    .unwrap();
                assert_eq!(count, 1);
            }
        }
    }
    // Archive owns the writer first. All three automatic doors must inspect its
    // committed post-image, not an earlier live snapshot, and enqueue nothing.
    for kind in 0..3 {
        db.execute("DELETE FROM conversation_inboxes; UPDATE conversations SET archived_at = NULL; DELETE FROM conversation_agents; INSERT INTO conversation_agents VALUES('c','a');").await.unwrap();
        let mut held = db.begin_with("BEGIN IMMEDIATE").await.unwrap();
        sqlx::query("UPDATE conversations SET archived_at = 100")
            .execute(&mut *held)
            .await
            .unwrap();
        let ensure = store.committed_message_inbox_ensure();
        let attempt = async {
            match kind {
                0 => store.onboarding_inbox_ensure().ensure(&c, &a, &m).await,
                1 => ensure.ensure_for_live_recipient(&c, &a, &m).await,
                _ => ensure.ensure_for_sealed_recipient(&c, &a, &m).await,
            }
        };
        let release = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            held.commit().await.unwrap();
        };
        let (result, ()) = tokio::join!(attempt, release);
        assert_eq!(result.unwrap(), InboxEnsureOutcome::SkippedArchived);
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM conversation_inboxes")
            .fetch_one(&mut db)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    db.close().await.unwrap();
}
