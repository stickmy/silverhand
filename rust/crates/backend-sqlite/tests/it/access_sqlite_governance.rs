//! The request-boundary governance door against the real, checked-in current
//! schema.
//!
//! This is the door that answers *before* any enrollment work runs, so what it
//! must be pinned against is the product's own tables: the `workspaces` row shape
//! (including the `CHECK ((owner_agent_id IS NULL) = (name_key IS NOT NULL))`
//! that makes an agent home structurally distinct) and the authoritative
//! `workspace_users.role` column with its `CHECK` set in force. The database is
//! materialized only by the shared current-schema loader; this test adds no
//! schema of its own.
//!
//! A corrupt role token is deliberately NOT exercised here. The committed schema
//! constrains `role` to the three tokens, so reaching that arm would mean
//! subverting the very constraint this test exists to run against; the arm is
//! there because the parse is fallible, and its safe-500 mapping is pinned where
//! it is reachable, not by defeating the schema.

use sqlx::{Connection, SqliteConnection};

use silverhand_backend_core::foundation::identity::{AgentId, Principal, UserId, WorkspaceId};
use silverhand_backend_core::identity_access::access::{
    WorkspaceAccessPolicy, WorkspaceGovernance, WorkspaceRole,
};
use silverhand_backend_sqlite::identity_access::access::SqliteAccessStore;
use tempfile::TempDir;

use crate::current_schema;

const WORKSPACE: &str = "ws-1";
/// A second regular workspace nobody in this test joins, so "not a member here"
/// is distinguishable from "not a member anywhere".
const OTHER_WORKSPACE: &str = "ws-2";
/// The private home of `AGENT`, which is a container with no roster at all.
const AGENT_HOME: &str = "ws-agent-home";
const MISSING_WORKSPACE: &str = "ws-nonexistent";

const OWNER: &str = "user-owner";
const ADMIN: &str = "user-admin";
const MEMBER: &str = "user-member";
/// A registered user holding no role row in `WORKSPACE`.
const STRANGER: &str = "user-stranger";
const AGENT: &str = "agent-1";

/// Materializes the real schema and seeds the containers and role rows the door
/// reads: three regular workspaces' worth of parent rows, one agent home, and one
/// role row per governance rank.
async fn schema_with_fixtures() -> (TempDir, std::path::PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("governance.db");
    let mut connection = current_schema::create_current_schema(&path).await;

    for user in [OWNER, ADMIN, MEMBER, STRANGER] {
        sqlx::query(
            "INSERT INTO users (id, name, display_name, created_at, updated_at) \
             VALUES (?, ?, ?, 1, 1)",
        )
        .bind(user)
        .bind(user)
        .bind(user)
        .execute(&mut connection)
        .await
        .unwrap();
    }

    sqlx::query(
        "INSERT INTO hosts (id, name, kind, created_at, user_id, provisioning_source) \
         VALUES ('host-1', 'host-1', 'remote', 1, ?, 'user_invite')",
    )
    .bind(OWNER)
    .execute(&mut connection)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, created_by_user_id) \
         VALUES (?, ?, 'claude', 'host-1', 1, 1, ?)",
    )
    .bind(AGENT)
    .bind(AGENT)
    .bind(OWNER)
    .execute(&mut connection)
    .await
    .unwrap();

    // Regular workspaces: owner_agent_id NULL requires a non-null name_key.
    for workspace in [WORKSPACE, OTHER_WORKSPACE] {
        sqlx::query(
            "INSERT INTO workspaces (id, name, name_key, created_at, updated_at) \
             VALUES (?, ?, ?, 1, 1)",
        )
        .bind(workspace)
        .bind(workspace)
        .bind(workspace)
        .execute(&mut connection)
        .await
        .unwrap();
    }

    // The agent's private home: the CHECK forces name_key NULL here, so this row
    // could not be mistaken for a regular workspace even by accident.
    sqlx::query(
        "INSERT INTO workspaces (id, name, owner_agent_id, created_at, updated_at) \
         VALUES (?, 'Home', ?, 1, 1)",
    )
    .bind(AGENT_HOME)
    .bind(AGENT)
    .execute(&mut connection)
    .await
    .unwrap();

    for (user, role) in [(OWNER, "owner"), (ADMIN, "admin"), (MEMBER, "member")] {
        sqlx::query(
            "INSERT INTO workspace_users (workspace_id, user_id, joined_at, role) \
             VALUES (?, ?, 1, ?)",
        )
        .bind(WORKSPACE)
        .bind(user)
        .bind(role)
        .execute(&mut connection)
        .await
        .unwrap();
    }

    // The stranger is a real workspace member — elsewhere. Membership is per
    // workspace, so this row must not grant anything in WORKSPACE.
    sqlx::query(
        "INSERT INTO workspace_users (workspace_id, user_id, joined_at, role) \
         VALUES (?, ?, 1, 'owner')",
    )
    .bind(OTHER_WORKSPACE)
    .bind(STRANGER)
    .execute(&mut connection)
    .await
    .unwrap();

    connection.close().await.unwrap();
    (directory, path)
}

