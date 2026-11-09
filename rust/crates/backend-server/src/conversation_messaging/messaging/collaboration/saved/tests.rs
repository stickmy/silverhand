use std::{
    io::{self, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use axum::{
    body::{Body, to_bytes},
    extract::Extension,
    http::{Method, Request, StatusCode},
};
use serde_json::{Value, json};
use silverhand_backend_core::{
    conversation_messaging::messaging::collaboration::saved::{
        SavedAftermath, SavedChangeFact, SavedListQuery, SavedPublicationFailure, SavedSender,
    },
    foundation::identity::UserId,
};
use silverhand_backend_sqlite::conversation_messaging::messaging::MessagingStore;
use sqlx::{Connection, Executor, SqliteConnection};
use tower::ServiceExt;
use tracing_subscriber::fmt::MakeWriter;

use super::{ListQueryWire, changed_event, list_query, router};

fn fixture_case(name: &str) -> Value {
    let fixture: Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../rust/test-fixtures/fixtures/saved-wire.json"
    )))
    .unwrap();
    fixture
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["name"] == name)
        .unwrap()
        .clone()
}

fn fixture_body(name: &str) -> Value {
    let case = fixture_case(name);
    if let Some(body_case) = case.get("body_case").and_then(Value::as_str) {
        return fixture_body(body_case);
    }
    if let Some(saved_case) = case
        .get("body")
        .and_then(|body| body.get("saved_case"))
        .and_then(Value::as_str)
    {
        return json!({"saved":[fixture_body(saved_case)["saved"].clone()]});
    }
    case["body"].clone()
}

fn normalized_saved(mut body: Value) -> Value {
    let entries = if body["saved"].is_array() {
        body["saved"].as_array_mut().unwrap()
    } else {
        std::slice::from_mut(body.get_mut("saved").unwrap())
    };
    for entry in entries {
        entry["id"] = json!("saved-1");
        entry["saved_at"] = json!(1000);
    }
    body
}

#[derive(Clone, Default)]
struct Events(Arc<Mutex<Vec<SavedChangeFact>>>);

impl SavedAftermath for Events {
    async fn announce(&self, fact: &SavedChangeFact) -> Result<(), SavedPublicationFailure> {
        self.0.lock().unwrap().push(fact.clone());
        Ok(())
    }
}

#[derive(Clone, Default)]
struct FailingEvents(Arc<AtomicUsize>);

impl SavedAftermath for FailingEvents {
    async fn announce(&self, _fact: &SavedChangeFact) -> Result<(), SavedPublicationFailure> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(SavedPublicationFailure::from_source(io::Error::other(
            "viewer event transport failed",
        )))
    }
}

#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

struct LogWriter(LogBuffer);

impl Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for LogBuffer {
    type Writer = LogWriter;
    fn make_writer(&'a self) -> Self::Writer {
        LogWriter(self.clone())
    }
}

