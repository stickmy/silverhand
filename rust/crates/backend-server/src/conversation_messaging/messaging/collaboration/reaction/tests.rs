use std::{
    io::{self, Write},
    sync::{Arc, Mutex},
};

use axum::{
    body::{Body, to_bytes},
    extract::Extension,
    http::{Method, Request, StatusCode},
};
use serde_json::{Value, json};
use silverhand_backend_core::{
    conversation_messaging::messaging::{
        MessageId,
        collaboration::reaction::{
            ReactionAftermath, ReactionChangedFact, ReactionReadPort, Reactor,
        },
    },
    foundation::identity::{DaemonInstanceId, UserId},
    host_runtime::runtime_credential::{RuntimeCustody, RuntimeGeneration, RuntimePrincipal},
};
use silverhand_backend_sqlite::conversation_messaging::messaging::MessagingStore;
use sqlx::{Connection, Executor, SqliteConnection};
use tower::ServiceExt;
use tracing_subscriber::fmt::MakeWriter;

use super::{router, wire};

fn fixture_case(name: &str) -> (StatusCode, Value) {
    let fixture: Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../rust/test-fixtures/fixtures/reaction-wire.json"
    )))
    .unwrap();
    let case = fixture
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["name"] == name)
        .unwrap();
    (
        StatusCode::from_u16(case["status"].as_u64().unwrap() as u16).unwrap(),
        case["body"].clone(),
    )
}

fn fixture_event(name: &str) -> Value {
    let fixture: Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../rust/test-fixtures/fixtures/reaction-wire.json"
    )))
    .unwrap();
    fixture
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["name"] == name)
        .unwrap()["event"]
        .clone()
}

#[derive(Clone, Default)]
struct Events(Arc<Mutex<Vec<ReactionChangedFact>>>);

