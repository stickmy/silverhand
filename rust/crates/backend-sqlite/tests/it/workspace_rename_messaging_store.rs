use std::{
    error::Error,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use silverhand_backend_core::{
    conversation_messaging::messaging::{
        SendClock, SendEntropy,
        collaboration::workspace_rename_messaging::{
            ConversationAgentsReadPort, WorkspaceRenameMessageAuthorPort,
            WorkspaceRenameMessageDraft, author_workspace_rename_message,
            read_workspace_rename_channels,
        },
        environment::EnvironmentUnavailable,
    },
    foundation::identity::{AgentId, ConversationId, WorkspaceId},
    foundation::persistence::PersistenceFailure,
};
use silverhand_backend_sqlite::conversation_messaging::messaging::{
    MessagingStore, collaboration::workspace_rename_messaging::WorkspaceRenameMessagingStoreError,
};
use sqlx::{Connection, Executor, Row};

use crate::current_schema;

#[derive(Clone)]
struct FixedEnvironment {
    entropy_calls: Arc<AtomicUsize>,
}

impl FixedEnvironment {
    fn new() -> Self {
        Self {
            entropy_calls: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl SendClock for FixedEnvironment {
    fn now_unix_ms(&self) -> Result<i64, EnvironmentUnavailable> {
        Ok(77)
    }
}

impl SendEntropy for FixedEnvironment {
    fn fill(&self, buffer: &mut [u8]) -> Result<(), EnvironmentUnavailable> {
        let byte = u8::try_from(self.entropy_calls.fetch_add(1, Ordering::SeqCst) + 1).unwrap();
        buffer.fill(byte);
        Ok(())
    }
}

fn workspace(raw: &str) -> WorkspaceId {
    WorkspaceId::parse(raw).unwrap()
}

fn conversation(raw: &str) -> ConversationId {
    ConversationId::parse(raw).unwrap()
}

fn draft() -> WorkspaceRenameMessageDraft {
    WorkspaceRenameMessageDraft::for_names(
        workspace("w"),
        conversation("active-a"),
        "Old Workspace",
        "New Workspace",
    )
}

async fn seed(path: &std::path::Path) {
    let mut db = current_schema::create_current_schema(path).await;
    for statement in [
        "INSERT INTO workspaces(id,name,name_key,created_at,updated_at) VALUES \
         ('w','Old Workspace','old-workspace',1,1),('other','Other','other',1,1)",
        "INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,status,visibility,created_at,updated_at) VALUES \
         ('active-b','w','workspace','open','B','b','idle','private',1,1),\
         ('active-a','w','workspace','open','A','a','idle','private',1,1),\
         ('fixed','w','workspace','fixed','Fixed','fixed','idle','private',1,1),\
         ('foreign','other','workspace','open','Foreign','foreign','idle','private',1,1)",
        "UPDATE conversations SET archived_at=9 WHERE id='active-b'",
        "INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,status,visibility,created_at,updated_at,closed_at) VALUES \
         ('closed','w','workspace','open','Closed','closed','closed','private',1,1,9)",
    ] {
        db.execute(statement).await.unwrap();
    }
    db.close().await.unwrap();
}

fn source(error: &PersistenceFailure) -> &WorkspaceRenameMessagingStoreError {
    Error::source(error)
        .and_then(|source| source.downcast_ref())
        .expect("workspace rename persistence failures preserve their named SQLite source")
}

#[tokio::test]
async fn population_is_exactly_open_roster_non_closed_and_keeps_archived_channels() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspace-rename-population.sqlite");
    seed(&path).await;
    let port = MessagingStore::open(&path)
        .await
        .unwrap()
        .workspace_rename_messaging();

    let selected = read_workspace_rename_channels(&port, workspace("w"))
        .await
        .unwrap();
    assert_eq!(
        selected
            .channels()
            .iter()
            .map(|row| row.conversation().as_str())
            .collect::<Vec<_>>(),
        ["active-a", "active-b"]
    );
}

#[tokio::test]
async fn authoring_allocates_one_seq_and_returns_the_actual_committed_system_row() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspace-rename-author.sqlite");
    seed(&path).await;
    let port = MessagingStore::open_with(&path, FixedEnvironment::new())
        .await
        .unwrap()
        .workspace_rename_messaging();
    let requested = draft();

    let committed = author_workspace_rename_message(&port, requested.clone())
        .await
        .unwrap();
    assert_eq!(committed.draft(), &requested);
    assert_eq!(committed.seq().get(), 1);
    assert_eq!(committed.created_at().get(), 77);

    let mut db = sqlx::SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    let row = sqlx::query(
        "SELECT conversation_id,from_kind,content,metadata,seq,created_at FROM messages WHERE id=?",
    )
    .bind(committed.message().as_str())
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(row.get::<String, _>("conversation_id"), "active-a");
    assert_eq!(row.get::<String, _>("from_kind"), "system");
    assert_eq!(row.get::<String, _>("content"), requested.content());
    assert_eq!(row.get::<i64, _>("seq"), 1);
    assert_eq!(row.get::<i64, _>("created_at"), 77);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&row.get::<String, _>("metadata")).unwrap(),
        serde_json::json!({
            "subtype": "rename",
            "old_name": "Old Workspace",
            "new_name": "New Workspace",
            "scope": "workspace",
        })
    );
}

