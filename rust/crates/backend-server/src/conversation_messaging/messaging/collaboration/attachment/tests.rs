use std::{
    convert::Infallible,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
};

use axum::{
    body::{Body, Bytes, to_bytes},
    extract::Extension,
    http::{Method, Request, StatusCode, header},
};
use futures_util::stream;
use serde_json::{Value, json};
use silverhand_backend_core::{
    conversation_messaging::messaging::{
        SendClock, SendEntropy,
        collaboration::attachment::{
            AttachmentChangedFact, AttachmentChangedPublisher, AttachmentPostCommitMaintenance,
        },
        environment::EnvironmentUnavailable,
    },
    foundation::blob_store::{
        BlobAliasId, BlobDeleteOutcome, BlobNamespace, BlobReadOutcome, BlobStorageKey, BlobStore,
        StorageKind, StoredBlob,
    },
    foundation::identity::UserId,
};
use silverhand_backend_filesystem::foundation::blob_store::{
    LocalBlobAliasDeleteOutcome, LocalBlobAliasOutcome, LocalBlobAliasStore, LocalBlobStore,
};
use silverhand_backend_sqlite::conversation_messaging::messaging::MessagingStore;
use sqlx::{Connection, Executor, SqliteConnection};
use tower::ServiceExt;

use super::{
    BlobMaintenance, MAX_FILE_BYTES, MULTIPART_ENVELOPE_BYTES, changed_event, multipart_envelope,
    router, sanitize_filename,
};

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

#[derive(Clone, Default)]
struct FailingPhysicalDelete {
    physical_deletes: Arc<AtomicU8>,
    alias_deletes: Arc<AtomicU8>,
}

impl BlobStore for FailingPhysicalDelete {
    type Error = std::io::Error;

    async fn put(
        &self,
        _namespace: &BlobNamespace,
        _normalized_content_type: &str,
        _bytes: Vec<u8>,
    ) -> Result<StoredBlob, Self::Error> {
        Err(std::io::Error::other("unused put"))
    }

    async fn read(&self, _blob: &StoredBlob) -> Result<BlobReadOutcome, Self::Error> {
        Err(std::io::Error::other("unused read"))
    }

    async fn delete(&self, _blob: &StoredBlob) -> Result<BlobDeleteOutcome, Self::Error> {
        self.physical_deletes.fetch_add(1, Ordering::SeqCst);
        Err(std::io::Error::other("physical delete failed"))
    }
}

impl LocalBlobAliasStore for FailingPhysicalDelete {
    type Error = std::io::Error;

    async fn materialize_alias(
        &self,
        _physical: &StoredBlob,
        _alias: &BlobAliasId,
    ) -> Result<LocalBlobAliasOutcome, Self::Error> {
        Err(std::io::Error::other("unused materialize"))
    }

    async fn delete_alias(
        &self,
        _physical: &StoredBlob,
        _alias: &BlobAliasId,
    ) -> Result<LocalBlobAliasDeleteOutcome, Self::Error> {
        self.alias_deletes.fetch_add(1, Ordering::SeqCst);
        Err(std::io::Error::other("alias delete failed"))
    }
}

