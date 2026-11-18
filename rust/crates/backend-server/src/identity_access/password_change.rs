//! Authenticated self-service password change. Passwords never enter tracing.
use crate::{
    foundation::{bearer::strict_bearer_credential, wire_admission::ValidatedJson},
    identity_access::auth::{PublicJsonUnauthorized, require_human_session},
};
use axum::{
    Json, Router,
    extract::{Extension, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use garde::Validate;
use serde::Deserialize;
use silverhand_backend_core::{
    foundation::identity::UserId,
    identity_access::{
        auth::{AuthenticateHumanSession, SessionClock, SessionTokenHash},
        credential::{PasswordChangeStore, PasswordHasher, change_password},
    },
};

use super::human_auth::{LOGIN_PASSWORD_MIN_UTF16, SIGNUP_PASSWORD_MIN_UTF16, password_within};

pub const PATH: &str = "/api/me/password";

#[derive(Deserialize, Validate)]
struct Input {
    #[garde(custom(password_within(LOGIN_PASSWORD_MIN_UTF16)))]
    current_password: String,
    #[garde(custom(password_within(SIGNUP_PASSWORD_MIN_UTF16)))]
    new_password: String,
}
#[derive(Clone)]
struct Deps<S, H, C> {
    store: S,
    hasher: H,
    clock: C,
}

pub fn router<
    S: PasswordChangeStore,
    H: PasswordHasher,
    C: SessionClock,
    A: AuthenticateHumanSession,
>(
    store: S,
    hasher: H,
    clock: C,
    sessions: A,
) -> Router {
    require_human_session(
        Router::new()
            .route(PATH, post(handle::<S, H, C>).get(status::<S, H, C>))
            .with_state(Deps {
                store,
                hasher,
                clock,
            }),
        sessions,
        PublicJsonUnauthorized,
    )
}

async fn handle<S: PasswordChangeStore, H: PasswordHasher, C: SessionClock>(
    State(deps): State<Deps<S, H, C>>,
    Extension(user): Extension<UserId>,
    headers: HeaderMap,
    ValidatedJson(input): ValidatedJson<Input>,
) -> Response {
    let Some(token) = strict_bearer_credential(&headers) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let session = SessionTokenHash::of_plaintext(token);
    match change_password(
        &deps.store,
        &deps.hasher,
        &user,
        &session,
        &input.current_password,
        &input.new_password,
        deps.clock.now_unix_millis(),
    )
    .await
    {
        Ok(true) => Json(serde_json::json!({"ok": true})).into_response(),
        Ok(false) => (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error_code":"password_change_rejected"})),
        )
            .into_response(),
        Err(cause) => {
            tracing::error!(
                stage = "change",
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &crate::foundation::error::describe(&cause)
                ),
                "password operation failed"
            );
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
        }
    }
}

async fn status<S: PasswordChangeStore, H: PasswordHasher, C: SessionClock>(
    State(deps): State<Deps<S, H, C>>,
    Extension(user): Extension<UserId>,
) -> Response {
    match deps.store.password_for_user(&user).await {
        Ok(password) => {
            Json(serde_json::json!({"has_password": password.is_some()})).into_response()
        }
        Err(cause) => {
            tracing::error!(
                stage = "status",
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &crate::foundation::error::describe(&cause)
                ),
                "password operation failed"
            );
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
        }
    }
}
