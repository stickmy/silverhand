//! End-to-end coverage of email recovery-code password reset over a real SQLite
//! file. Test doubles: the hasher (deterministic), the clock (steppable) and the
//! mailer (records or fails).

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicI64, Ordering},
};
use std::time::Duration;

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use silverhand_backend_core::{
    host_runtime::environment::OsEntropy,
    identity_access::{
        auth::SessionClock,
        credential::{
            Argon2idCost, PasswordHashError, PasswordHashParams, PasswordHasher,
            PreparedPasswordCredential, RecoveryCodeKey, RustCalibration,
        },
    },
};
use silverhand_backend_server::identity_access::password_recovery::{
    self, MailSendFailure, PasswordRecoveryDeps, RecoveryMailer,
};
use silverhand_backend_sqlite::identity_access::credential::SqliteCredentialStore;
use sqlx::{Connection, SqliteConnection};
use tempfile::TempDir;
use tower::ServiceExt;

use crate::current_schema;

const MINUTE: i64 = 60_000;
const START: i64 = 1_000_000_000;

#[derive(Clone)]
struct StepClock(Arc<AtomicI64>);

impl StepClock {
    fn advance(&self, millis: i64) {
        self.0.fetch_add(millis, Ordering::SeqCst);
    }
}

impl SessionClock for StepClock {
    fn now_unix_millis(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Clone)]
struct FakeHasher;

