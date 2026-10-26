use std::error::Error;

use silverhand_backend_core::{
    conversation_messaging::direct_pair_read::{DirectPairCounterpart, DirectPairReadModel},
    conversation_messaging::messaging::{ActiveAuthor, PrincipalIdentity},
    foundation::identity::UserId,
    identity_access::agents::runtime::RuntimeKind,
};
use silverhand_backend_sqlite::conversation_messaging::direct_pair_read::{
    DirectPairReadError, SqliteDirectPairReadModel,
};
use sqlx::Connection;

use crate::current_schema;

async fn seeded(path: &std::path::Path) {
    seeded_scope(path, "agent").await
}

async fn seeded_scope(path: &std::path::Path, scope: &str) {
    let mut db = current_schema::create_current_schema(path).await;
    for statement in [
        "INSERT INTO users (id, name, display_name, avatar_storage_key, avatar_content_type, \
         created_at, updated_at) VALUES \
         ('user-1', 'viewer', 'Viewer', 'viewer-key', 'image/png', 1, 11)",
        "INSERT INTO users (id, name, display_name, created_at, updated_at) \
         VALUES ('user-2', 'other', 'Other Human', 2, 12)",
        "INSERT INTO users (id, name, display_name, created_at, updated_at) \
         VALUES ('user-3', 'foreign', 'Foreign', 3, 13)",
        "INSERT INTO hosts (id, name, kind, created_at) VALUES ('host-1', 'Host', 'local', 1)",
        "INSERT INTO agents (id, name, display_name, runtime, host_id, avatar_storage_key, \
         avatar_content_type, created_by_user_id, created_at, updated_at, session_scope) VALUES \
         ('agent-1', 'agent', 'Agent One', 'codex', 'host-1', 'agent-key', 'image/png', \
         'user-1', 1, 21, ?)",
        // Oldest, viewer in slot B, agent counterpart.
        "INSERT INTO conversations (id, placement_kind, roster_policy, title, status, \
         endpoint_a_kind, endpoint_a_id, endpoint_b_kind, endpoint_b_id, created_at, updated_at) \
         VALUES ('pair-agent', 'direct_pair', 'fixed', '', 'open', 'agent', 'agent-1', 'human', \
         'user-1', 100, 100)",
        // Same creation time as the next row; id is the deterministic tie-break.
        "INSERT INTO conversations (id, placement_kind, roster_policy, title, status, \
         endpoint_a_kind, endpoint_a_id, endpoint_b_kind, endpoint_b_id, created_at, updated_at) \
         VALUES ('pair-a-human', 'direct_pair', 'fixed', '', 'open', 'human', 'user-1', 'human', \
         'user-2', 200, 200)",
        // A second row at the same millisecond exercises the id tie-break.
        "INSERT INTO conversations (id, placement_kind, roster_policy, title, status, \
         endpoint_a_kind, endpoint_a_id, endpoint_b_kind, endpoint_b_id, created_at, updated_at) \
         VALUES ('pair-b-human', 'direct_pair', 'fixed', '', 'open', 'human', 'user-1', 'human', \
         'user-3', 200, 200)",
        // A foreign viewer's row must never leak onto user-1's complete face.
        "INSERT INTO conversations (id, placement_kind, roster_policy, title, status, \
         endpoint_a_kind, endpoint_a_id, endpoint_b_kind, endpoint_b_id, created_at, updated_at) \
         VALUES ('pair-foreign', 'direct_pair', 'fixed', '', 'open', 'agent', 'agent-1', 'human', \
         'user-3', 50, 50)",
        "INSERT INTO messages (id, conversation_id, from_kind, from_agent_id, content, seq, \
         created_at) VALUES ('message-1', 'pair-agent', 'agent', 'agent-1', 'one', 1, 1)",
        "INSERT INTO messages (id, conversation_id, from_kind, from_agent_id, content, seq, \
         created_at) VALUES ('message-2', 'pair-agent', 'agent', 'agent-1', 'two', 2, 2)",
    ] {
        let query = sqlx::query(statement);
        if statement.starts_with("INSERT INTO agents") {
            query.bind(scope).execute(&mut db).await.unwrap();
        } else {
            query.execute(&mut db).await.unwrap();
        }
    }
    db.close().await.unwrap();
}

fn viewer() -> UserId {
    UserId::parse("user-1").unwrap()
}

