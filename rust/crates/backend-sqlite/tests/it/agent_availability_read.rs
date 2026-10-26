//! Integration tests for the availability cold-read SQL (#task:2195, batch 2).
//!
//! The route-level tests
//! (backend-server/src/host_runtime/availability.rs) pin the two-authority ORDER
//! against fake facts; these pin that the adapter's SQL RESOLVES those facts
//! correctly from real committed rows. The differential each case turns on is the
//! set of viewers the three ACL arms union — drop or mis-gate one arm and a viewer
//! admitted only by it disappears — plus the strict decode of the reachability
//! inputs the view carries.
//!
//! The resolved [`AgentViewFacts`] is compared by value: `AgentViewerArms` collapses
//! its three arms into one admitted set, so constructing the expected bundle with a
//! viewer in a given arm and asserting equality pins both that the arm admitted it
//! AND that no other arm leaked a stranger in.

use silverhand_backend_core::{
    foundation::identity::{AgentId, HostId, UserId},
    foundation::persistence::PersistenceFailure,
    host_runtime::agent_availability_read::{AgentAvailabilityReadModel, AgentViewFacts},
    host_runtime::availability::HostReachabilityFacts,
    identity_access::agent_lifecycle::MaterializationStatus,
    identity_access::capability::agent_view::{AgentViewerArms, AgentVisibility},
};
use silverhand_backend_sqlite::host_runtime::agent_availability_read::SqliteAgentAvailabilityColdRead;
use sqlx::Connection;

use crate::current_schema;

fn agent(id: &str) -> AgentId {
    AgentId::parse(id).expect("agent id")
}

fn host(id: &str) -> HostId {
    HostId::parse(id).expect("host id")
}

fn user(id: &str) -> UserId {
    UserId::parse(id).expect("user id")
}

/// Create a fresh current-schema database, run every seed statement, and open the
/// adapter over it. The `TempDir` guard is returned so the file outlives the pool.
async fn seeded(
    statements: &[&'static str],
) -> (tempfile::TempDir, SqliteAgentAvailabilityColdRead) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("availability-read.sqlite");
    let mut db = current_schema::create_current_schema(&path).await;
    for statement in statements {
        sqlx::query(*statement).execute(&mut db).await.unwrap();
    }
    db.close().await.unwrap();
    let cold_read = SqliteAgentAvailabilityColdRead::open(&path).await.unwrap();
    (directory, cold_read)
}

const CREATOR: &str = "INSERT INTO users (id, name, display_name, created_at, updated_at) \
     VALUES ('u-creator', 'creator', 'Creator', 1000, 1000)";
const HOST_H1: &str =
    "INSERT INTO hosts (id, name, kind, created_at) VALUES ('h1', 'host-one', 'desktop', 1000)";
const AGENT_A1: &str = "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, created_by_user_id) \
     VALUES ('a1', 'agent-one', 'qoder', 'h1', 1000, 1000, 'u-creator')";

/// The admitting view every arm case shares: agent `a1` on host `h1`, active, not
/// being deleted, with the given union of admitted viewers.
fn active_view(viewers: AgentViewerArms) -> AgentViewFacts {
    AgentViewFacts::from_source(
        agent("a1"),
        AgentVisibility {
            viewers,
            materialization_status: MaterializationStatus::Active,
        },
        host("h1"),
        None,
    )
}

async fn view_of(cold_read: &SqliteAgentAvailabilityColdRead, id: &str) -> Option<AgentViewFacts> {
    cold_read.read_agent_view(&agent(id)).await.unwrap()
}

/// The fallible read, so a falsifier can assert a corrupt committed row surfaces as
/// a read error rather than being coerced to a default.
async fn try_view(
    cold_read: &SqliteAgentAvailabilityColdRead,
    id: &str,
) -> Result<Option<AgentViewFacts>, PersistenceFailure> {
    cold_read.read_agent_view(&agent(id)).await
}

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

    assert_eq!(
        view_of(&cold_read, "a1").await,
        Some(active_view(AgentViewerArms::new(
            Some(user("u-creator")),
            [],
            []
        )))
    );
}

