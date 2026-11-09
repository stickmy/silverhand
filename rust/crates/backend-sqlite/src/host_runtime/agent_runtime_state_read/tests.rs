//! Adapter tests: the source-bound snapshot resolves participation membership,
//! per-conversation redaction (both the workspace-roster and direct-pair arms),
//! the archived split, per-scope queue depth, and seat redaction — all off
//! one `BEGIN DEFERRED` read composed from the SHARED ACL and participation macros.

use sqlx::{Connection, Executor};

use silverhand_backend_core::{
    foundation::identity::{AgentId, ConversationId, UserId},
    host_runtime::agent_runtime_state_read::{
        AgentRuntimeStateReadModel, AgentRuntimeStateView, PresentationFocus, RuntimeFocusView,
        RuntimeSeatScopeView,
    },
    host_runtime::runtime_credential::RuntimeGeneration,
    host_runtime::runtime_event::RuntimeObservedStatus,
};

use super::SqliteAgentRuntimeStateColdRead;

const DATABASE_FILE: &str = "agent-runtime-state.sqlite";

/// One fixture exercising every readability arm and scope.
///
/// * `a-agentscope` (session_scope `agent`) participates in four workspace channels
///   and one direct pair, and holds ONE agent-scope seat.
/// * `a-convscope` (session_scope `conversation`) holds two conversation-scope seats,
///   one the viewer may read and one it may not.
///
/// Readability is seeded so each arm holds a conversation the other cannot reach:
/// `u-member` reads `c-ws`/`c-arch`/`c-recent` through the workspace arm (a
/// `workspace_users` AND `conversation_users` row), `u-dp` reads `c-dp` through the
/// direct-pair endpoint, and `c-hidden` (workspace `w2`, which `u-member` is not in)
/// is readable by neither.
const SEED: &str = concat!(
    "INSERT INTO users (id, name, display_name, created_at, updated_at) VALUES ('u-creator','u-creator','Creator',1,1), ('u-member','u-member','Member',1,1), ('u-dp','u-dp','DirectPair',1,1), ('u-outsider','u-outsider','Outsider',1,1); ",
    "INSERT INTO hosts (id, name, kind, created_at, user_id, provisioning_source) VALUES ('h1','h1','local',1,'u-creator','app_install'); ",
    seed_agent!(
        id = "a-agentscope",
        display_name = "AgentScope",
        host_id = "h1",
        created_by_user_id = "u-creator",
        session_scope = "agent",
    ),
    "; ",
    seed_agent!(
        id = "a-convscope",
        display_name = "ConvScope",
        host_id = "h1",
        created_by_user_id = "u-creator",
    ),
    "; ",
    "INSERT INTO workspaces (id, name, name_key, created_at, updated_at) VALUES ('w1','W1','w1',1,1), ('w2','W2','w2',1,1); ",
    "INSERT INTO workspace_users (workspace_id, user_id, joined_at) VALUES ('w1','u-member',1), ('w1','u-creator',1), ('w2','u-outsider',1); ",
    "INSERT INTO workspace_members (workspace_id, agent_id, joined_at) VALUES ('w1','a-agentscope',1), ('w2','a-agentscope',1), ('w1','a-convscope',1); ",
    seed_channel!(
        id = "c-ws",
        workspace_id = "w1",
        title = "General",
        title_key = "general",
        updated_at = 300,
    ),
    "; ",
    seed_channel!(
        id = "c-arch",
        workspace_id = "w1",
        title = "Archived",
        title_key = "archived-ch",
        updated_at = 200,
        archived_at = 50,
    ),
    "; ",
    seed_channel!(
        id = "c-recent",
        workspace_id = "w1",
        title = "Recent",
        title_key = "recent-ch",
        updated_at = 400,
    ),
    "; ",
    seed_channel!(
        id = "c-hidden",
        workspace_id = "w2",
        title = "Hidden",
        title_key = "hidden-ch",
        updated_at = 350,
    ),
    "; ",
    seed_channel!(
        id = "c-cv1",
        workspace_id = "w1",
        title = "Conv One",
        title_key = "conv-one",
        updated_at = 110,
    ),
    "; ",
    seed_channel!(
        id = "c-cv2",
        workspace_id = "w1",
        title = "Conv Two",
        title_key = "conv-two",
        updated_at = 120,
    ),
    "; ",
    seed_direct_pair!(
        id = "c-dp",
        endpoint_a_kind = "agent",
        endpoint_a_id = "a-agentscope",
        endpoint_b_kind = "human",
        endpoint_b_id = "u-dp",
        updated_at = 500,
    ),
    "; ",
    "INSERT INTO conversation_agents (conversation_id, agent_id, joined_at) VALUES ('c-ws','a-agentscope',1), ('c-arch','a-agentscope',1), ('c-recent','a-agentscope',1), ('c-hidden','a-agentscope',1), ('c-cv1','a-convscope',1), ('c-cv2','a-convscope',1); ",
    "INSERT INTO conversation_users (conversation_id, user_id, joined_at) VALUES ('c-ws','u-member',1), ('c-arch','u-member',1), ('c-recent','u-member',1), ('c-cv1','u-member',1), ('c-cv1','u-creator',1), ('c-cv2','u-creator',1); ",
    "INSERT INTO messages (id, conversation_id, from_kind, content, seq, created_at) VALUES ('m1','c-ws','user','one',1,1), ('m2','c-ws','user','two',2,1), ('m3','c-arch','user','three',3,1), ('m4','c-hidden','user','four',4,1), ('m5','c-cv1','user','five',5,1), ('m6','c-cv1','user','six',6,1), ('m7','c-cv1','user','seven',7,1), ('m8','c-cv2','user','eight',8,1), ('m9','c-hidden','user','nine',9,1); ",
    "INSERT INTO conversation_inboxes (conversation_id, agent_id, message_id, delivered_at, failed_at, created_at) VALUES ('c-ws','a-agentscope','m1',NULL,NULL,1), ('c-ws','a-agentscope','m2',NULL,NULL,1), ('c-arch','a-agentscope','m3',10,NULL,1), ('c-hidden','a-agentscope','m4',NULL,20,1), ('c-hidden','a-agentscope','m9',NULL,NULL,1), ('c-cv1','a-convscope','m5',NULL,NULL,1), ('c-cv1','a-convscope','m6',NULL,NULL,1), ('c-cv1','a-convscope','m7',30,NULL,1), ('c-cv2','a-convscope','m8',NULL,NULL,1); ",
    "DELETE FROM agent_runtime_slots; ",
    "INSERT INTO agent_runtime_slots (agent_id, conversation_id, generation, observed_status, observed_pid, cli_session_id) VALUES ('a-agentscope',NULL,5,'active',999,'cli-a'), ('a-convscope','c-cv1',3,'idle',NULL,NULL), ('a-convscope','c-cv2',4,'starting',NULL,NULL); ",
);

