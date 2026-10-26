use std::{error::Error, time::Duration};

use silverhand_backend_core::{
    conversation_messaging::messaging::{
        AttachmentId, ResolvedStream,
        collaboration::attachment::{
            AgentAttachmentContentUpdateCommand, AgentAttachmentContentUpdateRefusal,
            AgentAttachmentContentUpdateUnitOfWork, AgentAttachmentCreateCommand,
            AgentAttachmentCreateRefusal, AgentAttachmentCreateUnitOfWork,
            AgentAttachmentDownloadReadPort, AgentAttachmentLocateReadPort,
            AgentAttachmentReadRefusal, AgentAttachmentRuntime, AgentAttachmentTarget,
            AgentAttachmentUpdateAdmissionPort, AgentAttachmentUpdateAdmissionRefusal,
            AgentAttachmentUploadAdmissionPort, AttachmentContentType, AttachmentFilename,
        },
    },
    foundation::blob_store::{BlobStorageKey, StorageKind, StoredBlob},
    foundation::identity::{AgentId, ConversationId, DaemonInstanceId},
    foundation::persistence::PersistenceFailure,
    host_runtime::runtime_credential::{
        RuntimeCustody, RuntimeGeneration, RuntimePrincipal, RuntimeScope,
    },
};
use silverhand_backend_sqlite::conversation_messaging::messaging::{
    MessagingStore, collaboration::attachment::AgentAttachmentStoreError,
};
use sqlx::{Connection, Executor, Row, SqliteConnection};

use crate::current_schema;

fn agent(raw: &str) -> AgentId {
    AgentId::parse(raw).unwrap()
}

fn conversation(raw: &str) -> ConversationId {
    ConversationId::parse(raw).unwrap()
}

fn attachment(raw: &str) -> AttachmentId {
    AttachmentId::parse(raw).unwrap()
}

fn blob(key: &str) -> StoredBlob {
    StoredBlob::new(StorageKind::Local, BlobStorageKey::parse(key).unwrap())
}

fn runtime(scope: RuntimeScope, path_agent: &str) -> AgentAttachmentRuntime {
    let principal = RuntimePrincipal::for_scope(
        scope,
        RuntimeCustody::new(
            RuntimeGeneration::new(1).unwrap(),
            DaemonInstanceId::parse("daemon-1").unwrap(),
        ),
    );
    AgentAttachmentRuntime::bind(&principal, &agent(path_agent)).unwrap()
}

fn agent_slot() -> AgentAttachmentRuntime {
    runtime(RuntimeScope::AgentSlot { agent: agent("a") }, "a")
}

fn conversation_slot(source: &str) -> AgentAttachmentRuntime {
    runtime(
        RuntimeScope::ConversationSlot {
            agent: agent("a"),
            source: conversation(source),
        },
        "a",
    )
}