fn human(user: &str) -> Principal {
    Principal::Human {
        user_id: UserId::parse(user).unwrap(),
    }
}

async fn ceiling(
    store: &SqliteAccessStore,
    principal: &Principal,
    workspace: &str,
) -> WorkspaceGovernance {
    store
        .authorize_member_governance(principal, &WorkspaceId::parse(workspace).unwrap())
        .await
        .unwrap()
}

#[tokio::test]
async fn a_humans_ceiling_is_the_authoritative_role_column() {
    let (_dir, path) = schema_with_fixtures().await;
    let store = SqliteAccessStore::open(&path).await.unwrap();

    assert_eq!(
        ceiling(&store, &human(OWNER), WORKSPACE).await,
        WorkspaceGovernance::Admin
    );
    assert_eq!(
        ceiling(&store, &human(ADMIN), WORKSPACE).await,
        WorkspaceGovernance::Admin
    );
    assert_eq!(
        ceiling(&store, &human(MEMBER), WORKSPACE).await,
        WorkspaceGovernance::Member,
        "a plain member clears the agent roster's floor, not the people roster's"
    );
}

#[tokio::test]
async fn membership_is_per_workspace_so_a_role_elsewhere_grants_nothing_here() {
    let (_dir, path) = schema_with_fixtures().await;
    let store = SqliteAccessStore::open(&path).await.unwrap();

    assert_eq!(
        ceiling(&store, &human(STRANGER), OTHER_WORKSPACE).await,
        WorkspaceGovernance::Admin,
        "the stranger really does own the other workspace"
    );
    assert_eq!(
        ceiling(&store, &human(STRANGER), WORKSPACE).await,
        WorkspaceGovernance::Denied,
        "the join predicate is scoped to the requested workspace, not to the user"
    );
}

#[tokio::test]
async fn an_agent_home_is_the_same_not_found_as_a_missing_workspace() {
    let (_dir, path) = schema_with_fixtures().await;
    let store = SqliteAccessStore::open(&path).await.unwrap();

    // The workspace owner asks about a container that exists in the table but has
    // no roster to govern. It must be indistinguishable from a row that is not
    // there at all, or the door leaks the existence of agent homes.
    assert_eq!(
        ceiling(&store, &human(OWNER), AGENT_HOME).await,
        WorkspaceGovernance::WorkspaceNotFound
    );
    assert_eq!(
        ceiling(&store, &human(OWNER), MISSING_WORKSPACE).await,
        WorkspaceGovernance::WorkspaceNotFound
    );
}

#[tokio::test]
async fn existence_is_resolved_before_the_caller_is_graded() {
    let (_dir, path) = schema_with_fixtures().await;
    let store = SqliteAccessStore::open(&path).await.unwrap();

    // Even the principal that governs everything learns only "not found" about a
    // workspace that is not there — and so does the one that governs nothing.
    for principal in [
        Principal::System,
        human(OWNER),
        Principal::Agent {
            agent_id: AgentId::parse(AGENT).unwrap(),
        },
    ] {
        assert_eq!(
            ceiling(&store, &principal, MISSING_WORKSPACE).await,
            WorkspaceGovernance::WorkspaceNotFound
        );
        assert_eq!(
            ceiling(&store, &principal, AGENT_HOME).await,
            WorkspaceGovernance::WorkspaceNotFound
        );
    }
}

