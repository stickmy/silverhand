//! Full-route end-to-end coverage of the five human credentialed-session routes
//! over a real SQLite database, driving the production application flows and the
//! production persistence adapters through the actual axum router.
//!
//! Only two collaborators are test doubles, and only because determinism demands
//! it: the password hasher (the real Argon2id calibrates against the wall clock
//! and would make every run slow and time-dependent) and the `user_changed` sink
//! (counted here; WebSocket delivery is owned by its transport integration tests).
//! Everything else is the shipping code: `SqliteHandleNamespace`,
//! `SqliteUserStore`, `SqliteHumanRegistration`, `SqliteCredentialStore`,
//! `SqliteSessionStore`, the real `EntropySessionTokenMint`, the real
//! `HumanSessionAuthenticator` perimeter, and the `human_auth::router` topology.
//!
//! The session token is minted by the production CSPRNG mint, so a test can never
//! know it in advance: every authenticated assertion reads the `user_token` out of
//! the signup/login response body and presents it back as `Authorization: Bearer`,
//! exactly as a client does. This proves the mint → persisted-hash → presented-
//! bearer → authenticator loop closes end to end.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use silverhand_backend_core::{
    foundation::identity::UserId,
    host_runtime::environment::OsEntropy,
    identity_access::auth::{
        EntropySessionTokenMint, HumanSessionAuthenticator, SessionClock, SessionTtl,
    },
    identity_access::credential::{
        Argon2idCost, PasswordHashError, PasswordHashParams, PasswordHasher,
        PreparedPasswordCredential, RustCalibration,
    },
    identity_access::user_identity::{UserChangedAnnounceFailure, UserChangedSink},
};
use silverhand_backend_server::identity_access::human_auth::{self, HumanAuthDeps};
use silverhand_backend_sqlite::{
    identity_access::auth::SqliteSessionStore, identity_access::credential::SqliteCredentialStore,
    identity_access::handle_namespace::SqliteHandleNamespace,
    identity_access::human_registration::SqliteHumanRegistration,
    identity_access::user_identity::SqliteUserStore,
};
use sqlx::Connection;
use tempfile::TempDir;
use tower::ServiceExt;

use crate::current_schema;

/// The request instant every flow reads. Fixed so `created_at`/`updated_at` land
/// on a known value and the session stays inside its TTL for the whole scenario.
const NOW_MS: i64 = 10_000;
const SESSION_TTL_MS: i64 = 60_000;

#[derive(Clone)]
struct FixedClock;

impl SessionClock for FixedClock {
    fn now_unix_millis(&self) -> i64 {
        NOW_MS
    }
}

/// A deterministic hasher: the "PHC" is just a tag over the password, so `verify`
/// is an exact-match check. It records nothing beyond what the flows need, and its
/// params are a fixed valid Rust-arm shape (the persistence codec serializes them).
/// It counts `verify_dummy` calls so a test can prove the uniform-timing path is
/// walked for a login that resolves to no verifiable credential.
#[derive(Clone)]
struct FakeHasher {
    dummy_calls: Arc<AtomicUsize>,
}

