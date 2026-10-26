use silverhand_backend_core::{
    conversation_messaging::messaging::collaboration::conversation_agent_runtime::{
        ConversationAgentRuntimeReadPort, ConversationAgentRuntimeRefusal,
    },
    foundation::identity::{ConversationId, UserId},
};
use silverhand_backend_sqlite::conversation_messaging::messaging::{
    MessagingStore, collaboration::conversation_agent_runtime::ConversationAgentRuntimeStoreError,
};
use sqlx::{Connection, Executor};

use crate::current_schema;

async fn seed(path: &std::path::Path) {
    let mut db = current_schema::create_current_schema(path).await;
    for statement in [
        "INSERT INTO hosts(id,name,kind,created_at) VALUES('h','host','local',1)",
        "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES \
         ('member','runtime-member','Member',1,1),('browser','runtime-browser','Browser',1,1),\
         ('other','runtime-other','Other',1,1)",
        "INSERT INTO agents(id,name,runtime,host_id,created_by_user_id,created_at,updated_at,display_name,session_scope) VALUES \
         ('a-conversation','runtime-conversation','codex','h','member',1,1,'Conversation','conversation'),\
         ('z-global','runtime-global','codex','h','member',1,1,'Global','agent')",
        "INSERT INTO workspaces(id,name,name_key,created_at,updated_at) VALUES \
         ('w','Workspace','runtime-workspace',1,1)",
        "INSERT INTO workspace_users(workspace_id,user_id,joined_at,role) VALUES \
         ('w','member',1,'member'),('w','browser',1,'member')",
        "INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,status,visibility,created_at,updated_at) VALUES \
         ('private','w','workspace','open','Private','private','idle','private',1,1),\
         ('public','w','workspace','open','Public','public','idle','public',1,1)",
        "INSERT INTO conversation_users(conversation_id,user_id,joined_at) VALUES ('private','member',1)",
        "INSERT INTO conversation_agents(conversation_id,agent_id,joined_at) VALUES \
         ('private','z-global',1),('private','a-conversation',1),('public','a-conversation',1)",
        "INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,endpoint_a_kind,endpoint_a_id,endpoint_b_kind,endpoint_b_id,title,title_key,status,visibility,created_at,updated_at) VALUES \
         ('pair',NULL,'direct_pair','fixed','agent','z-global','human','member','',NULL,'idle','private',1,1)",
        "UPDATE agent_runtime_slots SET observed_status='active',observed_pid=123,cli_session_id='cli-c' \
         WHERE agent_id='a-conversation' AND conversation_id='private'",
        "UPDATE agent_runtime_slots SET observed_status='starting',observed_pid=456,cli_session_id='cli-g' \
         WHERE agent_id='z-global' AND conversation_id IS NULL",
    ] {
        db.execute(statement).await.unwrap();
    }
    db.close().await.unwrap();
}

async fn read(
    path: &std::path::Path,
    viewer: &str,
    conversation: &str,
) -> Result<
    Result<
        silverhand_backend_core::conversation_messaging::messaging::collaboration::conversation_agent_runtime::ConversationAgentRuntimeProjection,
        ConversationAgentRuntimeRefusal,
    >,
    silverhand_backend_core::foundation::persistence::PersistenceFailure,
>{
    MessagingStore::open(path)
        .await
        .unwrap()
        .conversation_agent_runtime()
        .read(
            &UserId::parse(viewer).unwrap(),
            &ConversationId::parse(conversation).unwrap(),
        )
        .await
}

fn source(
    error: &silverhand_backend_core::foundation::persistence::PersistenceFailure,
) -> &ConversationAgentRuntimeStoreError {
    std::error::Error::source(error)
        .and_then(|source| source.downcast_ref())
        .expect("runtime projection failures preserve their named store source")
}

