//! Integration tests for the agent-LIST cold-read SQL (#task:2195, batch 5).
//!
//! The route-level tests pin the narrow/decide split against fake facts; the core
//! `agent_list_read` unit tests pin the decider. These pin that the adapter's SQL
//! RESOLVES the right candidate set from real committed rows, and — the load-bearing
//! one — that the two DIRECTIONS of the shared relation agree.
//!
//!  * The candidate set is `creator-owned UNION relation-admitted`. The creator arm
//!    is a plain column, so an agent a viewer CREATED but shares no pair or workspace
//!    with must still be a candidate; a relation-only narrowing would drop it (the
//!    "reverse gap").
//!  * The order is the port's `created_at ASC`, carried verbatim.
//!  * The list read decodes each row through the SAME strict decode as detail, so a
//!    corrupt committed column is a read error here too, never a silent default.
//!  * The PAIR-MATRIX differential: the LIST direction (by viewer) and the AUDIENCE
//!    direction (by agent, the one detail/availability decide against) are two reads
//!    of one relation. For every fixture user, the agents the list offers as
//!    candidates must equal the agents whose audience admits that user — no forward
//!    gap (a candidate the ACL refuses) and no reverse gap (an admitted agent the
//!    narrowing dropped). Every fixture agent is active, so the comparison is exact.

use std::collections::BTreeSet;

use silverhand_backend_core::{
    foundation::identity::{AgentId, Principal, UserId},
    foundation::persistence::PersistenceFailure,
    identity_access::agent_detail_read::AgentDetailReadModel,
    identity_access::agent_lifecycle::MaterializationStatus,
    identity_access::agent_list_read::AgentListReadModel,
    identity_access::capability::agent_view::{VisibleStages, can_view_agent},
};
use silverhand_backend_sqlite::{
    identity_access::agent_detail_read::SqliteAgentDetailColdRead,
    identity_access::agent_list_read::SqliteAgentListColdRead,
};
use sqlx::Connection;

use crate::current_schema;

fn agent(id: &str) -> AgentId {
    AgentId::parse(id).expect("agent id")
}

fn user(id: &str) -> UserId {
    UserId::parse(id).expect("user id")
}

/// A fresh current-schema database seeded verbatim, with BOTH cold reads opened over
/// it: the list adapter under test and the detail adapter that supplies the audience
/// direction of the differential. The `TempDir` guard outlives both pools.
async fn seeded(
    statements: &[&'static str],
) -> (
    tempfile::TempDir,
    SqliteAgentListColdRead,
    SqliteAgentDetailColdRead,
) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("agent-list-read.sqlite");
    let mut db = current_schema::create_current_schema(&path).await;
    for statement in statements {
        sqlx::query(*statement).execute(&mut db).await.unwrap();
    }
    db.close().await.unwrap();
    let list = SqliteAgentListColdRead::open(&path).await.unwrap();
    let detail = SqliteAgentDetailColdRead::open(&path).await.unwrap();
    (directory, list, detail)
}

/// The candidate agent ids the list adapter offers `viewer`, in the order returned.
async fn candidate_ids(list: &SqliteAgentListColdRead, viewer: &str) -> Vec<String> {
    list.read_agent_candidates(&user(viewer))
        .await
        .expect("candidates")
        .iter()
        .map(|facts| facts.source().as_str().to_string())
        .collect()
}

/// The fallible read, so a falsifier can assert a corrupt committed row surfaces as a
/// read error rather than being coerced to a default.
async fn try_candidates(
    list: &SqliteAgentListColdRead,
    viewer: &str,
) -> Result<(), PersistenceFailure> {
    list.read_agent_candidates(&user(viewer)).await.map(|_| ())
}

// --- The fixture: four active agents across every arm, with a creator-only one. ----
//
// Ownership and reach (all agents ACTIVE, distinct `created_at` for a defined order):
//   a1 (100) created by u-alpha — creator arm only.
//   a2 (200) created by u-beta, in a direct pair with u-alpha — pair arm reaches alpha.
//   a3 (300) created by u-gamma, NO relation edges — the creator-only reverse gap.
//   a4 (400) created by u-alpha, member of workspace w1 whose user is u-delta.
// u-stranger is a registered human in no arm at all.