#[tokio::test]
async fn complete_list_is_endpoint_scoped_ordered_and_projects_both_counterpart_kinds() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("direct-pairs.sqlite");
    seeded(&path).await;
    let model = SqliteDirectPairReadModel::open(&path).await.unwrap();
    let list = model.list_direct_pairs(&viewer()).await.unwrap();

    assert_eq!(
        list.items()
            .iter()
            .map(|item| item.conversation().as_str())
            .collect::<Vec<_>>(),
        ["pair-agent", "pair-a-human", "pair-b-human"]
    );
    let agent = &list.items()[0];
    assert_eq!(agent.unread_count().get(), 2);
    assert!(!agent.is_read_only());
    assert_eq!(agent.created_at_ms(), 100);
    let DirectPairCounterpart::Active(ActiveAuthor::Agent(agent)) = agent.counterpart() else {
        panic!("the agent pair must project an active agent counterpart")
    };
    assert_eq!(agent.id().as_str(), "agent-1");
    assert_eq!(agent.name(), "agent");
    assert_eq!(agent.display_name(), "Agent One");
    assert_eq!(agent.runtime(), RuntimeKind::Codex);
    assert_eq!(agent.avatar().unwrap().cache_buster(), 21);

    for (human_pair, expected) in list.items()[1..].iter().zip(["user-2", "user-3"]) {
        let DirectPairCounterpart::Active(ActiveAuthor::Human(human)) = human_pair.counterpart()
        else {
            panic!("both slot orders must project the other human")
        };
        assert_eq!(human.id().as_str(), expected);
    }
}

#[tokio::test]
async fn the_complete_face_has_no_hidden_page_or_activity_cap() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("direct-pair-complete.sqlite");
    let mut db = current_schema::create_current_schema(&path).await;
    for statement in [
        "INSERT INTO users (id, name, display_name, created_at, updated_at) \
         VALUES ('user-1', 'viewer', 'Viewer', 1, 1)",
        "INSERT INTO hosts (id, name, kind, created_at) VALUES ('host-1', 'Host', 'local', 1)",
    ] {
        sqlx::query(statement).execute(&mut db).await.unwrap();
    }
    for index in 0..120 {
        let agent = format!("agent-{index:03}");
        let pair = format!("pair-{index:03}");
        sqlx::query(
            "INSERT INTO agents (id, name, display_name, runtime, host_id, created_by_user_id, \
             created_at, updated_at) VALUES (?, ?, ?, 'claude', 'host-1', 'user-1', ?, ?)",
        )
        .bind(&agent)
        .bind(&agent)
        .bind(&agent)
        .bind(index)
        .bind(index)
        .execute(&mut db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO conversations (id, placement_kind, roster_policy, title, status, \
             endpoint_a_kind, endpoint_a_id, endpoint_b_kind, endpoint_b_id, created_at, \
             updated_at) VALUES (?, 'direct_pair', 'fixed', '', 'open', 'agent', ?, 'human', \
             'user-1', ?, ?)",
        )
        .bind(&pair)
        .bind(&agent)
        .bind(index)
        .bind(index)
        .execute(&mut db)
        .await
        .unwrap();
    }
    db.close().await.unwrap();

    let list = SqliteDirectPairReadModel::open(&path)
        .await
        .unwrap()
        .list_direct_pairs(&viewer())
        .await
        .unwrap();
    assert_eq!(list.items().len(), 120);
    assert_eq!(
        list.items().first().unwrap().conversation().as_str(),
        "pair-000"
    );
    assert_eq!(
        list.items().last().unwrap().conversation().as_str(),
        "pair-119"
    );
}

#[tokio::test]
async fn read_cursor_clears_the_canonical_rollup_without_removing_the_pair() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("direct-pair-unread.sqlite");
    seeded(&path).await;
    let model = SqliteDirectPairReadModel::open(&path).await.unwrap();
    assert_eq!(
        model.list_direct_pairs(&viewer()).await.unwrap().items()[0]
            .unread_count()
            .get(),
        2
    );

    let mut db = sqlx::SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO conversation_reads (user_id, conversation_id, thread_root_id, last_read_seq, \
         read_at) VALUES ('user-1', 'pair-agent', NULL, 2, 3)",
    )
    .execute(&mut db)
    .await
    .unwrap();
    db.close().await.unwrap();

    let list = model.list_direct_pairs(&viewer()).await.unwrap();
    assert_eq!(list.items()[0].conversation().as_str(), "pair-agent");
    assert_eq!(list.items()[0].unread_count().get(), 0);
}

#[tokio::test]
async fn a_gone_counterpart_remains_as_a_typed_read_only_row() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("direct-pair-deleted.sqlite");
    seeded(&path).await;
    let mut db = sqlx::SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    sqlx::query("DELETE FROM agents WHERE id = 'agent-1'")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();

    let list = SqliteDirectPairReadModel::open(&path)
        .await
        .unwrap()
        .list_direct_pairs(&viewer())
        .await
        .unwrap();
    let row = &list.items()[0];
    assert!(row.is_read_only());
    assert!(matches!(
        row.counterpart(),
        DirectPairCounterpart::Deleted(PrincipalIdentity::Agent(id)) if id.as_str() == "agent-1"
    ));
}

