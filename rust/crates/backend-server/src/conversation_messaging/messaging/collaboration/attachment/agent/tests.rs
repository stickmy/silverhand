use std::{
    convert::Infallible,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
    },
};

use axum::{
    Router,
    body::{Body, Bytes, to_bytes},
    extract::Extension,
    http::{Method, Request, StatusCode, header},
};
use futures_util::stream;
use serde_json::Value;
use silverhand_backend_core::{
    conversation_messaging::messaging::{
        AddressRefusal, DirectPairAllocation, ResolvedStream, SendClock, SendEntropy,
        TargetAddress,
        collaboration::attachment::{
            AgentAttachmentCreateCommand, AgentAttachmentCreateRefusal,
            AgentAttachmentCreateUnitOfWork, AgentAttachmentCreatedFact,
            AgentAttachmentUploadAdmission, AgentAttachmentUploadAdmissionPort,
            AgentAttachmentUploadRefusal, AttachmentChangedFact, AttachmentChangedPublisher,
        },
        environment::EnvironmentUnavailable,
    },
    foundation::blob_store::{
        BlobDeleteOutcome, BlobNamespace, BlobReadOutcome, BlobStorageKey, BlobStore, StorageKind,
        StoredBlob,
    },
    foundation::identity::{AgentId, ConversationId, DaemonInstanceId},
    foundation::persistence::PersistenceFailure,
    host_runtime::runtime_credential::{
        RuntimeCustody, RuntimeGeneration, RuntimePrincipal, RuntimeScope,
    },
};
use silverhand_backend_filesystem::foundation::blob_store::{LocalBlobStore, LocalBlobStoreError};
use silverhand_backend_sqlite::conversation_messaging::messaging::{
    MessagingStore, collaboration::attachment::SqliteAgentAttachment,
};
use sqlx::{Connection, Executor, Row, SqliteConnection};
use tower::ServiceExt;

use super::{
    agent_attachment_download_router, agent_attachment_locate_router,
    agent_attachment_update_router, agent_attachment_upload_router,
};

#[derive(Clone, Copy, Default)]
struct Target;

impl TargetAddress for Target {
    type Error = Infallible;

    async fn resolve(
        &self,
        _agent: &AgentId,
        target: &str,
        _allocation: DirectPairAllocation,
        _intent: silverhand_backend_core::conversation_messaging::messaging::DeliveryIntent,
    ) -> Result<Result<ResolvedStream, AddressRefusal>, Self::Error> {
        Ok(if target == "#target" {
            Ok(ResolvedStream {
                conversation: ConversationId::parse("target").unwrap(),
                thread_root: None,
            })
        } else {
            Err(AddressRefusal::Malformed)
        })
    }
}

#[derive(Clone, Default)]
struct Environment(Arc<AtomicU8>);

impl SendEntropy for Environment {
    fn fill(&self, bytes: &mut [u8]) -> Result<(), EnvironmentUnavailable> {
        let value = self.0.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
        bytes.fill(value);
        Ok(())
    }
}

impl SendClock for Environment {
    fn now_unix_ms(&self) -> Result<i64, EnvironmentUnavailable> {
        Ok(100)
    }
}

#[derive(Clone, Default)]
struct Events(Arc<Mutex<Vec<AttachmentChangedFact>>>);

impl AttachmentChangedPublisher for Events {
    type Error = Infallible;

    async fn publish(&self, fact: &AttachmentChangedFact) -> Result<(), Self::Error> {
        self.0.lock().unwrap().push(fact.clone());
        Ok(())
    }
}

#[derive(Clone, Copy, Default)]
struct FailingEvents;

impl AttachmentChangedPublisher for FailingEvents {
    type Error = std::io::Error;

    async fn publish(&self, _fact: &AttachmentChangedFact) -> Result<(), Self::Error> {
        Err(std::io::Error::other("publisher unavailable"))
    }
}

#[derive(Clone)]
struct CountedAttachments {
    inner: SqliteAgentAttachment,
    creates: Arc<AtomicUsize>,
}

impl AgentAttachmentUploadAdmissionPort for CountedAttachments {
    type Error = PersistenceFailure;

    async fn admit_upload(
        &self,
        agent: &AgentId,
        conversation: &ConversationId,
    ) -> Result<Result<AgentAttachmentUploadAdmission, AgentAttachmentUploadRefusal>, Self::Error>
    {
        self.inner.admit_upload(agent, conversation).await
    }
}

impl AgentAttachmentCreateUnitOfWork for CountedAttachments {
    type Error = PersistenceFailure;

