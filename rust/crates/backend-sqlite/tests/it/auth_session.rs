use std::{error::Error, time::Duration};

use silverhand_backend_core::{
    foundation::identity::UserId,
    identity_access::auth::{HumanSessionAuthenticator, SessionBearer, SessionClock, SessionTtl},
};
use silverhand_backend_sqlite::identity_access::auth::SqliteSessionStore;
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use tempfile::TempDir;

use crate::current_schema;

const NOW: i64 = 1_000_000;
const TTL: i64 = 30 * 24 * 60 * 60 * 1_000;
const VALID_HASH: &str = "OXoqnFv14szsOMJZa2grsb0F_m5OzqbBDPQnVf8iVAM";
const REVOKED_HASH: &str = "oGgRJHJu_7FtXTjQVIJO4CegvaRq_JWjptU6zmpNusk";
const EXPIRED_HASH: &str = "tSs-8iM4WM4RVthfI1zyxB7d-oyh7tySQ5i5rx2zA8s";
const BOUNDARY_HASH: &str = "rAzQshJaBPdCX82x4n3GQPepXUl8FxwXubjtlRZSBX0";
const EMPTY_USER_HASH: &str = "TIPzKVFB8ZoQg-Fcla9AwGIxD5Upr5yHyKr6sSVFStE";
const DANGLING_HASH: &str = "VYguZpuzxBLIPi-HTFzK0B_Jgr8VjbxjrAoTTTxnLFQ";

#[derive(Clone)]
struct FixedClock;

impl SessionClock for FixedClock {
    fn now_unix_millis(&self) -> i64 {
        NOW
    }
}

async fn fixture() -> (TempDir, std::path::PathBuf, SqliteSessionStore) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite");
    let mut connection = current_schema::create_current_schema(&path).await;
    sqlx::query(
        "INSERT INTO users (id, name, display_name, created_at, updated_at)
         VALUES ('user-1', 'user-1', 'User One', 1, 1)",
    )
    .execute(&mut connection)
    .await
    .unwrap();
    connection.close().await.unwrap();
    let store = SqliteSessionStore::open(&path).await.unwrap();
    (directory, path, store)
}

async fn insert_session(
    path: &std::path::Path,
    id: &str,
    user_id: &str,
    hash: &str,
    last_used_at: i64,
    revoked_at: Option<i64>,
) {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .foreign_keys(true);
    let mut connection = SqliteConnection::connect_with(&options).await.unwrap();
    sqlx::query(
        "INSERT INTO auth_sessions
            (id, user_id, token_hash, created_at, last_used_at, revoked_at,
             user_agent, ip_first_seen)
         VALUES (?, ?, ?, 1, ?, ?, NULL, NULL)",
    )
    .bind(id)
    .bind(user_id)
    .bind(hash)
    .bind(last_used_at)
    .bind(revoked_at)
    .execute(&mut connection)
    .await
    .unwrap();
    connection.close().await.unwrap();
}

fn authenticator(
    store: SqliteSessionStore,
) -> HumanSessionAuthenticator<SqliteSessionStore, FixedClock> {
    HumanSessionAuthenticator::new(store, FixedClock, SessionTtl::from_millis(TTL).unwrap())
}

async fn last_used_at(path: &std::path::Path, id: &str) -> i64 {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .foreign_keys(true);
    let mut connection = SqliteConnection::connect_with(&options).await.unwrap();
    let value = sqlx::query_scalar("SELECT last_used_at FROM auth_sessions WHERE id = ?")
        .bind(id)
        .fetch_one(&mut connection)
        .await
        .unwrap();
    connection.close().await.unwrap();
    value
}

#[tokio::test]
async fn valid_session_returns_its_typed_identity_and_commits_renewal() {
    let (_directory, path, store) = fixture().await;
    insert_session(&path, "valid", "user-1", VALID_HASH, 9_500, None).await;

    let user = authenticator(store)
        .authenticate(SessionBearer::parse("valid-token").unwrap())
        .await
        .unwrap()
        .unwrap();

    assert_eq!(user, UserId::parse("user-1").unwrap());
    assert_eq!(last_used_at(&path, "valid").await, NOW);
}

#[tokio::test]
async fn unknown_revoked_and_expired_sessions_share_one_refusal_and_are_not_touched() {
    let (_directory, path, store) = fixture().await;
    insert_session(&path, "revoked", "user-1", REVOKED_HASH, 9_500, Some(9_900)).await;
    insert_session(
        &path,
        "expired",
        "user-1",
        EXPIRED_HASH,
        NOW - TTL - 1,
        None,
    )
    .await;
    let authenticator = authenticator(store);

    for token in ["unknown-token", "revoked-token", "expired-token"] {
        assert!(
            authenticator
                .authenticate(SessionBearer::parse(token).unwrap())
                .await
                .unwrap()
                .is_none(),
            "{token} escaped the uniform refusal"
        );
    }
    assert_eq!(last_used_at(&path, "revoked").await, 9_500);
    assert_eq!(last_used_at(&path, "expired").await, NOW - TTL - 1);
}

