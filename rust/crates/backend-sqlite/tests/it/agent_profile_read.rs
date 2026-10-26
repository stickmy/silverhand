//! Integration tests for the narrow internal agent-profile cold-read SQL
//! (#task:2226, CP3).
//!
//! These pin that the adapter's SQL resolves the narrow fact correctly from real
//! committed rows, and — the point Cindy asked for — that the fact's `id` and
//! `name` are SOURCED FROM THE RETURNED ROW, not assumed from the bound query
//! coordinate. Two folds are exercised:
//!
//!  * source-binding — the fact carries the row's own `id` (a peer read will bind
//!    by handle, so the returned id must be the target's real id) and `name` as a
//!    committed source witness.
//!  * the avatar fold — `avatar_storage_key` + `updated_at` collapse to
//!    `Option<AvatarRevision>` by truthiness (empty/NULL → none), carrying this
//!    row's own `updated_at` as the cache-buster and never the storage locator.

use silverhand_backend_core::{
    conversation_messaging::messaging::AvatarRevision,
    foundation::identity::AgentId,
    foundation::persistence::PersistenceFailure,
    identity_access::agent_profile_read::{AgentProfileFact, AgentProfileReadModel},
};
use silverhand_backend_sqlite::identity_access::agent_profile_read::SqliteAgentProfileRead;
use sqlx::Connection;

use crate::current_schema;

fn agent(id: &str) -> AgentId {
    AgentId::parse(id).expect("agent id")
}