impl FakeHasher {
    fn new() -> Self {
        Self {
            dummy_calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn phc_for(password: &str) -> String {
        format!("phc:{password}")
    }

    fn dummy_calls(&self) -> usize {
        self.dummy_calls.load(Ordering::SeqCst)
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
        self.dummy_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// A sink that accepts every hint and counts the calls, so a test can assert the
/// committed identity change actually announced.
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

/// The concrete port bundle every route in this test shares.
type Deps = HumanAuthDeps<
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

struct Backend {
    _directory: TempDir,
    path: std::path::PathBuf,
    sink: CountingSink,
    hasher: FakeHasher,
    app: Router,
}

/// Wire the five routes over one real SQLite file with the production adapters.
async fn backend(signup_enabled: bool) -> Backend {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("human-auth.sqlite");
    // Materialize the current schema, then hand every adapter its own pool onto the
    // same file (WAL) — the shipping composition-root shape.
    current_schema::create_current_schema(&path)
        .await
        .close()
        .await
        .unwrap();

    let users = SqliteUserStore::open(&path).await.unwrap();
    let sessions = SqliteSessionStore::open(&path).await.unwrap();
    let sink = CountingSink::new();
    let hasher = FakeHasher::new();

    let deps: Deps = HumanAuthDeps {
        handles: SqliteHandleNamespace::open(&path).await.unwrap(),
        emails: users.clone(),
        hasher: hasher.clone(),
        registration: SqliteHumanRegistration::open(&path).await.unwrap(),
        mint: EntropySessionTokenMint::new(OsEntropy),
        sessions: sessions.clone(),
        revoke: sessions.clone(),
        credentials: SqliteCredentialStore::open(&path).await.unwrap(),
        reads: users.clone(),
        writes: users,
        user_changed: sink.clone(),
        clock: FixedClock,
        signup_enabled,
    };

    let authority = HumanSessionAuthenticator::new(
        sessions,
        FixedClock,
        SessionTtl::from_millis(SESSION_TTL_MS).unwrap(),
    );
    Backend {
        _directory: directory,
        path,
        sink,
        hasher,
        app: human_auth::router(deps, authority),
    }
}

/// Read the single `auth_sessions.token_hash` persisted for the scenario, opening
/// the test tempfile read-only (never a live product database).
async fn stored_token_hashes(path: &std::path::Path) -> Vec<String> {
    let mut connection = sqlx::SqliteConnection::connect(&format!("sqlite://{}", path.display()))
        .await
        .unwrap();
    let rows: Vec<(String,)> = sqlx::query_as("SELECT token_hash FROM auth_sessions")
        .fetch_all(&mut connection)
        .await
        .unwrap();
    connection.close().await.unwrap();
    rows.into_iter().map(|(hash,)| hash).collect()
}

/// Stamp an avatar onto a `users` row (the test tempfile), leaving `updated_at`
/// untouched so the cache-buster the projection appends is known. The schema's
/// CHECK pairs the storage key with a content type, so both are written. No route
/// sets an avatar, so this simulates a user who already has one.
async fn set_avatar_storage_key(path: &std::path::Path, user_id: &str, key: &str) {
    let mut connection = sqlx::SqliteConnection::connect(&format!("sqlite://{}", path.display()))
        .await
        .unwrap();
    sqlx::query(
        "UPDATE users SET avatar_storage_key = ?, avatar_content_type = 'image/png' WHERE id = ?",
    )
    .bind(key)
    .bind(user_id)
    .execute(&mut connection)
    .await
    .unwrap();
    connection.close().await.unwrap();
}

async fn call(app: &Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, body)
}

fn post_json(path: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn signup_body() -> Value {
    json!({
        "email": "Alice@Example.com",
        "password": "correct-horse",
        "name": "alice",
        "display_name": "Alice A"
    })
}

/// The `user_token` from a successful signup/login response.
fn token_of(body: &Value) -> String {
    body.get("user_token")
        .and_then(Value::as_str)
        .expect("a session response carries user_token")
        .to_owned()
}

#[tokio::test]
async fn signup_creates_an_identity_and_returns_a_usable_session() {
    let backend = backend(true).await;

    let (status, body) = call(
        &backend.app,
        post_json(human_auth::SIGNUP_PATH, signup_body()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // The DTO is the bare snake_case projection: no email, avatar_url null, name is
    // the handle, and the timestamps are the fixed request instant.
    let user = &body["user"];
    assert_eq!(user["name"], json!("alice"));
    assert_eq!(user["display_name"], json!("Alice A"));
    assert_eq!(user["avatar_url"], Value::Null);
    assert_eq!(user["created_at"], json!(NOW_MS));
    assert_eq!(user["updated_at"], json!(NOW_MS));
    assert!(user.get("email").is_none(), "email must never be projected");
    assert!(user["id"].as_str().is_some_and(|id| !id.is_empty()));
    assert_eq!(backend.sink.count(), 1, "signup owes one user_changed hint");

    // The minted token authenticates GET /api/me end to end.
    let token = token_of(&body);
    let stored = stored_token_hashes(&backend.path).await;
    assert_eq!(stored.len(), 1, "signup issues exactly one session");
    assert_ne!(stored[0], token, "the plaintext bearer must not be stored");
    assert!(!stored[0].is_empty());
    let (status, me) = call(&backend.app, authed_get(human_auth::ME_PATH, &token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me["name"], json!("alice"));
    assert_eq!(me["id"], user["id"]);
    // Only the caller's own profile carries the email.
    assert_eq!(me["email"], json!("alice@example.com"));
}

#[tokio::test]
async fn signup_with_a_taken_handle_is_a_409_error_code() {
    let backend = backend(true).await;
    let (first, _) = call(
        &backend.app,
        post_json(human_auth::SIGNUP_PATH, signup_body()),
    )
    .await;
    assert_eq!(first, StatusCode::OK);

    // Same handle, a different email — the security-ordered flow answers handle_taken.
    let clash = json!({
        "email": "other@example.com",
        "password": "correct-horse",
        "name": "alice",
        "display_name": "Impostor"
    });
    let (status, body) = call(&backend.app, post_json(human_auth::SIGNUP_PATH, clash)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body, json!({ "error_code": "handle_taken" }));
}

#[tokio::test]
async fn signup_disabled_answers_a_typed_403_before_the_body() {
    let backend = backend(false).await;
    // A body that would otherwise succeed still gets the disabled answer.
    let (status, body) = call(
        &backend.app,
        post_json(human_auth::SIGNUP_PATH, signup_body()),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        body,
        json!({ "error": "signup is disabled on this backend", "code": "signup_disabled" })
    );
}

#[tokio::test]
async fn signup_with_a_short_password_is_a_422_invalid_input() {
    let backend = backend(true).await;
    let body = json!({
        "email": "alice@example.com",
        "password": "short",
        "name": "alice",
        "display_name": "Alice A"
    });
    let (status, body) = call(&backend.app, post_json(human_auth::SIGNUP_PATH, body)).await;
    // A well-formed body whose password fails the garde length bound is the
    // minimal semantic refusal under the frozen admission model.
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body, json!({ "error": "invalid input" }));
}

#[tokio::test]
async fn signup_from_a_value_shaped_array_is_a_native_rejection_with_zero_effect() {
    let backend = backend(true).await;

    // The sharp hazard the shared object floor defeats: a top-level JSON array
    // whose elements line up by TYPE, VALUE and ORDER with `SignupRaw`'s fields
    // (email, password, name, display_name) would — if decoded positionally by a
    // derived struct visitor — build a fully valid signup. The object floor
    // rejects the sequence SHAPE before any field is read, so it is a native
    // structural rejection (not our semantic `{ "error": "invalid input" }`), and
    // no identity is ever created.
    let array = json!(["alice@example.com", "correct-horse", "alice", "Alice A"]);
    let (status, body) = call(&backend.app, post_json(human_auth::SIGNUP_PATH, array)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_ne!(body, json!({ "error": "invalid input" }));
    assert_eq!(
        backend.sink.count(),
        0,
        "a floor-rejected body must never create a user"
    );
    // Zero authority reach: those credentials name no account, so a login with the
    // exact array values answers the uniform unknown 401.
    let login = json!({ "email": "alice@example.com", "password": "correct-horse" });
    let (status, _) = call(&backend.app, post_json(human_auth::LOGIN_PATH, login)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn login_verifies_the_stored_credential_and_mints_a_fresh_session() {
    let backend = backend(true).await;
    call(
        &backend.app,
        post_json(human_auth::SIGNUP_PATH, signup_body()),
    )
    .await;

    let login = json!({ "email": "alice@example.com", "password": "correct-horse" });
    let (status, body) = call(&backend.app, post_json(human_auth::LOGIN_PATH, login)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["user"]["name"], json!("alice"));

    // The login-minted token also authenticates.
    let token = token_of(&body);
    let (status, _) = call(&backend.app, authed_get(human_auth::ME_PATH, &token)).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn login_with_a_wrong_password_is_the_uniform_401() {
    let backend = backend(true).await;
    call(
        &backend.app,
        post_json(human_auth::SIGNUP_PATH, signup_body()),
    )
    .await;

    let login = json!({ "email": "alice@example.com", "password": "wrong-password" });
    let (status, body) = call(&backend.app, post_json(human_auth::LOGIN_PATH, login)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body, json!({ "error_code": "auth_failed" }));
}

#[tokio::test]
async fn login_for_an_unknown_email_is_the_same_uniform_401() {
    let backend = backend(true).await;
    let login = json!({ "email": "nobody@example.com", "password": "correct-horse" });
    let (status, body) = call(&backend.app, post_json(human_auth::LOGIN_PATH, login)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body, json!({ "error_code": "auth_failed" }));
    // The unknown-email path spends the dummy verify so its timing matches a real
    // wrong-password verify — the login flow must not skip the cost.
    assert_eq!(
        backend.hasher.dummy_calls(),
        1,
        "an unknown email must still spend one dummy verify"
    );
}

#[tokio::test]
async fn login_from_a_value_shaped_array_is_a_native_rejection_with_zero_effect() {
    let backend = backend(true).await;
    call(
        &backend.app,
        post_json(human_auth::SIGNUP_PATH, signup_body()),
    )
    .await;
    let sessions_before = stored_token_hashes(&backend.path).await.len();

    // A top-level array lined up by type/value/order with `LoginRaw`'s fields
    // (email, password) would positionally decode into the real account's
    // credentials. The object floor rejects the sequence before any verify runs,
    // so it is a native structural rejection, no session is minted, and the
    // credential verify cost is never even spent.
    let array = json!(["alice@example.com", "correct-horse"]);
    let (status, body) = call(&backend.app, post_json(human_auth::LOGIN_PATH, array)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_ne!(body, json!({ "error": "invalid input" }));
    assert!(
        body.get("user_token").is_none(),
        "a floor-rejected login must mint no session"
    );
    assert_eq!(
        stored_token_hashes(&backend.path).await.len(),
        sessions_before,
        "a floor-rejected login must add no session row"
    );
}

#[tokio::test]
async fn get_me_projects_a_cache_busted_avatar_url_when_the_user_has_one() {
    let backend = backend(true).await;
    let (_, signup) = call(
        &backend.app,
        post_json(human_auth::SIGNUP_PATH, signup_body()),
    )
    .await;
    let token = token_of(&signup);
    let user_id = signup["user"]["id"].as_str().unwrap().to_owned();

    // A freshly signed-up user has no avatar — the DTO projects null.
    assert_eq!(signup["user"]["avatar_url"], Value::Null);

    // Give the user an avatar, then read the profile back: the projection folds the
    // storage key into `/api/users/:id/avatar?v=:updated_at`, cache-busted by the
    // row's fixed `updated_at` (NOW_MS, untouched by the stamp).
    set_avatar_storage_key(&backend.path, &user_id, "avatars/alice.png").await;
    let (status, me) = call(&backend.app, authed_get(human_auth::ME_PATH, &token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        me["avatar_url"],
        json!(format!("/api/users/{user_id}/avatar?v={NOW_MS}"))
    );
}

#[tokio::test]
async fn get_me_applies_public_bearer_grammar_to_the_minted_session() {
    #[derive(serde::Deserialize)]
    struct BearerCase {
        name: String,
        header: Option<String>,
        rust_extracted_token: Option<String>,
    }
    let backend = backend(true).await;
    let (status, signup) = call(
        &backend.app,
        post_json(human_auth::SIGNUP_PATH, signup_body()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let token = token_of(&signup);
    let cases: Vec<BearerCase> = serde_json::from_str(include_str!(
        "../../src/identity_access/auth/fixtures/public_bearer_grammar.json"
    ))
    .unwrap();
    for case in cases {
        let mut request = Request::builder().method("GET").uri(human_auth::ME_PATH);
        if let Some(header) = case.header {
            request = request.header("authorization", header.replace("valid-token", &token));
        }
        let (status, body) = call(&backend.app, request.body(Body::empty()).unwrap()).await;
        if case.rust_extracted_token.as_deref() == Some("valid-token") {
            assert_eq!(status, StatusCode::OK, "{}", case.name);
            assert_eq!(body["id"], signup["user"]["id"], "{}", case.name);
        } else {
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{}", case.name);
            assert_eq!(body, json!({ "error": "unauthorized" }), "{}", case.name);
        }
    }
}

#[tokio::test]
async fn patch_me_updates_the_display_name_and_announces() {
    let backend = backend(true).await;
    let (_, signup) = call(
        &backend.app,
        post_json(human_auth::SIGNUP_PATH, signup_body()),
    )
    .await;
    let token = token_of(&signup);
    let announces_after_signup = backend.sink.count();

    let (status, body) = call(
        &backend.app,
        authed_patch(
            human_auth::ME_PATH,
            &token,
            json!({ "display_name": "Renamed" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["display_name"], json!("Renamed"));
    assert_eq!(body["name"], json!("alice"));
    assert_eq!(
        backend.sink.count(),
        announces_after_signup + 1,
        "a committed profile change owes a user_changed hint"
    );
}

#[tokio::test]
async fn patch_me_with_the_same_display_name_is_a_noop_that_emits_no_event() {
    let backend = backend(true).await;
    let (_, signup) = call(
        &backend.app,
        post_json(human_auth::SIGNUP_PATH, signup_body()),
    )
    .await;
    let token = token_of(&signup);
    let announces_after_signup = backend.sink.count();

    // display_name is already "Alice A" — writing the same value changes no field.
    let (status, body) = call(
        &backend.app,
        authed_patch(
            human_auth::ME_PATH,
            &token,
            json!({ "display_name": "Alice A" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["display_name"], json!("Alice A"));
    assert_eq!(
        backend.sink.count(),
        announces_after_signup,
        "a no-op patch owes no user_changed hint"
    );
}

#[tokio::test]
async fn patch_me_with_no_fields_is_the_terminal_invalid_input() {
    let backend = backend(true).await;
    let (_, signup) = call(
        &backend.app,
        post_json(human_auth::SIGNUP_PATH, signup_body()),
    )
    .await;
    let token = token_of(&signup);

    // The Zod `.refine` "no fields" case: `{}` decodes fine (display_name absent →
    // Missing), reaches the handler, and collapses into the one terminal semantic
    // 422 (ratified divergence #1), not TS's distinct "no fields to update".
    let (status, body) = call(
        &backend.app,
        authed_patch(human_auth::ME_PATH, &token, json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body, json!({ "error": "invalid input" }));
}

#[tokio::test]
async fn patch_me_with_an_invalid_display_name_is_a_native_structural_rejection() {
    let backend = backend(true).await;
    let (_, signup) = call(
        &backend.app,
        post_json(human_auth::SIGNUP_PATH, signup_body()),
    )
    .await;
    let token = token_of(&signup);

    // A non-string value for the key is a native decode fault (a present
    // `RequiredPatchField` that is not a string), so it keeps axum's native `422`
    // and is NOT re-rendered as our semantic envelope.
    let (status, body) = call(
        &backend.app,
        authed_patch(human_auth::ME_PATH, &token, json!({ "display_name": 42 })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_ne!(body, json!({ "error": "invalid input" }));
}

#[tokio::test]
async fn patch_me_from_a_value_shaped_array_is_a_native_rejection_with_zero_effect() {
    let backend = backend(true).await;
    let (_, signup) = call(
        &backend.app,
        post_json(human_auth::SIGNUP_PATH, signup_body()),
    )
    .await;
    let token = token_of(&signup);
    let announces_after_signup = backend.sink.count();

    // `MePatchRaw` has a single field (display_name), so a one-element array
    // carrying a valid display-name value lines up positionally. The object floor
    // rejects the array shape before the field is read: a native structural `422`,
    // not our semantic envelope, and the profile is untouched.
    let array = json!(["Renamed via array"]);
    let (status, body) = call(
        &backend.app,
        authed_patch(human_auth::ME_PATH, &token, array),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_ne!(body, json!({ "error": "invalid input" }));
    assert_eq!(
        backend.sink.count(),
        announces_after_signup,
        "a floor-rejected patch owes no user_changed hint"
    );
    // Zero effect: the display_name still reads its signup value.
    let (status, me) = call(&backend.app, authed_get(human_auth::ME_PATH, &token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me["display_name"], json!("Alice A"));
}

#[tokio::test]
async fn logout_revokes_the_session_then_the_bearer_is_dead() {
    let backend = backend(true).await;
    let (_, signup) = call(
        &backend.app,
        post_json(human_auth::SIGNUP_PATH, signup_body()),
    )
    .await;
    let token = token_of(&signup);

    let (status, body) = call(&backend.app, authed_post(human_auth::LOGOUT_PATH, &token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "ok": true }));

    // The perimeter now refuses the revoked bearer.
    let (status, _) = call(&backend.app, authed_get(human_auth::ME_PATH, &token)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // A second logout of the now-unknown bearer is the same 401 a missing bearer gets.
    let (status, body) = call(&backend.app, authed_post(human_auth::LOGOUT_PATH, &token)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body, json!({ "error": "unauthorized" }));
}

#[tokio::test]
async fn logout_without_a_bearer_is_401() {
    let backend = backend(true).await;
    let request = Request::builder()
        .method("POST")
        .uri(human_auth::LOGOUT_PATH)
        .body(Body::empty())
        .unwrap();
    let (status, body) = call(&backend.app, request).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body, json!({ "error": "unauthorized" }));
}

fn authed_get(path: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(path)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

fn authed_post(path: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

fn authed_patch(path: &str, token: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("PATCH")
        .uri(path)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}