#[tokio::test]
async fn exact_sliding_ttl_boundary_is_still_eligible() {
    let (_directory, path, store) = fixture().await;
    insert_session(&path, "boundary", "user-1", BOUNDARY_HASH, NOW - TTL, None).await;

    let result = authenticator(store)
        .authenticate(SessionBearer::parse("boundary-token").unwrap())
        .await
        .unwrap();

    assert_eq!(result.unwrap().as_str(), "user-1");
    assert_eq!(last_used_at(&path, "boundary").await, NOW);
}

#[tokio::test]
async fn due_renewal_rechecks_a_revoke_committed_while_waiting_for_writer() {
    let (_directory, path, store) = fixture().await;
    insert_session(&path, "valid", "user-1", VALID_HASH, 9_500, None).await;
    let options = SqliteConnectOptions::new()
        .filename(&path)
        .foreign_keys(true);
    let mut connection = SqliteConnection::connect_with(&options).await.unwrap();
    let mut revoke = sqlx::Connection::begin_with(&mut connection, "BEGIN IMMEDIATE")
        .await
        .unwrap();
    sqlx::query("UPDATE auth_sessions SET revoked_at = 9999 WHERE id = 'valid'")
        .execute(&mut *revoke)
        .await
        .unwrap();

    let mut validation = tokio::spawn(async move {
        authenticator(store)
            .authenticate(SessionBearer::parse("valid-token").unwrap())
            .await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(25), &mut validation)
            .await
            .is_err(),
        "validation escaped while revoke still owned the writer"
    );
    revoke.commit().await.unwrap();

    assert!(validation.await.unwrap().unwrap().is_none());
    assert_eq!(last_used_at(&path, "valid").await, 9_500);
}

#[tokio::test]
async fn invalid_typed_user_fails_before_renewal_and_preserves_the_decode_cause() {
    let (_directory, path, store) = fixture().await;
    let options = SqliteConnectOptions::new()
        .filename(&path)
        .foreign_keys(true);
    let mut connection = SqliteConnection::connect_with(&options).await.unwrap();
    sqlx::query(
        "INSERT INTO users (id, name, display_name, created_at, updated_at)
         VALUES ('', 'empty-id-user', 'Empty Id', 1, 1)",
    )
    .execute(&mut connection)
    .await
    .unwrap();
    connection.close().await.unwrap();
    insert_session(&path, "empty-user", "", EMPTY_USER_HASH, 9_500, None).await;

    let failure = authenticator(store)
        .authenticate(SessionBearer::parse("empty-user-token").unwrap())
        .await
        .unwrap_err();

    assert!(error_chain(&failure).contains("persisted session user id is invalid"));
    assert!(error_chain(&failure).contains("user id must not be empty"));
    assert_eq!(last_used_at(&path, "empty-user").await, 9_500);
}

#[tokio::test]
async fn dangling_user_is_an_integrity_failure_before_a_principal_can_escape() {
    let (_directory, path, store) = fixture().await;
    let options = SqliteConnectOptions::new()
        .filename(&path)
        .foreign_keys(false);
    let mut connection = SqliteConnection::connect_with(&options).await.unwrap();
    sqlx::query(
        "INSERT INTO auth_sessions
            (id, user_id, token_hash, created_at, last_used_at, revoked_at,
             user_agent, ip_first_seen)
         VALUES ('dangling', 'missing-user', ?, 1, 9500, NULL, NULL, NULL)",
    )
    .bind(DANGLING_HASH)
    .execute(&mut connection)
    .await
    .unwrap();
    connection.close().await.unwrap();

    let failure = authenticator(store)
        .authenticate(SessionBearer::parse("dangling-token").unwrap())
        .await
        .unwrap_err();

    assert!(error_chain(&failure).contains("authenticated session references a missing user"));
    assert_eq!(last_used_at(&path, "dangling").await, 9_500);
}

#[tokio::test]
async fn real_driver_failure_is_returned_as_the_persistence_source() {
    let (_directory, path, store) = fixture().await;
    let options = SqliteConnectOptions::new()
        .filename(&path)
        .foreign_keys(true);
    let mut connection = SqliteConnection::connect_with(&options).await.unwrap();
    sqlx::query("DROP TABLE auth_sessions")
        .execute(&mut connection)
        .await
        .unwrap();
    connection.close().await.unwrap();

    let failure = authenticator(store)
        .authenticate(SessionBearer::parse("valid-token").unwrap())
        .await
        .unwrap_err();

    assert!(error_chain(&failure).contains("no such table: auth_sessions"));
}