/// Create a fresh current-schema database, run every seed statement, and open the
/// adapter over it. The `TempDir` guard is returned so the file outlives the pool.
async fn seeded(statements: &[&'static str]) -> (tempfile::TempDir, SqliteAgentProfileRead) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("agent-profile-read.sqlite");
    let mut db = current_schema::create_current_schema(&path).await;
    for statement in statements {
        sqlx::query(*statement).execute(&mut db).await.unwrap();
    }
    db.close().await.unwrap();
    let cold_read = SqliteAgentProfileRead::open(&path).await.unwrap();
    (directory, cold_read)
}

async fn profile_of(cold_read: &SqliteAgentProfileRead, id: &str) -> Option<AgentProfileFact> {
    cold_read.read_agent_profile(&agent(id)).await.unwrap()
}

async fn profile_by_handle(
    cold_read: &SqliteAgentProfileRead,
    handle: &str,
) -> Option<AgentProfileFact> {
    cold_read
        .read_agent_profile_by_handle(handle)
        .await
        .unwrap()
}

async fn try_profile(
    cold_read: &SqliteAgentProfileRead,
    id: &str,
) -> Result<Option<AgentProfileFact>, PersistenceFailure> {
    cold_read.read_agent_profile(&agent(id)).await
}

/// The host and creator the seeded agents point at — `agents.host_id` and
/// `agents.created_by_user_id` both carry FKs, and the avatar columns carry a
/// paired CHECK (`storage_key` and `content_type` are both NULL or both set), so
/// every seed below honours all three.
const HOST: &str =
    "INSERT INTO hosts (id, name, kind, created_at) VALUES ('h1', 'host-one', 'desktop', 1000)";
const CREATOR: &str = "INSERT INTO users (id, name, display_name, created_at, updated_at) \
     VALUES ('u-creator', 'creator', 'Creator', 1000, 1000)";

#[tokio::test]
async fn a_present_row_projects_the_narrow_fact_from_its_own_columns() {
    let (_dir, cold_read) = seeded(&[
        HOST,
        CREATOR,
        "INSERT INTO agents (id, name, display_name, runtime, host_id, created_at, updated_at, \
             created_by_user_id, description, avatar_storage_key, avatar_content_type) \
         VALUES ('a1', 'agent-one', 'Agent One', 'qoder', 'h1', 1000, 2500, 'u-creator', \
             'a bio', 'avatars/a1.png', 'image/png')",
    ])
    .await;

    let fact = profile_of(&cold_read, "a1").await.expect("row present");
    assert_eq!(
        fact,
        AgentProfileFact {
            id: agent("a1"),
            name: "agent-one".to_string(),
            display_name: "Agent One".to_string(),
            description: Some("a bio".to_string()),
            // A non-empty key folds to a revision carrying THIS row's updated_at,
            // never the storage key itself.
            avatar: Some(AvatarRevision::at(2500)),
        }
    );
}

#[tokio::test]
async fn distinct_rows_keep_their_own_name_and_avatar_revision() {
    // Two rows with distinct updated_at and names. Reading each returns its OWN id,
    // name and cache-buster — so the fact is bound to the row the adapter returned,
    // the invariant a by-handle peer read will rely on.
    let (_dir, cold_read) = seeded(&[
        HOST,
        CREATOR,
        "INSERT INTO agents (id, name, display_name, runtime, host_id, created_at, updated_at, \
             created_by_user_id, avatar_storage_key, avatar_content_type) \
         VALUES ('a1', 'first', 'First', 'qoder', 'h1', 1000, 1111, 'u-creator', 'avatars/a1.png', 'image/png')",
        "INSERT INTO agents (id, name, display_name, runtime, host_id, created_at, updated_at, \
             created_by_user_id, avatar_storage_key, avatar_content_type) \
         VALUES ('a2', 'second', 'Second', 'qoder', 'h1', 1000, 2222, 'u-creator', 'avatars/a2.png', 'image/png')",
    ])
    .await;

    let one = profile_of(&cold_read, "a1").await.expect("a1 present");
    assert_eq!(one.id, agent("a1"));
    assert_eq!(one.name, "first");
    assert_eq!(one.avatar, Some(AvatarRevision::at(1111)));

    let two = profile_of(&cold_read, "a2").await.expect("a2 present");
    assert_eq!(two.id, agent("a2"));
    assert_eq!(two.name, "second");
    assert_eq!(two.avatar, Some(AvatarRevision::at(2222)));
}

#[tokio::test]
async fn an_empty_or_absent_avatar_key_is_no_avatar() {
    let (_dir, cold_read) = seeded(&[
        HOST,
        CREATOR,
        // NULL avatar (both avatar columns omitted → the paired-NULL arm).
        "INSERT INTO agents (id, name, display_name, runtime, host_id, created_at, updated_at, \
             created_by_user_id) \
         VALUES ('none', 'no-avatar', 'No Avatar', 'qoder', 'h1', 1000, 3000, 'u-creator')",
        // Empty-string avatar key (paired with a content type to satisfy the CHECK) —
        // truthiness, not IS NOT NULL, so still no avatar.
        "INSERT INTO agents (id, name, display_name, runtime, host_id, created_at, updated_at, \
             created_by_user_id, avatar_storage_key, avatar_content_type) \
         VALUES ('empty', 'empty-key', 'Empty Key', 'qoder', 'h1', 1000, 3000, 'u-creator', '', 'image/png')",
    ])
    .await;

    assert_eq!(profile_of(&cold_read, "none").await.unwrap().avatar, None);
    assert_eq!(profile_of(&cold_read, "empty").await.unwrap().avatar, None);
    // description absent → None (the DTO's `null`).
    assert_eq!(
        profile_of(&cold_read, "none").await.unwrap().description,
        None
    );
}

#[tokio::test]
async fn an_absent_row_is_none_not_a_failure() {
    let (_dir, cold_read) = seeded(&[]).await;
    assert!(try_profile(&cold_read, "ghost").await.unwrap().is_none());
}

#[tokio::test]
async fn by_handle_resolves_the_row_and_matches_name_exactly() {
    let (_dir, cold_read) = seeded(&[
        HOST,
        CREATOR,
        "INSERT INTO agents (id, name, display_name, runtime, host_id, created_at, updated_at, \
             created_by_user_id, avatar_storage_key, avatar_content_type) \
         VALUES ('a1', 'agent-one', 'Agent One', 'qoder', 'h1', 1000, 2500, 'u-creator', \
             'avatars/a1.png', 'image/png')",
    ])
    .await;

    let fact = profile_by_handle(&cold_read, "agent-one")
        .await
        .expect("handle resolves");
    assert_eq!(fact.id, agent("a1"));
    assert_eq!(fact.name, "agent-one");
    assert_eq!(fact.avatar, Some(AvatarRevision::at(2500)));

    // Exact match, no COLLATE: a different case is a different handle, so the
    // adapter returns nothing rather than the row — the invariant the peer
    // source-binding relies on.
    assert!(profile_by_handle(&cold_read, "Agent-One").await.is_none());
    assert!(profile_by_handle(&cold_read, "missing").await.is_none());
}