    async fn create_for_agent(
        &self,
        command: AgentAttachmentCreateCommand,
    ) -> Result<Result<AgentAttachmentCreatedFact, AgentAttachmentCreateRefusal>, Self::Error> {
        self.creates.fetch_add(1, Ordering::SeqCst);
        self.inner.create_for_agent(command).await
    }
}

#[derive(Clone)]
struct CountedBlob {
    inner: LocalBlobStore,
    puts: Arc<AtomicUsize>,
}

impl BlobStore for CountedBlob {
    type Error = LocalBlobStoreError;

    async fn put(
        &self,
        namespace: &BlobNamespace,
        normalized_content_type: &str,
        bytes: Vec<u8>,
    ) -> Result<StoredBlob, Self::Error> {
        self.puts.fetch_add(1, Ordering::SeqCst);
        self.inner
            .put(namespace, normalized_content_type, bytes)
            .await
    }

    async fn read(&self, blob: &StoredBlob) -> Result<BlobReadOutcome, Self::Error> {
        self.inner.read(blob).await
    }

    async fn delete(&self, blob: &StoredBlob) -> Result<BlobDeleteOutcome, Self::Error> {
        self.inner.delete(blob).await
    }
}

fn principal(scope: RuntimeScope) -> RuntimePrincipal {
    RuntimePrincipal::for_scope(
        scope,
        RuntimeCustody::new(
            RuntimeGeneration::new(1).unwrap(),
            DaemonInstanceId::parse("daemon-1").unwrap(),
        ),
    )
}

fn agent_slot(agent: &str) -> RuntimePrincipal {
    principal(RuntimeScope::AgentSlot {
        agent: AgentId::parse(agent).unwrap(),
    })
}

fn conversation_slot(agent: &str, source: &str) -> RuntimePrincipal {
    principal(RuntimeScope::ConversationSlot {
        agent: AgentId::parse(agent).unwrap(),
        source: ConversationId::parse(source).unwrap(),
    })
}

async fn seeded() -> (
    tempfile::TempDir,
    std::path::PathBuf,
    MessagingStore,
    LocalBlobStore,
) {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("agent-attachments.sqlite");
    let mut db = crate::current_schema::create_current_schema(&database).await;
    db.execute(sqlx::raw_sql(
        concat!(
            "INSERT INTO hosts(id,name,kind,created_at) VALUES('h','Attachment Host','local',1); ",
            "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES ('owner','agent-attachment-owner','Owner',1,1); ",
            seed_agent!(
                id = "a",
                name = "agent-attachment-a",
                runtime = "codex",
                host_id = "h",
                created_by_user_id = "owner",
                display_name = "A",
                session_scope = "agent",
            ),
            "; ",
            seed_agent!(
                id = "b",
                name = "agent-attachment-b",
                runtime = "codex",
                host_id = "h",
                created_by_user_id = "owner",
                display_name = "B",
                session_scope = "agent",
            ),
            "; ",
            "INSERT INTO workspaces(id,name,name_key,created_at,updated_at) VALUES ('w','Agent Attachment Workspace','agent-attachment-workspace',1,1); ",
            "INSERT INTO workspace_members(workspace_id,agent_id,joined_at) VALUES('w','a',1); ",
            "INSERT INTO workspace_agent_capabilities(workspace_id,agent_id,capability) VALUES ('w','a','read_attachments'),('w','a','upload_attachments'); ",
            seed_channel!(id = "source", workspace_id = "w", title = "Source"),
            "; ",
            seed_channel!(id = "target", workspace_id = "w", title = "Target"),
            "; ",
            "INSERT INTO conversation_agents(conversation_id,agent_id,joined_at) VALUES ('source','a',1),('target','a',1); ",
        ),
    ))
    .await
    .unwrap();
    db.close().await.unwrap();
    let store = MessagingStore::open(&database).await.unwrap();
    let blobs = LocalBlobStore::open(directory.path().join("blobs"))
        .await
        .unwrap();
    (directory, database, store, blobs)
}

fn app<P>(
    store: &MessagingStore,
    blobs: &LocalBlobStore,
    publisher: P,
    runtime: Option<RuntimePrincipal>,
) -> Router
where
    P: AttachmentChangedPublisher + Send + Sync + 'static,
{
    let attachments = store.agent_attachments();
    let app = agent_attachment_upload_router(
        Target,
        attachments.clone(),
        blobs.clone(),
        Environment::default(),
    )
    .merge(agent_attachment_update_router(
        attachments.clone(),
        blobs.clone(),
        publisher,
    ))
    .merge(agent_attachment_locate_router(
        attachments.clone(),
        blobs.clone(),
    ))
    .merge(agent_attachment_download_router(attachments, blobs.clone()));
    match runtime {
        Some(runtime) => app.layer(Extension(runtime)),
        None => app,
    }
}

