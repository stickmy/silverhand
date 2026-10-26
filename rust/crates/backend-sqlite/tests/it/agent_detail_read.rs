//! Integration tests for the agent-detail cold-read SQL (#task:2195, batch 3).
//!
//! The route-level tests (backend-server/src/identity_access/agent_detail.rs) pin the
//! two-authority ORDER (source-bind, then ACL, then stage widener) against fake
//! facts; these pin that the adapter's SQL RESOLVES those facts correctly from real
//! committed rows. Two differentials are exercised:
//!
//!  * the ACL union — the detail arms run the SAME `agent_view_acl` SQL as
//!    availability, so dropping or mis-gating an arm makes a viewer admitted only by
//!    it disappear; `AgentViewerArms` collapses to one admitted set, so constructing
//!    the expected bundle with a viewer in a given arm pins both that the arm
//!    admitted it AND that no other arm leaked a stranger in.
//!  * the projection — every wire-facing column decodes off the one snapshot, with
//!    the reachable numeric columns decoded strictly so a corrupt committed row is a
//!    read error, not a silent default.

use silverhand_backend_core::{
    foundation::identity::{AgentId, UserId},
    foundation::persistence::PersistenceFailure,
    identity_access::agent_detail_read::{
        AgentDetailFacts, AgentDetailProjection, AgentDetailReadModel,
    },
    identity_access::agent_lifecycle::MaterializationStatus,
    identity_access::agents::runtime::RuntimeKind,
    identity_access::capability::agent_view::{AgentViewerArms, AgentVisibility},
};
use silverhand_backend_sqlite::identity_access::agent_detail_read::SqliteAgentDetailColdRead;
use sqlx::Connection;

use crate::current_schema;

fn agent(id: &str) -> AgentId {
    AgentId::parse(id).expect("agent id")
}

fn user(id: &str) -> UserId {
    UserId::parse(id).expect("user id")
}