async fn assert_trigger_corruption_rolls_back(
    label: &str,
    trigger_body: &str,
    expected_field: &'static str,
) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(format!("workspace-rename-{label}.sqlite"));
    seed(&path).await;
    let mut db = sqlx::SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    let trigger = format!(
        "CREATE TRIGGER corrupt_workspace_rename AFTER INSERT ON messages BEGIN {trigger_body}; END"
    );
    // `trigger_body` is one of three test-owned literals below, never input.
    sqlx::query(sqlx::AssertSqlSafe(trigger))
        .execute(&mut db)
        .await
        .unwrap();
    let port = MessagingStore::open_with(&path, FixedEnvironment::new())
        .await
        .unwrap()
        .workspace_rename_messaging();

    let error = port.author(draft()).await.unwrap_err();
    assert!(matches!(
        source(&error),
        WorkspaceRenameMessagingStoreError::Corrupt { field } if *field == expected_field
    ));
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT latest_message_seq FROM conversations WHERE id='active-a'"
        )
        .fetch_one(&mut db)
        .await
        .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM messages")
            .fetch_one(&mut db)
            .await
            .unwrap(),
        0
    );

    db.execute("DROP TRIGGER corrupt_workspace_rename")
        .await
        .unwrap();
    author_workspace_rename_message(&port, draft())
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT latest_message_seq FROM conversations WHERE id='active-a'"
        )
        .fetch_one(&mut db)
        .await
        .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM messages")
            .fetch_one(&mut db)
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn a_trigger_cannot_splice_the_committed_message_into_another_conversation() {
    assert_trigger_corruption_rolls_back(
        "conversation",
        "UPDATE messages SET conversation_id='foreign' WHERE id=NEW.id",
        "messages.conversation_id",
    )
    .await;
}

#[tokio::test]
async fn a_trigger_cannot_rewrite_the_committed_message_content() {
    assert_trigger_corruption_rolls_back(
        "content",
        "UPDATE messages SET content='wrong' WHERE id=NEW.id",
        "messages.content:workspace_rename",
    )
    .await;
}

#[tokio::test]
async fn a_trigger_cannot_rewrite_the_committed_rename_metadata() {
    assert_trigger_corruption_rolls_back(
        "metadata",
        "UPDATE messages SET metadata=json_object('subtype','rename','old_name','Old Workspace','new_name','Wrong Workspace','scope','workspace') WHERE id=NEW.id",
        "messages.metadata:workspace_rename",
    )
    .await;
}

#[tokio::test]
async fn a_trigger_cannot_diverge_the_conversation_head_from_the_committed_message() {
    assert_trigger_corruption_rolls_back(
        "head",
        "UPDATE conversations SET latest_message_seq=999 WHERE id=NEW.conversation_id",
        "conversations.latest_message_seq:readback",
    )
    .await;
}

#[tokio::test]
async fn conversation_agents_are_the_ungated_ordered_membership_of_that_conversation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir
        .path()
        .join("workspace-rename-conversation-agents.sqlite");
    seed(&path).await;
    let mut db = sqlx::SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    for statement in [
        "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES ('u','u','U',1,1)",
        "INSERT INTO hosts(id,name,kind,created_at) VALUES ('h','H','local',1)",
        "INSERT INTO agents(id,name,display_name,runtime,host_id,created_by_user_id,created_at,updated_at) VALUES \
         ('agent-b','b','B','claude','h','u',1,1),\
         ('agent-a','a','A','claude','h','u',1,1),\
         ('agent-z','z','Z','claude','h','u',1,1)",
        // active-a holds agent-b and agent-a; active-b holds only agent-z. The read
        // must return active-a's pair ordered by id and never bleed active-b's agent.
        "INSERT INTO conversation_agents(conversation_id,agent_id,joined_at) VALUES \
         ('active-a','agent-b',1),('active-a','agent-a',1),('active-b','agent-z',1)",
    ] {
        db.execute(statement).await.unwrap();
    }
    db.close().await.unwrap();

    let port = MessagingStore::open(&path)
        .await
        .unwrap()
        .workspace_rename_messaging();

    // active-a: both its agents, ordered by id; active-b's agent-z is excluded.
    assert_eq!(
        port.agent_ids(&conversation("active-a")).await.unwrap(),
        vec![
            AgentId::parse("agent-a").unwrap(),
            AgentId::parse("agent-b").unwrap(),
        ]
    );
    // A conversation with no joined agents is the empty audience, not an error.
    assert!(
        port.agent_ids(&conversation("fixed"))
            .await
            .unwrap()
            .is_empty()
    );
    // An absent conversation is equally the empty audience: no one to push to.
    assert!(
        port.agent_ids(&conversation("missing"))
            .await
            .unwrap()
            .is_empty()
    );
}