#[tokio::test]
async fn the_direct_pair_human_endpoint_arm_resolves() {
    // The arc-ordering CHECK forces the agent into slot A ('agent' < 'human'), so
    // the human endpoint is slot B — the adapter's first UNION arm.
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

    assert_eq!(
        view_of(&cold_read, "a1").await,
        Some(active_view(AgentViewerArms::new(
            Some(user("u-creator")),
            [user("u-pair")],
            [],
        )))
    );
}

#[tokio::test]
async fn an_agent_to_agent_pair_contributes_no_viewer() {
    // The human-endpoint SQL gates the OTHER endpoint on `= 'human'` in both slot
    // orders. Drop that gate and the peer AGENT id ('a2') would decode as a UserId
    // and pollute the admitted set — so the pin is that the pair arm stays empty
    // and the view equals the creator-only bundle.
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

    assert_eq!(
        view_of(&cold_read, "a1").await,
        Some(active_view(AgentViewerArms::new(
            Some(user("u-creator")),
            [],
            []
        )))
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

    assert_eq!(
        view_of(&cold_read, "a1").await,
        Some(active_view(AgentViewerArms::new(
            Some(user("u-creator")),
            [],
            [user("u-member")],
        )))
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

    assert_eq!(
        view_of(&cold_read, "a1").await,
        Some(active_view(AgentViewerArms::new(
            Some(user("u-creator")),
            [],
            [user("u-home")],
        )))
    );
}

#[tokio::test]
async fn an_absent_agent_reads_as_no_view() {
    let (_guard, cold_read) = seeded(&[CREATOR, HOST_H1, AGENT_A1]).await;
    assert_eq!(view_of(&cold_read, "a-missing").await, None);
}

#[tokio::test]
async fn the_reachability_inputs_and_stage_decode_off_the_agent_row() {
    // A non-active stage and a deletion marker are carried on the resolved view
    // verbatim, so a stored value that is not a legal stage would surface as a read
    // error rather than being coerced to a default.
    let (_guard, cold_read) = seeded(&[
        CREATOR,
        HOST_H1,
        "INSERT INTO agents \
           (id, name, runtime, host_id, created_at, updated_at, created_by_user_id, \
            materialization_status, deletion_started_at) \
         VALUES ('a1', 'agent-one', 'qoder', 'h1', 1000, 1000, 'u-creator', 'failed', 5000)",
    ])
    .await;

    assert_eq!(
        view_of(&cold_read, "a1").await,
        Some(AgentViewFacts::from_source(
            agent("a1"),
            AgentVisibility {
                viewers: AgentViewerArms::new(Some(user("u-creator")), [], []),
                materialization_status: MaterializationStatus::Failed,
            },
            host("h1"),
            Some(5000),
        ))
    );
}

#[tokio::test]
async fn a_host_row_decodes_its_revocation_and_an_absent_host_is_no_row() {
    // The host read is `Option`: an absent row is a fact (`HostGone` to the rule),
    // not a failed read. FK RESTRICT keeps an agent's host present, so this arm is
    // reachable only by asking about a host directly — which the whole-wire parity
    // fixtures cannot do.
    let (_guard, cold_read) = seeded(&[
        CREATOR,
        HOST_H1,
        "INSERT INTO hosts (id, name, kind, created_at, revoked_at) \
         VALUES ('h-revoked', 'revoked-host', 'desktop', 1000, 2000)",
        AGENT_A1,
    ])
    .await;

    assert_eq!(
        cold_read.read_host_reachability(&host("h1")).await.unwrap(),
        Some(HostReachabilityFacts {
            id: host("h1"),
            revoked_at: None,
        })
    );
    assert_eq!(
        cold_read
            .read_host_reachability(&host("h-revoked"))
            .await
            .unwrap(),
        Some(HostReachabilityFacts {
            id: host("h-revoked"),
            revoked_at: Some(2000),
        })
    );
    assert_eq!(
        cold_read
            .read_host_reachability(&host("h-absent"))
            .await
            .unwrap(),
        None
    );
}

// Falsifiers for the strict decode of every REACHABLE committed column (#task:2195
// P1). Each pins that the decode gate is load-bearing: a corrupt row surfaces as a
// read error (a safe 500 upstream), and flipping ONLY that column to a legal value
// is the sole change that lets the read succeed — a coercion-to-default would erase
// the difference. `agents.id` is excluded (it is WHERE-bound, so a corrupt id is an
// absent row, not a decode) as is `materialization_status` (a CHECK constraint bars
// an illegal value from ever committing).

/// `INTEGER`-affinity columns keep a non-numeric text verbatim, so `'soon'` is a
/// committed-but-undecodable timestamp — distinct from a numeric string, which
/// affinity would coerce to an integer and hide the gate.
const CORRUPT_DELETION_AGENT: &str = "INSERT INTO agents \
       (id, name, runtime, host_id, created_at, updated_at, created_by_user_id, deletion_started_at) \
     VALUES ('a1', 'agent-one', 'qoder', 'h1', 1000, 1000, 'u-creator', 'soon')";
const REPAIRED_DELETION_AGENT: &str = "INSERT INTO agents \
       (id, name, runtime, host_id, created_at, updated_at, created_by_user_id, deletion_started_at) \
     VALUES ('a1', 'agent-one', 'qoder', 'h1', 1000, 1000, 'u-creator', 5000)";

#[tokio::test]
async fn a_non_numeric_agent_deletion_marker_is_a_read_error_not_a_default() {
    let (_bad_guard, bad) = seeded(&[CREATOR, HOST_H1, CORRUPT_DELETION_AGENT]).await;
    assert!(try_view(&bad, "a1").await.is_err());

    let (_good_guard, good) = seeded(&[CREATOR, HOST_H1, REPAIRED_DELETION_AGENT]).await;
    assert!(try_view(&good, "a1").await.is_ok());
}

#[tokio::test]
async fn a_non_numeric_host_revocation_is_a_read_error_not_a_default() {
    let (_bad_guard, bad) = seeded(&[
        "INSERT INTO hosts (id, name, kind, created_at, revoked_at) \
         VALUES ('h-bad', 'bad-host', 'desktop', 1000, 'soon')",
    ])
    .await;
    assert!(bad.read_host_reachability(&host("h-bad")).await.is_err());

    let (_good_guard, good) = seeded(&[
        "INSERT INTO hosts (id, name, kind, created_at, revoked_at) \
         VALUES ('h-bad', 'bad-host', 'desktop', 1000, 2000)",
    ])
    .await;
    assert_eq!(
        good.read_host_reachability(&host("h-bad")).await.unwrap(),
        Some(HostReachabilityFacts {
            id: host("h-bad"),
            revoked_at: Some(2000),
        })
    );
}

/// A host row with the empty id, so `agents.host_id = ''` satisfies FK RESTRICT and
/// the corruption reaches the decode rather than being refused at write time.
const EMPTY_HOST: &str =
    "INSERT INTO hosts (id, name, kind, created_at) VALUES ('', 'empty-host', 'desktop', 1000)";

#[tokio::test]
async fn an_empty_host_id_on_the_agent_row_is_a_read_error() {
    let (_bad_guard, bad) = seeded(&[
        CREATOR,
        EMPTY_HOST,
        "INSERT INTO agents \
           (id, name, runtime, host_id, created_at, updated_at, created_by_user_id) \
         VALUES ('a1', 'agent-one', 'qoder', '', 1000, 1000, 'u-creator')",
    ])
    .await;
    assert!(try_view(&bad, "a1").await.is_err());

    let (_good_guard, good) = seeded(&[CREATOR, HOST_H1, AGENT_A1]).await;
    assert!(try_view(&good, "a1").await.is_ok());
}

/// A user row with the empty id, the FK RESTRICT target for a corrupt
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
    assert!(try_view(&bad, "a1").await.is_err());

    let (_good_guard, good) = seeded(&[CREATOR, HOST_H1, AGENT_A1]).await;
    assert!(try_view(&good, "a1").await.is_ok());
}

#[tokio::test]
async fn an_empty_direct_pair_human_endpoint_is_a_read_error() {
    // The birth trigger requires each endpoint to name an existing principal, so an
    // empty-id user lets the row exist; the arc-order CHECK still holds because
    // 'agent' < 'human' regardless of id. The decode of the endpoint as a `UserId`
    // is what refuses it.
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
    assert!(try_view(&bad, "a1").await.is_err());

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
    assert!(try_view(&good, "a1").await.is_ok());
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
    assert!(try_view(&bad, "a1").await.is_err());

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
    assert!(try_view(&good, "a1").await.is_ok());
}