#[tokio::test]
async fn malformed_endpoint_and_driver_failures_preserve_their_distinct_sources() {
    // Decode failure: preserve a row that matches the viewer through slot A, but forge
    // the other slot outside the closed kind set while constraints are deliberately
    // disabled for this corruption fixture.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("direct-pair-corrupt.sqlite");
    let mut db = current_schema::create_current_schema(&path).await;
    sqlx::query("PRAGMA ignore_check_constraints = ON")
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::query("DROP TRIGGER trg_conversations_endpoints_birth")
        .execute(&mut db)
        .await
        .unwrap();
    for statement in [
        "INSERT INTO users (id, name, display_name, created_at, updated_at) \
         VALUES ('user-1', 'viewer', 'Viewer', 1, 1)",
        "INSERT INTO conversations (id, placement_kind, roster_policy, title, status, \
         endpoint_a_kind, endpoint_a_id, endpoint_b_kind, endpoint_b_id, created_at, updated_at) \
         VALUES ('pair-corrupt', 'direct_pair', 'fixed', '', 'open', 'human', 'user-1', \
         'machine', 'x', 1, 1)",
    ] {
        sqlx::query(statement).execute(&mut db).await.unwrap();
    }
    db.close().await.unwrap();
    let error = SqliteDirectPairReadModel::open(&path)
        .await
        .unwrap()
        .list_direct_pairs(&viewer())
        .await
        .unwrap_err();
    assert!(matches!(
        Error::source(&error).and_then(|source| source.downcast_ref()),
        Some(DirectPairReadError::UnknownEndpointKind)
    ));

    // The kind can be valid while its persisted coordinate is not. This must stay a
    // decode failure rather than become a deleted counterpart with an unusable id.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("direct-pair-corrupt-id.sqlite");
    let mut db = current_schema::create_current_schema(&path).await;
    sqlx::query("PRAGMA ignore_check_constraints = ON")
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::query("DROP TRIGGER trg_conversations_endpoints_birth")
        .execute(&mut db)
        .await
        .unwrap();
    for statement in [
        "INSERT INTO users (id, name, display_name, created_at, updated_at) \
         VALUES ('user-1', 'viewer', 'Viewer', 1, 1)",
        "INSERT INTO conversations (id, placement_kind, roster_policy, title, status, \
         endpoint_a_kind, endpoint_a_id, endpoint_b_kind, endpoint_b_id, created_at, updated_at) \
         VALUES ('pair-corrupt', 'direct_pair', 'fixed', '', 'open', 'human', '', \
         'human', 'user-1', 1, 1)",
    ] {
        sqlx::query(statement).execute(&mut db).await.unwrap();
    }
    db.close().await.unwrap();
    let error = SqliteDirectPairReadModel::open(&path)
        .await
        .unwrap()
        .list_direct_pairs(&viewer())
        .await
        .unwrap_err();
    assert!(matches!(
        Error::source(&error).and_then(|source| source.downcast_ref()),
        Some(DirectPairReadError::MalformedIdentity { .. })
    ));

    // Driver failure reaches the same public port with its sqlx cause still below it.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("direct-pair-driver.sqlite");
    seeded(&path).await;
    let model = SqliteDirectPairReadModel::open(&path).await.unwrap();
    let mut db = sqlx::SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    sqlx::query("DROP TABLE conversations")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    let error = model.list_direct_pairs(&viewer()).await.unwrap_err();
    let source = Error::source(&error).unwrap();
    let read_error = source.downcast_ref::<DirectPairReadError>().unwrap();
    assert!(matches!(read_error, DirectPairReadError::Sql(_)));
    assert!(Error::source(read_error).is_some());
}

/// `agents.runtime` carries no CHECK, so a token outside the closed set is storable,
/// and this face renders the counterpart's runtime straight onto the wire. Passing an
/// unknown token through would answer `200` with a value the wire enum cannot name,
/// so the whole read fails; the repair afterwards is projected as ITSELF, which is
/// what rules out a coerced default hiding behind the same green.
#[tokio::test]
async fn an_unknown_stored_runtime_fails_the_read_until_it_is_repaired() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("direct-pair-runtime.sqlite");
    seeded(&path).await;
    let model = SqliteDirectPairReadModel::open(&path).await.unwrap();

    let mut db = sqlx::SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    sqlx::query("UPDATE agents SET runtime = 'aider' WHERE id = 'agent-1'")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    let error = model.list_direct_pairs(&viewer()).await.unwrap_err();
    assert!(matches!(
        Error::source(&error).and_then(|source| source.downcast_ref()),
        Some(DirectPairReadError::UnknownRuntime)
    ));

    let mut db = sqlx::SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    sqlx::query("UPDATE agents SET runtime = 'cursor' WHERE id = 'agent-1'")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    let list = model.list_direct_pairs(&viewer()).await.unwrap();
    let DirectPairCounterpart::Active(ActiveAuthor::Agent(agent)) = list.items()[0].counterpart()
    else {
        panic!("the repaired row must project an active agent counterpart")
    };
    assert_eq!(agent.runtime(), RuntimeKind::Cursor);
}

#[tokio::test]
async fn conversation_scoped_pair_keeps_history_and_counterpart_but_is_read_only() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("direct-pairs.sqlite");
    seeded_scope(&path, "conversation").await;
    let model = SqliteDirectPairReadModel::open(&path).await.unwrap();
    let list = model.list_direct_pairs(&viewer()).await.unwrap();
    let pair = &list.items()[0];
    assert!(pair.is_read_only());
    assert_eq!(pair.unread_count().get(), 2);
    assert!(matches!(
        pair.counterpart(),
        DirectPairCounterpart::Active(_)
    ));
    assert!(!list.items()[1].is_read_only());
}