#[tokio::test]
async fn the_system_principal_governs_without_a_role_row() {
    let (_dir, path) = schema_with_fixtures().await;
    let store = SqliteAccessStore::open(&path).await.unwrap();

    // No workspace_users row names `system`; the rule admits it without one, so
    // the adapter must not read a role for it at all.
    assert_eq!(
        ceiling(&store, &Principal::System, WORKSPACE).await,
        WorkspaceGovernance::Admin
    );
}

#[tokio::test]
async fn an_agent_principal_is_never_a_governance_actor_even_in_its_own_home() {
    let (_dir, path) = schema_with_fixtures().await;
    let store = SqliteAccessStore::open(&path).await.unwrap();

    let agent = Principal::Agent {
        agent_id: AgentId::parse(AGENT).unwrap(),
    };
    assert_eq!(
        ceiling(&store, &agent, WORKSPACE).await,
        WorkspaceGovernance::Denied
    );
    assert_eq!(
        ceiling(&store, &agent, AGENT_HOME).await,
        WorkspaceGovernance::WorkspaceNotFound,
        "owning the home does not turn it into a governable roster"
    );
}

#[tokio::test]
async fn the_door_re_reads_the_role_on_every_call_and_never_caches_a_ceiling() {
    let (_dir, path) = schema_with_fixtures().await;
    let store = SqliteAccessStore::open(&path).await.unwrap();

    assert_eq!(
        ceiling(&store, &human(ADMIN), WORKSPACE).await,
        WorkspaceGovernance::Admin
    );

    let mut probe = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
        .await
        .unwrap();
    sqlx::query(
        "UPDATE workspace_users SET role = 'member' WHERE workspace_id = ? AND user_id = ?",
    )
    .bind(WORKSPACE)
    .bind(ADMIN)
    .execute(&mut probe)
    .await
    .unwrap();
    probe.close().await.unwrap();

    // A store that memoized the first verdict — per process, per pool, per
    // principal — would still answer `Admin` here and keep admitting a demoted
    // caller to the people roster for as long as it lived.
    assert_eq!(
        ceiling(&store, &human(ADMIN), WORKSPACE).await,
        WorkspaceGovernance::Member,
        "the ceiling is read fresh, so a committed demotion is visible at once"
    );
}

#[tokio::test]
async fn every_role_token_the_schema_permits_is_understood_by_the_domain() {
    // The door's parse and the schema's CHECK set must agree exactly: a token the
    // schema permits that the domain cannot read would be an unreachable safe-500
    // on a perfectly legal row. Enumerated from the domain side and asserted to be
    // storable, so adding a role to one side without the other fails here.
    let (_dir, path) = schema_with_fixtures().await;
    let store = SqliteAccessStore::open(&path).await.unwrap();
    let mut probe = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
        .await
        .unwrap();

    let expected = [
        (WorkspaceRole::Owner, WorkspaceGovernance::Admin),
        (WorkspaceRole::Admin, WorkspaceGovernance::Admin),
        (WorkspaceRole::Member, WorkspaceGovernance::Member),
    ];
    for (role, verdict) in expected {
        sqlx::query(
            "INSERT OR REPLACE INTO workspace_users (workspace_id, user_id, joined_at, role) \
             VALUES (?, ?, 1, ?)",
        )
        .bind(WORKSPACE)
        .bind(STRANGER)
        .bind(role.as_str())
        .execute(&mut probe)
        .await
        .expect("a token the domain names must satisfy the schema CHECK");

        assert_eq!(
            ceiling(&store, &human(STRANGER), WORKSPACE).await,
            verdict,
            "the stored token {} must resolve to a verdict, never a safe-500",
            role.as_str()
        );
    }
    probe.close().await.unwrap();
}
