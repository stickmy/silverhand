use super::*;

const BOUNDARY: &str = "XDESIGN2856";
const DESIGN: &str = r#"{"parts_version":3,"config":{"parts":{"hair":2,"hat":-1},"bg":1}}"#;

fn upload(design: Option<&str>, design_first: bool) -> Body {
    let mut file = Vec::new();
    file.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
    file.extend_from_slice(
        b"Content-Disposition: form-data; name=\"file\"; filename=\"a.png\"\r\n",
    );
    file.extend_from_slice(b"Content-Type: image/png\r\n\r\nPNGBYTES\r\n");
    let design = design
        .map(|json| {
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"design\"\r\n\r\n{json}\r\n"
            )
            .into_bytes()
        })
        .unwrap_or_default();
    let mut body = Vec::new();
    if design_first {
        body.extend(design);
        body.extend(file);
    } else {
        body.extend(file);
        body.extend(design);
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    Body::from(body)
}

async fn post_avatar(router: &Router, uri: &str, body: Body) -> (StatusCode, String) {
    post_avatar_as(router, uri, &format!("Bearer {TOKEN}"), body).await
}

async fn post_avatar_as(
    router: &Router,
    uri: &str,
    bearer: &str,
    body: Body,
) -> (StatusCode, String) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("authorization", bearer)
                .header(
                    "content-type",
                    format!("multipart/form-data; boundary={BOUNDARY}"),
                )
                .body(body)
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

async fn design(router: &Router, uri: &str) -> serde_json::Value {
    let (status, body) = request(router, "GET", uri, Some(&format!("Bearer {TOKEN}"))).await;
    assert_eq!(status, StatusCode::OK, "{uri}: {body}");
    serde_json::from_str(&body).unwrap()
}

async fn design_rows(pool: &sqlx::SqlitePool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM avatar_designs")
        .fetch_one(pool)
        .await
        .unwrap()
}

fn expected_design() -> serde_json::Value {
    serde_json::json!({"design":{"parts_version":3,"config":{"parts":{"hair":2,"hat":-1},"bg":1}}})
}

