use super::*;
use silverhand_backend_core::{
    foundation::identity::UserId,
    identity_access::auth::{SessionAudit, SessionClock, SessionIssue, SessionTokenHash},
    identity_access::credential::PasswordHasher,
};
use silverhand_backend_sqlite::identity_access::credential::serialize_password_hash_params;

async fn json(
    router: &Router,
    method: &str,
    uri: &str,
    token: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let (status, text) = json_request_with_body(
        router,
        method,
        uri,
        Some(token),
        Body::from(body.to_string()),
    )
    .await;
    (
        status,
        serde_json::from_str(&text).unwrap_or(serde_json::Value::Null),
    )
}

#[tokio::test]
async fn account_deletion_requires_handover_then_keeps_shared_content() {
    let fixture = fixture(true).await;
    seed_human(&fixture.path).await;
    seed_workspace_conversation(&fixture.path).await;
    let pool = SqlitePoolOptions::new()
        .connect(fixture.path.to_str().unwrap())
        .await
        .unwrap();
    let hasher = SpawnBlockingHasher::new(Argon2idPasswordHasher::production(OsEntropy));
    let phc = hasher.hash("delete-me-2800").await.unwrap();
    sqlx::query("INSERT INTO user_password_credentials(user_id,password_hash,password_hash_params,password_updated_at) VALUES('user-1',?,?,1)")
        .bind(&phc.phc)
        .bind(serialize_password_hash_params(&phc.params))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(
        "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES('peer','peer','Peer',1,1); \
         INSERT INTO workspace_users(workspace_id,user_id,joined_at,role) VALUES('workspace0000001','peer',1,'member'); \
         INSERT INTO conversation_users(conversation_id,user_id,joined_at) VALUES('conv000000000001','peer',1);",
    )
    .execute(&pool)
    .await
    .unwrap();
    let app = prepare(&remote_config(fixture.path.clone())).await.unwrap();
    SqliteSessionStore::open(&fixture.path)
        .await
        .unwrap()
        .issue(
            &UserId::parse("peer").unwrap(),
            SessionTokenHash::of_plaintext("peer-token"),
            &SessionAudit::default(),
            SystemSessionClock.now_unix_millis(),
        )
        .await
        .unwrap();
    let router = app.router.clone();
    let me = format!("Bearer {TOKEN}");
    let peer = "Bearer peer-token";
    let (status, _) = json(
        &router,
        "POST",
        &format!("/api/channels/{CONVERSATION_ID}/messages"),
        &me,
        serde_json::json!({"content":"keep me"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, window) = json(
        &router,
        "GET",
        &format!("/api/channels/{CONVERSATION_ID}/messages"),
        peer,
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "pre {window}");
    let (status, preflight) = json(
        &router,
        "GET",
        "/api/me/deletion",
        &me,
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(preflight["blockers"]["workspaces"][0]["id"], WORKSPACE_ID);
    let input = serde_json::json!({"password":"delete-me-2800","handle":"viewer"});
    let (status, blocked) = json(&router, "DELETE", "/api/me", &me, input.clone()).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(blocked["code"], "account_deletion_blocked");

    let transfer = format!("/api/workspaces/{WORKSPACE_ID}/transfer-ownership");
    assert_eq!(
        json(
            &router,
            "POST",
            &transfer,
            peer,
            serde_json::json!({"user_id":"user-1"})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        json(
            &router,
            "POST",
            &transfer,
            &me,
            serde_json::json!({"user_id":"peer"})
        )
        .await
        .0,
        StatusCode::OK
    );

    for wrong in [
        serde_json::json!({"password":"nope","handle":"viewer"}),
        serde_json::json!({"password":"delete-me-2800","handle":"peer"}),
    ] {
        assert_eq!(
            json(&router, "DELETE", "/api/me", &me, wrong).await.0,
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        json(&router, "DELETE", "/api/me", &me, input.clone())
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        request(&router, "GET", "/api/me", Some(&me)).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        json(&router, "DELETE", "/api/me", &me, input).await.0,
        StatusCode::UNAUTHORIZED
    );

    let (status, window) = json(
        &router,
        "GET",
        &format!("/api/channels/{CONVERSATION_ID}/messages"),
        peer,
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{window}");
    let text = window.to_string();
    assert!(text.contains("keep me"), "{text}");
    let owner: String = sqlx::query_scalar("SELECT owner_user_id FROM workspaces WHERE id=?")
        .bind(WORKSPACE_ID)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(owner, "peer");
}
