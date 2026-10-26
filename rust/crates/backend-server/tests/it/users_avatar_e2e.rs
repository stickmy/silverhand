//! Full-route end-to-end coverage of the human-directory and avatar family
//! (`GET /api/users`, `POST`/`DELETE /api/me/avatar`, `GET /api/users/{id}/avatar`)
//! over a real SQLite database, driving the production application flows and the
//! production `SqliteUserStore` adapter through the actual axum router.
//!
//! Two collaborators are test doubles: the `user_changed` sink and the avatar
//! blob backend. The blob store is a deterministic
//! in-memory map so the lifecycle — put → serve → replace (old blob deleted) →
//! clear (blob deleted) → 404 — is observable without a filesystem.
//!
//! Sessions are minted by the production perimeter: the harness mounts the
//! `human_auth` signup route on the SAME database and reads the real `user_token`
//! out of the signup response, then presents it as `Authorization: Bearer` to the
//! avatar routes — exactly as a client does. A `FakeHasher` keeps signup
//! deterministic (the real Argon2id calibrates against the wall clock).

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header::HeaderName},
};
use serde_json::{Value, json};
use silverhand_backend_core::{
    foundation::identity::UserId,
    foundation::persistence::PersistenceFailure,
    host_runtime::environment::OsEntropy,
    identity_access::auth::{
        AuthenticateHumanSession, EntropySessionTokenMint, HumanSessionAuthenticator,
        SessionBearer, SessionClock, SessionTtl,
    },
    identity_access::avatar::{
        AvatarBlobPointer, AvatarBlobRead, AvatarBlobReadOutcome, AvatarBlobStore,
        AvatarContentType, AvatarStorageFailure,
    },
    identity_access::credential::{
        Argon2idCost, PasswordHashError, PasswordHashParams, PasswordHasher,
        PreparedPasswordCredential, RustCalibration,
    },
    identity_access::user_identity::{UserChangedAnnounceFailure, UserChangedSink},
};
use silverhand_backend_server::identity_access::{
    human_auth::{self, HumanAuthDeps},
    users::{self, UsersDeps},
};
use silverhand_backend_sqlite::{
    identity_access::auth::SqliteSessionStore, identity_access::credential::SqliteCredentialStore,
    identity_access::handle_namespace::SqliteHandleNamespace,
    identity_access::human_registration::SqliteHumanRegistration,
    identity_access::user_identity::SqliteUserStore,
};
use sqlx::Connection as _;
use tempfile::TempDir;
use tower::ServiceExt;

use crate::current_schema;

const NOW_MS: i64 = 10_000;
const SESSION_TTL_MS: i64 = 60_000;

#[derive(Clone)]
struct FixedClock;
impl SessionClock for FixedClock {
    fn now_unix_millis(&self) -> i64 {
        NOW_MS
    }
}

/// A deterministic hasher whose "PHC" is a tag over the password, so `verify` is an
/// exact match. Only signup uses it here (to mint a real session bearer).
#[derive(Clone)]
struct FakeHasher;
impl FakeHasher {
    fn phc_for(password: &str) -> String {
        format!("phc:{password}")
    }
}
impl PasswordHasher for FakeHasher {
    async fn hash(&self, password: &str) -> Result<PreparedPasswordCredential, PasswordHashError> {
        Ok(PreparedPasswordCredential {
            phc: Self::phc_for(password),
            params: PasswordHashParams {
                cost: Argon2idCost {
                    memory_cost_kib: 19 * 1024,
                    time_cost: 2,
                    parallelism: 1,
                    output_len: 32,
                },
                calibration: RustCalibration {
                    target_millis: 250,
                    measured_millis: 262,
                    candidate_count: 3,
                    floor_memory_cost_kib: 19 * 1024,
                    floor_time_cost: 2,
                },
            },
        })
    }
    async fn verify(&self, phc: &str, password: &str) -> Result<bool, PasswordHashError> {
        Ok(phc == Self::phc_for(password))
    }
    async fn verify_dummy(&self, _password: &str) -> Result<(), PasswordHashError> {
        Ok(())
    }
}

