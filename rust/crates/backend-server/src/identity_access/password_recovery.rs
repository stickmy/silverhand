//! Public email recovery-code password reset.
//!
//! `request` answers 202 after admission only; the account lookup, limits, code
//! issuance and SMTP send run on a bounded background queue so the response does
//! not depend on whether the email is registered. A full queue refuses with 503
//! regardless of the email. Queued sends and outstanding codes do not survive a
//! restart; the user requests a new code.
use std::{
    future::Future,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use garde::Validate;
use serde::Deserialize;
use silverhand_backend_core::{
    host_runtime::environment::EntropySource,
    identity_access::{
        auth::SessionClock,
        credential::{
            PasswordHasher, PasswordRecoveryStore, RecoveryCodeKey, issue_recovery_code,
            reset_password_with_code,
        },
        user_identity::EmailAddress,
    },
};
use tokio::sync::{Semaphore, mpsc};

use super::human_auth::{SIGNUP_PASSWORD_MIN_UTF16, password_within};
use crate::foundation::{error, safe_log_field::safe_display, wire_admission::ValidatedJson};

pub const REQUEST_PATH: &str = "/auth/password-recovery/request";
pub const RESET_PATH: &str = "/auth/password-recovery/reset";

const QUEUE_CAPACITY: usize = 16;
const SEND_CONCURRENCY: usize = 4;
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
const ADMISSIONS_PER_MINUTE: u32 = 60;

/// Delivers one recovery code. Implementations must not log the address or code.
pub trait RecoveryMailer: Clone + Send + Sync + 'static {
    fn send(
        &self,
        to: &str,
        code: &str,
    ) -> impl Future<Output = Result<(), MailSendFailure>> + Send;
}

/// The only send-failure detail that reaches logs: SMTP replies echo addresses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MailSendFailure {
    pub kind: &'static str,
    pub smtp_status: Option<u16>,
}

pub struct PasswordRecoveryDeps<S, H, C, E, M> {
    pub store: S,
    pub hasher: H,
    pub clock: C,
    pub entropy: E,
    pub key: RecoveryCodeKey,
    /// `None` when this server has no mail configuration: both routes answer 503.
    pub mailer: Option<M>,
}

struct Enabled<S, H, C> {
    store: S,
    hasher: H,
    clock: C,
    key: RecoveryCodeKey,
    queue: mpsc::Sender<String>,
    request_budget: Budget,
    reset_budget: Budget,
}

type Shared<S, H, C> = Option<Arc<Enabled<S, H, C>>>;

/// Process-wide fixed-window admission count.
struct Budget(Mutex<(Instant, u32)>);

impl Budget {
    fn new() -> Self {
        Self(Mutex::new((Instant::now(), 0)))
    }

    fn admit(&self) -> bool {
        let mut window = self.0.lock().expect("budget lock");
        if window.0.elapsed() >= Duration::from_secs(60) {
            *window = (Instant::now(), 0);
        }
        if window.1 >= ADMISSIONS_PER_MINUTE {
            return false;
        }
        window.1 += 1;
        true
    }
}

pub fn router<S, H, C, E, M>(deps: PasswordRecoveryDeps<S, H, C, E, M>) -> Router
where
    S: PasswordRecoveryStore,
    H: PasswordHasher,
    C: SessionClock,
    E: EntropySource + Clone + 'static,
    M: RecoveryMailer,
{
    let state: Shared<S, H, C> = deps.mailer.map(|mailer| {
        let (queue, jobs) = mpsc::channel(QUEUE_CAPACITY);
        tokio::spawn(run_sender(
            jobs,
            deps.store.clone(),
            deps.key.clone(),
            deps.entropy,
            deps.clock.clone(),
            mailer,
        ));
        Arc::new(Enabled {
            store: deps.store,
            hasher: deps.hasher,
            clock: deps.clock,
            key: deps.key,
            queue,
            request_budget: Budget::new(),
            reset_budget: Budget::new(),
        })
    });
    Router::new()
        .route(REQUEST_PATH, post(request::<S, H, C>))
        .route(RESET_PATH, post(reset::<S, H, C>))
        .with_state(state)
}