impl ReactionAftermath for Events {
    async fn announce(
        &self,
        fact: &ReactionChangedFact,
    ) -> Result<
        (),
        silverhand_backend_core::conversation_messaging::messaging::collaboration::reaction::ReactionPublicationFailure,
    >{
        self.0.lock().unwrap().push(fact.clone());
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct FailingEvents;

impl ReactionAftermath for FailingEvents {
    async fn announce(
        &self,
        _fact: &ReactionChangedFact,
    ) -> Result<
        (),
        silverhand_backend_core::conversation_messaging::messaging::collaboration::reaction::ReactionPublicationFailure,
    >{
        Err(
            silverhand_backend_core::conversation_messaging::messaging::collaboration::reaction::ReactionPublicationFailure::from_source(
                io::Error::other("scope event hub failed"),
            ),
        )
    }
}

#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl LogBuffer {
    fn events(&self) -> Vec<Value> {
        let bytes = self.0.lock().unwrap().clone();
        String::from_utf8(bytes)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

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
    let path = directory.path().join("reactions.sqlite");
    let mut db = crate::current_schema::create_current_schema(&path).await;
    db.execute(sqlx::raw_sql(
        "INSERT INTO users (id, name, display_name, created_at, updated_at) VALUES \
           ('user-1', 'owner', 'Owner', 0, 0), ('user-2', 'outsider', 'Outsider', 0, 0); \
         INSERT INTO hosts (id, name, kind, created_at) VALUES ('host-1', 'Host', 'local', 0); \
         INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, display_name, \
           created_by_user_id, materialization_status, session_scope) VALUES \
           ('agent-1', 'agent', 'codex', 'host-1', 0, 0, 'Agent', 'user-1', 'active', 'conversation'); \
         INSERT INTO workspaces (id, name, name_key, owner_user_id, created_at, updated_at) \
           VALUES ('workspace-1', 'Space', 'space', 'user-1', 0, 0); \
         INSERT INTO workspace_users (workspace_id, user_id, joined_at, role) VALUES \
           ('workspace-1', 'user-1', 0, 'owner'), ('workspace-1', 'user-2', 0, 'member'); \
         INSERT INTO workspace_members (workspace_id, agent_id, joined_at) VALUES \
           ('workspace-1', 'agent-1', 0); \
         INSERT INTO workspace_agent_capabilities (workspace_id, agent_id, capability) VALUES \
           ('workspace-1', 'agent-1', 'send_messages'); \
         INSERT INTO conversations (id, workspace_id, placement_kind, roster_policy, title, \
           title_key, visibility, created_at, updated_at, latest_message_seq) VALUES \
           ('conversation-1', 'workspace-1', 'workspace', 'open', 'General', 'general', 'private', 0, 0, 2), \
           ('conversation-archived', 'workspace-1', 'workspace', 'open', 'Archived', 'archived', 'private', 0, 0, 1); \
         INSERT INTO conversations (id, workspace_id, placement_kind, roster_policy, title, \
           title_key, visibility, endpoint_a_kind, endpoint_a_id, endpoint_b_kind, endpoint_b_id, \
           created_at, updated_at, latest_message_seq) VALUES \
           ('pair-1', NULL, 'direct_pair', 'fixed', 'Pair', NULL, 'private', \
            'agent', 'agent-1', 'human', 'user-1', 0, 0, 1); \
         UPDATE conversations SET archived_at = 1 WHERE id = 'conversation-archived'; \
         INSERT INTO conversation_users (conversation_id, user_id, joined_at) VALUES \
           ('conversation-1', 'user-1', 0), ('conversation-archived', 'user-1', 0); \
         INSERT INTO conversation_agents (conversation_id, agent_id, joined_at) VALUES \
           ('conversation-1', 'agent-1', 0); \
         INSERT INTO messages (id, conversation_id, from_kind, from_user_id, content, seq, created_at) VALUES \
           ('message-1', 'conversation-1', 'user', 'user-1', 'hello', 1, 1), \
           ('system-1', 'conversation-1', 'system', NULL, 'notice', 2, 2), \
           ('reply-1', 'conversation-1', 'user', 'user-1', 'reply', 3, 3), \
           ('archived-message', 'conversation-archived', 'user', 'user-1', 'old', 1, 1), \
           ('pair-message', 'pair-1', 'user', 'user-1', 'dm', 1, 1); \
         UPDATE messages SET thread_root_id='message-1' WHERE id='reply-1'; \
         INSERT INTO conversation_stream_heads (conversation_id, thread_root_id, latest_seq) VALUES \
           ('conversation-1', NULL, 2), ('conversation-1', 'message-1', 3);"
    )).await.unwrap();
    db.close().await.unwrap();
    let store = MessagingStore::open(&path).await.unwrap();
    (directory, path, store, Events::default())
}

async fn call(app: axum::Router, method: Method, uri: &str, body: Value) -> (StatusCode, Value) {
    call_raw(app, method, uri, body.to_string()).await
}

async fn call_raw(
    app: axum::Router,
    method: Method,
    uri: &str,
    body: impl Into<String>,
) -> (StatusCode, Value) {
    call_with_content_type(app, method, uri, body, Some("application/json")).await
}

/// Like [`call_raw`] but lets the caller omit or override the `Content-Type` header, so an
/// admission test can drive the missing/wrong content-type → native 415 discriminator.
async fn call_with_content_type(
    app: axum::Router,
    method: Method,
    uri: &str,
    body: impl Into<String>,
    content_type: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(content_type) = content_type {
        builder = builder.header("content-type", content_type);
    }
    let response = app
        .oneshot(builder.body(Body::from(body.into())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()))
    };
    (status, value)
}

#[tokio::test]
async fn human_add_remove_is_idempotent_and_never_moves_message_freshness() {
    let (_directory, path, store, events) = seeded().await;
    let app = router(store.reactions(), store.reactions(), events.clone())
        .layer(Extension(UserId::parse("user-1").unwrap()));

    let (status, body) = call(
        app.clone(),
        Method::POST,
        "/api/messages/message-1/reactions",
        json!({"emoji":"👍"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"changed":true,"emoji":"👍","count":1}));
    let (status, body) = call(
        app.clone(),
        Method::POST,
        "/api/messages/message-1/reactions",
        json!({"emoji":"👍"}),
    )
    .await;
    assert_eq!(
        (status, body),
        (
            StatusCode::OK,
            json!({"changed":false,"emoji":"👍","count":1})
        )
    );
    assert_eq!(events.0.lock().unwrap().len(), 1);

    let (status, detail) = call(
        app.clone(),
        Method::GET,
        "/api/messages/message-1/reactions",
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        detail,
        json!({"reactions":[{"emoji":"👍","reactors":[{"kind":"user","id":"user-1","display_name":"Owner"}]}]})
    );

    let mut db = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    let freshness: (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT seq FROM messages WHERE id='message-1'), \
         (SELECT latest_message_seq FROM conversations WHERE id='conversation-1'), \
         (SELECT latest_seq FROM conversation_stream_heads WHERE conversation_id='conversation-1' AND thread_root_id IS NULL)"
    ).fetch_one(&mut db).await.unwrap();
    assert_eq!(freshness, (1, 2, 2));