fn error_chain(error: &(dyn Error + 'static)) -> String {
    let mut messages = vec![error.to_string()];
    let mut current = error.source();
    while let Some(cause) = current {
        messages.push(cause.to_string());
        current = cause.source();
    }
    messages.join(": ")
}

#[tokio::test]
async fn transport_recheck_observes_revocation_without_extending_idle_expiry() {
    use silverhand_backend_core::identity_access::auth::{SessionRevoke, SessionTokenHash};
    let (_directory, path, store) = fixture().await;
    insert_session(&path, "socket", "user-1", VALID_HASH, NOW - 10, None).await;
    let auth = authenticator(store.clone());
    let bearer = SessionBearer::parse("valid-token").unwrap();
    assert_eq!(
        bearer.reauthenticate(&auth).await.unwrap(),
        Some(UserId::parse("user-1").unwrap())
    );
    assert_eq!(last_used_at(&path, "socket").await, NOW - 10);
    store
        .revoke(SessionTokenHash::of_plaintext("valid-token"), NOW)
        .await
        .unwrap();
    assert_eq!(bearer.reauthenticate(&auth).await.unwrap(), None);
    assert_eq!(last_used_at(&path, "socket").await, NOW - 10);
}

#[tokio::test]
async fn recent_authentication_passes_while_writer_holds_uncommitted_revoke() {
    let (_directory, path, store) = fixture().await;
    insert_session(&path, "valid", "user-1", VALID_HASH, NOW - 299_999, None).await;
    let mut connection =
        SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
            .await
            .unwrap();
    let mut writer = connection.begin_with("BEGIN IMMEDIATE").await.unwrap();
    sqlx::query("UPDATE auth_sessions SET revoked_at = ? WHERE id = 'valid'")
        .bind(NOW)
        .execute(&mut *writer)
        .await
        .unwrap();

    let auth = authenticator(store);
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        auth.authenticate(SessionBearer::parse("valid-token").unwrap()),
    )
    .await
    .expect("recent authentication must not wait for writer")
    .unwrap();
    assert!(
        result.is_some(),
        "uncommitted revocation is outside the read snapshot"
    );
    writer.commit().await.unwrap();

    assert!(
        auth.authenticate(SessionBearer::parse("valid-token").unwrap())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(last_used_at(&path, "valid").await, NOW - 299_999);
}

#[tokio::test]
async fn concurrent_threshold_renewals_write_once_and_never_move_time_backwards() {
    let (_directory, path, store) = fixture().await;
    insert_session(&path, "valid", "user-1", VALID_HASH, NOW - 300_000, None).await;
    let mut connection =
        SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
            .await
            .unwrap();
    sqlx::raw_sql(
        "CREATE TABLE touches (n INTEGER); INSERT INTO touches VALUES(0);
        CREATE TRIGGER count_touches AFTER UPDATE OF last_used_at ON auth_sessions
        BEGIN UPDATE touches SET n=n+1; END;",
    )
    .execute(&mut connection)
    .await
    .unwrap();

    let mut attempts = tokio::task::JoinSet::new();
    for _ in 0..16 {
        let auth = authenticator(store.clone());
        attempts.spawn(async move {
            auth.authenticate(SessionBearer::parse("valid-token").unwrap())
                .await
        });
    }
    while let Some(result) = attempts.join_next().await {
        assert!(result.unwrap().unwrap().is_some());
    }
    assert_eq!(last_used_at(&path, "valid").await, NOW);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT n FROM touches")
            .fetch_one(&mut connection)
            .await
            .unwrap(),
        1
    );

    // Simulate a later request committing first, then an older clock arriving.
    sqlx::query("UPDATE auth_sessions SET last_used_at=? WHERE id='valid'")
        .bind(NOW + 50)
        .execute(&mut connection)
        .await
        .unwrap();
    assert!(
        authenticator(store)
            .authenticate(SessionBearer::parse("valid-token").unwrap())
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(last_used_at(&path, "valid").await, NOW + 50);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT n FROM touches")
            .fetch_one(&mut connection)
            .await
            .unwrap(),
        2
    );
}

#[tokio::test]
async fn due_renewal_does_not_revive_expiry_changed_while_waiting() {
    let (_directory, path, store) = fixture().await;
    insert_session(&path, "valid", "user-1", VALID_HASH, NOW - 300_000, None).await;
    let mut connection =
        SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
            .await
            .unwrap();
    let mut writer = connection.begin_with("BEGIN IMMEDIATE").await.unwrap();
    sqlx::query("UPDATE auth_sessions SET last_used_at=? WHERE id='valid'")
        .bind(NOW - TTL - 1)
        .execute(&mut *writer)
        .await
        .unwrap();

    let mut attempt = tokio::spawn(async move {
        authenticator(store)
            .authenticate(SessionBearer::parse("valid-token").unwrap())
            .await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(25), &mut attempt)
            .await
            .is_err()
    );
    writer.commit().await.unwrap();

    assert!(attempt.await.unwrap().unwrap().is_none());
    assert_eq!(last_used_at(&path, "valid").await, NOW - TTL - 1);
}