async fn seeded() -> (
    tempfile::TempDir,
    std::path::PathBuf,
    MessagingStore,
    Events,
) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("saved.sqlite");
    let mut db = crate::current_schema::create_current_schema(&path).await;
    db.execute(sqlx::raw_sql(
        concat!(
            "INSERT INTO users (id, name, display_name, created_at, updated_at) VALUES ('user-1', 'local-test', 'Local Test User', 0, 0), ('user-2', 'peer', 'Peer', 0, 0), ('user-3', 'stale', 'Stale', 0, 0); ",
            "INSERT INTO workspaces (id, name, name_key, owner_user_id, created_at, updated_at) VALUES ('workspace-1', 'Space', 'space', 'user-1', 0, 0); ",
            "INSERT INTO workspace_users (workspace_id, user_id, joined_at, role) VALUES ('workspace-1', 'user-1', 0, 'owner'), ('workspace-1', 'user-2', 0, 'member'); ",
            seed_channel!(
                id = "conversation-1",
                workspace_id = "workspace-1",
                title = "saved-main",
                title_key = "saved-main",
                created_at = 0,
                updated_at = 0,
                latest_message_seq = 2,
            ),
            "; ",
            seed_channel!(
                id = "conversation-public",
                workspace_id = "workspace-1",
                title = "Public",
                title_key = "public",
                visibility = "public",
                created_at = 0,
                updated_at = 0,
                latest_message_seq = 1,
            ),
            "; ",
            seed_direct_pair!(
                id = "pair-1",
                title = "Pair",
                endpoint_a_kind = "human",
                endpoint_a_id = "user-1",
                endpoint_b_kind = "human",
                endpoint_b_id = "user-2",
                created_at = 0,
                updated_at = 0,
                latest_message_seq = 1,
            ),
            "; ",
            "INSERT INTO conversation_users (conversation_id, user_id, joined_at) VALUES ('conversation-1', 'user-1', 0), ('conversation-1', 'user-2', 0), ('conversation-1', 'user-3', 0); ",
            "INSERT INTO messages (id, conversation_id, from_kind, from_user_id, author_id, author_name, author_display_name, content, metadata, seq, created_at, thread_root_id) VALUES ('message-1', 'conversation-1', 'user', 'user-1', 'user-1', 'owner', 'Owner', 'saved parity', NULL, 1, 100, NULL), ('reply-1', 'conversation-1', 'user', 'user-2', 'user-2', 'peer', 'Peer', 'reply', NULL, 2, 200, 'message-1'), ('pair-message', 'pair-1', 'user', 'user-2', 'user-2', 'peer', 'Peer', 'dm', NULL, 1, 300, NULL), ('public-message', 'conversation-public', 'user', 'user-1', 'user-1', 'local-test', 'Local Test User', 'public', NULL, 1, 400, NULL); ",
        ),
    ))
    .await
    .unwrap();
    db.close().await.unwrap();
    let store = MessagingStore::open(&path).await.unwrap();
    (directory, path, store, Events::default())
}

async fn call(app: axum::Router, method: Method, uri: &str) -> (StatusCode, Value) {
    let response = app
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()))
    };
    (status, body)
}

#[tokio::test]
async fn save_and_unsave_publish_only_the_viewers_committed_changes() {
    let (_directory, path, store, events) = seeded().await;
    let app = router(store.saved(), store.saved(), events.clone())
        .layer(Extension(UserId::parse("user-1").unwrap()));

    let (status, first) = call(app.clone(), Method::POST, "/api/messages/message-1/save").await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(first["saved"]["message"]["id"], "message-1");
    assert_eq!(first["saved"]["place"]["kind"], "workspace_channel");
    assert_eq!(
        normalized_saved(first.clone()),
        fixture_body("save_changed")
    );
    let first_id = first["saved"]["id"].clone();

    let (status, duplicate) = call(app.clone(), Method::POST, "/api/messages/message-1/save").await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(duplicate["saved"]["id"], first_id);
    assert_eq!(normalized_saved(duplicate), fixture_body("save_duplicate"));
    assert_eq!(events.0.lock().unwrap().len(), 1);

    let (status, listed) = call(
        app.clone(),
        Method::GET,
        "/api/saved?workspace_id=workspace-1&user_id=user-1",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(normalized_saved(listed), fixture_body("list"));

    let (status, _) = call(app.clone(), Method::DELETE, "/api/messages/message-1/save").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(app, Method::DELETE, "/api/messages/message-1/save").await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    {
        let facts = events.0.lock().unwrap();
        assert_eq!(facts.len(), 2);
        assert_eq!(facts[0].viewer().as_str(), "user-1");
        assert_eq!(facts[1].viewer().as_str(), "user-1");
        assert_eq!(
            changed_event(&facts[0]),
            fixture_case("save_changed")["event"]
        );
        assert_eq!(
            changed_event(&facts[1]),
            fixture_case("unsave_changed")["event"]
        );
    }

    let mut db = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM saved_messages")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn cold_list_rederives_workspace_and_direct_pair_participation() {
    let (_directory, path, store, events) = seeded().await;
    let owner = router(store.saved(), store.saved(), events.clone())
        .layer(Extension(UserId::parse("user-1").unwrap()));
    for message in ["message-1", "pair-message"] {
        assert_eq!(
            call(
                owner.clone(),
                Method::POST,
                &format!("/api/messages/{message}/save"),
            )
            .await
            .0,
            StatusCode::CREATED
        );
    }
    let (status, body) = call(owner.clone(), Method::GET, "/api/saved").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["saved"].as_array().unwrap().len(), 2);
    assert_eq!(body["saved"][0]["place"]["kind"], "direct_pair");
    assert_eq!(body["saved"][1]["place"]["kind"], "workspace_channel");

    let mut db = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    sqlx::query(
        "DELETE FROM workspace_users WHERE workspace_id='workspace-1' AND user_id='user-1'",
    )
    .execute(&mut db)
    .await
    .unwrap();
    let (status, body) = call(owner, Method::GET, "/api/saved").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["saved"].as_array().unwrap().len(), 1);
    assert_eq!(body["saved"][0]["place"]["kind"], "direct_pair");

    let stale = router(store.saved(), store.saved(), events)
        .layer(Extension(UserId::parse("user-3").unwrap()));
    let (status, _) = call(stale, Method::POST, "/api/messages/message-1/save").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let public_nonseat = router(store.saved(), store.saved(), Events::default())
        .layer(Extension(UserId::parse("user-2").unwrap()));
    let (status, body) = call(
        public_nonseat,
        Method::POST,
        "/api/messages/public-message/save",
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "public read authority does not widen the saved participation axis"
    );
    assert_eq!(body, fixture_body("save_public_unseated"));
}