#[tokio::test]
async fn committed_delete_attempts_both_cleanup_legs_and_reports_both_failures() {
    let blobs = FailingPhysicalDelete::default();
    let deleted = StoredBlob::new(
        StorageKind::Local,
        BlobStorageKey::parse("attachments/a/blob.bin").unwrap(),
    );
    let alias = BlobAliasId::from_semantic_id("a").unwrap();

    let error =
        AttachmentPostCommitMaintenance::deleted(&BlobMaintenance(blobs.clone()), &deleted, &alias)
            .await
            .unwrap_err();
    assert_eq!(error.operation, "delete attachment blob and alias");
    let chain = crate::foundation::error::describe(&error);
    assert!(chain.contains("physical delete failed"), "{chain}");
    assert!(chain.contains("alias delete failed"), "{chain}");
    assert_eq!(blobs.physical_deletes.load(Ordering::SeqCst), 1);
    assert_eq!(blobs.alias_deletes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn double_cleanup_failure_does_not_roll_back_committed_delete_http() {
    let (_directory, database, store, _local_blobs, events) = seeded().await;
    let mut db = SqliteConnection::connect(database.to_str().unwrap())
        .await
        .unwrap();
    db.execute(
        "INSERT INTO attachments(\
           id,channel_id,source_conversation_id,filename,size,content_type,storage,storage_key,\
           created_by_kind,created_by_id,created_at,content_revision) VALUES \
           ('cleanup','c','c','cleanup.bin',4,'application/octet-stream','local',\
            'attachments/c/cleanup.bin','user','owner',1,1)",
    )
    .await
    .unwrap();
    db.close().await.unwrap();
    let blobs = FailingPhysicalDelete::default();

    let response = router(
        store.attachment_reads(),
        store.attachment_writes(),
        blobs.clone(),
        Environment::default(),
        events,
    )
    .layer(Extension(UserId::parse("owner").unwrap()))
    .oneshot(
        Request::builder()
            .method(Method::DELETE)
            .uri("/api/attachments/cleanup")
            .body(Body::empty())
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(blobs.physical_deletes.load(Ordering::SeqCst), 1);
    assert_eq!(blobs.alias_deletes.load(Ordering::SeqCst), 1);

    let mut db = SqliteConnection::connect(database.to_str().unwrap())
        .await
        .unwrap();
    let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM attachments WHERE id='cleanup'")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(remaining, 0);
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

async fn seeded() -> (
    tempfile::TempDir,
    std::path::PathBuf,
    MessagingStore,
    LocalBlobStore,
    Events,
) {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("attachments.sqlite");
    let mut db = crate::current_schema::create_current_schema(&database).await;
    db.execute(sqlx::raw_sql(
        "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES \
           ('owner','owner','Owner',1,1),('other','other','Other',1,1); \
         INSERT INTO workspaces(id,name,name_key,created_at,updated_at) VALUES \
           ('w','Workspace','workspace',1,1); \
         INSERT INTO workspace_users(workspace_id,user_id,joined_at,role) VALUES \
           ('w','owner',1,'member'),('w','other',1,'member'); \
         INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,visibility,created_at,updated_at) VALUES \
           ('c','w','workspace','open','General','general','private',1,1); \
         INSERT INTO conversation_users(conversation_id,user_id,joined_at) VALUES \
           ('c','owner',1);",
    ))
    .await
    .unwrap();
    db.close().await.unwrap();
    let store = MessagingStore::open(&database).await.unwrap();
    let blobs = LocalBlobStore::open(directory.path().join("blobs"))
        .await
        .unwrap();
    (directory, database, store, blobs, Events::default())
}

fn app(
    store: &MessagingStore,
    blobs: &LocalBlobStore,
    events: &Events,
    user: &str,
) -> axum::Router {
    router(
        store.attachment_reads(),
        store.attachment_writes(),
        blobs.clone(),
        Environment::default(),
        events.clone(),
    )
    .layer(Extension(UserId::parse(user).unwrap()))
}

fn multipart(field: &str, filename: &str, content_type: &str, bytes: &[u8]) -> (String, Vec<u8>) {
    let boundary = "silverhand-attachment-boundary";
    let mut body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"{field}\"; filename=\"{filename}\"\r\nContent-Type: {content_type}\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

type MultipartTestPart<'a> = (&'a str, Option<(&'a str, &'a str)>, &'a [u8]);

fn multipart_parts(parts: &[MultipartTestPart<'_>]) -> (String, Vec<u8>) {
    let boundary = "silverhand-attachment-duplicate-boundary";
    let mut body = Vec::new();
    for (name, file, bytes) in parts {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"").as_bytes(),
        );
        if let Some((filename, content_type)) = file {
            body.extend_from_slice(
                format!("; filename=\"{filename}\"\r\nContent-Type: {content_type}").as_bytes(),
            );
        }
        body.extend_from_slice(b"\r\n\r\n");
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

fn multipart_request(content_type: &str, body: Vec<u8>) -> Request<Body> {
    Request::builder()
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(body))
        .unwrap()
}

#[test]
fn filename_sanitization_matches_the_typescript_utf16_and_trim_contract() {
    assert_eq!(
        sanitize_filename("\u{feff}a/\u{0085}\u{001f}b\u{feff}"),
        Some("a_\u{0085}b".to_owned())
    );

    let exact = format!("{}a", "😀".repeat(127));
    assert_eq!(sanitize_filename(&exact), Some(exact));
    let over = format!("{}ab", "😀".repeat(127));
    assert_eq!(
        sanitize_filename(&over),
        Some(format!("{}a", "😀".repeat(127)))
    );

    let split_discriminator = format!("{}😀", "a".repeat(254));
    assert_eq!(
        split_discriminator.encode_utf16().count(),
        256,
        "the final astral scalar crosses the 255-unit boundary"
    );
    assert_eq!(
        sanitize_filename(&split_discriminator),
        Some("a".repeat(254)),
        "the sanitizer must not manufacture an isolated high surrogate"
    );
}

#[tokio::test]
async fn multipart_duplicate_names_follow_form_data_get_first_entry_semantics() {
    let (content_type, body) = multipart_parts(&[
        ("file", None, b"not-a-file"),
        ("file", Some(("second.txt", "text/plain")), b"second"),
    ]);
    assert!(
        multipart_envelope(multipart_request(&content_type, body), false)
            .await
            .is_err(),
        "a later file must not rescue a first same-named string entry"
    );

    let (content_type, body) = multipart_parts(&[
        ("file", Some(("first.txt", "text/plain")), b"first"),
        ("file", Some(("second.txt", "text/plain")), b"second"),
    ]);
    let selected = multipart_envelope(multipart_request(&content_type, body), false)
        .await
        .unwrap();
    assert_eq!(selected.file.filename, "first.txt");
    assert_eq!(selected.file.bytes, b"first");

    let (content_type, body) = multipart_parts(&[
        ("target", Some(("wrong.txt", "text/plain")), b"wrong"),
        ("target", None, b"conversation-2"),
        ("file", Some(("first.txt", "text/plain")), b"first"),
    ]);
    let selected = multipart_envelope(multipart_request(&content_type, body), true)
        .await
        .unwrap();
    assert_eq!(selected.target, None);
}

async fn call(
    app: axum::Router,
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

#[tokio::test]
async fn six_routes_share_real_sqlite_and_filesystem_authorities() {
    let (_directory, database, store, blobs, events) = seeded().await;
    let png_a = [0x89, 0x50, 0x4e, 0x47, 1, 2, 3, 4];
    let (content_type, body) = multipart("file", "shot.png", "image/png", &png_a);
    let (status, _, body) = call(
        app(&store, &blobs, &events, "owner"),
        Method::POST,
        "/api/channels/c/attachments",
        Some(&content_type),
        Body::from(body),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let uploaded = json_body(&body);
    let attachment = uploaded["id"].as_str().unwrap().to_owned();
    assert_eq!(uploaded["channel_id"], "c");
    assert_eq!(uploaded["source_conversation_id"], "c");
    assert_eq!(uploaded["content_type"], "image/png");
    assert_eq!(uploaded["content_revision"], 1);

    let (status, _, body) = call(
        app(&store, &blobs, &events, "owner"),
        Method::GET,
        "/api/channels/c/attachments",
        None,
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json_body(&body), json!({"items":[],"next_cursor":null}));

    let mut db = SqliteConnection::connect(database.to_str().unwrap())
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO messages(id,conversation_id,from_kind,from_user_id,author_id,author_name,\
         author_display_name,content,metadata,seq,created_at) VALUES \
         ('m','c','user','owner','owner','owner','Owner','',?,1,101)",
    )
    .bind(json!({"attachments":[{"id":attachment}]}).to_string())
    .execute(&mut db)
    .await
    .unwrap();

    let (status, _, body) = call(
        app(&store, &blobs, &events, "owner"),
        Method::GET,
        "/api/channels/c/attachments?limit=1&sender_kind=user&sender_id=owner&since=100&until=102",
        None,
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let listed = json_body(&body);
    assert_eq!(listed["items"][0]["attachment"]["id"], attachment);
    assert_eq!(listed["items"][0]["source"]["message_id"], "m");
    assert_eq!(listed["items"][0]["source"]["author"]["status"], "active");

    let (status, _, body) = call(
        app(&store, &blobs, &events, "owner"),
        Method::GET,
        "/api/channels/c/attachments/senders",
        None,
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json_body(&body)["senders"][0]["from_user_id"], "owner");

    let (status, headers, body) = call(
        app(&store, &blobs, &events, "owner"),
        Method::GET,
        &format!("/api/attachments/{attachment}"),
        None,
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, png_a);
    assert_eq!(headers[header::CONTENT_TYPE], "image/png");
    assert_eq!(headers[header::CACHE_CONTROL], "private, no-store");
    assert!(
        headers[header::CONTENT_DISPOSITION]
            .to_str()
            .unwrap()
            .contains("shot.png")
    );

    let png_b = [0x89, 0x50, 0x4e, 0x47, 9, 8, 7, 6, 5];
    let (content_type, body) = multipart("file", "ignored.png", "text/plain", &png_b);
    let (status, _, body) = call(
        app(&store, &blobs, &events, "owner"),
        Method::PATCH,
        &format!("/api/attachments/{attachment}"),
        Some(&content_type),
        Body::from(body),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json_body(&body)["content_revision"], 2);
    {
        let published = events.0.lock().unwrap();
        assert_eq!(published.len(), 1);
        assert_eq!(
            changed_event(&published[0]),
            json!({"attachment_id":attachment,"content_revision":2})
        );
    }

    let (status, _, body) = call(
        app(&store, &blobs, &events, "owner"),
        Method::GET,
        &format!("/api/attachments/{attachment}"),
        None,
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, png_b);

    let (status, _, _) = call(
        app(&store, &blobs, &events, "owner"),
        Method::DELETE,
        &format!("/api/attachments/{attachment}"),
        None,
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    db.execute("DELETE FROM messages WHERE id='m'")
        .await
        .unwrap();
    let (status, _, body) = call(
        app(&store, &blobs, &events, "owner"),
        Method::DELETE,
        &format!("/api/attachments/{attachment}"),
        None,
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(body.is_empty());
    let (status, _, body) = call(
        app(&store, &blobs, &events, "owner"),
        Method::GET,
        &format!("/api/attachments/{attachment}"),
        None,
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json_body(&body), json!({"error":"attachment not found"}));
}

#[tokio::test]
async fn upload_and_update_refusals_precede_every_body_shape() {
    let (_directory, _database, store, blobs, events) = seeded().await;
    let png = [0x89, 0x50, 0x4e, 0x47, 1];
    let (content_type, body) = multipart("file", "a.png", "image/png", &png);
    let (status, _, response) = call(
        app(&store, &blobs, &events, "owner"),
        Method::POST,
        "/api/channels/c/attachments",
        Some(&content_type),
        Body::from(body),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let attachment = json_body(&response)["id"].as_str().unwrap().to_owned();

    for (method, uri) in [
        (Method::POST, "/api/channels/c/attachments".to_owned()),
        (Method::PATCH, format!("/api/attachments/{attachment}")),
    ] {
        for declared in [None, Some(MULTIPART_ENVELOPE_BYTES + 1)] {
            let polled = Arc::new(AtomicBool::new(false));
            let observed = Arc::clone(&polled);
            let body = Body::from_stream(stream::once(async move {
                observed.store(true, Ordering::SeqCst);
                Ok::<Bytes, Infallible>(Bytes::from_static(b"never poll"))
            }));
            let mut builder = Request::builder().method(method.clone()).uri(&uri);
            if let Some(declared) = declared {
                builder = builder.header(header::CONTENT_LENGTH, declared);
            }
            let response = app(&store, &blobs, &events, "other")
                .oneshot(builder.body(body).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
            assert!(!polled.load(Ordering::SeqCst), "declared={declared:?}");
        }
    }

    let polled = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&polled);
    let body = Body::from_stream(stream::once(async move {
        observed.store(true, Ordering::SeqCst);
        Ok::<Bytes, Infallible>(Bytes::from_static(b"never poll"))
    }));
    let response = router(
        store.attachment_reads(),
        store.attachment_writes(),
        blobs.clone(),
        Environment::default(),
        events.clone(),
    )
    .oneshot(
        Request::builder()
            .method(Method::POST)
            .uri("/api/channels/c/attachments")
            .body(body)
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert!(!polled.load(Ordering::SeqCst));
}

#[tokio::test]
async fn multipart_envelope_preserves_the_full_file_cap_and_rejects_bad_shapes() {
    let (_directory, _database, store, blobs, events) = seeded().await;
    let route = || app(&store, &blobs, &events, "owner");

    let (content_type, body) = multipart("file", "declared.bin", "application/octet-stream", &[1]);
    let response = route()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/channels/c/attachments")
                .header(header::CONTENT_TYPE, content_type)
                .header(header::CONTENT_LENGTH, MULTIPART_ENVELOPE_BYTES + 1)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);

    let (status, _, _) = call(
        route(),
        Method::POST,
        "/api/channels/c/attachments",
        None,
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    for (field, bytes, expected) in [
        ("wrong", vec![1], StatusCode::BAD_REQUEST),
        ("file", Vec::new(), StatusCode::BAD_REQUEST),
        (
            "file",
            vec![1; MAX_FILE_BYTES + 1],
            StatusCode::PAYLOAD_TOO_LARGE,
        ),
        (
            "file",
            vec![1; MULTIPART_ENVELOPE_BYTES + 1],
            StatusCode::PAYLOAD_TOO_LARGE,
        ),
    ] {
        let (content_type, body) = multipart(field, "a.bin", "application/octet-stream", &bytes);
        let (status, _, _) = call(
            route(),
            Method::POST,
            "/api/channels/c/attachments",
            Some(&content_type),
            Body::from(body),
        )
        .await;
        assert_eq!(status, expected, "field={field}, bytes={}", bytes.len());
    }

    let exact = vec![1; MAX_FILE_BYTES];
    let (content_type, body) = multipart("file", "exact.bin", "application/octet-stream", &exact);
    assert!(body.len() > MAX_FILE_BYTES);
    assert!(body.len() < MULTIPART_ENVELOPE_BYTES);
    let (status, _, response) = call(
        route(),
        Method::POST,
        "/api/channels/c/attachments",
        Some(&content_type),
        Body::from(body),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&response)
    );
    assert_eq!(json_body(&response)["size"], MAX_FILE_BYTES as i64);
}

#[tokio::test]
async fn chunked_multipart_cannot_bypass_the_file_cap_without_content_length() {
    let (_directory, _database, store, blobs, events) = seeded().await;
    let (content_type, body) = multipart(
        "file",
        "chunked.bin",
        "application/octet-stream",
        &vec![1; MAX_FILE_BYTES + 1],
    );
    let chunks = body
        .chunks(64 * 1024)
        .map(Bytes::copy_from_slice)
        .collect::<Vec<_>>();
    let streamed = Body::from_stream(stream::iter(chunks.into_iter().map(Ok::<_, Infallible>)));

    let (status, _, _) = call(
        app(&store, &blobs, &events, "owner"),
        Method::POST,
        "/api/channels/c/attachments",
        Some(&content_type),
        streamed,
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn download_seals_committed_size_and_requires_the_physical_blob() {
    let (_directory, database, store, blobs, events) = seeded().await;
    let png = [0x89, 0x50, 0x4e, 0x47, 1, 2, 3];
    let (content_type, body) = multipart("file", "proof.png", "image/png", &png);
    let (status, _, response) = call(
        app(&store, &blobs, &events, "owner"),
        Method::POST,
        "/api/channels/c/attachments",
        Some(&content_type),
        Body::from(body),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let attachment = json_body(&response)["id"].as_str().unwrap().to_owned();

    let mut db = SqliteConnection::connect(database.to_str().unwrap())
        .await
        .unwrap();
    sqlx::query("UPDATE attachments SET size=size+1 WHERE id=?")
        .bind(&attachment)
        .execute(&mut db)
        .await
        .unwrap();
    let (status, _, _) = call(
        app(&store, &blobs, &events, "owner"),
        Method::GET,
        &format!("/api/attachments/{attachment}"),
        None,
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);

    sqlx::query("UPDATE attachments SET size=size-1 WHERE id=?")
        .bind(&attachment)
        .execute(&mut db)
        .await
        .unwrap();
    let storage_key: String = sqlx::query_scalar("SELECT storage_key FROM attachments WHERE id=?")
        .bind(&attachment)
        .fetch_one(&mut db)
        .await
        .unwrap();
    tokio::fs::remove_file(blobs.root().join(storage_key))
        .await
        .unwrap();
    let (status, _, _) = call(
        app(&store, &blobs, &events, "owner"),
        Method::GET,
        &format!("/api/attachments/{attachment}"),
        None,
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn publisher_failure_cannot_compensate_a_committed_pointer_swap() {
    let (_directory, _database, store, blobs, _events) = seeded().await;
    let route = || {
        router(
            store.attachment_reads(),
            store.attachment_writes(),
            blobs.clone(),
            Environment::default(),
            FailingEvents,
        )
        .layer(Extension(UserId::parse("owner").unwrap()))
    };
    let png_a = [0x89, 0x50, 0x4e, 0x47, 1];
    let (content_type, body) = multipart("file", "proof.png", "image/png", &png_a);
    let (status, _, response) = call(
        route(),
        Method::POST,
        "/api/channels/c/attachments",
        Some(&content_type),
        Body::from(body),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let attachment = json_body(&response)["id"].as_str().unwrap().to_owned();

    let png_b = [0x89, 0x50, 0x4e, 0x47, 9, 8, 7];
    let (content_type, body) = multipart("file", "ignored.png", "image/png", &png_b);
    let (status, _, response) = call(
        route(),
        Method::PATCH,
        &format!("/api/attachments/{attachment}"),
        Some(&content_type),
        Body::from(body),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json_body(&response)["content_revision"], 2);

    let (status, _, body) = call(
        route(),
        Method::GET,
        &format!("/api/attachments/{attachment}"),
        None,
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, png_b);
}