async fn seed(path: &std::path::Path) {
    let mut db = current_schema::create_current_schema(path).await;
    for statement in [
        "INSERT INTO hosts(id,name,kind,created_at) VALUES('h','Attachment Host','local',1)",
        "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES \
         ('owner','agent-attachment-owner','Owner',1,1)",
        "INSERT INTO agents(id,name,runtime,host_id,created_by_user_id,created_at,updated_at,display_name,session_scope) VALUES \
         ('a','agent-attachment-a','codex','h','owner',1,1,'A','agent'),\
         ('b','agent-attachment-b','codex','h','owner',1,1,'B','agent')",
        "INSERT INTO workspaces(id,name,name_key,created_at,updated_at) VALUES \
         ('w','Agent Attachment Workspace','agent-attachment-workspace',1,1)",
        "INSERT INTO workspace_members(workspace_id,agent_id,joined_at) VALUES('w','a',1)",
        "INSERT INTO workspace_agent_capabilities(workspace_id,agent_id,capability) VALUES \
         ('w','a','read_attachments'),('w','a','upload_attachments')",
        "INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,status,visibility,created_at,updated_at) VALUES \
         ('source','w','workspace','open','Source','source','idle','private',1,1),\
         ('target','w','workspace','open','Target','target','idle','private',1,1)",
        "INSERT INTO conversation_agents(conversation_id,agent_id,joined_at) VALUES \
         ('source','a',1),('target','a',1)",
        "INSERT INTO workspaces(id,name,name_key,owner_agent_id,created_at,updated_at) VALUES \
         ('home','Agent Home',NULL,'a',1,1)",
        "INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,status,visibility,created_at,updated_at) VALUES \
         ('home-channel','home','workspace','fixed','Home','home','idle','private',1,1)",
        "INSERT INTO conversation_agents(conversation_id,agent_id,joined_at) VALUES('home-channel','a',1)",
        "INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,endpoint_a_kind,endpoint_a_id,endpoint_b_kind,endpoint_b_id,title,title_key,status,visibility,created_at,updated_at) VALUES \
         ('pair',NULL,'direct_pair','fixed','agent','a','human','owner','',NULL,'idle','private',1,1)",
        "INSERT INTO attachments(id,channel_id,source_conversation_id,filename,size,content_type,storage,storage_key,created_by_kind,created_by_id,created_at,content_revision) VALUES \
         ('existing','target','target','mockup.png',4,'image/png','local','attachments/target/old.png','agent','a',10,1)",
    ] {
        db.execute(statement).await.unwrap();
    }
    db.close().await.unwrap();
}

fn store_source(error: &PersistenceFailure) -> &AgentAttachmentStoreError {
    Error::source(error)
        .and_then(|source| source.downcast_ref())
        .expect("agent attachment failures preserve their named SQLite source")
}

async fn update_command(
    store: &silverhand_backend_sqlite::conversation_messaging::messaging::collaboration::attachment::SqliteAgentAttachment,
    runtime: &AgentAttachmentRuntime,
    key: &str,
) -> AgentAttachmentContentUpdateCommand {
    let admitted = store
        .admit_update(runtime, &attachment("existing"))
        .await
        .unwrap()
        .unwrap()
        .bind(runtime, &attachment("existing"))
        .unwrap();
    AgentAttachmentContentUpdateCommand::new(&admitted, 8, blob(key)).unwrap()
}

async fn create_command(
    store: &silverhand_backend_sqlite::conversation_messaging::messaging::collaboration::attachment::SqliteAgentAttachment,
    id: &str,
    key: &str,
) -> AgentAttachmentCreateCommand {
    let runtime = agent_slot();
    let target = AgentAttachmentTarget::resolved(
        &runtime,
        ResolvedStream {
            conversation: conversation("target"),
            thread_root: None,
        },
    )
    .unwrap();
    let admitted = store
        .admit_upload(runtime.agent(), target.conversation())
        .await
        .unwrap()
        .unwrap()
        .bind(&target)
        .unwrap();
    AgentAttachmentCreateCommand::new(
        &admitted,
        attachment(id),
        AttachmentFilename::parse("mockup.png").unwrap(),
        4,
        AttachmentContentType::parse("image/png").unwrap(),
        blob(key),
        10,
    )
    .unwrap()
}

#[tokio::test]
async fn update_reach_is_a_product_refusal_while_agent_slot_commits() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agent-attachment-update.sqlite");
    seed(&path).await;
    let store = MessagingStore::open(&path)
        .await
        .unwrap()
        .agent_attachments();

    assert_eq!(
        store
            .admit_update(&conversation_slot("source"), &attachment("existing"))
            .await
            .unwrap(),
        Err(AgentAttachmentUpdateAdmissionRefusal::NotFound)
    );
    let mut db = SqliteConnection::connect(path.to_str().unwrap())
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT storage_key FROM attachments WHERE id='existing'")
            .fetch_one(&mut db)
            .await
            .unwrap(),
        "attachments/target/old.png"
    );

    let runtime = agent_slot();
    let command = update_command(&store, &runtime, "attachments/target/new.png").await;
    let committed = store
        .update_content_for_agent(command)
        .await
        .unwrap()
        .unwrap()
        .bind(&runtime, &attachment("existing"))
        .unwrap();
    assert_eq!(committed.after().content_revision().get(), 2);
    assert_eq!(
        committed.after().blob().key().as_str(),
        "attachments/target/new.png"
    );
}