const FIXTURE: &[&str] = &[
    "INSERT INTO users (id, name, display_name, created_at, updated_at) VALUES \
       ('u-alpha', 'alpha', 'Alpha', 1, 1), \
       ('u-beta', 'beta', 'Beta', 1, 1), \
       ('u-gamma', 'gamma', 'Gamma', 1, 1), \
       ('u-delta', 'delta', 'Delta', 1, 1), \
       ('u-stranger', 'stranger', 'Stranger', 1, 1)",
    "INSERT INTO hosts (id, name, kind, created_at) VALUES ('h1', 'host-one', 'desktop', 1)",
    "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, created_by_user_id) VALUES \
       ('a1', 'agent-one', 'qoder', 'h1', 100, 100, 'u-alpha')",
    "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, created_by_user_id) VALUES \
       ('a2', 'agent-two', 'qoder', 'h1', 200, 200, 'u-beta')",
    "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, created_by_user_id) VALUES \
       ('a3', 'agent-three', 'qoder', 'h1', 300, 300, 'u-gamma')",
    "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, created_by_user_id) VALUES \
       ('a4', 'agent-four', 'qoder', 'h1', 400, 400, 'u-alpha')",
    // a2 <-> u-alpha direct pair: 'agent' < 'human', so the agent is endpoint_a.
    "INSERT INTO conversations \
       (id, workspace_id, endpoint_a_kind, endpoint_a_id, endpoint_b_kind, endpoint_b_id, \
        placement_kind, roster_policy, title, status, created_at, updated_at) \
     VALUES ('c-pair', NULL, 'agent', 'a2', 'human', 'u-alpha', \
        'direct_pair', 'fixed', 'dm', 'active', 200, 200)",
    // a4 in regular workspace w1 (owner_agent_id NULL), whose only user is u-delta.
    "INSERT INTO workspaces \
       (id, name, name_key, description, owner_agent_id, archived_at, created_at, updated_at) \
     VALUES ('w1', 'Workspace One', 'workspace-one', NULL, NULL, NULL, 1, 1)",
    "INSERT INTO workspace_members (workspace_id, agent_id, joined_at) VALUES ('w1', 'a4', 1)",
    "INSERT INTO workspace_users (workspace_id, user_id, joined_at) VALUES ('w1', 'u-delta', 1)",
];

/// Every agent the fixture commits, for the AUDIENCE side of the differential.
const FIXTURE_AGENTS: &[&str] = &["a1", "a2", "a3", "a4"];
/// Every registered human, so the differential also covers the empty-audience user.
const FIXTURE_USERS: &[&str] = &["u-alpha", "u-beta", "u-gamma", "u-delta", "u-stranger"];

#[tokio::test]
async fn a_non_numeric_created_at_is_a_read_error_not_a_default() {
    // The list read shares detail's strict decode. A committed non-numeric
    // `created_at` on a candidate row is corruption: it must surface as a read error
    // (a safe 500 upstream), and repairing that column alone lets the read succeed.
    let corrupt: &[&str] = &[
        "INSERT INTO users (id, name, display_name, created_at, updated_at) \
         VALUES ('u-alpha', 'alpha', 'Alpha', 1, 1)",
        "INSERT INTO hosts (id, name, kind, created_at) VALUES ('h1', 'host-one', 'desktop', 1)",
        "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, created_by_user_id) \
         VALUES ('a1', 'agent-one', 'qoder', 'h1', 'soon', 100, 'u-alpha')",
    ];
    let (_bad_guard, bad, _bad_detail) = seeded(corrupt).await;
    assert!(try_candidates(&bad, "u-alpha").await.is_err());

    let repaired: &[&str] = &[
        "INSERT INTO users (id, name, display_name, created_at, updated_at) \
         VALUES ('u-alpha', 'alpha', 'Alpha', 1, 1)",
        "INSERT INTO hosts (id, name, kind, created_at) VALUES ('h1', 'host-one', 'desktop', 1)",
        "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, created_by_user_id) \
         VALUES ('a1', 'agent-one', 'qoder', 'h1', 100, 100, 'u-alpha')",
    ];
    let (_good_guard, good, _good_detail) = seeded(repaired).await;
    assert!(try_candidates(&good, "u-alpha").await.is_ok());
}

#[tokio::test]
async fn the_list_and_audience_directions_agree_for_every_user() {
    // The pair-matrix differential. For every registered human, the agents the LIST
    // direction offers as candidates must equal the agents whose AUDIENCE admits that
    // human — the two ends of one relation, cross-checked over the whole fixture.
    let (_guard, list, detail) = seeded(FIXTURE).await;

    // Every stage admitted: the candidate SQL never filters by stage (the core stage
    // widener does), so the audience side must be stage-agnostic too.
    let every_stage = VisibleStages::including([
        MaterializationStatus::PendingMaterialization,
        MaterializationStatus::Active,
        MaterializationStatus::Failed,
    ]);

    for viewer in FIXTURE_USERS {
        let list_set: BTreeSet<String> = candidate_ids(&list, viewer).await.into_iter().collect();

        let mut audience_set = BTreeSet::new();
        for id in FIXTURE_AGENTS {
            let facts = detail
                .read_agent_detail(&agent(id))
                .await
                .expect("detail read")
                .expect("fixture agent present");
            let admitted = can_view_agent(
                &Principal::human(user(viewer)),
                facts.source(),
                Some(facts.visibility()),
                &every_stage,
            )
            .is_allowed();
            if admitted {
                audience_set.insert((*id).to_string());
            }
        }

        assert_eq!(list_set, audience_set, "directions disagree for {viewer}");
    }
}