impl PasswordHasher for FakeHasher {
    async fn hash(&self, password: &str) -> Result<PreparedPasswordCredential, PasswordHashError> {
        Ok(PreparedPasswordCredential {
            phc: format!("phc:{password}"),
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
        Ok(phc == format!("phc:{password}"))
    }
    async fn verify_dummy(&self, _password: &str) -> Result<(), PasswordHashError> {
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq)]
enum MailMode {
    Deliver,
    Fail,
    Hang,
}

#[derive(Clone)]
struct RecordingMailer {
    mode: Arc<Mutex<MailMode>>,
    sent: Arc<Mutex<Vec<(String, String)>>>,
}

impl RecordingMailer {
    fn new(mode: MailMode) -> Self {
        Self {
            mode: Arc::new(Mutex::new(mode)),
            sent: Arc::new(Mutex::new(Vec::new())),
        }
    }
    fn sent(&self) -> Vec<(String, String)> {
        self.sent.lock().unwrap().clone()
    }
    fn set_mode(&self, mode: MailMode) {
        *self.mode.lock().unwrap() = mode;
    }
}

impl RecoveryMailer for RecordingMailer {
    async fn send(&self, to: &str, code: &str) -> Result<(), MailSendFailure> {
        let mode = *self.mode.lock().unwrap();
        match mode {
            MailMode::Deliver => {
                self.sent
                    .lock()
                    .unwrap()
                    .push((to.to_owned(), code.to_owned()));
                Ok(())
            }
            MailMode::Fail => Err(MailSendFailure {
                kind: "permanent",
                smtp_status: Some(550),
            }),
            MailMode::Hang => std::future::pending().await,
        }
    }
}

struct Backend {
    _directory: TempDir,
    path: std::path::PathBuf,
    store: SqliteCredentialStore,
    clock: StepClock,
    mailer: RecordingMailer,
    app: Router,
}

impl Backend {
    async fn new(mode: MailMode) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("recovery.sqlite");
        let mut db = current_schema::create_current_schema(&path).await;
        sqlx::raw_sql(
            "INSERT INTO users (id, name, display_name, email, created_at, updated_at) VALUES \
               ('user-1', 'alice', 'Alice', 'alice@example.com', 1, 1), \
               ('user-2', 'bob', 'Bob', 'bob@example.com', 1, 1), \
               ('user-3', 'carol', 'Carol', 'carol@example.com', 1, 1), \
               ('user-4', 'dave', 'Dave', 'dave@example.com', 1, 1), \
               ('user-5', 'erin', 'Erin', NULL, 1, 1); \
             INSERT INTO user_password_credentials \
               (user_id, password_hash, password_hash_params, password_updated_at) \
               VALUES ('user-1', 'phc:old-password', '{\"algorithm\":\"argon2id\"}', 1); \
             INSERT INTO auth_sessions (id, user_id, token_hash, created_at, last_used_at) VALUES \
               ('s1', 'user-1', 'hash-1', 1, 1), ('s2', 'user-1', 'hash-2', 1, 1), \
               ('s3', 'user-2', 'hash-3', 1, 1);",
        )
        .execute(&mut db)
        .await
        .unwrap();
        db.close().await.unwrap();
        let store = SqliteCredentialStore::open(&path).await.unwrap();
        let clock = StepClock(Arc::new(AtomicI64::new(START)));
        let mailer = RecordingMailer::new(mode);
        let app = router(&store, &clock, Some(mailer.clone()));
        Self {
            _directory: directory,
            path,
            store,
            clock,
            mailer,
            app,
        }
    }

    /// A fresh router models a process restart: a new in-memory code key.
    fn restart(&mut self) {
        self.app = router(&self.store, &self.clock, Some(self.mailer.clone()));
    }

    async fn request(&self, email: &str) -> (StatusCode, Value) {
        call(
            &self.app,
            post_json(password_recovery::REQUEST_PATH, json!({ "email": email })),
        )
        .await
    }

    async fn reset(&self, email: &str, code: &str, password: &str) -> (StatusCode, Value) {
        call(
            &self.app,
            post_json(
                password_recovery::RESET_PATH,
                json!({ "email": email, "code": code, "new_password": password }),
            ),
        )
        .await
    }

    /// Waits until `count` mails have been recorded and returns the latest code.
    async fn mailed_code(&self, count: usize) -> String {
        for _ in 0..200 {
            let sent = self.mailer.sent();
            if sent.len() >= count {
                return sent[count - 1].1.clone();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "expected {count} recovery mails, got {:?}",
            self.mailer.sent()
        );
    }

    async fn settle(&self) {
        tokio::time::sleep(Duration::from_millis(150)).await;
    }

    async fn scalar<T>(&self, sql: &str) -> T
    where
        T: for<'r> sqlx::Decode<'r, sqlx::Sqlite> + sqlx::Type<sqlx::Sqlite> + Send + Unpin,
    {
        let mut db = SqliteConnection::connect(&format!("sqlite://{}", self.path.display()))
            .await
            .unwrap();
        let sql = sql.to_owned();
        let value = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .fetch_one(&mut db)
            .await
            .unwrap();
        db.close().await.unwrap();
        value
    }

    async fn password(&self) -> String {
        self.scalar("SELECT password_hash FROM user_password_credentials WHERE user_id = 'user-1'")
            .await
    }

    async fn live_sessions(&self, user: &str) -> i64 {
        self.scalar(&format!(
            "SELECT COUNT(*) FROM auth_sessions WHERE user_id = '{user}' AND revoked_at IS NULL"
        ))
        .await
    }
}

fn router(
    store: &SqliteCredentialStore,
    clock: &StepClock,
    mailer: Option<RecordingMailer>,
) -> Router {
    password_recovery::router(PasswordRecoveryDeps {
        store: store.clone(),
        hasher: FakeHasher,
        clock: clock.clone(),
        entropy: OsEntropy,
        key: RecoveryCodeKey::generate(&OsEntropy).unwrap(),
        mailer,
    })
}

async fn call(app: &Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn post_json(path: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn wrong(code: &str) -> String {
    let next = (code.parse::<u32>().unwrap() + 1) % 1_000_000;
    format!("{next:06}")
}

const INVALID: (StatusCode, &str) = (StatusCode::BAD_REQUEST, "invalid_or_expired_code");

fn assert_invalid(response: (StatusCode, Value)) {
    assert_eq!(response.0, INVALID.0, "{}", response.1);
    assert_eq!(response.1, json!({ "error_code": INVALID.1 }));
}

#[tokio::test]
async fn without_mail_configuration_both_routes_refuse_as_disabled() {
    let backend = Backend::new(MailMode::Deliver).await;
    let app = router(&backend.store, &backend.clock, None);
    for request in [
        post_json(
            password_recovery::REQUEST_PATH,
            json!({ "email": "alice@example.com" }),
        ),
        post_json(
            password_recovery::RESET_PATH,
            json!({ "email": "alice@example.com", "code": "123456", "new_password": "new-password" }),
        ),
    ] {
        let (status, body) = call(&app, request).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body, json!({ "error_code": "password_recovery_disabled" }));
    }
}

#[tokio::test]
async fn the_request_answer_does_not_reveal_whether_an_email_is_registered() {
    let backend = Backend::new(MailMode::Deliver).await;
    let known = backend.request("Alice@Example.com").await;
    let unknown = backend.request("nobody@example.com").await;
    let no_email_account = backend.request("erin@example.com").await;
    assert_eq!(known, (StatusCode::ACCEPTED, json!({ "ok": true })));
    assert_eq!(unknown, known);
    assert_eq!(no_email_account, known);
    backend.mailed_code(1).await;
    backend.settle().await;
    let sent = backend.mailer.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, "alice@example.com");
    // Requesting changes neither the password nor any session.
    assert_eq!(backend.password().await, "phc:old-password");
    assert_eq!(backend.live_sessions("user-1").await, 2);
    // Unknown-email reset answers exactly like a wrong code.
    assert_invalid(
        backend
            .reset("nobody@example.com", "123456", "new-password")
            .await,
    );
}

#[tokio::test]
async fn a_correct_code_resets_once_revokes_sessions_and_does_not_log_in() {
    let backend = Backend::new(MailMode::Deliver).await;
    backend.request("alice@example.com").await;
    let code = backend.mailed_code(1).await;
    assert_eq!(code.len(), 6);

    assert_invalid(
        backend
            .reset("alice@example.com", &wrong(&code), "new-password")
            .await,
    );
    assert_eq!(backend.password().await, "phc:old-password");
    assert_eq!(backend.live_sessions("user-1").await, 2);
    assert_eq!(
        backend
            .scalar::<i64>("SELECT code_attempts FROM password_recovery_state")
            .await,
        1
    );

    let (status, body) = backend
        .reset("alice@example.com", &code, "new-password")
        .await;
    assert_eq!((status, &body), (StatusCode::OK, &json!({ "ok": true })));
    assert!(body.get("user_token").is_none());
    assert_eq!(backend.password().await, "phc:new-password");
    assert_eq!(backend.live_sessions("user-1").await, 0);
    assert_eq!(backend.live_sessions("user-2").await, 1);
    assert!(
        backend
            .scalar::<Option<String>>("SELECT code_hash FROM password_recovery_state")
            .await
            .is_none()
    );

    // Replay of the spent code fails and changes nothing.
    assert_invalid(
        backend
            .reset("alice@example.com", &code, "third-password")
            .await,
    );
    assert_eq!(backend.password().await, "phc:new-password");
}

#[tokio::test]
async fn an_account_without_a_password_gains_one_through_recovery() {
    let backend = Backend::new(MailMode::Deliver).await;
    backend.request("bob@example.com").await;
    let code = backend.mailed_code(1).await;
    assert_eq!(
        backend
            .reset("bob@example.com", &code, "bob-password")
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        backend
            .scalar::<String>(
                "SELECT password_hash FROM user_password_credentials WHERE user_id = 'user-2'"
            )
            .await,
        "phc:bob-password"
    );
}

#[tokio::test]
async fn a_resend_replaces_the_code_and_respects_the_interval() {
    let backend = Backend::new(MailMode::Deliver).await;
    backend.request("alice@example.com").await;
    let first = backend.mailed_code(1).await;

    // Within the resend interval nothing is issued.
    backend.clock.advance(30_000);
    backend.request("alice@example.com").await;
    backend.settle().await;
    assert_eq!(backend.mailer.sent().len(), 1);

    backend.clock.advance(31_000);
    backend.request("alice@example.com").await;
    let second = backend.mailed_code(2).await;

    if first != second {
        assert_invalid(
            backend
                .reset("alice@example.com", &first, "new-password")
                .await,
        );
    }
    assert_eq!(
        backend
            .reset("alice@example.com", &second, "new-password")
            .await
            .0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn an_expired_code_is_refused() {
    let backend = Backend::new(MailMode::Deliver).await;
    backend.request("alice@example.com").await;
    let code = backend.mailed_code(1).await;
    backend.clock.advance(15 * MINUTE);
    assert_invalid(
        backend
            .reset("alice@example.com", &code, "new-password")
            .await,
    );
    assert_eq!(backend.password().await, "phc:old-password");
}

/// Advances the clock while hashing, as a slow argon2 or a busy pool would.
#[derive(Clone)]
struct SlowHasher(StepClock, i64);

impl PasswordHasher for SlowHasher {
    async fn hash(&self, password: &str) -> Result<PreparedPasswordCredential, PasswordHashError> {
        self.0.advance(self.1);
        FakeHasher.hash(password).await
    }
    async fn verify(&self, phc: &str, password: &str) -> Result<bool, PasswordHashError> {
        FakeHasher.verify(phc, password).await
    }
    async fn verify_dummy(&self, password: &str) -> Result<(), PasswordHashError> {
        FakeHasher.verify_dummy(password).await
    }
}

#[tokio::test]
async fn a_code_that_expires_while_the_password_hashes_is_not_consumed() {
    let mut backend = Backend::new(MailMode::Deliver).await;
    backend.app = password_recovery::router(PasswordRecoveryDeps {
        store: backend.store.clone(),
        hasher: SlowHasher(backend.clock.clone(), 2 * MINUTE),
        clock: backend.clock.clone(),
        entropy: OsEntropy,
        key: RecoveryCodeKey::generate(&OsEntropy).unwrap(),
        mailer: Some(backend.mailer.clone()),
    });
    backend.request("alice@example.com").await;
    let code = backend.mailed_code(1).await;
    // One minute left at the precheck; two minutes of hashing cross expiry.
    backend.clock.advance(14 * MINUTE);
    assert_invalid(
        backend
            .reset("alice@example.com", &code, "new-password")
            .await,
    );
    assert_eq!(backend.password().await, "phc:old-password");
    assert_eq!(backend.live_sessions("user-1").await, 2);
}

#[tokio::test]
async fn five_wrong_attempts_void_the_code() {
    let backend = Backend::new(MailMode::Deliver).await;
    backend.request("alice@example.com").await;
    let code = backend.mailed_code(1).await;
    for _ in 0..5 {
        assert_invalid(
            backend
                .reset("alice@example.com", &wrong(&code), "new-password")
                .await,
        );
    }
    assert_invalid(
        backend
            .reset("alice@example.com", &code, "new-password")
            .await,
    );
    assert_eq!(backend.password().await, "phc:old-password");
}

#[tokio::test]
async fn resending_does_not_restore_the_hourly_attempt_budget() {
    let backend = Backend::new(MailMode::Deliver).await;
    let mut mails = 0;
    let mut code = String::new();
    // Two codes, five wrong attempts each, exhaust the ten-attempt hourly budget.
    for _ in 0..2 {
        backend.request("alice@example.com").await;
        mails += 1;
        code = backend.mailed_code(mails).await;
        for _ in 0..5 {
            assert_invalid(
                backend
                    .reset("alice@example.com", &wrong(&code), "new-password")
                    .await,
            );
        }
        backend.clock.advance(MINUTE);
    }
    // No new code is issued while the budget is spent.
    backend.request("alice@example.com").await;
    backend.settle().await;
    assert_eq!(backend.mailer.sent().len(), mails);
    assert_invalid(
        backend
            .reset("alice@example.com", &code, "new-password")
            .await,
    );

    // The window rolls over after an hour.
    backend.clock.advance(60 * MINUTE);
    backend.request("alice@example.com").await;
    let fresh = backend.mailed_code(mails + 1).await;
    assert_eq!(
        backend
            .reset("alice@example.com", &fresh, "new-password")
            .await
            .0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn at_most_five_codes_are_sent_per_hour() {
    let backend = Backend::new(MailMode::Deliver).await;
    for round in 1..=6 {
        backend.request("alice@example.com").await;
        if round <= 5 {
            backend.mailed_code(round).await;
        } else {
            backend.settle().await;
        }
        backend.clock.advance(MINUTE);
    }
    assert_eq!(backend.mailer.sent().len(), 5);
}

#[tokio::test]
async fn a_changed_email_invalidates_the_outstanding_code() {
    let backend = Backend::new(MailMode::Deliver).await;
    backend.request("alice@example.com").await;
    let code = backend.mailed_code(1).await;
    let mut db = SqliteConnection::connect(&format!("sqlite://{}", backend.path.display()))
        .await
        .unwrap();
    sqlx::query("UPDATE users SET email = 'alice2@example.com' WHERE id = 'user-1'")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    assert_invalid(
        backend
            .reset("alice@example.com", &code, "new-password")
            .await,
    );
    assert_invalid(
        backend
            .reset("alice2@example.com", &code, "new-password")
            .await,
    );
    assert_eq!(backend.password().await, "phc:old-password");
}

#[tokio::test]
async fn a_restart_voids_delivered_codes_and_a_new_request_recovers() {
    let mut backend = Backend::new(MailMode::Deliver).await;
    backend.request("alice@example.com").await;
    let delivered = backend.mailed_code(1).await;
    backend.restart();
    assert_invalid(
        backend
            .reset("alice@example.com", &delivered, "new-password")
            .await,
    );

    backend.clock.advance(MINUTE);
    backend.request("alice@example.com").await;
    let fresh = backend.mailed_code(2).await;
    assert_eq!(
        backend
            .reset("alice@example.com", &fresh, "new-password")
            .await
            .0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn concurrent_resets_with_the_right_code_succeed_exactly_once() {
    let backend = Backend::new(MailMode::Deliver).await;
    backend.request("alice@example.com").await;
    let code = backend.mailed_code(1).await;
    let attempts = (0..4).map(|index| {
        let app = backend.app.clone();
        let code = code.clone();
        async move {
            call(
                &app,
                post_json(
                    password_recovery::RESET_PATH,
                    json!({ "email": "alice@example.com", "code": code, "new_password": format!("password-{index}") }),
                ),
            )
            .await
            .0
        }
    });
    let statuses = futures_util::future::join_all(attempts).await;
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == StatusCode::OK)
            .count(),
        1,
        "{statuses:?}"
    );
    assert!(
        statuses
            .iter()
            .all(|status| *status == StatusCode::OK || *status == StatusCode::BAD_REQUEST)
    );
    assert_eq!(backend.live_sessions("user-1").await, 0);
}

#[tokio::test]
async fn a_failed_send_still_answers_accepted_and_a_later_request_recovers() {
    let backend = Backend::new(MailMode::Fail).await;
    assert_eq!(
        backend.request("alice@example.com").await.0,
        StatusCode::ACCEPTED
    );
    backend.settle().await;
    assert!(backend.mailer.sent().is_empty());
    assert_eq!(backend.password().await, "phc:old-password");

    backend.mailer.set_mode(MailMode::Deliver);
    backend.clock.advance(MINUTE);
    backend.request("alice@example.com").await;
    let code = backend.mailed_code(1).await;
    assert_eq!(
        backend
            .reset("alice@example.com", &code, "new-password")
            .await
            .0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn a_saturated_queue_refuses_instead_of_queueing_without_bound() {
    let backend = Backend::new(MailMode::Hang).await;
    let mut statuses = Vec::new();
    for email in [
        "alice@example.com",
        "bob@example.com",
        "carol@example.com",
        "dave@example.com",
    ] {
        statuses.push(backend.request(email).await.0);
    }
    backend.settle().await;
    for index in 0..30 {
        let (status, body) = backend.request(&format!("nobody{index}@example.com")).await;
        if status == StatusCode::SERVICE_UNAVAILABLE {
            assert_eq!(body, json!({ "error_code": "password_recovery_busy" }));
        }
        statuses.push(status);
    }
    assert!(
        statuses[..4]
            .iter()
            .all(|status| *status == StatusCode::ACCEPTED)
    );
    assert!(statuses.contains(&StatusCode::SERVICE_UNAVAILABLE));
    assert!(statuses.iter().all(
        |status| *status == StatusCode::ACCEPTED || *status == StatusCode::SERVICE_UNAVAILABLE
    ));
}

#[tokio::test]
async fn malformed_input_is_refused_before_any_work() {
    let backend = Backend::new(MailMode::Deliver).await;
    assert_eq!(
        backend.request("not-an-email").await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        backend
            .reset("alice@example.com", "123456", "short")
            .await
            .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_invalid(
        backend
            .reset("alice@example.com", "12345a", "new-password")
            .await,
    );
}