#[tokio::test]
async fn roster_population_runtime_scope_order_and_visibility_are_independent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("conversation-agent-runtime.sqlite");
    seed(&path).await;

    let page = read(&path, "member", "private")
        .await
        .unwrap()
        .unwrap()
        .bind(
            &UserId::parse("member").unwrap(),
            &ConversationId::parse("private").unwrap(),
        )
        .unwrap();
    assert_eq!(
        page.rows()
            .iter()
            .map(|row| (
                row.agent().as_str(),
                row.scope_kind(),
                row.status().as_str()
            ))
            .collect::<Vec<_>>(),
        [
            ("a-conversation", "conversation", "active"),
            ("z-global", "agent", "starting")
        ]
    );
    assert_eq!(page.rows()[0].pid().unwrap().get(), 123);
    assert_eq!(page.rows()[0].cli_session().unwrap().as_str(), "cli-c");

    assert_eq!(
        read(&path, "member", "missing").await.unwrap().unwrap_err(),
        ConversationAgentRuntimeRefusal::NotFound
    );
    assert_eq!(
        read(&path, "browser", "private")
            .await
            .unwrap()
            .unwrap_err(),
        ConversationAgentRuntimeRefusal::Forbidden
    );
    assert!(read(&path, "browser", "public").await.unwrap().is_ok());

    // Endpoint participation admits the viewer, but never manufactures an
    // agent roster. The agent-scoped endpoint has a slot and is still absent.
    let pair = read(&path, "member", "pair")
        .await
        .unwrap()
        .unwrap()
        .bind(
            &UserId::parse("member").unwrap(),
            &ConversationId::parse("pair").unwrap(),
        )
        .unwrap();
    assert!(pair.rows().is_empty());
}

#[tokio::test]
async fn every_selected_slot_is_strictly_decoded_before_projection() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("conversation-agent-runtime-corrupt.sqlite");
    seed(&path).await;
    let mut db = sqlx::SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    db.execute("PRAGMA ignore_check_constraints=ON")
        .await
        .unwrap();

    db.execute(
        "UPDATE agent_runtime_slots SET observed_status='future',cli_session_id='' \
         WHERE agent_id='a-conversation' AND conversation_id='public'",
    )
    .await
    .unwrap();
    // A malformed slot from another conversation is outside this read face.
    assert!(read(&path, "member", "private").await.unwrap().is_ok());

    db.execute(
        "UPDATE agent_runtime_slots SET observed_status='future' \
         WHERE agent_id='z-global' AND conversation_id IS NULL",
    )
    .await
    .unwrap();
    let error = read(&path, "member", "private").await.unwrap_err();
    assert!(matches!(
        source(&error),
        ConversationAgentRuntimeStoreError::Status
    ));
    db.execute(
        "UPDATE agent_runtime_slots SET observed_status='starting' \
         WHERE agent_id='z-global' AND conversation_id IS NULL",
    )
    .await
    .unwrap();

    db.execute("DROP TRIGGER agent_runtime_slots_generation_monotonic")
        .await
        .unwrap();
    db.execute(
        "UPDATE agent_runtime_slots SET generation=-1 \
         WHERE agent_id='z-global' AND conversation_id IS NULL",
    )
    .await
    .unwrap();
    let error = read(&path, "member", "private").await.unwrap_err();
    assert!(matches!(
        source(&error),
        ConversationAgentRuntimeStoreError::Generation
    ));
    db.execute(
        "UPDATE agent_runtime_slots SET generation=0,cli_session_id='' \
         WHERE agent_id='z-global' AND conversation_id IS NULL",
    )
    .await
    .unwrap();
    let error = read(&path, "member", "private").await.unwrap_err();
    assert!(matches!(
        source(&error),
        ConversationAgentRuntimeStoreError::CliSession
    ));
    db.execute(
        "UPDATE agent_runtime_slots SET cli_session_id='cli-g' \
         WHERE agent_id='z-global' AND conversation_id IS NULL",
    )
    .await
    .unwrap();
    // The foreign malformed slot remains planted; it still cannot poison the
    // requested conversation projection.
    assert!(read(&path, "member", "private").await.unwrap().is_ok());
}

#[tokio::test]
async fn agent_scope_and_slot_coordinate_mismatch_is_named_corruption() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("conversation-agent-runtime-scope.sqlite");
    seed(&path).await;
    let mut db = sqlx::SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    db.execute("DROP TRIGGER agents_session_scope_immutable")
        .await
        .unwrap();
    db.execute("UPDATE agents SET session_scope='conversation' WHERE id='z-global'")
        .await
        .unwrap();
    let error = read(&path, "member", "private").await.unwrap_err();
    assert!(matches!(
        source(&error),
        ConversationAgentRuntimeStoreError::SlotScope
    ));
    db.execute("UPDATE agents SET session_scope='agent' WHERE id='z-global'")
        .await
        .unwrap();
    assert!(read(&path, "member", "private").await.unwrap().is_ok());
}