#[tokio::test]
async fn workspace_and_sender_filters_preserve_exact_message_projection() {
    let (_directory, _path, store, events) = seeded().await;
    let app = router(store.saved(), store.saved(), events)
        .layer(Extension(UserId::parse("user-1").unwrap()));
    for message in ["message-1", "reply-1", "pair-message"] {
        assert_eq!(
            call(
                app.clone(),
                Method::POST,
                &format!("/api/messages/{message}/save"),
            )
            .await
            .0,
            StatusCode::CREATED
        );
    }

    let (status, body) = call(
        app.clone(),
        Method::GET,
        "/api/saved?workspace_id=workspace-1&user_id=user-2",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["saved"].as_array().unwrap().len(), 1);
    assert_eq!(
        body["saved"][0]["message"],
        json!({
            "id":"reply-1",
            "conversation_id":"conversation-1",
            "from_kind":"user",
            "from_agent_id":null,
            "from_user_id":"user-2",
            "author":{
                "status":"active","kind":"user","id":"user-2","name":"peer",
                "display_name":"Peer","avatar_url":null,"runtime":null
            },
            "content":"reply",
            "metadata":null,
            "attachments":null,
            "reactions":null,
            "seq":2,
            "created_at":200,
            "thread_root_id":"message-1",
            "linked_task":null
        })
    );

    let (status, body) = call(
        app.clone(),
        Method::GET,
        "/api/saved?user_id=user-1&agent_id=agent-1",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, fixture_body("list_both_senders"));
    let (status, body) = call(app, Method::GET, "/api/saved?since=-1").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, fixture_body("list_negative_since"));
}

#[tokio::test]
async fn workspace_filter_keeps_missing_and_forbidden_distinct() {
    let (_directory, _path, store, events) = seeded().await;
    let owner = router(store.saved(), store.saved(), events.clone())
        .layer(Extension(UserId::parse("user-1").unwrap()));
    assert_eq!(
        call(owner, Method::GET, "/api/saved?workspace_id=missing").await,
        (
            StatusCode::NOT_FOUND,
            json!({"error":"workspace not found"})
        )
    );

    let outsider = router(store.saved(), store.saved(), events)
        .layer(Extension(UserId::parse("user-3").unwrap()));
    assert_eq!(
        call(outsider, Method::GET, "/api/saved?workspace_id=workspace-1",).await,
        (StatusCode::FORBIDDEN, json!({"error":"forbidden"}))
    );
}

#[tokio::test]
async fn missing_messages_keep_the_save_and_unsave_wire_refusals() {
    let (_directory, _path, store, events) = seeded().await;
    let app = router(store.saved(), store.saved(), events)
        .layer(Extension(UserId::parse("user-1").unwrap()));
    for (name, method) in [
        ("save_missing", Method::POST),
        ("unsave_missing", Method::DELETE),
    ] {
        let (status, body) = call(app.clone(), method, "/api/messages/missing/save").await;
        assert_eq!(
            status.as_u16(),
            fixture_case(name)["status"].as_u64().unwrap() as u16
        );
        assert_eq!(body, fixture_body(name));
    }
}

