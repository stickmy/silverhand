use silverhand_backend_core::{
    conversation_messaging::messaging::collaboration::stale_message_resolution::{
        StaleMessageResolutionAdmissionPort, StaleMessageResolutionAdmissionRefusal,
    },
    foundation::identity::{AgentId, ConversationId},
};
use silverhand_backend_sqlite::conversation_messaging::messaging::MessagingStore;
use sqlx::{Connection, Executor, SqliteConnection};

use crate::current_schema;

const AGENT: &str = "stale-agent";
const WORKSPACE_CONVERSATION: &str = "stale-workspace";
const DIRECT_PAIR: &str = "stale-pair";
const MISSING_PEER_PAIR: &str = "stale-missing-peer";

async fn seed(path: &std::path::Path) {
    let mut db = current_schema::create_current_schema(path).await;
    db.execute(
        "INSERT INTO hosts(id,name,kind,created_at) VALUES \
         ('stale-host','Stale Host','local',1)",
    )
    .await
    .unwrap();
    db.execute(
        "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES \
         ('stale-peer','stale-peer','Stale Peer',1,1),\
         ('stale-ghost','stale-ghost','Stale Ghost',1,1)",
    )
    .await
    .unwrap();
    db.execute(
        "INSERT INTO agents(id,name,runtime,host_id,created_at,updated_at,display_name,\
         created_by_user_id,materialization_status,session_scope) VALUES \
         ('stale-agent','stale-agent','codex','stale-host',1,1,'Stale Agent','stale-peer',\
          'active','agent')",
    )
    .await
    .unwrap();
    db.execute(
        "INSERT INTO workspaces(id,name,name_key,created_at,updated_at) VALUES \
         ('stale-workspace-id','Stale Workspace','stale workspace',1,1)",
    )
    .await
    .unwrap();
    db.execute(
        "INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,\
         status,visibility,created_at,updated_at) VALUES \
         ('stale-workspace','stale-workspace-id','workspace','open','Stale Channel',\
          'stale channel','idle','private',1,1)",
    )
    .await
    .unwrap();
    db.execute(
        "INSERT INTO conversation_agents(conversation_id,agent_id,joined_at) VALUES \
         ('stale-workspace','stale-agent',1)",
    )
    .await
    .unwrap();
    db.execute(
        "INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,endpoint_a_kind,\
         endpoint_a_id,endpoint_b_kind,endpoint_b_id,title,title_key,status,visibility,created_at,\
         updated_at) VALUES \
         ('stale-pair',NULL,'direct_pair','fixed','agent','stale-agent','human','stale-peer','',\
          NULL,'idle','private',1,1),\
         ('stale-missing-peer',NULL,'direct_pair','fixed','agent','stale-agent','human',\
          'stale-ghost','',NULL,'idle','private',1,1)",
    )
    .await
    .unwrap();
    db.close().await.unwrap();
}

fn agent() -> AgentId {
    AgentId::parse(AGENT).unwrap()
}

fn conversation(raw: &str) -> ConversationId {
    ConversationId::parse(raw).unwrap()
}

#[tokio::test]
async fn workspace_and_live_pair_return_actual_source_coordinates() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stale-resolution-admission.sqlite");
    seed(&path).await;
    let admission = MessagingStore::open(&path)
        .await
        .unwrap()
        .stale_message_resolution_admission();

    for id in [WORKSPACE_CONVERSATION, DIRECT_PAIR] {
        let requested = conversation(id);
        let source = admission
            .admit(&agent(), &requested)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            source
                .clone()
                .bind(&agent(), &requested)
                .unwrap()
                .into_parts(),
            (agent(), requested.clone()),
            "{id}"
        );
        assert!(
            source
                .clone()
                .bind(&AgentId::parse("other-agent").unwrap(), &requested)
                .is_err(),
            "{id}"
        );
        assert!(
            source
                .bind(&agent(), &conversation("other-conversation"))
                .is_err(),
            "{id}"
        );
    }

    assert_eq!(
        admission
            .admit(
                &AgentId::parse("other-agent").unwrap(),
                &conversation(WORKSPACE_CONVERSATION)
            )
            .await
            .unwrap()
            .err(),
        Some(StaleMessageResolutionAdmissionRefusal::Forbidden)
    );
    assert_eq!(
        admission
            .admit(&agent(), &conversation("missing-conversation"))
            .await
            .unwrap()
            .err(),
        Some(StaleMessageResolutionAdmissionRefusal::Forbidden)
    );
}

#[tokio::test]
async fn half_dead_direct_pairs_refuse_and_repairs_use_the_same_adapter() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stale-resolution-half-dead.sqlite");
    seed(&path).await;
    let admission = MessagingStore::open(&path)
        .await
        .unwrap()
        .stale_message_resolution_admission();
    let missing_peer = conversation(MISSING_PEER_PAIR);
    let mut db = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    db.execute("PRAGMA foreign_keys=OFF").await.unwrap();
    db.execute("DROP TRIGGER trg_users_stamp_endpoint_deleted")
        .await
        .unwrap();
    db.execute("DELETE FROM users WHERE id='stale-ghost'")
        .await
        .unwrap();
    assert_eq!(
        admission
            .admit(&agent(), &missing_peer)
            .await
            .unwrap()
            .err(),
        Some(StaleMessageResolutionAdmissionRefusal::Forbidden)
    );

    db.execute(
        "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES \
         ('stale-ghost','stale-ghost','Stale Ghost',1,1)",
    )
    .await
    .unwrap();
    assert!(
        admission
            .admit(&agent(), &missing_peer)
            .await
            .unwrap()
            .is_ok()
    );

    db.execute("UPDATE conversations SET endpoint_b_deleted_at=2 WHERE id='stale-pair'")
        .await
        .unwrap();
    assert_eq!(
        admission
            .admit(&agent(), &conversation(DIRECT_PAIR))
            .await
            .unwrap()
            .err(),
        Some(StaleMessageResolutionAdmissionRefusal::Forbidden)
    );
    db.execute("DROP TRIGGER trg_conversations_endpoint_tombstone_monotonic")
        .await
        .unwrap();
    db.execute("UPDATE conversations SET endpoint_b_deleted_at=NULL WHERE id='stale-pair'")
        .await
        .unwrap();
    assert!(
        admission
            .admit(&agent(), &conversation(DIRECT_PAIR))
            .await
            .unwrap()
            .is_ok()
    );
}
