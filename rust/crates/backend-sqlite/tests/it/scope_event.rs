use crate::current_schema;

use silverhand_backend_core::{
    foundation::identity::{ConversationId, UserId, WorkspaceId},
    foundation::scope_event::{AuthorizeEventScope, EventScope},
};
use silverhand_backend_sqlite::foundation::scope_event::SqliteEventScopeAuthority;
use sqlx::Connection;
use std::error::Error;

const USER: &str = "user-1";
const OTHER: &str = "user-2";
const WORKSPACE: &str = "workspace-1";
const PRIVATE: &str = "private-1";
const PUBLIC: &str = "public-1";
const FIXED: &str = "fixed-1";
const PAIR: &str = "pair-1";

async fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("scope-event.sqlite");
    let mut db = current_schema::create_current_schema(&path).await;
    for (id, name) in [(USER, "user-one"), (OTHER, "user-two")] {
        sqlx::query(
            "INSERT INTO users (id, name, display_name, created_at, updated_at) VALUES (?, ?, ?, 1, 1)",
        )
        .bind(id)
        .bind(name)
        .bind(name)
        .execute(&mut db)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO workspaces (id, name, name_key, owner_user_id, created_at, updated_at) VALUES (?, 'Space', 'space', ?, 1, 1)",
    )
    .bind(WORKSPACE)
    .bind(USER)
    .execute(&mut db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO workspace_users (workspace_id, user_id, joined_at, role) VALUES (?, ?, 1, 'owner')",
    )
    .bind(WORKSPACE)
    .bind(USER)
    .execute(&mut db)
    .await
    .unwrap();
    for (id, title, key, visibility) in [
        (PRIVATE, "Private", "private", "private"),
        (PUBLIC, "Public", "public", "public"),
    ] {
        sqlx::query(
            "INSERT INTO conversations (id, workspace_id, placement_kind, roster_policy, title, title_key, status, visibility, created_at, updated_at) VALUES (?, ?, 'workspace', 'open', ?, ?, 'open', ?, 1, 1)",
        )
        .bind(id)
        .bind(WORKSPACE)
        .bind(title)
        .bind(key)
        .bind(visibility)
        .execute(&mut db)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO conversation_users (conversation_id, user_id, joined_at) VALUES (?, ?, 1)",
    )
    .bind(PRIVATE)
    .bind(USER)
    .execute(&mut db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversations (id, workspace_id, placement_kind, roster_policy, title, title_key, status, visibility, created_at, updated_at) VALUES (?, ?, 'workspace', 'fixed', 'Fixed', 'fixed', 'open', 'private', 1, 1)",
    )
    .bind(FIXED)
    .bind(WORKSPACE)
    .execute(&mut db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversation_users (conversation_id, user_id, joined_at) VALUES (?, ?, 1)",
    )
    .bind(FIXED)
    .bind(USER)
    .execute(&mut db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversations (id, workspace_id, placement_kind, roster_policy, endpoint_a_kind, endpoint_a_id, endpoint_b_kind, endpoint_b_id, title, status, visibility, created_at, updated_at) VALUES (?, NULL, 'direct_pair', 'fixed', 'human', ?, 'human', ?, '', 'idle', 'private', 1, 1)",
    )
    .bind(PAIR)
    .bind(USER)
    .bind(OTHER)
    .execute(&mut db)
    .await
    .unwrap();
    db.close().await.unwrap();
    (directory, path)
}

fn user() -> UserId {
    UserId::parse(USER).unwrap()
}

#[tokio::test]
async fn real_scope_authority_preserves_each_public_scope_rule() {
    let (_directory, path) = fixture().await;
    let authority = SqliteEventScopeAuthority::open(&path).await.unwrap();
    let cases = [
        (EventScope::App, true),
        (EventScope::Viewer(user()), true),
        (EventScope::Viewer(UserId::parse(OTHER).unwrap()), false),
        (
            EventScope::Workspace(WorkspaceId::parse(WORKSPACE).unwrap()),
            true,
        ),
        (
            EventScope::Conversation(ConversationId::parse(PRIVATE).unwrap()),
            true,
        ),
        (
            EventScope::ConversationSide(ConversationId::parse(PRIVATE).unwrap()),
            true,
        ),
        (
            EventScope::Conversation(ConversationId::parse(PUBLIC).unwrap()),
            true,
        ),
        (
            EventScope::ConversationSide(ConversationId::parse(PUBLIC).unwrap()),
            false,
        ),
        (
            EventScope::Conversation(ConversationId::parse(FIXED).unwrap()),
            true,
        ),
        (
            EventScope::ConversationSide(ConversationId::parse(FIXED).unwrap()),
            true,
        ),
        (
            EventScope::Conversation(ConversationId::parse(PAIR).unwrap()),
            true,
        ),
    ];
    for (scope, expected) in cases {
        assert_eq!(
            authority
                .authorize_event_scope(&user(), &scope)
                .await
                .unwrap(),
            expected,
            "{scope:?}"
        );
    }
}

#[tokio::test]
async fn fixed_workspace_conversation_requires_explicit_membership() {
    let (_directory, path) = fixture().await;
    let authority = SqliteEventScopeAuthority::open(&path).await.unwrap();
    let mut db = sqlx::SqliteConnection::connect(&format!("sqlite://{}", path.display()))
        .await
        .unwrap();
    sqlx::query("DELETE FROM conversation_users WHERE conversation_id = ? AND user_id = ?")
        .bind(FIXED)
        .bind(USER)
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();

    for scope in [
        EventScope::Conversation(ConversationId::parse(FIXED).unwrap()),
        EventScope::ConversationSide(ConversationId::parse(FIXED).unwrap()),
    ] {
        assert!(
            !authority
                .authorize_event_scope(&user(), &scope)
                .await
                .unwrap()
        );
    }
}

#[tokio::test]
async fn driver_and_persisted_coordinate_failures_are_not_denials() {
    let (_directory, path) = fixture().await;
    let authority = SqliteEventScopeAuthority::open(&path).await.unwrap();
    let mut db = sqlx::SqliteConnection::connect(&format!("sqlite://{}", path.display()))
        .await
        .unwrap();
    sqlx::query("PRAGMA ignore_check_constraints = ON")
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::query("DROP TRIGGER conversations_placement_immutable")
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::query("UPDATE conversations SET placement_kind = 'broken' WHERE id = ?")
        .bind(PRIVATE)
        .execute(&mut db)
        .await
        .unwrap();
    let failure = authority
        .authorize_event_scope(
            &user(),
            &EventScope::Conversation(ConversationId::parse(PRIVATE).unwrap()),
        )
        .await
        .unwrap_err();
    assert!(root_cause(&failure).contains("persisted conversation placement is invalid"));

    sqlx::query("DROP TABLE workspace_users")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    let failure = authority
        .authorize_event_scope(
            &user(),
            &EventScope::Workspace(WorkspaceId::parse(WORKSPACE).unwrap()),
        )
        .await
        .unwrap_err();
    assert!(root_cause(&failure).contains("no such table: workspace_users"));
}

fn root_cause(error: &(dyn Error + 'static)) -> String {
    let mut current = error;
    while let Some(source) = current.source() {
        current = source;
    }
    current.to_string()
}