#[tokio::test]
async fn publication_failure_is_audited_once_without_compensating_the_save() {
    let (_directory, path, store, _events) = seeded().await;
    let failures = FailingEvents::default();
    let app = router(store.saved(), store.saved(), failures.clone())
        .layer(Extension(UserId::parse("user-1").unwrap()));
    let logs = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(logs.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let (status, _) = call(app, Method::POST, "/api/messages/message-1/save").await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(failures.0.load(Ordering::SeqCst), 1);

    let mut db = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    let saved: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM saved_messages WHERE user_id='user-1' AND message_id='message-1'",
    )
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(saved, 1);
    let output = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    let lines = output
        .lines()
        .filter(|line| line.contains("saved-message publication failed"))
        .collect::<Vec<_>>();
    assert_eq!(lines.len(), 1);
    assert!(lines[0].contains("viewer event transport failed"));
}

#[tokio::test]
async fn malformed_persisted_metadata_fails_closed_instead_of_emitting_an_invalid_dto() {
    let (_directory, path, store, events) = seeded().await;
    let app = router(store.saved(), store.saved(), events.clone())
        .layer(Extension(UserId::parse("user-1").unwrap()));
    assert_eq!(
        call(app.clone(), Method::POST, "/api/messages/message-1/save",)
            .await
            .0,
        StatusCode::CREATED
    );
    let mut db = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    sqlx::query("UPDATE messages SET metadata='[]' WHERE id='message-1'")
        .execute(&mut db)
        .await
        .unwrap();
    let (status, body) = call(app, Method::GET, "/api/saved").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body, Value::String("Internal Server Error".to_owned()));
    assert_eq!(events.0.lock().unwrap().len(), 1);
}

#[test]
fn the_rust_query_boundary_consumes_the_ts_signed_trim_and_coercion_contract() {
    let fixture: Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../rust/test-fixtures/fixtures/saved-query-wire.json"
    )))
    .unwrap();
    for case in fixture.as_array().unwrap() {
        let input = &case["input"];
        let parsed = list_query(ListQueryWire {
            workspace_id: input
                .get("workspace_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            since: input
                .get("since")
                .and_then(Value::as_str)
                .map(str::to_owned),
            user_id: input
                .get("user_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            agent_id: input
                .get("agent_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
        });
        if let Some(issues) = case.get("issues") {
            assert_eq!(
                Value::Array(parsed.unwrap_err()),
                *issues,
                "{}",
                case["name"]
            );
            continue;
        }
        let query = parsed.unwrap_or_else(|issues| panic!("{}: {issues:?}", case["name"]));
        assert_eq!(
            query_value(query),
            case.get("rust_output").unwrap_or(&case["output"]).clone(),
            "{}",
            case["name"]
        );
    }
}

fn query_value(query: SavedListQuery) -> Value {
    let mut out = serde_json::Map::new();
    if let Some(workspace) = query.workspace {
        out.insert("workspace_id".to_owned(), json!(workspace.as_str()));
    }
    if let Some(since) = query.since {
        out.insert(
            "since".to_owned(),
            since
                .persisted_value()
                .map_or_else(|| json!("beyond_persisted_range"), |value| json!(value)),
        );
    }
    if let Some(sender) = query.sender {
        match sender {
            SavedSender::Human(id) => out.insert("user_id".to_owned(), json!(id.as_str())),
            SavedSender::Agent(id) => out.insert("agent_id".to_owned(), json!(id.as_str())),
        };
    }
    Value::Object(out)
}

#[tokio::test]
async fn valid_since_beyond_i64_is_empty_only_after_workspace_authority() {
    let (_directory, database_path, store, events) = seeded().await;
    let app = router(store.saved(), store.saved(), events)
        .layer(Extension(UserId::parse("user-1").unwrap()));
    assert_eq!(
        call(app.clone(), Method::POST, "/api/messages/message-1/save")
            .await
            .0,
        StatusCode::CREATED
    );
    let fixture: Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../rust/test-fixtures/fixtures/saved-query-wire.json"
    )))
    .unwrap();
    for case in fixture
        .as_array()
        .unwrap()
        .iter()
        .filter(|case| case.get("path").is_some())
    {
        let path = case["path"].as_str().unwrap();
        if let Some(boundary) = case.get("boundary_saved_at").and_then(Value::as_str) {
            let mut db = SqliteConnection::connect(&format!("sqlite:{}", database_path.display()))
                .await
                .unwrap();
            sqlx::query("UPDATE saved_messages SET saved_at=? WHERE message_id='message-1'")
                .bind(boundary.parse::<i64>().unwrap())
                .execute(&mut db)
                .await
                .unwrap();
        }
        let expected_status = case
            .get("route_status")
            .and_then(Value::as_u64)
            .unwrap_or(200) as u16;
        let expected_body = if expected_status == 200 {
            json!({"saved":[]})
        } else {
            json!({"error":"invalid input","issues":case["issues"]})
        };
        assert_eq!(
            call(app.clone(), Method::GET, path).await,
            (
                StatusCode::from_u16(expected_status).unwrap(),
                expected_body
            ),
            "{}",
            case["name"]
        );
        if expected_status != 200 {
            continue;
        }
        assert_eq!(
            call(
                app.clone(),
                Method::GET,
                &format!("{path}&workspace_id=missing"),
            )
            .await,
            (
                StatusCode::NOT_FOUND,
                json!({"error":"workspace not found"})
            ),
            "{}: workspace authority first",
            case["name"]
        );
    }
}

