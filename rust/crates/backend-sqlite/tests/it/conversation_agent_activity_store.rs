use silverhand_backend_core::{
    conversation_messaging::messaging::collaboration::conversation_agent_activity::{
        ActivityPageRequest, ConversationAgentActivityReadPort, ConversationAgentActivityRefusal,
    },
    foundation::identity::{AgentId, ConversationId, UserId},
};
use silverhand_backend_sqlite::conversation_messaging::messaging::{
    MessagingStore, collaboration::conversation_agent_activity::ConversationAgentActivityStoreError,
};
use sqlx::{Connection, Executor};

use crate::current_schema;

async fn seed(path: &std::path::Path) {
    let mut db = current_schema::create_current_schema(path).await;
    for statement in [
        "INSERT INTO hosts(id,name,kind,created_at) VALUES('h','host','local',1)",
        "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES \
         ('member','activity-member','Member',1,1),('browser','activity-browser','Browser',1,1),\
         ('other','activity-other','Other',1,1)",
        "INSERT INTO agents(id,name,runtime,host_id,created_by_user_id,created_at,updated_at,display_name) VALUES \
         ('target','activity-target','codex','h','member',1,1,'Target'),\
         ('stranger','activity-stranger','codex','h','member',1,1,'Stranger')",
        "INSERT INTO workspaces(id,name,name_key,created_at,updated_at) VALUES \
         ('w','Workspace','activity-workspace',1,1)",
        "INSERT INTO workspace_users(workspace_id,user_id,joined_at,role) VALUES \
         ('w','member',1,'member'),('w','browser',1,'member')",
        "INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,status,visibility,created_at,updated_at) VALUES \
         ('private','w','workspace','open','Private','private','idle','private',1,1),\
         ('public','w','workspace','open','Public','public','idle','public',1,1)",
        "INSERT INTO conversation_users(conversation_id,user_id,joined_at) VALUES ('private','member',1)",
        "INSERT INTO conversation_agents(conversation_id,agent_id,joined_at) VALUES \
         ('private','target',1),('public','target',1)",
        "INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,endpoint_a_kind,endpoint_a_id,endpoint_b_kind,endpoint_b_id,title,title_key,status,visibility,created_at,updated_at) VALUES \
         ('pair',NULL,'direct_pair','fixed','agent','target','human','member','',NULL,'idle','private',1,1)",
        "INSERT INTO conversation_events(id,conversation_id,agent_id,kind,payload,created_at) VALUES \
         (1,'private','target','text','{\"text\":\"one\"}',11),\
         (2,'private','target','text','{\"text\":\"two\"}',12),\
         (3,'private','target','turn_end',NULL,13),\
         (4,'pair','target','text','{\"text\":\"pair\"}',14)",
    ] {
        db.execute(statement).await.unwrap();
    }
    db.close().await.unwrap();
}

async fn read(
    path: &std::path::Path,
    viewer: &str,
    conversation: &str,
    agent: &str,
    limit: u16,
    before: Option<i64>,
) -> Result<
    Result<
        silverhand_backend_core::conversation_messaging::messaging::collaboration::conversation_agent_activity::ConversationAgentActivityPage,
        ConversationAgentActivityRefusal,
    >,
    silverhand_backend_core::foundation::persistence::PersistenceFailure,
>{
    let before = before.map(|value| {
        silverhand_backend_core::conversation_messaging::messaging::collaboration::conversation_agent_activity::ActivityBefore::Persisted(
            silverhand_backend_core::conversation_messaging::messaging::collaboration::conversation_agent_activity::ActivityEventId::new(value)
                .unwrap(),
        )
    });
    MessagingStore::open(path)
        .await
        .unwrap()
        .conversation_agent_activity()
        .read(
            &UserId::parse(viewer).unwrap(),
            &ConversationId::parse(conversation).unwrap(),
            &AgentId::parse(agent).unwrap(),
            ActivityPageRequest::new(limit, before).unwrap(),
        )
        .await
}

fn storage_source(
    error: &silverhand_backend_core::foundation::persistence::PersistenceFailure,
) -> &ConversationAgentActivityStoreError {
    std::error::Error::source(error)
        .and_then(|source| source.downcast_ref())
        .expect("activity failures preserve their named store source")
}

