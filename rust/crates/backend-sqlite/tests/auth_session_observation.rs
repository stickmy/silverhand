#[path = "support/current_schema_snapshot.rs"]
mod current_schema;
use silverhand_backend_core::identity_access::auth::{
    HumanSessionAuthenticator, SessionBearer, SessionClock, SessionTokenHash, SessionTtl,
};
use silverhand_backend_sqlite::identity_access::auth::SqliteSessionStore;
use sqlx::{Connection, sqlite::SqliteConnectOptions};
use std::time::Duration;
const NOW: i64 = 1_000_000;
#[derive(Clone)]
struct Clock;
impl SessionClock for Clock {
    fn now_unix_millis(&self) -> i64 {
        NOW
    }
}

#[derive(Clone, Default)]
struct AuthSpans(std::sync::Arc<std::sync::Mutex<Vec<&'static str>>>);

impl tracing::Subscriber for AuthSpans {
    fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
        Some(tracing::level_filters::LevelFilter::TRACE)
    }
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        let mut spans = self.0.lock().unwrap();
        spans.push(span.metadata().name());
        tracing::span::Id::from_u64(spans.len() as u64)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, _: &tracing::Event<'_>) {}
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

#[tokio::test]
async fn pool_wait_is_observed_before_the_select_timer_starts() {
    use tracing::instrument::WithSubscriber;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("observe.sqlite");
    let mut db = current_schema::create_current_schema(&path).await;
    sqlx::query("INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES('user-1','user-1','User',1,1)")
        .execute(&mut db).await.unwrap();
    sqlx::query("INSERT INTO auth_sessions(id,user_id,token_hash,created_at,last_used_at) VALUES('valid','user-1',?,1,?)")
        .bind(SessionTokenHash::of_plaintext("valid-token").as_str()).bind(NOW-10)
        .execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    let held = pool.acquire().await.unwrap();
    let auth = HumanSessionAuthenticator::new(
        SqliteSessionStore::from_pool(pool.clone()),
        Clock,
        SessionTtl::default(),
    );
    let spans = AuthSpans::default();
    let pending = auth
        .authenticate(SessionBearer::parse("valid-token").unwrap())
        .with_subscriber(spans.clone());
    tokio::pin!(pending);

    assert!(
        tokio::time::timeout(Duration::from_millis(25), &mut pending)
            .await
            .is_err()
    );
    {
        let observed = spans.0.lock().unwrap();
        assert!(observed.contains(&"db.pool.acquire"));
        assert!(
            !observed.contains(&"db.statement"),
            "SELECT timer includes pool wait"
        );
    }

    drop(held);
    assert!(pending.await.unwrap().is_some());
    {
        let observed = spans.0.lock().unwrap();
        assert_eq!(
            observed
                .iter()
                .filter(|name| **name == "db.statement")
                .count(),
            1,
            "observed spans: {observed:?}"
        );
        assert!(!observed.contains(&"db.transaction.begin"));
    }
    let last_used: i64 =
        sqlx::query_scalar("SELECT last_used_at FROM auth_sessions WHERE id='valid'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(last_used, NOW - 10);
    pool.close().await;
}