async fn run_sender<S, E, C, M>(
    mut jobs: mpsc::Receiver<String>,
    store: S,
    key: RecoveryCodeKey,
    entropy: E,
    clock: C,
    mailer: M,
) where
    S: PasswordRecoveryStore,
    E: EntropySource + Clone + 'static,
    C: SessionClock,
    M: RecoveryMailer,
{
    let permits = Arc::new(Semaphore::new(SEND_CONCURRENCY));
    while let Some(email) = jobs.recv().await {
        let Ok(permit) = permits.clone().acquire_owned().await else {
            return;
        };
        let (store, key, entropy, clock, mailer) = (
            store.clone(),
            key.clone(),
            entropy.clone(),
            clock.clone(),
            mailer.clone(),
        );
        tokio::spawn(async move {
            let _permit = permit;
            let issued =
                match issue_recovery_code(&store, &key, &entropy, &email, clock.now_unix_millis())
                    .await
                {
                    Ok(Some(issued)) => issued,
                    Ok(None) => return,
                    Err(cause) => {
                        tracing::error!(
                            error_chain = safe_display(&error::describe(&cause)),
                            "password recovery code issuance failed"
                        );
                        return;
                    }
                };
            match tokio::time::timeout(SEND_TIMEOUT, mailer.send(&issued.email, &issued.code)).await
            {
                Ok(Ok(())) => {}
                Ok(Err(failure)) => tracing::warn!(
                    kind = failure.kind,
                    smtp_status = failure.smtp_status,
                    "password recovery mail send failed"
                ),
                Err(_) => tracing::warn!("password recovery mail send timed out"),
            }
        });
    }
}

#[derive(Deserialize, Validate)]
struct RequestInput {
    #[garde(length(max = 1024))]
    email: String,
}

#[derive(Deserialize, Validate)]
struct ResetInput {
    #[garde(length(max = 1024))]
    email: String,
    #[garde(length(max = 64))]
    code: String,
    #[garde(custom(password_within(SIGNUP_PASSWORD_MIN_UTF16)))]
    new_password: String,
}

async fn request<S: PasswordRecoveryStore, H: PasswordHasher, C: SessionClock>(
    State(state): State<Shared<S, H, C>>,
    ValidatedJson(input): ValidatedJson<RequestInput>,
) -> Response {
    let Some(state) = state else {
        return error_code(
            StatusCode::SERVICE_UNAVAILABLE,
            "password_recovery_disabled",
        );
    };
    let Ok(email) = EmailAddress::parse(&input.email) else {
        return crate::foundation::wire_admission::invalid_input();
    };
    if !state.request_budget.admit() {
        return error_code(StatusCode::TOO_MANY_REQUESTS, "rate_limited");
    }
    if state.queue.try_send(email.into_inner()).is_err() {
        return error_code(StatusCode::SERVICE_UNAVAILABLE, "password_recovery_busy");
    }
    (StatusCode::ACCEPTED, Json(serde_json::json!({"ok": true}))).into_response()
}

async fn reset<S: PasswordRecoveryStore, H: PasswordHasher, C: SessionClock>(
    State(state): State<Shared<S, H, C>>,
    ValidatedJson(input): ValidatedJson<ResetInput>,
) -> Response {
    let Some(state) = state else {
        return error_code(
            StatusCode::SERVICE_UNAVAILABLE,
            "password_recovery_disabled",
        );
    };
    let Ok(email) = EmailAddress::parse(&input.email) else {
        return crate::foundation::wire_admission::invalid_input();
    };
    if !state.reset_budget.admit() {
        return error_code(StatusCode::TOO_MANY_REQUESTS, "rate_limited");
    }
    let code = input.code.trim();
    if code.len() != 6 || !code.bytes().all(|byte| byte.is_ascii_digit()) {
        return invalid_code();
    }
    match reset_password_with_code(
        &state.store,
        &state.hasher,
        &state.clock,
        &state.key,
        email.as_str(),
        code,
        &input.new_password,
    )
    .await
    {
        Ok(true) => Json(serde_json::json!({"ok": true})).into_response(),
        Ok(false) => invalid_code(),
        Err(cause) => {
            tracing::error!(
                error_chain = safe_display(&error::describe(&cause)),
                "password recovery reset failed"
            );
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
        }
    }
}

fn invalid_code() -> Response {
    error_code(StatusCode::BAD_REQUEST, "invalid_or_expired_code")
}

fn error_code(status: StatusCode, code: &'static str) -> Response {
    (status, Json(serde_json::json!({ "error_code": code }))).into_response()
}