#[tokio::test]
async fn update_rereads_capability_after_writer_reservation_before_pointer_swap() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir
        .path()
        .join("agent-attachment-update-reservation.sqlite");
    seed(&path).await;
    let store = MessagingStore::open(&path)
        .await
        .unwrap()
        .agent_attachments();
    let command = update_command(&store, &agent_slot(), "attachments/target/denied.png").await;

    let mut blocker = SqliteConnection::connect(path.to_str().unwrap())
        .await
        .unwrap();
    blocker.execute("BEGIN IMMEDIATE").await.unwrap();
    let pending = {
        let store = store.clone();
        tokio::spawn(async move { store.update_content_for_agent(command).await })
    };
    tokio::time::sleep(Duration::from_millis(75)).await;
    assert!(!pending.is_finished());
    blocker
        .execute("DELETE FROM workspace_agent_capabilities WHERE workspace_id='w' AND agent_id='a' AND capability='upload_attachments'")
        .await
        .unwrap();
    blocker.execute("COMMIT").await.unwrap();

    assert_eq!(
        pending.await.unwrap().unwrap(),
        Err(AgentAttachmentContentUpdateRefusal::CapabilityDenied)
    );
    let row =
        sqlx::query("SELECT storage_key,content_revision FROM attachments WHERE id='existing'")
            .fetch_one(&mut blocker)
            .await
            .unwrap();
    assert_eq!(
        row.get::<String, _>("storage_key"),
        "attachments/target/old.png"
    );
    assert_eq!(row.get::<i64, _>("content_revision"), 1);
}

#[tokio::test]
async fn update_rereads_actual_creator_after_writer_reservation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agent-attachment-update-creator.sqlite");
    seed(&path).await;
    let store = MessagingStore::open(&path)
        .await
        .unwrap()
        .agent_attachments();
    let command = update_command(&store, &agent_slot(), "attachments/target/denied.png").await;

    let mut blocker = SqliteConnection::connect(path.to_str().unwrap())
        .await
        .unwrap();
    blocker.execute("BEGIN IMMEDIATE").await.unwrap();
    let pending = {
        let store = store.clone();
        tokio::spawn(async move { store.update_content_for_agent(command).await })
    };
    tokio::time::sleep(Duration::from_millis(75)).await;
    assert!(!pending.is_finished());
    blocker
        .execute("UPDATE attachments SET created_by_id='b' WHERE id='existing'")
        .await
        .unwrap();
    blocker.execute("COMMIT").await.unwrap();

    assert_eq!(
        pending.await.unwrap().unwrap(),
        Err(AgentAttachmentContentUpdateRefusal::NotCreator)
    );
    let row =
        sqlx::query("SELECT storage_key,content_revision FROM attachments WHERE id='existing'")
            .fetch_one(&mut blocker)
            .await
            .unwrap();
    assert_eq!(
        row.get::<String, _>("storage_key"),
        "attachments/target/old.png"
    );
    assert_eq!(row.get::<i64, _>("content_revision"), 1);
}

