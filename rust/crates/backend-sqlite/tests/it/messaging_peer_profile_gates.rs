//! The production spelling of the internal peer-profile route's caller-side gate
//! ports, implemented on [`MessagingStore`] (#2226). Each reuses a spelling the
//! store already owns — the placement decoder, the membership predicate, the
//! capability shape — so these tests pin that the peer route reads through the SAME
//! truth the task board and send gate do, minting no second authority.
//!
//! Three axes, held apart deliberately:
//!   * placement — the conversation floor: `None`=missing, `WorkspaceLess`=direct
//!     pair, `Workspace(_)`=workspace board, and a below-the-write-path corrupt row
//!     is a safe-500 (`Err`), never a silent collapse to workspace-less.
//!   * horizon — `members ∪ owned agent-home`, the cross-workspace reachability axis.
//!   * capability — the always-on `read_messages` grant (owner arm OR explicit grant;
//!     a `read_tasks`/`send_messages` grant does NOT satisfy it).

use std::error::Error;
use std::path::PathBuf;

use silverhand_backend_core::foundation::identity::{AgentId, ConversationId, WorkspaceId};
use silverhand_backend_core::identity_access::agent_peer_profile::{
    CallerConversationPlacement, CallerReadMessagesCapability, CallerWorkspaceHorizon,
    WorkspacePlacement,
};
use silverhand_backend_sqlite::conversation_messaging::messaging::MessagingStore;
use sqlx::SqliteConnection;
use tempfile::TempDir;

use crate::current_schema;

const USER: &str = "user-1";
const CALLER: &str = "agent-caller"; // member of WS_MAIN, holds read_messages
const MEMBER_NO_CAP: &str = "agent-plain"; // member of WS_MAIN, only read_tasks/send_messages
const OWNER: &str = "agent-owner"; // owns WS_HOME
const STRANGER: &str = "agent-stranger"; // member of nothing
const PEER: &str = "agent-peer";
const WS_MAIN: &str = "workspace-main";
const WS_HOME: &str = "workspace-home";
const CHANNEL: &str = "conv000000000001";
const PAIR: &str = "pair000000000001";

struct Fixture {
    _temp: TempDir,
    path: PathBuf,
    db: SqliteConnection,
}

impl Fixture {
    async fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("peer-gates.sqlite");
        let mut db = current_schema::create_current_schema(&path).await;
        seed(&mut db).await;
        Self {
            _temp: temp,
            path,
            db,
        }
    }

    async fn store(&self) -> MessagingStore {
        MessagingStore::open(&self.path).await.unwrap()
    }
}

fn an_agent(id: &str) -> AgentId {
    AgentId::parse(id).expect("agent id")
}

fn a_workspace(id: &str) -> WorkspaceId {
    WorkspaceId::parse(id).expect("workspace id")
}

fn a_conversation(id: &str) -> ConversationId {
    ConversationId::parse(id).expect("conversation id")
}

fn source_chain(error: &dyn Error) -> String {
    let mut chain = error.to_string();
    let mut cursor = error.source();
    while let Some(source) = cursor {
        chain.push_str(" -> ");
        chain.push_str(&source.to_string());
        cursor = source.source();
    }
    chain
}