#[tokio::test]
async fn page_order_cursor_visibility_and_target_participation_match_the_product_faces() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("conversation-agent-activity.sqlite");
    seed(&path).await;

    let first = read(&path, "member", "private", "target", 2, None)
        .await
        .unwrap()
        .unwrap()
        .bind(
            &UserId::parse("member").unwrap(),
            &ConversationId::parse("private").unwrap(),
            &AgentId::parse("target").unwrap(),
        )
        .unwrap();
    assert_eq!(
        first
            .events()
            .iter()
            .map(|event| event.id().get())
            .collect::<Vec<_>>(),
        [2, 3]
    );
    assert_eq!(first.next_before().unwrap().get(), 2);

    let second = read(&path, "member", "private", "target", 2, Some(2))
        .await
        .unwrap()
        .unwrap()
        .bind(
            &UserId::parse("member").unwrap(),
            &ConversationId::parse("private").unwrap(),
            &AgentId::parse("target").unwrap(),
        )
        .unwrap();
    assert_eq!(second.events()[0].id().get(), 1);
    assert!(second.next_before().is_none());

    assert_eq!(
        read(&path, "member", "missing", "target", 2, None)
            .await
            .unwrap()
            .unwrap_err(),
        ConversationAgentActivityRefusal::NotFound
    );
    assert_eq!(
        read(&path, "browser", "private", "target", 2, None)
            .await
            .unwrap()
            .unwrap_err(),
        ConversationAgentActivityRefusal::Forbidden
    );
    assert_eq!(
        read(&path, "member", "private", "stranger", 2, None)
            .await
            .unwrap()
            .unwrap_err(),
        ConversationAgentActivityRefusal::TargetNotParticipant
    );
    assert!(
        read(&path, "browser", "public", "target", 2, None)
            .await
            .unwrap()
            .is_ok()
    );
    assert!(
        read(&path, "member", "pair", "target", 2, None)
            .await
            .unwrap()
            .is_ok()
    );
}

#[tokio::test]
async fn lookahead_rows_are_fully_decoded_before_page_slicing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir
        .path()
        .join("conversation-agent-activity-corrupt.sqlite");
    seed(&path).await;
    let mut db = sqlx::SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    db.execute("PRAGMA ignore_check_constraints=ON")
        .await
        .unwrap();

    db.execute("UPDATE conversation_events SET payload='{' WHERE id=2")
        .await
        .unwrap();
    let error = read(&path, "member", "private", "target", 1, None)
        .await
        .unwrap_err();
    assert!(matches!(
        storage_source(&error),
        ConversationAgentActivityStoreError::Payload
    ));
    db.execute("UPDATE conversation_events SET payload='{\"text\":\"two\"}' WHERE id=2")
        .await
        .unwrap();

    db.execute("UPDATE conversation_events SET kind='future_kind' WHERE id=2")
        .await
        .unwrap();
    let error = read(&path, "member", "private", "target", 1, None)
        .await
        .unwrap_err();
    assert!(matches!(
        storage_source(&error),
        ConversationAgentActivityStoreError::Kind
    ));
    db.execute("UPDATE conversation_events SET kind='text',created_at=-1 WHERE id=2")
        .await
        .unwrap();

    let error = read(&path, "member", "private", "target", 1, None)
        .await
        .unwrap_err();
    assert!(matches!(
        storage_source(&error),
        ConversationAgentActivityStoreError::CreatedAt
    ));
    db.execute("UPDATE conversation_events SET created_at=12 WHERE id=2")
        .await
        .unwrap();
    assert!(
        read(&path, "member", "private", "target", 1, None)
            .await
            .unwrap()
            .is_ok()
    );
}

