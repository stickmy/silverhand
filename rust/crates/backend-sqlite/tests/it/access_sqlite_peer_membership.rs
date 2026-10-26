//! Production SQLite peer-target membership against the real, checked-in current
//! schema (#task:2226).
//!
//! The peer-profile route (`GET /internal/agents/{aid}/peer-profile`) gates its
//! target on whether the target agent is a member of the caller's workspace. That
//! edge is [`PeerTargetMembership`], implemented on the canonical
//! [`SqliteAccessStore`] — the same store that WRITES `workspace_members` — so no
//! second membership model exists to drift from `workspaces.isMember`.
//!
//! The load-bearing discriminator these tests pin: membership is the REGULAR
//! `workspace_members` edge only. An agent's private home is a workspace whose
//! `owner_agent_id` is that agent; the caller's workspace horizon may let it see
//! the home, but the agent is NOT a `workspace_members` member of it (the schema's
//! `workspace_members_no_agent_home_insert` trigger forbids such a row outright),
//! so the port must answer `false` for the home and `true` only where a real
//! regular edge exists.

use silverhand_backend_core::foundation::identity::{AgentId, WorkspaceId};
use silverhand_backend_core::identity_access::agent_peer_profile::PeerTargetMembership;
use silverhand_backend_sqlite::identity_access::access::SqliteAccessStore;
use sqlx::Connection;
use tempfile::TempDir;

use crate::current_schema;

const TARGET: &str = "agent-target";
const REGULAR_WS: &str = "ws-regular";
const HOME_WS: &str = "ws-home";

fn agent(id: &str) -> AgentId {
    AgentId::parse(id).expect("agent id")
}

fn workspace(id: &str) -> WorkspaceId {
    WorkspaceId::parse(id).expect("workspace id")
}

/// Materialize the real schema and seed:
///  * a creator user + host (the agent foreign keys),
///  * the target agent,
///  * a REGULAR workspace the target is enrolled in (a `workspace_members` row),
///  * the target's private HOME workspace (`owner_agent_id = target`), which by
///    schema can hold no `workspace_members` row.
async fn seeded() -> (TempDir, SqliteAccessStore) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("peer-membership.db");
    let mut db = current_schema::create_current_schema(&path).await;

    for statement in [
        "INSERT INTO users (id, name, display_name, created_at, updated_at) \
         VALUES ('user-creator', 'creator', 'Creator', 1, 1)",
        "INSERT INTO hosts (id, name, kind, created_at) VALUES ('host-1', 'host-1', 'desktop', 1)",
        "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, created_by_user_id) \
         VALUES ('agent-target', 'target', 'claude', 'host-1', 1, 1, 'user-creator')",
        // A regular workspace (owner_agent_id NULL requires a non-null name_key).
        "INSERT INTO workspaces (id, name, name_key, created_at, updated_at) \
         VALUES ('ws-regular', 'Regular', 'ws-regular', 1, 1)",
        // The target's private home (owner_agent_id set requires name_key NULL).
        "INSERT INTO workspaces (id, name, owner_agent_id, created_at, updated_at) \
         VALUES ('ws-home', 'Home', 'agent-target', 1, 1)",
        // The regular membership edge — the only shape that counts.
        "INSERT INTO workspace_members (workspace_id, agent_id, joined_at) \
         VALUES ('ws-regular', 'agent-target', 1)",
    ] {
        sqlx::query(statement).execute(&mut db).await.unwrap();
    }

    db.close().await.unwrap();
    let store = SqliteAccessStore::open(&path).await.unwrap();
    (directory, store)
}

#[tokio::test]
async fn a_regular_membership_edge_is_a_member() {
    let (_dir, store) = seeded().await;
    assert!(
        store
            .agent_is_member(&workspace(REGULAR_WS), &agent(TARGET))
            .await
            .expect("read"),
        "a real workspace_members row is membership"
    );
}

/// The discriminator: the target OWNS the home workspace — its
/// caller-horizon visibility is real — but it has no `workspace_members` row
/// there, so the peer membership port answers `false`. Owned-home presence must
/// never launder into target co-membership.
#[tokio::test]
async fn an_owned_home_is_not_a_member() {
    let (_dir, store) = seeded().await;
    assert!(
        !store
            .agent_is_member(&workspace(HOME_WS), &agent(TARGET))
            .await
            .expect("read"),
        "owning the home workspace is not workspace_members membership"
    );
}

#[tokio::test]
async fn an_agent_with_no_edge_in_a_workspace_is_not_a_member() {
    let (_dir, store) = seeded().await;
    assert!(
        !store
            .agent_is_member(&workspace(REGULAR_WS), &agent("agent-stranger"))
            .await
            .expect("read"),
        "no row for the pair is not membership"
    );
}