async fn seed(db: &mut SqliteConnection) {
    sqlx::query(
        "INSERT INTO hosts (id, name, kind, created_at) VALUES ('host-1', 'host', 'local', 0)",
    )
    .execute(&mut *db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO users (id, name, display_name, created_at, updated_at) \
         VALUES (?, 'human', 'human', 0, 0)",
    )
    .bind(USER)
    .execute(&mut *db)
    .await
    .unwrap();
    for id in [CALLER, MEMBER_NO_CAP, OWNER, STRANGER, PEER] {
        sqlx::query(
            "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, \
             display_name, created_by_user_id, materialization_status, session_scope) \
             VALUES (?, ?, 'codex', 'host-1', 0, 0, ?, ?, 'active', 'agent')",
        )
        .bind(id)
        .bind(id)
        .bind(id)
        .bind(USER)
        .execute(&mut *db)
        .await
        .unwrap();
    }
    // A regular (user-owned) workspace with two agent members and a workspace channel.
    sqlx::query(
        "INSERT INTO workspaces (id, name, name_key, owner_user_id, created_at, updated_at) \
         VALUES (?, 'Main', 'main', ?, 0, 0)",
    )
    .bind(WS_MAIN)
    .bind(USER)
    .execute(&mut *db)
    .await
    .unwrap();
    for id in [CALLER, MEMBER_NO_CAP] {
        sqlx::query(
            "INSERT INTO workspace_members (workspace_id, agent_id, joined_at) VALUES (?, ?, 0)",
        )
        .bind(WS_MAIN)
        .bind(id)
        .execute(&mut *db)
        .await
        .unwrap();
    }
    // CALLER holds read_messages; MEMBER_NO_CAP holds only the adjacent capabilities,
    // proving the token is not confused with read_tasks/send_messages.
    sqlx::query(
        "INSERT INTO workspace_agent_capabilities (workspace_id, agent_id, capability) \
         VALUES (?, ?, 'read_messages')",
    )
    .bind(WS_MAIN)
    .bind(CALLER)
    .execute(&mut *db)
    .await
    .unwrap();
    for capability in ["read_tasks", "send_messages"] {
        sqlx::query(
            "INSERT INTO workspace_agent_capabilities (workspace_id, agent_id, capability) \
             VALUES (?, ?, ?)",
        )
        .bind(WS_MAIN)
        .bind(MEMBER_NO_CAP)
        .bind(capability)
        .execute(&mut *db)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO conversations (id, workspace_id, placement_kind, roster_policy, title, \
         title_key, status, visibility, created_at, updated_at, latest_message_seq) \
         VALUES (?, ?, 'workspace', 'open', 'General', 'general', 'open', 'private', 0, 0, 0)",
    )
    .bind(CHANNEL)
    .bind(WS_MAIN)
    .execute(&mut *db)
    .await
    .unwrap();
    // An agent-home workspace owned by OWNER — no member/capability rows at all.
    sqlx::query(
        "INSERT INTO workspaces (id, name, owner_agent_id, created_at, updated_at) \
         VALUES (?, 'Home', ?, 0, 0)",
    )
    .bind(WS_HOME)
    .bind(OWNER)
    .execute(&mut *db)
    .await
    .unwrap();
    // A workspace-less direct pair between the caller agent and the human.
    sqlx::query(
        "INSERT INTO conversations (id, placement_kind, roster_policy, endpoint_a_kind, \
         endpoint_a_id, endpoint_b_kind, endpoint_b_id, title, status, visibility, \
         created_at, updated_at) VALUES (?, 'direct_pair', 'fixed', 'agent', ?, 'human', ?, \
         '', 'idle', 'private', 0, 0)",
    )
    .bind(PAIR)
    .bind(CALLER)
    .bind(USER)
    .execute(&mut *db)
    .await
    .unwrap();
}

// -- placement: the conversation floor's four arms ----------------------------

#[tokio::test]
async fn a_workspace_conversation_yields_its_workspace_placement() {
    let fixture = Fixture::new().await;
    let store = fixture.store().await;
    let placement = store
        .conversation_placement(&a_conversation(CHANNEL))
        .await
        .unwrap();
    assert_eq!(
        placement,
        Some(WorkspacePlacement::Workspace(a_workspace(WS_MAIN)))
    );
}

#[tokio::test]
async fn a_direct_pair_conversation_is_workspace_less() {
    let fixture = Fixture::new().await;
    let store = fixture.store().await;
    let placement = store
        .conversation_placement(&a_conversation(PAIR))
        .await
        .unwrap();
    assert_eq!(placement, Some(WorkspacePlacement::WorkspaceLess));
}

#[tokio::test]
async fn an_absent_conversation_is_none_not_a_denial() {
    let fixture = Fixture::new().await;
    let store = fixture.store().await;
    let placement = store
        .conversation_placement(&a_conversation("conv000000000404"))
        .await
        .unwrap();
    assert_eq!(placement, None);
}