async fn seeded() -> (tempfile::TempDir, SqliteAgentRuntimeStateColdRead) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(DATABASE_FILE);
    let mut db = crate::current_schema::create_current_schema(&path).await;
    db.execute(sqlx::raw_sql(SEED))
        .await
        .expect("seed must load against the current schema");
    db.close().await.unwrap();
    let cold_read = SqliteAgentRuntimeStateColdRead::open(&path).await.unwrap();
    (directory, cold_read)
}

async fn state(
    cold: &SqliteAgentRuntimeStateColdRead,
    agent: &str,
    viewer: &str,
) -> AgentRuntimeStateView {
    cold.read_agent_runtime_state(
        &AgentId::parse(agent).unwrap(),
        &UserId::parse(viewer).unwrap(),
    )
    .await
    .expect("read must not fault")
    .expect("agent present")
    .project(PresentationFocus::None)
}

/// The participation list is redacted to what the viewer may read and ordered
/// most-recently-updated first. `u-member` reads the three `w1` channels
/// through the workspace arm; `c-hidden` (a `w2` channel it is not in) and `c-dp` (a
/// pair it is not an endpoint of) are dropped.
#[tokio::test]
async fn participations_are_redacted_and_ordered() {
    let (_dir, cold) = seeded().await;
    let view = state(&cold, "a-agentscope", "u-member").await;

    let ids: Vec<&str> = view
        .participations
        .iter()
        .map(|row| row.conversation.as_str())
        .collect();
    assert_eq!(ids, ["c-recent", "c-ws", "c-arch"]);

    // Archived drops from the visible list.
    let visible: Vec<&str> = view
        .visible_participations
        .iter()
        .map(|row| row.conversation.as_str())
        .collect();
    assert_eq!(visible, ["c-recent", "c-ws"]);

    // Readable participation echoes the stored workspace fields.
    let row = view
        .participations
        .iter()
        .find(|row| row.conversation.as_str() == "c-ws")
        .expect("c-ws present");
    assert_eq!(row.workspace_id.as_deref(), Some("w1"));
    assert_eq!(row.workspace_name.as_deref(), Some("W1"));
    assert_eq!(row.placement_kind, "workspace");
    assert_eq!(row.channel_title, "General");
}

/// The direct-pair arm: `u-dp` reads its pair through the endpoint, and nothing else
/// — the workspace channels it is not in are dropped. Proves the redaction is the
/// human-participation authority's TWO arms, not a workspace-only shortcut.
#[tokio::test]
async fn the_direct_pair_arm_admits_only_its_endpoint() {
    let (_dir, cold) = seeded().await;
    let view = state(&cold, "a-agentscope", "u-dp").await;

    let ids: Vec<&str> = view
        .participations
        .iter()
        .map(|row| row.conversation.as_str())
        .collect();
    assert_eq!(ids, ["c-dp"]);
}