    let (status, body) = call(
        app,
        Method::DELETE,
        "/api/messages/message-1/reactions",
        json!({"emoji":"👍"}),
    )
    .await;
    assert_eq!(
        (status, body),
        (
            StatusCode::OK,
            json!({"changed":true,"emoji":"👍","count":0})
        )
    );
    assert_eq!(events.0.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn a_reply_reaction_preserves_both_stream_heads_and_projects_the_exact_cluster_coordinate() {
    let (_directory, path, store, events) = seeded().await;
    let app = router(store.reactions(), store.reactions(), events.clone())
        .layer(Extension(UserId::parse("user-1").unwrap()));

    let (status, _) = call(
        app,
        Method::POST,
        "/api/messages/reply-1/reactions",
        json!({"emoji":"🔥"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let fact = events.0.lock().unwrap()[0].clone();
    assert_eq!(
        wire::MESSAGE_REACTION_CHANGED_EVENT,
        "message_reaction_changed"
    );
    assert_eq!(
        serde_json::to_value(wire::changed_event(&fact)).unwrap(),
        json!({
            "message_id":"reply-1",
            "conversation_id":"conversation-1",
            "thread_root_id":"message-1",
            "emoji":"🔥",
            "op":"add",
            "reactor_kind":"user",
            "reactor_id":"user-1",
            "count":1
        })
    );

    let mut db = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    let heads: Vec<(Option<String>, i64)> = sqlx::query_as(
        "SELECT thread_root_id, latest_seq FROM conversation_stream_heads \
         WHERE conversation_id='conversation-1' ORDER BY thread_root_id",
    )
    .fetch_all(&mut db)
    .await
    .unwrap();
    assert_eq!(heads, vec![(None, 2), (Some("message-1".into()), 3)]);
    let latest: i64 = sqlx::query_scalar(
        "SELECT latest_message_seq FROM conversations WHERE id='conversation-1'",
    )
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(latest, 2);
}

#[tokio::test]
async fn runtime_route_binds_the_authenticated_agent_and_source_before_the_uow() {
    let (_directory, _path, store, events) = seeded().await;
    let custody = RuntimeCustody::new(
        RuntimeGeneration::new(1).unwrap(),
        DaemonInstanceId::parse("daemon-1").unwrap(),
    );
    let principal = RuntimePrincipal::ConversationSlot {
        agent: silverhand_backend_core::foundation::identity::AgentId::parse("agent-1").unwrap(),
        source: silverhand_backend_core::foundation::identity::ConversationId::parse(
            "conversation-1",
        )
        .unwrap(),
        custody,
    };
    let app =
        router(store.reactions(), store.reactions(), events.clone()).layer(Extension(principal));
    let (status, body) = call(
        app.clone(),
        Method::POST,
        "/internal/agents/agent-1/react-message",
        json!({"message_id":"message-1","emoji":"🎯"}),
    )
    .await;
    assert_eq!(
        (status, body),
        (
            StatusCode::OK,
            json!({"changed":true,"emoji":"🎯","count":1})
        )
    );
    let (status, _) = call(
        app,
        Method::POST,
        "/internal/agents/other/react-message",
        json!({"message_id":"message-1","emoji":"🎯"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(events.0.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn refusals_are_exact_and_commit_nothing() {
    let (_directory, path, store, events) = seeded().await;
    let owner = router(store.reactions(), store.reactions(), events.clone())
        .layer(Extension(UserId::parse("user-1").unwrap()));
    for (uri, emoji, status, code) in [
        (
            "/api/messages/missing/reactions",
            "👍",
            StatusCode::NOT_FOUND,
            "message_not_found",
        ),
        (
            "/api/messages/system-1/reactions",
            "👍",
            StatusCode::UNPROCESSABLE_ENTITY,
            "system_message_not_reactable",
        ),
        (
            "/api/messages/archived-message/reactions",
            "👍",
            StatusCode::CONFLICT,
            "conversation_archived",
        ),
        (
            "/api/messages/message-1/reactions",
            "words",
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_reaction",
        ),
    ] {
        let (actual_status, body) =
            call(owner.clone(), Method::POST, uri, json!({"emoji":emoji})).await;
        assert_eq!(actual_status, status);
        assert_eq!(body["code"], code);
    }
    let outsider = router(store.reactions(), store.reactions(), events.clone())
        .layer(Extension(UserId::parse("user-2").unwrap()));
    let (status, body) = call(
        outsider,
        Method::POST,
        "/api/messages/message-1/reactions",
        json!({"emoji":"👍"}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], "forbidden");
    let mut db = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM message_reactions")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(count, 0);
    assert!(events.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn cold_aggregate_and_live_fact_share_the_authoritative_post_change_count() {
    let (_directory, _path, store, events) = seeded().await;
    let app = router(store.reactions(), store.reactions(), events.clone())
        .layer(Extension(UserId::parse("user-1").unwrap()));
    let _ = call(
        app,
        Method::POST,
        "/api/messages/message-1/reactions",
        json!({"emoji":"✨"}),
    )
    .await;
    let cold = store
        .reactions()
        .aggregates(
            &Reactor::Human(UserId::parse("user-1").unwrap()),
            &[MessageId::parse("message-1").unwrap()],
        )
        .await
        .unwrap();
    assert_eq!(cold.len(), 1);
    assert_eq!(cold[0].0.as_str(), "message-1");
    assert_eq!(cold[0].1.len(), 1);
    assert_eq!(cold[0].1[0].count().value(), 1);
    assert!(cold[0].1[0].viewer_reacted());
    assert_eq!(cold[0].1[0].emoji().as_str(), "✨");
    let fact = events.0.lock().unwrap()[0].clone();
    assert_eq!(fact.count().value(), 1);
    assert_eq!(fact.emoji().as_str(), "✨");
    let other = store
        .reactions()
        .aggregates(
            &Reactor::Human(UserId::parse("user-2").unwrap()),
            &[MessageId::parse("message-1").unwrap()],
        )
        .await
        .unwrap();
    assert_eq!(other[0].1[0].count().value(), 1);
    assert!(!other[0].1[0].viewer_reacted());
}

fn runtime_principal(source: &str) -> RuntimePrincipal {
    RuntimePrincipal::ConversationSlot {
        agent: silverhand_backend_core::foundation::identity::AgentId::parse("agent-1").unwrap(),
        source: silverhand_backend_core::foundation::identity::ConversationId::parse(source)
            .unwrap(),
        custody: RuntimeCustody::new(
            RuntimeGeneration::new(1).unwrap(),
            DaemonInstanceId::parse("daemon-1").unwrap(),
        ),
    }
}

#[tokio::test]
async fn production_routes_consume_the_shared_fixture_cases_and_every_case_is_accounted_for() {
    // The consumed-shared set is COLLECTED AT RUNTIME, not hand-listed: every real read of a
    // shared fixture case goes through `case`/`event`, which registers the name. A case's
    // response body and its event are two distinct consumption channels of the SAME entry, so
    // each channel rejects its own duplicate read while the two share one name-set. If a
    // production-route `assert_eq!(..., case("agent_add"))` is deleted, the name is never
    // inserted and the final union stops equalling `all_names` — there is no separate 20-name
    // truth that could drift out of sync with what the test executes.
    let consumed = std::cell::RefCell::new(std::collections::BTreeSet::<String>::new());
    let body_reads = std::cell::RefCell::new(std::collections::BTreeSet::<String>::new());
    let event_reads = std::cell::RefCell::new(std::collections::BTreeSet::<String>::new());
    let case = |name: &str| -> (StatusCode, Value) {
        assert!(
            body_reads.borrow_mut().insert(name.to_owned()),
            "shared fixture case {name} body consumed twice"
        );
        consumed.borrow_mut().insert(name.to_owned());
        fixture_case(name)
    };
    let event = |name: &str| -> Value {
        assert!(
            event_reads.borrow_mut().insert(name.to_owned()),
            "shared fixture case {name} event consumed twice"
        );
        consumed.borrow_mut().insert(name.to_owned());
        fixture_event(name)
    };

    let (_directory, path, store, events) = seeded().await;
    let human = router(store.reactions(), store.reactions(), events.clone())
        .layer(Extension(UserId::parse("user-1").unwrap()));
    let mut db = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    db.execute(sqlx::raw_sql(
        "INSERT INTO conversations (id, workspace_id, placement_kind, roster_policy, title, title_key, visibility, created_at, updated_at) \
         VALUES ('conversation-other', 'workspace-1', 'workspace', 'open', 'Other', 'other', 'private', 0, 0); \
         INSERT INTO conversation_agents (conversation_id, agent_id, joined_at) VALUES ('conversation-other', 'agent-1', 0);"
    )).await.unwrap();

    for (name, method, uri, body) in [
        (
            "human_add",
            Method::POST,
            "/api/messages/message-1/reactions",
            json!({"emoji":"👀"}),
        ),
        (
            "human_duplicate",
            Method::POST,
            "/api/messages/message-1/reactions",
            json!({"emoji":"👀"}),
        ),
        (
            "human_remove",
            Method::DELETE,
            "/api/messages/message-1/reactions",
            json!({"emoji":"👀"}),
        ),
        (
            "invalid_reaction",
            Method::POST,
            "/api/messages/message-1/reactions",
            json!({"emoji":"not emoji"}),
        ),
        (
            "missing_message",
            Method::POST,
            "/api/messages/missing/reactions",
            json!({"emoji":"👍"}),
        ),
        (
            "system_target",
            Method::POST,
            "/api/messages/system-1/reactions",
            json!({"emoji":"👍"}),
        ),
    ] {
        assert_eq!(
            call(human.clone(), method, uri, body).await,
            case(name),
            "{name}"
        );
    }
    // NOTE: the human admission faults (malformed JSON, missing/non-string `emoji`) are NOT
    // consumed from the shared TS-signed fixture. The TS route returns `400` with a Zod
    // `issues[]` report; the Rust route routes the body through the shared
    // `ValidatedJson<HumanReactRaw>` admission, so a structural fault keeps axum's native
    // rejection and no validator report is serialized. That accepted-set deviation is pinned
    // by `human_structural_admission_pins_exact_native_status_without_a_validator_report`
    // below, which asserts the exact native status + zero handler reach without pinning
    // native wording.
    let first = events.0.lock().unwrap()[0].clone();
    assert_eq!(
        serde_json::to_value(wire::changed_event(&first)).unwrap(),
        event("human_add")
    );
    sqlx::query("UPDATE conversations SET archived_at=1 WHERE id='conversation-1'")
        .execute(&mut db)
        .await
        .unwrap();
    assert_eq!(
        call(
            human.clone(),
            Method::POST,
            "/api/messages/message-1/reactions",
            json!({"emoji":"👍"})
        )
        .await,
        case("archived_conversation")
    );
    sqlx::query("UPDATE conversations SET archived_at=NULL WHERE id='conversation-1'")
        .execute(&mut db)
        .await
        .unwrap();

    sqlx::query("UPDATE users SET display_name='Local Display' WHERE id='user-1'")
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::query("UPDATE agents SET name='reaction-agent', display_name='' WHERE id='agent-1'")
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO messages \
         (id, conversation_id, from_kind, from_user_id, content, seq, created_at) \
         VALUES ('detail-message', 'conversation-1', 'user', 'user-1', 'detail', 4, 4)",
    )
    .execute(&mut db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO message_reactions \
         (id, message_id, reactor_kind, reactor_id, emoji, created_at) VALUES \
         ('detail-user-one', 'detail-message', 'user', 'user-1', '👍', 10), \
         ('detail-agent-one', 'detail-message', 'agent', 'agent-1', '👍', 11), \
         ('detail-user-two', 'detail-message', 'user', 'user-1', '🎯', 12)",
    )
    .execute(&mut db)
    .await
    .unwrap();
    assert_eq!(
        call(
            human.clone(),
            Method::GET,
            "/api/messages/detail-message/reactions",
            Value::Null,
        )
        .await,
        case("human_detail")
    );
    assert_eq!(
        call(
            human.clone(),
            Method::GET,
            "/api/messages/missing/reactions",
            Value::Null,
        )
        .await,
        case("human_detail_missing")
    );
    let invisible = router(store.reactions(), store.reactions(), events.clone())
        .layer(Extension(UserId::parse("user-2").unwrap()));
    assert_eq!(
        call(
            invisible,
            Method::GET,
            "/api/messages/message-1/reactions",
            Value::Null,
        )
        .await,
        case("human_detail_invisible")
    );

    let agent = router(store.reactions(), store.reactions(), events.clone())
        .layer(Extension(runtime_principal("conversation-1")));
    // NOTE: an empty `message_id` is NOT consumed from the shared fixture. The TS route
    // returns `400` with a Zod `too_small` issue; the Rust route decodes `message_id` as a
    // plain string and lets `MessageId::parse` reject the empty value into the minimal
    // semantic `422 {"error":"invalid input"}` — no validator report. Pinned by
    // `agent_empty_message_id_is_the_minimal_semantic_422_without_touching_the_store` below.
    assert_eq!(
        call(
            agent.clone(),
            Method::POST,
            "/internal/agents/agent-1/react-message",
            json!({"message_id":"message-1","emoji":"not emoji"}),
        )
        .await,
        case("agent_invalid_reaction")
    );
    assert_eq!(
        call(
            agent.clone(),
            Method::POST,
            "/internal/agents/agent-1/react-message",
            json!({"message_id":"message-1","emoji":"🎯"})
        )
        .await,
        case("agent_add")
    );
    assert_eq!(
        call(
            agent.clone(),
            Method::DELETE,
            "/internal/agents/agent-1/react-message",
            json!({"message_id":"message-1","emoji":"🎯"}),
        )
        .await,
        case("agent_remove")
    );
    assert_eq!(
        call(
            agent.clone(),
            Method::DELETE,
            "/internal/agents/agent-1/react-message",
            json!({"message_id":"message-1","emoji":"🎯"}),
        )
        .await,
        case("agent_remove_idempotent")
    );
    let agent_remove = events
        .0
        .lock()
        .unwrap()
        .iter()
        .find(|fact| {
            fact.reactor().kind() == "agent"
                && fact.change()
                    == silverhand_backend_core::conversation_messaging::messaging::collaboration::reaction::ReactionChange::Remove
        })
        .unwrap()
        .clone();
    assert_eq!(
        serde_json::to_value(wire::changed_event(&agent_remove)).unwrap(),
        event("agent_remove")
    );
    let cross_event_count = events.0.lock().unwrap().len();
    let cross_row_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM message_reactions")
        .fetch_one(&mut db)
        .await
        .unwrap();
    let wrong = router(store.reactions(), store.reactions(), events.clone())
        .layer(Extension(runtime_principal("conversation-other")));
    assert_eq!(
        call(
            wrong.clone(),
            Method::POST,
            "/internal/agents/agent-1/react-message",
            json!({"message_id":"message-1","emoji":"🚀"})
        )
        .await,
        case("agent_wrong_source")
    );
    assert_eq!(
        call(
            agent.clone(),
            Method::POST,
            "/internal/agents/agent-1/react-message",
            json!({"message_id":"missing","emoji":"not emoji"}),
        )
        .await,
        case("agent_invalid_emoji_missing_target")
    );
    assert_eq!(
        call(
            wrong,
            Method::POST,
            "/internal/agents/agent-1/react-message",
            json!({"message_id":"message-1","emoji":"not emoji"}),
        )
        .await,
        case("agent_invalid_emoji_wrong_source")
    );
    sqlx::query("DELETE FROM workspace_agent_capabilities WHERE workspace_id='workspace-1' AND agent_id='agent-1' AND capability='send_messages'").execute(&mut db).await.unwrap();
    let denied = router(store.reactions(), store.reactions(), events.clone())
        .layer(Extension(runtime_principal("conversation-1")));
    assert_eq!(
        call(
            denied,
            Method::POST,
            "/internal/agents/agent-1/react-message",
            json!({"message_id":"message-1","emoji":"🚀"})
        )
        .await,
        case("agent_capability_denied")
    );
    let denied = router(store.reactions(), store.reactions(), Events::default())
        .layer(Extension(runtime_principal("conversation-1")));
    assert_eq!(
        call(
            denied,
            Method::POST,
            "/internal/agents/agent-1/react-message",
            json!({"message_id":"message-1","emoji":"not emoji"}),
        )
        .await,
        case("agent_invalid_emoji_capability_denied")
    );
    assert_eq!(events.0.lock().unwrap().len(), cross_event_count);
    let rows_after_cross: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM message_reactions")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(rows_after_cross, cross_row_count);

    let fixture: Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../rust/test-fixtures/fixtures/reaction-wire.json"
    )))
    .unwrap();
    let all_names = fixture
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["name"].as_str().unwrap().to_owned())
        .collect::<std::collections::BTreeSet<_>>();

    // Mechanical completeness gate, driven by RUNTIME evidence. `consumed` was filled by the
    // `case`/`event` wrappers as each shared case was actually read above — there is no
    // separate hand-maintained name list. The four admission cases are the ONLY fixture cases
    // Rust deliberately does not consume (they diverge to native structural rejections pinned
    // by the resident `*_structural_admission_*` tests; TS keeps signing its own 400+issues
    // shape). If a shared case stops being consumed, its name never enters `consumed` and the
    // union below no longer equals `all_names`; if a fixture case is added or removed, the same
    // assertion fails — without anyone remembering the 24 names.
    let consumed = consumed.into_inner();
    let admission_deviations = std::collections::BTreeSet::from([
        "human_malformed_json".to_owned(),
        "human_missing_emoji".to_owned(),
        "human_non_string_emoji".to_owned(),
        "agent_empty_message_id".to_owned(),
    ]);
    assert!(
        consumed.is_disjoint(&admission_deviations),
        "a consumed shared case must not also be a named admission deviation"
    );
    let accounted = consumed
        .union(&admission_deviations)
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        accounted, all_names,
        "every fixture case must be either a consumed shared case or a named admission deviation"
    );
}

#[tokio::test]
async fn direct_pair_endpoints_write_without_roster_but_a_half_dead_pair_is_read_only() {
    let (_directory, path, store, events) = seeded().await;
    let human = router(store.reactions(), store.reactions(), events.clone())
        .layer(Extension(UserId::parse("user-1").unwrap()));
    let (status, _) = call(
        human.clone(),
        Method::POST,
        "/api/messages/pair-message/reactions",
        json!({"emoji":"👍"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let agent = router(store.reactions(), store.reactions(), events.clone())
        .layer(Extension(runtime_principal("pair-1")));
    let (status, body) = call(
        agent,
        Method::POST,
        "/internal/agents/agent-1/react-message",
        json!({"message_id":"pair-message","emoji":"👍"}),
    )
    .await;
    assert_eq!(
        (status, body),
        (
            StatusCode::OK,
            json!({"changed":true,"emoji":"👍","count":2})
        )
    );

    let mut db = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    sqlx::query("UPDATE conversations SET endpoint_a_deleted_at=9 WHERE id='pair-1'")
        .execute(&mut db)
        .await
        .unwrap();
    let (status, body) = call(
        human.clone(),
        Method::POST,
        "/api/messages/pair-message/reactions",
        json!({"emoji":"🎯"}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], "forbidden");
    let (status, detail) = call(
        human,
        Method::GET,
        "/api/messages/pair-message/reactions",
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        detail["reactions"][0]["reactors"].as_array().unwrap().len(),
        2
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_real_mutation_driver_failure_is_safe_logged_once_and_publishes_nothing() {
    let (_directory, path, store, events) = seeded().await;
    let mut breaker = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    breaker
        .execute("DROP TABLE message_reactions")
        .await
        .unwrap();
    breaker.close().await.unwrap();
    let app = router(store.reactions(), store.reactions(), events.clone())
        .layer(Extension(UserId::parse("user-1").unwrap()));

    let logs = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .without_time()
        .with_target(false)
        .with_writer(logs.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let (status, body) = call(
        app,
        Method::POST,
        "/api/messages/message-1/reactions",
        json!({"emoji":"👍"}),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body, Value::String("Internal Server Error".into()));
    assert!(events.0.lock().unwrap().is_empty());
    let errors = logs
        .events()
        .into_iter()
        .filter(|event| event["level"] == "ERROR")
        .collect::<Vec<_>>();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0]["fields"]["message"], "reaction mutation failed");
    let chain = errors[0]["fields"]["error_chain"].as_str().unwrap();
    assert!(chain.contains("database operation failed"), "{chain}");
    assert!(
        chain.contains("no such table: message_reactions"),
        "{chain}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn malformed_persisted_reaction_data_is_a_safe_read_failure_with_its_source_chain() {
    let (_directory, path, store, events) = seeded().await;
    let mut db = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO message_reactions \
         (id, message_id, reactor_kind, reactor_id, emoji, created_at) \
         VALUES ('bad-reaction', 'message-1', 'user', 'user-1', 'not-emoji', 1)",
    )
    .execute(&mut db)
    .await
    .unwrap();
    db.close().await.unwrap();
    let app = router(store.reactions(), store.reactions(), events)
        .layer(Extension(UserId::parse("user-1").unwrap()));

    let logs = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .without_time()
        .with_target(false)
        .with_writer(logs.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let (status, body) = call(
        app,
        Method::GET,
        "/api/messages/message-1/reactions",
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body, Value::String("Internal Server Error".into()));
    let errors = logs
        .events()
        .into_iter()
        .filter(|event| event["level"] == "ERROR")
        .collect::<Vec<_>>();
    assert_eq!(errors.len(), 1);
    assert_eq!(
        errors[0]["fields"]["message"],
        "reaction detail read failed"
    );
    let chain = errors[0]["fields"]["error_chain"].as_str().unwrap();
    assert!(
        chain.contains("stored reaction emoji is outside the published grammar"),
        "{chain}"
    );
    assert!(!chain.contains("not-emoji"), "{chain}");
}

#[tokio::test(flavor = "current_thread")]
async fn publication_failure_is_audited_once_without_compensating_the_committed_reaction() {
    let (_directory, path, store, _events) = seeded().await;
    let app = router(store.reactions(), store.reactions(), FailingEvents)
        .layer(Extension(UserId::parse("user-1").unwrap()));
    let logs = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .without_time()
        .with_target(false)
        .with_writer(logs.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let (status, body) = call(
        app,
        Method::POST,
        "/api/messages/message-1/reactions",
        json!({"emoji":"👍"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"changed":true,"emoji":"👍","count":1}));
    let mut db = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM message_reactions \
         WHERE message_id='message-1' AND reactor_kind='user' \
         AND reactor_id='user-1' AND emoji='👍'",
    )
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(count, 1);
    let errors = logs
        .events()
        .into_iter()
        .filter(|event| event["level"] == "ERROR")
        .collect::<Vec<_>>();
    assert_eq!(errors.len(), 1);
    assert_eq!(
        errors[0]["fields"]["message"],
        "reaction publication failed"
    );
    let chain = errors[0]["fields"]["error_chain"].as_str().unwrap();
    assert!(
        chain.contains("the committed reaction could not be published"),
        "{chain}"
    );
    assert!(chain.contains("scope event hub failed"), "{chain}");
}

// ===========================================================================
// Accepted-set deviation from the shared TS-signed fixture (#2259).
//
// The TS routes decode with Zod and, on a structural body fault, return `400` with a
// serialized `issues[]` report. The Rust routes decode through the shared
// `ValidatedJson<{Human,Agent}ReactRaw>` admission, so a structural fault keeps axum's
// native rejection and NO validator report is ever serialized. These resident tests pin
// that deviation: native status + minimal semantic body (only where a semantic verdict
// applies) + handler/store zero-reach — never the native rejection wording, which is not a
// product contract. They replace the `human_malformed_json`, `human_missing_emoji`,
// `human_non_string_emoji`, and `agent_empty_message_id` cases that Rust no longer consumes
// from the fixture (TS keeps signing those from its own routes).
// ===========================================================================

/// A mutation + read port that fails the test if any admission-fault request reaches the
/// application layer. Its methods are never expected to run for a rejected body.
#[derive(Clone, Copy)]
struct NeverReached;

impl silverhand_backend_core::conversation_messaging::messaging::collaboration::reaction::ReactionMutationPort
    for NeverReached
{
    type Error = std::io::Error;
    async fn mutate(
        &self,
        _mutation: silverhand_backend_core::conversation_messaging::messaging::collaboration::reaction::ReactionMutation,
    ) -> Result<
        Result<
            silverhand_backend_core::conversation_messaging::messaging::collaboration::reaction::CommittedReaction,
            silverhand_backend_core::conversation_messaging::messaging::collaboration::reaction::ReactionMutationRefusal,
        >,
        Self::Error,
    > {
        panic!("a rejected admission body reached the reaction mutation store");
    }
}

impl silverhand_backend_core::conversation_messaging::messaging::collaboration::reaction::ReactionReadPort
    for NeverReached
{
    type Error = std::io::Error;
    async fn detail(
        &self,
        _viewer: &UserId,
        _message: &MessageId,
    ) -> Result<
        Result<
            Vec<silverhand_backend_core::conversation_messaging::messaging::collaboration::reaction::ReactionDetail>,
            silverhand_backend_core::conversation_messaging::messaging::collaboration::reaction::ReactionReadRefusal,
        >,
        Self::Error,
    > {
        panic!("a rejected admission body reached the reaction detail read");
    }
    async fn aggregates(
        &self,
        _viewer: &Reactor,
        _messages: &[MessageId],
    ) -> Result<
        Vec<(
            MessageId,
            Vec<silverhand_backend_core::conversation_messaging::messaging::collaboration::reaction::ReactionAggregate>,
        )>,
        Self::Error,
    > {
        panic!("a rejected admission body reached the reaction aggregate read");
    }
}

/// A never-called aftermath: a rejected body must publish nothing.
#[derive(Clone, Copy)]
struct NeverAnnounced;

impl ReactionAftermath for NeverAnnounced {
    async fn announce(
        &self,
        _fact: &ReactionChangedFact,
    ) -> Result<
        (),
        silverhand_backend_core::conversation_messaging::messaging::collaboration::reaction::ReactionPublicationFailure,
    >{
        panic!("a rejected admission body published a reaction-changed event");
    }
}

fn human_admission_app() -> axum::Router {
    router(NeverReached, NeverReached, NeverAnnounced)
        .layer(Extension(UserId::parse("user-1").unwrap()))
}

fn agent_admission_app() -> axum::Router {
    router(NeverReached, NeverReached, NeverAnnounced)
        .layer(Extension(runtime_principal("conversation-1")))
}

/// A structural fault must never leak a Zod-style `issues[]` report and (there being no
/// garde rule on either DTO) must never collapse to the minimal semantic envelope.
fn assert_no_validator_report(name: &str, value: &Value) {
    if let Some(object) = value.as_object() {
        assert!(
            !object.contains_key("issues"),
            "{name}: a Zod-style issues report leaked: {value}"
        );
        assert_ne!(
            value,
            &json!({ "error": "invalid input" }),
            "{name}: a structural fault must not collapse to the semantic body"
        );
    }
}

#[tokio::test]
async fn human_structural_admission_pins_exact_native_status_without_a_validator_report() {
    // Structural faults keep axum's native `Json` rejection. Status is an EXTERNAL
    // accepted-set contract, so it is pinned exactly (never the wording): malformed syntax is
    // 400; a data fault that decoded as JSON but violated the object-only floor / the DTO
    // (wrong type, missing required field, unknown field, array/scalar shape) is 422. Content
    // -type faults are covered separately below. The store/aftermath are never reached (the
    // panicking `NeverReached`/`NeverAnnounced` ports), and the body is never a serialized
    // validator report nor the minimal semantic envelope (there is no garde rule).
    for (name, body, expected) in [
        ("malformed_json", "{".to_owned(), StatusCode::BAD_REQUEST),
        (
            "missing_emoji",
            json!({}).to_string(),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "non_string_emoji",
            json!({ "emoji": 7 }).to_string(),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "unknown_field",
            json!({ "emoji": "👍", "surprise": 1 }).to_string(),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        // The array-shaped-body bypass the shared ObjectOnly floor closes.
        (
            "value_shaped_array",
            json!(["👍"]).to_string(),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "empty_array",
            json!([]).to_string(),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "top_level_scalar",
            json!(42).to_string(),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
    ] {
        let (status, value) = call_raw(
            human_admission_app(),
            Method::POST,
            "/api/messages/message-1/reactions",
            body,
        )
        .await;
        assert_eq!(status, expected, "{name}: native structural status");
        assert_no_validator_report(name, &value);
    }
}

#[tokio::test]
async fn human_content_type_faults_are_the_native_415() {
    // A missing or non-JSON content-type keeps axum's native 415; the store is never reached.
    for (name, content_type) in [("missing", None), ("wrong", Some("text/plain"))] {
        let (status, value) = call_with_content_type(
            human_admission_app(),
            Method::POST,
            "/api/messages/message-1/reactions",
            json!({ "emoji": "👍" }).to_string(),
            content_type,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "{name} content-type"
        );
        assert_no_validator_report(name, &value);
    }
}

#[tokio::test]
async fn agent_empty_message_id_is_the_minimal_semantic_422_without_touching_the_store() {
    // An empty `message_id` decodes structurally (it is a valid JSON string) but is a domain
    // verdict: `MessageId::parse` rejects it, and the route collapses that to the minimal
    // semantic `422 {"error":"invalid input"}` — no Zod `too_small` issue, and the store is
    // never reached (proved by the panicking `NeverReached` port).
    let (status, body) = call(
        agent_admission_app(),
        Method::POST,
        "/internal/agents/agent-1/react-message",
        json!({ "message_id": "", "emoji": "🎯" }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body, json!({ "error": "invalid input" }));
}

#[tokio::test]
async fn agent_structural_admission_pins_exact_native_status_without_a_validator_report() {
    for (name, body, expected) in [
        ("malformed_json", "{".to_owned(), StatusCode::BAD_REQUEST),
        (
            "missing_message_id",
            json!({ "emoji": "🎯" }).to_string(),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "missing_emoji",
            json!({ "message_id": "message-1" }).to_string(),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "non_string_message_id",
            json!({ "message_id": 7, "emoji": "🎯" }).to_string(),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "unknown_field",
            json!({ "message_id": "message-1", "emoji": "🎯", "surprise": 1 }).to_string(),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "value_shaped_array",
            json!(["message-1", "🎯"]).to_string(),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
    ] {
        let (status, value) = call_raw(
            agent_admission_app(),
            Method::POST,
            "/internal/agents/agent-1/react-message",
            body,
        )
        .await;
        assert_eq!(status, expected, "{name}: native structural status");
        assert_no_validator_report(name, &value);
    }
}

#[tokio::test]
async fn agent_content_type_faults_are_the_native_415() {
    for (name, content_type) in [("missing", None), ("wrong", Some("text/plain"))] {
        let (status, value) = call_with_content_type(
            agent_admission_app(),
            Method::POST,
            "/internal/agents/agent-1/react-message",
            json!({ "message_id": "message-1", "emoji": "🎯" }).to_string(),
            content_type,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "{name} content-type"
        );
        assert_no_validator_report(name, &value);
    }
}

#[tokio::test]
async fn agent_self_pin_refuses_a_mismatched_path_before_the_body_is_consulted() {
    // The path agent is not the authenticated principal: an indistinguishable 404 stands even
    // though the body would otherwise be a structural fault, proving identity is checked
    // before the body and the store is never reached.
    let (status, _body) = call_raw(
        agent_admission_app(),
        Method::POST,
        "/internal/agents/agent-999/react-message",
        "{".to_owned(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