/// A PRESENT workspace-placed row whose own `workspace_id` coordinate is NULL is
/// below-the-write-path corruption: the placement decoder rejects the row whose kind
/// and coordinates disagree (`conversations.placement`), and the port propagates it as
/// a safe-500 (`Err`), never collapsing to workspace-less. The placement is
/// trigger-immutable and the coordinate is CHECK-guarded, so the corruption is injected
/// exactly as the board resolver's own repair test does.
#[tokio::test]
async fn a_corrupt_workspace_coordinate_is_a_safe_500_not_workspace_less() {
    let mut fixture = Fixture::new().await;
    sqlx::query("DROP TRIGGER conversations_placement_immutable")
        .execute(&mut fixture.db)
        .await
        .unwrap();
    sqlx::query("PRAGMA ignore_check_constraints = ON")
        .execute(&mut fixture.db)
        .await
        .unwrap();
    sqlx::query("UPDATE conversations SET workspace_id = NULL WHERE id = ?")
        .bind(CHANNEL)
        .execute(&mut fixture.db)
        .await
        .unwrap();
    let store = fixture.store().await;

    let error = store
        .conversation_placement(&a_conversation(CHANNEL))
        .await
        .expect_err("a NULL workspace coordinate on a workspace row is corruption");
    assert!(
        source_chain(&error).contains("conversations.placement"),
        "the safe-500 names the disagreeing placement shape: {}",
        source_chain(&error)
    );
}

// -- horizon: members ∪ owned agent-home --------------------------------------

#[tokio::test]
async fn the_horizon_reaches_a_joined_workspace_an_owned_home_and_denies_a_stranger() {
    let fixture = Fixture::new().await;
    let store = fixture.store().await;

    assert!(
        store
            .can_access_workspace(&an_agent(CALLER), &a_workspace(WS_MAIN))
            .await
            .unwrap(),
        "a regular member is inside the horizon"
    );
    assert!(
        store
            .can_access_workspace(&an_agent(OWNER), &a_workspace(WS_HOME))
            .await
            .unwrap(),
        "the agent-home owner is inside its own horizon through the owner arm"
    );
    assert!(
        !store
            .can_access_workspace(&an_agent(STRANGER), &a_workspace(WS_MAIN))
            .await
            .unwrap(),
        "a non-member is outside the horizon"
    );
    assert!(
        !store
            .can_access_workspace(&an_agent(CALLER), &a_workspace("workspace-404"))
            .await
            .unwrap(),
        "a non-existent workspace is outside every horizon"
    );
}

/// The agent-home owner arm is NOT a members-table fallback: a non-owner agent is
/// never reached by an agent-home workspace's horizon (its membership is owner-only).
#[tokio::test]
async fn the_owned_home_horizon_admits_its_owner_only() {
    let fixture = Fixture::new().await;
    let store = fixture.store().await;
    assert!(
        !store
            .can_access_workspace(&an_agent(CALLER), &a_workspace(WS_HOME))
            .await
            .unwrap(),
        "a non-owner has no members-table fallback into an agent-home workspace"
    );
}

// -- capability: the always-on read_messages grant ----------------------------

#[tokio::test]
async fn read_messages_admits_the_home_owner_and_the_granted_member_only() {
    let fixture = Fixture::new().await;
    let store = fixture.store().await;

    assert!(
        store
            .may_read_messages(&an_agent(OWNER), &a_workspace(WS_HOME))
            .await
            .unwrap(),
        "the agent-home owner reads messages through the owner arm without a grant row"
    );
    assert!(
        store
            .may_read_messages(&an_agent(CALLER), &a_workspace(WS_MAIN))
            .await
            .unwrap(),
        "a member holding the read_messages grant may read"
    );
    assert!(
        !store
            .may_read_messages(&an_agent(MEMBER_NO_CAP), &a_workspace(WS_MAIN))
            .await
            .unwrap(),
        "a read_tasks/send_messages grant does NOT satisfy read_messages"
    );
    assert!(
        !store
            .may_read_messages(&an_agent(STRANGER), &a_workspace(WS_MAIN))
            .await
            .unwrap(),
        "a non-member holds no capability"
    );
}