/// One principal's full lifecycle through the composed router: design upload,
/// replacement, manual upload, re-design, clear — the design read and the row
/// count follow each write.
async fn assert_design_lifecycle(
    router: &Router,
    pool: &sqlx::SqlitePool,
    avatar: &str,
    design_uri: &str,
) {
    assert_eq!(
        design(router, design_uri).await,
        serde_json::json!({"design":null})
    );

    let (status, body) = post_avatar(router, avatar, upload(Some(DESIGN), false)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(design(router, design_uri).await, expected_design());
    assert_eq!(design_rows(pool).await, 1);

    let (status, _) = post_avatar(router, avatar, upload(Some(DESIGN), true)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(design(router, design_uri).await, expected_design());
    assert_eq!(design_rows(pool).await, 1, "a replaced design is released");

    let (status, _) = post_avatar(router, avatar, upload(None, false)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        design(router, design_uri).await,
        serde_json::json!({"design":null})
    );
    assert_eq!(
        design_rows(pool).await,
        0,
        "a manual upload clears the design"
    );

    let (status, _) = post_avatar(router, avatar, upload(Some(DESIGN), false)).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = request(router, "DELETE", avatar, Some(&format!("Bearer {TOKEN}"))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        design(router, design_uri).await,
        serde_json::json!({"design":null})
    );
    assert_eq!(
        design_rows(pool).await,
        0,
        "removing the avatar clears the design"
    );

    for bad in [
        r#"{"parts_version":0,"config":{}}"#,
        r#"{"parts_version":1,"config":[]}"#,
        "not json",
    ] {
        let (status, body) = post_avatar(router, avatar, upload(Some(bad), false)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap(),
            serde_json::json!({"error":"Avatar design is invalid.","code":"avatar_design_invalid"})
        );
    }
    assert_eq!(
        design_rows(pool).await,
        0,
        "a refused design writes nothing"
    );
}

#[tokio::test]
async fn composed_avatar_design_lifecycle_for_users_and_agents() {
    let fixture = fixture(true).await;
    seed_human(&fixture.path).await;
    let pool = SqlitePoolOptions::new()
        .connect(fixture.path.to_str().unwrap())
        .await
        .unwrap();
    sqlx::query(
        concat!(
            "INSERT INTO hosts (id, name, kind, created_at, user_id) VALUES ('h-d', 'host-d', 'remote', 1, 'user-1'); ",
            "INSERT INTO users (id, name, display_name, created_at, updated_at) VALUES ('user-2', 'other', 'Other', 1, 1); ",
            seed_agent!(
                id = "a-d",
                name = "agent-d",
                runtime = "qoder",
                host_id = "h-d",
                created_by_user_id = "user-1",
            ),
            "; ",
            seed_agent!(
                id = "a-other",
                name = "agent-other",
                runtime = "qoder",
                host_id = "h-d",
                created_by_user_id = "user-2",
            ),
            "; ",
        ),
    )
    .execute(&pool)
    .await
    .unwrap();
    let app = prepare(&config(fixture.path.clone())).await.unwrap();

    assert_design_lifecycle(
        &app.router,
        &pool,
        "/api/me/avatar",
        "/api/me/avatar/design",
    )
    .await;
    assert_design_lifecycle(
        &app.router,
        &pool,
        "/api/agents/a-d/avatar",
        "/api/agents/a-d/avatar/design",
    )
    .await;

    // Reads require a session and the agent read requires edit authority.
    assert_eq!(
        request(&app.router, "GET", "/api/me/avatar/design", None)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(
            &app.router,
            "GET",
            "/api/agents/a-other/avatar/design",
            Some(&format!("Bearer {TOKEN}"))
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );

    // The agent's own internal upload honours the same rules.
    seed_runtime_agent(&fixture.path, "a-self").await;
    let runtime = mint_runtime_bearer(&app, "a-self").await;
    let (status, body) = post_avatar_as(
        &app.router,
        "/internal/agents/a-self/avatar",
        &runtime,
        upload(Some(DESIGN), false),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        design(&app.router, "/api/agents/a-self/avatar/design").await,
        expected_design()
    );
    let (status, _) = post_avatar_as(
        &app.router,
        "/internal/agents/a-self/avatar",
        &runtime,
        upload(None, false),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        design(&app.router, "/api/agents/a-self/avatar/design").await,
        serde_json::json!({"design":null})
    );

    // Deleting a principal releases its design.
    let (status, _) = post_avatar(
        &app.router,
        "/api/agents/a-d/avatar",
        upload(Some(DESIGN), false),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(design_rows(&pool).await, 1);
    sqlx::query("DELETE FROM agents WHERE id = 'a-d'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(design_rows(&pool).await, 0);
    pool.close().await;
}

#[tokio::test]
async fn schema_refuses_a_design_that_outlives_its_image() {
    let fixture = fixture(true).await;
    seed_human(&fixture.path).await;
    let pool = SqlitePoolOptions::new()
        .connect(fixture.path.to_str().unwrap())
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO avatar_designs VALUES ('d1', 1, '{}', 1); \
         UPDATE users SET avatar_storage_key = 'k1', avatar_content_type = 'image/png', \
           avatar_design_id = 'd1' WHERE id = 'user-1';",
    )
    .execute(&pool)
    .await
    .unwrap();
    for statement in [
        "UPDATE users SET avatar_storage_key = 'k2' WHERE id = 'user-1'",
        "UPDATE users SET avatar_storage_key = NULL, avatar_content_type = NULL WHERE id = 'user-1'",
        "INSERT INTO avatar_designs VALUES ('d2', 1, '[]', 1)",
        "INSERT INTO avatar_designs VALUES ('d3', 0, '{}', 1)",
    ] {
        assert!(
            sqlx::query(statement).execute(&pool).await.is_err(),
            "{statement}"
        );
    }
    pool.close().await;
}