#[tokio::test]
async fn agent_stream_uses_existing_viewer_arms_without_channel_slicing_and_rechecks_revocation() {
    use silverhand_backend_core::{
        conversation_messaging::messaging::collaboration::conversation_agent_activity::{
            ActivityBefore, AgentActivityReadPort,
        },
        foundation::scope_event::{AuthorizeEventScope, EventScope},
    };
    use silverhand_backend_sqlite::foundation::scope_event::SqliteEventScopeAuthority;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agent-stream.sqlite");
    seed(&path).await;
    let mut db = sqlx::SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    db.execute("PRAGMA foreign_keys=ON").await.unwrap();
    db.execute("INSERT INTO agents(id,name,runtime,host_id,created_by_user_id,session_scope,created_at,updated_at,display_name) VALUES ('stream','stream','codex','h','member','agent',1,1,'Stream')").await.unwrap();
    db.execute(
        "INSERT INTO workspace_members(workspace_id,agent_id,joined_at) VALUES ('w','stream',1)",
    )
    .await
    .unwrap();
    db.execute("INSERT INTO conversation_events(conversation_id,agent_id,kind,payload,created_at) VALUES ('private','stream','thinking','{\"text\":\"private thought\"}',1),('public','stream','tool_call','{\"name\":\"shell\",\"input\":{}}',2),('pair','stream','tool_output','{\"outcome\":\"output\"}',3)").await.unwrap();
    let port = MessagingStore::open(&path)
        .await
        .unwrap()
        .conversation_agent_activity();
    let agent = AgentId::parse("stream").unwrap();
    let viewer = UserId::parse("browser").unwrap(); // workspace member, no channel membership
    let first = port
        .read_agent_activity(&viewer, &agent, ActivityPageRequest::new(2, None).unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.events().len(), 2);
    assert_eq!(first.events()[0].conversation().as_str(), "public");
    assert_eq!(first.events()[1].conversation().as_str(), "pair");
    let prior = port
        .read_agent_activity(
            &viewer,
            &agent,
            ActivityPageRequest::new(
                2,
                Some(ActivityBefore::Persisted(first.next_before().unwrap())),
            )
            .unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(prior.events().len(), 1);
    assert_eq!(prior.events()[0].conversation().as_str(), "private");
    let authority = SqliteEventScopeAuthority::open(&path).await.unwrap();
    assert!(
        authority
            .authorize_event_scope(&viewer, &EventScope::Agent(agent.clone()))
            .await
            .unwrap()
    );
    assert!(
        port.read_agent_activity(
            &UserId::parse("other").unwrap(),
            &agent,
            ActivityPageRequest::new(10, None).unwrap()
        )
        .await
        .unwrap()
        .is_none()
    );
    db.execute("DELETE FROM workspace_users WHERE user_id='browser'")
        .await
        .unwrap();
    assert!(
        port.read_agent_activity(&viewer, &agent, ActivityPageRequest::new(10, None).unwrap())
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        !authority
            .authorize_event_scope(&viewer, &EventScope::Agent(agent.clone()))
            .await
            .unwrap()
    );
    // Creator remains admitted after workspace revocation; a human direct pair is its own arm.
    assert!(
        port.read_agent_activity(
            &UserId::parse("member").unwrap(),
            &agent,
            ActivityPageRequest::new(10, None).unwrap()
        )
        .await
        .unwrap()
        .is_some()
    );
    db.execute("INSERT INTO conversations(id,placement_kind,roster_policy,endpoint_a_kind,endpoint_a_id,endpoint_b_kind,endpoint_b_id,title,status,visibility,created_at,updated_at) VALUES ('stream-dm','direct_pair','fixed','agent','stream','human','browser','','idle','private',1,1)").await.unwrap();
    assert!(
        authority
            .authorize_event_scope(&viewer, &EventScope::Agent(agent.clone()))
            .await
            .unwrap()
    );
    db.execute("DELETE FROM conversations WHERE id='stream-dm'")
        .await
        .unwrap();
    assert!(
        !authority
            .authorize_event_scope(&viewer, &EventScope::Agent(agent.clone()))
            .await
            .unwrap()
    );
    assert!(
        !authority
            .authorize_event_scope(
                &viewer,
                &EventScope::Agent(AgentId::parse("target").unwrap())
            )
            .await
            .unwrap()
    );
    // No backfill or reassignment: original rows retain their source and count.
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM conversation_events WHERE agent_id='target'")
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(count, 4);
    use sqlx::Row;
    let plan=sqlx::query("EXPLAIN QUERY PLAN SELECT id FROM conversation_events WHERE agent_id='stream' ORDER BY id DESC LIMIT 3").fetch_all(&mut db).await.unwrap();
    let details: Vec<String> = plan.iter().map(|row| row.get("detail")).collect();
    assert!(
        details
            .iter()
            .any(|detail| detail.contains("SEARCH conversation_events")
                && detail.contains("agent_id=?")),
        "{details:?}"
    );
    assert!(
        !details.iter().any(|detail| detail.contains("TEMP B-TREE")),
        "{details:?}"
    );
}

#[tokio::test]
async fn retention_seven_day_boundary_shared_stream_and_expired_page_cursor() {
    use silverhand_backend_core::conversation_messaging::messaging::collaboration::conversation_agent_activity::{
        ActivityBefore, ActivityEventId, AgentActivityReadPort,
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("retention.sqlite");
    seed(&path).await;
    let mut db = sqlx::SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    let now = 604_800_000 + 100; // Seven days, independent of the production retention constant.
    // Deliberately non-monotone timestamps: retention is time-based, not an id cutoff.
    db.execute("UPDATE conversation_events SET created_at=CASE id WHEN 1 THEN 100 WHEN 2 THEN 99 WHEN 3 THEN 101 ELSE 99 END").await.unwrap();
    db.execute("INSERT INTO agents(id,name,runtime,host_id,created_by_user_id,session_scope,created_at,updated_at) VALUES ('stream','stream','codex','h','member','agent',1,1)").await.unwrap();
    db.execute("INSERT INTO conversation_events(id,conversation_id,agent_id,kind,payload,created_at) VALUES (5,'private','stream','thinking','{\"text\":\"expired\"}',99),(6,'public','stream','text','{\"text\":\"boundary\"}',100),(7,'pair','stream','tool_output',NULL,101)").await.unwrap();
    db.execute("INSERT INTO messages(id,conversation_id,from_kind,from_user_id,content,seq,created_at,author_id,author_name) VALUES('kept-message','private','user','member','keep',1,1,'member','member')").await.unwrap();
    let store = MessagingStore::open(&path).await.unwrap();
    let port = store.conversation_agent_activity();
    assert_eq!(store.prune_expired_activity(now).await.unwrap(), 3);
    assert_eq!(store.prune_expired_activity(now).await.unwrap(), 0);
    let ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM conversation_events ORDER BY id")
        .fetch_all(&mut db)
        .await
        .unwrap();
    assert_eq!(ids, [1, 3, 6, 7]);
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT content FROM messages WHERE id='kept-message'")
            .fetch_one(&mut db)
            .await
            .unwrap(),
        "keep"
    );
    let page = read(&path, "member", "private", "target", 10, Some(2))
        .await
        .unwrap()
        .unwrap()
        .bind(
            &UserId::parse("member").unwrap(),
            &ConversationId::parse("private").unwrap(),
            &AgentId::parse("target").unwrap(),
        )
        .unwrap();
    assert_eq!(
        page.events()
            .iter()
            .map(|e| e.id().get())
            .collect::<Vec<_>>(),
        [1]
    );
    // The removed cursor itself is not looked up; no deleted row is resurrected.
    let page = port
        .read_agent_activity(
            &UserId::parse("member").unwrap(),
            &AgentId::parse("stream").unwrap(),
            ActivityPageRequest::new(
                10,
                Some(ActivityBefore::Persisted(ActivityEventId::new(5).unwrap())),
            )
            .unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
    assert!(page.events().is_empty());
    let page = port
        .read_agent_activity(
            &UserId::parse("member").unwrap(),
            &AgentId::parse("stream").unwrap(),
            ActivityPageRequest::new(10, None).unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        page.events()
            .iter()
            .map(|e| e.id().get())
            .collect::<Vec<_>>(),
        [6, 7]
    );
    assert_eq!(store.prune_expired_activity(now + 1).await.unwrap(), 2); // boundary now expired
    assert_eq!(store.prune_expired_activity(now + 2).await.unwrap(), 2); // entire table now empty
    db.execute("INSERT INTO conversation_events(conversation_id,agent_id,kind,payload,created_at) VALUES('public','stream','turn_end',NULL,102)").await.unwrap();
    assert!(
        sqlx::query_scalar::<_, i64>("SELECT MAX(id) FROM conversation_events")
            .fetch_one(&mut db)
            .await
            .unwrap()
            > 7
    );
    assert!(
        port.read_agent_activity(
            &UserId::parse("other").unwrap(),
            &AgentId::parse("stream").unwrap(),
            ActivityPageRequest::new(10, None).unwrap()
        )
        .await
        .unwrap()
        .is_none()
    );
}