type MultipartPart<'a> = (&'a str, Option<(&'a str, &'a str)>, &'a [u8]);

fn multipart(parts: &[MultipartPart<'_>]) -> (String, Vec<u8>) {
    let boundary = "silverhand-agent-attachment-boundary";
    let mut body = Vec::new();
    for (name, file, bytes) in parts {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        match file {
            Some((filename, content_type)) => body.extend_from_slice(
                format!(
                    "Content-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\nContent-Type: {content_type}\r\n\r\n"
                )
                .as_bytes(),
            ),
            None => body.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
            ),
        }
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

async fn call(
    app: Router,
    method: Method,
    uri: &str,
    content_type: Option<&str>,
    body: Body,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(content_type) = content_type {
        builder = builder.header(header::CONTENT_TYPE, content_type);
    }
    let response = app.oneshot(builder.body(body).unwrap()).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, headers, body)
}

fn json_body(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).unwrap()
}

fn observed_body(polled: Arc<AtomicBool>) -> Body {
    Body::from_stream(stream::once(async move {
        polled.store(true, Ordering::SeqCst);
        Ok::<_, Infallible>(Bytes::from_static(b"must not be read"))
    }))
}

#[tokio::test]
async fn runtime_and_object_authority_refusals_do_not_poll_bodies() {
    let (_directory, _database, store, blobs) = seeded().await;
    for runtime in [None, Some(agent_slot("b"))] {
        let polled = Arc::new(AtomicBool::new(false));
        let (status, _, _) = call(
            app(&store, &blobs, Events::default(), runtime),
            Method::POST,
            "/internal/agents/a/attachments",
            None,
            observed_body(Arc::clone(&polled)),
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(!polled.load(Ordering::SeqCst));
    }

    let polled = Arc::new(AtomicBool::new(false));
    let (status, _, _) = call(
        app(
            &store,
            &blobs,
            Events::default(),
            Some(conversation_slot("a", "source")),
        ),
        Method::PATCH,
        "/internal/agents/a/attachments/foreign",
        None,
        observed_body(Arc::clone(&polled)),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(!polled.load(Ordering::SeqCst));
}

#[tokio::test]
async fn upload_parses_file_before_target_then_authorizes_before_blob_or_database_effects() {
    let (_directory, database, store, blobs) = seeded().await;
    let png = [0x89, 0x50, 0x4e, 0x47, 1, 2, 3, 4];
    let (content_type, body) = multipart(&[
        ("file", Some(("shot.png", "image/png")), &png),
        ("target", None, b"#target"),
    ]);
    let (status, _, response) = call(
        app(&store, &blobs, Events::default(), Some(agent_slot("a"))),
        Method::POST,
        "/internal/agents/a/attachments",
        Some(&content_type),
        Body::from(body),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(json_body(&response)["channel_id"], "target");

    let mut db = SqliteConnection::connect(database.to_str().unwrap())
        .await
        .unwrap();
    db.execute("DELETE FROM workspace_agent_capabilities WHERE workspace_id='w' AND agent_id='a' AND capability='upload_attachments'")
        .await
        .unwrap();
    let polled = Arc::new(AtomicBool::new(false));
    let (content_type, denied_body) = multipart(&[
        ("target", None, b"#target"),
        ("file", Some(("denied.png", "image/png")), &png),
    ]);
    let bytes = Bytes::from(denied_body);
    let polled_clone = Arc::clone(&polled);
    let creates = Arc::new(AtomicUsize::new(0));
    let puts = Arc::new(AtomicUsize::new(0));
    let counted_attachments = CountedAttachments {
        inner: store.agent_attachments(),
        creates: Arc::clone(&creates),
    };
    let counted_blobs = CountedBlob {
        inner: blobs.clone(),
        puts: Arc::clone(&puts),
    };
    let (status, _, _) = call(
        agent_attachment_upload_router(
            Target,
            counted_attachments,
            counted_blobs,
            Environment::default(),
        )
        .layer(Extension(agent_slot("a"))),
        Method::POST,
        "/internal/agents/a/attachments",
        Some(&content_type),
        Body::from_stream(stream::once(async move {
            polled_clone.store(true, Ordering::SeqCst);
            Ok::<_, Infallible>(bytes)
        })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(polled.load(Ordering::SeqCst));
    assert_eq!(puts.load(Ordering::SeqCst), 0);
    assert_eq!(creates.load(Ordering::SeqCst), 0);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM attachments")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn four_factories_share_committed_blob_size_alias_and_event_contracts() {
    let (_directory, database, store, blobs) = seeded().await;
    let events = Events::default();
    let png_a = [0x89, 0x50, 0x4e, 0x47, 1, 2, 3, 4];
    let (content_type, body) = multipart(&[
        ("target", None, b"#target"),
        ("file", Some(("shot.png", "image/png")), &png_a),
    ]);
    let (status, _, body) = call(
        app(&store, &blobs, events.clone(), Some(agent_slot("a"))),
        Method::POST,
        "/internal/agents/a/attachments",
        Some(&content_type),
        Body::from(body),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let attachment = json_body(&body)["id"].as_str().unwrap().to_owned();

    let (status, _, body) = call(
        app(&store, &blobs, events.clone(), Some(agent_slot("a"))),
        Method::GET,
        &format!("/internal/agents/a/attachments/{attachment}/locate"),
        None,
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let locate = json_body(&body);
    assert_eq!(locate["storage"], "local");
    assert!(locate["local_path"].as_str().unwrap().contains("alias-"));

    let (status, headers, body) = call(
        app(&store, &blobs, events.clone(), Some(agent_slot("a"))),
        Method::GET,
        &format!("/internal/agents/a/attachments/{attachment}/download"),
        None,
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "image/png");
    assert_eq!(body, png_a);

    let png_b = [0x89, 0x50, 0x4e, 0x47, 9, 8, 7, 6];
    let (content_type, body) = multipart(&[(
        "file",
        Some(("ignored.png", "application/octet-stream")),
        &png_b,
    )]);
    let (status, _, body) = call(
        app(&store, &blobs, events.clone(), Some(agent_slot("a"))),
        Method::PATCH,
        &format!("/internal/agents/a/attachments/{attachment}"),
        Some(&content_type),
        Body::from(body),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json_body(&body)["content_revision"], 2);
    assert_eq!(events.0.lock().unwrap().len(), 1);

    let mut db = SqliteConnection::connect(database.to_str().unwrap())
        .await
        .unwrap();
    sqlx::query("UPDATE attachments SET size=9 WHERE id=?")
        .bind(&attachment)
        .execute(&mut db)
        .await
        .unwrap();
    let (status, _, _) = call(
        app(&store, &blobs, events, Some(agent_slot("a"))),
        Method::GET,
        &format!("/internal/agents/a/attachments/{attachment}/download"),
        None,
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn missing_committed_blob_is_safe_500_and_publisher_failure_does_not_reverse_update() {
    let (_directory, database, store, blobs) = seeded().await;
    let png_a = [0x89, 0x50, 0x4e, 0x47, 1, 2, 3, 4];
    let (content_type, body) = multipart(&[
        ("target", None, b"#target"),
        ("file", Some(("shot.png", "image/png")), &png_a),
    ]);
    let (status, _, body) = call(
        app(&store, &blobs, FailingEvents, Some(agent_slot("a"))),
        Method::POST,
        "/internal/agents/a/attachments",
        Some(&content_type),
        Body::from(body),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let attachment = json_body(&body)["id"].as_str().unwrap().to_owned();

    let png_b = [0x89, 0x50, 0x4e, 0x47, 9, 8, 7, 6];
    let (content_type, body) = multipart(&[("file", Some(("next.png", "image/png")), &png_b)]);
    let (status, _, body) = call(
        app(&store, &blobs, FailingEvents, Some(agent_slot("a"))),
        Method::PATCH,
        &format!("/internal/agents/a/attachments/{attachment}"),
        Some(&content_type),
        Body::from(body),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json_body(&body)["content_revision"], 2);

    let mut db = SqliteConnection::connect(database.to_str().unwrap())
        .await
        .unwrap();
    let storage_key: String = sqlx::query("SELECT storage_key FROM attachments WHERE id=?")
        .bind(&attachment)
        .fetch_one(&mut db)
        .await
        .unwrap()
        .get("storage_key");
    let physical = StoredBlob::new(
        StorageKind::Local,
        BlobStorageKey::parse(storage_key).unwrap(),
    );
    blobs.delete(&physical).await.unwrap();
    let (status, _, _) = call(
        app(&store, &blobs, FailingEvents, Some(agent_slot("a"))),
        Method::GET,
        &format!("/internal/agents/a/attachments/{attachment}/locate"),
        None,
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    let (status, _, _) = call(
        app(&store, &blobs, FailingEvents, Some(agent_slot("a"))),
        Method::GET,
        &format!("/internal/agents/a/attachments/{attachment}/download"),
        None,
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
}
