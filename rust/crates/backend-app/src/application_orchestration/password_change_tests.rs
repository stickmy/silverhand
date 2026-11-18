use super::*;
use silverhand_backend_core::identity_access::{
    auth::{SessionAudit, SessionIssue, SessionTokenHash},
    credential::{PasswordChangeStore, PasswordHasher},
};
use silverhand_backend_core::{foundation::identity::UserId, identity_access::auth::SessionClock};
use silverhand_backend_sqlite::identity_access::credential::{
    SqliteCredentialStore, serialize_password_hash_params,
};

async fn password_request(
    router: &Router,
    token: Option<&str>,
    body: serde_json::Value,
) -> (StatusCode, String) {
    json_request_with_body(
        router,
        "POST",
        "/api/me/password",
        token,
        Body::from(body.to_string()),
    )
    .await
}

async fn assert_password_admission(
    router: &Router,
    pool: &sqlx::SqlitePool,
    bearer: &str,
    agent_bearer: &str,
    input: &serde_json::Value,
    old_phc: &str,
) {
    for token in [None, Some("Bearer host-token-cp2"), Some(agent_bearer)] {
        assert_eq!(
            password_request(router, token, input.clone()).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    let (_, password_status) = request(
        router,
        "GET",
        "/api/me/password",
        Some("Bearer other-token"),
    )
    .await;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&password_status).unwrap(),
        serde_json::json!({"has_password":false})
    );
    assert_eq!(
        password_request(router, Some("Bearer other-token"), input.clone())
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let wrong = password_request(
        router,
        Some(bearer),
        serde_json::json!({"current_password":"wrong","new_password":"new-password-2542"}),
    )
    .await;
    assert_eq!(wrong.0, StatusCode::FORBIDDEN);
    let stored: String = sqlx::query_scalar(
        "SELECT password_hash FROM user_password_credentials WHERE user_id='user-1'",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(stored, old_phc);
    assert_eq!(
        request(router, "GET", "/api/me", Some("Bearer second-token"))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        password_request(
            router,
            Some(bearer),
            serde_json::json!({"current_password":"old-password-2542","new_password":"short"})
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
}

async fn assert_login_transition_and_publish(router: &Router) {
    for (password, expected) in [
        ("old-password-2542", StatusCode::UNAUTHORIZED),
        ("new-password-2542", StatusCode::OK),
    ] {
        let (status, body) = json_request_with_body(
            router,
            "POST",
            "/auth/login",
            None,
            Body::from(
                serde_json::json!({"email":"password2542@example.com","password":password})
                    .to_string(),
            ),
        )
        .await;
        assert_eq!(status, expected, "{body}");
        if status == StatusCode::OK {
            let value: serde_json::Value = serde_json::from_str(&body).unwrap();
            let fresh = format!("Bearer {}", value["user_token"].as_str().unwrap());
            // Produces a real viewer event; the old subscribed socket must close,
            // not receive that event, even without reconnecting or client input.
            let notified = json_request_with_body(
                router,
                "POST",
                "/api/notifications/read",
                Some(&fresh),
                Body::from(r#"{"all":true}"#),
            )
            .await;
            assert_eq!(notified.0, StatusCode::OK, "{}", notified.1);
        }
    }
}

#[tokio::test]
async fn password_change_revokes_sessions_and_connected_socket_without_touching_hosts() {
    let fixture = fixture(true).await;
    seed_human(&fixture.path).await;
    seed_runtime_agent(&fixture.path, "password-agent").await;
    seed_workspace_conversation(&fixture.path).await;
    seed_message(&fixture.path, "password-notification").await;

    let app = prepare(&remote_config(fixture.path.clone())).await.unwrap();
    let agent_bearer = mint_runtime_bearer(&app, "password-agent").await;
    let pool = SqlitePoolOptions::new()
        .connect(fixture.path.to_str().unwrap())
        .await
        .unwrap();
    sqlx::query("INSERT INTO notifications(id,kind,user_id,workspace_id,conversation_id,message_id,payload,created_at)
        VALUES('password-notification','mention_self','user-1',?,?,'password-notification','{}',10)")
        .bind(WORKSPACE_ID)
        .bind(CONVERSATION_ID)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES('password-other','password-other','Other',1,1)")
        .execute(&pool).await.unwrap();
    let hasher = SpawnBlockingHasher::new(Argon2idPasswordHasher::production(OsEntropy));
    let old = hasher.hash("old-password-2542").await.unwrap();
    sqlx::query("UPDATE users SET email='password2542@example.com' WHERE id='user-1'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO user_password_credentials(user_id,password_hash,password_hash_params,password_updated_at) VALUES('user-1',?,?,1)")
        .bind(&old.phc)
        .bind(serialize_password_hash_params(&old.params))
        .execute(&pool)
        .await
        .unwrap();
    let sessions = SqliteSessionStore::open(&fixture.path).await.unwrap();
    let user = UserId::parse("user-1").unwrap();
    let now = SystemSessionClock.now_unix_millis();
    sessions
        .issue(
            &user,
            SessionTokenHash::of_plaintext("second-token"),
            &SessionAudit::default(),
            now,
        )
        .await
        .unwrap();
    sessions
        .issue(
            &UserId::parse("password-other").unwrap(),
            SessionTokenHash::of_plaintext("other-token"),
            &SessionAudit::default(),
            now,
        )
        .await
        .unwrap();
    let router = app.router.clone();
    let bearer = format!("Bearer {TOKEN}");
    let input = serde_json::json!({"current_password":"old-password-2542","new_password":"new-password-2542"});

    assert_password_admission(&router, &pool, &bearer, &agent_bearer, &input, &old.phc).await;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let running = tokio::spawn(serve(listener, app, async move {
        let _ = shutdown_rx.await;
        Ok::<(), std::io::Error>(())
    }));
    let mut socket = subscribe_own_viewer(address).await;
    let hosts_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM host_credentials")
        .fetch_one(&pool)
        .await
        .unwrap();

    // Discarding the success body simulates a caller that cannot use its response.
    assert_eq!(
        password_request(&router, Some(&bearer), input.clone())
            .await
            .0,
        StatusCode::OK
    );
    for token in [&bearer, "Bearer second-token"] {
        assert_eq!(
            request(&router, "GET", "/api/me", Some(token)).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        password_request(&router, Some(&bearer), input).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM host_credentials")
            .fetch_one(&pool)
            .await
            .unwrap(),
        hosts_before
    );
    assert_eq!(
        request(&router, "GET", "/api/me", Some("Bearer other-token"))
            .await
            .0,
        StatusCode::OK
    );
    let host_revoked: Option<i64> = sqlx::query_scalar(
        "SELECT revoked_at FROM host_credentials WHERE id='cred-registry-probe'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(host_revoked, None);
    // A stale login that verified before replacement cannot mint a fresh session.
    assert!(
        !sessions
            .issue_verified(
                &user,
                &old.phc,
                SessionTokenHash::of_plaintext("racing-login"),
                &SessionAudit::default(),
                now
            )
            .await
            .unwrap()
    );

    assert_login_transition_and_publish(&router).await;
    let frame = tokio::time::timeout(std::time::Duration::from_secs(3), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let ClientMessage::Close(Some(close)) = frame else {
        panic!("expected session revocation close, got {frame:?}");
    };
    assert_eq!(u16::from(close.code), 1008);
    assert_eq!(close.reason, "session_expired");
    let _ = shutdown_tx.send(());
    running.await.unwrap().unwrap();
    pool.close().await;
}

#[tokio::test]
async fn concurrent_password_replacement_commits_once() {
    let fixture = fixture(true).await;
    seed_human(&fixture.path).await;
    let pool = SqlitePoolOptions::new()
        .connect(fixture.path.to_str().unwrap())
        .await
        .unwrap();
    let hasher = SpawnBlockingHasher::new(Argon2idPasswordHasher::production(OsEntropy));
    let old = hasher.hash("old-password").await.unwrap();
    sqlx::query("INSERT INTO user_password_credentials(user_id,password_hash,password_hash_params,password_updated_at) VALUES('user-1',?,?,1)")
        .bind(&old.phc)
        .bind(serialize_password_hash_params(&old.params))
        .execute(&pool)
        .await
        .unwrap();
    let store = SqliteCredentialStore::open(&fixture.path).await.unwrap();
    let user = UserId::parse("user-1").unwrap();
    let token = SessionTokenHash::of_plaintext(TOKEN);
    let first = hasher.hash("first-next-password").await.unwrap();
    let second = hasher.hash("second-next-password").await.unwrap();
    let (a, b) = tokio::join!(
        store.replace_password(&user, &token, &old.phc, first.clone(), 100),
        store.replace_password(&user, &token, &old.phc, second.clone(), 200)
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_ne!(a, b, "exactly one replacement must commit");
    let (winner, committed_at) = if a { (first, 100) } else { (second, 200) };
    let stored: (String, String, i64) = sqlx::query_as(
        "SELECT password_hash, password_hash_params, password_updated_at
         FROM user_password_credentials WHERE user_id='user-1'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        stored,
        (
            winner.phc,
            serialize_password_hash_params(&winner.params),
            committed_at
        )
    );
    let revoked_at: Option<i64> =
        sqlx::query_scalar("SELECT revoked_at FROM auth_sessions WHERE id='session-1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(revoked_at, Some(committed_at));
    pool.close().await;
}