/// Create a fresh current-schema database, run every seed statement, and open the
/// adapter over it. The `TempDir` guard is returned so the file outlives the pool.
async fn seeded(statements: &[&'static str]) -> (tempfile::TempDir, SqliteAgentDetailColdRead) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("agent-detail-read.sqlite");
    let mut db = current_schema::create_current_schema(&path).await;
    for statement in statements {
        sqlx::query(*statement).execute(&mut db).await.unwrap();
    }
    db.close().await.unwrap();
    let cold_read = SqliteAgentDetailColdRead::open(&path).await.unwrap();
    (directory, cold_read)
}

async fn detail_of(cold_read: &SqliteAgentDetailColdRead, id: &str) -> Option<AgentDetailFacts> {
    cold_read.read_agent_detail(&agent(id)).await.unwrap()
}

/// The fallible read, so a falsifier can assert a corrupt committed row surfaces as
/// a read error rather than being coerced to a default.
async fn try_detail(
    cold_read: &SqliteAgentDetailColdRead,
    id: &str,
) -> Result<Option<AgentDetailFacts>, PersistenceFailure> {
    cold_read.read_agent_detail(&agent(id)).await
}

fn active_visibility(viewers: AgentViewerArms) -> AgentVisibility {
    AgentVisibility {
        viewers,
        materialization_status: MaterializationStatus::Active,
    }
}

const CREATOR: &str = "INSERT INTO users (id, name, display_name, created_at, updated_at) \
     VALUES ('u-creator', 'creator', 'Creator', 1000, 1000)";
const HOST_H1: &str =
    "INSERT INTO hosts (id, name, kind, created_at) VALUES ('h1', 'host-one', 'desktop', 1000)";
const AGENT_A1: &str = "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, created_by_user_id) \
     VALUES ('a1', 'agent-one', 'qoder', 'h1', 1000, 1000, 'u-creator')";

// --- ACL arm resolution: the visibility union the detail read carries. ------------

#[tokio::test]
async fn the_creator_arm_resolves_and_excludes_unrelated_users() {
    // A registered human who is in no arm must not appear: the creator arm is the
    // only admitted viewer.
    let (_guard, cold_read) = seeded(&[
        CREATOR,
        "INSERT INTO users (id, name, display_name, created_at, updated_at) \
         VALUES ('u-stranger', 'stranger', 'Stranger', 1000, 1000)",
        HOST_H1,
        AGENT_A1,
    ])
    .await;

    let facts = detail_of(&cold_read, "a1").await.expect("a1 present");
    assert_eq!(facts.source(), &agent("a1"));
    assert_eq!(
        facts.visibility(),
        &active_visibility(AgentViewerArms::new(Some(user("u-creator")), [], []))
    );
}

#[tokio::test]
async fn the_direct_pair_human_endpoint_arm_resolves() {
    // The arc-ordering CHECK forces the agent into slot A ('agent' < 'human'), so the
    // human endpoint is slot B — the adapter's first UNION arm.
    let (_guard, cold_read) = seeded(&[
        CREATOR,
        "INSERT INTO users (id, name, display_name, created_at, updated_at) \
         VALUES ('u-pair', 'pair', 'Pair Human', 1000, 1000)",
        HOST_H1,
        AGENT_A1,
        "INSERT INTO conversations \
           (id, workspace_id, endpoint_a_kind, endpoint_a_id, endpoint_b_kind, endpoint_b_id, \
            placement_kind, roster_policy, title, status, created_at, updated_at) \
         VALUES ('c-pair', NULL, 'agent', 'a1', 'human', 'u-pair', \
            'direct_pair', 'fixed', 'dm', 'active', 1000, 1000)",
    ])
    .await;

    let facts = detail_of(&cold_read, "a1").await.expect("a1 present");
    assert_eq!(
        facts.visibility(),
        &active_visibility(AgentViewerArms::new(
            Some(user("u-creator")),
            [user("u-pair")],
            [],
        ))
    );
}

#[tokio::test]
async fn an_agent_to_agent_pair_contributes_no_viewer() {
    // The human-endpoint SQL gates the OTHER endpoint on `= 'human'` in both slot
    // orders. Drop that gate and the peer AGENT id ('a2') would decode as a UserId and
    // pollute the admitted set — so the pin is that the pair arm stays empty and the
    // view equals the creator-only bundle.
    let (_guard, cold_read) = seeded(&[
        CREATOR,
        HOST_H1,
        AGENT_A1,
        "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, created_by_user_id) \
         VALUES ('a2', 'agent-two', 'qoder', 'h1', 1000, 1000, 'u-creator')",
        // 'a1' < 'a2', so a1 is endpoint_a and a2 endpoint_b: an agent↔agent pair.
        "INSERT INTO conversations \
           (id, workspace_id, endpoint_a_kind, endpoint_a_id, endpoint_b_kind, endpoint_b_id, \
            placement_kind, roster_policy, title, status, created_at, updated_at) \
         VALUES ('c-aa', NULL, 'agent', 'a1', 'agent', 'a2', \
            'direct_pair', 'fixed', 'dm', 'active', 1000, 1000)",
    ])
    .await;

    let facts = detail_of(&cold_read, "a1").await.expect("a1 present");
    assert_eq!(
        facts.visibility(),
        &active_visibility(AgentViewerArms::new(Some(user("u-creator")), [], []))
    );
}

#[tokio::test]
async fn the_regular_workspace_membership_arm_resolves() {
    // A regular workspace (owner_agent_id NULL ⇒ name_key present) the agent is a
    // `workspace_members` row of: the first branch of the workspace union.
    let (_guard, cold_read) = seeded(&[
        CREATOR,
        "INSERT INTO users (id, name, display_name, created_at, updated_at) \
         VALUES ('u-member', 'member', 'Workspace Member', 1000, 1000)",
        HOST_H1,
        AGENT_A1,
        "INSERT INTO workspaces \
           (id, name, name_key, description, owner_agent_id, archived_at, created_at, updated_at) \
         VALUES ('w1', 'Workspace One', 'workspace-one', NULL, NULL, NULL, 1000, 1000)",
        "INSERT INTO workspace_members (workspace_id, agent_id, joined_at) VALUES ('w1', 'a1', 1000)",
        "INSERT INTO workspace_users (workspace_id, user_id, joined_at) VALUES ('w1', 'u-member', 1000)",
    ])
    .await;

    let facts = detail_of(&cold_read, "a1").await.expect("a1 present");
    assert_eq!(
        facts.visibility(),
        &active_visibility(AgentViewerArms::new(
            Some(user("u-creator")),
            [],
            [user("u-member")],
        ))
    );
}

#[tokio::test]
async fn the_agent_home_workspace_arm_resolves() {
    // The agent's OWN agent-home workspace (owner_agent_id = agent ⇒ name_key NULL):
    // the second branch of the workspace union, a distinct SQL path from regular
    // membership.
    let (_guard, cold_read) = seeded(&[
        CREATOR,
        "INSERT INTO users (id, name, display_name, created_at, updated_at) \
         VALUES ('u-home', 'home', 'Agent-Home Member', 1000, 1000)",
        HOST_H1,
        AGENT_A1,
        "INSERT INTO workspaces \
           (id, name, name_key, description, owner_agent_id, archived_at, created_at, updated_at) \
         VALUES ('wh1', 'Home One', NULL, NULL, 'a1', NULL, 1000, 1000)",
        "INSERT INTO workspace_users (workspace_id, user_id, joined_at) VALUES ('wh1', 'u-home', 1000)",
    ])
    .await;

    let facts = detail_of(&cold_read, "a1").await.expect("a1 present");
    assert_eq!(
        facts.visibility(),
        &active_visibility(AgentViewerArms::new(
            Some(user("u-creator")),
            [],
            [user("u-home")],
        ))
    );
}

#[tokio::test]
async fn an_absent_agent_reads_as_no_detail() {
    let (_guard, cold_read) = seeded(&[CREATOR, HOST_H1, AGENT_A1]).await;
    assert_eq!(detail_of(&cold_read, "a-missing").await, None);
}

// --- Projection: every wire-facing column decodes off the one snapshot. -----------

#[tokio::test]
async fn the_projection_decodes_every_wire_column() {
    // A fully-populated row on a non-active stage: the stage is carried on the
    // visibility (never a second copy), the failure columns and every optional string
    // decode verbatim, the host label columns arrive raw off the LEFT JOIN, and the
    // creator is the ONE decoded id shared by the ACL and the wire echo.
    let (_guard, cold_read) = seeded(&[
        CREATOR,
        "INSERT INTO hosts (id, name, kind, created_at, display_label) \
         VALUES ('h1', 'host-one', 'desktop', 1000, 'Host One Label')",
        "INSERT INTO agents \
           (id, name, runtime, model, host_id, description, created_at, updated_at, display_name, \
            reasoning_effort, fast_mode, created_by_user_id, avatar_storage_key, avatar_content_type, \
            materialization_status, materialization_failed_code, materialization_failed_at, \
            deletion_started_at, session_scope) \
         VALUES ('a1', 'agent-one', 'qoder', 'sonnet', 'h1', 'a description', 1000, 2000, 'Agent One', \
            'high', 1, 'u-creator', 'avatars/a1.png', 'image/png', 'failed', 'boom', 3000, 4000, 'agent')",
    ])
    .await;

    let facts = detail_of(&cold_read, "a1").await.expect("a1 present");
    assert_eq!(facts.source(), &agent("a1"));
    assert_eq!(
        facts.visibility(),
        &AgentVisibility {
            viewers: AgentViewerArms::new(Some(user("u-creator")), [], []),
            materialization_status: MaterializationStatus::Failed,
        }
    );
    assert_eq!(
        facts.projection(),
        &AgentDetailProjection {
            name: "agent-one".to_string(),
            display_name: "Agent One".to_string(),
            runtime: RuntimeKind::Qoder,
            model: Some("sonnet".to_string()),
            reasoning_effort: Some("high".to_string()),
            fast_mode: Some(true),
            host_id: "h1".to_string(),
            host_display_label: Some("Host One Label".to_string()),
            host_name: Some("host-one".to_string()),
            description: Some("a description".to_string()),
            session_scope: "agent".to_string(),
            avatar_storage_key: Some("avatars/a1.png".to_string()),
            materialization_failed_code: Some("boom".to_string()),
            materialization_failed_at: Some(3000),
            deletion_started_at: Some(4000),
            created_at: 1000,
            updated_at: 2000,
        }
    );
    // The creator is carried once on the bundle: the same decoded id the ACL creator
    // arm above was built from is what the wire echoes.
    assert_eq!(facts.created_by_user_id(), &user("u-creator"));
}

#[tokio::test]
async fn the_optional_columns_decode_absent_when_null() {
    // The minimal row leaves every nullable column NULL: each maps to `None`, and the
    // defaulted `display_name`/`session_scope` decode to their column defaults. The
    // host has no `display_label`, so that arm is `None` while `host_name` is present.
    let (_guard, cold_read) = seeded(&[CREATOR, HOST_H1, AGENT_A1]).await;

    let facts = detail_of(&cold_read, "a1").await.expect("a1 present");
    assert_eq!(
        facts.projection(),
        &AgentDetailProjection {
            name: "agent-one".to_string(),
            display_name: String::new(),
            runtime: RuntimeKind::Qoder,
            model: None,
            reasoning_effort: None,
            fast_mode: None,
            host_id: "h1".to_string(),
            host_display_label: None,
            host_name: Some("host-one".to_string()),
            description: None,
            session_scope: "conversation".to_string(),
            avatar_storage_key: None,
            materialization_failed_code: None,
            materialization_failed_at: None,
            deletion_started_at: None,
            created_at: 1000,
            updated_at: 1000,
        }
    );
}

#[tokio::test]
async fn fast_mode_maps_zero_to_false() {
    // The DTO's `Boolean(fast)` over the stored 0/1 is exactly `!= 0`: a stored 0 is
    // `Some(false)`, distinct from the NULL that the minimal row above proves is
    // `None`.
    let (_guard, cold_read) = seeded(&[
        CREATOR,
        HOST_H1,
        "INSERT INTO agents \
           (id, name, runtime, host_id, created_at, updated_at, created_by_user_id, fast_mode) \
         VALUES ('a1', 'agent-one', 'qoder', 'h1', 1000, 1000, 'u-creator', 0)",
    ])
    .await;

    let facts = detail_of(&cold_read, "a1").await.expect("a1 present");
    assert_eq!(facts.projection().fast_mode, Some(false));
}

// --- Strict-decode falsifiers for every REACHABLE decoded column (#task:2195 P1). --
//
// Each pins that the decode gate is load-bearing: a corrupt row surfaces as a read
// error (a safe 500 upstream), and flipping ONLY that column to a legal value is the
// sole change that lets the read succeed — a coercion-to-default would erase the
// difference. Excluded: `agents.id` (WHERE-bound, so a corrupt id is an absent row,
// not a decode), `materialization_status` (a CHECK bars an illegal value from ever
// committing), and `host_id` plus every PASS-THROUGH wire string, which the DTO
// carries verbatim with no decode of its own.
//
// `agents.runtime` is decoded too (through the closed set's owner) and has no CHECK,
// so it IS falsifiable — its gate lives in
// `backend-server/tests/agent_runtime_wire_vocabulary.rs`, which drives the corrupt
// and the repaired row through BOTH public runtime faces over one seeded store. The
// point of that column is that the two faces cannot disagree, which is not observable
// from this adapter alone.

/// `INTEGER`-affinity columns keep a non-numeric text verbatim, so `'soon'`/`'yes'`
/// is a committed-but-undecodable value — distinct from a numeric string, which
/// affinity would coerce and hide the gate. Each entry is `(column, corrupt row,
/// repaired row)`; the repaired row differs from the corrupt one in that column
/// alone.
const NUMERIC_FALSIFIERS: &[(&str, &str, &str)] = &[
    (
        "created_at",
        "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, created_by_user_id) \
         VALUES ('a1', 'agent-one', 'qoder', 'h1', 'soon', 1000, 'u-creator')",
        AGENT_A1,
    ),
    (
        "updated_at",
        "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, created_by_user_id) \
         VALUES ('a1', 'agent-one', 'qoder', 'h1', 1000, 'soon', 'u-creator')",
        AGENT_A1,
    ),
    (
        "fast_mode",
        "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, created_by_user_id, fast_mode) \
         VALUES ('a1', 'agent-one', 'qoder', 'h1', 1000, 1000, 'u-creator', 'yes')",
        "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, created_by_user_id, fast_mode) \
         VALUES ('a1', 'agent-one', 'qoder', 'h1', 1000, 1000, 'u-creator', 1)",
    ),
    (
        "materialization_failed_at",
        "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, created_by_user_id, materialization_failed_at) \
         VALUES ('a1', 'agent-one', 'qoder', 'h1', 1000, 1000, 'u-creator', 'soon')",
        "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, created_by_user_id, materialization_failed_at) \
         VALUES ('a1', 'agent-one', 'qoder', 'h1', 1000, 1000, 'u-creator', 3000)",
    ),
    (
        "deletion_started_at",
        "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, created_by_user_id, deletion_started_at) \
         VALUES ('a1', 'agent-one', 'qoder', 'h1', 1000, 1000, 'u-creator', 'soon')",
        "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, created_by_user_id, deletion_started_at) \
         VALUES ('a1', 'agent-one', 'qoder', 'h1', 1000, 1000, 'u-creator', 4000)",
    ),
];

#[tokio::test]
async fn a_non_numeric_reachable_integer_column_is_a_read_error_not_a_default() {
    for (column, corrupt, repaired) in NUMERIC_FALSIFIERS {
        let (_bad_guard, bad) = seeded(&[CREATOR, HOST_H1, corrupt]).await;
        assert!(
            try_detail(&bad, "a1").await.is_err(),
            "corrupt {column} must surface as a read error",
        );

        let (_good_guard, good) = seeded(&[CREATOR, HOST_H1, repaired]).await;
        assert!(
            try_detail(&good, "a1").await.is_ok(),
            "repairing {column} alone must let the read succeed",
        );
    }
}

/// A user row with the empty id: the FK RESTRICT target for a corrupt
/// `created_by_user_id` and the birth-trigger principal for a corrupt endpoint.
const EMPTY_USER: &str = "INSERT INTO users (id, name, display_name, created_at, updated_at) \
     VALUES ('', 'empty-user', 'Empty', 1000, 1000)";

#[tokio::test]
async fn an_empty_creator_id_on_the_agent_row_is_a_read_error() {
    let (_bad_guard, bad) = seeded(&[
        CREATOR,
        EMPTY_USER,
        HOST_H1,
        "INSERT INTO agents \
           (id, name, runtime, host_id, created_at, updated_at, created_by_user_id) \
         VALUES ('a1', 'agent-one', 'qoder', 'h1', 1000, 1000, '')",
    ])
    .await;
    assert!(try_detail(&bad, "a1").await.is_err());

    let (_good_guard, good) = seeded(&[CREATOR, HOST_H1, AGENT_A1]).await;
    assert!(try_detail(&good, "a1").await.is_ok());
}

#[tokio::test]
async fn an_empty_direct_pair_human_endpoint_is_a_read_error() {
    // The birth trigger requires each endpoint to name an existing principal, so an
    // empty-id user lets the row exist; the arc-order CHECK still holds because
    // 'agent' < 'human' regardless of id. The decode of the endpoint as a `UserId` is
    // what refuses it — the SAME shared arm SQL availability decodes.
    let (_bad_guard, bad) = seeded(&[
        CREATOR,
        EMPTY_USER,
        HOST_H1,
        AGENT_A1,
        "INSERT INTO conversations \
           (id, workspace_id, endpoint_a_kind, endpoint_a_id, endpoint_b_kind, endpoint_b_id, \
            placement_kind, roster_policy, title, status, created_at, updated_at) \
         VALUES ('c-pair', NULL, 'agent', 'a1', 'human', '', \
            'direct_pair', 'fixed', 'dm', 'active', 1000, 1000)",
    ])
    .await;
    assert!(try_detail(&bad, "a1").await.is_err());

    let (_good_guard, good) = seeded(&[
        CREATOR,
        "INSERT INTO users (id, name, display_name, created_at, updated_at) \
         VALUES ('u-pair', 'pair', 'Pair Human', 1000, 1000)",
        HOST_H1,
        AGENT_A1,
        "INSERT INTO conversations \
           (id, workspace_id, endpoint_a_kind, endpoint_a_id, endpoint_b_kind, endpoint_b_id, \
            placement_kind, roster_policy, title, status, created_at, updated_at) \
         VALUES ('c-pair', NULL, 'agent', 'a1', 'human', 'u-pair', \
            'direct_pair', 'fixed', 'dm', 'active', 1000, 1000)",
    ])
    .await;
    assert!(try_detail(&good, "a1").await.is_ok());
}

/// A regular workspace the agent is a member of, so `workspace_users` rows for it
/// enter the shared-member arm.
const REGULAR_WORKSPACE: &str = "INSERT INTO workspaces \
       (id, name, name_key, description, owner_agent_id, archived_at, created_at, updated_at) \
     VALUES ('w1', 'Workspace One', 'workspace-one', NULL, NULL, NULL, 1000, 1000)";
const WORKSPACE_MEMBERSHIP: &str =
    "INSERT INTO workspace_members (workspace_id, agent_id, joined_at) VALUES ('w1', 'a1', 1000)";

#[tokio::test]
async fn an_empty_shared_workspace_member_id_is_a_read_error() {
    let (_bad_guard, bad) = seeded(&[
        CREATOR,
        EMPTY_USER,
        HOST_H1,
        AGENT_A1,
        REGULAR_WORKSPACE,
        WORKSPACE_MEMBERSHIP,
        "INSERT INTO workspace_users (workspace_id, user_id, joined_at) VALUES ('w1', '', 1000)",
    ])
    .await;
    assert!(try_detail(&bad, "a1").await.is_err());

    let (_good_guard, good) = seeded(&[
        CREATOR,
        "INSERT INTO users (id, name, display_name, created_at, updated_at) \
         VALUES ('u-member', 'member', 'Workspace Member', 1000, 1000)",
        HOST_H1,
        AGENT_A1,
        REGULAR_WORKSPACE,
        WORKSPACE_MEMBERSHIP,
        "INSERT INTO workspace_users (workspace_id, user_id, joined_at) VALUES ('w1', 'u-member', 1000)",
    ])
    .await;
    assert!(try_detail(&good, "a1").await.is_ok());
}