#[tokio::test]
async fn saved_and_message_conversation_mismatch_is_corruption_not_a_split_wire_coordinate() {
    let (_directory, path, store, events) = seeded().await;
    let app = router(store.saved(), store.saved(), events)
        .layer(Extension(UserId::parse("user-1").unwrap()));
    assert_eq!(
        call(app.clone(), Method::POST, "/api/messages/message-1/save")
            .await
            .0,
        StatusCode::CREATED
    );
    let mut db = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    sqlx::query("UPDATE saved_messages SET conversation_id='pair-1' WHERE message_id='message-1'")
        .execute(&mut db)
        .await
        .unwrap();
    assert_eq!(
        call(app, Method::GET, "/api/saved").await,
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Value::String("Internal Server Error".to_owned())
        )
    );
}

#[tokio::test]
async fn linked_task_actual_conversation_is_bound_to_its_source_message() {
    let (_directory, path, store, events) = seeded().await;
    let app = router(store.saved(), store.saved(), events)
        .layer(Extension(UserId::parse("user-1").unwrap()));
    assert_eq!(
        call(app.clone(), Method::POST, "/api/messages/message-1/save")
            .await
            .0,
        StatusCode::CREATED
    );
    let mut db = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO tasks (id, workspace_id, task_number, conversation_id, title, status, \
         creator_kind, creator_user_id, source_message_id, created_at, board_key) VALUES \
         ('task-saved', 'workspace-1', 1, 'conversation-1', 'Linked task', 'todo', \
          'user', 'user-1', 'message-1', 1, 'ws:workspace-1')",
    )
    .execute(&mut db)
    .await
    .unwrap();
    sqlx::query("DROP TRIGGER trg_tasks_board_coordinates_immutable")
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::query("UPDATE tasks SET conversation_id='pair-1' WHERE id='task-saved'")
        .execute(&mut db)
        .await
        .unwrap();
    assert_eq!(
        call(app.clone(), Method::GET, "/api/saved").await.0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    sqlx::query("UPDATE tasks SET conversation_id='conversation-1' WHERE id='task-saved'")
        .execute(&mut db)
        .await
        .unwrap();
    let (status, body) = call(app, Method::GET, "/api/saved").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["saved"][0]["message"]["linked_task"]["task_id"],
        "task-saved"
    );
}