/// The agent-scope seat's queue depth is the agent-WIDE undelivered total across all
/// conversations — not per-conversation and not viewer-redacted. Two visible pending
/// rows and one hidden pending row count; delivered and failed rows remain excluded.
#[tokio::test]
async fn the_agent_seat_queue_is_the_agent_wide_undelivered_total() {
    let (_dir, cold) = seeded().await;
    let facts = cold
        .read_agent_runtime_state(
            &AgentId::parse("a-agentscope").unwrap(),
            &UserId::parse("u-member").unwrap(),
        )
        .await
        .unwrap()
        .expect("agent present");
    assert_eq!(facts.source(), &AgentId::parse("a-agentscope").unwrap());
    assert_eq!(
        facts.agent_scope_generation(),
        Some(RuntimeGeneration::new(5).unwrap())
    );
    let view = facts.project(PresentationFocus::None);

    assert_eq!(view.seats.len(), 1);
    let seat = &view.seats[0];
    assert_eq!(seat.scope, RuntimeSeatScopeView::Agent);
    assert_eq!(seat.queue_depth, 3);
    assert_eq!(seat.generation, RuntimeGeneration::new(5).unwrap());
    assert_eq!(seat.status, RuntimeObservedStatus::Active);
    assert_eq!(seat.pid, Some(999));
    assert_eq!(seat.cli_session_id.as_deref(), Some("cli-a"));
    // No live focus supplied ⇒ null, never a fabricated conversation.
    assert_eq!(seat.focus, None);
}

/// A conversation seat the viewer cannot read is dropped WHOLE; a readable one
/// survives and carries its own conversation's undelivered count (two of three rows;
/// the delivered one excluded), focused on its own conversation. Proves per-conversation
/// queue scoping and seat redaction together.
#[tokio::test]
async fn conversation_seats_are_redacted_and_counted_per_conversation() {
    let (_dir, cold) = seeded().await;
    let view = state(&cold, "a-convscope", "u-member").await;

    assert_eq!(view.seats.len(), 1, "c-cv2 seat is redacted away");
    let seat = &view.seats[0];
    assert_eq!(
        seat.scope,
        RuntimeSeatScopeView::Conversation(ConversationId::parse("c-cv1").unwrap())
    );
    assert_eq!(seat.queue_depth, 2);
    assert_eq!(
        seat.focus,
        Some(RuntimeFocusView::Conversation(
            ConversationId::parse("c-cv1").unwrap()
        ))
    );
}

/// A viewer who reads BOTH conversation seats sees each counted against its OWN
/// conversation — `c-cv2`'s single undelivered row does not bleed into `c-cv1`'s two.
#[tokio::test]
async fn per_conversation_queue_counts_do_not_bleed() {
    let (_dir, cold) = seeded().await;
    let view = state(&cold, "a-convscope", "u-creator").await;

    let mut depths: Vec<(String, i64)> = view
        .seats
        .iter()
        .map(|seat| match &seat.scope {
            RuntimeSeatScopeView::Conversation(id) => (id.as_str().to_owned(), seat.queue_depth),
            RuntimeSeatScopeView::Agent => ("agent".to_owned(), seat.queue_depth),
        })
        .collect();
    depths.sort();
    assert_eq!(
        depths,
        vec![("c-cv1".to_owned(), 2), ("c-cv2".to_owned(), 1)]
    );
}

/// An absent agent is the ACL's absence arm — `None`, never a fault.
#[tokio::test]
async fn an_absent_agent_reads_none() {
    let (_dir, cold) = seeded().await;
    let facts = cold
        .read_agent_runtime_state(
            &AgentId::parse("a-missing").unwrap(),
            &UserId::parse("u-member").unwrap(),
        )
        .await
        .unwrap();
    assert!(facts.is_none());
}

/// The post-snapshot focus-readability probe reuses the SAME two-arm authority: the
/// workspace arm admits `u-member` on `c-ws`, the direct-pair arm admits `u-dp` on
/// `c-dp`, and a non-member is refused.
#[tokio::test]
async fn conversation_visibility_reuses_both_arms() {
    let (_dir, cold) = seeded().await;

    let readable = |viewer: &'static str, conversation: &'static str| {
        let cold = cold.clone();
        async move {
            cold.read_conversation_visibility(
                &UserId::parse(viewer).unwrap(),
                &ConversationId::parse(conversation).unwrap(),
            )
            .await
            .unwrap()
        }
    };

    assert!(readable("u-member", "c-ws").await, "workspace arm");
    assert!(readable("u-dp", "c-dp").await, "direct-pair arm");
    assert!(!readable("u-outsider", "c-ws").await, "non-member refused");
    assert!(
        !readable("u-member", "c-hidden").await,
        "workspace non-member refused"
    );
}