#[derive(Clone)]
struct CountingSink(Arc<AtomicUsize>);
impl CountingSink {
    fn new() -> Self {
        Self(Arc::new(AtomicUsize::new(0)))
    }
    fn count(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}
impl UserChangedSink for CountingSink {
    async fn user_changed(&self, _user_id: &UserId) -> Result<(), UserChangedAnnounceFailure> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// The stored `(pointer key, bytes)` pairs the in-memory blob backend holds.
type StoredBlobs = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

/// A deterministic in-memory avatar blob backend. Each `put` mints a fresh
/// sequential key (so a re-upload of
/// the same bytes never clobbers or cache-hits), stores the bytes, and records
/// deletes so the test can prove the replaced/cleared blob is cleaned up.
#[derive(Clone, Default)]
struct MemoryBlobStore {
    blobs: StoredBlobs,
    deleted: Arc<Mutex<Vec<String>>>,
    next_id: Arc<AtomicUsize>,
}
impl MemoryBlobStore {
    fn deleted(&self) -> Vec<String> {
        self.deleted.lock().unwrap().clone()
    }
    fn live_count(&self) -> usize {
        self.blobs.lock().unwrap().len()
    }
}
impl AvatarBlobRead for MemoryBlobStore {
    async fn read(
        &self,
        pointer: &AvatarBlobPointer,
    ) -> Result<AvatarBlobReadOutcome, AvatarStorageFailure> {
        let blobs = self.blobs.lock().unwrap();
        match blobs.iter().find(|(key, _)| key == pointer.as_str()) {
            Some((_, bytes)) => Ok(AvatarBlobReadOutcome::Found(bytes.clone())),
            None => Ok(AvatarBlobReadOutcome::Missing),
        }
    }
}
impl AvatarBlobStore for MemoryBlobStore {
    async fn put(
        &self,
        user_id: &UserId,
        content_type: AvatarContentType,
        bytes: Vec<u8>,
    ) -> Result<AvatarBlobPointer, AvatarStorageFailure> {
        let n = self.next_id.fetch_add(1, Ordering::SeqCst);
        let key = format!(
            "users/{}/avatars/{n}.{}",
            user_id.as_str(),
            content_type.subtype()
        );
        self.blobs.lock().unwrap().push((key.clone(), bytes));
        Ok(AvatarBlobPointer::parse(key).unwrap())
    }
    async fn delete(&self, pointer: &AvatarBlobPointer) -> Result<(), AvatarStorageFailure> {
        self.deleted
            .lock()
            .unwrap()
            .push(pointer.as_str().to_owned());
        self.blobs
            .lock()
            .unwrap()
            .retain(|(key, _)| key != pointer.as_str());
        Ok(())
    }
}

type AuthDeps = HumanAuthDeps<
    SqliteHandleNamespace,
    SqliteUserStore,
    FakeHasher,
    SqliteHumanRegistration,
    EntropySessionTokenMint<OsEntropy>,
    SqliteSessionStore,
    SqliteSessionStore,
    SqliteCredentialStore,
    SqliteUserStore,
    SqliteUserStore,
    CountingSink,
    FixedClock,
>;

type UsersDepsT = UsersDeps<
    SqliteUserStore,
    SqliteUserStore,
    SqliteUserStore,
    SqliteUserStore,
    MemoryBlobStore,
    CountingSink,
    FixedClock,
>;

struct Backend {
    _directory: TempDir,
    sink: CountingSink,
    blobs: MemoryBlobStore,
    app: Router,
}

/// Wire the avatar/directory routes AND the signup route over one real SQLite file,
/// sharing the session store so a signup-minted bearer authenticates the avatar
/// routes. The avatar routes get the production `SqliteUserStore` plus the two
/// test doubles (sink, blob store).
async fn backend() -> Backend {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("users-avatar.sqlite");
    current_schema::create_current_schema(&path)
        .await
        .close()
        .await
        .unwrap();

    let users_store = SqliteUserStore::open(&path).await.unwrap();
    let sessions = SqliteSessionStore::open(&path).await.unwrap();
    let sink = CountingSink::new();
    let blobs = MemoryBlobStore::default();

    let auth_deps: AuthDeps = HumanAuthDeps {
        handles: SqliteHandleNamespace::open(&path).await.unwrap(),
        emails: users_store.clone(),
        hasher: FakeHasher,
        registration: SqliteHumanRegistration::open(&path).await.unwrap(),
        mint: EntropySessionTokenMint::new(OsEntropy),
        sessions: sessions.clone(),
        revoke: sessions.clone(),
        credentials: SqliteCredentialStore::open(&path).await.unwrap(),
        reads: users_store.clone(),
        writes: users_store.clone(),
        user_changed: sink.clone(),
        clock: FixedClock,
        signup_enabled: true,
    };
    let users_deps: UsersDepsT = UsersDeps {
        directory: users_store.clone(),
        avatar_refs: users_store.clone(),
        reads: users_store.clone(),
        avatar_writes: users_store,
        blobs: blobs.clone(),
        user_changed: sink.clone(),
        clock: FixedClock,
    };

    let ttl = SessionTtl::from_millis(SESSION_TTL_MS).unwrap();
    let auth_app = human_auth::router(
        auth_deps,
        HumanSessionAuthenticator::new(sessions.clone(), FixedClock, ttl),
    );
    let users_app = users::router(
        users_deps,
        HumanSessionAuthenticator::new(sessions, FixedClock, ttl),
    );

    Backend {
        _directory: directory,
        sink,
        blobs,
        app: auth_app.merge(users_app),
    }
}

/// A perimeter double that resolves ANY presented bearer to a fixed principal id.
/// Paired with a database that has no such `users` row, it produces the
/// dangling-session state — a valid session principal whose row is gone — that a
/// real signup-then-delete cannot reach, because `auth_sessions` is
/// `ON DELETE CASCADE` on `users` (deleting the user takes the session with it,
/// yielding a perimeter 401 instead). It lets the real `users::router` exercise
/// the refusal PRECEDENCE the `me` routes ratified: authenticated principal →
/// transport decode → existence.
#[derive(Clone)]
struct DanglingSessionAuth {
    principal: UserId,
}
impl AuthenticateHumanSession for DanglingSessionAuth {
    async fn recheck_session(
        &self,
        bearer: SessionBearer,
    ) -> Result<Option<UserId>, PersistenceFailure> {
        self.authenticate_session(bearer).await
    }
    async fn authenticate_session(
        &self,
        _bearer: SessionBearer,
    ) -> Result<Option<UserId>, PersistenceFailure> {
        Ok(Some(self.principal.clone()))
    }
}

/// The real `users::router` over a real `SqliteUserStore` whose database has NO
/// users row, behind the [`DanglingSessionAuth`] double, so every authenticated
/// request resolves to a principal that does not exist.
async fn dangling_backend() -> Backend {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("users-avatar-dangling.sqlite");
    current_schema::create_current_schema(&path)
        .await
        .close()
        .await
        .unwrap();

    let users_store = SqliteUserStore::open(&path).await.unwrap();
    let sink = CountingSink::new();
    let blobs = MemoryBlobStore::default();
    let users_deps: UsersDepsT = UsersDeps {
        directory: users_store.clone(),
        avatar_refs: users_store.clone(),
        reads: users_store.clone(),
        avatar_writes: users_store,
        blobs: blobs.clone(),
        user_changed: sink.clone(),
        clock: FixedClock,
    };
    let app = users::router(
        users_deps,
        DanglingSessionAuth {
            principal: UserId::parse("ghost-user").unwrap(),
        },
    );
    Backend {
        _directory: directory,
        sink,
        blobs,
        app,
    }
}

// ===========================================================================
// Request helpers
// ===========================================================================

async fn call(
    app: &Router,
    request: Request<Body>,
) -> (StatusCode, Vec<(String, String)>, Vec<u8>) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response
        .headers()
        .iter()
        .map(|(name, value)| (name.as_str().to_owned(), value.to_str().unwrap().to_owned()))
        .collect();
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, headers, bytes)
}