#[tokio::test]
async fn same_millisecond_saved_rows_have_one_total_order_across_the_cap() {
    let (_directory, path, store, events) = seeded().await;
    let ordering: Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../rust/test-fixtures/fixtures/saved-ordering.json"
    )))
    .unwrap();
    let count = ordering["seed_count"].as_i64().unwrap();
    let mut db = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    for ordinal in 1..=count {
        let message = format!("ordered-message-{ordinal:03}");
        sqlx::query(
            "INSERT INTO messages (id, conversation_id, from_kind, from_user_id, author_id, author_name, author_display_name, content, seq, created_at) VALUES (?, 'conversation-1', 'user', 'user-1', 'user-1', 'owner', 'Owner', ?, ?, 100)",
        )
        .bind(&message)
        .bind(format!("same-millisecond-{ordinal}"))
        .bind(ordinal + 10)
        .execute(&mut db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO saved_messages (id, user_id, message_id, conversation_id, saved_at) VALUES (?, 'user-1', ?, 'conversation-1', 1000)",
        )
        .bind(format!("saved-{ordinal:03}"))
        .bind(message)
        .execute(&mut db)
        .await
        .unwrap();
    }
    drop(db);
    let app = router(store.saved(), store.saved(), events)
        .layer(Extension(UserId::parse("user-1").unwrap()));
    let (status, body) = call(app, Method::GET, "/api/saved").await;
    assert_eq!(status, StatusCode::OK);
    let entries = body["saved"].as_array().unwrap();
    assert_eq!(entries.len() as u64, ordering["limit"].as_u64().unwrap());
    assert_eq!(entries[0]["id"], ordering["first_saved_id"]);
    assert_eq!(entries.last().unwrap()["id"], ordering["last_saved_id"]);
}

/// Plant a linked task whose `tasks.task_number` sits one past
/// `Number.MAX_SAFE_INTEGER`. The canonical `TaskNumber` rejects it at decode, so the
/// saved projection faults closed (500) rather than serialising a number the JSON wire
/// would round — the drift the projection-local `SavedTaskNumber` used to allow.
#[tokio::test]
async fn a_saved_linked_task_number_past_the_javascript_safe_integer_faults_the_list() {
    let (_directory, path, store, events) = seeded().await;
    let app = router(store.saved(), store.saved(), events)
        .layer(Extension(UserId::parse("user-1").unwrap()));
    assert_eq!(
        call(app.clone(), Method::POST, "/api/messages/message-1/save")
            .await
            .0,
        StatusCode::CREATED
    );
    let mut db = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    // Number.MAX_SAFE_INTEGER + 1: the first task_number the wire would round.
    sqlx::query(
        "INSERT INTO tasks (id, workspace_id, task_number, conversation_id, title, status, \
         creator_kind, creator_user_id, source_message_id, created_at, board_key) VALUES \
         ('task-saved', 'workspace-1', 9007199254740992, 'conversation-1', 'Linked task', \
          'todo', 'user', 'user-1', 'message-1', 1, 'ws:workspace-1')",
    )
    .execute(&mut db)
    .await
    .unwrap();
    assert_eq!(
        call(app, Method::GET, "/api/saved").await,
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Value::String("Internal Server Error".to_owned())
        )
    );
}

/// `Number.MAX_SAFE_INTEGER` itself and every one of the five task statuses survive the
/// round trip through the canonical `TaskNumber`/`TaskStatus` saved-wire projection.
#[tokio::test]
async fn the_max_safe_task_number_and_every_status_round_trip_through_the_saved_projection() {
    for status in ["todo", "in_progress", "in_review", "done", "closed"] {
        let (_directory, path, store, events) = seeded().await;
        let app = router(store.saved(), store.saved(), events)
            .layer(Extension(UserId::parse("user-1").unwrap()));
        assert_eq!(
            call(app.clone(), Method::POST, "/api/messages/message-1/save")
                .await
                .0,
            StatusCode::CREATED
        );
        let mut db = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
            .await
            .unwrap();
        // The schema ties status='closed' to a non-null closed_at.
        let closed_at: Option<i64> = if status == "closed" { Some(1) } else { None };
        sqlx::query(
            "INSERT INTO tasks (id, workspace_id, task_number, conversation_id, title, status, \
             creator_kind, creator_user_id, source_message_id, created_at, board_key, closed_at) \
             VALUES ('task-saved', 'workspace-1', 9007199254740991, 'conversation-1', \
              'Linked task', ?, 'user', 'user-1', 'message-1', 1, 'ws:workspace-1', ?)",
        )
        .bind(status)
        .bind(closed_at)
        .execute(&mut db)
        .await
        .unwrap();
        let (http_status, body) = call(app, Method::GET, "/api/saved").await;
        assert_eq!(http_status, StatusCode::OK);
        assert_eq!(
            body["saved"][0]["message"]["linked_task"],
            json!({
                "task_id": "task-saved",
                "task_number": 9_007_199_254_740_991_i64,
                "title": "Linked task",
                "status": status,
                "assignee": Value::Null,
            }),
            "status {status} must round-trip"
        );
    }
}
