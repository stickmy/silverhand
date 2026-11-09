use super::*;

const OVERSIZED_UTF16: usize = 138_484;

/// The 10,000 UTF-16 bound applies only to body INPUT. A stored body of any
/// length is read verbatim and survives every operation that does not set it.
#[tokio::test]
async fn an_oversized_stored_task_body_reads_and_survives_non_body_operations() {
    let fixture = fixture(true).await;
    seed_human(&fixture.path).await;
    seed_runtime_agent(&fixture.path, "task-agent").await;
    seed_workspace_conversation(&fixture.path).await;
    let pool = SqlitePoolOptions::new()
        .connect(fixture.path.to_str().unwrap())
        .await
        .unwrap();
    sqlx::query("INSERT INTO conversation_agents(conversation_id,agent_id,joined_at) VALUES(?,'task-agent',1)")
        .bind(CONVERSATION_ID)
        .execute(&pool)
        .await
        .unwrap();
    let app = prepare(&remote_config(fixture.path.clone())).await.unwrap();
    let router = app.router.clone();
    let bearer = format!("Bearer {TOKEN}");
    let post = |uri: String, body: String| {
        let router = router.clone();
        let bearer = bearer.clone();
        async move {
            json_request_with_body(&router, "POST", &uri, Some(&bearer), Body::from(body)).await
        }
    };

    let (status, created) = post(
        "/api/tasks".into(),
        format!(r#"{{"title":"Legacy","conversation_id":"{CONVERSATION_ID}"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let created: serde_json::Value = serde_json::from_str(&created).unwrap();
    let task_id = created["task"]["id"].as_str().unwrap().to_owned();
    let task_number = created["task"]["task_number"].as_i64().unwrap();

    let stored = format!("{}\u{1f600}", "x".repeat(OVERSIZED_UTF16 - 2));
    assert_eq!(stored.encode_utf16().count(), OVERSIZED_UTF16);
    sqlx::query("UPDATE tasks SET content=? WHERE id=?")
        .bind(&stored)
        .bind(&task_id)
        .execute(&pool)
        .await
        .unwrap();

    let read_body = || async {
        let (status, detail) = request(
            &router,
            "GET",
            &format!("/api/tasks/{task_id}"),
            Some(&bearer),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{detail}");
        let detail: serde_json::Value = serde_json::from_str(&detail).unwrap();
        detail["task"]["content"].as_str().unwrap().to_owned()
    };
    assert_eq!(read_body().await, stored);

    for uri in [
        format!("/api/tasks/board/column?workspace_id={WORKSPACE_ID}&status=todo"),
        format!("/api/tasks/board/counts?workspace_id={WORKSPACE_ID}"),
        format!("/api/tasks/by-number?conversation_id={CONVERSATION_ID}&task_number={task_number}"),
    ] {
        let (status, body) = request(&router, "GET", &uri, Some(&bearer)).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
    }

    for (uri, body) in [
        (
            format!("/api/tasks/{task_id}/claim"),
            r#"{"agent_id":"task-agent"}"#.into(),
        ),
        (format!("/api/tasks/{task_id}/unclaim"), "{}".into()),
        (
            format!("/api/tasks/{task_id}/status"),
            r#"{"status":"done"}"#.into(),
        ),
        (
            format!("/api/tasks/{task_id}/edit"),
            r#"{"title":"Renamed"}"#.into(),
        ),
    ] {
        let (status, response) = post(uri.clone(), body).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {response}");
        assert_eq!(
            read_body().await,
            stored,
            "{uri} must leave the body untouched"
        );
    }

    // Body input stays bounded: an explicit over-limit edit is refused and changes nothing.
    let over = serde_json::json!({ "body": "y".repeat(10_001) }).to_string();
    let (status, _) = post(format!("/api/tasks/{task_id}/edit"), over).await;
    assert!(status.is_client_error(), "{status}");
    assert_eq!(read_body().await, stored);
    let at_limit = "z".repeat(10_000);
    let (status, response) = post(
        format!("/api/tasks/{task_id}/edit"),
        serde_json::json!({ "body": at_limit }).to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(read_body().await, at_limit);

    let (status, _) = post(
        "/api/tasks".into(),
        serde_json::json!({
            "title": "Too long",
            "body": "y".repeat(10_001),
            "conversation_id": CONVERSATION_ID,
        })
        .to_string(),
    )
    .await;
    assert!(status.is_client_error(), "{status}");
    pool.close().await;
}