fn json_of(bytes: &[u8]) -> Value {
    if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(bytes).unwrap_or(Value::Null)
    }
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    let wanted = HeaderName::from_bytes(name.as_bytes()).unwrap();
    headers
        .iter()
        .find(|(key, _)| key.as_str() == wanted.as_str())
        .map(|(_, value)| value.as_str())
}

fn signup_body() -> Value {
    json!({
        "email": "alice@example.com",
        "password": "correct-horse",
        "name": "alice",
        "display_name": "Alice A"
    })
}

/// Sign up and return `(token, user_id)`.
async fn signed_up(app: &Router) -> (String, String) {
    let request = Request::builder()
        .method("POST")
        .uri(human_auth::SIGNUP_PATH)
        .header("content-type", "application/json")
        .body(Body::from(signup_body().to_string()))
        .unwrap();
    let (status, _, bytes) = call(app, request).await;
    assert_eq!(status, StatusCode::OK);
    let body = json_of(&bytes);
    (
        body["user_token"].as_str().unwrap().to_owned(),
        body["user"]["id"].as_str().unwrap().to_owned(),
    )
}

fn authed_get(path: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(path)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

fn authed_delete(path: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(path)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

/// A multipart upload request. `filename`/`content_type` are optional so the tests
/// can drive the "not a File" (no filename) and "no declared type" cases.
fn upload(
    token: &str,
    field_name: &str,
    filename: Option<&str>,
    content_type: Option<&str>,
    bytes: &[u8],
) -> Request<Body> {
    let boundary = "XBOUNDARY123";
    let mut body: Vec<u8> = Vec::new();
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    let mut disposition = format!("Content-Disposition: form-data; name=\"{field_name}\"");
    if let Some(name) = filename {
        disposition.push_str(&format!("; filename=\"{name}\""));
    }
    body.extend_from_slice(disposition.as_bytes());
    body.extend_from_slice(b"\r\n");
    if let Some(content_type) = content_type {
        body.extend_from_slice(format!("Content-Type: {content_type}\r\n").as_bytes());
    }
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    Request::builder()
        .method("POST")
        .uri(users::ME_AVATAR_PATH)
        .header("authorization", format!("Bearer {token}"))
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(body))
        .unwrap()
}

fn avatar_path(user_id: &str) -> String {
    format!("/api/users/{user_id}/avatar")
}

// ===========================================================================
// The lifecycle
// ===========================================================================

#[tokio::test]
async fn full_avatar_lifecycle_upload_serve_replace_clear_then_missing() {
    let backend = backend().await;
    let (token, user_id) = signed_up(&backend.app).await;
    let announces_after_signup = backend.sink.count();

    // The directory lists the freshly created user with a null avatar_url.
    let (status, _, bytes) = call(&backend.app, authed_get(users::USERS_PATH, &token)).await;
    assert_eq!(status, StatusCode::OK);
    let directory = json_of(&bytes);
    let users = directory["users"].as_array().unwrap();
    assert_eq!(users.len(), 1);
    assert_eq!(users[0]["id"], json!(user_id));
    assert_eq!(users[0].get("avatar_url"), Some(&Value::Null));

    // Fetching the avatar before one exists is a product 404.
    let (status, _, bytes) = call(&backend.app, authed_get(&avatar_path(&user_id), &token)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json_of(&bytes), json!({ "error": "avatar not found" }));

    // Upload a PNG: 200, the response mirrors avatar_url at top level and inside
    // user, the URL is cache-busted by the row's updated_at, one announce fires.
    let png = vec![0x89, b'P', b'N', b'G', 1, 2, 3];
    let (status, _, bytes) = call(
        &backend.app,
        upload(&token, "file", Some("me.png"), Some("image/png"), &png),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let uploaded = json_of(&bytes);
    let expected_url = json!(format!("/api/users/{user_id}/avatar?v={NOW_MS}"));
    assert_eq!(uploaded["avatar_url"], expected_url);
    assert_eq!(uploaded["user"]["avatar_url"], expected_url);
    assert_eq!(uploaded["user"]["id"], json!(user_id));
    assert_eq!(backend.sink.count(), announces_after_signup + 1);
    assert_eq!(backend.blobs.live_count(), 1);
    assert!(
        backend.blobs.deleted().is_empty(),
        "first upload has no prior blob"
    );

    // Serve the bytes back with the product's header set.
    let (status, headers, served) =
        call(&backend.app, authed_get(&avatar_path(&user_id), &token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(served, png);
    assert_eq!(header(&headers, "content-type"), Some("image/png"));
    assert_eq!(
        header(&headers, "content-length"),
        Some(png.len().to_string().as_str())
    );
    assert_eq!(
        header(&headers, "content-disposition"),
        Some("inline; filename=\"avatar.png\"; filename*=UTF-8''avatar.png")
    );
    assert_eq!(header(&headers, "cache-control"), Some("private, no-store"));

    // Replace with a WEBP: the previous blob is deleted, a fresh one stored, and
    // another announce fires. The stored content type moves to image/webp.
    let webp = vec![b'R', b'I', b'F', b'F', 9, 9];
    let (status, _, _) = call(
        &backend.app,
        upload(&token, "file", Some("me.webp"), Some("image/webp"), &webp),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(backend.sink.count(), announces_after_signup + 2);
    assert_eq!(
        backend.blobs.live_count(),
        1,
        "the replaced blob is gone, the new one remains"
    );
    assert_eq!(
        backend.blobs.deleted().len(),
        1,
        "exactly the replaced blob was deleted"
    );

    let (status, headers, served) =
        call(&backend.app, authed_get(&avatar_path(&user_id), &token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(served, webp);
    assert_eq!(header(&headers, "content-type"), Some("image/webp"));
    assert_eq!(
        header(&headers, "content-disposition"),
        Some("inline; filename=\"avatar.webp\"; filename*=UTF-8''avatar.webp")
    );

    // Clear: 200 with the bare DTO (avatar_url null), the blob deleted, one announce.
    let (status, _, bytes) = call(&backend.app, authed_delete(users::ME_AVATAR_PATH, &token)).await;
    assert_eq!(status, StatusCode::OK);
    let cleared = json_of(&bytes);
    assert_eq!(cleared.get("avatar_url"), Some(&Value::Null));
    assert_eq!(cleared["id"], json!(user_id));
    assert!(
        cleared.get("user").is_none(),
        "clear returns the bare DTO, not {{ avatar_url, user }}"
    );
    assert_eq!(backend.sink.count(), announces_after_signup + 3);
    assert_eq!(backend.blobs.live_count(), 0);

    // A clear of the now-clear row is a no-op: no announce, no delete.
    let deletes_before = backend.blobs.deleted().len();
    let (status, _, bytes) = call(&backend.app, authed_delete(users::ME_AVATAR_PATH, &token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json_of(&bytes).get("avatar_url"), Some(&Value::Null));
    assert_eq!(
        backend.sink.count(),
        announces_after_signup + 3,
        "a no-op clear never announces"
    );
    assert_eq!(
        backend.blobs.deleted().len(),
        deletes_before,
        "a no-op clear deletes nothing"
    );

    // The avatar fetch is a 404 again once cleared.
    let (status, _, bytes) = call(&backend.app, authed_get(&avatar_path(&user_id), &token)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json_of(&bytes), json!({ "error": "avatar not found" }));
}

// ===========================================================================
// Focused refusals
// ===========================================================================

#[tokio::test]
async fn directory_without_a_bearer_is_the_perimeter_401() {
    let backend = backend().await;
    let request = Request::builder()
        .method("GET")
        .uri(users::USERS_PATH)
        .body(Body::empty())
        .unwrap();
    let (status, _, bytes) = call(&backend.app, request).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(json_of(&bytes), json!({ "error": "unauthorized" }));
}

#[tokio::test]
async fn upload_of_a_non_multipart_body_is_multipart_required() {
    let backend = backend().await;
    let (token, _) = signed_up(&backend.app).await;
    let request = Request::builder()
        .method("POST")
        .uri(users::ME_AVATAR_PATH)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let (status, _, bytes) = call(&backend.app, request).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        json_of(&bytes),
        json!({ "error": "multipart/form-data required" })
    );
}

#[tokio::test]
async fn upload_without_a_file_part_is_file_field_required() {
    let backend = backend().await;
    let (token, _) = signed_up(&backend.app).await;
    // A `file` field that is a plain field (no filename) is not a `File`.
    let (status, _, bytes) = call(
        &backend.app,
        upload(&token, "file", None, Some("image/png"), b"x"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json_of(&bytes), json!({ "error": "file field required" }));
}

#[tokio::test]
async fn upload_of_an_unsupported_type_is_415() {
    let backend = backend().await;
    let (token, _) = signed_up(&backend.app).await;
    let (status, _, bytes) = call(
        &backend.app,
        upload(&token, "file", Some("x.txt"), Some("text/plain"), b"hello"),
    )
    .await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(
        json_of(&bytes),
        json!({ "error": "Avatar must be a PNG, JPEG, GIF, or WEBP image.", "code": "avatar_unsupported_type" })
    );
}

#[tokio::test]
async fn upload_with_no_declared_type_falls_back_to_octet_stream_and_is_415() {
    let backend = backend().await;
    let (token, _) = signed_up(&backend.app).await;
    // No Content-Type on the part → `application/octet-stream` → outside the allowlist.
    let (status, _, bytes) = call(
        &backend.app,
        upload(&token, "file", Some("me"), None, b"bytes"),
    )
    .await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(json_of(&bytes)["code"], json!("avatar_unsupported_type"));
}

#[tokio::test]
async fn upload_of_an_empty_file_is_avatar_missing_400() {
    let backend = backend().await;
    let (token, _) = signed_up(&backend.app).await;
    let (status, _, bytes) = call(
        &backend.app,
        upload(&token, "file", Some("me.png"), Some("image/png"), b""),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        json_of(&bytes),
        json!({ "error": "Avatar file is empty.", "code": "avatar_missing" })
    );
    assert_eq!(
        backend.blobs.live_count(),
        0,
        "a rejected upload writes no blob"
    );
}

#[tokio::test]
async fn fetch_of_an_unknown_user_id_is_404_not_found() {
    let backend = backend().await;
    let (token, _) = signed_up(&backend.app).await;
    let (status, _, bytes) = call(
        &backend.app,
        authed_get(&avatar_path("nobody-here"), &token),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json_of(&bytes), json!({ "error": "avatar not found" }));
}

// ===========================================================================
// Dangling-session refusal precedence (P0-1 lock)
// ===========================================================================

#[tokio::test]
async fn dangling_session_with_a_well_formed_upload_is_a_safe_500_and_writes_no_blob() {
    let backend = dangling_backend().await;
    let png = vec![0x89, b'P', b'N', b'G', 1, 2, 3];
    // Well-formed multipart, a valid principal token, but no user row: the domain's
    // existence pre-check (which follows a successful transport decode) yields the
    // dangling-pointer safe-500 — a bare text/plain 500, never a product body.
    let (status, _, body) = call(
        &backend.app,
        upload(
            "any-bearer",
            "file",
            Some("me.png"),
            Some("image/png"),
            &png,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body, b"Internal Server Error");
    assert_eq!(
        backend.blobs.live_count(),
        0,
        "existence precedes the put: a dangling session writes no blob"
    );
    assert_eq!(backend.blobs.deleted().len(), 0);
    assert_eq!(backend.sink.count(), 0, "no announce on a non-commit");
}

#[tokio::test]
async fn dangling_session_with_a_malformed_body_is_the_multipart_400_before_existence() {
    let backend = dangling_backend().await;
    // A non-multipart body with the same dangling principal: transport decode fails
    // FIRST, so the response is the 400 multipart refusal — the existence check
    // (which would 500) never runs. This locks the ratified precedence: authenticated
    // principal → transport decode → existence (not authority-first).
    let request = Request::builder()
        .method("POST")
        .uri(users::ME_AVATAR_PATH)
        .header("authorization", "Bearer any-bearer")
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let (status, _, bytes) = call(&backend.app, request).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        json_of(&bytes),
        json!({ "error": "multipart/form-data required" })
    );
    assert_eq!(backend.blobs.live_count(), 0);
    assert_eq!(backend.sink.count(), 0);
}