#[tokio::test]
async fn create_rereads_participation_after_writer_reservation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir
        .path()
        .join("agent-attachment-create-reservation.sqlite");
    seed(&path).await;
    let store = MessagingStore::open(&path)
        .await
        .unwrap()
        .agent_attachments();
    let command = create_command(&store, "created", "attachments/target/created.png").await;

    let mut blocker = SqliteConnection::connect(path.to_str().unwrap())
        .await
        .unwrap();
    blocker.execute("BEGIN IMMEDIATE").await.unwrap();
    let pending = {
        let store = store.clone();
        tokio::spawn(async move { store.create_for_agent(command).await })
    };
    tokio::time::sleep(Duration::from_millis(75)).await;
    assert!(!pending.is_finished());
    blocker
        .execute("DELETE FROM conversation_agents WHERE conversation_id='target' AND agent_id='a'")
        .await
        .unwrap();
    blocker.execute("COMMIT").await.unwrap();

    assert_eq!(
        pending.await.unwrap().unwrap(),
        Err(AgentAttachmentCreateRefusal::NotFound)
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM attachments WHERE id='created'")
        .fetch_one(&mut blocker)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn read_facts_bind_authority_and_fail_closed_on_source_corruption() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agent-attachment-read.sqlite");
    seed(&path).await;
    let store = MessagingStore::open(&path)
        .await
        .unwrap()
        .agent_attachments();
    let runtime = agent_slot();

    assert!(
        store
            .locate_for_agent(runtime.agent(), &attachment("existing"))
            .await
            .unwrap()
            .unwrap()
            .bind(&runtime, &attachment("existing"))
            .is_ok()
    );
    assert!(
        store
            .download_for_agent(runtime.agent(), &attachment("existing"))
            .await
            .unwrap()
            .unwrap()
            .bind(&runtime, &attachment("existing"))
            .is_ok()
    );

    let mut db = SqliteConnection::connect(path.to_str().unwrap())
        .await
        .unwrap();
    db.execute("DELETE FROM workspace_agent_capabilities WHERE workspace_id='w' AND agent_id='a' AND capability='read_attachments'")
        .await
        .unwrap();
    assert_eq!(
        store
            .locate_for_agent(runtime.agent(), &attachment("existing"))
            .await
            .unwrap(),
        Err(AgentAttachmentReadRefusal::CapabilityDenied)
    );
    db.execute("INSERT INTO workspace_agent_capabilities(workspace_id,agent_id,capability) VALUES('w','a','read_attachments')")
        .await
        .unwrap();
    db.execute("UPDATE attachments SET source_conversation_id='source' WHERE id='existing'")
        .await
        .unwrap();
    let failure = store
        .download_for_agent(runtime.agent(), &attachment("existing"))
        .await
        .unwrap_err();
    assert!(matches!(
        store_source(&failure),
        AgentAttachmentStoreError::Fact
    ));
    db.execute("UPDATE attachments SET source_conversation_id='target' WHERE id='existing'")
        .await
        .unwrap();
    assert!(
        store
            .download_for_agent(runtime.agent(), &attachment("existing"))
            .await
            .unwrap()
            .is_ok()
    );
}

#[tokio::test]
async fn agent_home_and_direct_pair_use_their_placement_authorities_without_workspace_grants() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agent-attachment-placement-arms.sqlite");
    seed(&path).await;
    let store = MessagingStore::open(&path)
        .await
        .unwrap()
        .agent_attachments();
    assert!(
        store
            .admit_upload(&agent("a"), &conversation("home-channel"))
            .await
            .unwrap()
            .is_ok()
    );
    assert!(
        store
            .admit_upload(&agent("a"), &conversation("pair"))
            .await
            .unwrap()
            .is_ok()
    );
    let mut db = SqliteConnection::connect(path.to_str().unwrap())
        .await
        .unwrap();
    db.execute("UPDATE conversations SET endpoint_b_deleted_at=2 WHERE id='pair'")
        .await
        .unwrap();
    assert_eq!(
        store
            .admit_upload(&agent("a"), &conversation("pair"))
            .await
            .unwrap(),
        Err(silverhand_backend_core::conversation_messaging::messaging::collaboration::attachment::AgentAttachmentUploadRefusal::NotFound)
    );
    assert_eq!(
        store
            .admit_upload(&agent("b"), &conversation("home-channel"))
            .await
            .unwrap(),
        Err(silverhand_backend_core::conversation_messaging::messaging::collaboration::attachment::AgentAttachmentUploadRefusal::NotFound)
    );
}
